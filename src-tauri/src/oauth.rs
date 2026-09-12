use base64::Engine;
use chrono::Utc;
use rand::RngCore;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tiny_http::{Request, Response, Server, StatusCode as TinyStatusCode};
use url::Url;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const ORIGINATOR: &str = "codex_cli_rs";
// Track openai/codex latest stable: https://github.com/openai/codex/releases/latest
const CODEX_CLI_VERSION: &str = "0.154.0";
const LOCAL_NO_PROXY: &str = "localhost,127.0.0.1,::1";
const CALLBACK_PORTS: [u16; 2] = [1455, 1457];
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CALLBACK_POLL_INTERVAL: Duration = Duration::from_millis(100);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const ACCESS_TOKEN_REFRESH_WINDOW_MINUTES: i64 = 5;
static LOGIN_CANCELLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, PartialEq)]
pub enum AuthHealth {
    Valid(Vec<u8>),
    Invalid,
}

#[derive(Debug, PartialEq)]
pub enum LocalAuthState {
    Current,
    NeedsRefresh,
    Invalid,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

struct PkceCodes {
    verifier: String,
    challenge: String,
}

enum Callback {
    Code(String),
    Rejected(String),
    StateMismatch,
    MissingCode,
}

pub fn refresh_auth(auth: &[u8]) -> Result<AuthHealth, Box<dyn Error>> {
    let client = http_client()?;
    refresh_auth_with(&client, &format!("{ISSUER}/oauth/token"), auth)
}

pub fn inspect_auth(auth: &[u8]) -> LocalAuthState {
    let document: Value = match serde_json::from_slice(auth) {
        Ok(Value::Object(object)) => Value::Object(object),
        _ => return LocalAuthState::Invalid,
    };
    let Some(refresh_token) = token_field(&document, "refresh_token") else {
        return LocalAuthState::Invalid;
    };
    if refresh_token.trim().is_empty() {
        return LocalAuthState::Invalid;
    }
    let Some(access_token) = token_field(&document, "access_token") else {
        return LocalAuthState::NeedsRefresh;
    };
    let Some(expires_at) = jwt_expiration(&access_token) else {
        return LocalAuthState::NeedsRefresh;
    };
    let refresh_at = Utc::now().timestamp() + ACCESS_TOKEN_REFRESH_WINDOW_MINUTES * 60;
    if expires_at > refresh_at {
        LocalAuthState::Current
    } else {
        LocalAuthState::NeedsRefresh
    }
}

pub fn begin_login() {
    LOGIN_CANCELLED.store(false, Ordering::SeqCst);
}

pub fn cancel_login() {
    LOGIN_CANCELLED.store(true, Ordering::SeqCst);
}

pub fn ensure_login_active() -> Result<(), Box<dyn Error>> {
    ensure_not_cancelled(&LOGIN_CANCELLED)
}

pub fn browser_login() -> Result<Vec<u8>, Box<dyn Error>> {
    ensure_login_active()?;
    let (server, port) = bind_callback_server()?;
    let redirect_uri = format!("http://localhost:{port}/auth/callback");
    let pkce = generate_pkce();
    let state = random_urlsafe(32);
    let auth_url = build_authorize_url(&redirect_uri, &pkce.challenge, &state)?;

    ensure_login_active()?;
    webbrowser::open(auth_url.as_str()).map_err(|error| format!("打开登录页面失败：{error}"))?;

    let (request, code) =
        wait_for_authorization_code(&server, &state, LOGIN_TIMEOUT, &LOGIN_CANCELLED)?;
    if let Err(error) = ensure_login_active() {
        respond(request, 409, "Sign-in cancelled. Return to CSwitch.")?;
        return Err(error);
    }

    let client = http_client()?;
    let result = exchange_code(
        &client,
        &format!("{ISSUER}/oauth/token"),
        &redirect_uri,
        &pkce.verifier,
        &code,
    );
    match result {
        Ok(auth) => {
            if let Err(error) = ensure_login_active() {
                respond(request, 409, "Sign-in cancelled. Return to CSwitch.")?;
                return Err(error);
            }
            respond(request, 200, "Sign-in complete. You can close this page.")?;
            Ok(auth)
        }
        Err(error) => {
            respond(request, 502, "Token exchange failed. Return to CSwitch.")?;
            Err(error)
        }
    }
}

fn wait_for_authorization_code(
    server: &Server,
    expected_state: &str,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<(Request, String), Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        ensure_not_cancelled(cancelled)?;
        let now = Instant::now();
        if now >= deadline {
            return Err("官方登录等待超时，请重新点击官方登录".into());
        }
        let wait = (deadline - now).min(CALLBACK_POLL_INTERVAL);
        let request = server.recv_timeout(wait)?;
        if cancelled.load(Ordering::SeqCst) {
            if let Some(request) = request {
                respond(request, 409, "Sign-in cancelled. Return to CSwitch.")?;
            }
            return Err("官方登录已取消".into());
        }
        let Some(request) = request else {
            continue;
        };
        let callback = parse_callback(request.url(), expected_state);
        match callback {
            Callback::StateMismatch => {
                respond(request, 400, "Invalid OAuth state")?;
            }
            Callback::MissingCode => {
                respond(request, 400, "Missing authorization code")?;
            }
            Callback::Rejected(reason) => {
                respond(request, 400, "OpenAI sign-in was not completed")?;
                return Err(format!("官方登录未完成：{reason}").into());
            }
            Callback::Code(code) => {
                return Ok((request, code));
            }
        }
    }
}

