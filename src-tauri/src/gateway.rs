use crate::profiles::{ProfileStore, ProviderProfile, atomic_write_private};
use crate::{gateway_transform, network, sse};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::HeaderMap,
    response::Response,
    routing::{get, post},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const START_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const TOKEN_HEADER: &str = "X-CSwitch-Gateway-Token";
const MAX_BODY: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayState {
    pid: u32,
    port: u16,
    provider_id: String,
    token: String,
    #[serde(default)]
    executable: PathBuf,
    #[serde(default)]
    started: u64,
}

pub(crate) fn local_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}
fn state_path(home: &Path) -> PathBuf {
    home.join("cswitch-profiles/gateway.json")
}
fn instances(home: &Path) -> PathBuf {
    home.join("cswitch-profiles/gateways")
}
fn state_paths(home: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut paths = Vec::new();
    let legacy = state_path(home);
    if legacy.is_file() {
        paths.push(legacy);
    }
    let root = instances(home);
    if root.exists() {
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}
#[cfg(test)]
fn states(home: &Path) -> Result<Vec<(PathBuf, GatewayState)>, Box<dyn Error>> {
    state_paths(home)?
        .into_iter()
        .map(|path| Ok((path.clone(), read_state(&path)?)))
        .collect()
}
fn read_state(path: &Path) -> Result<GatewayState, Box<dyn Error>> {
    let bytes =
        fs::read(path).map_err(|error| format!("读取路由状态失败 {}：{error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("路由状态格式无效 {}：{error}", path.display()).into())
}
fn write_state(path: &Path, state: &GatewayState) -> Result<(), Box<dyn Error>> {
    atomic_write_private(path, &serde_json::to_vec_pretty(state)?)
}

pub(crate) fn run_from_args<I: IntoIterator<Item = String>>(
    args: I,
) -> Result<bool, Box<dyn Error>> {
    let args = args.into_iter().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) != Some("--cswitch-gateway") {
        return Ok(false);
    }
    let home = Path::new(args.get(2).ok_or("本地路由缺少 Codex 目录参数")?);
    let provider = args.get(3).ok_or("本地路由缺少供应商参数")?;
    let id = args.get(4).ok_or("本地路由缺少实例编号")?;
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("本地路由实例编号无效".into());
    }
    let path = instances(home).join(format!("{id}.json"));
    run_server(home, provider, 0, path)?;
    Ok(true)
}

// The old gateway remains alive until the config/history transaction commits.
pub(crate) struct PreparedGateway {
    home: PathBuf,
    state: GatewayState,
    process: GatewayChild,
}
impl PreparedGateway {
    pub fn port(&self) -> u16 {
        self.state.port
    }
    pub fn commit(self) -> Vec<String> {
        let active_path = self.process.path.clone();
        self.process.detach();
        let mut warnings = Vec::new();
        match stop_matching(&self.home, Some(&active_path), None) {
            Ok(errors) => warnings.extend(
                errors
                    .into_iter()
                    .map(|error| format!("新路由已启用，旧路由清理失败：{error}")),
            ),
            Err(error) => warnings.push(format!("新路由已启用，读取旧路由目录失败：{error}")),
        }
        warnings
    }
}
// std::process::Child has no Drop cleanup. Own it before the first fallible readiness check.
struct GatewayChild {
    path: PathBuf,
    child: Option<Child>,
}
impl GatewayChild {
    fn detach(mut self) {
        if let Some(mut child) = self.child.take() {
            // Reap successfully committed children too, otherwise Unix retains zombie processes.
            thread::spawn(move || {
                if let Err(error) = child.wait() {
                    eprintln!("等待本地路由进程退出失败：{error}");
                }
            });
        }
    }
}
impl Drop for GatewayChild {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if !matches!(child.try_wait(), Ok(Some(_))) {
            if let Err(error) = child.kill() {
                eprintln!(
                    "结束未提交路由失败，进程 {}，状态 {}：{error}",
                    child.id(),
                    self.path.display()
                );
                return;
            }
            if let Err(error) = child.wait() {
                eprintln!("回收未提交路由失败，进程 {}：{error}", child.id());
                return;
            }
        }
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("清理未提交路由状态失败 {}：{error}", self.path.display());
        }
    }
}
pub(crate) fn prepare(home: &Path, provider: &str) -> Result<PreparedGateway, Box<dyn Error>> {
    prepare_with_executable(home, provider, &std::env::current_exe()?)
}

