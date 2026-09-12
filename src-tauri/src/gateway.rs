use crate::gateway_transform;
use crate::profiles::{ProfileStore, ProviderProfile};
use rand::random;
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const START_TIMEOUT: Duration = Duration::from_secs(8);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const CONTROL_TIMEOUT: Duration = Duration::from_millis(500);
const TOKEN_HEADER: &str = "X-CSwitch-Gateway-Token";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayState {
    pid: u32,
    port: u16,
    provider_id: String,
    token: String,
}

pub(crate) fn run_from_args<I>(args: I) -> Result<bool, Box<dyn Error>>
where
    I: IntoIterator<Item = String>,
{
    let args = args.into_iter().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) != Some("--cswitch-gateway") {
        return Ok(false);
    }
    let codex_home = args.get(2).ok_or("本地路由缺少 Codex 目录参数")?;
    let provider_id = args.get(3).ok_or("本地路由缺少供应商参数")?;
    let port = args
        .get(4)
        .ok_or("本地路由缺少端口参数")?
        .parse::<u16>()
        .map_err(|error| format!("本地路由端口无效：{error}"))?;
    run_process(Path::new(codex_home), provider_id, port)?;
    Ok(true)
}

pub(crate) fn local_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

pub(crate) fn ensure_running(codex_home: &Path, provider_id: &str) -> Result<u16, Box<dyn Error>> {
    let path = state_path(codex_home);
    if let Ok(state) = read_state(&path)
        && state.provider_id == provider_id
        && health_check(&state)
    {
        return Ok(state.port);
    }

    stop(codex_home)?;
    let port = available_port()?;
    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable)
        .arg("--cswitch-gateway")
        .arg(codex_home)
        .arg(provider_id)
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(state) = read_state(&path)
            && state.provider_id == provider_id
            && state.port == port
            && health_check(&state)
        {
            return Ok(port);
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("本地路由进程提前退出，状态为 {status}").into());
        }
        thread::sleep(Duration::from_millis(80));
    }
    terminate_child(&mut child);
    let _ = fs::remove_file(path);
    Err("本地路由启动超时".into())
}

pub(crate) fn active_base_url(codex_home: &Path, provider_id: &str) -> Option<String> {
    let state = read_state(&state_path(codex_home)).ok()?;
    (state.provider_id == provider_id).then(|| local_base_url(state.port))
}

pub(crate) fn stop_provider(codex_home: &Path, provider_id: &str) -> Result<(), Box<dyn Error>> {
    let path = state_path(codex_home);
    if read_state(&path).is_ok_and(|state| state.provider_id == provider_id) {
        stop(codex_home)?;
    }
    Ok(())
}

