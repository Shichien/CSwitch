//! Read the selected provider, not every credential in the configuration.
//! Import fixed credentials only; environment-based auth stays user-managed.
use crate::config::{api_key_from_auth, normalize_api_url, parse_config, validate_api_key};
use toml_edit::{Item, TableLike};

pub(crate) struct ImportCandidate {
    pub provider_id: String,
    pub name: String,
    pub api_url: String,
    pub api_key: String,
}

pub(crate) enum Inspection {
    Official,
    Ready(ImportCandidate),
    NeedsInput(String),
}

pub(crate) fn inspect(config: &str, auth: Option<&[u8]>) -> Inspection {
    match resolve(config, auth) {
        Ok(Some(candidate)) => Inspection::Ready(candidate),
        Ok(None) => Inspection::Official,
        Err(reason) => Inspection::NeedsInput(format!(
            "当前供应商自动导入未完成：{reason}；原配置保持原样"
        )),
    }
}

fn resolve(config: &str, auth: Option<&[u8]>) -> Result<Option<ImportCandidate>, String> {
    // TOML parser errors can contain the offending line, including credentials.
    let doc = parse_config(config).map_err(|_| "config.toml 格式解析失败".to_string())?;
    let id = string(doc.as_table(), "model_provider")?.unwrap_or("openai");
    if id == "openai" {
        return Ok(None);
    }
    let provider = doc
        .get("model_providers")
        .and_then(Item::as_table_like)
        .and_then(|providers| providers.get(id))
        .and_then(Item::as_table_like)
        .ok_or_else(|| format!("缺少 model_providers.{id} 配置"))?;
    let name = string(provider, "name")?
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(id)
        .trim();
    crate::config::validate_provider_name(name).map_err(|_| "供应商名称无效".to_string())?;
    let base = string(provider, "base_url")?.ok_or("当前提供方缺少 base_url")?;
    let api_url =
        normalize_api_url(base).map_err(|_| "当前提供方的 base_url 格式无效".to_string())?;
    if string(provider, "wire_api")?.unwrap_or("responses") != "responses" {
        return Err("当前 wire_api 不是 responses，请在 API 配置中确认接口协议".into());
    }
    if provider.contains_key("auth") || provider.contains_key("aws") {
        return Err("检测到 auth 命令或 aws 动态认证，需要在 API 配置中填写可用的固定 API Key；自动导入不会执行取令牌命令".into());
    }
    let storage = string(doc.as_table(), "cli_auth_credentials_store")?.unwrap_or("file");
    if storage != "file" {
        return Err(format!(
            "认证存储为 {storage}，本次导入使用 auth.json 文件认证，请先在 Codex 中选择文件认证后重新导入"
        ));
    }
    if provider.contains_key("env_key") {
        // Do not read the variable or silently substitute an unrelated file key.
        return Err("当前提供方使用环境变量认证，请在 API 配置中填写 API Key".into());
    }
    let key = if let Some(token) = string(provider, "experimental_bearer_token")? {
        token.to_string()
    } else {
        let value = auth
            .map(serde_json::from_slice::<serde_json::Value>)
            .transpose()
            .map_err(|_| "auth.json 格式解析失败".to_string())?;
        if value.as_ref().is_some_and(|value| {
            value
                .get("auth_mode")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|mode| mode != "apikey")
                || value.get("tokens").is_some_and(|tokens| !tokens.is_null())
                    && value
                        .get("OPENAI_API_KEY")
                        .and_then(serde_json::Value::as_str)
                        .is_none()
        }) {
            return Err("当前认证是官方登录或其他动态凭据，请单独填写此供应商的 API Key".into());
        }
        if let Some(key) = auth.and_then(|bytes| api_key_from_auth(bytes).ok().flatten()) {
            key
        } else if let Some(header) = authorization_header(provider)? {
            let (scheme, key) = header
                .split_once(' ')
                .ok_or("Authorization 请求头不是 Bearer 密钥")?;
            if !scheme.eq_ignore_ascii_case("bearer") {
                return Err(
                    "Authorization 请求头不是 Bearer 密钥，请在 API 配置中确认认证方式".into(),
                );
            }
            key.to_string()
        } else {
            return Err("未找到 API Key，请检查 auth.json 或固定 Authorization 请求头，或在 API 配置中填写密钥".into());
        }
    };
    let key = validate_api_key(&key)
        .map_err(|_| "选中的密钥为空或含空白字符，请检查密钥来源".to_string())?;
    if key == "PROXY_MANAGED" {
        return Err("检测到其他代理的占位凭据，请填写实际供应商地址和 API Key".into());
    }
    Ok(Some(ImportCandidate {
        provider_id: id.to_string(),
        name: name.to_string(),
        api_url,
        api_key: key.to_string(),
    }))
}

fn string<'a>(table: &'a dyn TableLike, field: &str) -> Result<Option<&'a str>, String> {
    table
        .get(field)
        .map(|value| value.as_str().ok_or_else(|| format!("{field} 应为字符串")))
        .transpose()
}

