use super::*;
use crate::official_accounts::{Store, tests::auth};
use serde_json::{Value, json};
use tempfile::tempdir;

fn write_live(home: &Path, token: &[u8]) {
    fs::write(home.join("auth.json"), token).unwrap();
    fs::write(
        home.join("config.toml"),
        "model='unchanged'\nmodel_reasoning_effort='high'\n[features]\nfixture=true\n",
    )
    .unwrap();
}
fn validate_same(auth: &[u8]) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    Ok(Some(auth.to_vec()))
}

#[test]
fn adding_accounts_preserves_live_files_and_deduplicates_existing_account() {
    let home = tempdir().unwrap();
    let home = home.path();
    let a = auth("space-a", "user-a", "a@example.invalid", "a-old");
    write_live(home, &a);
    let original_config = fs::read(home.join("config.toml")).unwrap();
    let progress = ProgressReporter::default();
    add_official_account_with(home, &progress, || {
        Ok(auth("space-b", "user-b", "b@example.invalid", "b-one"))
    })
    .unwrap();
    add_official_account_with(home, &progress, || {
        Ok(auth("space-c", "user-c", "c@example.invalid", "c-one"))
    })
    .unwrap();
    add_official_account_with(home, &progress, || {
        Ok(auth("space-a", "user-a", "a@example.invalid", "a-new"))
    })
    .unwrap();
    sync_official_accounts(home).unwrap();
    let rows = Store::new(home).list().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .find(|a| a.identity.account == "space-a")
            .unwrap()
            .auth["tokens"]["refresh_token"],
        "a-new"
    );
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), a);
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), original_config);
    assert!(!home.join("cswitch-backups").exists());
    assert!(!home.join("cswitch-profiles/settings.json").exists());
    let state = list_provider_state_read_only(home).unwrap();
    assert_eq!(
        state.official_accounts.iter().filter(|a| a.active).count(),
        1
    );
    let public = serde_json::to_string(&state).unwrap();
    assert!(!public.contains("refresh_token"));
    assert!(!public.contains("a-new"));
}

#[test]
fn failed_or_cancelled_add_does_not_change_active_credentials() {
    let home = tempdir().unwrap();
    let token = auth("s", "u", "a@example.invalid", "old");
    write_live(home.path(), &token);
    assert!(
        add_official_account_with(home.path(), &ProgressReporter::default(), || Err(
            "官方登录已取消".into()
        ))
        .is_err()
    );
    assert_eq!(fs::read(home.path().join("auth.json")).unwrap(), token);
    assert_eq!(Store::new(home.path()).list().unwrap().len(), 1);
}

#[test]
fn switching_preserves_last_rotation_and_switches_only_selected_identity() {
    let home = tempdir().unwrap();
    let home = home.path();
    let a = auth("space-a", "user-a", "a@example.invalid", "a-old");
    write_live(home, &a);
    let store = Store::new(home);
    let old = store.save(&a).unwrap();
    let b = store
        .save(&auth("space-b", "user-b", "b@example.invalid", "b-old"))
        .unwrap();
    let config = fs::read(home.join("config.toml")).unwrap();
    switch_official_account_with(
        home,
        &b.id,
        &ProgressReporter::default(),
        || {
            fs::write(
                home.join("auth.json"),
                auth("space-a", "user-a", "a@example.invalid", "a-final"),
            )?;
            Ok(true)
        },
        |candidate| {
            assert_eq!(
                crate::official_accounts::identity(candidate).unwrap(),
                b.identity
            );
            Ok(Some(auth(
                "space-b",
                "user-b",
                "b@example.invalid",
                "b-refreshed",
            )))
        },
    )
    .unwrap();
    assert_eq!(
        store.load(&old.id).unwrap().auth["tokens"]["refresh_token"],
        "a-final"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(home.join("auth.json")).unwrap()).unwrap()["tokens"]
            ["refresh_token"],
        "b-refreshed"
    );
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), config);
    switch_official_account_with(
        home,
        &old.id,
        &ProgressReporter::default(),
        || Ok(true),
        validate_same,
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(home.join("auth.json")).unwrap()).unwrap()["tokens"]
            ["refresh_token"],
        "a-final"
    );
}

#[test]
fn invalid_selected_account_does_not_fall_back_to_another_account() {
    let home = tempdir().unwrap();
    let home = home.path();
    let a = auth("a", "a", "a@example.invalid", "a-token");
    write_live(home, &a);
    let b = Store::new(home)
        .save(&auth("b", "b", "b@example.invalid", "b-token"))
        .unwrap();
    let err = switch_official_account_with(
        home,
        &b.id,
        &ProgressReporter::default(),
        || Ok(true),
        |_| Ok(None),
    )
    .unwrap_err();
    assert!(err.to_string().contains("失效"));
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), a);
    assert!(!home.join("cswitch-backups").exists());
}

#[test]
fn close_failure_does_not_validate_or_overwrite_auth() {
    let home = tempdir().unwrap();
    let a = auth("a", "a", "a@example.invalid", "old");
    write_live(home.path(), &a);
    let b = Store::new(home.path())
        .save(&auth("b", "b", "b@example.invalid", "new"))
        .unwrap();
    assert!(
        switch_official_account_with(
            home.path(),
            &b.id,
            &ProgressReporter::default(),
            || Err("close denied".into()),
            |_| panic!("must not refresh before closing")
        )
        .is_err()
    );
    assert_eq!(fs::read(home.path().join("auth.json")).unwrap(), a);
}

