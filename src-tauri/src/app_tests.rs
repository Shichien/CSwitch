use super::*;
use crate::desktop::operation_error;
use crate::profiles::AppSettings;
use base64::Engine;
use serde_json::{Value, json};
use tempfile::tempdir;
use toml_edit::Item;

#[path = "resident_tests.rs"]
mod resident_tests;

#[path = "import_tests.rs"]
mod import_tests;

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
    assert_eq!(
        document["model_catalog_json"].as_str(),
        Some("/old/models.json")
    );
    assert_eq!(
        document["model_providers"]["custom"]
            .get("request_max_retries")
            .and_then(Item::as_integer),
        Some(9)
    );
    assert!(
        document["model_providers"]["custom"]
            .get("experimental_bearer_token")
            .is_none()
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
                    .respond(
                        Response::from_string(
                            r#"{"error":{"param":"model","message":"model is required"}}"#,
                        )
                        .with_status_code(StatusCode(400)),
                    )
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
fn official_config_preserves_a_user_catalog_pointer() {
    let original = "model_provider = \"custom\"\nmodel_catalog_json = \"/tmp/models.json\"\nmodel = \"test\"\n";
    let updated = build_official_config(original).expect("build official config");
    assert!(is_official_config(&updated).expect("classify official"));
    assert!(updated.contains("model = \"test\""));
    assert!(!updated.contains("model_provider"));
    assert!(updated.contains("model_catalog_json"));
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
    let selected = select_official_auth_with(
        &[custom, official.clone()],
        |auth| Ok(AuthHealth::Valid(auth.to_vec())),
        |_| {
            refresh_calls += 1;
            Err("refresh should not run".into())
        },
    )
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
    let selected = select_official_auth_with(
        &[custom, expiring],
        |_| panic!("expired token must not be validated"),
        |_| {
            refresh_calls += 1;
            Ok(AuthHealth::Valid(refreshed.clone()))
        },
    )
    .expect("select refreshed auth")
    .expect("official auth");
    assert_eq!(selected, refreshed);
    assert_eq!(refresh_calls, 1);
}

#[test]
#[ignore = "uses CSWITCH_TEST_BINARY; run scripts/run-process-tests.py"]
fn keep_official_auth_preserves_chatgpt_tokens_and_only_changes_route() {
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
            ..Default::default()
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
        document["model_provider"].as_str(),
        Some(config::HTTP_ROUTE_PROVIDER_ID)
    );
    assert_eq!(
        document["model_providers"][config::HTTP_ROUTE_PROVIDER_ID]["supports_websockets"]
            .as_bool(),
        Some(false)
    );
    assert!(
        document["model_providers"][config::HTTP_ROUTE_PROVIDER_ID]["base_url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
    assert!(!config.contains("provider-key"));
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
        fs::read_to_string(codex_home.join("config.toml")).unwrap(),
        config,
        "hot switching must retain the existing address and configuration bytes"
    );
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
    gateway::stop(codex_home).unwrap();
}

#[test]
#[ignore = "uses CSWITCH_TEST_BINARY; run scripts/run-process-tests.py"]
fn disabling_official_route_restores_address_and_preserves_auth() {
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
            ..Default::default()
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
        fs::read(codex_home.join("auth.json")).unwrap(),
        official_auth
    );
    assert!(
        !fs::read_to_string(codex_home.join("config.toml"))
            .unwrap()
            .contains("openai_base_url")
    );
    let state = list_provider_state(codex_home).expect("provider state");
    assert!(!state.keep_official_auth);
    assert!(state.active_provider_id.is_none());
    assert!(state.official_active);
}

#[test]
fn regression_close_reloads_user_configuration() {
    let directory = tempdir().unwrap();
    let home = directory.path();
    fs::write(home.join("config.toml"), "model = 'before-close'\n").unwrap();
    let record = save_fixture_provider(
        home,
        "Fixture",
        "http://localhost",
        "fixture-key",
        &["fixture"],
    );
    activate_provider_inner_with_close(home, &record.id, || {
        fs::write(home.join("config.toml"), "model = 'saved-on-close'\n").unwrap();
        Ok(true)
    })
    .unwrap();
    assert_eq!(
        parse_config(&fs::read_to_string(home.join("config.toml")).unwrap()).unwrap()["model"]
            .as_str(),
        Some("saved-on-close")
    );
}