fn prepare_with_executable(
    home: &Path,
    provider: &str,
    executable: &Path,
) -> Result<PreparedGateway, Box<dyn Error>> {
    let id = format!("{:032x}", rand::random::<u128>());
    let root = instances(home);
    fs::create_dir_all(&root)?;
    let path = root.join(format!("{id}.json"));
    let log = root.join(format!("{id}.log"));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut command = Command::new(executable);
    command
        .args(["--cswitch-gateway"])
        .arg(home)
        .arg(provider)
        .arg(&id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(options.open(&log)?));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    await_started(home, provider, path, &log, command.spawn()?)
}

fn await_started(
    home: &Path,
    provider: &str,
    path: PathBuf,
    log: &Path,
    child: Child,
) -> Result<PreparedGateway, Box<dyn Error>> {
    let mut process = GatewayChild {
        path,
        child: Some(child),
    };
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let child = process.child.as_mut().expect("startup owns its child");
        if process.path.exists() {
            let state = read_state(&process.path).map_err(|error| {
                format!(
                    "读取启动路由状态失败 {}：{error}；日志：{}",
                    process.path.display(),
                    log.display()
                )
            })?;
            if state.pid == child.id() && state.provider_id == provider && health_check(&state) {
                return Ok(PreparedGateway {
                    home: home.into(),
                    state,
                    process,
                });
            }
        }
        if let Some(status) = child.try_wait()? {
            let detail = fs::read_to_string(log)?;
            return Err(format!(
                "本地路由启动失败（{status}）：{}；日志：{}",
                detail.trim(),
                log.display()
            )
            .into());
        }
        if Instant::now() >= deadline {
            return Err(format!("本地路由启动超时；日志：{}", log.display()).into());
        }
        thread::sleep(Duration::from_millis(60));
    }
}
pub(crate) fn active_base_url(home: &Path, provider: &str) -> Option<String> {
    let text = fs::read_to_string(home.join("config.toml")).ok()?;
    let document = crate::config::parse_config(&text).ok()?;
    let url = crate::config::custom_provider(&document)?
        .get("base_url")?
        .as_str()?;
    state_paths(home)
        .ok()?
        .into_iter()
        .filter_map(|path| read_state(&path).ok())
        .find(|state| state.provider_id == provider && local_base_url(state.port) == url)
        .map(|state| local_base_url(state.port))
}
pub(crate) fn stop_provider(home: &Path, provider: &str) -> Result<(), Box<dyn Error>> {
    finish_stopping(stop_matching(home, None, Some(provider))?)
}
pub(crate) fn stop(home: &Path) -> Result<(), Box<dyn Error>> {
    finish_stopping(stop_matching(home, None, None)?)
}
fn finish_stopping(errors: Vec<String>) -> Result<(), Box<dyn Error>> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("；").into())
    }
}
fn stop_matching(
    home: &Path,
    keep: Option<&Path>,
    provider: Option<&str>,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut errors = Vec::new();
    for path in state_paths(home)? {
        if keep == Some(path.as_path()) {
            continue;
        }
        let result = (|| {
            let state = read_state(&path)?;
            if provider.is_none_or(|provider| provider == state.provider_id) {
                stop_state(&path, &state)?;
            }
            Ok::<_, Box<dyn Error>>(())
        })();
        if let Err(error) = result {
            errors.push(error.to_string());
        }
    }
    Ok(errors)
}
fn control_client() -> Result<reqwest::blocking::Client, Box<dyn Error>> {
    Ok(reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(CONTROL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}
fn health_check(state: &GatewayState) -> bool {
    control_client()
        .and_then(|client| {
            Ok(client
                .get(format!("http://127.0.0.1:{}/health", state.port))
                .header(TOKEN_HEADER, &state.token)
                .send()?)
        })
        .is_ok_and(|response| response.status().is_success())
}
fn stop_state(path: &Path, state: &GatewayState) -> Result<(), Box<dyn Error>> {
    let response = control_client()?
        .post(format!("http://127.0.0.1:{}/shutdown", state.port))
        .header(TOKEN_HEADER, &state.token)
        .send();
    if response.is_ok_and(|r| r.status().is_success()) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(30));
        }
        if !path.exists() {
            return Ok(());
        }
    }
    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(state.pid)]),
        true,
    );
    if let Some(process) = system.process(sysinfo::Pid::from_u32(state.pid)) {
        // Never terminate a reused PID or an unverified legacy process.
        if state.started == 0
            || process.start_time() != state.started
            || process.exe() != Some(state.executable.as_path())
        {
            return Err(format!(
                "路由进程身份不匹配，保留进程 {}，请检查 {}",
                state.pid,
                path.display()
            )
            .into());
        }
        if !process.kill() {
            return Err(format!("系统拒绝结束路由进程 {}", state.pid).into());
        }
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[derive(Clone)]
struct Runtime {
    provider: Arc<ProviderProfile>,
    state: GatewayState,
    client: reqwest::Client,
    shutdown: Arc<Notify>,
}
fn run_server(home: &Path, provider: &str, port: u16, path: PathBuf) -> Result<(), Box<dyn Error>> {
    let provider = Arc::new(ProfileStore::new(home).load_provider(provider)?);
    if provider.record.routing_mode != "local" {
        return Err("供应商没有启用本地路由".into());
    }
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        // Port zero is assigned directly to the listening socket; no probe/rebind race.
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        let mut system = sysinfo::System::new();
        let pid = sysinfo::get_current_pid()?;
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        let started = system
            .process(pid)
            .ok_or("读取本地路由进程身份失败")?
            .start_time();
        let state = GatewayState {
            pid: std::process::id(),
            port: listener.local_addr()?.port(),
            provider_id: provider.record.id.clone(),
            token: format!("{:032x}", rand::random::<u128>()),
            executable: std::env::current_exe()?,
            started,
        };
        let shutdown = Arc::new(Notify::new());
        let data = Runtime {
            provider,
            state: state.clone(),
            client: network::async_builder()?.build()?,
            shutdown: shutdown.clone(),
        };
        let app = Router::new()
            .route("/health", get(health))
            .route("/shutdown", post(shutdown_handler))
            .route("/responses", post(inference))
            .route("/v1/responses", post(inference))
            .route("/responses/compact", post(compact))
            .route("/v1/responses/compact", post(compact))
            .layer(DefaultBodyLimit::max(MAX_BODY))
            .with_state(data);
        write_state(&path, &state)?;
        // Dropping the server also drops pending requests and their upstream streams.
        tokio::select! { result=axum::serve(listener,app)=>result?, _=shutdown.notified()=>{} }
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok::<(), Box<dyn Error>>(())
    })
}
async fn health(State(data): State<Runtime>, headers: HeaderMap) -> Response {
    if headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) != Some(&data.state.token) {
        return error(403, "本地路由控制凭据无效");
    }
    response(
        200,
        "application/json",
        Body::from(json!({"providerId":data.state.provider_id}).to_string()),
    )
}
async fn shutdown_handler(State(data): State<Runtime>, headers: HeaderMap) -> Response {
    if headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) != Some(&data.state.token) {
        return error(403, "本地路由控制凭据无效");
    }
    let signal = data.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        signal.notify_one();
    });
    response(200, "application/json", Body::from("{}"))
}
async fn compact() -> Response {
    error(
        501,
        "该上游未提供 Responses compact 接口，请使用客户端本地上下文压缩",
    )
}
fn response(status: u16, kind: &str, body: Body) -> Response {
    Response::builder()
        .status(status)
        .header("Content-Type", kind)
        .body(body)
        .expect("static response headers")
}
fn error(status: u16, message: &str) -> Response {
    response(
        status,
        "application/json",
        Body::from(json!({"error":{"message":message,"type":"cswitch_gateway_error"}}).to_string()),
    )
}
async fn inference(State(data): State<Runtime>, headers: HeaderMap, bytes: Bytes) -> Response {
    match forward(data, headers, bytes).await {
        Ok(response) => response,
        Err(message) => error(502, &message),
    }
}
async fn forward(data: Runtime, headers: HeaderMap, bytes: Bytes) -> Result<Response, String> {
    let key = crate::config::api_key_from_auth(&data.provider.auth)
        .map_err(|e| e.to_string())?
        .ok_or("供应商缺少 API Key")?;
    if headers.get("Authorization").and_then(|v| v.to_str().ok())
        != Some(format!("Bearer {key}").as_str())
    {
        return Ok(error(401, "本地路由请求凭据无效"));
    }
    let body: Value = serde_json::from_slice(&bytes).map_err(|_| "Responses 请求不是有效 JSON")?;
    let protocol = data.provider.record.protocol.clone();
    let converted =
        gateway_transform::convert_request(&protocol, &body).map_err(|e| e.to_string())?;
    let endpoint = data
        .provider
        .record
        .inference_endpoint
        .as_deref()
        .ok_or("供应商缺少已探测的推理接口")?;
    let request = data.client.post(endpoint).json(&converted.body);
    let request = if protocol == "anthropic_messages" {
        request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
    } else {
        request.bearer_auth(key)
    };
    let upstream = request
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = upstream.status();
    let is_sse = upstream
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    if body["stream"] == true && status.is_success() && is_sse {
        let mut converter =
            sse::Converter::new(&protocol, body["model"].clone(), converted.context);
        let stream = async_stream::stream! {
            yield Ok::<Bytes,std::io::Error>(Bytes::from(converter.start()));
            let mut decoder=sse::Decoder::default();let mut chunks=upstream.bytes_stream();let mut failed=false;
            while let Some(chunk)=chunks.next().await {
                let events=match chunk {Ok(bytes)=>decoder.push(&bytes),Err(_)=>Err("上游连接中断或读取超时".into())};
                match events {
                    Ok(events)=>{for event in events {match converter.push(event){Ok(bytes)=>{if !bytes.is_empty(){yield Ok(Bytes::from(bytes));}},Err(reason)=>{yield Ok(Bytes::from(converter.failed(&reason)));failed=true;break;}}}},
                    Err(reason)=>{yield Ok(Bytes::from(converter.failed(&reason)));failed=true;}
                }
                if failed{break;}
            }
            if !failed {match decoder.finish().and_then(|()|converter.finish()){Ok(bytes)=>yield Ok(Bytes::from(bytes)),Err(reason)=>yield Ok(Bytes::from(converter.failed(&reason)))}}
        };
        return Ok(response(
            status.as_u16(),
            "text/event-stream",
            Body::from_stream(stream),
        ));
    }
    let kind = if is_sse {
        "text/event-stream"
    } else {
        "application/json"
    };
    let mut chunks = upstream.bytes_stream();
    let mut result = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| "读取上游响应失败")?;
        if result.len() + chunk.len() > MAX_BODY {
            return Err("上游响应超过 32 MiB".into());
        }
        result.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        return Ok(response(
            status.as_u16(),
            "application/json",
            Body::from(gateway_transform::normalize_error(&result).map_err(|e| e.to_string())?),
        ));
    }
    let converted =
        gateway_transform::convert_response(&protocol, kind, &result, &converted.context)
            .map_err(|e| e.to_string())?;
    if body["stream"] == true {
        Ok(response(
            status.as_u16(),
            "text/event-stream",
            Body::from(gateway_transform::response_to_sse(&converted).map_err(|e| e.to_string())?),
        ))
    } else {
        Ok(response(
            status.as_u16(),
            "application/json",
            Body::from(converted.to_string()),
        ))
    }
}

