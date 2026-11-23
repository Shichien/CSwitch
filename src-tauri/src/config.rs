use serde_json::{Value, json};
use std::error::Error;
use toml_edit::{DocumentMut, Item, Table, value};
use url::Url;

pub(crate) const LEGACY_RESPONSE_STORAGE: &str = "disable_response_storage";
const CUSTOM_PROVIDER_ID: &str = "custom";
const OFFICIAL_PROVIDER_ID: &str = "openai";
pub(crate) const HTTP_ROUTE_PROVIDER_ID: &str = "cswitch_local";
const AUTH_FIELDS: &[&str] = &[
    "requires_openai_auth",
    "env_key",
    "env_key_instructions",
    "experimental_bearer_token",
    "auth",
    "aws",
];

fn clear_provider_auth(table: &mut dyn toml_edit::TableLike) {
    for field in AUTH_FIELDS {
        table.remove(field);
    }
    for field in ["http_headers", "env_http_headers"] {
        if let Some(headers) = table.get_mut(field).and_then(Item::as_table_like_mut) {
            let names: Vec<_> = headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(name, _)| name.to_string())
                .collect();
            for name in names {
                headers.remove(&name);
            }
        }
    }
}

pub(crate) fn uses_managed_file_auth(
    content: &str,
    provider: &str,
) -> Result<bool, Box<dyn Error>> {
    let doc = parse_config(content)?;
    let Some(table) = doc
        .get("model_providers")
        .and_then(Item::as_table_like)
        .and_then(|providers| providers.get(provider))
        .and_then(Item::as_table_like)
    else {
        return Ok(false);
    };
    Ok(!doc.contains_key("experimental_bearer_token")
        && table.get("requires_openai_auth").and_then(Item::as_bool) == Some(true)
        && ["env_key", "experimental_bearer_token", "auth", "aws"]
            .iter()
            .all(|field| !table.contains_key(field))
        && ["http_headers", "env_http_headers"].iter().all(|field| {
            table
                .get(field)
                .and_then(Item::as_table_like)
                .is_none_or(|headers| {
                    !headers
                        .iter()
                        .any(|(key, _)| key.eq_ignore_ascii_case("authorization"))
                })
        }))
}

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
    clear_provider_auth(provider);
    provider["name"] = value(name);
    provider["base_url"] = value(api_url);
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(true);
    provider.remove("experimental_bearer_token");
    Ok(document.to_string())
}

pub(crate) fn build_provider_config_from_snapshot(
    original: &str,
    snapshot: &str,
    name: &str,
    api_url: &str,
    api_key: &str,
) -> Result<String, Box<dyn Error>> {
    let source = parse_config(snapshot)?;
    let source_id = selected_provider(snapshot)?;
    let provider = source
        .get("model_providers")
        .and_then(Item::as_table_like)
        .and_then(|providers| providers.get(&source_id));
    let mut doc = parse_config(original)?;
    let live_provider = doc
        .get("model_providers")
        .and_then(Item::as_table_like)
        .and_then(|providers| providers.get(&source_id));
    let same_provider = selected_provider(original)? == source_id
        && live_provider.zip(provider).is_some_and(|(a, b)| {
            ["name", "base_url"].iter().all(|field| {
                a.get(field).and_then(Item::as_str) == b.get(field).and_then(Item::as_str)
            })
        });
    let provider = if same_provider {
        live_provider
    } else {
        provider
    }
    .cloned();
    if let Some(provider) = provider.filter(|provider| provider.is_table_like()) {
        if !doc.contains_key("model_providers") {
            let mut providers = Table::new();
            providers.set_implicit(true);
            doc["model_providers"] = Item::Table(providers);
        }
        doc["model_providers"][CUSTOM_PROVIDER_ID] = provider;
    }
    build_provider_config(&doc.to_string(), name, api_url, api_key)
}

pub(crate) fn clear_request_bearer(config: &str) -> Result<String, Box<dyn Error>> {
    let mut document = parse_config(config)?;
    document.remove("experimental_bearer_token");
    if let Some(provider) = document
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
        .and_then(|providers| providers.get_mut("custom"))
        .and_then(Item::as_table_like_mut)
    {
        provider.remove("experimental_bearer_token");
    }
    Ok(document.to_string())
}

pub(crate) fn selected_provider(content: &str) -> Result<String, Box<dyn Error>> {
    Ok(parse_config(content)?
        .get("model_provider")
        .and_then(Item::as_str)
        .unwrap_or("openai")
        .to_string())
}

pub(crate) fn provider_base_url(
    content: &str,
    provider: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    let doc = parse_config(content)?;
    let item = if provider == "openai" {
        doc.get("openai_base_url")
    } else {
        doc.get("model_providers")
            .and_then(Item::as_table_like)
            .and_then(|providers| providers.get(provider))
            .and_then(Item::as_table_like)
            .and_then(|table| table.get("base_url"))
    };
    item.map(|item| {
        item.as_str()
            .map(str::to_string)
            .ok_or_else(|| "提供方请求地址不是字符串".into())
    })
    .transpose()
}

