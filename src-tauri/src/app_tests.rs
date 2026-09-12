use super::*;
use crate::desktop::operation_error;
use crate::profiles::AppSettings;
use base64::Engine;
use serde_json::{Value, json};
use tempfile::tempdir;

fn official_auth(expires_at: i64, refresh_token: &str) -> Vec<u8> {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&json!({"exp": expires_at})).expect("serialize access claims"));
    serde_json::to_vec(&json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "access_token": format!("{header}.{payload}.signature"),
            "refresh_token": refresh_token
        }
    }))
    .expect("serialize official auth")
}

fn catalog_ids(catalog: &[u8]) -> Vec<String> {
    let value: Value = serde_json::from_slice(catalog).expect("parse catalog");
    value["models"]
        .as_array()
        .expect("models array")
        .iter()
        .map(|model| model["slug"].as_str().expect("model slug").to_string())
        .collect()
}

fn save_fixture_provider(
    codex_home: &Path,
    name: &str,
    api_url: &str,
    api_key: &str,
    models: &[&str],
) -> ProviderRecord {
    let profiles = ProfileStore::new(codex_home);
    let catalog = model_catalog::build(models.iter().map(|model| (*model).to_string()))
        .expect("build catalog");
    let auth = build_custom_auth(api_key).expect("build auth");
    let source = read_optional_file(&codex_home.join("config.toml"))
        .expect("read config")
        .unwrap_or_default();
    let source = std::str::from_utf8(&source).expect("utf8 config");
    let config =
        build_provider_config(source, name, api_url, api_key).expect("build provider config");
    profiles
        .save_provider(
            None,
            name,
            api_url,
            &auth,
            Some(&catalog.bytes),
            config.as_bytes(),
        )
        .expect("save provider")
}

#[test]
fn operation_errors_include_home_stage_and_reason() {
    let error = operation_error(
        Path::new("/fixture/.codex"),
        "写入配置",
        "Permission denied",
    );
    assert!(error.contains("Codex 目录：/fixture/.codex"));
    assert!(error.contains("失败阶段：写入配置"));
    assert!(error.contains("原因：Permission denied"));
}

#[test]
fn cancelled_login_stays_a_plain_status() {
    assert_eq!(
        operation_error(
            Path::new("/fixture/.codex"),
            "恢复官方登录",
            "官方登录已取消"
        ),
        "官方登录已取消"
    );
}

#[test]
fn provider_config_preserves_user_settings_and_replaces_managed_fields() {
    let original = r#"model = "official-model"
model_reasoning_effort = "xhigh"
model_verbosity = "high"
approval_policy = "never"
model_catalog_json = "/old/models.json"

[desktop]
localeOverride = "zh-CN"

[model_providers.existing]
name = "Existing"

[model_providers.custom]
name = "Old"
base_url = "https://old.example"
request_max_retries = 9
"#;
    let updated = build_provider_config(
        original,
        "新供应商",
        "https://api.example.com",
        "fixture-key",
    )
    .expect("build config");
    let document = parse_config(&updated).expect("parse config");
    assert_eq!(document["model"].as_str(), Some("official-model"));
    assert_eq!(document["model_reasoning_effort"].as_str(), Some("xhigh"));
    assert_eq!(document["model_verbosity"].as_str(), Some("high"));
    assert_eq!(document["approval_policy"].as_str(), Some("never"));
    assert_eq!(
        document["desktop"]["localeOverride"].as_str(),
        Some("zh-CN")
    );
    assert_eq!(
        document["model_providers"]["existing"]["name"].as_str(),
        Some("Existing")
    );
    assert_eq!(document["model_provider"].as_str(), Some("custom"));
    assert_eq!(
        document["model_providers"]["custom"]["name"].as_str(),
        Some("新供应商")
    );
    assert_eq!(
        document["model_providers"]["custom"]["base_url"].as_str(),
        Some("https://api.example.com")
    );
    assert_eq!(
        document["model_providers"]["custom"]["wire_api"].as_str(),
        Some("responses")
    );
    assert_eq!(
        document["model_providers"]["custom"]["requires_openai_auth"].as_bool(),
        Some(true)
    );
    assert!(!document.contains_key("model_catalog_json"));
    assert_eq!(
        document["model_providers"]["custom"]
            .get("request_max_retries")
            .and_then(Item::as_integer),
        Some(9)
    );
    assert_eq!(
        document["model_providers"]["custom"]["experimental_bearer_token"].as_str(),
        Some("fixture-key")
    );
}