#[test]
fn regression_failed_auth_mode_change_restores_setting() {
    let directory = tempdir().unwrap();
    let home = directory.path();
    let record = save_fixture_provider(
        home,
        "Fixture",
        "http://localhost",
        "fixture-key",
        &["fixture"],
    );
    activate_provider_inner_with_close(home, &record.id, || Ok(false)).unwrap();
    fs::create_dir(home.join("sessions")).unwrap();
    fs::write(home.join("sessions/rollout-broken.jsonl"), b"").unwrap();
    assert!(set_keep_official_auth_inner(home, true).is_err());
    assert!(!ProfileStore::new(home).keep_official_auth().unwrap());
}

#[test]
fn regression_direct_auth_keeps_key_out_of_config() {
    let directory = tempdir().unwrap();
    let home = directory.path();
    let record = save_fixture_provider(
        home,
        "Fixture",
        "http://localhost",
        "fixture-key",
        &["fixture"],
    );
    activate_provider_inner_with_close(home, &record.id, || Ok(false)).unwrap();
    let config = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(!config.contains("experimental_bearer_token"));
    assert_eq!(
        api_key_from_auth(&fs::read(home.join("auth.json")).unwrap())
            .unwrap()
            .as_deref(),
        Some("fixture-key")
    );
}

#[test]
fn regression_preserves_user_catalog_pointer() {
    let config = build_provider_config(
        "model_catalog_json = '/user/models.json'\n",
        "Fixture",
        "http://localhost",
        "fixture-key",
    )
    .unwrap();
    assert_eq!(
        parse_config(&config).unwrap()["model_catalog_json"].as_str(),
        Some("/user/models.json")
    );
}

#[test]
fn cleanup_failure_keeps_provider_list_and_reports_warning() {
    let directory = tempdir().unwrap();
    let home = directory.path();
    let record = save_fixture_provider(
        home,
        "Fixture",
        "http://localhost",
        "fixture-key",
        &["fixture"],
    );
    fs::create_dir(home.join("cswitch-backups")).unwrap();
    fs::create_dir(home.join("cswitch-backups/broken")).unwrap();
    fs::write(home.join("cswitch-backups/broken/config.toml"), b"[invalid").unwrap();
    let state = list_provider_state(home).unwrap();
    assert_eq!(state.providers.len(), 1);
    assert_eq!(state.providers[0].id, record.id);
    assert_eq!(state.warnings.len(), 1);
    assert!(state.warnings[0].contains("config.toml"));
}