pub(crate) fn stop(codex_home: &Path) -> Result<(), Box<dyn Error>> {
    let path = state_path(codex_home);
    let Ok(state) = read_state(&path) else {
        return Ok(());
    };
    let response = control_client()?
        .post(format!("http://127.0.0.1:{}/shutdown", state.port))
        .header(TOKEN_HEADER, &state.token)
        .send();
    if !response.is_ok_and(|response| response.status().is_success()) {
        terminate_pid(state.pid);
    } else {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && health_check(&state) {
            thread::sleep(Duration::from_millis(40));
        }
        if health_check(&state) {
            terminate_pid(state.pid);
        }
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn run_process(codex_home: &Path, provider_id: &str, port: u16) -> Result<(), Box<dyn Error>> {
    let provider = ProfileStore::new(codex_home).load_provider(provider_id)?;
    if provider.record.routing_mode != "local" {
        return Err("供应商没有启用本地路由".into());
    }
    let server = Server::http(format!("127.0.0.1:{port}"))
        .map_err(|error| format!("绑定本地路由端口失败：{error}"))?;
    let state = GatewayState {
        pid: std::process::id(),
        port,
        provider_id: provider_id.to_string(),
        token: format!("{:032x}", random::<u128>()),
    };
    write_state(&state_path(codex_home), &state)?;

    for mut request in server.incoming_requests() {
        let path = request.url().split('?').next().unwrap_or_default();
        if path == "/health" {
            respond_health(request, &state);
            continue;
        }
        if path == "/shutdown" && request.method() == &Method::Post {
            if control_token(&request) == Some(state.token.as_str()) {
                respond_text(request, 200, "stopping");
                break;
            }
            respond_error(request, 403, "本地路由控制凭据无效");
            continue;
        }
        if request.method() != &Method::Post
            || !matches!(
                path,
                "/responses" | "/v1/responses" | "/responses/compact" | "/v1/responses/compact"
            )
        {
            respond_error(request, 404, "本地路由仅接收 Responses 请求");
            continue;
        }
        let key = match api_key_from_auth(&provider.auth) {
            Ok(Some(key)) => key,
            Ok(None) => {
                respond_error(request, 500, "供应商缺少 API Key");
                continue;
            }
            Err(error) => {
                respond_error(request, 500, &error.to_string());
                continue;
            }
        };
        if bearer_token(&request) != Some(key.as_str()) {
            respond_error(request, 401, "本地路由请求凭据无效");
            continue;
        }
        let mut body = Vec::new();
        if let Err(error) = request.as_reader().read_to_end(&mut body) {
            respond_error(request, 400, &format!("读取 Responses 请求失败：{error}"));
            continue;
        }
        match handle_request(&provider, &key, &body) {
            Ok((status, content_type, bytes)) => {
                respond_bytes(request, status, content_type, bytes)
            }
            Err(error) => respond_error(request, 502, &error.to_string()),
        }
    }
    let _ = fs::remove_file(state_path(codex_home));
    Ok(())
}

fn handle_request(
    provider: &ProviderProfile,
    key: &str,
    request_body: &[u8],
) -> Result<(u16, &'static str, Vec<u8>), Box<dyn Error>> {
    let body: Value = serde_json::from_slice(request_body)
        .map_err(|error| format!("Responses 请求不是有效 JSON：{error}"))?;
    let endpoint = provider
        .record
        .inference_endpoint
        .as_deref()
        .ok_or("供应商缺少上游接口地址")?;
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let converted_request = gateway_transform::convert_request(&provider.record.protocol, &body)?;
    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(Policy::none())
        .user_agent(concat!("CSwitch/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let request = client.post(endpoint);
    let response = if provider.record.protocol == "anthropic_messages" {
        request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .json(&converted_request.body)
            .send()?
    } else {
        request
            .bearer_auth(key)
            .json(&converted_request.body)
            .send()?
    };
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = response.bytes()?.to_vec();
    if status >= 400 {
        return Ok((
            status,
            "application/json",
            gateway_transform::normalize_error(&bytes)?,
        ));
    }
    let converted = gateway_transform::convert_response(
        &provider.record.protocol,
        &content_type,
        &bytes,
        &converted_request.context,
    )?;
    if stream {
        Ok((
            status,
            "text/event-stream",
            gateway_transform::response_to_sse(&converted)?,
        ))
    } else {
        Ok((status, "application/json", serde_json::to_vec(&converted)?))
    }
}

fn api_key_from_auth(auth: &[u8]) -> Result<Option<String>, Box<dyn Error>> {
    let value: Value = serde_json::from_slice(auth)?;
    Ok(value
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string))
}

fn bearer_token(request: &Request) -> Option<&str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Authorization"))
        .and_then(|header| header.value.as_str().strip_prefix("Bearer "))
}

fn control_token(request: &Request) -> Option<&str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(TOKEN_HEADER))
        .map(|header| header.value.as_str())
}

fn respond_health(request: Request, state: &GatewayState) {
    if control_token(&request) != Some(state.token.as_str()) {
        respond_error(request, 403, "本地路由控制凭据无效");
        return;
    }
    respond_bytes(
        request,
        200,
        "application/json",
        serde_json::to_vec(&json!({"providerId": state.provider_id})).unwrap_or_default(),
    );
}

fn respond_error(request: Request, status: u16, message: &str) {
    respond_bytes(
        request,
        status,
        "application/json",
        serde_json::to_vec(&json!({
            "error": {"message": message, "type": "cswitch_gateway_error"}
        }))
        .unwrap_or_default(),
    );
}

fn respond_text(request: Request, status: u16, text: &str) {
    respond_bytes(
        request,
        status,
        "text/plain; charset=utf-8",
        text.as_bytes().to_vec(),
    );
}

fn respond_bytes(request: Request, status: u16, content_type: &str, bytes: Vec<u8>) {
    let mut response = Response::from_data(bytes).with_status_code(StatusCode(status));
    if let Ok(header) = Header::from_bytes("Content-Type", content_type) {
        response = response.with_header(header);
    }
    let _ = request.respond(response);
}

fn health_check(state: &GatewayState) -> bool {
    control_client()
        .and_then(|client| {
            client
                .get(format!("http://127.0.0.1:{}/health", state.port))
                .header(TOKEN_HEADER, &state.token)
                .send()
                .map_err(Into::into)
        })
        .is_ok_and(|response| response.status().is_success())
}

