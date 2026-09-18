use super::*;

fn probe_server(key: &'static str) -> (String, std::thread::JoinHandle<()>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", server.server_addr());
    let handle = std::thread::spawn(move || {
        for (path, body, status) in [
            (
                "/v1/responses",
                r#"{"error":{"message":"Missing required parameter: model","param":"model"}}"#,
                400,
            ),
            ("/v1/models", r#"{"data":[{"id":"fixture-model"}]}"#, 200),
        ] {
            let request = server
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .expect("request");
            assert_eq!(request.url(), path);
            assert_eq!(
                request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("authorization"))
                    .unwrap()
                    .value
                    .as_str(),
                format!("Bearer {key}")
            );
            request
                .respond(
                    tiny_http::Response::from_string(body)
                        .with_status_code(status)
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Type", "application/json")
                                .unwrap(),
                        ),
                )
                .unwrap();
        }
    });
    (base, handle)
}

fn write_config(home: &Path, id: &str, name: &str, url: &str, auth_fields: &str) -> String {
    let text = format!(
        "model_provider = '{id}'\nmodel = 'user-model'\nmodel_reasoning_effort = 'high'\n[model_providers.'{id}']\nname = '{name}'\nbase_url = '{url}'\n{auth_fields}\n[mcp_servers.fixture]\ncommand = 'user-command'\n"
    );
    fs::write(home.join("config.toml"), &text).unwrap();
    text
}