#[test]
fn transaction_failure_keeps_rotated_target_token_and_original_live_account() {
    let home = tempdir().unwrap();
    let home = home.path();
    let a = auth("a", "a", "a@example.invalid", "a-old");
    write_live(home, &a);
    let b = Store::new(home)
        .save(&auth("b", "b", "b@example.invalid", "b-old"))
        .unwrap();
    fs::write(home.join("state_5.sqlite"), b"broken database").unwrap();
    assert!(
        switch_official_account_with(
            home,
            &b.id,
            &ProgressReporter::default(),
            || Ok(true),
            |_| Ok(Some(auth("b", "b", "b@example.invalid", "b-rotated")))
        )
        .is_err()
    );
    assert_eq!(
        Store::new(home).load(&b.id).unwrap().auth["tokens"]["refresh_token"],
        "b-rotated"
    );
    assert_eq!(fs::read(home.join("auth.json")).unwrap(), a);
}

#[test]
fn legacy_snapshot_and_live_auth_are_imported_as_separate_accounts() {
    let home = tempdir().unwrap();
    let home = home.path();
    let a = auth("a", "a", "a@example.invalid", "a-old");
    // Construct the pre-multi-account layout, not the new save hook.
    fs::create_dir_all(home.join("cswitch-profiles/official")).unwrap();
    fs::write(home.join("cswitch-profiles/official/auth.json"), &a).unwrap();
    fs::write(
        home.join("cswitch-profiles/official/config.toml"),
        b"model='old'\n",
    )
    .unwrap();
    write_live(home, &auth("b", "b", "b@example.invalid", "b-live"));
    sync_official_accounts(home).unwrap();
    sync_official_accounts(home).unwrap();
    assert_eq!(Store::new(home).list().unwrap().len(), 2);
}

#[test]
#[ignore = "uses CSWITCH_TEST_BINARY; multiple accounts on a fixed local route"]
fn resident_account_switch_keeps_port_config_history_and_forwards_selected_credentials() {
    use tiny_http::{Response, Server};
    let server = Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", server.server_addr());
    let dir = tempdir().unwrap();
    let home = dir.path();
    let a = auth("a", "a", "a@example.invalid", "a-old");
    write_live(home, &a);
    fs::write(
        home.join("config.toml"),
        format!("model='fixture'\nopenai_base_url='{base}'\n"),
    )
    .unwrap();
    let store = Store::new(home);
    let arow = store.save(&a).unwrap();
    let b = store
        .save(&auth("b", "b", "b@example.invalid", "b-old"))
        .unwrap();
    activate_official_route_with_close(home, "openai", &ProgressReporter::default(), || Ok(true))
        .unwrap();
    let profiles = ProfileStore::new(home);
    let route = profiles.load_settings().unwrap().official_route.unwrap();
    let config = fs::read(home.join("config.toml")).unwrap();
    // Account changes must not open or migrate history at all.
    fs::write(home.join("state_5.sqlite"), b"history stays untouched").unwrap();
    switch_official_account_with(
        home,
        &b.id,
        &ProgressReporter::default(),
        || Ok(true),
        validate_same,
    )
    .unwrap();
    assert_eq!(fs::read(home.join("config.toml")).unwrap(), config);
    assert_eq!(
        fs::read(home.join("state_5.sqlite")).unwrap(),
        b"history stays untouched"
    );
    assert_eq!(
        profiles
            .load_settings()
            .unwrap()
            .official_route
            .unwrap()
            .local_base_url,
        route.local_base_url
    );
    let live: Value = serde_json::from_slice(&fs::read(home.join("auth.json")).unwrap()).unwrap();
    let token = live["tokens"]["access_token"].as_str().unwrap().to_owned();
    let expected = token.clone();
    let upstream = std::thread::spawn(move || {
        let req = server
            .recv_timeout(std::time::Duration::from_secs(15))
            .unwrap()
            .unwrap();
        assert_eq!(
            req.headers()
                .iter()
                .find(|h| h.field.equiv("authorization"))
                .unwrap()
                .value
                .as_str(),
            format!("Bearer {expected}")
        );
        assert_eq!(
            req.headers()
                .iter()
                .find(|h| h.field.equiv("chatgpt-account-id"))
                .unwrap()
                .value
                .as_str(),
            "b"
        );
        req.respond(Response::from_string("{}")).unwrap();
    });
    let result = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/responses", route.local_base_url))
        .bearer_auth(token)
        .header("chatgpt-account-id", "b")
        .json(&json!({"model":"fixture","input":"hello"}))
        .send()
        .unwrap();
    assert!(result.status().is_success());
    upstream.join().unwrap();
    let before = fs::read_dir(home.join("cswitch-backups")).unwrap().count();
    switch_official_account_with(
        home,
        &b.id,
        &ProgressReporter::default(),
        || panic!("same account must not close"),
        |_| panic!("same account stays live"),
    )
    .unwrap();
    assert_eq!(
        fs::read_dir(home.join("cswitch-backups")).unwrap().count(),
        before
    );
    switch_official_account_with(
        home,
        &arow.id,
        &ProgressReporter::default(),
        || Ok(true),
        validate_same,
    )
    .unwrap();
    assert_eq!(
        crate::official_accounts::identity(&fs::read(home.join("auth.json")).unwrap())
            .unwrap()
            .account,
        "a"
    );
    gateway::stop(home).unwrap();
}

#[test]
fn saving_legacy_snapshot_never_overwrites_reauthenticated_live_identity() {
    let home = tempdir().unwrap();
    let old = auth("a", "a", "a@example.invalid", "old");
    write_live(home.path(), &old);
    let store = Store::new(home.path());
    store.sync_live(&old).unwrap();
    let fresh = store
        .save(&auth("a", "a", "a@example.invalid", "fresh"))
        .unwrap();
    ProfileStore::new(home.path())
        .save_official(b"model='fixture'", &old)
        .unwrap();
    assert_eq!(
        store.load(&fresh.id).unwrap().auth["tokens"]["refresh_token"],
        "fresh"
    );
}