fn control_client() -> Result<Client, Box<dyn Error>> {
    Ok(Client::builder().timeout(CONTROL_TIMEOUT).build()?)
}

fn available_port() -> Result<u16, Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_pid(pid: u32) {
    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    } else {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .output();
    }
}

fn state_path(codex_home: &Path) -> PathBuf {
    codex_home.join("cswitch-profiles").join("gateway.json")
}

fn read_state(path: &Path) -> Result<GatewayState, Box<dyn Error>> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn write_state(path: &Path, state: &GatewayState) -> Result<(), Box<dyn Error>> {
    let parent = path.parent().ok_or("本地路由状态文件没有父目录")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".gateway-{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.sync_all()?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn ignores_normal_desktop_arguments() {
        assert!(!run_from_args(["cswitch".to_string()]).expect("parse desktop args"));
    }

    #[test]
    fn rejects_incomplete_gateway_arguments() {
        let error = run_from_args(["cswitch".to_string(), "--cswitch-gateway".to_string()])
            .expect_err("missing gateway args");
        assert!(error.to_string().contains("Codex 目录"));
    }

    #[test]
    fn reads_the_active_gateway_address_without_requiring_health() {
        let directory = tempdir().expect("tempdir");
        let state = GatewayState {
            pid: 123,
            port: 43210,
            provider_id: "provider-1".to_string(),
            token: "fixture-token".to_string(),
        };
        write_state(&state_path(directory.path()), &state).expect("write state");
        assert_eq!(
            active_base_url(directory.path(), "provider-1").as_deref(),
            Some("http://127.0.0.1:43210/v1")
        );
        assert!(active_base_url(directory.path(), "provider-2").is_none());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn local_gateway_converts_a_streaming_tool_call_end_to_end() {
        let directory = tempdir().expect("tempdir");
        let upstream = Server::http("127.0.0.1:0").expect("upstream server");
        let upstream_address = upstream.server_addr();
        let upstream_handle = thread::spawn(move || {
            let mut request = upstream.recv().expect("upstream request");
            assert_eq!(request.url(), "/chat/completions");
            assert_eq!(
                request
                    .headers()
                    .iter()
                    .find(|header| header.field.equiv("Authorization"))
                    .map(|header| header.value.as_str()),
                Some("Bearer fixture-key")
            );
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("read upstream request");
            let body: Value = serde_json::from_str(&body).expect("parse upstream request");
            assert_eq!(body["messages"][0]["role"], "user");
            assert_eq!(body["tools"][0]["function"]["name"], "shell");
            request
                .respond(
                    Response::from_string(
                        r#"{"id":"chat_1","model":"fixture-model","choices":[{"message":{"tool_calls":[{"id":"call_1","function":{"name":"shell","arguments":"{\"command\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":4,"completion_tokens":3,"total_tokens":7}}"#,
                    )
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type"),
                    ),
                )
                .expect("respond upstream");
        });

        let endpoint = format!("http://{upstream_address}/chat/completions");
        let auth = serde_json::to_vec(&json!({"OPENAI_API_KEY": "fixture-key"})).unwrap();
        let provider = ProfileStore::new(directory.path())
            .save_provider_with_routing(
                None,
                "Chat 上游",
                &format!("http://{upstream_address}"),
                &auth,
                None,
                "openai_chat",
                "local",
                Some(&endpoint),
                b"",
            )
            .expect("save provider");

        let port = available_port().expect("gateway port");
        let codex_home = directory.path().to_path_buf();
        let provider_id = provider.id.clone();
        let gateway_handle = thread::spawn(move || {
            run_process(&codex_home, &provider_id, port).expect("run gateway")
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let state = loop {
            if let Ok(state) = read_state(&state_path(directory.path()))
                && health_check(&state)
            {
                break state;
            }
            assert!(Instant::now() < deadline, "gateway start timed out");
            thread::sleep(Duration::from_millis(20));
        };

        let response = Client::new()
            .post(format!("http://127.0.0.1:{}/v1/responses", state.port))
            .bearer_auth("fixture-key")
            .json(&json!({
                "model": "fixture-model",
                "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "run pwd"}]}],
                "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object"}}],
                "stream": true
            }))
            .send()
            .expect("gateway request");
        assert!(response.status().is_success());
        let events = response.text().expect("gateway response");
        assert!(events.contains("event: response.function_call_arguments.done"));
        assert!(events.contains("\"call_id\":\"call_1\""));
        assert!(events.contains("event: response.completed"));

        stop(directory.path()).expect("stop gateway");
        gateway_handle.join().expect("join gateway");
        upstream_handle.join().expect("join upstream");
    }
}
