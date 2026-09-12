use serde::Serialize;
use std::error::Error;
use std::fs;
use std::path::Path;
use toml_edit::Item;

use crate::{
    codex_process, config, gateway, model_catalog, oauth, profiles, provider_sync, upstream,
};
use config::{
    api_key_from_auth, build_custom_auth, build_official_config, build_provider_config,
    config_selects_custom, custom_provider, is_official_config, normalize_api_url, parse_config,
    validate_api_key, validate_provider_name, verify_provider_content,
};
use oauth::{AuthHealth, LocalAuthState};
use profiles::ProfileStore;
use profiles::ProviderRecord;
use provider_sync::{AuthUpdate, ProviderSyncReport};

const PROVIDER_ID: &str = "custom";
const OFFICIAL_PROVIDER_ID: &str = "openai";
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderSummary {
    id: String,
    name: String,
    api_url: String,
    model_count: usize,
    has_api_key: bool,
    active: bool,
    protocol: String,
    routing_mode: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderState {
    providers: Vec<ProviderSummary>,
    active_provider_id: Option<String>,
    official_active: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SavedProvider {
    provider: ProviderSummary,
    routing_required: bool,
    routing_message: Option<String>,
}

pub(crate) fn list_provider_state(codex_home: &Path) -> Result<ProviderState, Box<dyn Error>> {
    ensure_provider_migration(codex_home)?;
    let profiles = ProfileStore::new(codex_home);
    let records = profiles.list_providers()?;
    let active_provider_id = detect_active_provider_id(codex_home, &profiles, &records)?;
    let config = read_optional_file(&codex_home.join("config.toml"))?;
    let official_config =
        is_official_config(std::str::from_utf8(config.as_deref().unwrap_or_default())?)?;
    let official_active = if official_config {
        read_optional_file(&codex_home.join("auth.json"))?
            .as_deref()
            .is_some_and(|auth| oauth::inspect_auth(auth) != LocalAuthState::Invalid)
    } else {
        false
    };
    let mut providers = Vec::with_capacity(records.len());
    for record in records {
        let profile = profiles.load_provider(&record.id)?;
        providers.push(ProviderSummary {
            id: record.id.clone(),
            name: record.name,
            api_url: record.api_url,
            model_count: record.model_count,
            has_api_key: api_key_from_auth(&profile.auth)?.is_some(),
            active: active_provider_id.as_deref() == Some(record.id.as_str()),
            protocol: record.protocol,
            routing_mode: record.routing_mode,
        });
    }
    Ok(ProviderState {
        providers,
        active_provider_id,
        official_active,
    })
}

pub(crate) fn save_provider_inner(
    codex_home: &Path,
    provider_id: Option<&str>,
    name: &str,
    api_url: &str,
    api_key: &str,
) -> Result<SavedProvider, Box<dyn Error>> {
    ensure_provider_migration(codex_home)?;
    let name = validate_provider_name(name)?;
    let api_url = normalize_api_url(api_url)?;
    let profiles = ProfileStore::new(codex_home);
    let existing = provider_id
        .map(|id| profiles.load_provider(id))
        .transpose()?;
    let key = provider_api_key(existing.as_ref(), &api_url, api_key)?;
    let detection = upstream::detect(&api_url, &key)?;
    let catalog = model_catalog::fetch_with_auth(&api_url, &key, detection.anthropic_auth)?;
    let source = existing
        .as_ref()
        .map(|profile| profile.config.clone())
        .or(read_optional_file(&codex_home.join("config.toml"))?)
        .unwrap_or_default();
    let source_text = std::str::from_utf8(&source)?;
    let auth = build_custom_auth(&key)?;
    let direct_base_url =
        upstream::base_url_for_endpoint(&detection.inference_endpoint, &detection.protocol)?;
    let config = build_provider_config(source_text, name, &direct_base_url)?;
    let record = profiles.save_provider_with_routing(
        provider_id,
        name,
        &api_url,
        &auth,
        Some(&catalog.bytes),
        &detection.protocol,
        "direct",
        Some(&detection.inference_endpoint),
        config.as_bytes(),
    )?;
    let profile = profiles.load_provider(&record.id)?;
    let active_provider_id =
        detect_active_provider_id(codex_home, &profiles, std::slice::from_ref(&record))?;
    Ok(SavedProvider {
        provider: provider_summary(
            record,
            api_key_from_auth(&profile.auth)?.is_some(),
            active_provider_id.as_deref(),
        ),
        routing_required: detection.routing_required,
        routing_message: detection.routing_required.then_some(detection.message),
    })
}

#[cfg(test)]
fn save_provider_inner_with<P, F>(
    codex_home: &Path,
    provider_id: Option<&str>,
    name: &str,
    api_url: &str,
    api_key: &str,
    probe: P,
    fetch_catalog: F,
) -> Result<SavedProvider, Box<dyn Error>>
where
    P: FnOnce(&str, &str) -> Result<(), Box<dyn Error>>,
    F: FnOnce(&str, &str) -> Result<model_catalog::ModelCatalog, Box<dyn Error>>,
{
    ensure_provider_migration(codex_home)?;
    let name = validate_provider_name(name)?;
    let api_url = normalize_api_url(api_url)?;
    let profiles = ProfileStore::new(codex_home);
    let existing = provider_id
        .map(|id| profiles.load_provider(id))
        .transpose()?;
    let existing_url = existing
        .as_ref()
        .map(|profile| profile.record.api_url.as_str());
    let key = if api_key.trim().is_empty() {
        let existing = existing.as_ref().ok_or("API Key 不能为空")?;
        if existing_url != Some(api_url.as_str()) {
            return Err("API URL 已变化，请重新填写对应的 API Key".into());
        }
        api_key_from_auth(&existing.auth)?.ok_or("API Key 不能为空")?
    } else {
        validate_api_key(api_key)?.to_string()
    };
    probe(&api_url, &key)?;
    let catalog = fetch_catalog(&api_url, &key)?;
    let active_config = read_optional_file(&codex_home.join("config.toml"))?;
    let source = existing
        .as_ref()
        .map(|profile| profile.config.clone())
        .or(active_config)
        .unwrap_or_default();
    let source_text = std::str::from_utf8(&source)?;
    let auth = build_custom_auth(&key)?;
    let config = build_provider_config(source_text, name, &api_url)?;
    let record = profiles.save_provider(
        provider_id,
        name,
        &api_url,
        &auth,
        Some(&catalog.bytes),
        config.as_bytes(),
    )?;
    let profile = profiles.load_provider(&record.id)?;
    let active_provider_id =
        detect_active_provider_id(codex_home, &profiles, std::slice::from_ref(&record))?;
    Ok(SavedProvider {
        provider: ProviderSummary {
            id: record.id.clone(),
            name: record.name,
            api_url: record.api_url,
            model_count: record.model_count,
            has_api_key: api_key_from_auth(&profile.auth)?.is_some(),
            active: active_provider_id.as_deref() == Some(record.id.as_str()),
            protocol: record.protocol,
            routing_mode: record.routing_mode,
        },
        routing_required: false,
        routing_message: None,
    })
}

fn provider_api_key(
    existing: Option<&profiles::ProviderProfile>,
    api_url: &str,
    api_key: &str,
) -> Result<String, Box<dyn Error>> {
    if !api_key.trim().is_empty() {
        return Ok(validate_api_key(api_key)?.to_string());
    }
    let existing = existing.ok_or("API Key 不能为空")?;
    if existing.record.api_url != api_url {
        return Err("API URL 已变化，请重新填写对应的 API Key".into());
    }
    api_key_from_auth(&existing.auth)?.ok_or_else(|| "API Key 不能为空".into())
}

fn provider_summary(
    record: ProviderRecord,
    has_api_key: bool,
    active_provider_id: Option<&str>,
) -> ProviderSummary {
    let active = active_provider_id == Some(record.id.as_str());
    ProviderSummary {
        id: record.id,
        name: record.name,
        api_url: record.api_url,
        model_count: record.model_count,
        has_api_key,
        active,
        protocol: record.protocol,
        routing_mode: record.routing_mode,
    }
}

pub(crate) fn enable_provider_routing_inner(
    codex_home: &Path,
    provider_id: &str,
) -> Result<(), Box<dyn Error>> {
    ensure_provider_migration(codex_home)?;
    let profiles = ProfileStore::new(codex_home);
    let provider = profiles.load_provider(provider_id)?;
    if provider.record.protocol == "openai_responses" {
        return Err("该供应商已原生支持 Responses，不需要本地路由".into());
    }
    profiles.update_provider_routing(
        provider_id,
        &provider.record.protocol,
        "local",
        provider.record.inference_endpoint.as_deref(),
    )?;
    Ok(())
}

pub(crate) fn activate_provider_inner(
    codex_home: &Path,
    provider_id: &str,
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    activate_provider_inner_with_close(codex_home, provider_id, codex_process::close_if_running)
}

fn activate_provider_inner_with_close<C>(
    codex_home: &Path,
    provider_id: &str,
    close_codex: C,
) -> Result<ProviderSyncReport, Box<dyn Error>>
where
    C: FnOnce() -> Result<bool, Box<dyn Error>>,
{
    ensure_provider_migration(codex_home)?;
    let profiles = ProfileStore::new(codex_home);
    let mut target = profiles.load_provider(provider_id)?;
    if target.catalog.is_none() || target.record.inference_endpoint.is_none() {
        let key = api_key_from_auth(&target.auth)?.ok_or("供应商缺少 API Key")?;
        let detection = upstream::detect(&target.record.api_url, &key)?;
        let catalog =
            model_catalog::fetch_with_auth(&target.record.api_url, &key, detection.anthropic_auth)?;
        let source = std::str::from_utf8(&target.config)?;
        let auth = build_custom_auth(&key)?;
        let direct_base_url =
            upstream::base_url_for_endpoint(&detection.inference_endpoint, &detection.protocol)?;
        let routing_mode = if target.record.protocol == detection.protocol
            && target.record.routing_mode == "local"
        {
            "local"
        } else {
            "direct"
        };
        let config = build_provider_config(source, &target.record.name, &direct_base_url)?;
        let record = profiles.save_provider_with_routing(
            Some(provider_id),
            &target.record.name,
            &target.record.api_url,
            &auth,
            Some(&catalog.bytes),
            &detection.protocol,
            routing_mode,
            Some(&detection.inference_endpoint),
            config.as_bytes(),
        )?;
        target = profiles.load_provider(&record.id)?;
    }
    if target.record.protocol != "openai_responses" && target.record.routing_mode != "local" {
        return Err(format!(
            "{} 需要先启用本地路由进行协议转换",
            upstream::protocol_label(&target.record.protocol)
        )
        .into());
    }
    let original = read_optional_file(&codex_home.join("config.toml"))?;
    let original_bytes = original.as_deref().unwrap_or_default();
    let original_text = std::str::from_utf8(original_bytes)?;
    let active_id = detect_active_provider_id(codex_home, &profiles, &profiles.list_providers()?)?;
    let previous_local_id = active_id
        .as_deref()
        .filter(|id| *id != provider_id)
        .and_then(|id| profiles.load_provider(id).ok())
        .filter(|profile| profile.record.routing_mode == "local")
        .map(|profile| profile.record.id);
    let using_gateway = target.record.routing_mode == "local";
    let activated = codex_process::after_closed_with(close_codex, || {
        if let Some(active_id) = active_id.as_deref().filter(|id| *id != provider_id)
            && let Some(active_auth) = read_optional_file(&codex_home.join("auth.json"))?
        {
            profiles.update_provider_snapshot(active_id, original_bytes, &active_auth)?;
        }
        if is_official_config(original_text)? {
            capture_official_profile(codex_home, &profiles, original_bytes)?;
        }

        let active_base_url = if using_gateway {
            match gateway::ensure_running(codex_home, provider_id) {
                Ok(port) => gateway::local_base_url(port),
                Err(error) => {
                    if let Some(previous_id) = previous_local_id.as_deref() {
                        let _ = gateway::ensure_running(codex_home, previous_id);
                    }
                    return Err(error);
                }
            }
        } else {
            match target.record.inference_endpoint.as_deref() {
                Some(endpoint) => {
                    upstream::base_url_for_endpoint(endpoint, &target.record.protocol)?
                }
                None if target.record.protocol == "openai_responses" => {
                    target.record.api_url.clone()
                }
                None => return Err("供应商缺少已探测的推理接口".into()),
            }
        };
        let updated = build_provider_config(original_text, &target.record.name, &active_base_url)?;
        verify_provider_content(
            updated.as_bytes(),
            &target.auth,
            &target.record.name,
            &active_base_url,
        )?;
        profiles.update_provider_snapshot(provider_id, updated.as_bytes(), &target.auth)?;
        activate_custom(
            codex_home,
            original.as_deref(),
            updated.as_bytes(),
            &target.auth,
        )
    });
    match activated {
        Ok(report) => {
            if !using_gateway {
                gateway::stop(codex_home)?;
            }
            Ok(report)
        }
        Err(error) => {
            if using_gateway {
                let _ = gateway::stop(codex_home);
                if let Some(previous_id) = previous_local_id {
                    let _ = gateway::ensure_running(codex_home, &previous_id);
                }
            }
            Err(error)
        }
    }
}

pub(crate) fn delete_provider_inner(
    codex_home: &Path,
    provider_id: &str,
) -> Result<(), Box<dyn Error>> {
    ensure_provider_migration(codex_home)?;
    let profiles = ProfileStore::new(codex_home);
    let records = profiles.list_providers()?;
    let active_id = detect_active_provider_id(codex_home, &profiles, &records)?;
    if active_id.as_deref() == Some(provider_id) {
        return Err("当前供应商正在使用，请先切换到官方登录或其他供应商".into());
    }
    gateway::stop_provider(codex_home, provider_id)?;
    profiles.delete_provider(provider_id)
}

pub(crate) fn ensure_provider_migration(codex_home: &Path) -> Result<(), Box<dyn Error>> {
    profiles::migrate_legacy_profile_storage(codex_home)?;
    let profiles = ProfileStore::new(codex_home);
    if profiles.provider_registry_exists() {
        return Ok(());
    }
    let active_config = read_optional_file(&codex_home.join("config.toml"))?;
    let active_auth = read_optional_file(&codex_home.join("auth.json"))?;
    let legacy = profiles
        .load_custom_config()?
        .zip(profiles.load_custom_auth()?);
    let active_pair = match (active_config.as_ref(), active_auth.as_ref()) {
        (Some(config), Some(auth))
            if config_selects_custom(config)? && api_key_from_auth(auth)?.is_some() =>
        {
            Some((config.clone(), auth.clone()))
        }
        _ => None,
    };
    let pair = active_pair.or(legacy);
    if let Some((config, auth)) = pair {
        let text = std::str::from_utf8(&config)?;
        let document = parse_config(text)?;
        if let Some(provider) = custom_provider(&document)
            && let (Some(name), Some(api_url), Some(_key)) = (
                provider.get("name").and_then(Item::as_str),
                provider.get("base_url").and_then(Item::as_str),
                api_key_from_auth(&auth)?,
            )
        {
            let name = if name.trim().is_empty() {
                "第三方 API"
            } else {
                name
            };
            let url = normalize_api_url(api_url)?;
            profiles.save_provider_without_catalog(name, &url, &auth, &config)?;
        }
    }
    profiles.ensure_provider_registry()
}

fn detect_active_provider_id(
    codex_home: &Path,
    profiles: &ProfileStore,
    records: &[ProviderRecord],
) -> Result<Option<String>, Box<dyn Error>> {
    let config = read_optional_file(&codex_home.join("config.toml"))?;
    let Some(config) = config else {
        return Ok(None);
    };
    if !config_selects_custom(&config)? {
        return Ok(None);
    }
    let document = parse_config(std::str::from_utf8(&config)?)?;
    let Some(provider) = custom_provider(&document) else {
        return Ok(None);
    };
    let name = provider
        .get("name")
        .and_then(Item::as_str)
        .unwrap_or_default();
    let api_url = provider
        .get("base_url")
        .and_then(Item::as_str)
        .map(normalize_api_url)
        .transpose()?;
    let Some(api_url) = api_url else {
        return Ok(None);
    };
    let Some(auth) = read_optional_file(&codex_home.join("auth.json"))? else {
        return Ok(None);
    };
    let key = api_key_from_auth(&auth)?;
    for record in records {
        let expected_url = if record.routing_mode == "local" {
            gateway::active_base_url(codex_home, &record.id)
        } else {
            record
                .inference_endpoint
                .as_deref()
                .and_then(|endpoint| {
                    upstream::base_url_for_endpoint(endpoint, &record.protocol).ok()
                })
                .or_else(|| Some(record.api_url.clone()))
        };
        if record.name == name && expected_url.as_deref() == Some(api_url.as_str()) {
            let profile = profiles.load_provider(&record.id)?;
            if api_key_from_auth(&profile.auth)? == key {
                return Ok(Some(record.id.clone()));
            }
        }
    }
    Ok(None)
}

pub(crate) fn switch_to_official(codex_home: &Path) -> Result<ProviderSyncReport, Box<dyn Error>> {
    oauth::ensure_login_active()?;
    fs::create_dir_all(codex_home)?;
    let config_path = codex_home.join("config.toml");
    let original = read_optional_file(&config_path)?;
    let original_bytes = original.as_deref().unwrap_or_default();
    let original_text = std::str::from_utf8(original_bytes)?;
    let active_is_official = is_official_config(original_text)?;
    let profiles = ProfileStore::new(codex_home);
    let saved_profile = profiles.load_official()?;

    if !active_is_official {
        if profiles.provider_registry_exists() {
            let records = profiles.list_providers()?;
            if let Some(active_id) = detect_active_provider_id(codex_home, &profiles, &records)?
                && let Some(auth) = read_custom_auth_for_profile(codex_home)?
            {
                profiles.update_provider_snapshot(&active_id, original_bytes, &auth)?;
            }
        } else {
            profiles.save_custom_config(original_bytes)?;
            if let Some(auth) = read_custom_auth_for_profile(codex_home)? {
                profiles.save_custom_auth(&auth)?;
            }
        }
    }

    let mut official_config = if active_is_official {
        original_bytes.to_vec()
    } else if let Some(profile) = saved_profile.as_ref() {
        profile.config.clone()
    } else {
        match profiles.load_official_config()? {
            Some(config) => config,
            None => build_official_config(original_text)?.into_bytes(),
        }
    };

    let official_text = std::str::from_utf8(&official_config)?;
    if !is_official_config(official_text)? {
        official_config = build_official_config(official_text)?.into_bytes();
    }

    let mut candidates = Vec::new();
    if !active_is_official && let Some(profile) = saved_profile.as_ref() {
        push_unique_auth(&mut candidates, profile.auth.clone());
    }
    if active_is_official && let Some(auth) = read_optional_file(&codex_home.join("auth.json"))? {
        push_unique_auth(&mut candidates, auth);
    }
    if active_is_official && let Some(profile) = saved_profile.as_ref() {
        push_unique_auth(&mut candidates, profile.auth.clone());
    }

    let official_auth = match select_official_auth(&candidates)? {
        Some(auth) => auth,
        None => oauth::browser_login()?,
    };

    oauth::ensure_login_active()?;
    let report = codex_process::after_closed(|| {
        profiles.save_official(&official_config, &official_auth)?;
        activate_official(
            codex_home,
            original.as_deref(),
            &official_config,
            &official_auth,
        )
    })?;
    gateway::stop(codex_home)?;
    Ok(report)
}

fn push_unique_auth(candidates: &mut Vec<Vec<u8>>, candidate: Vec<u8>) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

fn select_official_auth(candidates: &[Vec<u8>]) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    select_official_auth_with(candidates, oauth::refresh_auth)
}

fn select_official_auth_with<F>(
    candidates: &[Vec<u8>],
    mut refresh: F,
) -> Result<Option<Vec<u8>>, Box<dyn Error>>
where
    F: FnMut(&[u8]) -> Result<AuthHealth, Box<dyn Error>>,
{
    if let Some(current) = candidates
        .iter()
        .find(|candidate| oauth::inspect_auth(candidate) == LocalAuthState::Current)
    {
        return Ok(Some(current.clone()));
    }

    for candidate in candidates {
        if oauth::inspect_auth(candidate) != LocalAuthState::NeedsRefresh {
            continue;
        }
        match refresh(candidate)? {
            AuthHealth::Valid(refreshed) => return Ok(Some(refreshed)),
            AuthHealth::Invalid => {}
        }
    }
    Ok(None)
}

fn capture_official_profile(
    codex_home: &Path,
    profiles: &ProfileStore,
    config: &[u8],
) -> Result<(), Box<dyn Error>> {
    profiles.save_official_config(config)?;
    let Some(candidate) = read_optional_file(&codex_home.join("auth.json"))? else {
        profiles.discard_official_auth()?;
        return Ok(());
    };
    let fallback =
        (oauth::inspect_auth(&candidate) != LocalAuthState::Invalid).then(|| candidate.clone());
    let auth = match select_official_auth(std::slice::from_ref(&candidate)) {
        Ok(Some(auth)) => auth,
        Ok(None) => {
            profiles.discard_official_auth()?;
            return Ok(());
        }
        Err(_) if fallback.is_some() => fallback.expect("checked fallback"),
        Err(error) => return Err(error),
    };
    if oauth::inspect_auth(&auth) == LocalAuthState::Invalid {
        profiles.discard_official_auth()?;
        return Ok(());
    }
    profiles.save_official(config, &auth)
}

fn activate_custom(
    codex_home: &Path,
    original_config: Option<&[u8]>,
    updated_config: &[u8],
    custom_auth: &[u8],
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    provider_sync::apply_provider_state(
        codex_home,
        original_config,
        updated_config,
        PROVIDER_ID,
        AuthUpdate::Replace(custom_auth),
    )
}

fn activate_official(
    codex_home: &Path,
    original_config: Option<&[u8]>,
    official_config: &[u8],
    official_auth: &[u8],
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    let report = provider_sync::apply_provider_state(
        codex_home,
        original_config,
        official_config,
        OFFICIAL_PROVIDER_ID,
        AuthUpdate::Replace(official_auth),
    )?;
    if fs::read(codex_home.join("config.toml"))? != official_config {
        return Err("官方 config.toml 恢复后的字节验证失败".into());
    }
    if fs::read(codex_home.join("auth.json"))? != official_auth {
        return Err("官方 auth.json 恢复后的字节验证失败".into());
    }
    Ok(report)
}

fn read_custom_auth_for_profile(codex_home: &Path) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    if let Some(auth) = read_optional_file(&codex_home.join("auth.json"))?
        && api_key_from_auth(&auth)?.is_some()
    {
        return Ok(Some(auth));
    }
    Ok(None)
}

fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    match fs::read(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取 {} 失败：{error}", path.display()).into()),
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
