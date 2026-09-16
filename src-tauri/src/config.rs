use serde_json::{Value, json};
use std::error::Error;
use toml_edit::{DocumentMut, Item, Table, value};
use url::Url;

const CUSTOM_PROVIDER_ID: &str = "custom";
const OFFICIAL_PROVIDER_ID: &str = "openai";

pub(crate) fn api_key_from_auth(auth: &[u8]) -> Result<Option<String>, Box<dyn Error>> {
    let document: Value = match serde_json::from_slice(auth) {
        Ok(Value::Object(document)) => Value::Object(document),
        _ => return Ok(None),
    };
    Ok(document
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string))
}

pub(crate) fn normalize_api_url(input: &str) -> Result<String, Box<dyn Error>> {
    let input = input.trim().trim_end_matches('/');
    let parsed = Url::parse(input).map_err(|error| format!("API URL 无效：{error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("API URL 只支持 http 或 https".into());
    }
    if parsed.host_str().is_none() || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("API URL 必须是没有查询参数和片段的基础地址".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("API URL 不能包含用户名或密码".into());
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

pub(crate) fn validate_api_key(input: &str) -> Result<&str, Box<dyn Error>> {
    let key = input.trim();
    if key.is_empty() {
        return Err("API Key 不能为空".into());
    }
    if key.chars().any(char::is_whitespace) {
        return Err("API Key 不能包含空白字符".into());
    }
    Ok(key)
}

pub(crate) fn validate_provider_name(input: &str) -> Result<&str, Box<dyn Error>> {
    let name = input.trim();
    if name.is_empty() {
        return Err("供应商名称不能为空".into());
    }
    if name.chars().count() > 80 {
        return Err("供应商名称不能超过 80 个字符".into());
    }
    if name.chars().any(char::is_control) {
        return Err("供应商名称不能包含控制字符".into());
    }
    Ok(name)
}

pub(crate) fn build_provider_config(
    original: &str,
    name: &str,
    api_url: &str,
    api_key: &str,
) -> Result<String, Box<dyn Error>> {
    validate_api_key(api_key)?;
    let mut document = parse_config(original)?;
    document.remove("experimental_bearer_token");
    document["model_provider"] = value(CUSTOM_PROVIDER_ID);
    if !document.contains_key("model_providers") {
        let mut providers = Table::new();
        providers.set_implicit(true);
        document["model_providers"] = Item::Table(providers);
    }
    let providers = document["model_providers"]
        .as_table_mut()
        .ok_or("config.toml 中的 model_providers 不是表")?;
    if !providers
        .get(CUSTOM_PROVIDER_ID)
        .is_some_and(|item| item.is_table())
    {
        providers[CUSTOM_PROVIDER_ID] = Item::Table(Table::new());
    }
    let provider = providers[CUSTOM_PROVIDER_ID]
        .as_table_mut()
        .ok_or("config.toml 中的 custom 供应商不是表")?;
    provider["name"] = value(name);
    provider["base_url"] = value(api_url);
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(true);
    provider.remove("experimental_bearer_token");
    Ok(document.to_string())
}

pub(crate) fn set_request_bearer(
    config: &str,
    token: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let mut document = parse_config(config)?;
    let provider = document["model_providers"]["custom"]
        .as_table_mut()
        .ok_or("缺少 custom 供应商配置")?;
    if let Some(token) = token {
        provider["experimental_bearer_token"] = value(validate_api_key(token)?);
    } else {
        provider.remove("experimental_bearer_token");
    }
    Ok(document.to_string())
}

pub(crate) fn build_custom_auth(api_key: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let api_key = validate_api_key(api_key)?;
    Ok(serde_json::to_vec_pretty(&json!({
        "OPENAI_API_KEY": api_key,
    }))?)
}

pub(crate) fn build_official_config(original: &str) -> Result<String, Box<dyn Error>> {
    let mut document = parse_config(original)?;
    document.remove("model_provider");
    document.remove("experimental_bearer_token");
    if let Some(providers) = document
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        && let Some(provider) = providers
            .get_mut(CUSTOM_PROVIDER_ID)
            .and_then(Item::as_table_mut)
    {
        provider.remove("experimental_bearer_token");
    }
    Ok(document.to_string())
}

pub(crate) fn parse_config(content: &str) -> Result<DocumentMut, Box<dyn Error>> {
    let content = content.trim_start_matches('\u{feff}');
    if content.trim().is_empty() {
        Ok(DocumentMut::new())
    } else {
        let mut document = content.parse::<DocumentMut>()?;
        if let Some(item) = document.get_mut("model_providers") {
            normalize_table(item);
            if let Some(providers) = item.as_table_mut()
                && let Some(custom) = providers.get_mut(CUSTOM_PROVIDER_ID)
            {
                normalize_table(custom);
            }
        }
        Ok(document)
    }
}

fn normalize_table(item: &mut Item) {
    if let Item::Value(toml_edit::Value::InlineTable(inline)) = item {
        *item = Item::Table(inline.clone().into_table());
    }
}

pub(crate) fn is_official_config(content: &str) -> Result<bool, Box<dyn Error>> {
    let document = parse_config(content)?;
    Ok(document
        .get("model_provider")
        .and_then(Item::as_str)
        .is_none_or(|provider| provider == OFFICIAL_PROVIDER_ID))
}

pub(crate) fn config_selects_custom(content: &[u8]) -> Result<bool, Box<dyn Error>> {
    let content = std::str::from_utf8(content)?;
    let document = parse_config(content)?;
    Ok(document.get("model_provider").and_then(Item::as_str) == Some(CUSTOM_PROVIDER_ID))
}

pub(crate) fn custom_provider(document: &DocumentMut) -> Option<&Table> {
    document
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(CUSTOM_PROVIDER_ID))
        .and_then(Item::as_table)
}

pub(crate) fn custom_bearer_token(document: &DocumentMut) -> Option<&str> {
    custom_provider(document)
        .and_then(|provider| provider.get("experimental_bearer_token"))
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

pub(crate) fn verify_provider_content(
    config: &[u8],
    auth: &[u8],
    expected_name: &str,
    expected_url: &str,
) -> Result<(), Box<dyn Error>> {
    let content = std::str::from_utf8(config)?;
    let document = parse_config(content)?;
    let provider = custom_provider(&document).ok_or("写入后的 custom 提供方不存在")?;
    let expected_key = api_key_from_auth(auth)?.ok_or("供应商缺少 API Key")?;
    let auth: Value = serde_json::from_slice(auth)?;
    let auth_has_only_api_key = auth.as_object().is_some_and(|object| {
        object.len() == 1
            && object
                .get("OPENAI_API_KEY")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.trim().is_empty())
    });
    let valid = document.get("model_provider").and_then(Item::as_str) == Some(CUSTOM_PROVIDER_ID)
        && provider.get("name").and_then(Item::as_str) == Some(expected_name)
        && provider.get("base_url").and_then(Item::as_str) == Some(expected_url)
        && provider.get("wire_api").and_then(Item::as_str) == Some("responses")
        && provider.get("requires_openai_auth").and_then(Item::as_bool) == Some(true)
        && provider
            .get("experimental_bearer_token")
            .is_none_or(|token| token.as_str() == Some(expected_key.as_str()))
        && auth_has_only_api_key;
    if !valid {
        return Err("配置写入后的验证未通过".into());
    }
    Ok(())
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    #[test]
    fn official_switch_preserves_user_catalog_even_inside_profile_storage() {
        let original = "model_provider = 'custom'\nmodel_catalog_json = '/home/user/.codex/cswitch-profiles/providers/saved/models-user.json'\n";
        let updated = build_official_config(original).unwrap();
        assert_eq!(
            parse_config(&updated).unwrap()["model_catalog_json"].as_str(),
            parse_config(original).unwrap()["model_catalog_json"].as_str()
        );
    }
    #[test]
    fn preserves_bom_and_inline_provider_settings() {
        let source = "\u{feff}model_providers = { custom = { name = 'old', request_max_retries = 9 }, other = { name = 'other' } }\n";
        let updated =
            build_provider_config(source, "new", "http://localhost", "fixture-key").unwrap();
        let document = parse_config(&updated).unwrap();
        assert_eq!(
            document["model_providers"]["custom"]["request_max_retries"].as_integer(),
            Some(9)
        );
        assert!(document["model_providers"].get("other").is_some());
    }
}
