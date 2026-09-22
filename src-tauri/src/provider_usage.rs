use reqwest::blocking::Client;
use serde::Serialize;
use serde_json::Value;
use std::error::Error;
use std::io::Read;
use std::time::Duration;

const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const NEW_API_QUOTA_PER_USD: f64 = 500_000.0;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderUsageStatus {
    Available,
    Unsupported,
    Unauthorized,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderUsage {
    pub(crate) provider_id: String,
    pub(crate) status: ProviderUsageStatus,
    pub(crate) system: Option<String>,
    pub(crate) balance: Option<f64>,
    pub(crate) total: Option<f64>,
    pub(crate) used: Option<f64>,
    pub(crate) unit: Option<String>,
    pub(crate) unlimited: bool,
    pub(crate) plan: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) queried_at: i64,
}

#[derive(Debug)]
enum ProbeResult {
    Available(UsageValues),
    Unsupported,
    Unauthorized,
    Unknown(&'static str),
}

#[derive(Debug)]
struct UsageValues {
    system: &'static str,
    balance: Option<f64>,
    total: Option<f64>,
    used: Option<f64>,
    unit: Option<&'static str>,
    unlimited: bool,
    plan: Option<String>,
    message: Option<String>,
}

struct JsonResponse {
    status: reqwest::StatusCode,
    body: Option<Value>,
}

pub(crate) fn query(provider_id: &str, api_url: &str, api_key: &str) -> ProviderUsage {
    let client = match crate::network::blocking_builder(QUERY_TIMEOUT).and_then(|builder| {
        Ok(builder
            .user_agent(concat!("CSwitch/", env!("CARGO_PKG_VERSION")))
            .build()?)
    }) {
        Ok(client) => client,
        Err(_) => return unknown(provider_id, "创建余额查询连接失败"),
    };

    let mut unauthorized = false;
    let mut temporary = None;
    for result in [
        probe_sub2api(&client, &sub2api_endpoint(api_url), api_key),
        probe_new_api(&client, &new_api_endpoint(api_url), api_key),
    ] {
        match result {
            ProbeResult::Available(values) => return available(provider_id, values),
            ProbeResult::Unauthorized => unauthorized = true,
            ProbeResult::Unknown(message) => {
                temporary.get_or_insert(message);
            }
            ProbeResult::Unsupported => {}
        }
    }

    if unauthorized {
        ProviderUsage {
            provider_id: provider_id.to_string(),
            status: ProviderUsageStatus::Unauthorized,
            system: None,
            balance: None,
            total: None,
            used: None,
            unit: None,
            unlimited: false,
            plan: None,
            message: Some("API Key 被余额接口拒绝".into()),
            queried_at: now(),
        }
    } else if let Some(message) = temporary {
        unknown(provider_id, message)
    } else {
        ProviderUsage {
            provider_id: provider_id.to_string(),
            status: ProviderUsageStatus::Unsupported,
            system: None,
            balance: None,
            total: None,
            used: None,
            unit: None,
            unlimited: false,
            plan: None,
            message: None,
            queried_at: now(),
        }
    }
}

fn probe_sub2api(client: &Client, endpoint: &str, api_key: &str) -> ProbeResult {
    let response = match get_json(client, endpoint, api_key) {
        Ok(response) => response,
        Err(_) => return ProbeResult::Unknown("连接余额接口失败"),
    };
    if missing_endpoint(response.status) {
        return ProbeResult::Unsupported;
    }
    let body = response.body.as_ref();
    if matches!(response.status.as_u16(), 401 | 403) {
        return if body.is_some_and(is_sub2api_error) {
            ProbeResult::Unauthorized
        } else {
            ProbeResult::Unsupported
        };
    }
    if response.status.as_u16() == 429 || response.status.is_server_error() {
        return ProbeResult::Unknown("余额接口暂时不可用");
    }
    if !response.status.is_success() {
        return ProbeResult::Unsupported;
    }
    body.and_then(parse_sub2api)
        .map(ProbeResult::Available)
        .unwrap_or(ProbeResult::Unsupported)
}

fn probe_new_api(client: &Client, endpoint: &str, api_key: &str) -> ProbeResult {
    let response = match get_json(client, endpoint, api_key) {
        Ok(response) => response,
        Err(_) => return ProbeResult::Unknown("连接余额接口失败"),
    };
    if missing_endpoint(response.status) {
        return ProbeResult::Unsupported;
    }
    let body = response.body.as_ref();
    if matches!(response.status.as_u16(), 401 | 403) {
        return if body.is_some_and(is_new_api_error) {
            ProbeResult::Unauthorized
        } else {
            ProbeResult::Unsupported
        };
    }
    if response.status.as_u16() == 429 || response.status.is_server_error() {
        return ProbeResult::Unknown("余额接口暂时不可用");
    }
    if !response.status.is_success() {
        return ProbeResult::Unsupported;
    }
    if let Some(values) = body.and_then(parse_new_api) {
        return ProbeResult::Available(values);
    }
    if body.is_some_and(is_new_api_error) {
        return ProbeResult::Unauthorized;
    }
    ProbeResult::Unsupported
}

fn get_json(
    client: &Client,
    endpoint: &str,
    api_key: &str,
) -> Result<JsonResponse, Box<dyn Error>> {
    let response = client
        .get(endpoint)
        .bearer_auth(api_key)
        .send()
        .map_err(|error| error.without_url())?;
    let status = response.status();
    let mut bytes = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err("余额接口响应超过 256 KiB".into());
    }
    let body = if bytes.is_empty() {
        None
    } else {
        serde_json::from_slice(&bytes).ok()
    };
    Ok(JsonResponse { status, body })
}

