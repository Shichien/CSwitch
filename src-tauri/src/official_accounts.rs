use crate::oauth;
use crate::profiles::atomic_write_private;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha512};
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

// One atomic document per identity; no second registry that can lose alignment.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Identity {
    pub account: String,
    pub user: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Account {
    pub id: String,
    pub identity: Identity,
    pub label: String,
    pub auth: Value,
    #[serde(default)]
    live_revision: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Summary {
    pub id: String,
    pub label: String,
    pub workspace: String,
    pub active: bool,
    pub login_retained: bool,
}
fn claims(doc: &Value, name: &str) -> Value {
    let token = doc
        .get("tokens")
        .and_then(|v| v.get(name))
        .or_else(|| doc.get(name))
        .and_then(Value::as_str)
        .unwrap_or_default();
    token
        .split('.')
        .nth(1)
        .and_then(|s| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(s)
                .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(s))
                .ok()
        })
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}
fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
pub(crate) fn identity(auth: &[u8]) -> Option<Identity> {
    if oauth::inspect_auth(auth) == oauth::LocalAuthState::Invalid {
        return None;
    }
    let doc: Value = serde_json::from_slice(auth).ok()?;
    let access = claims(&doc, "access_token");
    let id = claims(&doc, "id_token");
    let account = text(doc.pointer("/tokens/account_id"))
        .or_else(|| text(doc.get("chatgpt_account_id")))
        .or_else(|| text(doc.get("account_id")))
        .or_else(|| text(access.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")))
        .or_else(|| text(id.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")))?;
    // A workspace may contain several users. Never merge them by workspace ID alone.
    let user = text(access.pointer("/https:~1~1api.openai.com~1auth/user_id"))
        .or_else(|| text(id.pointer("/https:~1~1api.openai.com~1auth/user_id")))
        .or_else(|| text(id.get("sub")))
        .or_else(|| text(access.get("sub")))?;
    Some(Identity { account, user })
}
fn label(auth: &Value) -> String {
    let id = claims(auth, "id_token");
    let access = claims(auth, "access_token");
    text(id.get("email"))
        .or_else(|| text(access.pointer("/https:~1~1api.openai.com~1profile/email")))
        .or_else(|| text(auth.get("email")))
        .or_else(|| text(id.get("name")))
        .unwrap_or_else(|| "官方账号".into())
}
fn revision(auth: &[u8]) -> String {
    format!("{:x}", Sha512::digest(auth))
}

pub(crate) struct Store {
    root: PathBuf,
}
impl Store {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join("cswitch-profiles/official-accounts"),
        }
    }
    fn path(&self, id: &str) -> Result<PathBuf, Box<dyn Error>> {
        if id.len() != 32 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("官方账号编号无效".into());
        }
        Ok(self.root.join(format!("{id}.json")))
    }
    pub fn list(&self) -> Result<Vec<Account>, Box<dyn Error>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut rows = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let p = entry?.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let row: Account = serde_json::from_slice(&fs::read(&p)?)?;
            if self.path(&row.id)? != p
                || identity(&serde_json::to_vec(&row.auth)?) != Some(row.identity.clone())
            {
                return Err(format!("官方账号快照身份不一致：{}", p.display()).into());
            }
            rows.push(row);
        }
        rows.sort_by(|a, b| a.label.cmp(&b.label).then(a.id.cmp(&b.id)));
        Ok(rows)
    }
    pub fn load(&self, id: &str) -> Result<Account, Box<dyn Error>> {
        self.path(id)?;
        self.list()?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| "官方账号不存在".into())
    }
    pub fn save(&self, auth: &[u8]) -> Result<Account, Box<dyn Error>> {
        let identity = identity(auth).ok_or("官方凭据缺少账号或用户标识，请重新添加账号")?;
        let old = self.list()?.into_iter().find(|a| a.identity == identity);
        let auth: Value = serde_json::from_slice(auth)?;
        let account = Account {
            id: old
                .as_ref()
                .map(|a| a.id.clone())
                .unwrap_or_else(|| format!("{:032x}", rand::random::<u128>())),
            identity,
            label: label(&auth),
            auth,
            live_revision: old.and_then(|a| a.live_revision),
        };
        self.write(&account)?;
        Ok(account)
    }
    fn write(&self, a: &Account) -> Result<(), Box<dyn Error>> {
        atomic_write_private(&self.path(&a.id)?, &serde_json::to_vec_pretty(a)?)
    }
    pub fn import_legacy(&self, auth: &[u8]) -> Result<(), Box<dyn Error>> {
        if let Some(key) = identity(auth)
            && !self.list()?.iter().any(|a| a.identity == key)
        {
            self.save(auth)?;
        }
        Ok(())
    }
    pub fn sync_live(&self, auth: &[u8]) -> Result<(), Box<dyn Error>> {
        let Some(key) = identity(auth) else {
            return Ok(());
        };
        let rev = revision(auth);
        if self
            .list()?
            .iter()
            .any(|a| a.identity == key && a.live_revision.as_deref() == Some(&rev))
        {
            // A fresh OAuth login may have updated the saved account while the live
            // file is still unchanged. Do not overwrite it with the old refresh token.
            return Ok(());
        }
        let mut row = self.save(auth)?;
        row.live_revision = Some(rev);
        self.write(&row)
    }
    pub fn mark_live(&self, auth: &[u8]) -> Result<(), Box<dyn Error>> {
        let mut row = self.save(auth)?;
        row.live_revision = Some(revision(auth));
        self.write(&row)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn auth(account: &str, user: &str, email: &str, refresh: &str) -> Vec<u8> {
        let claims = serde_json::json!({"exp":4102444800i64,"sub":user,"email":email,
            "https://api.openai.com/auth":{"chatgpt_account_id":account,"user_id":user}});
        let token = format!(
            "e30.{}.test",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&claims).unwrap())
        );
        serde_json::to_vec(&serde_json::json!({"auth_mode":"chatgpt","tokens":{"account_id":account,"id_token":token,"access_token":token,"refresh_token":refresh}})).unwrap()
    }
    #[test]
    fn separates_users_and_workspaces_and_deduplicates_relogin() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let a = store
            .save(&auth("space1", "user1", "one@example.invalid", "r1"))
            .unwrap();
        store
            .save(&auth("space1", "user2", "two@example.invalid", "r2"))
            .unwrap();
        store
            .save(&auth("space2", "user1", "one@example.invalid", "r3"))
            .unwrap();
        let renewed = store
            .save(&auth("space1", "user1", "renamed@example.invalid", "r4"))
            .unwrap();
        assert_eq!(a.id, renewed.id);
        assert_eq!(store.list().unwrap().len(), 3);
        assert_eq!(renewed.auth["tokens"]["refresh_token"], "r4");
    }
    #[test]
    fn newer_login_is_not_overwritten_by_unchanged_live_file_or_legacy_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let old = auth("space", "user", "one@example.invalid", "old");
        store.sync_live(&old).unwrap();
        let row = store
            .save(&auth("space", "user", "one@example.invalid", "new"))
            .unwrap();
        store.sync_live(&old).unwrap();
        store.import_legacy(&old).unwrap();
        assert_eq!(
            store.load(&row.id).unwrap().auth["tokens"]["refresh_token"],
            "new"
        );
        store
            .sync_live(&auth("space", "user", "one@example.invalid", "rotated"))
            .unwrap();
        assert_eq!(
            store.load(&row.id).unwrap().auth["tokens"]["refresh_token"],
            "rotated"
        );
    }
    #[test]
    fn invalid_ids_and_unidentified_credentials_do_not_create_entries() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        assert!(store.load("../../auth").is_err());
        store
            .import_legacy(br#"{"OPENAI_API_KEY":"fixture"}"#)
            .unwrap();
        assert!(store.save(br#"{"tokens":{"refresh_token":"r"}}"#).is_err());
        assert!(!store.root.exists());
    }
}