fn ensure_not_cancelled(cancelled: &AtomicBool) -> Result<(), Box<dyn Error>> {
    if cancelled.load(Ordering::SeqCst) {
        return Err("官方登录已取消".into());
    }
    Ok(())
}

fn http_client() -> Result<Client, Box<dyn Error>> {
    let mut headers = HeaderMap::new();
    headers.insert("originator", HeaderValue::from_static(ORIGINATOR));
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    let mut builder = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(oauth_user_agent())
        .default_headers(headers);
    if let Some(proxy_url) = detect_outbound_proxy() {
        let proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|error| format!("系统代理无效（{proxy_url}）：{error}"))?
            .no_proxy(reqwest::NoProxy::from_string(&no_proxy_list()));
        builder = builder.proxy(proxy);
    }
    Ok(builder.build()?)
}

fn oauth_user_agent() -> String {
    format!("{ORIGINATOR}/{CODEX_CLI_VERSION}")
}

fn refresh_auth_with(
    client: &Client,
    token_url: &str,
    auth: &[u8],
) -> Result<AuthHealth, Box<dyn Error>> {
    let mut document: Value = match serde_json::from_slice(auth) {
        Ok(Value::Object(object)) => Value::Object(object),
        _ => return Ok(AuthHealth::Invalid),
    };
    let Some(refresh_token) = token_field(&document, "refresh_token") else {
        return Ok(AuthHealth::Invalid);
    };
    if refresh_token.trim().is_empty() {
        return Ok(AuthHealth::Invalid);
    }

    let response = post_token_form(
        client,
        token_url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", CLIENT_ID),
            ("scope", "openid profile email"),
        ],
    )?;
    let status = response.status();
    let body = response.bytes()?;
    if !status.is_success() {
        if status == StatusCode::UNAUTHORIZED || is_permanent_refresh_failure(&body) {
            return Ok(AuthHealth::Invalid);
        }
        return Err(token_service_error("官方登录测活", status, &body).into());
    }

    let refreshed: TokenResponse =
        serde_json::from_slice(&body).map_err(|error| format!("官方登录测活响应无效：{error}"))?;
    update_auth_document(&mut document, refreshed)?;
    Ok(AuthHealth::Valid(serde_json::to_vec_pretty(&document)?))
}

fn exchange_code(
    client: &Client,
    token_url: &str,
    redirect_uri: &str,
    verifier: &str,
    code: &str,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let response = post_token_form(
        client,
        token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ],
    )?;
    let status = response.status();
    let body = response.bytes()?;
    if !status.is_success() {
        return Err(token_service_error("官方登录令牌交换", status, &body).into());
    }
    let tokens: TokenResponse =
        serde_json::from_slice(&body).map_err(|error| format!("官方登录令牌响应无效：{error}"))?;
    let id_token = required_response_token(tokens.id_token, "id_token")?;
    let access_token = required_response_token(tokens.access_token, "access_token")?;
    let refresh_token = required_response_token(tokens.refresh_token, "refresh_token")?;
    let account_id = account_id_from_jwt(&id_token).or_else(|| account_id_from_jwt(&access_token));
    let auth = json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": refresh_token,
            "account_id": account_id,
        },
        "last_refresh": Utc::now().to_rfc3339(),
    });
    Ok(serde_json::to_vec_pretty(&auth)?)
}

