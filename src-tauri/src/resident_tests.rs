use super::*;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tiny_http::{Header, Response, Server};

#[test]
#[ignore = "uses CSWITCH_TEST_BINARY; isolated HTTP route upgrade and rollback"]
fn legacy_resident_route_upgrades_once_and_failed_migration_preserves_state() {
    let dir = tempdir().unwrap();
    let home = dir.path();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let base = gateway::local_base_url(port);
    let original = format!("model='unchanged'\nopenai_base_url='{base}'\n");
    let auth = official_auth(chrono::Utc::now().timestamp() + 3600, "fixture-refresh");
    fs::write(home.join("config.toml"), &original).unwrap();
    fs::write(home.join("auth.json"), &auth).unwrap();
    fs::create_dir(home.join("sessions")).unwrap();
    let rollout = home.join("sessions/rollout-fixture.jsonl");
    let original_history = b"{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\"}}\n{\"type\":\"fixture_body\"}\n";
    fs::write(&rollout, original_history).unwrap();
    let profiles = ProfileStore::new(home);
    let legacy: AppSettings = serde_json::from_value(json!({
        "keepOfficialAuth":true,
        "officialRoute": {
            "providerId":"openai", "configProvider":"openai",
            "previousBaseUrl":null, "localBaseUrl":base, "resident":true,
        }
    }))
    .unwrap();
    profiles.save_settings(&legacy).unwrap();
    assert!(
        activate_official_route_with_close(home, "openai", &ProgressReporter::default(), || Err(
            "fixture close denied".into()
        ))
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        original
    );
    assert_eq!(profiles.load_settings().unwrap(), legacy);
    assert_eq!(fs::read(&rollout).unwrap(), original_history);

    // Cancellation at the commit boundary restores already migrated history and settings.
    let login = oauth::begin_login().unwrap();
    oauth::cancel_login();
    assert!(
        provider_sync::apply_route_state(
            home,
            Some(original.as_bytes()),
            b"model='cancelled'\n",
            Some(config::HTTP_ROUTE_PROVIDER_ID),
            AuthUpdate::Keep,
            b"{}"
        )
        .is_err()
    );
    drop(login);
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        original
    );
    assert_eq!(profiles.load_settings().unwrap(), legacy);
    assert_eq!(fs::read(&rollout).unwrap(), original_history);

    let report =
        activate_official_route_with_close(home, "openai", &ProgressReporter::default(), || {
            Ok(true)
        })
        .unwrap();
    assert_eq!(report.rollout_files_updated, 1);
    let routed = fs::read_to_string(home.join("config.toml")).unwrap();
    let route = profiles.load_settings().unwrap().official_route.unwrap();
    assert_eq!(route.local_base_url, base);
    verify_route_config(&routed, &route).unwrap();
    let history = fs::read(&rollout).unwrap();
    let report =
        activate_official_route_with_close(home, "openai", &ProgressReporter::default(), || {
            panic!("hot switch must not close Codex")
        })
        .unwrap();
    assert!(report.backup_path.is_empty());
    assert_eq!(
        fs::read_to_string(home.join("config.toml")).unwrap(),
        routed
    );
    assert_eq!(fs::read(&rollout).unwrap(), history);
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth);
    stop_official_route_with_close(home, false, || Ok(true)).unwrap();
    let restored = fs::read_to_string(home.join("config.toml")).unwrap();
    assert_eq!(config::selected_provider(&restored).unwrap(), "openai");
    assert!(
        config::provider_base_url(&restored, "openai")
            .unwrap()
            .is_none()
    );
    let restored_history = fs::read_to_string(&rollout).unwrap();
    let original_history = std::str::from_utf8(original_history).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(restored_history.lines().next().unwrap()).unwrap(),
        serde_json::from_str::<Value>(original_history.lines().next().unwrap()).unwrap(),
    );
    assert_eq!(
        restored_history.split_once('\n').unwrap().1,
        original_history.split_once('\n').unwrap().1
    );
}

fn fixture_request(server: &Server, key: &str, account: bool) -> tiny_http::Request {
    let request = server
        .recv_timeout(Duration::from_secs(15))
        .unwrap()
        .expect("fixture request");
    assert_eq!(
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .unwrap()
            .value
            .as_str(),
        format!("Bearer {key}")
    );
    assert_eq!(
        request
            .headers()
            .iter()
            .any(|h| h.field.equiv("ChatGPT-Account-Id")),
        account
    );
    request
}