pub(crate) fn with_provider_base_url(
    content: &str,
    provider: &str,
    url: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let mut doc = parse_config(content)?;
    doc.remove("experimental_bearer_token");
    if provider == "openai" {
        if let Some(url) = url {
            doc["openai_base_url"] = value(url);
        } else {
            doc.remove("openai_base_url");
        }
    } else {
        let table = doc
            .get_mut("model_providers")
            .and_then(Item::as_table_like_mut)
            .and_then(|providers| providers.get_mut(provider))
            .and_then(Item::as_table_like_mut)
            .ok_or("当前提供方配置不存在")?;
        if let Some(url) = url {
            table.insert("base_url", value(url));
        } else {
            table.remove("base_url");
        }
        // Previous CSwitch versions stored the third-party bearer here. The local relay owns it now.
        table.remove("experimental_bearer_token");
    }
    Ok(doc.to_string())
}

pub(crate) fn provider_websockets(
    content: &str,
    provider: &str,
) -> Result<Option<bool>, Box<dyn Error>> {
    let doc = parse_config(content)?;
    doc.get("model_providers")
        .and_then(Item::as_table_like)
        .and_then(|providers| providers.get(provider))
        .and_then(Item::as_table_like)
        .and_then(|table| table.get("supports_websockets"))
        .map(|item| {
            item.as_bool()
                .ok_or_else(|| "supports_websockets 不是布尔值".into())
        })
        .transpose()
}

pub(crate) fn capture_route_transport(
    content: &str,
    provider: &str,
) -> Result<crate::profiles::RouteHttpTransport, Box<dyn Error>> {
    let doc = parse_config(content)?;
    Ok(crate::profiles::RouteHttpTransport {
        // Preserve the original selector's spelling and comments, including its absence.
        previous_model_provider: doc
            .get("model_provider")
            .map(|item| format!("model_provider ={item}\n")),
        previous_websockets: provider_websockets(content, provider)?,
        previous_auth_fields: Some(capture_auth_fields(&doc, provider).to_string()),
    })
}

fn capture_auth_fields(doc: &DocumentMut, provider: &str) -> DocumentMut {
    let mut saved = DocumentMut::new();
    if let Some(item) = doc.get("experimental_bearer_token") {
        saved["experimental_bearer_token"] = item.clone();
    }
    if provider != "openai"
        && let Some(table) = doc
            .get("model_providers")
            .and_then(Item::as_table_like)
            .and_then(|providers| providers.get(provider))
            .and_then(Item::as_table_like)
    {
        let mut auth = Table::new();
        for field in AUTH_FIELDS {
            if let Some(item) = table.get(field) {
                auth[field] = item.clone();
            }
        }
        for field in ["http_headers", "env_http_headers"] {
            if let Some(headers) = table.get(field).and_then(Item::as_table_like) {
                let mut saved_headers = Table::new();
                for (key, item) in headers
                    .iter()
                    .filter(|(key, _)| key.eq_ignore_ascii_case("authorization"))
                {
                    saved_headers[key] = item.clone();
                }
                if !saved_headers.is_empty() {
                    auth[field] = Item::Table(saved_headers);
                }
            }
        }
        saved["provider"] = Item::Table(auth);
    }
    saved
}

fn restore_auth_fields(
    content: &str,
    provider: &str,
    saved: Option<&str>,
) -> Result<String, Box<dyn Error>> {
    let Some(saved) = saved else {
        return Ok(content.to_string());
    };
    let saved = parse_config(saved)?;
    let mut doc = parse_config(content)?;
    if let Some(item) = saved.get("experimental_bearer_token") {
        doc["experimental_bearer_token"] = item.clone();
    }
    if provider != "openai" {
        let table = doc
            .get_mut("model_providers")
            .and_then(Item::as_table_like_mut)
            .and_then(|providers| providers.get_mut(provider))
            .and_then(Item::as_table_like_mut)
            .ok_or("待恢复提供方配置不存在")?;
        clear_provider_auth(table);
        if let Some(auth) = saved.get("provider").and_then(Item::as_table_like) {
            for field in AUTH_FIELDS {
                if let Some(item) = auth.get(field) {
                    table.insert(field, item.clone());
                }
            }
            for field in ["http_headers", "env_http_headers"] {
                if let Some(headers) = auth.get(field).and_then(Item::as_table_like) {
                    if !table.contains_key(field) {
                        table.insert(field, Item::Table(Table::new()));
                    }
                    let target = table
                        .get_mut(field)
                        .and_then(Item::as_table_like_mut)
                        .ok_or("待恢复请求头不是表")?;
                    for (key, item) in headers.iter() {
                        target.insert(key, item.clone());
                    }
                }
            }
        }
    }
    Ok(doc.to_string())
}