#[test]
#[ignore = "uses CSWITCH_TEST_BINARY; run scripts/run-process-tests.py"]
#[allow(clippy::result_large_err)] // tungstenite handshake callback requires an HTTP error response by value.
fn official_route_migrates_history_and_keeps_legacy_websocket_clients_working() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::thread;
    use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};
    let home = tempdir().unwrap();
    let root = home.path();
    let auth = official_auth(chrono::Utc::now().timestamp() + 3600, "fixture-refresh");
    let value: Value = serde_json::from_slice(&auth).unwrap();
    let official_token = value["tokens"]["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(root.join("auth.json"), &auth).unwrap();
    fs::write(
        root.join("config.toml"),
        b"model = 'unchanged-model'\nopenai_base_url = 'https://original.example/v1'\n",
    )
    .unwrap();
    fs::create_dir(root.join("sessions")).unwrap();
    fs::write(
        root.join("sessions/rollout-fixture.jsonl"),
        b"{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\"}}\n",
    )
    .unwrap();
    let db = rusqlite::Connection::open(root.join("state_5.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY, model_provider TEXT); INSERT INTO threads VALUES ('fixture', 'openai');").unwrap();
    drop(db);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let provider = save_fixture_provider(
        root,
        "fixture",
        &api_url,
        "upstream-fixture-key",
        &["model"],
    );
    let server = thread::spawn(move || {
        for expected in ["/responses", "/responses/compact"] {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(8)))
                .unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with(&format!("POST {expected} ")));
            let mut headers = String::new();
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse::<usize>().unwrap();
                }
                headers.push_str(&line);
            }
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer upstream-fixture-key")
            );
            assert!(!headers.to_ascii_lowercase().contains("chatgpt-account-id"));
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            assert_eq!(
                body,
                br#"{"model":"do-not-change","input":"fixture","stream":true}"#
            );
            let payload = "data: {\"type\":\"response.completed\",\"fixture\":true}\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).as_bytes()).unwrap();
        }
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(8)))
            .unwrap();
        let mut socket = tungstenite::accept_hdr(
            socket,
            |request: &tungstenite::handshake::server::Request, response| {
                assert_eq!(request.uri().path(), "/responses");
                assert_eq!(
                    request.headers()["authorization"],
                    "Bearer upstream-fixture-key"
                );
                assert!(request.headers().get("ChatGPT-Account-Id").is_none());
                Ok(response)
            },
        )
        .unwrap();
        let message = socket.read().unwrap();
        assert_eq!(message.to_text().unwrap(), "websocket-fixture");
        socket.send(message).unwrap();
        drop(socket);
        let (mut denied, _) = listener.accept().unwrap();
        // An HTTP-only provider must retain its original handshake status for the client.
        let mut bytes = [0u8; 4096];
        let _ = denied.read(&mut bytes).unwrap();
        denied.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let state = set_keep_official_auth_inner(root, true).unwrap();
    assert_eq!(
        state.active_provider_id.as_deref(),
        Some(provider.id.as_str())
    );
    assert!(!state.official_active);
    let routed = fs::read_to_string(root.join("config.toml")).unwrap();
    assert_eq!(
        config::selected_provider(&routed).unwrap(),
        config::HTTP_ROUTE_PROVIDER_ID
    );
    let url = config::provider_base_url(&routed, config::HTTP_ROUTE_PROVIDER_ID)
        .unwrap()
        .unwrap();
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    assert_eq!(
        client
            .post(format!("{url}/responses"))
            .bearer_auth("wrong-token")
            .body("{}")
            .send()
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    for suffix in ["/responses", "/responses/compact"] {
        let response = client
            .post(format!("{url}{suffix}"))
            .bearer_auth(&official_token)
            .header("ChatGPT-Account-Id", "must-not-leak")
            .header("Content-Type", "application/json")
            .body(r#"{"model":"do-not-change","input":"fixture","stream":true}"#)
            .send()
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert!(response.text().unwrap().contains("response.completed"));
    }
    let mut request = format!("{url}/responses")
        .replacen("http:", "ws:", 1)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {official_token}").parse().unwrap(),
    );
    request
        .headers_mut()
        .insert("ChatGPT-Account-Id", "must-not-leak".parse().unwrap());
    let (mut websocket, _) = tungstenite::connect(request).unwrap();
    websocket
        .send(Message::Text("websocket-fixture".into()))
        .unwrap();
    assert_eq!(
        websocket.read().unwrap().to_text().unwrap(),
        "websocket-fixture"
    );
    drop(websocket);
    let mut denied_request = format!("{url}/responses")
        .replacen("http:", "ws:", 1)
        .into_client_request()
        .unwrap();
    denied_request.headers_mut().insert(
        "Authorization",
        format!("Bearer {official_token}").parse().unwrap(),
    );
    assert!(
        matches!(tungstenite::connect(denied_request), Err(tungstenite::Error::Http(response)) if response.status().as_u16()==405)
    );
    server.join().unwrap();
    fs::write(
        root.join("config.toml"),
        format!("model_verbosity = 'high'\n{routed}"),
    )
    .unwrap();
    set_keep_official_auth_inner(root, false).unwrap();
    let restored = fs::read_to_string(root.join("config.toml")).unwrap();
    assert_eq!(
        config::provider_base_url(&restored, "openai")
            .unwrap()
            .as_deref(),
        Some("https://original.example/v1")
    );
    assert!(restored.contains("model_verbosity = 'high'"));
    assert_eq!(fs::read(root.join("auth.json")).unwrap(), auth);
    let db = rusqlite::Connection::open(root.join("state_5.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT model_provider FROM threads", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "openai"
    );
    assert!(
        fs::read_to_string(root.join("sessions/rollout-fixture.jsonl"))
            .unwrap()
            .contains("\"model_provider\":\"openai\"")
    );
}

#[test]
#[ignore = "uses CSWITCH_TEST_BINARY; run scripts/run-process-tests.py"]
fn custom_mode_route_keeps_provider_and_restores_api_auth_without_history_scan() {
    let home = tempdir().unwrap();
    let root = home.path();
    let official = official_auth(chrono::Utc::now().timestamp() + 3600, "fixture-refresh");
    fs::write(root.join("auth.json"), &official).unwrap();
    fs::write(root.join("config.toml"), b"model='original'\n").unwrap();
    let provider = save_fixture_provider(
        root,
        "fixture",
        "https://fixture.test",
        "fixture-key",
        &["model"],
    );
    activate_provider_inner_with_close(root, &provider.id, || Ok(false)).unwrap();
    let before = fs::read(root.join("config.toml")).unwrap();
    fs::write(root.join("state_5.sqlite"), b"do-not-scan").unwrap();
    set_keep_official_auth_inner(root, true).unwrap();
    let routed = fs::read_to_string(root.join("config.toml")).unwrap();
    assert_eq!(config::selected_provider(&routed).unwrap(), "custom");
    assert_eq!(fs::read(root.join("auth.json")).unwrap(), official);
    set_keep_official_auth_inner(root, false).unwrap();
    assert_eq!(fs::read(root.join("config.toml")).unwrap(), before);
    assert_eq!(
        api_key_from_auth(&fs::read(root.join("auth.json")).unwrap())
            .unwrap()
            .as_deref(),
        Some("fixture-key")
    );
    assert_eq!(
        fs::read(root.join("state_5.sqlite")).unwrap(),
        b"do-not-scan"
    );
}

#[test]
fn disabling_legacy_keep_auth_uses_small_transaction_and_retains_latest_official_tokens() {
    let home = tempdir().unwrap();
    let root = home.path();
    let auth = official_auth(chrono::Utc::now().timestamp() + 3600, "latest-refresh");
    let record = save_fixture_provider(
        root,
        "fixture",
        "https://fixture.test",
        "fixture-key",
        &["model"],
    );
    let profiles = ProfileStore::new(root);
    let config = String::from_utf8(profiles.load_provider(&record.id).unwrap().config).unwrap();
    fs::write(
        root.join("config.toml"),
        format!("{config}\nexperimental_bearer_token = 'fixture-key'\n"),
    )
    .unwrap();
    fs::write(root.join("auth.json"), &auth).unwrap();
    fs::write(root.join("state_5.sqlite"), b"must-not-open").unwrap();
    profiles
        .save_settings(&AppSettings {
            keep_official_auth: true,
            ..Default::default()
        })
        .unwrap();
    set_keep_official_auth_inner(root, false).unwrap();
    assert_eq!(profiles.load_official().unwrap().unwrap().auth, auth);
    assert_eq!(
        api_key_from_auth(&fs::read(root.join("auth.json")).unwrap())
            .unwrap()
            .as_deref(),
        Some("fixture-key")
    );
    assert!(
        !fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .contains("experimental_bearer_token")
    );
    assert_eq!(
        fs::read(root.join("state_5.sqlite")).unwrap(),
        b"must-not-open"
    );
}

#[test]
fn disabling_custom_route_saves_rotated_official_credentials() {
    let home = tempdir().unwrap();
    let root = home.path();
    let provider = save_fixture_provider(
        root,
        "fixture",
        "https://fixture.test",
        "fixture-key",
        &["model"],
    );
    let store = ProfileStore::new(root);
    let stale = official_auth(chrono::Utc::now().timestamp() + 3600, "old-refresh");
    let fresh = official_auth(chrono::Utc::now().timestamp() + 7200, "rotated-refresh");
    store.save_official(b"model='original'\n", &stale).unwrap();
    let source = String::from_utf8(store.load_provider(&provider.id).unwrap().config).unwrap();
    let routed =
        config::with_provider_base_url(&source, "custom", Some("http://127.0.0.1:1234/v1"))
            .unwrap();
    fs::write(root.join("config.toml"), &routed).unwrap();
    fs::write(root.join("auth.json"), &fresh).unwrap();
    store
        .save_settings(&AppSettings {
            keep_official_auth: true,
            official_route: Some(profiles::OfficialRoute {
                resident: false,
                http_transport: None,
                direct_provider_id: None,
                provider_id: provider.id,
                config_provider: "custom".into(),
                previous_base_url: Some("https://fixture.test".into()),
                local_base_url: "http://127.0.0.1:1234/v1".into(),
            }),
        })
        .unwrap();
    stop_official_route(root, false).unwrap();
    assert_eq!(
        store.load_official().unwrap().unwrap().auth,
        fresh,
        "latest refresh token lost when returning to API key mode"
    );
    assert_eq!(
        api_key_from_auth(&fs::read(root.join("auth.json")).unwrap())
            .unwrap()
            .as_deref(),
        Some("fixture-key")
    );
}

#[test]
fn auth_transitions_stop_writer_and_read_its_final_rotation() {
    let home = tempdir().unwrap();
    let auth = home.path().join("auth.json");
    let initial = official_auth(chrono::Utc::now().timestamp() + 3600, "old");
    let latest = official_auth(chrono::Utc::now().timestamp() + 7200, "latest");
    fs::write(&auth, &initial).unwrap();
    assert_eq!(
        read_auth_for_transition(home.path(), None, || panic!(
            "address-only change must not close Codex"
        ))
        .unwrap(),
        Some(initial.clone())
    );
    let api = build_custom_auth("fixture-key").unwrap();
    assert_eq!(
        read_auth_for_transition(home.path(), Some(&api), || {
            fs::write(&auth, &latest)?;
            Ok(true)
        })
        .unwrap(),
        Some(latest.clone())
    );
    assert_eq!(fs::read(&auth).unwrap(), latest);
    fs::write(&auth, &api).unwrap();
    assert!(
        read_auth_for_transition(home.path(), None, || Err("fixture access denied".into()))
            .is_err()
    );
    assert_eq!(fs::read(&auth).unwrap(), api);
}

#[test]
fn official_snapshot_never_restores_a_retired_local_proxy() {
    let home = tempdir().unwrap();
    let store = ProfileStore::new(home.path());
    store
        .save_settings(&AppSettings {
            keep_official_auth: true,
            official_route: Some(profiles::OfficialRoute {
                resident: false,
                http_transport: None,
                direct_provider_id: None,
                provider_id: "fixture".into(),
                config_provider: "openai".into(),
                previous_base_url: Some("https://original.test/v1".into()),
                local_base_url: "http://127.0.0.1:1234/v1".into(),
            }),
        })
        .unwrap();
    let original = "openai_base_url = 'http://127.0.0.1:1234/v1'\nmodel='unchanged'\n";
    let snapshot = official_snapshot_config(original, &store).unwrap();
    assert_eq!(
        config::provider_base_url(&snapshot, "openai")
            .unwrap()
            .as_deref(),
        Some("https://original.test/v1")
    );
    assert!(snapshot.contains("model='unchanged'"));
    let external = "openai_base_url = 'https://user-edited.test'\n";
    assert_eq!(
        official_snapshot_config(external, &store).unwrap(),
        external
    );
}

#[test]
fn http_route_restores_original_transport_and_preserves_user_edits() {
    for (source, provider) in [
        (
            "# original\nmodel='original'\n[desktop]\nlocaleOverride='en-US'\n",
            "openai",
        ),
        (
            "model_provider = 'openai' # explicit\nmodel='original'\nopenai_base_url='https://official.test/api'\n[model_providers.other]\nname='keep'\nsupports_websockets=true\n",
            "openai",
        ),
        (
            "model_provider='custom'\nmodel='original'\n[model_providers.custom]\nname='Existing'\nbase_url='https://third.test'\nsupports_websockets=true\n",
            "custom",
        ),
        (
            "model_provider='custom'\nmodel='original'\n[model_providers.custom]\nname='Existing'\nbase_url='https://third.test'\nsupports_websockets=false\n",
            "custom",
        ),
        (
            "model_provider='custom'\nmodel='original'\n[model_providers.custom]\nname='Existing'\nbase_url='https://third.test'\n",
            "custom",
        ),
    ] {
        let route = profiles::OfficialRoute {
            provider_id: "fixture".into(),
            config_provider: provider.into(),
            previous_base_url: config::provider_base_url(source, provider).unwrap(),
            local_base_url: "http://127.0.0.1:1234/v1".into(),
            resident: true,
            direct_provider_id: None,
            http_transport: Some(config::capture_route_transport(source, provider).unwrap()),
        };
        let updated = config::with_http_route(source, provider, &route.local_base_url).unwrap();
        verify_route_config(&updated, &route).unwrap();
        let edited = updated.replace("model='original'", "model='user-edited'");
        let restored = config::restore_route_config(&edited, &route).unwrap();
        assert!(restored.contains("model='user-edited'"));
        assert_eq!(config::selected_provider(&restored).unwrap(), provider);
        assert_eq!(
            config::provider_base_url(&restored, provider).unwrap(),
            route.previous_base_url
        );
        assert_eq!(
            config::provider_websockets(&restored, provider).unwrap(),
            route.http_transport.as_ref().unwrap().previous_websockets
        );
        assert!(!restored.contains(config::HTTP_ROUTE_PROVIDER_ID));
        if source.contains("[desktop]") {
            assert!(restored.contains("[desktop]\nlocaleOverride='en-US'"));
        }
        if source.contains("[model_providers.other]") {
            assert!(
                restored.contains("[model_providers.other]\nname='keep'\nsupports_websockets=true")
            );
            assert!(restored.contains("'openai' # explicit"));
        }
        let conflicted =
            config::with_provider_websockets(&updated, route.active_config_provider(), Some(true))
                .unwrap();
        assert!(verify_route_config(&conflicted, &route).is_err());
        let home = tempdir().unwrap();
        let store = ProfileStore::new(home.path());
        store
            .save_settings(&AppSettings {
                keep_official_auth: true,
                official_route: Some(route),
            })
            .unwrap();
        if provider == "openai" {
            assert_eq!(official_snapshot_config(&edited, &store).unwrap(), restored);
        }
    }
    assert!(
        config::capture_route_transport(
            "[model_providers.cswitch_local]\nname='user-owned'\n",
            "openai"
        )
        .is_err()
    );
}

#[test]
fn official_selection_does_not_hide_network_errors_by_trying_old_credentials() {
    let auth = official_auth(chrono::Utc::now().timestamp() + 3600, "live");
    let saved = official_auth(chrono::Utc::now().timestamp() + 3600, "saved");
    let mut calls = 0;
    let result = select_official_auth_with(
        &[auth, saved],
        |_| {
            calls += 1;
            Err("fixture network failure".into())
        },
        |_| panic!("refresh not expected"),
    );
    assert!(result.is_err());
    assert_eq!(calls, 1);
}

#[test]
fn imports_named_provider_even_after_registry_exists() {
    let home = tempdir().unwrap();
    let store = ProfileStore::new(home.path());
    store.ensure_provider_registry().unwrap();
    let config = b"model_provider = 'my-provider'\n[model_providers.my-provider]\nname = 'Imported'\nbase_url = 'https://import.example/v1'\nrequires_openai_auth = true\n";
    let auth = build_custom_auth("import-key").unwrap();
    fs::write(home.path().join("config.toml"), config).unwrap();
    fs::write(home.path().join("auth.json"), &auth).unwrap();
    ensure_provider_migration(home.path()).unwrap();
    let records = store.list_providers().unwrap();
    assert_eq!(
        records.len(),
        1,
        "selected named provider must be discovered"
    );
    assert_eq!(records[0].name, "Imported");
    assert_eq!(
        detect_active_provider_id(home.path(), &store, &records).unwrap(),
        Some(records[0].id.clone())
    );
    ensure_provider_migration(home.path()).unwrap();
    assert_eq!(store.list_providers().unwrap().len(), 1);
    assert_eq!(fs::read(home.path().join("config.toml")).unwrap(), config);
    assert_eq!(fs::read(home.path().join("auth.json")).unwrap(), auth);
}