fn parse_sub2api(body: &Value) -> Option<UsageValues> {
    let object = body.as_object()?;
    let mode = object.get("mode")?.as_str()?;
    object.get("isValid").and_then(Value::as_bool)?;
    match mode {
        "quota_limited" if object.get("status").and_then(Value::as_str).is_some() => {}
        "unrestricted" if object.get("planName").and_then(Value::as_str).is_some() => {}
        _ => return None,
    }
    if object
        .get("unit")
        .and_then(Value::as_str)
        .is_some_and(|unit| unit != "USD")
    {
        return None;
    }
    let quota = object.get("quota").and_then(Value::as_object);
    let balance = number(object.get("remaining"))
        .or_else(|| number(object.get("balance")))
        .or_else(|| quota.and_then(|quota| number(quota.get("remaining"))));
    let total = quota.and_then(|quota| number(quota.get("limit")));
    let used = quota.and_then(|quota| number(quota.get("used")));
    let plan = object
        .get("planName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let message = (balance.is_none() && total.is_none()).then(|| {
        if mode == "quota_limited" {
            "此密钥按周期限额，接口未返回总余额".to_string()
        } else {
            "余额接口已连接，当前套餐未返回可用余额".to_string()
        }
    });
    Some(UsageValues {
        system: "SUB2API",
        balance,
        total,
        used,
        unit: Some("USD"),
        unlimited: false,
        plan,
        message,
    })
}

fn parse_new_api(body: &Value) -> Option<UsageValues> {
    if body.get("code").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let data = body.get("data")?.as_object()?;
    if data.get("object").and_then(Value::as_str) != Some("token_usage") {
        return None;
    }
    let unlimited = data
        .get("unlimited_quota")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let convert = |field| number(data.get(field)).map(|value| value / NEW_API_QUOTA_PER_USD);
    let balance = (!unlimited).then(|| convert("total_available")).flatten();
    Some(UsageValues {
        system: "NewAPI",
        balance,
        total: (!unlimited).then(|| convert("total_granted")).flatten(),
        used: convert("total_used"),
        unit: Some("USD"),
        unlimited,
        plan: data
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        message: None,
    })
}

fn is_sub2api_error(body: &Value) -> bool {
    let error = body.get("error").unwrap_or(body);
    error
        .get("code")
        .and_then(Value::as_str)
        .is_some_and(|code| {
            matches!(
                code,
                "API_KEY_REQUIRED"
                    | "INVALID_API_KEY"
                    | "API_KEY_DISABLED"
                    | "API_KEY_EXPIRED"
                    | "API_KEY_AUTH_OVERLOADED"
            )
        })
        || error
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "authentication_error")
}