pub(crate) fn with_http_route(
    content: &str,
    provider: &str,
    url: &str,
) -> Result<String, Box<dyn Error>> {
    if provider != "openai" {
        let text = with_provider_base_url(content, provider, Some(url))?;
        let mut doc = parse_config(&text)?;
        let table = doc
            .get_mut("model_providers")
            .and_then(Item::as_table_like_mut)
            .and_then(|providers| providers.get_mut(provider))
            .and_then(Item::as_table_like_mut)
            .ok_or("当前提供方配置不存在")?;
        clear_provider_auth(table);
        table.insert("requires_openai_auth", value(true));
        return with_provider_websockets(&doc.to_string(), provider, Some(false));
    }
    let mut doc = parse_config(content)?;
    if doc
        .get("model_providers")
        .and_then(Item::as_table_like)
        .is_some_and(|providers| providers.contains_key(HTTP_ROUTE_PROVIDER_ID))
    {
        return Err("config.toml 已存在 cswitch_local 提供方，请先为该自定义提供方改名".into());
    }
    doc.remove("experimental_bearer_token");
    doc["model_provider"] = value(HTTP_ROUTE_PROVIDER_ID);
    if !doc.contains_key("model_providers") {
        let mut providers = Table::new();
        providers.set_implicit(true);
        doc["model_providers"] = Item::Table(providers);
    }
    let providers = doc["model_providers"]
        .as_table_mut()
        .ok_or("model_providers 不是表")?;
    let mut table = Table::new();
    // Codex does not allow overriding the built-in openai provider. Keep the OpenAI
    // name/auth semantics on a managed provider with an explicit HTTP-only capability.
    table["name"] = value("OpenAI");
    table["base_url"] = value(url);
    table["wire_api"] = value("responses");
    table["requires_openai_auth"] = value(true);
    table["supports_websockets"] = value(false);
    table["supports_standalone_web_search"] = value(true);
    providers[HTTP_ROUTE_PROVIDER_ID] = Item::Table(table);
    Ok(doc.to_string())
}

pub(crate) fn with_native_openai_route(content: &str, url: &str) -> Result<String, Box<dyn Error>> {
    let mut doc = parse_config(content)?;
    doc.remove("experimental_bearer_token");
    doc["openai_base_url"] = value(url);
    Ok(doc.to_string())
}

pub(crate) fn with_provider_websockets(
    content: &str,
    provider: &str,
    enabled: Option<bool>,
) -> Result<String, Box<dyn Error>> {
    let mut doc = parse_config(content)?;
    let table = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
        .and_then(|providers| providers.get_mut(provider))
        .and_then(Item::as_table_like_mut)
        .ok_or("当前提供方配置不存在")?;
    if let Some(enabled) = enabled {
        table.insert("supports_websockets", value(enabled));
    } else {
        table.remove("supports_websockets");
    }
    Ok(doc.to_string())
}

pub(crate) fn restore_route_config(
    content: &str,
    route: &crate::profiles::OfficialRoute,
) -> Result<String, Box<dyn Error>> {
    if route.native_openai {
        let restored = restore_auth_fields(
            content,
            "openai",
            route
                .http_transport
                .as_ref()
                .and_then(|transport| transport.previous_auth_fields.as_deref()),
        )?;
        let mut doc = parse_config(&restored)?;
        if let Some(url) = &route.previous_base_url {
            doc["openai_base_url"] = value(url.as_str());
        } else {
            doc.remove("openai_base_url");
        }
        if let Some(original) = route
            .http_transport
            .as_ref()
            .and_then(|transport| transport.previous_model_provider.as_deref())
        {
            doc["model_provider"] = parse_config(original)?["model_provider"].clone();
        } else {
            doc.remove("model_provider");
        }
        return Ok(doc.to_string());
    }
    let text = if (route.http_transport.is_some() && route.config_provider == "openai")
        || provider_base_url(content, &route.config_provider)? == route.previous_base_url
    {
        content.to_string()
    } else {
        with_provider_base_url(
            content,
            &route.config_provider,
            route.previous_base_url.as_deref(),
        )?
    };
    let Some(transport) = &route.http_transport else {
        return Ok(text);
    };
    let text = restore_auth_fields(
        &text,
        &route.config_provider,
        transport.previous_auth_fields.as_deref(),
    )?;
    if route.config_provider != "openai" {
        return with_provider_websockets(
            &text,
            &route.config_provider,
            transport.previous_websockets,
        );
    }
    let mut doc = parse_config(&text)?;
    if let Some(original) = &transport.previous_model_provider {
        doc["model_provider"] = parse_config(original)?["model_provider"].clone();
    } else {
        doc.remove("model_provider");
    }
    if let Some(providers) = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        providers.remove(HTTP_ROUTE_PROVIDER_ID);
        if providers.is_empty() {
            doc.remove("model_providers");
        }
    }
    Ok(doc.to_string())
}