fn post_token_form(
    client: &Client,
    token_url: &str,
    fields: &[(&str, &str)],
) -> Result<reqwest::blocking::Response, Box<dyn Error>> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().copied())
        .finish();
    Ok(client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()?)
}

fn detect_outbound_proxy() -> Option<String> {
    const KEYS: [&str; 6] = [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ];
    for key in KEYS {
        if let Ok(value) = env::var(key)
            && let Some(url) = normalize_proxy_url(&value)
        {
            return Some(url);
        }
    }
    #[cfg(windows)]
    {
        windows_system_proxy()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn no_proxy_list() -> String {
    match env::var("NO_PROXY")
        .ok()
        .or_else(|| env::var("no_proxy").ok())
    {
        Some(extra) if !extra.trim().is_empty() => format!("{extra},{LOCAL_NO_PROXY}"),
        _ => LOCAL_NO_PROXY.to_string(),
    }
}

fn normalize_proxy_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let url = if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    let parsed = Url::parse(&url).ok()?;
    matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h").then_some(url)
}

fn parse_windows_proxy_server(server: &str) -> Option<String> {
    let server = server.trim();
    if server.is_empty() {
        return None;
    }
    if !server.contains('=') {
        return normalize_proxy_url(server);
    }
    let mut http = None;
    let mut socks = None;
    for part in server.split(';') {
        let Some((scheme, address)) = part.split_once('=') else {
            continue;
        };
        let address = address.trim();
        if address.is_empty() {
            continue;
        }
        match scheme.trim().to_ascii_lowercase().as_str() {
            "https" => http = normalize_proxy_url(address),
            "http" if http.is_none() => http = normalize_proxy_url(address),
            "socks" | "socks5" | "socks5h" => {
                socks = Some(if address.contains("://") {
                    address.to_string()
                } else {
                    format!("socks5://{address}")
                })
            }
            _ => {}
        }
    }
    http.or(socks)
}

#[cfg(windows)]
fn windows_system_proxy() -> Option<String> {
    let key = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    let enabled: u32 = key.get_value("ProxyEnable").ok()?;
    if enabled == 0 {
        return None;
    }
    let server: String = key.get_value("ProxyServer").ok()?;
    parse_windows_proxy_server(&server)
}

fn token_service_error(action: &str, status: StatusCode, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if looks_like_waf(&text) {
        return format!(
            "{action}失败，令牌服务返回 {status}。请求很可能被 Cloudflare 拦截；谷歌登录本身可用，请确认 CSwitch 能走系统代理后重试"
        );
    }
    let detail = oauth_error_detail(body);
    if status == StatusCode::FORBIDDEN {
        return match detail {
            Some(detail) => format!(
                "{action}失败，令牌服务返回 {status}：{detail}。这与用谷歌还是邮箱登录无关，通常是换票请求未走系统代理"
            ),
            None => format!(
                "{action}失败，令牌服务返回 {status}。这与用谷歌还是邮箱登录无关，通常是换票请求未走系统代理"
            ),
        };
    }
    match detail {
        Some(detail) => format!("{action}失败，令牌服务返回 {status}：{detail}"),
        None => format!("{action}失败，令牌服务返回 {status}"),
    }
}

fn looks_like_waf(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("just a moment")
        || lower.contains("cf-mitigated")
        || lower.contains("attention required")
        || lower.contains("cloudflare")
}

fn oauth_error_detail(body: &[u8]) -> Option<String> {
    let document = serde_json::from_slice::<Value>(body).ok()?;
    let description = document
        .get("error_description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let code = document.get("error").and_then(|error| match error {
        Value::String(code) => Some(code.as_str()),
        Value::Object(object) => object.get("code").and_then(Value::as_str),
        _ => None,
    });
    match (code, description) {
        (Some(code), Some(description)) => Some(format!("{code}: {description}")),
        (None, Some(description)) => Some(description),
        (Some(code), None) => Some(code.to_string()),
        (None, None) => None,
    }
}

fn required_response_token(value: Option<String>, name: &str) -> Result<String, Box<dyn Error>> {
    value
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| format!("官方登录令牌响应缺少 {name}").into())
}