#[cfg(test)]
fn available_port() -> Result<u16, Box<dyn Error>> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}
#[cfg(test)]
fn run_process(home: &Path, provider: &str, port: u16) -> Result<(), Box<dyn Error>> {
    run_server(home, provider, port, state_path(home))
}
#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn startup_child_fixture() {
        // A child test process, isolated by its own environment rather than a global test setting.
        if std::env::var_os("CSWITCH_TEST_STARTUP_CHILD").is_some() {
            thread::sleep(Duration::from_secs(30));
        }
    }
    #[test]
    fn malformed_startup_state_reaps_child_and_removes_state() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("broken.json");
        fs::write(&path, b"not-json").unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "gateway::lifecycle_tests::startup_child_fixture",
                "--nocapture",
            ])
            .env("CSWITCH_TEST_STARTUP_CHILD", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let child = command.spawn().unwrap();
        let pid = sysinfo::Pid::from_u32(child.id());
        let result = await_started(
            home.path(),
            "fixture",
            path.clone(),
            &home.path().join("fixture.log"),
            child,
        );
        assert!(result.is_err());
        assert!(!path.exists());
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        assert!(
            system.process(pid).is_none(),
            "startup error leaked a child process"
        );
    }
    #[test]
    fn ignores_normal_desktop_arguments() {
        assert!(!run_from_args(["cswitch".into()]).unwrap());
    }
    #[test]
    fn rejects_incomplete_gateway_arguments() {
        assert!(run_from_args(["cswitch".into(), "--cswitch-gateway".into()]).is_err());
    }
    #[test]
    fn does_not_kill_reused_pid() {
        let home = tempfile::tempdir().unwrap();
        let path = state_path(home.path());
        let state = GatewayState {
            pid: std::process::id(),
            port: 1,
            provider_id: "test".into(),
            token: "fixture".into(),
            started: 1,
            executable: std::env::current_exe().unwrap(),
        };
        write_state(&path, &state).unwrap();
        assert!(
            stop_state(&path, &state)
                .unwrap_err()
                .to_string()
                .contains("身份")
        );
    }
}
#[cfg(test)]
mod integration_tests {
    use super::*;
    use reqwest::blocking::Client;
    use tempfile::tempdir;
    use tiny_http::{Header, Response, Server};

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

#[cfg(test)]
mod streaming_integration_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::mpsc;
    #[test]
    fn delayed_upstream_streams_early_and_does_not_block_a_second_request() {
        let home = tempfile::tempdir().unwrap();
        let upstream = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/chat/completions", upstream.local_addr().unwrap());
        let (release_tx, release_rx) = mpsc::channel();
        let upstream_thread = thread::spawn(move || {
            let (mut first, _) = upstream.accept().unwrap();
            first
                .set_read_timeout(Some(Duration::from_secs(8)))
                .unwrap();
            read_request(&mut first);
            first.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            first.write_all(b"data: {\"id\":\"chat_first\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"early-text\"}}]}\n\n").unwrap();
            first.flush().unwrap();
            let (mut second, _) = upstream.accept().unwrap();
            read_request(&mut second);
            second.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"id\":\"chat_second\",\"choices\":[{\"message\":{\"content\":\"second completed\"},\"finish_reason\":\"stop\"}]}").unwrap();
            drop(second);
            release_rx.recv_timeout(Duration::from_secs(8)).unwrap();
            first.write_all(b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").unwrap();
        });
        let auth = serde_json::to_vec(&json!({"OPENAI_API_KEY":"fixture-key"})).unwrap();
        let provider = ProfileStore::new(home.path())
            .save_provider_with_routing(
                None,
                "test",
                "http://localhost",
                &auth,
                None,
                "openai_chat",
                "local",
                Some(&endpoint),
                b"",
            )
            .unwrap();
        let port = available_port().unwrap();
        let root = home.path().to_path_buf();
        let server = thread::spawn(move || run_process(&root, &provider.id, port).unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        let state = loop {
            if let Ok(state) = read_state(&state_path(home.path()))
                && health_check(&state)
            {
                break state;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        };
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(6))
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{}/v1/responses", state.port);
        let first = client
            .post(&url)
            .bearer_auth("fixture-key")
            .json(&json!({"model":"test","input":"first","stream":true}))
            .send()
            .unwrap();
        let mut reader = BufReader::new(first);
        let mut line = String::new();
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line.contains("early-text") {
                break;
            }
        }
        assert!(
            health_check(&state),
            "health is served while inference is pending"
        );
        let second = client
            .post(&url)
            .bearer_auth("fixture-key")
            .json(&json!({"model":"test","input":"second","stream":false}))
            .send()
            .unwrap()
            .text()
            .unwrap();
        assert!(second.contains("second completed"));
        release_tx.send(()).unwrap();
        let mut tail = String::new();
        reader.read_to_string(&mut tail).unwrap();
        assert!(tail.contains("response.completed"));
        stop(home.path()).unwrap();
        server.join().unwrap();
        upstream_thread.join().unwrap();
    }
    fn read_request(stream: &mut std::net::TcpStream) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let mut length = 0;
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
    }
}

