use super::*;
use base64::Engine;
use serde_json::json;

const API: &[u8] = br#"{"OPENAI_API_KEY":"fixture-api-key"}"#;

fn auth(user: &str) -> Vec<u8> {
    let claims = json!({"exp": 4102444800_i64, "https://api.openai.com/auth": {"user_id": user}});
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap());
    serde_json::to_vec(&json!({"auth_mode":"chatgpt","tokens":{
        "account_id":"workspace", "access_token":format!("e30.{payload}.signature"), "refresh_token":"fixture"
    }})).unwrap()
}

fn apply(state: &Value, auth: &[u8]) -> Value {
    let bytes = serde_json::to_vec(state).unwrap();
    serde_json::from_slice(&prepare(&bytes, Some(auth)).unwrap().unwrap_or(bytes)).unwrap()
}

// Reproduce the desktop constructor's null-owner reset and subsequent principal
// selection. This catches a superficially restored top-level project list that
// disappears on the next launch (the original regression).
fn desktop_start(state: &mut Value, owner: Option<&str>) {
    let Some(mut accounts) = state.get(ACCOUNT_STATE).cloned() else {
        return;
    };
    if let Some(previous) = accounts["owner"].as_str().map(str::to_owned) {
        let mut atoms = state.get(ATOMS).cloned().unwrap_or(json!({}));
        atoms
            .as_object_mut()
            .unwrap()
            .retain(|k, _| !GLOBAL_ATOMS.contains(&k.as_str()));
        let globals: Map<_, _> = LOCAL_KEYS
            .iter()
            .chain(ACCOUNT_ONLY_KEYS)
            .filter_map(|k| state.get(*k).map(|v| ((*k).into(), v.clone())))
            .collect();
        accounts["inactive"][previous] = json!({"atoms":atoms,"globals":globals});
    }
    let root = state.as_object_mut().unwrap();
    for key in LOCAL_KEYS.iter().chain(ACCOUNT_ONLY_KEYS) {
        root.remove(*key);
    }
    let mut atoms = root.get(ATOMS).cloned().unwrap_or(json!({}));
    atoms
        .as_object_mut()
        .unwrap()
        .retain(|k, _| GLOBAL_ATOMS.contains(&k.as_str()));
    if let Some(owner) = owner
        && let Some(snapshot) = accounts["inactive"].as_object_mut().unwrap().remove(owner)
    {
        root.extend(snapshot["globals"].as_object().unwrap().clone());
        let mut restored = snapshot["atoms"].as_object().unwrap().clone();
        restored.extend(atoms.as_object().unwrap().clone());
        atoms = Value::Object(restored);
    }
    accounts["owner"] = owner
        .map(|s| Value::String(s.into()))
        .unwrap_or(Value::Null);
    root.insert(ATOMS.into(), atoms);
    root.insert(ACCOUNT_STATE.into(), accounts);
}

fn broken_state() -> Value {
    let owner = owner_for_auth(Some(&auth("alice"))).unwrap().unwrap();
    json!({
        "local-projects":{"new":{"name":"New project","rootPaths":["/new"]}},
        "project-order":["new"],
        "selected-project":{"type":"local","projectId":"new"},
        "theme":"keep",
        ATOMS:{"thread-sort-key":"updated","api-only-ui":"keep"},
        ACCOUNT_STATE:{"owner":null,"inactive":{owner:{
            "atoms":{"cloud-sidebar":{"account":"alice"}},
            "globals":{
                "local-projects":{"old":{"name":"Original","rootPaths":["/old"]}},
                "project-order":["old"],
                "thread-project-assignments":{"t1":{"projectKind":"local","projectId":"old"},"cloud":{"projectKind":"chatgpt","projectId":"private"}},
                "app-server-project-id-by-legacy-project-id-by-host":{"local:test":{"old":"database-id"}},
                "remote-projects":{"remote":"private"},
                "queued-follow-ups":["never execute from another account"]
            }
        }}}
    })
}

