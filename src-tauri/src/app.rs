use serde::Serialize;
use std::error::Error;
use std::fs;
use std::path::Path;

use crate::{
    codex_process, config, gateway, model_catalog, oauth, profiles, progress, provider_import,
    provider_sync, provider_usage, upstream,
};
#[cfg(test)]
use config::build_provider_config;
use config::{
    api_key_from_auth, build_custom_auth, build_official_config, custom_bearer_token,
    is_official_config, normalize_api_url, parse_config, validate_api_key, validate_provider_name,
    verify_provider_content,
};
use oauth::{AuthHealth, LocalAuthState};
use profiles::{ProfileStore, ProviderRecord};
use progress::ProgressReporter;
use provider_sync::{AuthUpdate, ProviderSyncReport};

const PROVIDER_ID: &str = "custom";
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) api_url: String,
    pub(crate) model_count: usize,
    pub(crate) has_api_key: bool,
    pub(crate) active: bool,
    pub(crate) protocol: String,
    pub(crate) routing_mode: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderState {
    pub(crate) warnings: Vec<String>,
    pub(crate) providers: Vec<ProviderSummary>,
    pub(crate) official_accounts: Vec<crate::official_accounts::Summary>,
    pub(crate) active_provider_id: Option<String>,
    pub(crate) official_active: bool,
    pub(crate) keep_official_auth: bool,
    pub(crate) official_auth_available: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OfficialAccountHealth {
    Valid,
    ReauthRequired,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OfficialAccountUsage {
    pub(crate) account_id: String,
    pub(crate) health: OfficialAccountHealth,
    pub(crate) plan: Option<String>,
    pub(crate) windows: Vec<oauth::OfficialQuotaWindow>,
    pub(crate) credits: Option<oauth::OfficialCredits>,
    pub(crate) message: Option<String>,
    pub(crate) queried_at: i64,
}

pub(crate) enum OfficialAccountUsageProbe {
    Ready(OfficialAccountUsage),
    NeedsExclusiveRefresh,
}

impl OfficialAccountUsage {
    pub(crate) fn unavailable(account_id: &str, message: impl Into<String>) -> Self {
        Self {
            account_id: account_id.to_string(),
            health: OfficialAccountHealth::Unknown,
            plan: None,
            windows: Vec::new(),
            credits: None,
            message: Some(message.into()),
            queried_at: chrono::Utc::now().timestamp_millis(),
        }
    }

    fn reauth_required(account_id: &str) -> Self {
        Self {
            account_id: account_id.to_string(),
            health: OfficialAccountHealth::ReauthRequired,
            plan: None,
            windows: Vec::new(),
            credits: None,
            message: Some("官方登录已失效，请重新登录此账号".into()),
            queried_at: chrono::Utc::now().timestamp_millis(),
        }
    }

    fn valid(account_id: &str, quota: oauth::OfficialQuota, message: Option<String>) -> Self {
        Self {
            account_id: account_id.to_string(),
            health: OfficialAccountHealth::Valid,
            plan: quota.plan,
            windows: quota.windows,
            credits: quota.credits,
            message,
            queried_at: chrono::Utc::now().timestamp_millis(),
        }
    }
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
    sync_official_accounts(codex_home)?;
    let mut state = list_provider_state_read_only(codex_home)?;
    if let Err(error) = ProfileStore::new(codex_home).cleanup() {
        state.warnings.push(format!(
            "供应商列表已读取，清理旧快照失败（{}）：{error}",
            codex_home.display()
        ));
    }
    Ok(state)
}

pub(crate) fn list_provider_state_read_only(
    codex_home: &Path,
) -> Result<ProviderState, Box<dyn Error>> {
    let profiles = ProfileStore::new(codex_home);
    let records = profiles.list_providers()?;
    let active_provider_id = detect_active_provider_id(codex_home, &profiles, &records)?;
    let config = read_optional_file(&codex_home.join("config.toml"))?;
    let official_config =
        is_official_config(std::str::from_utf8(config.as_deref().unwrap_or_default())?)?;
    let route = profiles.load_settings()?.official_route;
    let route_is_official = route.as_ref().is_some_and(|r| {
        r.provider_id == "openai"
            && verify_route_config(
                std::str::from_utf8(config.as_deref().unwrap_or_default()).unwrap_or_default(),
                r,
            )
            .is_ok()
    });
    let official_active = if (official_config && active_provider_id.is_none()) || route_is_official
    {
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
    let mut warnings = Vec::new();
    if route.as_ref().is_some_and(|r| r.http_transport.is_none()) {
        warnings.push(
            "当前路由仍使用旧版传输设置，请点击当前线路应用 HTTP/SSE 配置，再重新打开 Codex".into(),
        );
    }
    if route.is_none()
        && let Some(config) = config.as_deref()
        && let provider_import::Inspection::NeedsInput(reason) = provider_import::inspect(
            std::str::from_utf8(config)?,
            read_optional_file(&codex_home.join("auth.json"))?.as_deref(),
        )
    {
        warnings.push(reason);
    }
    let live_identity = read_optional_file(&codex_home.join("auth.json"))?
        .as_deref()
        .and_then(crate::official_accounts::identity);
    let official_accounts = crate::official_accounts::Store::new(codex_home)
        .list()?
        .into_iter()
        .map(|a| {
            let retained = live_identity.as_ref() == Some(&a.identity);
            crate::official_accounts::Summary {
                id: a.id,
                label: a.label,
                workspace: a.identity.account,
                active: official_active && retained,
                login_retained: retained,
            }
        })
        .collect();
    Ok(ProviderState {
        official_accounts,
        warnings,
        providers,
        active_provider_id,
        official_active,
        keep_official_auth: profiles.keep_official_auth()?,
        official_auth_available: official_credential_available(codex_home, &profiles)?,
    })
}

pub(crate) fn probe_official_account_usage(
    codex_home: &Path,
    account_id: &str,
    allow_refresh: bool,
) -> Result<OfficialAccountUsageProbe, Box<dyn Error>> {
    if allow_refresh {
        sync_official_accounts(codex_home)?;
    }
    let accounts = crate::official_accounts::Store::new(codex_home);
    let account = accounts.load(account_id)?;
    let candidate = serde_json::to_vec_pretty(&account.auth)?;

    if !allow_refresh {
        return Ok(match oauth::inspect_auth(&candidate) {
            LocalAuthState::Invalid => {
                OfficialAccountUsageProbe::Ready(OfficialAccountUsage::reauth_required(account_id))
            }
            LocalAuthState::NeedsRefresh => OfficialAccountUsageProbe::NeedsExclusiveRefresh,
            LocalAuthState::Current => match oauth::fetch_official_usage(&candidate) {
                oauth::OfficialUsageFetch::Available(quota) => OfficialAccountUsageProbe::Ready(
                    OfficialAccountUsage::valid(account_id, quota, None),
                ),
                oauth::OfficialUsageFetch::Rejected => {
                    OfficialAccountUsageProbe::NeedsExclusiveRefresh
                }
                oauth::OfficialUsageFetch::Unavailable(message) => {
                    OfficialAccountUsageProbe::Ready(OfficialAccountUsage::unavailable(
                        account_id, message,
                    ))
                }
            },
        });
    }

    let live_before = read_optional_file(&codex_home.join("auth.json"))?;
    let live_is_selected = live_before
        .as_deref()
        .and_then(crate::official_accounts::identity)
        .as_ref()
        == Some(&account.identity);
    let result = match oauth::check_official_usage(&candidate) {
        oauth::OfficialUsageCheck::ReauthRequired => {
            OfficialAccountUsage::reauth_required(account_id)
        }
        oauth::OfficialUsageCheck::Unavailable(message) => {
            OfficialAccountUsage::unavailable(account_id, message)
        }
        oauth::OfficialUsageCheck::Valid { auth, quota } => {
            if crate::official_accounts::identity(&auth).as_ref() != Some(&account.identity) {
                return Err("刷新后的认证身份与选中账号不一致，已停止写入".into());
            }
            let mut message = None;
            if auth != candidate {
                accounts.save(&auth)?;
                if live_is_selected {
                    if read_optional_file(&codex_home.join("auth.json"))? == live_before {
                        match profiles::atomic_write_private(&codex_home.join("auth.json"), &auth) {
                            Ok(()) => accounts.mark_live(&auth)?,
                            Err(error) => {
                                message = Some(format!(
                                    "额度已读取，新凭据已保存；同步当前 auth.json 失败：{error}"
                                ));
                            }
                        }
                    } else {
                        message = Some(
                            "额度已读取；查询期间当前 auth.json 已更新，未用旧结果覆盖".into(),
                        );
                    }
                }
            }
            OfficialAccountUsage::valid(account_id, quota, message)
        }
    };
    Ok(OfficialAccountUsageProbe::Ready(result))
}

pub(crate) fn query_provider_usage(
    codex_home: &Path,
    provider_id: &str,
) -> Result<provider_usage::ProviderUsage, Box<dyn Error>> {
    let profile = ProfileStore::new(codex_home).load_provider(provider_id)?;
    let api_key = api_key_from_auth(&profile.auth)?.ok_or("供应商缺少 API Key")?;
    Ok(provider_usage::query(
        provider_id,
        &profile.record.api_url,
        &api_key,
    ))
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
    let config = config::build_provider_config_from_snapshot(
        source_text,
        source_text,
        name,
        &direct_base_url,
        &key,
    )?;
    let routing_mode = existing
        .as_ref()
        .filter(|profile| profile.record.protocol == detection.protocol)
        .map(|profile| profile.record.routing_mode.as_str())
        .unwrap_or("direct");
    let routing_required = detection.routing_required && routing_mode != "local";
    let record = profiles.save_provider_with_routing(
        provider_id,
        name,
        &api_url,
        &auth,
        Some(&catalog.bytes),
        &detection.protocol,
        routing_mode,
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
        routing_required,
        routing_message: routing_required.then_some(detection.message),
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
    let config = build_provider_config(source_text, name, &api_url, &key)?;
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

#[cfg(test)]
pub(crate) fn set_keep_official_auth_inner(
    codex_home: &Path,
    enabled: bool,
) -> Result<ProviderState, Box<dyn Error>> {
    set_keep_official_auth_with_progress(codex_home, enabled, &ProgressReporter::default())
}

pub(crate) fn set_keep_official_auth_with_progress(
    codex_home: &Path,
    enabled: bool,
    progress: &ProgressReporter,
) -> Result<ProviderState, Box<dyn Error>> {
    let profiles = ProfileStore::new(codex_home);
    let mut settings = profiles.load_settings()?;
    let mut warnings = Vec::new();
    if !enabled {
        if settings.official_route.is_some() {
            warnings.extend(stop_official_route(codex_home, false)?.warnings);
        } else {
            settings.keep_official_auth = false;
            let original = read_optional_file(&codex_home.join("config.toml"))?;
            let text = std::str::from_utf8(original.as_deref().unwrap_or_default())?;
            let legacy = custom_bearer_token(&parse_config(text)?).is_some();
            let active =
                detect_active_provider_id(codex_home, &profiles, &profiles.list_providers()?)?;
            if legacy && let Some(id) = active {
                let target = profiles.load_provider(&id)?;
                let live = read_auth_for_transition(
                    codex_home,
                    Some(&target.auth),
                    codex_process::close_if_running,
                )?;
                let original = read_optional_file(&codex_home.join("config.toml"))?;
                let text = std::str::from_utf8(original.as_deref().unwrap_or_default())?;
                if let Some(auth) = live
                    && is_official_credential(&auth)
                {
                    let config = profiles
                        .load_official_config()?
                        .unwrap_or(build_official_config(text)?.into_bytes());
                    profiles.save_official(&config, &auth)?;
                }
                let config = config::clear_request_bearer(text)?;
                warnings.extend(
                    provider_sync::apply_configuration_state(
                        codex_home,
                        original.as_deref(),
                        config.as_bytes(),
                        AuthUpdate::Replace(&target.auth),
                        &serde_json::to_vec_pretty(&settings)?,
                    )?
                    .warnings,
                );
            } else {
                profiles.save_settings(&settings)?;
            }
        }
    } else {
        let records = profiles.list_providers()?;
        let target = detect_active_provider_id(codex_home, &profiles, &records)?
            .or_else(|| (records.len() == 1).then(|| records[0].id.clone()));
        if let Some(id) = target {
            warnings.extend(activate_official_route(codex_home, &id, progress)?.warnings);
        } else {
            warnings.extend(activate_official_route(codex_home, "openai", progress)?.warnings);
        }
    }
    let mut state = list_provider_state_read_only(codex_home)?;
    state.warnings.extend(warnings);
    Ok(state)
}

pub(crate) fn verify_route_config(
    text: &str,
    route: &profiles::OfficialRoute,
) -> Result<(), Box<dyn Error>> {
    if config::selected_provider(text)? != route.active_config_provider()
        || config::provider_base_url(text, route.active_config_provider())?.as_deref()
            != Some(route.local_base_url.as_str())
    {
        return Err("请求地址已被外部修改，已保留当前配置；请恢复原地址后再操作路由".into());
    }
    if route.http_transport.is_some()
        && config::provider_websockets(text, route.active_config_provider())? != Some(false)
    {
        return Err(
            "本地路由的 HTTP/SSE 设置已被外部修改，请恢复 supports_websockets = false 后再切换"
                .into(),
        );
    }
    if route
        .http_transport
        .as_ref()
        .is_some_and(|transport| transport.previous_auth_fields.is_some())
        && !config::uses_managed_file_auth(text, route.active_config_provider())?
    {
        return Err("路由认证字段已被外部修改，已保留当前配置；请恢复路由认证设置后再操作".into());
    }
    Ok(())
}

fn empty_route_report() -> ProviderSyncReport {
    ProviderSyncReport {
        rollout_files_updated: 0,
        sqlite_rows_updated: 0,
        providers_detected: Vec::new(),
        backup_path: String::new(),
        warnings: Vec::new(),
    }
}

fn activate_official_route(
    codex_home: &Path,
    id: &str,
    progress: &ProgressReporter,
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    activate_official_route_with_close(codex_home, id, progress, codex_process::close_if_running)
}

fn activate_official_route_with_close<C>(
    codex_home: &Path,
    id: &str,
    progress: &ProgressReporter,
    close_codex: C,
) -> Result<ProviderSyncReport, Box<dyn Error>>
where
    C: FnOnce() -> Result<bool, Box<dyn Error>>,
{
    let profiles = ProfileStore::new(codex_home);
    if id != "openai" {
        let target = profiles.load_provider(id)?;
        api_key_from_auth(&target.auth)?.ok_or("供应商缺少 API Key")?;
        if target.record.protocol != "openai_responses" && target.record.routing_mode != "local" {
            return Err("请先启用该供应商的协议转换".into());
        }
        target
            .record
            .inference_endpoint
            .as_ref()
            .ok_or("供应商缺少已验证的推理接口，请重新保存供应商")?;
    }
    let mut settings = profiles.load_settings()?;
    let original = read_optional_file(&codex_home.join("config.toml"))?;
    let text = std::str::from_utf8(original.as_deref().unwrap_or_default())?;
    let old_route = settings.official_route.clone();
    if let Some(route) = &old_route {
        verify_route_config(text, route)?;
    }
    let live_auth = read_optional_file(&codex_home.join("auth.json"))?;
    // Once taken over, an external logout must remain visible. Never resurrect a stale saved RT.
    if old_route.as_ref().is_some_and(|r| r.resident)
        && !live_auth.as_deref().is_some_and(is_official_credential)
    {
        return Err("官方登录已退出，请点击官方登录完成认证后再使用路由".into());
    }
    let auth = if let Some(auth) = live_auth
        .as_ref()
        .filter(|auth| is_official_credential(auth))
    {
        auth.clone()
    } else {
        profiles
            .load_official()?
            .filter(|p| is_official_credential(&p.auth))
            .map(|p| p.auth)
            .ok_or("请先完成官方登录，再开启本地路由")?
    };
    let config_provider = old_route
        .as_ref()
        .map(|r| r.config_provider.clone())
        .unwrap_or(config::selected_provider(text)?);
    let needs_http_config = old_route.as_ref().is_none_or(|r| {
        r.http_transport.as_ref().is_none_or(|transport| {
            r.config_provider != "openai" && transport.previous_auth_fields.is_none()
        })
    });
    if needs_http_config {
        config::capture_route_transport(text, &config_provider)?;
    }
    let had_live_official = live_auth.as_deref().is_some_and(is_official_credential);
    // Provider IDs must change once for the built-in openai provider: Codex ignores
    // capability overrides on that built-in ID. Stop writers before migrating history.
    let migrate_history = needs_http_config && config_provider == "openai";
    if migrate_history || live_auth.as_deref() != Some(auth.as_slice()) {
        close_codex()?;
    }
    let original = read_optional_file(&codex_home.join("config.toml"))?;
    let text = std::str::from_utf8(original.as_deref().unwrap_or_default())?;
    if let Some(route) = &old_route {
        verify_route_config(text, route)?;
    } else if config::selected_provider(text)? != config_provider {
        return Err("准备路由期间提供方已被外部修改，请重新操作".into());
    }
    let http_transport = match &old_route {
        Some(route) if route.http_transport.is_some() => {
            let mut transport = route.http_transport.clone().unwrap();
            if needs_http_config {
                transport.previous_auth_fields =
                    config::capture_route_transport(text, &config_provider)?.previous_auth_fields;
            }
            Some(transport)
        }
        _ => Some(config::capture_route_transport(text, &config_provider)?),
    };
    let previous_base_url = match &old_route {
        Some(route) => route.previous_base_url.clone(),
        None => config::provider_base_url(text, &config_provider)?,
    };
    let direct_provider_id = match &old_route {
        Some(route) => route.direct_provider_id.clone().or_else(|| {
            (!route.resident && route.config_provider != "openai")
                .then(|| route.provider_id.clone())
        }),
        None => detect_active_provider_id(codex_home, &profiles, &profiles.list_providers()?)?,
    };
    let port = old_route
        .as_ref()
        .filter(|r| r.resident)
        .map(|r| gateway::route_port(&r.local_base_url))
        .transpose()?
        .unwrap_or(0);
    progress.stage(
        1,
        2,
        "切换本地路由",
        "正在准备常驻代理，后续请求使用选中的线路。",
    );
    let prepared = gateway::prepare_resident(codex_home, id, port)?;
    let base_url = gateway::local_base_url(prepared.port());
    let updated = if !needs_http_config {
        text.to_string()
    } else {
        let direct_text = if old_route.is_some() && config_provider == "openai" {
            config::with_provider_base_url(text, "openai", previous_base_url.as_deref())?
        } else {
            text.to_string()
        };
        config::with_http_route(&direct_text, &config_provider, &base_url)?
    };
    settings.keep_official_auth = true;
    settings.official_route = Some(profiles::OfficialRoute {
        provider_id: id.into(),
        config_provider,
        previous_base_url,
        local_base_url: base_url,
        resident: true,
        direct_provider_id,
        http_transport,
    });
    let live_auth = read_optional_file(&codex_home.join("auth.json"))?;
    if had_live_official && !live_auth.as_deref().is_some_and(is_official_credential) {
        return Err("准备路由期间官方登录已退出，请重新登录".into());
    }
    oauth::ensure_login_active()?;
    let mut report = if !needs_http_config {
        if !live_auth.as_deref().is_some_and(is_official_credential) {
            return Err("切换期间官方登录已退出，已保留原线路，请重新登录".into());
        }
        // settings.json is the sole routing authority. An atomic rename is the commit point.
        profiles.save_settings(&settings)?;
        empty_route_report()
    } else {
        let auth_update = if live_auth.as_deref().is_some_and(is_official_credential) {
            AuthUpdate::Keep
        } else {
            AuthUpdate::Replace(&auth)
        };
        let settings_bytes = serde_json::to_vec_pretty(&settings)?;
        provider_sync::apply_route_state(
            codex_home,
            original.as_deref(),
            updated.as_bytes(),
            migrate_history.then_some(config::HTTP_ROUTE_PROVIDER_ID),
            auth_update,
            &settings_bytes,
        )?
    };
    report.warnings.extend(prepared.commit());
    if needs_http_config {
        report.warnings.push(
            "已启用 HTTP/SSE 本地路由，请重新打开 Codex；此后切换线路无需重启，CSwitch 需保持运行"
                .into(),
        );
    }
    progress.finish(2);
    Ok(report)
}

fn stop_official_route(
    codex_home: &Path,
    keep_preference: bool,
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    stop_official_route_with_close(codex_home, keep_preference, codex_process::close_if_running)
}

fn stop_official_route_with_close<C>(
    codex_home: &Path,
    keep_preference: bool,
    close_codex: C,
) -> Result<ProviderSyncReport, Box<dyn Error>>
where
    C: FnOnce() -> Result<bool, Box<dyn Error>>,
{
    let profiles = ProfileStore::new(codex_home);
    let mut settings = profiles.load_settings()?;
    let route = settings.official_route.take().ok_or("当前没有本地路由")?;
    verify_route_config(&fs::read_to_string(codex_home.join("config.toml"))?, &route)?;
    let direct_auth = if route.config_provider != "openai" && !keep_preference {
        Some(
            profiles
                .load_provider(
                    route
                        .direct_provider_id
                        .as_deref()
                        .unwrap_or(&route.provider_id),
                )?
                .auth,
        )
    } else {
        None
    };
    let migrate_history = route.http_transport.is_some() && route.config_provider == "openai";
    if migrate_history || direct_auth.is_some() {
        close_codex()?;
    }
    if let Some(target) = direct_auth.as_deref() {
        let live = read_auth_for_transition(codex_home, Some(target), || Ok(false))?;
        if let Some(auth) = live.filter(|auth| is_official_credential(auth)) {
            let config = profiles.load_official_config()?.unwrap_or_default();
            // Preserve the last rotation made by Codex during shutdown, before replacing it with an API key.
            profiles.save_official(&config, &auth)?;
        }
    }
    let original = read_optional_file(&codex_home.join("config.toml"))?;
    let text = std::str::from_utf8(original.as_deref().unwrap_or_default())?;
    verify_route_config(text, &route)?;
    let updated = config::restore_route_config(text, &route)?;
    settings.keep_official_auth = keep_preference;
    let auth_update = direct_auth
        .as_deref()
        .map(AuthUpdate::Replace)
        .unwrap_or(AuthUpdate::Keep);
    let settings_bytes = serde_json::to_vec_pretty(&settings)?;
    let mut report = provider_sync::apply_route_state(
        codex_home,
        original.as_deref(),
        updated.as_bytes(),
        migrate_history.then_some(route.config_provider.as_str()),
        auth_update,
        &settings_bytes,
    )?;
    if let Err(error) = gateway::stop(codex_home) {
        report
            .warnings
            .push(format!("地址已恢复，清理本地代理失败：{error}"));
    }
    Ok(report)
}

// Changing only an address is lightweight. Changing auth must stop the cached auth owner first.
// None means keep/restore official auth; Some means an exact target credential document.
fn read_auth_for_transition<C>(
    home: &Path,
    target: Option<&[u8]>,
    close: C,
) -> Result<Option<Vec<u8>>, Box<dyn Error>>
where
    C: FnOnce() -> Result<bool, Box<dyn Error>>,
{
    let live = read_optional_file(&home.join("auth.json"))?;
    let changes_auth = match target {
        Some(target) => live.as_deref() != Some(target),
        None => !live.as_deref().is_some_and(is_official_credential),
    };
    if changes_auth {
        close()?;
        return read_optional_file(&home.join("auth.json"));
    }
    Ok(live)
}

pub(crate) fn refresh_provider_models_inner(
    codex_home: &Path,
    id: &str,
) -> Result<ProviderState, Box<dyn Error>> {
    let profiles = ProfileStore::new(codex_home);
    let provider = profiles.load_provider(id)?;
    let key = api_key_from_auth(&provider.auth)?.ok_or("供应商缺少 API Key")?;
    let catalog = model_catalog::fetch_with_auth(
        &provider.record.api_url,
        &key,
        provider.record.protocol == "anthropic_messages",
    )?;
    profiles.update_provider_catalog(id, &catalog.bytes)?;
    list_provider_state(codex_home)
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

#[cfg(test)]
fn activate_provider_inner_with_close<C>(
    codex_home: &Path,
    provider_id: &str,
    close_codex: C,
) -> Result<ProviderSyncReport, Box<dyn Error>>
where
    C: FnOnce() -> Result<bool, Box<dyn Error>>,
{
    activate_provider_inner_with_progress(
        codex_home,
        provider_id,
        close_codex,
        &ProgressReporter::default(),
    )
}

pub(crate) fn activate_provider_inner_with_progress<C>(
    codex_home: &Path,
    provider_id: &str,
    close_codex: C,
    progress: &ProgressReporter,
) -> Result<ProviderSyncReport, Box<dyn Error>>
where
    C: FnOnce() -> Result<bool, Box<dyn Error>>,
{
    if ProfileStore::new(codex_home).keep_official_auth()? {
        return activate_official_route_with_close(codex_home, provider_id, progress, close_codex);
    }
    const TOTAL: u32 = 7;
    progress.stage(1, TOTAL, "准备切换", "正在读取供应商配置。");
    ensure_provider_migration(codex_home)?;
    let profiles = ProfileStore::new(codex_home);
    let mut target = profiles.load_provider(provider_id)?;
    progress.stage(
        2,
        TOTAL,
        "读取供应商",
        &format!("正在加载 {} 的配置和密钥。", target.record.name),
    );
    if target.catalog.is_none() || target.record.inference_endpoint.is_none() {
        progress.stage(
            2,
            TOTAL,
            "探测上游协议",
            "供应商缺少探测结果，正在探测协议并拉取模型目录。",
        );
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
        let config = config::build_provider_config_from_snapshot(
            source,
            source,
            &target.record.name,
            &direct_base_url,
            &key,
        )?;
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
    let provider_key = api_key_from_auth(&target.auth)?.ok_or("供应商缺少 API Key")?;
    let using_gateway = target.record.routing_mode == "local";
    let close_with_progress = || {
        progress.stage(
            3,
            TOTAL,
            "关闭 Codex 桌面端",
            "正在请求 Codex 退出，避免配置文件被占用。最多等待 5 秒。",
        );
        close_codex()
    };
    codex_process::after_closed_with(close_with_progress, || {
        let original = read_optional_file(&codex_home.join("config.toml"))?;
        let original_bytes = original.as_deref().unwrap_or_default();
        let original_text = std::str::from_utf8(original_bytes)?;
        let active_id =
            detect_active_provider_id(codex_home, &profiles, &profiles.list_providers()?)?;
        progress.stage(
            4,
            TOTAL,
            "备份当前配置",
            "正在保存当前供应商或官方登录的快照。",
        );
        if let Some(active_id) = active_id.as_deref().filter(|id| *id != provider_id) {
            snapshot_provider_without_clobbering_key(&profiles, active_id, original_bytes)?;
        }
        if is_official_config(original_text)? {
            capture_official_profile(codex_home, &profiles, original_bytes)?;
        }

        let prepared = if using_gateway {
            progress.stage(
                5,
                TOTAL,
                "启动本地路由",
                "正在准备新路由，旧路由保持可用直到切换提交。",
            );
            Some(gateway::prepare(codex_home, provider_id)?)
        } else {
            None
        };
        let active_base_url = if let Some(prepared) = prepared.as_ref() {
            gateway::local_base_url(prepared.port())
        } else {
            upstream::base_url_for_endpoint(
                target
                    .record
                    .inference_endpoint
                    .as_deref()
                    .ok_or("供应商缺少已探测的推理接口")?,
                &target.record.protocol,
            )?
        };
        let updated = config::build_provider_config_from_snapshot(
            original_text,
            std::str::from_utf8(&target.config)?,
            &target.record.name,
            &active_base_url,
            &provider_key,
        )?;
        let updated = if using_gateway {
            config::with_provider_websockets(&updated, PROVIDER_ID, Some(false))?
        } else {
            updated
        };
        verify_provider_content(
            updated.as_bytes(),
            &target.auth,
            &target.record.name,
            &active_base_url,
        )?;
        profiles.update_provider_snapshot(provider_id, updated.as_bytes(), &target.auth)?;
        let auth_update = AuthUpdate::Replace(&target.auth);
        report_history_stage(progress, 6, TOTAL);
        let mut settings = profiles.load_settings()?;
        let update_settings = settings.official_route.is_some();
        settings.keep_official_auth = false;
        settings.official_route = None;
        let settings = update_settings
            .then(|| serde_json::to_vec_pretty(&settings))
            .transpose()?;
        let mut report = provider_sync::apply_provider_state_with_settings(
            codex_home,
            original.as_deref(),
            updated.as_bytes(),
            PROVIDER_ID,
            auth_update,
            settings.as_deref(),
        )?;
        if let Some(prepared) = prepared {
            report.warnings.extend(prepared.commit());
        } else if let Err(error) = gateway::stop(codex_home) {
            report
                .warnings
                .push(format!("切换已完成，旧路由清理失败：{error}"));
        }
        progress.stage(7, TOTAL, "完成", "供应商已切换。");
        Ok(report)
    })
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
    if profiles
        .load_settings()?
        .official_route
        .as_ref()
        .is_some_and(|route| route.direct_provider_id.as_deref() == Some(provider_id))
    {
        return Err("关闭路由时需要恢复此供应商，请先关闭常驻路由再删除".into());
    }
    gateway::stop_provider(codex_home, provider_id)?;
    profiles.delete_provider(provider_id)
}

pub(crate) fn ensure_provider_migration(codex_home: &Path) -> Result<(), Box<dyn Error>> {
    profiles::migrate_legacy_profile_storage(codex_home)?;
    let profiles = ProfileStore::new(codex_home);
    let first_import = !profiles.provider_registry_exists();
    let active_config = read_optional_file(&codex_home.join("config.toml"))?;
    let active_auth = read_optional_file(&codex_home.join("auth.json"))?;
    // Route settings own the upstream identity. Never import our loopback endpoint.
    if profiles.load_settings()?.official_route.is_none()
        && let Some(config) = active_config.as_deref()
    {
        import_provider_snapshot(&profiles, codex_home, config, active_auth.as_deref())?;
    }
    if first_import
        && let Some((config, auth)) = profiles
            .load_custom_config()?
            .zip(profiles.load_custom_auth()?)
    {
        import_provider_snapshot(&profiles, codex_home, &config, Some(&auth))?;
    }
    profiles.ensure_provider_registry()
}

fn import_provider_snapshot(
    profiles: &ProfileStore,
    codex_home: &Path,
    config: &[u8],
    auth: Option<&[u8]>,
) -> Result<(), Box<dyn Error>> {
    let provider_import::Inspection::Ready(candidate) =
        provider_import::inspect(std::str::from_utf8(config)?, auth)
    else {
        // NeedsInput is returned to the UI as a warning by list_provider_state_read_only.
        // A missing external credential must not make the API configuration form inaccessible.
        return Ok(());
    };
    let records = profiles.list_providers()?;
    if find_matching_provider(codex_home, profiles, &records, &candidate)?.is_some() {
        return Ok(());
    }
    let mut name = candidate.name.clone();
    let mut suffix = 1;
    while records
        .iter()
        .any(|record| record.name.eq_ignore_ascii_case(&name))
    {
        let short_id: String = candidate.provider_id.chars().take(32).collect();
        let ending = if suffix == 1 {
            format!(" ({short_id})")
        } else {
            format!(" ({short_id}-{suffix})")
        };
        let prefix: String = candidate
            .name
            .chars()
            .take(80 - ending.chars().count())
            .collect();
        name = format!("{prefix}{ending}");
        suffix += 1;
    }
    // Keep the original configuration as the import snapshot. Only the profile's
    // credential is normalized; the live auth/config and history are untouched.
    let auth = build_custom_auth(&candidate.api_key)?;
    profiles.save_provider_without_catalog(&name, &candidate.api_url, &auth, config)?;
    Ok(())
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
    if let Some(route) = profiles.load_settings()?.official_route {
        let text = std::str::from_utf8(&config)?;
        if verify_route_config(text, &route).is_ok()
            && records.iter().any(|record| record.id == route.provider_id)
        {
            return Ok(Some(route.provider_id));
        }
    }
    let provider_import::Inspection::Ready(candidate) = provider_import::inspect(
        std::str::from_utf8(&config)?,
        read_optional_file(&codex_home.join("auth.json"))?.as_deref(),
    ) else {
        return Ok(None);
    };
    find_matching_provider(codex_home, profiles, records, &candidate)
}

fn find_matching_provider(
    codex_home: &Path,
    profiles: &ProfileStore,
    records: &[ProviderRecord],
    candidate: &provider_import::ImportCandidate,
) -> Result<Option<String>, Box<dyn Error>> {
    // Prefer the display name, but an imported name may have been disambiguated
    // or renamed. URL and credential must both match in every case.
    for record in records
        .iter()
        .filter(|r| r.name == candidate.name)
        .chain(records.iter().filter(|r| r.name != candidate.name))
    {
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
        if expected_url.as_deref() != Some(candidate.api_url.as_str()) {
            continue;
        }
        let profile = profiles.load_provider(&record.id)?;
        let profile_key = api_key_from_auth(&profile.auth)?;
        if profile_key.as_deref() == Some(candidate.api_key.as_str()) {
            return Ok(Some(record.id.clone()));
        }
    }
    Ok(None)
}

pub(crate) fn switch_to_official_with_progress(
    codex_home: &Path,
    progress: &ProgressReporter,
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    if ProfileStore::new(codex_home)
        .load_settings()?
        .official_route
        .is_some()
        && read_optional_file(&codex_home.join("auth.json"))?
            .as_deref()
            .is_some_and(is_official_credential)
    {
        oauth::ensure_login_active()?;
        return activate_official_route(codex_home, "openai", progress);
    }
    const TOTAL: u32 = 7;
    progress.stage(1, TOTAL, "准备切换", "正在检查官方登录状态。");
    oauth::ensure_login_active()?;
    progress.stage(
        2,
        TOTAL,
        "关闭 Codex",
        "先停止本地认证写入，避免同时刷新令牌。",
    );
    codex_process::close_if_running()?;
    fs::create_dir_all(codex_home)?;
    let profiles = ProfileStore::new(codex_home);
    let mut candidates = Vec::new();
    // Credential selection is independent of the active provider: custom mode may retain fresh OAuth tokens.
    let live_auth = read_optional_file(&codex_home.join("auth.json"))?;
    if let Some(auth) = live_auth.clone() {
        push_unique_auth(&mut candidates, auth);
    }
    // A live official login is authoritative, even if revoked. Never silently switch to a saved account.
    if !live_auth.as_deref().is_some_and(is_official_credential)
        && profiles.load_settings()?.official_route.is_none()
        && let Some(saved) = profiles.load_official()?
    {
        push_unique_auth(&mut candidates, saved.auth);
    }
    let selected = select_official_auth(&candidates)?;
    let refreshed_live = selected.as_ref().is_some_and(|auth| {
        live_auth.as_deref().is_some_and(is_official_credential) && live_auth.as_ref() != Some(auth)
    });
    let mut official_auth = match selected {
        Some(auth) => auth,
        None => {
            progress.stage(
                3,
                TOTAL,
                "等待浏览器登录",
                "请完成 ChatGPT 登录，或点击取消。最多等待 10 分钟。",
            );
            oauth::browser_login()?
        }
    };
    // Save rotated credentials before any unrelated close/write failure can lose them.
    let saved_config = profiles.load_official_config()?.unwrap_or_default();
    profiles.save_official(&saved_config, &official_auth)?;
    if refreshed_live {
        if read_optional_file(&codex_home.join("auth.json"))? != live_auth {
            return Err("认证检查期间 auth.json 已被其他程序修改，已保留最新凭据".into());
        }
        profiles::atomic_write_private(&codex_home.join("auth.json"), &official_auth)?;
        push_unique_auth(&mut candidates, official_auth.clone());
    }
    oauth::ensure_login_active()?;
    progress.stage(4, TOTAL, "关闭 Codex", "正在等待 Codex 完成退出。");
    codex_process::close_if_running()?;
    oauth::ensure_login_active()?;
    if let Some(current) = read_optional_file(&codex_home.join("auth.json"))?
        && !candidates.contains(&current)
        && oauth::inspect_auth(&current) != LocalAuthState::Invalid
    {
        official_auth = select_official_auth(&[current])?
            .ok_or("Codex 退出时更新了认证，但新认证已失效，请重试")?;
    }
    // The only configuration used below is read after the last writer has exited.
    let original = read_optional_file(&codex_home.join("config.toml"))?;
    let original_bytes = original.as_deref().unwrap_or_default();
    let original_text = std::str::from_utf8(original_bytes)?;
    let active_is_official = is_official_config(original_text)?
        || profiles
            .load_settings()?
            .official_route
            .as_ref()
            .is_some_and(|route| {
                route.config_provider == "openai"
                    && verify_route_config(original_text, route).is_ok()
            });
    if !active_is_official {
        let records = profiles.list_providers()?;
        if let Some(id) = detect_active_provider_id(codex_home, &profiles, &records)? {
            snapshot_provider_without_clobbering_key(&profiles, &id, original_bytes)?;
        }
    }
    let official_config = if active_is_official {
        official_snapshot_config(original_text, &profiles)?.into_bytes()
    } else if !saved_config.is_empty() {
        saved_config
    } else {
        build_official_config(original_text)?.into_bytes()
    };
    let official_config = if is_official_config(std::str::from_utf8(&official_config)?)? {
        official_config
    } else {
        build_official_config(std::str::from_utf8(&official_config)?)?.into_bytes()
    };
    profiles.save_official(&official_config, &official_auth)?;
    report_history_stage(progress, 6, TOTAL);
    let mut report = activate_official(
        codex_home,
        original.as_deref(),
        &official_config,
        &official_auth,
    )?;
    if let Err(error) = gateway::stop(codex_home) {
        report
            .warnings
            .push(format!("官方切换已完成，旧路由清理失败：{error}"));
    }
    if profiles.keep_official_auth()? {
        let route_report = activate_official_route(codex_home, "openai", progress)?;
        report.warnings.extend(route_report.warnings);
    }
    progress.stage(7, TOTAL, "完成", "已切换到官方登录。");
    Ok(report)
}

fn push_unique_auth(candidates: &mut Vec<Vec<u8>>, candidate: Vec<u8>) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

fn select_official_auth(candidates: &[Vec<u8>]) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    select_official_auth_with(candidates, oauth::validate_auth, oauth::refresh_auth)
}

fn select_official_auth_with<V, R>(
    candidates: &[Vec<u8>],
    mut validate: V,
    mut refresh: R,
) -> Result<Option<Vec<u8>>, Box<dyn Error>>
where
    V: FnMut(&[u8]) -> Result<AuthHealth, Box<dyn Error>>,
    R: FnMut(&[u8]) -> Result<AuthHealth, Box<dyn Error>>,
{
    for candidate in candidates {
        let health = match oauth::inspect_auth(candidate) {
            LocalAuthState::Invalid => continue,
            LocalAuthState::Current => match validate(candidate)? {
                AuthHealth::Valid(auth) => AuthHealth::Valid(auth),
                AuthHealth::Invalid => refresh(candidate)?,
            },
            LocalAuthState::NeedsRefresh => refresh(candidate)?,
        };
        match health {
            AuthHealth::Valid(auth) => return Ok(Some(auth)),
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
    let config = official_snapshot_config(std::str::from_utf8(config)?, profiles)?;
    let config = config.as_bytes();
    profiles.save_official_config(config)?;
    let Some(candidate) = read_optional_file(&codex_home.join("auth.json"))? else {
        profiles.discard_official_auth()?;
        return Ok(());
    };
    if oauth::inspect_auth(&candidate) == LocalAuthState::Invalid {
        profiles.discard_official_auth()?;
        return Ok(());
    }
    profiles.save_official(config, &candidate)
}

fn official_snapshot_config(text: &str, profiles: &ProfileStore) -> Result<String, Box<dyn Error>> {
    if let Some(route) = profiles.load_settings()?.official_route
        && route.config_provider == "openai"
        && verify_route_config(text, &route).is_ok()
    {
        return config::restore_route_config(text, &route);
    }
    Ok(text.to_string())
}

fn report_history_stage(progress: &ProgressReporter, current: u32, total: u32) {
    progress.stage(
        current,
        total,
        "检查会话历史",
        "正在检查任务提供方，只修改与目标不一致的记录。",
    );
}

fn snapshot_provider_without_clobbering_key(
    profiles: &ProfileStore,
    provider_id: &str,
    config: &[u8],
) -> Result<(), Box<dyn Error>> {
    let stored = profiles.load_provider(provider_id)?;
    // The active provider resolver already matched this exact credential. A live
    // auth.json can belong to another account when fixed provider auth wins.
    profiles.update_provider_snapshot(provider_id, config, &stored.auth)
}

fn is_official_credential(auth: &[u8]) -> bool {
    oauth::inspect_auth(auth) != LocalAuthState::Invalid
}

fn official_credential_available(
    codex_home: &Path,
    profiles: &ProfileStore,
) -> Result<bool, Box<dyn Error>> {
    if read_optional_file(&codex_home.join("auth.json"))?
        .as_deref()
        .is_some_and(is_official_credential)
    {
        return Ok(true);
    }
    Ok(profiles
        .load_official()?
        .is_some_and(|profile| is_official_credential(&profile.auth)))
}

fn activate_official(
    codex_home: &Path,
    original_config: Option<&[u8]>,
    official_config: &[u8],
    official_auth: &[u8],
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    let profiles = ProfileStore::new(codex_home);
    let mut settings = profiles.load_settings()?;
    let settings_update = if settings.official_route.take().is_some() {
        Some(serde_json::to_vec_pretty(&settings)?)
    } else {
        None
    };
    let report = provider_sync::apply_official_state(
        codex_home,
        original_config,
        official_config,
        official_auth,
        settings_update.as_deref(),
    )?;
    if fs::read(codex_home.join("config.toml"))? != official_config {
        return Err("官方 config.toml 恢复后的字节验证失败".into());
    }
    if fs::read(codex_home.join("auth.json"))? != official_auth {
        return Err("官方 auth.json 恢复后的字节验证失败".into());
    }
    Ok(report)
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
pub(crate) fn sync_official_accounts(home: &Path) -> Result<(), Box<dyn Error>> {
    let accounts = crate::official_accounts::Store::new(home);
    if let Some(legacy) = ProfileStore::new(home).load_official()? {
        accounts.import_legacy(&legacy.auth)?;
    }
    if let Some(live) = read_optional_file(&home.join("auth.json"))? {
        accounts.sync_live(&live)?;
    }
    Ok(())
}

pub(crate) fn add_official_account(
    home: &Path,
    progress: &ProgressReporter,
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    add_official_account_with(home, progress, oauth::browser_login_new_account)
}

fn add_official_account_with<L>(
    home: &Path,
    progress: &ProgressReporter,
    login: L,
) -> Result<ProviderSyncReport, Box<dyn Error>>
where
    L: FnOnce() -> Result<Vec<u8>, Box<dyn Error>>,
{
    sync_official_accounts(home)?;
    progress.stage(
        1,
        2,
        "添加官方账号",
        "请在浏览器登录要添加的账号。当前线路保持不变，也可以点击取消。",
    );
    oauth::ensure_login_active()?;
    let auth = login()?;
    oauth::ensure_login_active()?;
    crate::official_accounts::Store::new(home).save(&auth)?;
    progress.finish(2);
    Ok(empty_route_report())
}

pub(crate) fn switch_official_account(
    home: &Path,
    id: &str,
    progress: &ProgressReporter,
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    switch_official_account_with(
        home,
        id,
        progress,
        codex_process::close_if_running,
        |auth| select_official_auth(&[auth.to_vec()]),
    )
}

fn switch_official_account_with<C, V>(
    home: &Path,
    id: &str,
    progress: &ProgressReporter,
    close: C,
    validate: V,
) -> Result<ProviderSyncReport, Box<dyn Error>>
where
    C: FnOnce() -> Result<bool, Box<dyn Error>>,
    V: FnOnce(&[u8]) -> Result<Option<Vec<u8>>, Box<dyn Error>>,
{
    let accounts = crate::official_accounts::Store::new(home);
    let selected = accounts.load(id)?;
    let profiles = ProfileStore::new(home);
    let settings = profiles.load_settings()?;
    let live = read_optional_file(&home.join("auth.json"))?;
    if let Some(live) = live.as_deref() {
        accounts.sync_live(live)?;
        // Same account and same credentials: this is just an outlet change.
        if settings.official_route.is_some()
            && crate::official_accounts::identity(live).is_some()
            && accounts.load(id)?.auth == serde_json::from_slice::<serde_json::Value>(live)?
        {
            return activate_official_route_with_close(home, "openai", progress, close);
        }
    }
    oauth::ensure_login_active()?;
    progress.stage(
        1,
        3,
        "切换官方账号",
        "正在关闭 Codex，保存当前账号最后更新的凭据。",
    );
    close()?;
    oauth::ensure_login_active()?;
    let original = read_optional_file(&home.join("config.toml"))?;
    let original_auth = read_optional_file(&home.join("auth.json"))?;
    let text = std::str::from_utf8(original.as_deref().unwrap_or_default())?;
    let mut settings = profiles.load_settings()?;
    if let Some(route) = &settings.official_route {
        verify_route_config(text, route)?;
    }
    if let Some(live) = original_auth.as_deref() {
        accounts.sync_live(live)?;
    }
    let current = accounts.load(id)?;
    let candidate = serde_json::to_vec_pretty(&current.auth)?;
    progress.stage(
        2,
        3,
        "检查选中账号",
        "只验证这个账号；失效时请重新添加该账号。",
    );
    let auth = validate(&candidate)?.ok_or("这个官方账号的登录已失效，请通过加号重新添加该账号")?;
    if crate::official_accounts::identity(&auth).as_ref() != Some(&selected.identity) {
        return Err("认证返回的账号与选中账号不一致，已停止切换".into());
    }
    // A refresh token may already be rotated. Save it before any cancellable work.
    accounts.save(&auth)?;
    oauth::ensure_login_active()?;
    if read_optional_file(&home.join("auth.json"))? != original_auth {
        return Err("切换期间 auth.json 已被其他程序修改，已保留双方凭据，请重试".into());
    }
    let mut report = if let Some(route) = settings.official_route.as_mut() {
        if route.resident && route.http_transport.is_some() {
            let prepared = gateway::prepare_resident(
                home,
                "openai",
                gateway::route_port(&route.local_base_url)?,
            )?;
            route.provider_id = "openai".into();
            let mut report = provider_sync::apply_route_state(
                home,
                original.as_deref(),
                text.as_bytes(),
                None,
                AuthUpdate::Replace(&auth),
                &serde_json::to_vec_pretty(&settings)?,
            )?;
            report.warnings.extend(prepared.commit());
            report
        } else {
            return Err("请先用当前账号升级旧版本地路由，再切换其他官方账号".into());
        }
    } else {
        if let Some(active) =
            detect_active_provider_id(home, &profiles, &profiles.list_providers()?)?
        {
            snapshot_provider_without_clobbering_key(
                &profiles,
                &active,
                original.as_deref().unwrap_or_default(),
            )?;
        }
        let config = if is_official_config(text)? {
            text.as_bytes().to_vec()
        } else if let Some(saved) = profiles.load_official_config()?.filter(|b| !b.is_empty()) {
            build_official_config(std::str::from_utf8(&saved)?)?.into_bytes()
        } else {
            build_official_config(text)?.into_bytes()
        };
        activate_official(home, original.as_deref(), &config, &auth)?
    };
    // The live-file transaction is committed. Snapshot/cleanup errors are warnings,
    // never report a failed switch after the selected account is already active.
    let snapshot = (|| -> Result<(), Box<dyn Error>> {
        accounts.mark_live(&auth)?;
        let live_config = fs::read_to_string(home.join("config.toml"))?;
        profiles.save_official(
            official_snapshot_config(&live_config, &profiles)?.as_bytes(),
            &auth,
        )
    })();
    if let Err(e) = snapshot {
        report
            .warnings
            .push(format!("账号已切换，保存快照失败：{e}"));
    }
    if settings.official_route.is_none()
        && let Err(e) = gateway::stop(home)
    {
        report
            .warnings
            .push(format!("账号已切换，旧路由清理失败：{e}"));
    }
    report
        .warnings
        .push("已切换官方账号，请重新打开 Codex。".into());
    progress.finish(3);
    Ok(report)
}

#[cfg(test)]
#[path = "multi_account_tests.rs"]
mod multi_account_tests;