#[cfg(test)]
mod process_integration_tests {
    use super::*;
    #[test]
    #[ignore = "uses the separately built desktop executable; run with CSWITCH_TEST_BINARY"]
    fn staged_gateway_rollback_keeps_original_port_and_credentials() {
        let binary =
            PathBuf::from(std::env::var_os("CSWITCH_TEST_BINARY").expect("CSWITCH_TEST_BINARY"));
        let home = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(home.path());
        let first = store
            .save_provider_with_routing(
                None,
                "first",
                "http://localhost",
                br#"{"OPENAI_API_KEY":"first-key"}"#,
                None,
                "openai_chat",
                "local",
                Some("http://127.0.0.1:1/chat/completions"),
                b"",
            )
            .unwrap();
        let second = store
            .save_provider_with_routing(
                None,
                "second",
                "http://localhost",
                br#"{"OPENAI_API_KEY":"second-key"}"#,
                None,
                "openai_chat",
                "local",
                Some("http://127.0.0.1:1/chat/completions"),
                b"",
            )
            .unwrap();
        let a = prepare_with_executable(home.path(), &first.id, &binary).unwrap();
        let old_port = a.port();
        let state = a.state.clone();
        a.commit();
        let b = prepare_with_executable(home.path(), &second.id, &binary).unwrap();
        assert_ne!(old_port, b.port());
        assert!(health_check(&state));
        drop(b);
        assert!(health_check(&state));
        assert_eq!(states(home.path()).unwrap().len(), 1);
        let c = prepare_with_executable(home.path(), &second.id, &binary).unwrap();
        let active = c.state.clone();
        assert!(c.commit().is_empty());
        assert!(health_check(&active));
        assert!(!health_check(&state));
        let malformed = instances(home.path()).join("broken.json");
        fs::write(&malformed, b"not-json").unwrap();
        let error = stop(home.path()).unwrap_err().to_string();
        assert!(error.contains("broken.json"));
        assert!(
            !health_check(&active),
            "one corrupt state must not leave valid gateways running"
        );
        assert!(malformed.exists(), "keep corrupt state for diagnosis");
        fs::remove_file(malformed).unwrap();
        assert!(states(home.path()).unwrap().is_empty());
    }
}