#[test]
fn custom_auth_contains_only_openai_api_key() {
    let auth: Value =
        serde_json::from_slice(&build_custom_auth("fixture-key").expect("build custom auth"))
            .expect("parse auth");
    assert_eq!(auth, json!({"OPENAI_API_KEY": "fixture-key"}));
}

#[test]
fn stores_multiple_providers_with_independent_credentials_and_catalogs() {
    let directory = tempdir().expect("tempdir");
    let first = save_fixture_provider(
        directory.path(),
        "供应商一",
        "https://one.example",
        "first-key",
        &["model-one", "shared"],
    );
    let second = save_fixture_provider(
        directory.path(),
        "供应商二",
        "https://two.example/v1",
        "second-key",
        &["model-two"],
    );
    let profiles = ProfileStore::new(directory.path());
    let first_profile = profiles.load_provider(&first.id).expect("first profile");
    let second_profile = profiles.load_provider(&second.id).expect("second profile");
    assert_eq!(
        api_key_from_auth(&first_profile.auth).unwrap().as_deref(),
        Some("first-key")
    );
    assert_eq!(
        api_key_from_auth(&second_profile.auth).unwrap().as_deref(),
        Some("second-key")
    );
    assert_eq!(
        catalog_ids(first_profile.catalog.as_deref().unwrap()),
        ["model-one", "shared"]
    );
    assert_eq!(
        catalog_ids(second_profile.catalog.as_deref().unwrap()),
        ["model-two"]
    );
}

#[test]
fn official_active_marker_requires_an_official_credential() {
    let directory = tempdir().expect("tempdir");
    let codex_home = directory.path();
    let empty_state = list_provider_state(codex_home).expect("empty state");
    assert!(!empty_state.official_active);

    fs::write(codex_home.join("config.toml"), "model = \"gpt-official\"\n").expect("write config");
    fs::write(
        codex_home.join("auth.json"),
        official_auth(chrono::Utc::now().timestamp() + 3600, "refresh"),
    )
    .expect("write auth");
    let logged_in_state = list_provider_state(codex_home).expect("logged in state");
    assert!(logged_in_state.official_active);
}

