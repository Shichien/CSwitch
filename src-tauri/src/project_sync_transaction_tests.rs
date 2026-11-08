use super::*;
use serde_json::json;
use tempfile::tempdir;

const API: &[u8] = br#"{"OPENAI_API_KEY":"fixture-key"}"#;
const CONFIG: &[u8] = b"model_provider='custom'\n";

fn fixture(root: &Path) -> Vec<u8> {
    fs::write(root.join("auth.json"), API).unwrap();
    fs::write(root.join("config.toml"), CONFIG).unwrap();
    let state = serde_json::to_vec(&json!({
        "local-projects":{"new":{"name":"New"}},
        "electron-account-renderer-state-v1":{"owner":null,"inactive":{"account":{
            "atoms":{},"globals":{"local-projects":{"old":{"name":"Original"}},"project-order":["old"]}
        }}}
    })).unwrap();
    fs::write(root.join(crate::project_sync::FILE_NAME), &state).unwrap();
    state
}

#[test]
fn reapplying_same_provider_repairs_projects_even_when_database_and_auth_are_unchanged() {
    let home = tempdir().unwrap();
    let root = home.path();
    let original = fixture(root);
    let report = apply_provider_state(
        root,
        Some(CONFIG),
        CONFIG,
        "custom",
        AuthUpdate::Replace(API),
    )
    .unwrap();
    assert!(report.project_state_updated);
    assert_eq!(report.sqlite_rows_updated, 0);
    assert_eq!(
        fs::read(Path::new(&report.backup_path).join("global-state.json")).unwrap(),
        original
    );
    let current: Value =
        serde_json::from_slice(&fs::read(root.join(crate::project_sync::FILE_NAME)).unwrap())
            .unwrap();
    assert_eq!(current["local-projects"].as_object().unwrap().len(), 2);
    let second = apply_provider_state(
        root,
        Some(CONFIG),
        CONFIG,
        "custom",
        AuthUpdate::Replace(API),
    )
    .unwrap();
    assert!(!second.project_state_updated);
    assert!(second.backup_path.is_empty());
}

#[test]
fn concurrent_project_edit_is_preserved_and_database_update_is_rolled_back() {
    let home = tempdir().unwrap();
    let root = home.path();
    fixture(root);
    let db = Connection::open(root.join(STATE_DB_NAME)).unwrap();
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,model_provider TEXT);INSERT INTO threads VALUES('t','openai');").unwrap();
    let external = br#"{"user-edit":"keep"}"#;
    let result = apply_provider_config_with_hook(
        root,
        Some(CONFIG),
        CONFIG,
        "custom",
        AuthUpdate::Replace(API),
        || {
            fs::write(root.join(crate::project_sync::FILE_NAME), external)?;
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(
        fs::read(root.join(crate::project_sync::FILE_NAME)).unwrap(),
        external
    );
    assert_eq!(
        db.query_row("SELECT model_provider FROM threads", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "openai"
    );
    assert!(!root.join(TRANSACTION_FILE).exists());
}

#[test]
fn late_config_conflict_restores_already_written_projects() {
    let home = tempdir().unwrap();
    let root = home.path();
    let original = fixture(root);
    let result = apply_provider_config_with_hook(
        root,
        Some(CONFIG),
        CONFIG,
        "custom",
        AuthUpdate::Replace(API),
        || {
            fs::write(root.join("config.toml"), b"# user edit")?;
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(
        fs::read(root.join(crate::project_sync::FILE_NAME)).unwrap(),
        original
    );
    assert_eq!(fs::read(root.join("config.toml")).unwrap(), b"# user edit");
    assert!(!root.join(TRANSACTION_FILE).exists());
}

#[test]
fn interruption_recovery_restores_projects_and_refuses_to_overwrite_later_edits() {
    for edited in [false, true] {
        let home = tempdir().unwrap();
        let root = home.path();
        let original = fixture(root);
        let report = apply_provider_state(
            root,
            Some(CONFIG),
            CONFIG,
            "custom",
            AuthUpdate::Replace(API),
        )
        .unwrap();
        begin_transaction(root, Path::new(&report.backup_path)).unwrap();
        let external = br#"{"after-interruption":"keep"}"#;
        if edited {
            fs::write(root.join(crate::project_sync::FILE_NAME), external).unwrap();
        }
        let result = recover_pending_state(root);
        if edited {
            assert!(result.is_err());
            assert_eq!(
                fs::read(root.join(crate::project_sync::FILE_NAME)).unwrap(),
                external
            );
            assert!(root.join(TRANSACTION_FILE).exists());
        } else {
            result.unwrap();
            assert_eq!(
                fs::read(root.join(crate::project_sync::FILE_NAME)).unwrap(),
                original
            );
            assert!(!root.join(TRANSACTION_FILE).exists());
        }
    }
}

#[test]
fn hot_route_changes_do_not_read_or_write_the_running_desktops_state() {
    let home = tempdir().unwrap();
    let root = home.path();
    fixture(root);
    fs::write(
        root.join(crate::project_sync::FILE_NAME),
        b"not a stable snapshot",
    )
    .unwrap();
    let report = apply_provider_config_with_settings_and_hook(
        root,
        Some(CONFIG),
        CONFIG,
        "unchanged",
        AuthUpdate::Keep,
        None,
        false,
        || Ok(()),
    )
    .unwrap();
    assert!(!report.project_state_updated);
    assert_eq!(
        fs::read(root.join(crate::project_sync::FILE_NAME)).unwrap(),
        b"not a stable snapshot"
    );
}