#[test]
#[ignore = "uses CSWITCH_TEST_BINARY; isolated resident route integration"]
fn resident_switches_http_without_touching_auth_config_or_inflight_requests() {
    let dir = tempdir().unwrap();
    let home = dir.path();
    let official_server = Server::http("127.0.0.1:0").unwrap();
    let a = Server::http("127.0.0.1:0").unwrap();
    let b = Server::http("127.0.0.1:0").unwrap();
    let auth = official_auth(chrono::Utc::now().timestamp() + 3600, "fixture-rt");
    let token = serde_json::from_slice::<Value>(&auth).unwrap()["tokens"]["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    let original = format!(
        "# keep formatting\nmodel='original'\nmodel_reasoning_effort='high'\nopenai_base_url='http://{}/official'\n",
        official_server.server_addr()
    );
    fs::write(home.join("config.toml"), &original).unwrap();
    fs::write(home.join("auth.json"), &auth).unwrap();
    let db = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY, model_provider TEXT); INSERT INTO threads VALUES ('fixture', 'openai');").unwrap();
    drop(db);
    fs::create_dir(home.join("sessions")).unwrap();
    fs::write(
        home.join("sessions/rollout-fixture.jsonl"),
        b"{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"openai\"}}\n{\"type\":\"fixture_body\"}\n",
    )
    .unwrap();
    let pa = save_fixture_provider(
        home,
        "A",
        &format!("http://{}/a", a.server_addr()),
        "key-a",
        &["model-a"],
    );
    let pb = save_fixture_provider(
        home,
        "B",
        &format!("http://{}/b", b.server_addr()),
        "key-b",
        &["model-b"],
    );
    let profiles = ProfileStore::new(home);
    profiles
        .save_settings(&AppSettings {
            keep_official_auth: true,
            ..Default::default()
        })
        .unwrap();
    activate_provider_inner_with_close(home, &pa.id, || Ok(true)).unwrap();
    let config_bytes = fs::read(home.join("config.toml")).unwrap();
    let config_text = String::from_utf8(config_bytes.clone()).unwrap();
    assert_eq!(
        config::selected_provider(&config_text).unwrap(),
        config::HTTP_ROUTE_PROVIDER_ID
    );
    assert_eq!(
        config::provider_websockets(&config_text, config::HTTP_ROUTE_PROVIDER_ID).unwrap(),
        Some(false)
    );
    let history_bytes = fs::read(home.join("sessions/rollout-fixture.jsonl")).unwrap();
    let db_bytes = fs::read(home.join("state_5.sqlite")).unwrap();
    assert!(String::from_utf8_lossy(&history_bytes).contains(config::HTTP_ROUTE_PROVIDER_ID));
    let base = config::provider_base_url(&config_text, config::HTTP_ROUTE_PROVIDER_ID)
        .unwrap()
        .unwrap();
    let port = gateway::route_port(&base).unwrap();
    let backups = fs::read_dir(home.join("cswitch-backups")).unwrap().count();
    let (received_tx, received_rx) = mpsc::channel();
    let (complete_tx, complete_rx) = mpsc::channel();
    let upstream_a = thread::spawn(move || {
        let mut request = fixture_request(&a, "key-a", false);
        assert!(
            !request
                .headers()
                .iter()
                .any(|h| h.field.equiv("Content-Encoding"))
        );
        let mut body = String::new();
        request.as_reader().read_to_string(&mut body).unwrap();
        assert_eq!(
            body,
            r#"{"model":"unchanged","reasoning":{"effort":"high"},"input":"original"}"#
        );
        received_tx.send(()).unwrap();
        complete_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        request
            .respond(
                Response::from_string(
                    "data: {\"type\":\"response.completed\",\"route\":\"a\"}\n\n",
                )
                .with_header(Header::from_bytes("Content-Type", "text/event-stream").unwrap()),
            )
            .unwrap();
    });
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let pending_client = client.clone();
    let pending_base = base.clone();
    let pending_token = token.clone();
    let inflight = thread::spawn(move || {
        pending_client
            .post(format!("{pending_base}/responses"))
            .bearer_auth(pending_token)
            .header("ChatGPT-Account-Id", "fixture-account")
            .header("Content-Encoding", "zstd")
            .body(
                zstd::stream::encode_all(
                    r#"{"model":"unchanged","reasoning":{"effort":"high"},"input":"original"}"#
                        .as_bytes(),
                    0,
                )
                .unwrap(),
            )
            .send()
            .unwrap()
            .text()
            .unwrap()
    });
    received_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    activate_provider_inner_with_close(home, &pb.id, || panic!("hot switch closed Codex")).unwrap();
    // Start each fixture receive deadline only in the phase that sends its request.
    // The listener stays alive across switches, without an idle receiver timing out.
    let upstream_b = thread::spawn(move || {
        fixture_request(&b, "key-b", false)
            .respond(Response::from_string("b"))
            .unwrap();
        b
    });
    assert_eq!(
        client
            .post(format!("{base}/responses"))
            .bearer_auth(&token)
            .header("ChatGPT-Account-Id", "fixture-account")
            .body("{}")
            .send()
            .unwrap()
            .text()
            .unwrap(),
        "b"
    );
    let b = upstream_b.join().unwrap();
    complete_tx.send(()).unwrap();
    assert!(inflight.join().unwrap().contains("\"route\":\"a\""));
    for i in 0..20 {
        activate_provider_inner_with_close(home, if i % 2 == 0 { &pa.id } else { &pb.id }, || {
            panic!("hot switch closed Codex")
        })
        .unwrap();
    }
    // Deleting an inactive card must not stop the shared listener originally started for A.
    delete_provider_inner(home, &pa.id).unwrap();
    let login = oauth::begin_login().unwrap();
    switch_to_official_with_progress(home, &ProgressReporter::default()).unwrap();
    drop(login);
    let state = list_provider_state_read_only(home).unwrap();
    assert!(state.official_active);
    assert!(state.active_provider_id.is_none());
    let official_token = token.clone();
    let upstream_official = thread::spawn(move || {
        let req = fixture_request(&official_server, &official_token, true);
        assert_eq!(req.url(), "/official/models?client_version=fixture");
        req.respond(Response::from_string(
            r#"{"models":[{"slug":"original","context_window":12345}]}"#,
        ))
        .unwrap();
        fixture_request(&official_server, &official_token, true)
            .respond(Response::from_string("official-401").with_status_code(401))
            .unwrap();
    });
    let response = client
        .get(format!("{base}/models?client_version=fixture"))
        .bearer_auth(&token)
        .header("ChatGPT-Account-Id", "fixture-account")
        .send()
        .unwrap();
    assert_eq!(
        response.text().unwrap(),
        r#"{"models":[{"slug":"original","context_window":12345}]}"#
    );
    let response = client
        .post(format!("{base}/responses"))
        .bearer_auth(&token)
        .header("ChatGPT-Account-Id", "fixture-account")
        .body("{}")
        .send()
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(response.text().unwrap(), "official-401");
    activate_provider_inner_with_close(home, &pb.id, || panic!("hot switch closed Codex")).unwrap();
    gateway::stop(home).unwrap();
    let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    assert!(
        gateway::restore_resident(home).is_err(),
        "port conflicts must be reported"
    );
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), config_bytes);
    drop(occupied);
    gateway::restore_resident(home).unwrap();
    let upstream_b = thread::spawn(move || {
        fixture_request(&b, "key-b", false)
            .respond(Response::from_string("b"))
            .unwrap();
    });
    assert_eq!(
        client
            .post(format!("{base}/responses/compact"))
            .bearer_auth(&token)
            .body("{}")
            .send()
            .unwrap()
            .text()
            .unwrap(),
        "b"
    );
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth);
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), config_bytes);
    assert_eq!(fs::read(home.join("state_5.sqlite")).unwrap(), db_bytes);
    assert_eq!(
        fs::read(home.join("sessions/rollout-fixture.jsonl")).unwrap(),
        history_bytes
    );
    assert_eq!(
        fs::read_dir(home.join("cswitch-backups")).unwrap().count(),
        backups
    );
    let settings = fs::read(home.join("cswitch-profiles/settings.json")).unwrap();
    fs::remove_file(home.join("auth.json")).unwrap();
    assert!(
        activate_provider_inner_with_close(home, &pb.id, || panic!(
            "logout must not trigger auth rewrite"
        ))
        .is_err()
    );
    assert_eq!(
        fs::read(home.join("cswitch-profiles/settings.json")).unwrap(),
        settings
    );
    fs::write(home.join("auth.json"), &auth).unwrap();
    stop_official_route_with_close(home, false, || Ok(true)).unwrap();
    let restored = fs::read_to_string(home.join("config.toml")).unwrap();
    assert_eq!(
        config::provider_base_url(&restored, "openai").unwrap(),
        config::provider_base_url(&original, "openai").unwrap()
    );
    assert!(
        restored.contains("# keep formatting\nmodel='original'\nmodel_reasoning_effort='high'")
    );
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth);
    assert_eq!(config::selected_provider(&restored).unwrap(), "openai");
    assert!(!restored.contains("cswitch_local"));
    let db = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
    assert_eq!(
        db.query_row("SELECT model_provider FROM threads", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "openai"
    );
    assert!(
        fs::read_to_string(home.join("sessions/rollout-fixture.jsonl"))
            .unwrap()
            .contains("\"model_provider\":\"openai\"")
    );
    upstream_a.join().unwrap();
    upstream_b.join().unwrap();
    upstream_official.join().unwrap();
}