fn is_new_api_error(body: &Value) -> bool {
    (body.get("success").and_then(Value::as_bool) == Some(false)
        || body.get("code").and_then(Value::as_bool) == Some(false))
        && body.get("message").and_then(Value::as_str).is_some()
}

fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn available(provider_id: &str, values: UsageValues) -> ProviderUsage {
    ProviderUsage {
        provider_id: provider_id.to_string(),
        status: ProviderUsageStatus::Available,
        system: Some(values.system.into()),
        balance: values.balance,
        total: values.total,
        used: values.used,
        unit: values.unit.map(str::to_string),
        unlimited: values.unlimited,
        plan: values.plan,
        message: values.message,
        queried_at: now(),
    }
}

fn unknown(provider_id: &str, message: impl Into<String>) -> ProviderUsage {
    ProviderUsage {
        provider_id: provider_id.to_string(),
        status: ProviderUsageStatus::Unknown,
        system: None,
        balance: None,
        total: None,
        used: None,
        unit: None,
        unlimited: false,
        plan: None,
        message: Some(message.into()),
        queried_at: now(),
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn missing_endpoint(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 404 | 405 | 410)
}

fn sub2api_endpoint(api_url: &str) -> String {
    let base = api_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/usage")
    } else {
        format!("{base}/v1/usage")
    }
}