#[test]
fn switching_providers_updates_auth_catalog_model_and_active_marker() {
    let directory = tempdir().expect("tempdir");
    let codex_home = directory.path();
    let official_config = b"model = \"official-model\"\napproval_policy = \"never\"\n";
    let official_auth = official_auth(chrono::Utc::now().timestamp() + 3600, "refresh");
    fs::write(codex_home.join("config.toml"), official_config).expect("write config");
    fs::write(codex_home.join("auth.json"), &official_auth).expect("write auth");
    let rollout = codex_home.join("sessions/rollout-fixture.jsonl");
    fs::create_dir_all(rollout.parent().unwrap()).expect("create sessions");
    fs::write(
        &rollout,
        "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\"}}\n{\"message\":\"preserved\"}\n",
    )
    .expect("write rollout");
    let first = save_fixture_provider(
        codex_home,
        "供应商一",
        "https://one.example",
        "first-key",
        &["model-one"],
    );
    let second = save_fixture_provider(
        codex_home,
        "供应商二",
        "https://two.example",
        "second-key",
        &["model-two"],
    );

    let first_report = activate_provider_inner_with_close(codex_home, &first.id, || Ok(false))
        .expect("activate first");
    assert_eq!(first_report.rollout_files_updated, 1);
    let first_config = fs::read_to_string(codex_home.join("config.toml")).expect("first config");
    let first_document = parse_config(&first_config).expect("parse first config");
    assert_eq!(first_document["model"].as_str(), Some("official-model"));
    assert_eq!(first_document["approval_policy"].as_str(), Some("never"));
    assert!(!first_document.contains_key("model_catalog_json"));
    assert_eq!(
        api_key_from_auth(&fs::read(codex_home.join("auth.json")).unwrap())
            .unwrap()
            .as_deref(),
        Some("first-key")
    );

    let second_report = activate_provider_inner_with_close(codex_home, &second.id, || Ok(false))
        .expect("activate second");
    assert_eq!(second_report.rollout_files_updated, 0);
    let state = list_provider_state(codex_home).expect("provider state");
    assert_eq!(
        state.active_provider_id.as_deref(),
        Some(second.id.as_str())
    );
    assert!(!state.official_active);
    let second_config = fs::read_to_string(codex_home.join("config.toml")).expect("second config");
    let second_document = parse_config(&second_config).expect("parse second config");
    assert_eq!(second_document["model"].as_str(), Some("official-model"));
    assert_eq!(
        second_document["model_providers"]["custom"]["name"].as_str(),
        Some("供应商二")
    );
    assert!(!second_document.contains_key("model_catalog_json"));
    assert!(delete_provider_inner(codex_home, &second.id).is_err());
    delete_provider_inner(codex_home, &first.id).expect("delete inactive provider");
    assert_eq!(
        ProfileStore::new(codex_home)
            .list_providers()
            .unwrap()
            .len(),
        1
    );

    let saved_official = ProfileStore::new(codex_home)
        .load_official()
        .expect("load official")
        .expect("official profile");
    let active_custom = fs::read(codex_home.join("config.toml")).expect("active custom");
    activate_official(
        codex_home,
        Some(&active_custom),
        &saved_official.config,
        &saved_official.auth,
    )
    .expect("restore official");
    assert_eq!(
        fs::read(codex_home.join("config.toml")).unwrap(),
        official_config
    );
    assert_eq!(
        fs::read(codex_home.join("auth.json")).unwrap(),
        official_auth
    );
    assert!(fs::read_to_string(rollout).unwrap().contains("preserved"));
}

#[test]
fn a_codex_close_failure_leaves_live_state_unchanged() {
    let directory = tempdir().expect("tempdir");
    let codex_home = directory.path();
    let original_config = b"model = \"official-model\"\napproval_policy = \"never\"\n";
    let original_auth = official_auth(chrono::Utc::now().timestamp() + 3600, "refresh");
    fs::write(codex_home.join("config.toml"), original_config).expect("write config");
    fs::write(codex_home.join("auth.json"), &original_auth).expect("write auth");
    let rollout = codex_home.join("sessions/rollout-fixture.jsonl");
    fs::create_dir_all(rollout.parent().unwrap()).expect("create sessions");
    let rollout_content =
        b"{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\"}}\n";
    fs::write(&rollout, rollout_content).expect("write rollout");
    let provider = save_fixture_provider(
        codex_home,
        "供应商",
        "https://provider.example",
        "provider-key",
        &["provider-model"],
    );

    let error = activate_provider_inner_with_close(codex_home, &provider.id, || {
        Err("Codex 关闭失败".into())
    })
    .expect_err("close failure");

    assert_eq!(error.to_string(), "Codex 关闭失败");
    assert_eq!(
        fs::read(codex_home.join("config.toml")).unwrap(),
        original_config
    );
    assert_eq!(
        fs::read(codex_home.join("auth.json")).unwrap(),
        original_auth
    );
    assert_eq!(fs::read(rollout).unwrap(), rollout_content);
}

