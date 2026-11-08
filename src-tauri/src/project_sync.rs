//! Desktop 26.1103 scopes renderer state to a ChatGPT principal. Its constructor
//! selects the null owner, which clears scoped globals even on API-key restarts.
//! Suspend that envelope (without discarding it) while using a key, and restore
//! it before ChatGPT login. Only local project metadata is shared across owners.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, error::Error};

pub(crate) const FILE_NAME: &str = ".codex-global-state.json";
const ACCOUNT_STATE: &str = "electron-account-renderer-state-v1";
const SHARED_STATE: &str = "cswitch-shared-project-state-v1";
const ATOMS: &str = "electron-persisted-atom-state";

// Observed in the desktop's global-state implementation, not inferred by prefix.
const LOCAL_KEYS: &[&str] = &[
    "active-workspace-roots",
    "electron-saved-workspace-roots",
    "electron-workspace-root-labels",
    "local-projects",
    "app-server-project-id-by-legacy-project-id-by-host",
    "app-server-projects-migration-by-host",
    "app-server-pending-project-deletions-by-host",
    "selected-project",
    "project-writable-roots",
    "project-appearances",
    "sidebar-project-thread-orders",
    "thread-project-assignments",
    "thread-project-membership-host-ids",
    "thread-writable-roots",
    "thread-workspace-root-hints",
    "thread-projectless-output-directories",
    "projectless-thread-ids",
    "project-order",
];
const ACCOUNT_ONLY_KEYS: &[&str] = &[
    "open-in-target-preferences",
    "worktree-repository-preferences",
    "desktop-app-beta-preferences-by-account-id",
    "project-files",
    "pinned-thread-ids",
    "app-server-migrated-pinned-thread-ids-by-host",
    "pinned-project-ids",
    "sidebar-thread-metadata",
    "selected-remote-host-id",
    "remote-projects",
    "active-remote-project-id",
    "connection-group-order",
    "remote-cwds-by-host-and-workspace",
    "host-id-remote-control-allowed",
    "added-remote-control-env-ids",
    "remote-project-connection-backfill-completed",
    "remote-connection-auto-connect-by-host-id",
    "queued-follow-ups",
];
const GLOBAL_ATOMS: &[&str] = &[
    "last_completed_onboarding",
    "electron:onboarding-projectless-completed",
    "electron:bedrock-govcloud-guidance-acknowledged",
    "ctrl-tab-shortcut-notice-shown",
    "number-shortcut-notice-shown",
    "thread-sort-key",
];