fn new_api_endpoint(api_url: &str) -> String {
    let base = api_url.trim_end_matches('/');
    let root = base.strip_suffix("/v1").unwrap_or(base);
    format!("{root}/api/usage/token/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use tiny_http::{Header, Response, Server, StatusCode};

    fn json_response(body: &str, status: u16) -> Response<std::io::Cursor<Vec<u8>>> {
        Response::from_string(body)
            .with_status_code(StatusCode(status))
            .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
    }

    fn assert_bearer(request: &tiny_http::Request) {
        let auth = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Authorization"))
            .expect("authorization header");
        assert_eq!(auth.value.as_str(), "Bearer fixture-key");
    }

    #[test]
    fn root_and_v1_urls_do_not_duplicate_v1() {
        assert_eq!(
            sub2api_endpoint("https://example.com"),
            "https://example.com/v1/usage"
        );
        assert_eq!(
            sub2api_endpoint("https://example.com/v1"),
            "https://example.com/v1/usage"
        );
        assert_eq!(
            new_api_endpoint("https://example.com"),
            "https://example.com/api/usage/token/"
        );
        assert_eq!(
            new_api_endpoint("https://example.com/v1"),
            "https://example.com/api/usage/token/"
        );
    }

    #[test]
    fn reads_sub2api_wallet_balance_with_the_saved_api_key() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let task = thread::spawn(move || {
            let request = server.recv().unwrap();
            assert_eq!(request.url(), "/v1/usage");
            assert_bearer(&request);
            request
                .respond(json_response(
                    r#"{"mode":"unrestricted","isValid":true,"planName":"钱包余额","remaining":12.5,"balance":12.5,"unit":"USD"}"#,
                    200,
                ))
                .unwrap();
        });
        let usage = query("provider", &base, "fixture-key");
        assert_eq!(usage.status, ProviderUsageStatus::Available);
        assert_eq!(usage.system.as_deref(), Some("SUB2API"));
        assert_eq!(usage.balance, Some(12.5));
        assert_eq!(usage.unit.as_deref(), Some("USD"));
        task.join().unwrap();
    }

    #[test]
    fn reads_sub2api_key_quota_and_recognizes_rate_limit_only_keys() {
        for (body, balance, total, used, message) in [
            (
                r#"{"mode":"quota_limited","isValid":true,"status":"active","quota":{"limit":20,"used":7.5,"remaining":12.5,"unit":"USD"},"remaining":12.5,"unit":"USD"}"#,
                Some(12.5),
                Some(20.0),
                Some(7.5),
                None,
            ),
            (
                r#"{"mode":"quota_limited","isValid":true,"status":"active","rate_limits":[{"window":"5h","limit":100,"used":12,"remaining":88}]}"#,
                None,
                None,
                None,
                Some("此密钥按周期限额，接口未返回总余额"),
            ),
        ] {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let task = thread::spawn(move || {
                server
                    .recv()
                    .unwrap()
                    .respond(json_response(body, 200))
                    .unwrap();
            });
            let usage = query("provider", &base, "fixture-key");
            assert_eq!(usage.status, ProviderUsageStatus::Available);
            assert_eq!(usage.system.as_deref(), Some("SUB2API"));
            assert_eq!(usage.balance, balance);
            assert_eq!(usage.total, total);
            assert_eq!(usage.used, used);
            assert_eq!(usage.message.as_deref(), message);
            task.join().unwrap();
        }
    }

    #[test]
    fn falls_through_to_new_api_and_converts_quota_to_usd() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", server.server_addr());
        let task = thread::spawn(move || {
            let first = server.recv().unwrap();
            assert_eq!(first.url(), "/v1/usage");
            assert_bearer(&first);
            first.respond(Response::empty(StatusCode(404))).unwrap();
            let second = server.recv().unwrap();
            assert_eq!(second.url(), "/api/usage/token/");
            assert_bearer(&second);
            second
                .respond(json_response(
                    r#"{"code":true,"message":"ok","data":{"object":"token_usage","name":"Codex","total_granted":5000000,"total_used":1250000,"total_available":3750000,"unlimited_quota":false}}"#,
                    200,
                ))
                .unwrap();
        });
        let usage = query("provider", &base, "fixture-key");
        assert_eq!(usage.status, ProviderUsageStatus::Available);
        assert_eq!(usage.system.as_deref(), Some("NewAPI"));
        assert_eq!(usage.balance, Some(7.5));
        assert_eq!(usage.total, Some(10.0));
        assert_eq!(usage.used, Some(2.5));
        task.join().unwrap();
    }

    #[test]
    fn reports_new_api_unlimited_tokens_without_inventing_a_balance() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let task = thread::spawn(move || {
            server
                .recv()
                .unwrap()
                .respond(Response::empty(StatusCode(404)))
                .unwrap();
            server
                .recv()
                .unwrap()
                .respond(json_response(
                    r#"{"code":true,"message":"ok","data":{"object":"token_usage","name":"无限令牌","total_granted":0,"total_used":1000000,"total_available":0,"unlimited_quota":true}}"#,
                    200,
                ))
                .unwrap();
        });
        let usage = query("provider", &base, "fixture-key");
        assert_eq!(usage.status, ProviderUsageStatus::Available);
        assert_eq!(usage.system.as_deref(), Some("NewAPI"));
        assert!(usage.unlimited);
        assert_eq!(usage.balance, None);
        assert_eq!(usage.total, None);
        assert_eq!(usage.used, Some(2.0));
        task.join().unwrap();
    }

    #[test]
    fn distinguishes_rejected_keys_missing_interfaces_and_temporary_failures() {
        for (sub_status, sub_body, new_status, new_body, expected) in [
            (
                401,
                r#"{"error":{"code":"INVALID_API_KEY","message":"Invalid API key"}}"#,
                404,
                "{}",
                ProviderUsageStatus::Unauthorized,
            ),
            (404, "{}", 404, "{}", ProviderUsageStatus::Unsupported),
            (
                503,
                r#"{"error":{"code":"API_KEY_AUTH_OVERLOADED","message":"secret fixture-key"}}"#,
                404,
                "{}",
                ProviderUsageStatus::Unknown,
            ),
        ] {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let task = thread::spawn(move || {
                server
                    .recv()
                    .unwrap()
                    .respond(json_response(sub_body, sub_status))
                    .unwrap();
                server
                    .recv()
                    .unwrap()
                    .respond(json_response(new_body, new_status))
                    .unwrap();
            });
            let usage = query("provider", &base, "fixture-key");
            assert_eq!(usage.status, expected);
            assert!(
                !usage
                    .message
                    .as_deref()
                    .unwrap_or_default()
                    .contains("fixture-key")
            );
            task.join().unwrap();
        }
    }

    #[test]
    fn similar_but_unrecognized_json_is_not_reported_as_a_supported_system() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let task = thread::spawn(move || {
            for _ in 0..2 {
                server
                    .recv()
                    .unwrap()
                    .respond(json_response(r#"{"remaining":99,"unit":"USD"}"#, 200))
                    .unwrap();
            }
        });
        let usage = query("provider", &base, "fixture-key");
        assert_eq!(usage.status, ProviderUsageStatus::Unsupported);
        task.join().unwrap();
    }
}