#[test]
#[ignore = "uses CSWITCH_TEST_BINARY; isolated resident WebSocket integration"]
#[allow(clippy::result_large_err)] // Tungstenite fixes the handshake callback's response error type.
fn resident_switches_websocket_at_request_boundary_and_rejects_cross_provider_response_ids() {
    use std::net::TcpListener;
    use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};
    let dir = tempdir().unwrap();
    let home = dir.path();
    let a = TcpListener::bind("127.0.0.1:0").unwrap();
    let b = TcpListener::bind("127.0.0.1:0").unwrap();
    let auth = official_auth(chrono::Utc::now().timestamp() + 3600, "fixture-rt");
    let token = serde_json::from_slice::<Value>(&auth).unwrap()["tokens"]["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(home.join("auth.json"), auth).unwrap();
    fs::write(home.join("config.toml"), "model='fixture'\n").unwrap();
    let pa = save_fixture_provider(
        home,
        "A",
        &format!("http://{}", a.local_addr().unwrap()),
        "a-key",
        &["model"],
    );
    let pb = save_fixture_provider(
        home,
        "B",
        &format!("http://{}", b.local_addr().unwrap()),
        "b-key",
        &["model"],
    );
    let server_a = thread::spawn(move || {
        let (stream, _) = a.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut ws = tungstenite::accept_hdr(
            stream,
            |req: &tungstenite::handshake::server::Request, res| {
                assert_eq!(req.headers()["authorization"], "Bearer a-key");
                assert!(req.headers().get("chatgpt-account-id").is_none());
                Ok(res)
            },
        )
        .unwrap();
        let first: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(first["input"], "first");
        ws.send(Message::Text(
            r#"{"type":"response.completed","response":{"id":"from-a"}}"#.into(),
        ))
        .unwrap();
        // Keep the original upstream socket open until the relay changes its destination.
        let _ = ws.read();
    });
    let server_b = thread::spawn(move || {
        let (stream, _) = b.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut ws = tungstenite::accept_hdr(
            stream,
            |req: &tungstenite::handshake::server::Request, res| {
                assert_eq!(req.headers()["authorization"], "Bearer b-key");
                assert!(req.headers().get("chatgpt-account-id").is_none());
                Ok(res)
            },
        )
        .unwrap();
        let second: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(second["input"], "full-context");
        assert!(second.get("previous_response_id").is_none());
        ws.send(Message::Text(
            r#"{"type":"response.completed","response":{"id":"from-b"}}"#.into(),
        ))
        .unwrap();
        ws.read().unwrap();
        ws.send(Message::Text(
            r#"{"type":"error","status":401,"error":{"message":"key rejected"}}"#.into(),
        ))
        .unwrap();
    });
    ProfileStore::new(home)
        .save_settings(&AppSettings {
            keep_official_auth: true,
            ..Default::default()
        })
        .unwrap();
    activate_provider_inner_with_close(home, &pa.id, || Ok(true)).unwrap();
    let source = fs::read_to_string(home.join("config.toml")).unwrap();
    let base = config::provider_base_url(&source, config::HTTP_ROUTE_PROVIDER_ID)
        .unwrap()
        .unwrap();
    let mut request = format!("{base}/responses")
        .replacen("http:", "ws:", 1)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    request
        .headers_mut()
        .insert("ChatGPT-Account-Id", "fixture-account".parse().unwrap());
    let (mut ws, _) = tungstenite::connect(request).unwrap();
    ws.send(Message::Text(
        r#"{"type":"response.create","model":"model","input":"first"}"#.into(),
    ))
    .unwrap();
    assert!(ws.read().unwrap().to_text().unwrap().contains("from-a"));
    activate_provider_inner_with_close(home, &pb.id, || panic!("closed Codex")).unwrap();
    ws.send(Message::Text(
        r#"{"type":"response.create","previous_response_id":"from-a","input":"incremental"}"#
            .into(),
    ))
    .unwrap();
    assert!(
        ws.read()
            .unwrap()
            .to_text()
            .unwrap()
            .contains("previous_response_not_found")
    );
    ws.send(Message::Text(
        r#"{"type":"response.create","model":"model","input":"full-context"}"#.into(),
    ))
    .unwrap();
    assert!(ws.read().unwrap().to_text().unwrap().contains("from-b"));
    ws.send(Message::Text(
        r#"{"type":"response.create","input":"third"}"#.into(),
    ))
    .unwrap();
    let error: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(error["status"], 502);
    gateway::stop(home).unwrap();
    server_a.join().unwrap();
    server_b.join().unwrap();
}