#[test]
fn repairs_hidden_projects_and_survives_repeated_api_restarts_without_official_auth() {
    let mut state = apply(&broken_state(), API);
    assert_eq!(state["local-projects"].as_object().unwrap().len(), 2);
    assert_eq!(state["project-order"], json!(["new", "old"]));
    assert_eq!(state["selected-project"]["projectId"], "new");
    assert_eq!(
        state["thread-project-assignments"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        state["app-server-project-id-by-legacy-project-id-by-host"]["local:test"]["old"],
        "database-id"
    );
    assert!(state.get(ACCOUNT_STATE).is_none());
    assert!(state.get("queued-follow-ups").is_none());
    assert!(state.get("remote-projects").is_none());
    assert!(state[ATOMS].get("cloud-sidebar").is_none());
    assert_eq!(state["theme"], "keep");
    for _ in 0..3 {
        let expected = state.clone();
        desktop_start(&mut state, None);
        assert_eq!(state, expected);
        assert!(
            prepare(&serde_json::to_vec(&state).unwrap(), Some(API))
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn switching_back_preserves_additions_and_deletions_without_importing_cloud_state() {
    let mut state = apply(&broken_state(), API);
    state["local-projects"]
        .as_object_mut()
        .unwrap()
        .remove("old");
    state["project-order"] = json!(["new"]);
    state["thread-project-assignments"] = json!({});
    state["local-projects"]["new"]["name"] = "Renamed".into();
    let official = auth("alice");
    let owner = owner_for_auth(Some(&official)).unwrap().unwrap();
    state = apply(&state, &official);
    desktop_start(&mut state, Some(&owner));
    assert!(state["local-projects"].get("old").is_none());
    assert_eq!(state["local-projects"]["new"]["name"], "Renamed");
    assert_eq!(state[ATOMS]["cloud-sidebar"]["account"], "alice");
    assert_eq!(
        state["thread-project-assignments"]["cloud"]["projectKind"],
        "chatgpt"
    );
    state = apply(&state, API);
    desktop_start(&mut state, None);
    assert!(state["local-projects"].get("old").is_none());
    assert!(state[ATOMS].get("cloud-sidebar").is_none());
    assert!(state["thread-project-assignments"].get("cloud").is_none());
}

#[test]
fn official_account_switch_shares_projects_but_preserves_each_accounts_private_state() {
    let alice = auth("alice");
    let bob = auth("bob");
    let a = owner_for_auth(Some(&alice)).unwrap().unwrap();
    let b = owner_for_auth(Some(&bob)).unwrap().unwrap();
    let mut state = broken_state();
    state[ACCOUNT_STATE]["inactive"][&b] = json!({"atoms":{"cloud-sidebar":{"account":"bob"}},"globals":{"remote-projects":{"bob":"keep"}}});
    state = apply(&state, &alice);
    desktop_start(&mut state, Some(&a));
    state["local-projects"]["added"] = json!({"name":"Added","rootPaths":["/added"]});
    state = apply(&state, &bob);
    desktop_start(&mut state, Some(&b));
    assert_eq!(state["local-projects"].as_object().unwrap().len(), 3);
    assert_eq!(state[ATOMS]["cloud-sidebar"]["account"], "bob");
    assert_eq!(state["remote-projects"], json!({"bob":"keep"}));
    state = apply(&state, &alice);
    desktop_start(&mut state, Some(&a));
    assert_eq!(state[ATOMS]["cloud-sidebar"]["account"], "alice");
    assert!(state["local-projects"].get("added").is_some());
}

#[test]
fn live_project_records_replace_old_roots_instead_of_expanding_access() {
    let mut state = broken_state();
    state["local-projects"]["old"] = json!({"name":"Edited","rootPaths":["/allowed"]});
    let updated = apply(&state, API);
    assert_eq!(
        updated["local-projects"]["old"]["rootPaths"],
        json!(["/allowed"])
    );
}

#[test]
fn old_desktops_are_untouched_and_invalid_states_fail_explicitly() {
    assert!(
        prepare(br#"{"local-projects":{"p":{}}}"#, Some(API))
            .unwrap()
            .is_none()
    );
    assert!(prepare(b"not json", Some(API)).is_err());
    let mut state = broken_state();
    state["local-projects"] = false.into();
    assert!(prepare(&serde_json::to_vec(&state).unwrap(), Some(API)).is_err());
    let mut state = broken_state();
    state[SHARED_STATE] = json!({"version":99,"lastOwner":null,"suspended":null});
    assert!(prepare(&serde_json::to_vec(&state).unwrap(), Some(API)).is_err());
    assert!(
        prepare(
            &serde_json::to_vec(&broken_state()).unwrap(),
            Some(br#"{"auth_mode":"chatgpt"}"#)
        )
        .is_err()
    );
}