#[test]
fn migrates_the_legacy_custom_profile_once() {
    let directory = tempdir().expect("tempdir");
    let profiles = ProfileStore::new(directory.path());
    let config = build_provider_config(
        "model = \"legacy-model\"\n",
        "旧供应商",
        "https://legacy.example",
        "legacy-key",
    )
    .expect("legacy config");
    profiles
        .save_custom_config(config.as_bytes())
        .expect("save legacy config");
    profiles
        .save_custom_auth(&build_custom_auth("legacy-key").expect("legacy auth"))
        .expect("save legacy auth");

    ensure_provider_migration(directory.path()).expect("migrate legacy profile");
    ensure_provider_migration(directory.path()).expect("migration is idempotent");
    let records = profiles.list_providers().expect("list migrated providers");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, "旧供应商");
    assert_eq!(records[0].api_url, "https://legacy.example");
    assert_eq!(records[0].model_count, 0);
    assert!(records[0].catalog_file.is_none());
    let profile = profiles
        .load_provider(&records[0].id)
        .expect("migrated profile");
    assert_eq!(
        api_key_from_auth(&profile.auth).unwrap().as_deref(),
        Some("legacy-key")
    );
}

#[test]
fn failed_model_sync_does_not_create_a_provider_or_change_live_files() {
    let directory = tempdir().expect("tempdir");
    let codex_home = directory.path();
    let original_config = b"model = \"official\"\n";
    let original_auth = b"{\"auth_mode\":\"chatgpt\"}";
    fs::write(codex_home.join("config.toml"), original_config).expect("write config");
    fs::write(codex_home.join("auth.json"), original_auth).expect("write auth");

    let error = save_provider_inner_with(
        codex_home,
        None,
        "失败供应商",
        "https://failed.example",
        "fixture-key",
        |_, _| Ok(()),
        |_, _| Err("模型同步失败".into()),
    )
    .expect_err("catalog failure");
    assert_eq!(error.to_string(), "模型同步失败");
    assert_eq!(
        fs::read(codex_home.join("config.toml")).unwrap(),
        original_config
    );
    assert_eq!(
        fs::read(codex_home.join("auth.json")).unwrap(),
        original_auth
    );
    assert!(
        ProfileStore::new(codex_home)
            .list_providers()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn chat_only_provider_waits_for_confirmation_without_changing_live_files() {
    use std::thread;
    use tiny_http::{Header, Response, Server, StatusCode};

    let directory = tempdir().expect("tempdir");
    let codex_home = directory.path();
    let original_config = b"model = \"official\"\napproval_policy = \"never\"\n";
    let original_auth = official_auth(chrono::Utc::now().timestamp() + 3600, "refresh");
    fs::write(codex_home.join("config.toml"), original_config).expect("write config");
    fs::write(codex_home.join("auth.json"), &original_auth).expect("write auth");

    let server = Server::http("127.0.0.1:0").expect("server");
    let address = server.server_addr();
    let handle = thread::spawn(move || {
        for _ in 0..4 {
            let request = server.recv().expect("request");
            if request.method().as_str() == "GET" {
                request
                    .respond(
                        Response::from_string(r#"{"data":[{"id":"chat-model"}]}"#).with_header(
                            Header::from_bytes("Content-Type", "application/json")
                                .expect("content type"),
                        ),
                    )
                    .expect("respond models");
            } else if request.url().ends_with("/responses") {
                request
                    .respond(Response::empty(StatusCode(404)))
                    .expect("respond missing responses");
            } else {
                assert!(request.url().ends_with("/chat/completions"));
                request
                    .respond(Response::empty(StatusCode(400)))
                    .expect("respond chat probe");
            }
        }
    });

    let saved = save_provider_inner(
        codex_home,
        None,
        "Chat 上游",
        &format!("http://{address}"),
        "fixture-key",
    )
    .expect("save provider");
    handle.join().expect("join server");

    assert!(saved.routing_required);
    assert_eq!(saved.provider.protocol, "openai_chat");
    assert_eq!(saved.provider.routing_mode, "direct");
    assert_eq!(
        fs::read(codex_home.join("config.toml")).unwrap(),
        original_config
    );
    assert_eq!(
        fs::read(codex_home.join("auth.json")).unwrap(),
        original_auth
    );

    enable_provider_routing_inner(codex_home, &saved.provider.id).expect("enable routing");
    assert_eq!(
        fs::read(codex_home.join("config.toml")).unwrap(),
        original_config
    );
    assert_eq!(
        fs::read(codex_home.join("auth.json")).unwrap(),
        original_auth
    );
    let record = ProfileStore::new(codex_home)
        .list_providers()
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(record.routing_mode, "local");
}

#[test]
fn editing_a_url_requires_its_api_key() {
    let directory = tempdir().expect("tempdir");
    let provider = save_fixture_provider(
        directory.path(),
        "供应商",
        "https://old.example",
        "old-key",
        &["model-one"],
    );
    let error = save_provider_inner_with(
        directory.path(),
        Some(&provider.id),
        "供应商",
        "https://new.example",
        "",
        |_, _| panic!("probe must not run"),
        |_, _| panic!("catalog fetch must not run"),
    )
    .expect_err("changed url requires key");
    assert!(error.to_string().contains("API URL 已变化"));
}

#[test]
fn a_new_provider_requires_an_api_key_before_network_checks() {
    let directory = tempdir().expect("tempdir");
    let error = save_provider_inner_with(
        directory.path(),
        None,
        "供应商",
        "https://new.example",
        "",
        |_, _| panic!("probe must not run"),
        |_, _| panic!("catalog fetch must not run"),
    )
    .expect_err("new provider requires key");
    assert_eq!(error.to_string(), "API Key 不能为空");
}

#[test]
fn official_config_removes_a_third_party_catalog_pointer() {
    let original = "model_provider = \"custom\"\nmodel_catalog_json = \"/tmp/models.json\"\nmodel = \"test\"\n";
    let updated = build_official_config(original).expect("build official config");
    assert!(is_official_config(&updated).expect("classify official"));
    assert!(updated.contains("model = \"test\""));
    assert!(!updated.contains("model_provider"));
    assert!(!updated.contains("model_catalog_json"));
}

#[test]
fn rejects_invalid_provider_inputs() {
    assert!(normalize_api_url("file:///tmp/api").is_err());
    assert!(normalize_api_url("https://proxy.example/v1?x=1").is_err());
    assert!(normalize_api_url("https://user:password@proxy.example/v1").is_err());
    assert!(validate_api_key("has whitespace").is_err());
    assert!(validate_provider_name("   ").is_err());
}

#[test]
fn official_selection_skips_custom_auth_and_reuses_current_token() {
    let custom = build_custom_auth("fixture-key").expect("custom auth");
    let official = official_auth(chrono::Utc::now().timestamp() + 3600, "refresh");
    let mut refresh_calls = 0;
    let selected = select_official_auth_with(&[custom, official.clone()], |_| {
        refresh_calls += 1;
        Err("refresh should not run".into())
    })
    .expect("select current auth")
    .expect("official auth");
    assert_eq!(selected, official);
    assert_eq!(refresh_calls, 0);
}

#[test]
fn official_selection_refreshes_only_an_expiring_candidate() {
    let custom = build_custom_auth("fixture-key").expect("custom auth");
    let expiring = official_auth(chrono::Utc::now().timestamp() + 60, "old-refresh");
    let refreshed = official_auth(chrono::Utc::now().timestamp() + 3600, "new-refresh");
    let mut refresh_calls = 0;
    let selected = select_official_auth_with(&[custom, expiring], |_| {
        refresh_calls += 1;
        Ok(AuthHealth::Valid(refreshed.clone()))
    })
    .expect("select refreshed auth")
    .expect("official auth");
    assert_eq!(selected, refreshed);
    assert_eq!(refresh_calls, 1);
}

#[test]
fn keep_official_auth_preserves_chatgpt_tokens_and_writes_bearer_token() {
    let directory = tempdir().expect("tempdir");
    let codex_home = directory.path();
    let official_config = b"model = \"official-model\"\napproval_policy = \"never\"\n";
    let official_auth = official_auth(chrono::Utc::now().timestamp() + 3600, "refresh");
    fs::write(codex_home.join("config.toml"), official_config).expect("write config");
    fs::write(codex_home.join("auth.json"), &official_auth).expect("write auth");
    let provider = save_fixture_provider(
        codex_home,
        "供应商",
        "https://provider.example",
        "provider-key",
        &["provider-model"],
    );
    ProfileStore::new(codex_home)
        .save_settings(&AppSettings {
            keep_official_auth: true,
        })
        .expect("enable keep official auth");

    activate_provider_inner_with_close(codex_home, &provider.id, || Ok(false))
        .expect("activate with keep official auth");

    assert_eq!(
        fs::read(codex_home.join("auth.json")).unwrap(),
        official_auth
    );
    let config = fs::read_to_string(codex_home.join("config.toml")).expect("read config");
    let document = parse_config(&config).expect("parse config");
    assert_eq!(
        document["model_providers"]["custom"]["experimental_bearer_token"].as_str(),
        Some("provider-key")
    );
    assert_eq!(
        document["model_providers"]["custom"]["name"].as_str(),
        Some("供应商")
    );
    let state = list_provider_state(codex_home).expect("provider state");
    assert_eq!(
        state.active_provider_id.as_deref(),
        Some(provider.id.as_str())
    );
    assert!(!state.official_active);
    assert!(state.keep_official_auth);
    assert!(state.official_auth_available);

    let second = save_fixture_provider(
        codex_home,
        "供应商二",
        "https://two.example",
        "second-key",
        &["model-two"],
    );
    activate_provider_inner_with_close(codex_home, &second.id, || Ok(false))
        .expect("switch to second provider");
    assert_eq!(
        fs::read(codex_home.join("auth.json")).unwrap(),
        official_auth
    );
    assert_eq!(
        api_key_from_auth(
            &ProfileStore::new(codex_home)
                .load_provider(&provider.id)
                .expect("first profile")
                .auth
        )
        .unwrap()
        .as_deref(),
        Some("provider-key")
    );
}

#[test]
fn disabling_keep_official_auth_writes_the_api_key_into_auth() {
    let directory = tempdir().expect("tempdir");
    let codex_home = directory.path();
    let official_auth = official_auth(chrono::Utc::now().timestamp() + 3600, "refresh");
    fs::write(
        codex_home.join("config.toml"),
        b"model = \"official-model\"\n",
    )
    .expect("write config");
    fs::write(codex_home.join("auth.json"), &official_auth).expect("write auth");
    let provider = save_fixture_provider(
        codex_home,
        "供应商",
        "https://provider.example",
        "provider-key",
        &["provider-model"],
    );
    ProfileStore::new(codex_home)
        .save_settings(&AppSettings {
            keep_official_auth: true,
        })
        .expect("enable keep official auth");
    activate_provider_inner_with_close(codex_home, &provider.id, || Ok(false))
        .expect("activate while keeping official auth");
    assert_eq!(
        fs::read(codex_home.join("auth.json")).unwrap(),
        official_auth
    );

    set_keep_official_auth_inner(codex_home, false).expect("disable keep official auth");
    assert_eq!(
        api_key_from_auth(&fs::read(codex_home.join("auth.json")).unwrap())
            .unwrap()
            .as_deref(),
        Some("provider-key")
    );
    let state = list_provider_state(codex_home).expect("provider state");
    assert!(!state.keep_official_auth);
    assert_eq!(
        state.active_provider_id.as_deref(),
        Some(provider.id.as_str())
    );
}
