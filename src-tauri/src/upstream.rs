use reqwest::blocking::Client;
use serde::Serialize;
use std::error::Error;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detection {
    pub protocol: String,
    pub inference_endpoint: String,
    pub anthropic_auth: bool,
    pub routing_required: bool,
    pub message: String,
}

pub fn detect(api_url: &str, api_key: &str) -> Result<Detection, Box<dyn Error>> {
    let client = crate::network::blocking_builder(PROBE_TIMEOUT)?
        .user_agent(concat!("CSwitch/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let mut errors = Vec::new();
    for (probe, protocol, anthropic) in [
        ("responses", "openai_responses", false),
        ("chat", "openai_chat", false),
        ("anthropic", "anthropic_messages", true),
    ] {
        match probe_protocol(&client, api_url, api_key, probe, anthropic) {
            Ok(Some(endpoint)) => {
                return Ok(Detection {
                    protocol: protocol.into(),
                    inference_endpoint: endpoint,
                    anthropic_auth: anthropic,
                    routing_required: protocol != "openai_responses",
                    message: if protocol == "openai_responses" {
                        "上游提供 Responses，使用直连模式".into()
                    } else {
                        format!(
                            "上游提供 {}，需要启用本地协议转换",
                            protocol_label(protocol)
                        )
                    },
                });
            }
            Ok(None) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }
    Err(format!("尚未确认可用的推理接口。{}", errors.join("；")).into())
}

pub fn protocol_label(protocol: &str) -> &'static str {
    match protocol {
        "openai_responses" => "Responses",
        "openai_chat" => "OpenAI Chat Completions",
        "anthropic_messages" => "Anthropic Messages",
        _ => "未知协议",
    }
}

pub fn base_url_for_endpoint(endpoint: &str, protocol: &str) -> Result<String, Box<dyn Error>> {
    let suffix = match protocol {
        "openai_responses" => "/responses",
        "openai_chat" => "/chat/completions",
        "anthropic_messages" => "/messages",
        _ => return Err(format!("不支持的上游协议：{protocol}").into()),
    };
    endpoint
        .strip_suffix(suffix)
        .map(|base| base.trim_end_matches('/').to_string())
        .filter(|base| !base.is_empty())
        .ok_or_else(|| format!("推理接口地址与协议不匹配：{endpoint}").into())
}

fn probe_protocol(
    client: &Client,
    api_url: &str,
    api_key: &str,
    protocol: &str,
    anthropic_auth: bool,
) -> Result<Option<String>, Box<dyn Error>> {
    let mut errors = Vec::new();
    for endpoint in endpoint_candidates(api_url, protocol) {
        let request = client.post(&endpoint).json(&serde_json::json!({}));
        let response = if anthropic_auth {
            request
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
        } else {
            request.bearer_auth(api_key).send()
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                errors.push(format!("{endpoint}：{}", error.without_url()));
                continue;
            }
        };
        let status = response.status();
        if matches!(status.as_u16(), 404 | 405 | 410) {
            continue;
        }
        if matches!(status.as_u16(), 401 | 403) {
            errors.push(format!("{endpoint}：认证返回 {status}"));
            continue;
        }
        if status.is_server_error() || status.as_u16() == 429 || status.is_redirection() {
            errors.push(format!("{endpoint}：探测返回 {status}"));
            continue;
        }
        let body = response.json::<serde_json::Value>();
        let confirmed = body.as_ref().is_ok_and(|body| {
            let error = &body["error"];
            let message = error["message"]
                .as_str()
                .or_else(|| body["message"].as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            (matches!(status.as_u16(), 400 | 422)
                && (error["param"] == "model"
                    || (message.contains("model")
                        && (message.contains("required") || message.contains("missing")))))
                || (status.is_success()
                    && (body["object"] == "response"
                        || body["object"] == "chat.completion"
                        || body["type"] == "message"))
        });
        if confirmed {
            return Ok(Some(endpoint));
        }
        errors.push(format!("{endpoint}：{status} 响应没有提供可识别的协议结构"));
    }
    if errors.is_empty() {
        Ok(None)
    } else {
        Err(errors.join("；").into())
    }
}

fn endpoint_candidates(api_url: &str, protocol: &str) -> Vec<String> {
    let base = api_url.trim_end_matches('/');
    let suffix = match protocol {
        "responses" => "responses",
        "chat" => "chat/completions",
        "anthropic" => "messages",
        _ => return Vec::new(),
    };
    let mut candidates = Vec::new();
    if base.ends_with("/v1") {
        candidates.push(format!("{base}/{suffix}"));
    } else {
        candidates.push(format!("{base}/{suffix}"));
        candidates.push(format!("{base}/v1/{suffix}"));
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use tiny_http::{Response, Server, StatusCode};

    #[test]
    fn candidates_cover_root_and_v1_without_duplicate_v1() {
        assert_eq!(
            endpoint_candidates("https://api.example.com", "responses"),
            [
                "https://api.example.com/responses",
                "https://api.example.com/v1/responses"
            ]
        );
        assert_eq!(
            endpoint_candidates("https://api.example.com/v1", "chat"),
            ["https://api.example.com/v1/chat/completions"]
        );
    }

    #[test]
    fn detects_responses_without_trying_other_protocols() {
        let server = Server::http("127.0.0.1:0").expect("server");
        let address = server.server_addr();
        let handle = thread::spawn(move || {
            let request = server.recv().expect("request");
            assert!(request.url().ends_with("/responses"));
            request
                .respond(Response::from_string(r#"{"error":{"message":"Missing required parameter: model","param":"model"}}"#).with_status_code(StatusCode(400)))
                .expect("respond responses probe");
        });

        let result = detect(&format!("http://{address}"), "key").expect("detect");
        assert_eq!(result.protocol, "openai_responses");
        assert!(!result.routing_required);
        handle.join().expect("join");
    }

    #[test]
    fn detects_chat_when_responses_is_missing() {
        let server = Server::http("127.0.0.1:0").expect("server");
        let address = server.server_addr();
        let handle = thread::spawn(move || {
            for _ in 0..3 {
                let request = server.recv().expect("request");
                let status = if request.url().ends_with("/responses") {
                    StatusCode(404)
                } else {
                    StatusCode(400)
                };
                request.respond(Response::from_string(r#"{"error":{"message":"Missing required parameter: model","param":"model"}}"#).with_status_code(status)).expect("response");
            }
        });
        let result = detect(&format!("http://{address}"), "key").expect("detect");
        assert_eq!(result.protocol, "openai_chat");
        assert!(result.routing_required);
        handle.join().expect("join");
    }

    #[test]
    fn detects_anthropic_after_openai_protocols_are_missing() {
        let server = Server::http("127.0.0.1:0").expect("server");
        let address = server.server_addr();
        let handle = thread::spawn(move || {
            for _ in 0..5 {
                let request = server.recv().expect("request");
                if request.url().ends_with("/messages") {
                    assert!(
                        request
                            .headers()
                            .iter()
                            .any(|header| header.field.equiv("x-api-key")
                                && header.value.as_str() == "key")
                    );
                    assert!(
                        request
                            .headers()
                            .iter()
                            .any(|header| header.field.equiv("anthropic-version"))
                    );
                    request
                        .respond(Response::from_string(r#"{"error":{"message":"Missing required parameter: model","param":"model"}}"#).with_status_code(StatusCode(400)))
                        .expect("respond messages probe");
                } else {
                    request
                        .respond(Response::empty(StatusCode(404)))
                        .expect("respond missing OpenAI endpoint");
                }
            }
        });

        let result = detect(&format!("http://{address}"), "key").expect("detect");
        assert_eq!(result.protocol, "anthropic_messages");
        assert!(result.routing_required);
        assert!(result.anthropic_auth);
        handle.join().expect("join");
    }

    #[test]
    fn derives_the_codex_base_url_from_the_detected_endpoint() {
        assert_eq!(
            base_url_for_endpoint("https://api.example.com/v1/responses", "openai_responses")
                .unwrap(),
            "https://api.example.com/v1"
        );
        assert_eq!(
            base_url_for_endpoint("https://api.example.com/chat/completions", "openai_chat")
                .unwrap(),
            "https://api.example.com"
        );
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use tiny_http::{Response, Server, StatusCode};
    #[test]
    fn server_error_is_not_protocol_support() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.server_addr());
        let thread = std::thread::spawn(move || {
            for _ in 0..6 {
                server
                    .recv()
                    .unwrap()
                    .respond(Response::empty(StatusCode(500)))
                    .unwrap();
            }
        });
        assert!(detect(&endpoint, "fixture").is_err());
        thread.join().unwrap();
    }
}