pub(crate) fn build_custom_auth(api_key: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let api_key = validate_api_key(api_key)?;
    Ok(serde_json::to_vec_pretty(&json!({
        "OPENAI_API_KEY": api_key,
    }))?)
}

// Store ownership in a comment alongside the catalog path. Codex ignores TOML comments.
const CATALOG_COMMENT: &str = "# CSwitch catalog: ";

type CatalogState = (String, Option<String>);

pub(crate) fn catalog_state(doc: &DocumentMut) -> Result<Option<CatalogState>, Box<dyn Error>> {
    if let Some(marker) = doc.get("cswitch_model_catalog")
        && let Some(path) = marker.get("managed").and_then(Item::as_str)
    {
        let previous = marker
            .get("previous")
            .and_then(Item::as_str)
            .map(str::to_owned);
        return Ok(Some((path.to_owned(), previous)));
    }
    let state = doc
        .get("model_catalog_json")
        .and_then(Item::as_value)
        .and_then(|v| v.decor().suffix())
        .and_then(|s| s.as_str())
        .and_then(|s| s.trim().strip_prefix(CATALOG_COMMENT));
    Ok(state.map(serde_json::from_str).transpose()?)
}

pub(crate) fn with_model_catalog(
    content: &str,
    path: Option<&std::path::Path>,
) -> Result<String, Box<dyn Error>> {
    let mut doc = parse_config(content)?;
    doc.remove(LEGACY_RESPONSE_STORAGE);
    let state = catalog_state(&doc)?;
    doc.remove("cswitch_model_catalog");
    if let Some((managed, previous)) = state {
        if doc.get("model_catalog_json").and_then(Item::as_str) == Some(managed.as_str()) {
            if let Some(previous) = previous {
                doc["model_catalog_json"] = value(previous);
            } else {
                doc.remove("model_catalog_json");
            }
        } else if let Some(v) = doc
            .get_mut("model_catalog_json")
            .and_then(Item::as_value_mut)
            && v.decor()
                .suffix()
                .and_then(|s| s.as_str())
                .is_some_and(|s| s.trim().starts_with(CATALOG_COMMENT))
        {
            v.decor_mut().set_suffix("");
        }
    }
    if let Some(path) = path {
        let path = path.to_str().ok_or("模型目录路径不是 UTF-8")?;
        let previous = doc.get("model_catalog_json").and_then(Item::as_str);
        let state = serde_json::to_string(&(path, previous))?;
        let mut catalog = toml_edit::Value::from(path);
        catalog
            .decor_mut()
            .set_suffix(format!(" {CATALOG_COMMENT}{state}"));
        doc["model_catalog_json"] = Item::Value(catalog);
    }
    Ok(doc.to_string())
}

pub(crate) fn build_official_config(original: &str) -> Result<String, Box<dyn Error>> {
    let mut document = parse_config(&with_model_catalog(original, None)?)?;
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
        && ["env_key", "auth", "aws"]
            .iter()
            .all(|field| !provider.contains_key(field))
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

#[cfg(test)]
mod managed_catalog_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn restores_user_catalog_and_preserves_external_edits() {
        let original = "model_catalog_json='/user/models.json'\nmodel='keep'\n";
        let first = with_model_catalog(original, Some(Path::new("/managed/a.json"))).unwrap();
        assert!(!first.contains("cswitch_model_catalog"));
        let legacy = "model_catalog_json='/managed/a.json'\nmodel='keep'\n[cswitch_model_catalog]\nmanaged='/managed/a.json'\nprevious='/user/models.json'\n";
        assert_eq!(
            with_model_catalog(legacy, Some(Path::new("/managed/a.json"))).unwrap(),
            first
        );
        let second = with_model_catalog(&first, Some(Path::new("/managed/b.json"))).unwrap();
        let restored = parse_config(&build_official_config(&second).unwrap()).unwrap();
        assert_eq!(
            restored["model_catalog_json"].as_str(),
            Some("/user/models.json")
        );
        assert_eq!(restored["model"].as_str(), Some("keep"));
        assert!(!restored.contains_key("cswitch_model_catalog"));
        let mut external = parse_config(&second).unwrap();
        external["model_catalog_json"] = value("/external/models.json");
        let restored =
            parse_config(&with_model_catalog(&external.to_string(), None).unwrap()).unwrap();
        assert_eq!(
            restored["model_catalog_json"].as_str(),
            Some("/external/models.json")
        );
    }
}