fn update_auth_document(
    document: &mut Value,
    refreshed: TokenResponse,
) -> Result<(), Box<dyn Error>> {
    let old_id = token_field(document, "id_token");
    let old_access = token_field(document, "access_token");
    let old_refresh = token_field(document, "refresh_token");
    let id_token = refreshed.id_token.or(old_id);
    let access_token = refreshed.access_token.or(old_access);
    let refresh_token = refreshed.refresh_token.or(old_refresh);
    let id_token = required_response_token(id_token, "id_token")?;
    let access_token = required_response_token(access_token, "access_token")?;
    let refresh_token = required_response_token(refresh_token, "refresh_token")?;
    let account_id = existing_account_id(document)
        .or_else(|| account_id_from_jwt(&id_token))
        .or_else(|| account_id_from_jwt(&access_token));

    let object = document.as_object_mut().ok_or("auth.json 顶层必须是对象")?;
    object.insert(
        "auth_mode".to_string(),
        Value::String("chatgpt".to_string()),
    );
    object.insert(
        "last_refresh".to_string(),
        Value::String(Utc::now().to_rfc3339()),
    );
    let tokens = object
        .entry("tokens")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or("auth.json 中的 tokens 必须是对象")?;
    tokens.insert("id_token".to_string(), Value::String(id_token.clone()));
    tokens.insert(
        "access_token".to_string(),
        Value::String(access_token.clone()),
    );
    tokens.insert(
        "refresh_token".to_string(),
        Value::String(refresh_token.clone()),
    );
    tokens.insert(
        "account_id".to_string(),
        account_id.clone().map(Value::String).unwrap_or(Value::Null),
    );

    // Older exported sessions used flat token fields. Keep those fields current when present.
    for (name, value) in [
        ("id_token", id_token),
        ("access_token", access_token),
        ("refresh_token", refresh_token),
    ] {
        if object.contains_key(name) {
            object.insert(name.to_string(), Value::String(value));
        }
    }
    if object.contains_key("chatgpt_account_id")
        && let Some(account_id) = account_id
    {
        object.insert("chatgpt_account_id".to_string(), Value::String(account_id));
    }
    Ok(())
}

