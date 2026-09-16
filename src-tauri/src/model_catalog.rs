use serde_json::{Value, json};
use std::collections::HashSet;
use std::error::Error;
use std::time::Duration;

const MODELS_TIMEOUT: Duration = Duration::from_secs(20);
#[derive(Debug)]
pub struct ModelCatalog {
    pub bytes: Vec<u8>,
}

#[cfg(test)]
pub fn fetch(api_url: &str, api_key: &str) -> Result<ModelCatalog, Box<dyn Error>> {
    fetch_with_auth(api_url, api_key, false)
}

pub fn fetch_with_auth(
    api_url: &str,
    api_key: &str,
    anthropic_auth: bool,
) -> Result<ModelCatalog, Box<dyn Error>> {
    let client = crate::network::blocking_builder(MODELS_TIMEOUT)?
        .user_agent(concat!("CSwitch/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let base = api_url.trim_end_matches('/');
    let endpoints = if base.ends_with("/v1") {
        vec![format!("{base}/models")]
    } else {
        vec![format!("{base}/v1/models"), format!("{base}/models")]
    };
    for endpoint in endpoints {
        let mut after: Option<String> = None;
        let mut cursors = HashSet::new();
        let mut models = Vec::new();
        loop {
            let mut request = client.get(&endpoint);
            if let Some(cursor) = after.as_ref() {
                request = request.query(&[("after_id", cursor)]);
            }
            let response = if anthropic_auth {
                request
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .send()
            } else {
                request.bearer_auth(api_key).send()
            }
            .map_err(|error| format!("连接模型列表失败 {endpoint}：{}", error.without_url()))?;
            if response.status().as_u16() == 404 && after.is_none() {
                break;
            }
            if !response.status().is_success() {
                return Err(
                    format!("模型列表请求失败 {endpoint}，返回 {}", response.status()).into(),
                );
            }
            let payload: Value = response.json().map_err(|_| "模型列表返回了非 JSON 内容")?;
            let list = payload
                .get("data")
                .or_else(|| payload.get("models"))
                .and_then(Value::as_array)
                .ok_or("模型列表缺少 data 或 models 数组")?;
            for model in list {
                let id = model
                    .as_str()
                    .or_else(|| model.get("id").and_then(Value::as_str))
                    .or_else(|| model.get("slug").and_then(Value::as_str))
                    .ok_or("模型列表条目缺少模型编号")?;
                models.push(id.to_string());
            }
            if payload["has_more"] != true {
                return build(models);
            }
            let cursor = payload["last_id"]
                .as_str()
                .ok_or("分页模型列表缺少 last_id")?
                .to_string();
            if !cursors.insert(cursor.clone()) || cursors.len() > 100 {
                return Err("模型列表分页游标重复或超过 100 页".into());
            }
            after = Some(cursor);
        }
    }
    Err("供应商的 /v1/models 与 /models 均不存在".into())
}

pub fn build<I>(model_ids: I) -> Result<ModelCatalog, Box<dyn Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut seen = HashSet::new();
    let model_ids = model_ids
        .into_iter()
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
        .filter(|model| seen.insert(model.clone()))
        .collect::<Vec<_>>();
    if model_ids.is_empty() {
        return Err("模型列表中没有可用的模型 ID".into());
    }

    let models = model_ids
        .iter()
        .map(|model| model_info(model))
        .collect::<Vec<_>>();
    Ok(ModelCatalog {
        bytes: serde_json::to_vec_pretty(&json!({ "models": models }))?,
    })
}

#[cfg(test)]
fn models_endpoint(api_url: &str) -> String {
    let base = api_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

fn model_info(model: &str) -> Value {
    json!({
        "slug": model,
        "display_name": model
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use tiny_http::{Header, Response, Server};

    #[test]
    fn root_and_v1_urls_resolve_without_duplicate_segments() {
        assert_eq!(
            models_endpoint("https://api.example.com"),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            models_endpoint("https://api.example.com/v1"),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn catalog_preserves_order_and_removes_duplicate_ids() {
        let catalog = build([
            "model-b".to_string(),
            " model-a ".to_string(),
            "model-b".to_string(),
        ])
        .expect("build catalog");
        let value: Value = serde_json::from_slice(&catalog.bytes).expect("parse catalog");
        let ids = value["models"]
            .as_array()
            .expect("models array")
            .iter()
            .map(|model| model["slug"].as_str().expect("slug"))
            .collect::<Vec<_>>();
        assert_eq!(ids, ["model-b", "model-a"]);
        assert_eq!(
            value["models"],
            json!([
                {
                    "slug": "model-b",
                    "display_name": "model-b"
                },
                {
                    "slug": "model-a",
                    "display_name": "model-a"
                }
            ])
        );
    }

    #[test]
    fn fetches_standard_models_with_bearer_auth() {
        let server = Server::http("127.0.0.1:0").expect("bind server");
        let address = server.server_addr();
        let handle = thread::spawn(move || {
            let mut request = server.recv().expect("receive request");
            assert_eq!(request.method().as_str(), "GET");
            assert_eq!(request.url(), "/v1/models");
            let authorization = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Authorization"))
                .expect("authorization");
            assert_eq!(authorization.value.as_str(), "Bearer fixture-key");
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("read request");
            assert!(body.is_empty());
            request
                .respond(
                    Response::from_string(r#"{"data":[{"id":"model-a"},{"id":"model-b"}]}"#)
                        .with_header(
                            Header::from_bytes("Content-Type", "application/json")
                                .expect("content type"),
                        ),
                )
                .expect("respond");
        });

        let catalog = fetch(&format!("http://{address}"), "fixture-key").expect("fetch catalog");
        let value: Value = serde_json::from_slice(&catalog.bytes).expect("parse catalog");
        let ids = value["models"]
            .as_array()
            .expect("models array")
            .iter()
            .map(|model| model["slug"].as_str().expect("slug"))
            .collect::<Vec<_>>();
        assert_eq!(ids, ["model-a", "model-b"]);
        handle.join().expect("join server");
    }
}

#[cfg(test)]
mod pagination_regression_tests {
    use super::*;
    use tiny_http::{Response, Server};
    #[test]
    fn reads_all_pages_without_duplicate_models() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let task = std::thread::spawn(move || {
            let first = server.recv().unwrap();
            assert_eq!(first.url(), "/v1/models");
            first
                .respond(Response::from_string(
                    r#"{"data":[{"id":"first"}],"has_more":true,"last_id":"first"}"#,
                ))
                .unwrap();
            let second = server.recv().unwrap();
            assert!(second.url().contains("after_id=first"));
            second
                .respond(Response::from_string(
                    r#"{"data":[{"id":"first"},{"id":"second"}],"has_more":false}"#,
                ))
                .unwrap();
        });
        let catalog = fetch_with_auth(&url, "fixture", false).unwrap();
        let data: Value = serde_json::from_slice(&catalog.bytes).unwrap();
        assert_eq!(data["models"].as_array().unwrap().len(), 2);
        task.join().unwrap();
    }
}