#[derive(Clone, Default, Deserialize, Serialize)]
struct Snapshot {
    atoms: Map<String, Value>,
    globals: Map<String, Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct Accounts {
    owner: Option<String>,
    inactive: BTreeMap<String, Snapshot>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Shared {
    version: u8,
    last_owner: Option<String>,
    suspended: Option<Accounts>,
}

fn local_fields(globals: &Map<String, Value>) -> Map<String, Value> {
    let mut result: Map<_, _> = LOCAL_KEYS
        .iter()
        .filter_map(|key| {
            globals
                .get(*key)
                .map(|value| ((*key).into(), value.clone()))
        })
        .collect();
    if let Some(Value::Object(assignments)) = result.get_mut("thread-project-assignments") {
        assignments
            .retain(|_, value| value.get("projectKind").and_then(Value::as_str) == Some("local"));
    }
    if result
        .get("selected-project")
        .is_some_and(|v| v.get("type").and_then(Value::as_str) != Some("local"))
    {
        result.remove("selected-project");
    }
    result
}

#[cfg(test)]
#[path = "project_sync_tests.rs"]
mod tests;

// Merge only during initial adoption/recovery. Once sharing is established, the
// live view is authoritative: a deleted project must not return from a snapshot.
fn merge(target: &mut Value, incoming: Value) {
    match (target, incoming) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in b {
                match (a.get_mut(&key), value) {
                    (Some(Value::Object(old)), Value::Object(new)) if key != "selected-project" => {
                        if key == "app-server-project-id-by-legacy-project-id-by-host" {
                            for (host, ids) in new {
                                match (old.get_mut(&host), ids) {
                                    (Some(Value::Object(old_ids)), Value::Object(new_ids)) => {
                                        old_ids.extend(new_ids)
                                    }
                                    (_, ids) => {
                                        old.insert(host, ids);
                                    }
                                }
                            }
                        } else {
                            // Replace whole records: never union a project's old writable roots.
                            old.extend(new);
                        }
                    }
                    (Some(old @ Value::Array(_)), new @ Value::Array(_)) => merge(old, new),
                    (_, value) => {
                        a.insert(key, value);
                    }
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            let mut order = b;
            for value in a.drain(..) {
                if !order.contains(&value) {
                    order.push(value);
                }
            }
            *a = order;
        }
        (target, value) => *target = value,
    }
}

fn install_local(globals: &mut Map<String, Value>, shared: &Map<String, Value>) {
    let unrelated_assignments = globals
        .get("thread-project-assignments")
        .and_then(Value::as_object)
        .map(|items| {
            items
                .iter()
                .filter(|(_, value)| {
                    value.get("projectKind").and_then(Value::as_str) != Some("local")
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Map<_, _>>()
        })
        .unwrap_or_default();
    for key in LOCAL_KEYS {
        globals.remove(*key);
    }
    globals.extend(shared.clone());
    if !unrelated_assignments.is_empty() {
        globals
            .entry("thread-project-assignments")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("validated assignments")
            .extend(unrelated_assignments);
    }
}

fn validate_local(fields: &Map<String, Value>) -> Result<(), Box<dyn Error>> {
    for (key, value) in fields {
        let valid = match key.as_str() {
            "active-workspace-roots"
            | "electron-saved-workspace-roots"
            | "project-order"
            | "projectless-thread-ids" => value.is_array(),
            "selected-project" => value.is_object() || value.is_null(),
            _ => value.is_object(),
        };
        if !valid {
            return Err(format!("项目状态 {key} 格式不受支持，未修改原文件").into());
        }
    }
    Ok(())
}

fn owner_for_auth(auth: Option<&[u8]>) -> Result<Option<String>, Box<dyn Error>> {
    let Some(auth) = auth else {
        return Ok(None);
    };
    if crate::config::api_key_from_auth(auth)?.is_some() {
        return Ok(None);
    }
    let identity = crate::official_accounts::identity(auth)
        .ok_or("官方登录缺少用户或账号标识，无法恢复对应的项目状态")?;
    // This is the desktop's principal key, not a file checksum.
    let principal = serde_json::to_vec(&[identity.user, identity.account])?;
    Ok(Some(format!("{:x}", Sha256::digest(principal))))
}

pub(crate) fn prepare(
    original: &[u8],
    auth: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    let before: Value =
        serde_json::from_slice(original).map_err(|e| format!("读取项目状态失败：{e}"))?;
    let mut doc = before.as_object().ok_or("项目状态根节点不是对象")?.clone();
    if !doc.contains_key(ACCOUNT_STATE) && !doc.contains_key(SHARED_STATE) {
        return Ok(None);
    }
    let saved = doc
        .get(SHARED_STATE)
        .map(|v| serde_json::from_value::<Shared>(v.clone()))
        .transpose()?;
    if saved.as_ref().is_some_and(|s| s.version != 1) {
        return Err("不支持的 CSwitch 项目状态版本".into());
    }
    let target = owner_for_auth(auth)?;
    let live = doc
        .get(ACCOUNT_STATE)
        .map(|v| {
            if !v.get("owner").is_some_and(|o| o.is_null() || o.is_string()) {
                return Err("账号项目状态缺少 owner".into());
            }
            serde_json::from_value::<Accounts>(v.clone())
                .map_err(|e| -> Box<dyn Error> { e.into() })
        })
        .transpose()?;
    let mut accounts = live
        .clone()
        .or_else(|| saved.as_ref().and_then(|s| s.suspended.clone()))
        .unwrap_or_default();
    if live.is_some()
        && let Some(suspended) = saved.as_ref().and_then(|s| s.suspended.as_ref())
    {
        for (owner, snapshot) in &suspended.inactive {
            accounts
                .inactive
                .entry(owner.clone())
                .or_insert_with(|| snapshot.clone());
        }
    }
    let active_local = local_fields(&doc);
    validate_local(&active_local)?;
    let mut shared = Value::Object(Map::new());
    if saved.is_none() {
        for snapshot in accounts.inactive.values() {
            let fields = local_fields(&snapshot.globals);
            validate_local(&fields)?;
            merge(&mut shared, Value::Object(fields));
        }
    } else if live.as_ref().is_some_and(|a| a.owner.is_none())
        && let Some(snapshot) = saved
            .as_ref()
            .and_then(|s| s.last_owner.as_ref())
            .and_then(|o| accounts.inactive.get(o))
    {
        let fields = local_fields(&snapshot.globals);
        validate_local(&fields)?;
        shared = Value::Object(fields);
    }
    merge(&mut shared, Value::Object(active_local));
    let shared = shared.as_object().expect("merged project object");
    let last_owner = accounts
        .owner
        .clone()
        .or_else(|| saved.as_ref().and_then(|s| s.last_owner.clone()));
    if let Some(owner) = live.as_ref().and_then(|a| a.owner.as_ref()) {
        let atoms = doc
            .get(ATOMS)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let globals = LOCAL_KEYS
            .iter()
            .chain(ACCOUNT_ONLY_KEYS)
            .filter_map(|k| doc.get(*k).map(|v| ((*k).into(), v.clone())))
            .collect();
        let snapshot = accounts.inactive.entry(owner.clone()).or_default();
        snapshot.atoms = atoms
            .into_iter()
            .filter(|(k, _)| !GLOBAL_ATOMS.contains(&k.as_str()))
            .collect();
        snapshot.globals = globals;
    }
    accounts.owner = None;
    // Do not expose another account's cloud caches, remote connections or queued actions.
    if live.is_some() || target.is_some() {
        for key in LOCAL_KEYS.iter().chain(ACCOUNT_ONLY_KEYS) {
            doc.remove(*key);
        }
        if let Some(Value::Object(atoms)) = doc.get_mut(ATOMS) {
            atoms.retain(|k, _| GLOBAL_ATOMS.contains(&k.as_str()));
        }
    }
    install_local(&mut doc, shared);
    let shared_state = if let Some(owner) = target {
        install_local(
            &mut accounts.inactive.entry(owner.clone()).or_default().globals,
            shared,
        );
        doc.insert(ACCOUNT_STATE.into(), serde_json::to_value(accounts)?);
        Shared {
            version: 1,
            last_owner: Some(owner),
            suspended: None,
        }
    } else {
        doc.remove(ACCOUNT_STATE);
        Shared {
            version: 1,
            last_owner,
            suspended: Some(accounts),
        }
    };
    doc.insert(SHARED_STATE.into(), serde_json::to_value(shared_state)?);
    let after = Value::Object(doc);
    if after == before {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_vec(&after)?))
    }
}