fn authorization_header(provider: &dyn TableLike) -> Result<Option<String>, String> {
    if provider
        .get("env_http_headers")
        .and_then(Item::as_table_like)
        .is_some_and(|headers| {
            headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        })
    {
        return Err("当前 Authorization 使用环境变量，请在 API 配置中填写 API Key".into());
    }
    let Some(item) = provider.get("http_headers") else {
        return Ok(None);
    };
    let table = item.as_table_like().ok_or("http_headers 应为表")?;
    let mut headers = table
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"));
    let Some((_, value)) = headers.next() else {
        return Ok(None);
    };
    if headers.next().is_some() {
        return Err("http_headers 存在大小写重复的 Authorization".into());
    }
    value
        .as_str()
        .map(|value| Some(value.to_string()))
        .ok_or_else(|| "http_headers.Authorization 应为字符串".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: &str) -> String {
        format!(
            "model_provider = 'my-provider'\n[model_providers.my-provider]\nname = 'Fixture'\nbase_url = 'https://fixture.test/v1'\n{extra}"
        )
    }

    fn key(text: &str, auth: Option<&[u8]>) -> String {
        match inspect(text, auth) {
            Inspection::Ready(candidate) => candidate.api_key,
            Inspection::NeedsInput(reason) => panic!("{reason}"),
            Inspection::Official => panic!("expected provider"),
        }
    }

    #[test]
    fn resolves_fixed_bearer_file_and_headers() {
        let auth = Some(br#"{"OPENAI_API_KEY":"file-key"}"#.as_slice());
        assert_eq!(
            key(&config("experimental_bearer_token = 'bearer-key'"), auth),
            "bearer-key"
        );
        assert_eq!(
            key(
                &config("http_headers = { Authorization = 'Bearer header-key' }"),
                auth
            ),
            "file-key"
        );
        assert_eq!(
            key(
                &config("http_headers = { Authorization = 'Bearer header-key' }"),
                None
            ),
            "header-key"
        );
        assert_eq!(key(&config(""), auth), "file-key");
    }

    #[test]
    fn environment_auth_is_left_for_manual_configuration() {
        for auth in [
            None,
            Some(br#"{"OPENAI_API_KEY":"secret-file-key"}"#.as_slice()),
        ] {
            let Inspection::NeedsInput(reason) = inspect(
                &config("env_key = 'REQUIRED'\nexperimental_bearer_token = 'secret-bearer'"),
                auth,
            ) else {
                panic!("expected manual input")
            };
            assert!(reason.contains("API 配置"));
            assert!(!reason.contains("secret"));
        }
        let Inspection::NeedsInput(reason) = inspect(
            &config(
                "http_headers = { Authorization = 'Bearer secret-header' }\nenv_http_headers = { Authorization = 'AUTH_HEADER' }",
            ),
            None,
        ) else {
            panic!("expected manual input")
        };
        assert!(reason.contains("API 配置"));
        assert!(!reason.contains("secret"));
        // Non-auth environment headers do not interfere with a fixed credential.
        assert_eq!(
            key(
                &config(
                    "http_headers = { Authorization = 'Bearer header-key' }\nenv_http_headers = { 'X-Tenant' = 'TENANT' }"
                ),
                None
            ),
            "header-key"
        );
    }

    #[test]
    fn handles_quoted_unicode_ids_inline_tables_and_missing_display_name() {
        let text = "model_provider = '我的.provider'\nmodel_providers = { '我的.provider' = { base_url = 'https://fixture.test', experimental_bearer_token = 'fixture-key' } }";
        let Inspection::Ready(value) = inspect(text, None) else {
            panic!("expected candidate")
        };
        assert_eq!(value.provider_id, "我的.provider");
        assert_eq!(value.name, "我的.provider");
    }

    #[test]
    fn dynamic_credentials_and_proxy_placeholders_need_explicit_input() {
        for extra in [
            "auth = { command = 'do-not-run' }",
            "aws = { profile = 'test' }",
            "experimental_bearer_token = 'PROXY_MANAGED'",
            "http_headers = { Authorization = 'Basic secret' }",
            "wire_api = 'chat'",
        ] {
            assert!(matches!(
                inspect(&config(extra), None),
                Inspection::NeedsInput(_)
            ));
        }
        for storage in ["keyring", "auto", "ephemeral"] {
            assert!(matches!(
                inspect(
                    &format!("cli_auth_credentials_store = '{storage}'\n{}", config("")),
                    Some(br#"{"OPENAI_API_KEY":"stale-file"}"#)
                ),
                Inspection::NeedsInput(_)
            ));
        }
    }

    #[test]
    fn oauth_and_inactive_providers_are_not_imported_as_api_keys() {
        assert!(matches!(
            inspect(
                "[model_providers.inactive]\nbase_url='https://unused.test'\nexperimental_bearer_token='secret'",
                None
            ),
            Inspection::Official
        ));
        let auth = br#"{"auth_mode":"chatgpt","OPENAI_API_KEY":"stale-key","tokens":{"access_token":"official-token"}}"#;
        assert!(matches!(
            inspect(&config(""), Some(auth)),
            Inspection::NeedsInput(_)
        ));
        assert_eq!(
            key(
                &config("experimental_bearer_token = 'provider-key'"),
                Some(auth)
            ),
            "provider-key"
        );
    }
}