fn token_field(document: &Value, name: &str) -> Option<String> {
    document
        .get("tokens")
        .and_then(|tokens| tokens.get(name))
        .or_else(|| document.get(name))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn existing_account_id(document: &Value) -> Option<String> {
    document
        .get("tokens")
        .and_then(|tokens| tokens.get("account_id"))
        .or_else(|| document.get("chatgpt_account_id"))
        .or_else(|| document.get("account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn account_id_from_jwt(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .or_else(|| claims.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn jwt_expiration(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_i64()
}

fn is_permanent_refresh_failure(body: &[u8]) -> bool {
    let Ok(document) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let code = document
        .get("error")
        .and_then(|error| match error {
            Value::Object(object) => object.get("code").and_then(Value::as_str),
            Value::String(code) => Some(code.as_str()),
            _ => None,
        })
        .or_else(|| document.get("code").and_then(Value::as_str));
    code.is_some_and(|code| {
        matches!(
            code.to_ascii_lowercase().as_str(),
            "refresh_token_expired" | "refresh_token_reused" | "refresh_token_invalidated"
        )
    })
}

fn bind_callback_server() -> Result<(Server, u16), Box<dyn Error>> {
    let mut errors = Vec::new();
    for port in CALLBACK_PORTS {
        match Server::http(format!("127.0.0.1:{port}")) {
            Ok(server) => return Ok((server, port)),
            Err(error) => errors.push(format!("{port}: {error}")),
        }
    }
    Err(format!("本地登录回调端口不可用：{}", errors.join("；")).into())
}

fn build_authorize_url(
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> Result<Url, Box<dyn Error>> {
    let mut url = Url::parse(&format!("{ISSUER}/oauth/authorize"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair(
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        )
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "codex_cli_rs");
    Ok(url)
}

fn generate_pkce() -> PkceCodes {
    let verifier = random_urlsafe(64);
    let challenge = pkce_challenge(&verifier);
    PkceCodes {
        verifier,
        challenge,
    }
}

fn random_urlsafe(size: usize) -> String {
    let mut bytes = vec![0_u8; size];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn parse_callback(raw_url: &str, expected_state: &str) -> Callback {
    let Ok(url) = Url::parse(&format!("http://localhost{raw_url}")) else {
        return Callback::MissingCode;
    };
    if url.path() != "/auth/callback" {
        return Callback::MissingCode;
    }
    let params = url.query_pairs().into_owned().collect::<HashMap<_, _>>();
    let state = params.get("state").map(String::as_str);
    if state != Some(expected_state) {
        return Callback::StateMismatch;
    }
    if let Some(error) = params.get("error") {
        return Callback::Rejected(error.to_string());
    }
    params
        .get("code")
        .filter(|code| !code.is_empty())
        .map(|code| Callback::Code(code.to_string()))
        .unwrap_or(Callback::MissingCode)
}

fn respond(request: Request, status: u16, body: &str) -> Result<(), Box<dyn Error>> {
    request.respond(Response::from_string(body).with_status_code(TinyStatusCode(status)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn jwt(account_id: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
            }))
            .expect("serialize jwt payload"),
        );
        format!("{header}.{payload}.signature")
    }

    fn access_jwt(expires_at: i64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({"exp": expires_at})).expect("serialize access claims"),
        );
        format!("{header}.{payload}.signature")
    }

    fn mock_token_server(
        status: u16,
        response: Value,
    ) -> (String, thread::JoinHandle<HashMap<String, String>>) {
        let server = Server::http("127.0.0.1:0").expect("bind mock server");
        let address = server.server_addr().to_ip().expect("mock address");
        let url = format!("http://{address}/oauth/token");
        let handle = thread::spawn(move || {
            let mut request = server.recv().expect("receive refresh request");
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("read refresh body");
            request
                .respond(
                    Response::from_string(response.to_string())
                        .with_status_code(TinyStatusCode(status))
                        .with_header(
                            "Content-Type: application/json"
                                .parse::<tiny_http::Header>()
                                .expect("content type"),
                        ),
                )
                .expect("respond refresh");
            url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>()
        });
        (url, handle)
    }

    fn mock_exchange_server(
        response: Value,
    ) -> (String, thread::JoinHandle<HashMap<String, String>>) {
        let server = Server::http("127.0.0.1:0").expect("bind exchange server");
        let address = server.server_addr().to_ip().expect("exchange address");
        let url = format!("http://{address}/oauth/token");
        let handle = thread::spawn(move || {
            let mut request = server.recv().expect("receive exchange request");
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("read exchange body");
            request
                .respond(
                    Response::from_string(response.to_string()).with_header(
                        "Content-Type: application/json"
                            .parse::<tiny_http::Header>()
                            .expect("content type"),
                    ),
                )
                .expect("respond exchange");
            url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>()
        });
        (url, handle)
    }

    #[test]
    fn refreshes_nested_tokens_and_keeps_rotated_refresh_token() {
        let old_id = jwt("account-one");
        let new_id = jwt("account-one");
        let auth = serde_json::to_vec(&json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": old_id,
                "access_token": "old-access",
                "refresh_token": "old-refresh",
                "account_id": "account-one"
            }
        }))
        .expect("serialize auth");
        let (url, server) = mock_token_server(
            200,
            json!({
                "id_token": new_id,
                "access_token": "new-access",
                "refresh_token": "new-refresh"
            }),
        );
        let client = http_client().expect("http client");

        let AuthHealth::Valid(updated) =
            refresh_auth_with(&client, &url, &auth).expect("refresh auth")
        else {
            panic!("expected valid auth");
        };
        let updated: Value = serde_json::from_slice(&updated).expect("parse updated auth");
        assert_eq!(updated["tokens"]["access_token"], "new-access");
        assert_eq!(updated["tokens"]["refresh_token"], "new-refresh");
        let request = server.join().expect("join mock server");
        assert_eq!(
            request.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            request.get("client_id").map(String::as_str),
            Some(CLIENT_ID)
        );
        assert_eq!(
            request.get("refresh_token").map(String::as_str),
            Some("old-refresh")
        );
        assert_eq!(
            request.get("scope").map(String::as_str),
            Some("openid profile email")
        );
    }

    #[test]
    fn current_access_token_is_reused_without_refresh() {
        let auth = serde_json::to_vec(&json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": access_jwt(Utc::now().timestamp() + 60 * 60),
                "refresh_token": "refresh"
            }
        }))
        .expect("serialize auth");

        assert_eq!(inspect_auth(&auth), LocalAuthState::Current);
    }

    #[test]
    fn expiring_access_token_requires_refresh() {
        let auth = serde_json::to_vec(&json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": access_jwt(Utc::now().timestamp() + 60),
                "refresh_token": "refresh"
            }
        }))
        .expect("serialize auth");

        assert_eq!(inspect_auth(&auth), LocalAuthState::NeedsRefresh);
    }

    #[test]
    fn api_key_auth_is_not_an_official_candidate() {
        let auth =
            serde_json::to_vec(&json!({"OPENAI_API_KEY": "fixture-key"})).expect("serialize auth");

        assert_eq!(inspect_auth(&auth), LocalAuthState::Invalid);
    }

    #[test]
    fn exchanges_authorization_code_with_official_pkce_form() {
        let id_token = jwt("account-exchange");
        let (url, server) = mock_exchange_server(json!({
            "id_token": id_token,
            "access_token": "exchange-access",
            "refresh_token": "exchange-refresh"
        }));
        let client = http_client().expect("http client");

        let auth = exchange_code(
            &client,
            &url,
            "http://localhost:1455/auth/callback",
            "fixture-verifier",
            "fixture-code",
        )
        .expect("exchange code");
        let auth: Value = serde_json::from_slice(&auth).expect("parse exchanged auth");
        assert_eq!(auth["auth_mode"], "chatgpt");
        assert_eq!(auth["tokens"]["account_id"], "account-exchange");
        assert_eq!(auth["tokens"]["access_token"], "exchange-access");

        let form = server.join().expect("join exchange server");
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some("authorization_code")
        );
        assert_eq!(form.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(form.get("code").map(String::as_str), Some("fixture-code"));
        assert_eq!(
            form.get("code_verifier").map(String::as_str),
            Some("fixture-verifier")
        );
        assert_eq!(
            form.get("redirect_uri").map(String::as_str),
            Some("http://localhost:1455/auth/callback")
        );
    }

    #[test]
    fn accepts_flat_export_and_normalizes_it() {
        let id_token = jwt("account-flat");
        let auth = serde_json::to_vec(&json!({
            "id_token": id_token,
            "access_token": "flat-access",
            "refresh_token": "flat-refresh",
            "chatgpt_account_id": "account-flat"
        }))
        .expect("serialize auth");
        let (url, server) = mock_token_server(
            200,
            json!({"access_token": "new-access", "refresh_token": "new-refresh"}),
        );
        let client = http_client().expect("http client");

        let AuthHealth::Valid(updated) =
            refresh_auth_with(&client, &url, &auth).expect("refresh flat auth")
        else {
            panic!("expected valid auth");
        };
        let updated: Value = serde_json::from_slice(&updated).expect("parse updated auth");
        assert_eq!(updated["auth_mode"], "chatgpt");
        assert_eq!(updated["tokens"]["account_id"], "account-flat");
        assert_eq!(updated["access_token"], "new-access");
        server.join().expect("join mock server");
    }

    #[test]
    fn rejected_refresh_is_invalid_without_echoing_tokens() {
        let auth = serde_json::to_vec(&json!({
            "tokens": {
                "id_token": jwt("account-bad"),
                "access_token": "access-secret",
                "refresh_token": "refresh-secret"
            }
        }))
        .expect("serialize auth");
        let (url, server) = mock_token_server(
            401,
            json!({"error": "invalid_grant", "detail": "refresh-secret"}),
        );
        let client = http_client().expect("http client");

        assert_eq!(
            refresh_auth_with(&client, &url, &auth).expect("classify auth"),
            AuthHealth::Invalid
        );
        server.join().expect("join mock server");
    }

    #[test]
    fn known_refresh_token_failure_is_permanently_invalid() {
        let auth = serde_json::to_vec(&json!({
            "tokens": {
                "access_token": "access",
                "refresh_token": "refresh"
            }
        }))
        .expect("serialize auth");
        let (url, server) =
            mock_token_server(400, json!({"error": {"code": "refresh_token_reused"}}));
        let client = http_client().expect("http client");

        assert_eq!(
            refresh_auth_with(&client, &url, &auth).expect("classify auth"),
            AuthHealth::Invalid
        );
        server.join().expect("join mock server");
    }

    #[test]
    fn unknown_bad_request_is_transient_instead_of_forcing_login() {
        let auth = serde_json::to_vec(&json!({
            "tokens": {
                "access_token": "access",
                "refresh_token": "refresh"
            }
        }))
        .expect("serialize auth");
        let (url, server) = mock_token_server(400, json!({"error": "temporarily_unavailable"}));
        let client = http_client().expect("http client");

        let error = refresh_auth_with(&client, &url, &auth).expect_err("transient failure");
        assert!(error.to_string().contains("400"));
        server.join().expect("join mock server");
    }

    #[test]
    fn transient_refresh_error_does_not_echo_credentials() {
        let auth = serde_json::to_vec(&json!({
            "tokens": {
                "id_token": jwt("account-transient"),
                "access_token": "access-private-value",
                "refresh_token": "refresh-private-value"
            }
        }))
        .expect("serialize auth");
        let (url, server) = mock_token_server(
            500,
            json!({"error": "backend_failure", "detail": "refresh-private-value"}),
        );
        let client = http_client().expect("http client");

        let error = refresh_auth_with(&client, &url, &auth).expect_err("transient error");
        let message = error.to_string();
        assert!(!message.contains("access-private-value"));
        assert!(!message.contains("refresh-private-value"));
        server.join().expect("join mock server");
    }

    #[test]
    fn authorization_url_and_callback_enforce_pkce_and_state() {
        let challenge = pkce_challenge("fixture-verifier");
        let url = build_authorize_url(
            "http://localhost:1455/auth/callback",
            &challenge,
            "fixture-state",
        )
        .expect("build authorize url");
        let params = url
            .query_pairs()
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("state").map(String::as_str),
            Some("fixture-state")
        );
        assert!(matches!(
            parse_callback("/auth/callback?code=one&state=wrong", "fixture-state"),
            Callback::StateMismatch
        ));
        assert!(matches!(
            parse_callback("/auth/callback?code=one&state=fixture-state", "fixture-state"),
            Callback::Code(code) if code == "one"
        ));
    }

    #[test]
    fn callback_wait_stops_within_one_poll_after_cancellation() {
        let server = Server::http("127.0.0.1:0").expect("bind callback server");
        let cancelled = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&cancelled);
        let cancel_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            trigger.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        let error = wait_for_authorization_code(
            &server,
            "fixture-state",
            Duration::from_secs(5),
            cancelled.as_ref(),
        )
        .expect_err("cancel callback wait");

        cancel_thread.join().expect("join cancellation thread");
        assert_eq!(error.to_string(), "官方登录已取消");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn token_exchange_forbidden_does_not_blame_google_login() {
        let (url, server) = mock_token_server(403, json!({"error": "forbidden"}));
        let client = http_client().expect("http client");

        let error = exchange_code(
            &client,
            &url,
            "http://localhost:1455/auth/callback",
            "fixture-verifier",
            "fixture-code",
        )
        .expect_err("forbidden exchange");
        let message = error.to_string();
        assert!(message.contains("403"));
        assert!(message.contains("系统代理") || message.contains("谷歌"));
        server.join().expect("join mock server");
    }

    #[test]
    fn waf_challenge_error_does_not_echo_html_or_tokens() {
        let message = token_service_error(
            "官方登录令牌交换",
            StatusCode::FORBIDDEN,
            b"<html>Just a moment... cloudflare refresh-private-value</html>",
        );
        assert!(message.contains("Cloudflare"));
        assert!(!message.contains("refresh-private-value"));
        assert!(!message.contains("<html>"));
    }

    #[test]
    fn oauth_user_agent_matches_latest_stable_codex_cli() {
        assert_eq!(oauth_user_agent(), "codex_cli_rs/0.154.0");
    }

    #[test]
    fn windows_proxy_server_values_are_normalized() {
        assert_eq!(
            parse_windows_proxy_server("127.0.0.1:7890"),
            Some("http://127.0.0.1:7890".to_string())
        );
        assert_eq!(
            parse_windows_proxy_server("http=127.0.0.1:7890;https=127.0.0.1:7891"),
            Some("http://127.0.0.1:7891".to_string())
        );
        assert_eq!(
            parse_windows_proxy_server("socks=127.0.0.1:1080"),
            Some("socks5://127.0.0.1:1080".to_string())
        );
        assert_eq!(parse_windows_proxy_server(""), None);
    }
}