#[test]
fn imports_bearer_without_auth_file_and_preserves_every_live_byte() {
    let home = tempdir().unwrap();
    let config = write_config(
        home.path(),
        "other-name",
        "Imported",
        "https://import.test/v1",
        "experimental_bearer_token = 'fixture-key'",
    );
    let state = list_provider_state(home.path()).unwrap();
    assert_eq!(state.providers.len(), 1);
    assert!(state.providers[0].active);
    assert!(state.warnings.is_empty());
    assert!(!home.path().join("auth.json").exists());
    assert!(!home.path().join("cswitch-backups").exists());
    assert_eq!(
        fs::read_to_string(home.path().join("config.toml")).unwrap(),
        config
    );
    let store = ProfileStore::new(home.path());
    let profile = store.load_provider(&state.providers[0].id).unwrap();
    assert_eq!(profile.config, config.as_bytes());
    assert_eq!(
        api_key_from_auth(&profile.auth).unwrap().as_deref(),
        Some("fixture-key")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&profile.auth)
            .unwrap()
            .as_object()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn imports_header_credentials_and_switches_with_file_auth() {
    let home = tempdir().unwrap();
    let (base, server) = probe_server("fixture-key");
    write_config(
        home.path(),
        "other-name",
        "Imported",
        &base,
        "http_headers = { Authorization = 'Bearer fixture-key', 'X-Tenant' = 'tenant-a' }\nrequest_max_retries = 4",
    );
    let state = list_provider_state(home.path()).unwrap();
    let id = &state.providers[0].id;
    // Switching must use the imported provider table, not a stale unrelated custom table.
    fs::write(home.path().join("config.toml"), "model = 'new-user-model'\n[mcp_servers.new]\ncommand = 'new-user-command'\n[model_providers.custom]\nname = 'wrong'\nenv_key = 'WRONG_KEY'\nhttp_headers = { Authorization = 'Bearer wrong-key' }\n").unwrap();
    activate_provider_inner_with_close(home.path(), id, || Ok(false)).unwrap();
    server.join().unwrap();
    let text = fs::read_to_string(home.path().join("config.toml")).unwrap();
    let doc = parse_config(&text).unwrap();
    assert_eq!(doc["model"].as_str(), Some("new-user-model"));
    assert_eq!(
        doc["mcp_servers"]["new"]["command"].as_str(),
        Some("new-user-command")
    );
    let provider = &doc["model_providers"]["custom"];
    assert_eq!(provider["request_max_retries"].as_integer(), Some(4));
    assert_eq!(
        provider["http_headers"]["X-Tenant"].as_str(),
        Some("tenant-a")
    );
    assert!(provider.get("env_key").is_none());
    assert!(provider["http_headers"].get("Authorization").is_none());
    assert_eq!(provider["requires_openai_auth"].as_bool(), Some(true));
    assert_eq!(
        api_key_from_auth(&fs::read(home.path().join("auth.json")).unwrap())
            .unwrap()
            .as_deref(),
        Some("fixture-key")
    );
    assert_eq!(list_provider_state(home.path()).unwrap().providers.len(), 1);
}

#[test]
fn name_collisions_are_disambiguated_without_overwriting_existing_credentials() {
    let home = tempdir().unwrap();
    let existing = save_fixture_provider(
        home.path(),
        "Same Name",
        "https://old.test",
        "old-key",
        &["fixture-model"],
    );
    write_config(
        home.path(),
        "my-provider",
        "Same Name",
        "https://new.test",
        "experimental_bearer_token = 'new-key'",
    );
    for _ in 0..3 {
        ensure_provider_migration(home.path()).unwrap();
    }
    let state = list_provider_state(home.path()).unwrap();
    assert_eq!(state.providers.len(), 2);
    let new = state
        .providers
        .iter()
        .find(|p| p.id != existing.id)
        .unwrap();
    assert_eq!(new.name, "Same Name (my-provider)");
    assert!(new.active);
    assert_eq!(
        api_key_from_auth(
            &ProfileStore::new(home.path())
                .load_provider(&existing.id)
                .unwrap()
                .auth
        )
        .unwrap()
        .as_deref(),
        Some("old-key")
    );
}

#[test]
fn missing_credentials_warn_but_leave_the_form_and_later_import_available() {
    let home = tempdir().unwrap();
    let text = write_config(
        home.path(),
        "my-provider",
        "Missing Key",
        "https://fixture.test",
        "env_key = 'CSWITCH_NONEXISTENT_IMPORT_VARIABLE_718274'",
    );
    let auth = build_custom_auth("unrelated-file-key").unwrap();
    fs::write(home.path().join("auth.json"), &auth).unwrap();
    let state = list_provider_state(home.path()).unwrap();
    assert!(state.providers.is_empty());
    assert!(state.warnings.iter().any(|w| w.contains("API 配置")));
    assert!(
        !state
            .warnings
            .iter()
            .any(|w| w.contains("unrelated-file-key"))
    );
    assert_eq!(
        fs::read(home.path().join("config.toml")).unwrap(),
        text.as_bytes()
    );
    fs::write(
        home.path().join("config.toml"),
        text.replace(
            "env_key = 'CSWITCH_NONEXISTENT_IMPORT_VARIABLE_718274'",
            "experimental_bearer_token = 'now-available'",
        ),
    )
    .unwrap();
    assert_eq!(list_provider_state(home.path()).unwrap().providers.len(), 1);
    assert_eq!(fs::read(home.path().join("auth.json")).unwrap(), auth);
}

#[test]
fn active_detection_requires_a_real_matching_key() {
    let home = tempdir().unwrap();
    let record = save_fixture_provider(
        home.path(),
        "Same",
        "https://fixture.test",
        "saved-key",
        &["fixture-model"],
    );
    write_config(home.path(), "custom", "Same", "https://fixture.test", "");
    let store = ProfileStore::new(home.path());
    assert_eq!(
        detect_active_provider_id(home.path(), &store, &[record]).unwrap(),
        None
    );
}

#[test]
fn snapshot_never_replaces_provider_key_with_unrelated_live_auth() {
    let home = tempdir().unwrap();
    let record = save_fixture_provider(
        home.path(),
        "Imported",
        "https://fixture.test",
        "provider-key",
        &["fixture-model"],
    );
    let store = ProfileStore::new(home.path());
    fs::write(
        home.path().join("auth.json"),
        build_custom_auth("unrelated-key").unwrap(),
    )
    .unwrap();
    snapshot_provider_without_clobbering_key(&store, &record.id, b"# new settings").unwrap();
    assert_eq!(
        api_key_from_auth(&store.load_provider(&record.id).unwrap().auth)
            .unwrap()
            .as_deref(),
        Some("provider-key")
    );
}

#[test]
fn route_temporarily_replaces_and_restores_imported_auth_fields() {
    let source = "model_provider = 'my-provider'\nmodel = 'user-model'\n[model_providers.my-provider]\nname = 'Imported'\nbase_url = 'https://fixture.test'\nenv_key = 'FIXTURE_KEY'\nenv_key_instructions = 'custom instructions'\nrequires_openai_auth = false\nexperimental_bearer_token = 'old-bearer'\nhttp_headers = { authorization = 'Bearer old-header', 'X-Tenant' = 'tenant' }\nenv_http_headers = { Authorization = 'HEADER_ENV', 'X-User' = 'USER_ENV' }\nrequest_max_retries = 6\n";
    let route = profiles::OfficialRoute {
        provider_id: "fixture".into(),
        config_provider: "my-provider".into(),
        previous_base_url: Some("https://fixture.test".into()),
        local_base_url: "http://127.0.0.1:34567/v1".into(),
        resident: true,
        direct_provider_id: Some("fixture".into()),
        http_transport: Some(config::capture_route_transport(source, "my-provider").unwrap()),
    };
    let text = config::with_http_route(source, "my-provider", &route.local_base_url).unwrap();
    let doc = parse_config(&text).unwrap();
    let provider = &doc["model_providers"]["my-provider"];
    verify_route_config(&text, &route).unwrap();
    let tampered = text.replace(
        "requires_openai_auth = true",
        "requires_openai_auth = false",
    );
    assert!(verify_route_config(&tampered, &route).is_err());
    assert!(provider.get("env_key").is_none());
    assert!(provider.get("experimental_bearer_token").is_none());
    assert_eq!(provider["requires_openai_auth"].as_bool(), Some(true));
    assert_eq!(
        provider["http_headers"]["X-Tenant"].as_str(),
        Some("tenant")
    );
    // Edits outside managed auth fields while the route is active survive restoration.
    let text = text
        .replace("user-model", "edited-model")
        .replace("tenant'", "edited-tenant'");
    let restored = config::restore_route_config(&text, &route).unwrap();
    let expected = source
        .replace("user-model", "edited-model")
        .replace("tenant'", "edited-tenant'");
    assert_eq!(
        parse_config(&restored).unwrap().as_item().to_string(),
        parse_config(&expected).unwrap().as_item().to_string()
    );
}

#[test]
#[ignore = "uses CSWITCH_TEST_BINARY; fixed credential route and restore"]
fn imported_fixed_provider_routes_with_official_auth_and_restores_original_fields() {
    use tiny_http::{Response, Server};
    let server = Server::http("127.0.0.1:0").unwrap();
    let upstream_base = format!("http://{}/v1", server.server_addr());
    let home = tempdir().unwrap();
    let original = write_config(
        home.path(),
        "my-provider",
        "Imported Fixed Route",
        &upstream_base,
        "experimental_bearer_token = 'isolated-fixture-key'\nrequires_openai_auth = false",
    );
    let auth = official_auth(chrono::Utc::now().timestamp() + 3600, "fixture-refresh");
    fs::write(home.path().join("auth.json"), &auth).unwrap();
    let state = list_provider_state(home.path()).unwrap();
    let id = state.providers[0].id.clone();
    activate_official_route_with_close(home.path(), &id, &ProgressReporter::default(), || {
        Ok(false)
    })
    .unwrap();
    let profiles = ProfileStore::new(home.path());
    let route = profiles.load_settings().unwrap().official_route.unwrap();
    assert_eq!(route.direct_provider_id.as_deref(), Some(id.as_str()));
    let routed_config = fs::read_to_string(home.path().join("config.toml")).unwrap();
    let doc = parse_config(&routed_config).unwrap();
    assert!(
        doc["model_providers"]["my-provider"]
            .get("env_key")
            .is_none()
    );
    assert_eq!(
        doc["model_providers"]["my-provider"]["requires_openai_auth"].as_bool(),
        Some(true)
    );
    let service = std::thread::spawn(move || {
        let request = server
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .expect("routed request");
        assert_eq!(
            request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .unwrap()
                .value
                .as_str(),
            "Bearer isolated-fixture-key"
        );
        assert!(
            !request
                .headers()
                .iter()
                .any(|h| h.field.equiv("ChatGPT-Account-ID"))
        );
        request
            .respond(Response::from_string("{\"id\":\"fixture-response\"}"))
            .unwrap();
    });
    let token = serde_json::from_slice::<Value>(&auth).unwrap()["tokens"]["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    let response = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/responses", route.local_base_url))
        .bearer_auth(token)
        .header("ChatGPT-Account-ID", "fixture-account")
        .json(&json!({"model":"fixture-model","input":"fixture"}))
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    service.join().unwrap();
    assert_eq!(fs::read(home.path().join("auth.json")).unwrap(), auth);
    assert_eq!(list_provider_state(home.path()).unwrap().providers.len(), 1);
    stop_official_route_with_close(home.path(), false, || Ok(false)).unwrap();
    let restored = fs::read_to_string(home.path().join("config.toml")).unwrap();
    let restored = parse_config(&restored).unwrap();
    let original = parse_config(&original).unwrap();
    for field in ["model_provider", "model", "model_reasoning_effort"] {
        assert_eq!(restored[field].as_str(), original[field].as_str());
    }
    for field in ["name", "base_url", "experimental_bearer_token"] {
        assert_eq!(
            restored["model_providers"]["my-provider"][field].as_str(),
            original["model_providers"]["my-provider"][field].as_str()
        );
    }
    assert_eq!(
        restored["model_providers"]["my-provider"]["requires_openai_auth"].as_bool(),
        Some(false)
    );
    assert!(
        restored["model_providers"]["my-provider"]
            .get("supports_websockets")
            .is_none()
    );
    assert_eq!(
        restored["mcp_servers"]["fixture"]["command"].as_str(),
        Some("user-command")
    );
    assert_eq!(
        api_key_from_auth(&fs::read(home.path().join("auth.json")).unwrap())
            .unwrap()
            .as_deref(),
        Some("isolated-fixture-key")
    );
    assert_eq!(list_provider_state(home.path()).unwrap().providers.len(), 1);
}
