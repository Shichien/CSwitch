use chrono::Local;
use fs2::FileExt;
use rusqlite::backup::Backup;
use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const BACKUP_DIR: &str = "cswitch-backups";
const TRANSACTION_FILE: &str = "cswitch-sync-transaction.json";
const LEGACY_BACKUP_DIR: &str = "qpp-backups";
const LEGACY_TRANSACTION_FILE: &str = "qpp-sync-transaction.json";
const SYNC_LOCK_FILES: [&str; 2] = ["cswitch-sync.lock", "qpp-sync.lock"];
const MAX_BACKUPS: usize = 10;
const STATE_DB_NAME: &str = "state_5.sqlite";
const SESSION_DIRS: [&str; 2] = ["sessions", "archived_sessions"];
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCount {
    pub provider: String,
    pub rollout_files: usize,
    pub sqlite_threads: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSyncReport {
    pub rollout_files_updated: usize,
    pub sqlite_rows_updated: usize,
    pub providers_detected: Vec<ProviderCount>,
    pub backup_path: String,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
struct RolloutChange {
    path: PathBuf,
    original_first_line: String,
    separator: String,
    updated_first_line: String,
    original_provider: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupManifest<'a> {
    version: u8,
    created_at: String,
    target_provider: &'a str,
    config_present: bool,
    auth_present: bool,
    sqlite_path: Option<String>,
    providers_detected: &'a [ProviderCount],
    rollout_files: Vec<RolloutBackupEntry<'a>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryManifest {
    version: u8,
    config_present: bool,
    auth_present: bool,
    sqlite_path: Option<String>,
    rollout_files: Vec<RecoveryRolloutEntry>,
    #[serde(default)]
    settings_managed: bool,
    #[serde(default)]
    settings_present: bool,
    #[serde(default)]
    target_provider: Option<String>,
    #[serde(default)]
    checked_files: bool,
    #[serde(default)]
    auth_replaced: bool,
    #[serde(default)]
    extra_sqlite_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryRolloutEntry {
    path: String,
    original_first_line: String,
    updated_first_line: String,
    separator: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TransactionPhase {
    Prepared,
    Committed,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TransactionJournal {
    version: u8,
    backup_name: String,
    phase: TransactionPhase,
}

#[derive(Clone, Copy)]
pub enum AuthUpdate<'a> {
    Keep,
    Replace(&'a [u8]),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RolloutBackupEntry<'a> {
    path: String,
    original_first_line: &'a str,
    updated_first_line: &'a str,
    separator: &'a str,
    original_provider: &'a str,
}

#[cfg(test)]
fn apply_provider_config(
    codex_home: &Path,
    original_config: Option<&[u8]>,
    updated_config: &[u8],
    target_provider: &str,
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    apply_provider_config_with_hook(
        codex_home,
        original_config,
        updated_config,
        target_provider,
        AuthUpdate::Keep,
        || Ok(()),
    )
}

pub fn apply_provider_state(
    codex_home: &Path,
    original_config: Option<&[u8]>,
    updated_config: &[u8],
    target_provider: &str,
    auth_update: AuthUpdate<'_>,
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    apply_provider_config_with_hook(
        codex_home,
        original_config,
        updated_config,
        target_provider,
        auth_update,
        || Ok(()),
    )
}

pub fn recover_pending_state(codex_home: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(codex_home).map_err(|error| -> Box<dyn Error> {
        format!("创建 Codex 目录失败 {}：{error}", codex_home.display()).into()
    })?;
    migrate_legacy_sync_storage(codex_home)?;
    let _operation_lock = acquire_operation_lock(codex_home)?;
    recover_pending_transaction_locked(codex_home)
}

fn migrate_legacy_sync_storage(codex_home: &Path) -> Result<(), Box<dyn Error>> {
    let legacy_backups = codex_home.join(LEGACY_BACKUP_DIR);
    let current_backups = codex_home.join(BACKUP_DIR);
    if legacy_backups.exists() && !current_backups.exists() {
        fs::rename(&legacy_backups, &current_backups).map_err(|error| {
            format!(
                "迁移旧备份目录失败：{} -> {}：{error}",
                legacy_backups.display(),
                current_backups.display()
            )
        })?;
    }

    let legacy_transaction = codex_home.join(LEGACY_TRANSACTION_FILE);
    let current_transaction = codex_home.join(TRANSACTION_FILE);
    if legacy_transaction.exists() && current_transaction.exists() {
        return Err(format!(
            "同时存在旧事务和 CSwitch 事务：{}；{}",
            legacy_transaction.display(),
            current_transaction.display()
        )
        .into());
    }
    if legacy_transaction.exists() {
        fs::rename(&legacy_transaction, &current_transaction).map_err(|error| {
            format!(
                "迁移旧事务失败：{} -> {}：{error}",
                legacy_transaction.display(),
                current_transaction.display()
            )
        })?;
    }
    Ok(())
}

fn apply_provider_config_with_hook<F>(
    codex_home: &Path,
    original_config: Option<&[u8]>,
    updated_config: &[u8],
    target_provider: &str,
    auth_update: AuthUpdate<'_>,
    before_config_write: F,
) -> Result<ProviderSyncReport, Box<dyn Error>>
where
    F: FnOnce() -> Result<(), Box<dyn Error>>,
{
    apply_provider_config_with_settings_and_hook(
        codex_home,
        original_config,
        updated_config,
        target_provider,
        auth_update,
        None,
        before_config_write,
    )
}

pub fn apply_provider_state_with_settings(
    codex_home: &Path,
    original_config: Option<&[u8]>,
    updated_config: &[u8],
    target_provider: &str,
    auth_update: AuthUpdate<'_>,
    settings: Option<&[u8]>,
) -> Result<ProviderSyncReport, Box<dyn Error>> {
    apply_provider_config_with_settings_and_hook(
        codex_home,
        original_config,
        updated_config,
        target_provider,
        auth_update,
        settings,
        || Ok(()),
    )
}

fn apply_provider_config_with_settings_and_hook<F>(
    codex_home: &Path,
    original_config: Option<&[u8]>,
    updated_config: &[u8],
    target_provider: &str,
    auth_update: AuthUpdate<'_>,
    settings: Option<&[u8]>,
    before_config_write: F,
) -> Result<ProviderSyncReport, Box<dyn Error>>
where
    F: FnOnce() -> Result<(), Box<dyn Error>>,
{
    fs::create_dir_all(codex_home).map_err(|error| -> Box<dyn Error> {
        format!("创建 Codex 目录失败 {}：{error}", codex_home.display()).into()
    })?;
    let _operation_lock = acquire_operation_lock(codex_home)?;
    recover_pending_transaction_locked(codex_home)?;
    let auth_path = codex_home.join("auth.json");
    let original_auth = match fs::read(&auth_path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!("读取 {} 失败：{error}", auth_path.display()).into());
        }
    };
    let settings_path = codex_home.join("cswitch-profiles/settings.json");
    let original_settings = if settings.is_some() {
        match fs::read(&settings_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!("读取设置失败 {}：{error}", settings_path.display()).into());
            }
        }
    } else {
        None
    };
    let settings_unchanged = settings.is_none() || settings == original_settings.as_deref();
    let live_config = match fs::read(codex_home.join("config.toml")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if live_config.as_deref() != original_config {
        return Err("config.toml 在读取后发生变化，请重新切换".into());
    }
    let config_unchanged = original_config.is_some_and(|content| content == updated_config);
    let auth_unchanged = match auth_update {
        AuthUpdate::Keep => true,
        AuthUpdate::Replace(auth) => original_auth.as_deref() == Some(auth),
    };
    let changes = collect_rollout_changes(codex_home, target_provider)?;
    let state_db = resolve_state_db(codex_home)?;
    // Both layouts can coexist across desktop/CLI versions. Synchronize both; never guess the active file.
    let extra_databases = state_databases(codex_home)?
        .into_iter()
        .filter(|path| Some(path) != state_db.as_ref())
        .collect::<Vec<_>>();
    let mut sqlite_counts = match state_db.as_deref() {
        Some(path) => match read_sqlite_provider_counts(path) {
            Ok(counts) => counts,
            Err(error) if is_sqlite_busy(error.as_ref()) => {
                return Err(format!(
                    "state_5.sqlite 正在使用，请关闭 Codex 后重试 {}：{error}",
                    path.display()
                )
                .into());
            }
            Err(error) => return Err(error),
        },
        None => BTreeMap::new(),
    };
    for path in &extra_databases {
        for (provider, count) in read_sqlite_provider_counts(path)
            .map_err(|error| format!("读取 SQLite 失败 {}：{error}", path.display()))?
        {
            *sqlite_counts.entry(provider).or_default() += count;
        }
    }
    let sqlite_needs_update = sqlite_counts
        .iter()
        .any(|(provider, count)| provider.as_str() != target_provider && *count > 0);
    if changes.is_empty()
        && !sqlite_needs_update
        && config_unchanged
        && auth_unchanged
        && settings_unchanged
    {
        before_config_write()?;
        return Ok(unchanged_sync_report());
    }
    if sqlite_needs_update && let Some(path) = state_db.as_deref() {
        assert_sqlite_writable(path)?;
        for path in &extra_databases {
            assert_sqlite_writable(path)?;
        }
    }
    let sqlite_backup = if sqlite_needs_update {
        state_db.clone()
    } else {
        None
    };

    let largest = changes
        .iter()
        .map(|change| fs::metadata(&change.path).map(|meta| meta.len()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let database_size = extra_databases
        .iter()
        .map(|path| fs::metadata(path).map(|meta| meta.len()))
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .sum::<u64>()
        + sqlite_backup
            .as_ref()
            .map(fs::metadata)
            .transpose()?
            .map(|meta| meta.len())
            .unwrap_or(0);
    let required = largest
        .saturating_mul(2)
        .saturating_add(database_size)
        .saturating_add(1024 * 1024);
    let free = fs2::available_space(codex_home)?;
    if free < required {
        return Err(format!(
            "磁盘空间不足：同步与恢复至少需要 {required} 字节，当前可用 {free} 字节"
        )
        .into());
    }
    let providers_detected = merge_provider_counts(&changes, &sqlite_counts);
    let backup_dir = create_backup(
        codex_home,
        original_config,
        original_auth.as_deref(),
        sqlite_backup.as_deref(),
        &changes,
        &providers_detected,
        target_provider,
    )?;
    if sqlite_needs_update {
        for (index, path) in extra_databases.iter().enumerate() {
            backup_sqlite(
                path,
                &backup_dir
                    .join("sqlite")
                    .join(format!("extra-{index}.sqlite")),
            )?;
        }
    }
    if settings.is_some() {
        if let Some(bytes) = original_settings.as_deref() {
            write_new_file(&backup_dir.join("settings.json"), bytes)?;
        }
        let manifest_path = backup_dir.join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        manifest["settingsManaged"] = Value::Bool(true);
        manifest["settingsPresent"] = Value::Bool(original_settings.is_some());
        atomic_write(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)?;
    }
    // Persist intended bytes before publishing the journal, so recovery can detect later user edits.
    write_new_file(&backup_dir.join("config.applied.toml"), updated_config)?;
    if let AuthUpdate::Replace(auth) = auth_update {
        write_new_file(&backup_dir.join("auth.applied.json"), auth)?;
    }
    if let Some(bytes) = settings {
        write_new_file(&backup_dir.join("settings.applied.json"), bytes)?;
    }
    let manifest_path = backup_dir.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    manifest["extraSqlitePaths"] = serde_json::to_value(if sqlite_needs_update {
        extra_databases
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    })?;
    manifest["checkedFiles"] = Value::Bool(true);
    manifest["authReplaced"] = Value::Bool(matches!(auth_update, AuthUpdate::Replace(_)));
    atomic_write(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)?;
    let mut journal = begin_transaction(codex_home, &backup_dir)?;

    let config_path = codex_home.join("config.toml");
    let mut applied_rollouts = Vec::new();
    let mut sqlite_mutated = false;
    let mut config_written = false;
    let mut auth_mutated = false;
    let mut settings_mutated = false;

    let result = (|| -> Result<ProviderSyncReport, Box<dyn Error>> {
        for (index, change) in changes.iter().enumerate() {
            applied_rollouts.push(index);
            rewrite_first_line(
                &change.path,
                &change.original_first_line,
                &change.separator,
                &change.updated_first_line,
                index,
            )?;
        }

        let sqlite_rows_updated = if sqlite_needs_update {
            match state_db.as_deref() {
                Some(path) => {
                    sqlite_mutated = true;
                    let mut rows = update_sqlite_provider(path, target_provider)?;
                    for path in &extra_databases {
                        rows += update_sqlite_provider(path, target_provider)?;
                    }
                    rows
                }
                None => 0,
            }
        } else {
            0
        };

        before_config_write()?;
        ensure_file_unchanged(&config_path, original_config)?;
        if !config_unchanged {
            config_written = true;
            atomic_write(&config_path, updated_config)?;
            if fs::read(&config_path)? != updated_config {
                return Err("config.toml 写入后的字节验证失败".into());
            }
        }

        verify_applied_rollouts(&changes)?;
        if sqlite_needs_update && let Some(path) = state_db.as_deref() {
            verify_sqlite_provider(path, target_provider)?;
            for path in &extra_databases {
                verify_sqlite_provider(path, target_provider)?;
            }
        }

        if matches!(auth_update, AuthUpdate::Replace(_)) {
            ensure_file_unchanged(&auth_path, original_auth.as_deref())?;
        }
        match auth_update {
            AuthUpdate::Keep => {}
            AuthUpdate::Replace(_) if auth_unchanged => {}
            AuthUpdate::Replace(auth) => {
                auth_mutated = true;
                atomic_write(&auth_path, auth)?;
                if fs::read(&auth_path)? != auth {
                    return Err("auth.json 写入后的字节验证失败".into());
                }
            }
        }

        if settings.is_some() {
            ensure_file_unchanged(&settings_path, original_settings.as_deref())?;
        }
        if !settings_unchanged && let Some(bytes) = settings {
            settings_mutated = true;
            atomic_write(&settings_path, bytes)?;
            if fs::read(&settings_path)? != bytes {
                return Err("settings.json 写后校验失败".into());
            }
        }
        Ok(ProviderSyncReport {
            warnings: Vec::new(),
            rollout_files_updated: changes.len(),
            sqlite_rows_updated,
            providers_detected,
            backup_path: backup_dir.to_string_lossy().into_owned(),
        })
    })();

    match result {
        Ok(mut report) => {
            journal.phase = TransactionPhase::Committed;
            if let Err(error) = write_transaction_journal(codex_home, &journal) {
                let mut rollback_errors = rollback(
                    &config_path,
                    original_config,
                    &auth_path,
                    original_auth.as_deref(),
                    &backup_dir,
                    state_db.as_deref(),
                    sqlite_mutated,
                    config_written,
                    auth_mutated,
                    &changes,
                    &applied_rollouts,
                );
                if settings_mutated
                    && let Err(error) =
                        restore_optional_file(&settings_path, original_settings.as_deref())
                {
                    rollback_errors.push(format!("恢复设置失败：{error}"));
                }
                return finish_failed_transaction(
                    codex_home,
                    format!("无法提交同步事务：{error}"),
                    rollback_errors,
                    &backup_dir,
                );
            }
            if let Err(error) = clear_transaction_journal(codex_home) {
                report
                    .warnings
                    .push(format!("切换已完成，事务清理待下次重试：{error}"));
            }
            if let Err(error) = prune_backups(codex_home, &backup_dir) {
                report
                    .warnings
                    .push(format!("切换已完成，旧备份清理失败：{error}"));
            }
            Ok(report)
        }
        Err(error) => {
            let mut rollback_errors = rollback(
                &config_path,
                original_config,
                &auth_path,
                original_auth.as_deref(),
                &backup_dir,
                state_db.as_deref(),
                sqlite_mutated,
                config_written,
                auth_mutated,
                &changes,
                &applied_rollouts,
            );
            if settings_mutated
                && let Err(error) =
                    restore_optional_file(&settings_path, original_settings.as_deref())
            {
                rollback_errors.push(format!("恢复设置失败：{error}"));
            }
            finish_failed_transaction(codex_home, error.to_string(), rollback_errors, &backup_dir)
        }
    }
}

fn unchanged_sync_report() -> ProviderSyncReport {
    ProviderSyncReport {
        rollout_files_updated: 0,
        sqlite_rows_updated: 0,
        providers_detected: Vec::new(),
        backup_path: String::new(),
        warnings: Vec::new(),
    }
}

fn finish_failed_transaction<T>(
    codex_home: &Path,
    error: String,
    mut rollback_errors: Vec<String>,
    backup_dir: &Path,
) -> Result<T, Box<dyn Error>> {
    if rollback_errors.is_empty()
        && let Err(clear_error) = clear_transaction_journal(codex_home)
    {
        rollback_errors.push(format!("清除事务记录失败：{clear_error}"));
    }
    if rollback_errors.is_empty() {
        Err(format!("同步失败，所有改动已恢复：{error}").into())
    } else {
        Err(format!(
            "同步失败且自动恢复不完整：{error}。恢复错误：{}。备份位于 {}",
            rollback_errors.join("；"),
            backup_dir.display()
        )
        .into())
    }
}

struct SyncLock {
    _files: Vec<File>,
}

fn acquire_operation_lock(codex_home: &Path) -> Result<SyncLock, Box<dyn Error>> {
    let mut files = Vec::with_capacity(SYNC_LOCK_FILES.len());
    for name in SYNC_LOCK_FILES {
        files.push(acquire_sync_lock_file(&codex_home.join(name))?);
    }
    Ok(SyncLock { _files: files })
}

fn acquire_sync_lock_file(path: &Path) -> Result<File, Box<dyn Error>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| -> Box<dyn Error> {
            format!("打开同步锁文件失败 {}：{error}", path.display()).into()
        })?;
    file.try_lock_exclusive()
        .map_err(|error| -> Box<dyn Error> {
            format!("另一个 CSwitch 同步正在进行，请等待其完成后重试：{error}").into()
        })?;
    Ok(file)
}

fn begin_transaction(
    codex_home: &Path,
    backup_dir: &Path,
) -> Result<TransactionJournal, Box<dyn Error>> {
    let backup_name = backup_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("备份目录名称无效")?
        .to_string();
    validate_backup_name(&backup_name)?;
    let journal = TransactionJournal {
        version: 1,
        backup_name,
        phase: TransactionPhase::Prepared,
    };
    write_transaction_journal(codex_home, &journal)?;
    Ok(journal)
}

fn write_transaction_journal(
    codex_home: &Path,
    journal: &TransactionJournal,
) -> Result<(), Box<dyn Error>> {
    let content = serde_json::to_vec_pretty(journal)?;
    atomic_write(&codex_home.join(TRANSACTION_FILE), &content)
}

fn clear_transaction_journal(codex_home: &Path) -> Result<(), Box<dyn Error>> {
    let path = codex_home.join(TRANSACTION_FILE);
    match fs::remove_file(&path) {
        Ok(()) => sync_directory(codex_home),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn ensure_file_unchanged(path: &Path, expected: Option<&[u8]>) -> Result<(), Box<dyn Error>> {
    let current = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("写入前读取 {} 失败：{error}", path.display()).into()),
    };
    if current.as_deref() != expected {
        return Err(format!(
            "{} 在同步期间发生了新修改，保留当前文件并撤销本次同步",
            path.display()
        )
        .into());
    }
    Ok(())
}

fn recover_pending_transaction_locked(codex_home: &Path) -> Result<(), Box<dyn Error>> {
    let journal_path = codex_home.join(TRANSACTION_FILE);
    let content = match fs::read(&journal_path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let journal: TransactionJournal = serde_json::from_slice(&content)
        .map_err(|error| format!("未完成同步的事务记录无效：{error}"))?;
    if journal.version != 1 {
        return Err(format!("不支持的同步事务版本：{}", journal.version).into());
    }
    validate_backup_name(&journal.backup_name)?;
    if journal.phase == TransactionPhase::Committed {
        clear_transaction_journal(codex_home)?;
        return Ok(());
    }

    let backup_dir = codex_home.join(BACKUP_DIR).join(&journal.backup_name);
    let manifest_path = backup_dir.join("manifest.json");
    let manifest: RecoveryManifest = serde_json::from_slice(&fs::read(&manifest_path)?)
        .map_err(|error| format!("未完成同步的备份清单无效：{error}"))?;
    if manifest.version != 2 {
        return Err(format!("不支持的同步备份版本：{}", manifest.version).into());
    }

    // Recovery mutates live files too: never restore while a desktop writer is active.
    crate::codex_process::close_if_running()?;
    let mut errors = Vec::new();
    restore_from_recovery_manifest(codex_home, &backup_dir, &manifest, &mut errors);
    if errors.is_empty() {
        clear_transaction_journal(codex_home)?;
        Ok(())
    } else {
        Err(format!(
            "检测到上次未完成的同步，但自动恢复不完整：{}。备份位于 {}",
            errors.join("；"),
            backup_dir.display()
        )
        .into())
    }
}

fn restore_from_recovery_manifest(
    codex_home: &Path,
    backup_dir: &Path,
    manifest: &RecoveryManifest,
    errors: &mut Vec<String>,
) {
    if manifest.checked_files {
        for (name, planned, destination, present, managed) in [
            (
                "auth.json",
                "auth.applied.json",
                codex_home.join("auth.json"),
                manifest.auth_present,
                manifest.auth_replaced,
            ),
            (
                "config.toml",
                "config.applied.toml",
                codex_home.join("config.toml"),
                manifest.config_present,
                true,
            ),
            (
                "settings.json",
                "settings.applied.json",
                codex_home.join("cswitch-profiles/settings.json"),
                manifest.settings_present,
                manifest.settings_managed,
            ),
        ] {
            if !managed {
                continue;
            }
            let result = (|| -> Result<(), Box<dyn Error>> {
                let baseline = if present {
                    Some(fs::read(backup_dir.join(name))?)
                } else {
                    None
                };
                let intended = fs::read(backup_dir.join(planned))?;
                let current = match fs::read(&destination) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error.into()),
                };
                if current != baseline && current.as_deref() != Some(intended.as_slice()) {
                    return Err(
                        format!("{name} 在同步中断后发生了新修改，保留当前文件与备份").into(),
                    );
                }
                Ok(())
            })();
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        }
        if !errors.is_empty() {
            return;
        }
    }
    for (name, present) in [
        ("auth.json", manifest.auth_present),
        ("config.toml", manifest.config_present),
    ] {
        if name == "auth.json" && manifest.checked_files && !manifest.auth_replaced {
            continue;
        }
        let destination = codex_home.join(name);
        let content = if present {
            match fs::read(backup_dir.join(name)) {
                Ok(content) => Some(content),
                Err(error) => {
                    errors.push(format!("读取 {name} 备份失败：{error}"));
                    continue;
                }
            }
        } else {
            None
        };
        if let Err(error) = restore_optional_file(&destination, content.as_deref()) {
            errors.push(format!("恢复 {name} 失败：{error}"));
        }
    }

    if manifest.settings_managed {
        let settings = if manifest.settings_present {
            match fs::read(backup_dir.join("settings.json")) {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    errors.push(format!("读取设置备份失败：{error}"));
                    return;
                }
            }
        } else {
            None
        };
        if let Err(error) = restore_optional_file(
            &codex_home.join("cswitch-profiles/settings.json"),
            settings.as_deref(),
        ) {
            errors.push(format!("恢复设置失败：{error}"));
        }
    }
    if let Some(destination) = manifest.sqlite_path.as_deref() {
        let source = backup_dir.join("sqlite").join(STATE_DB_NAME);
        let destination = Path::new(destination);
        if !destination.canonicalize().is_ok_and(|path| {
            codex_home
                .canonicalize()
                .is_ok_and(|home| path.starts_with(home))
        }) {
            errors.push("SQLite 恢复路径不属于当前 .codex".into());
        } else if let Err(error) =
            restore_sqlite(&source, destination, manifest.target_provider.as_deref())
        {
            errors.push(format!("恢复 SQLite 失败：{error}"));
        }
    }

    restore_extra_databases(codex_home, backup_dir, manifest, errors);
    for (index, entry) in manifest.rollout_files.iter().enumerate() {
        let path = PathBuf::from(&entry.path);
        if let Err(error) = validate_rollout_recovery_path(codex_home, &path).and_then(|()| {
            restore_first_line(
                &path,
                &entry.original_first_line,
                &entry.updated_first_line,
                &entry.separator,
                index,
            )
        }) {
            errors.push(format!("恢复 rollout 失败 {}：{error}", path.display()));
        }
    }
}

fn validate_backup_name(name: &str) -> Result<(), Box<dyn Error>> {
    let path = Path::new(name);
    if name.is_empty()
        || name == "."
        || name == ".."
        || path.components().count() != 1
        || !path.is_relative()
    {
        return Err("同步事务中的备份目录名称无效".into());
    }
    Ok(())
}

fn validate_rollout_recovery_path(codex_home: &Path, path: &Path) -> Result<(), Box<dyn Error>> {
    let canonical_home = codex_home.canonicalize()?;
    let canonical_path = path.canonicalize()?;
    let relative = canonical_path
        .strip_prefix(&canonical_home)
        .map_err(|_| "rollout 恢复路径不在 Codex 目录内")?;
    let first = relative.components().next().ok_or("rollout 恢复路径无效")?;
    let allowed = SESSION_DIRS
        .iter()
        .any(|directory| first.as_os_str() == std::ffi::OsStr::new(directory));
    if !allowed {
        return Err("rollout 恢复路径不属于会话目录".into());
    }
    Ok(())
}

fn restore_first_line(
    path: &Path,
    original_first_line: &str,
    updated_first_line: &str,
    separator: &str,
    sequence: usize,
) -> Result<(), Box<dyn Error>> {
    let (current_first_line, current_separator) = read_first_line(path)?;
    if current_first_line == original_first_line && current_separator == separator {
        return Ok(());
    }
    if current_first_line != updated_first_line || current_separator != separator {
        return Err("rollout 在同步中断后又发生了变化".into());
    }
    rewrite_first_line(
        path,
        updated_first_line,
        separator,
        original_first_line,
        sequence,
    )
}

fn collect_rollout_changes(
    codex_home: &Path,
    target_provider: &str,
) -> Result<Vec<RolloutChange>, Box<dyn Error>> {
    let mut paths = Vec::new();
    for directory in SESSION_DIRS {
        collect_rollout_paths(&codex_home.join(directory), &mut paths)?;
    }
    paths.sort();

    let mut changes = Vec::new();
    let mut invalid = Vec::new();
    for path in paths {
        let (first_line, separator) = match read_first_line(&path) {
            Ok(line) => line,
            Err(error) => {
                invalid.push(format!("{}：{error}", path.display()));
                continue;
            }
        };
        let mut record: Value = match serde_json::from_str(&first_line) {
            Ok(record) => record,
            Err(error) => {
                invalid.push(format!("{}：首行 JSON 无效：{error}", path.display()));
                continue;
            }
        };
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            return Err(format!("rollout 首行不是 session_meta：{}", path.display()).into());
        }
        let payload = record
            .get_mut("payload")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| format!("rollout 缺少 session_meta.payload：{}", path.display()))?;
        let original_provider = payload
            .get("model_provider")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("(missing)")
            .to_string();
        if original_provider == target_provider {
            continue;
        }
        payload.insert(
            "model_provider".to_string(),
            Value::String(target_provider.to_string()),
        );
        changes.push(RolloutChange {
            path,
            original_first_line: first_line,
            separator,
            updated_first_line: serde_json::to_string(&record)?,
            original_provider,
        });
    }
    if !invalid.is_empty() {
        return Err(format!(
            "以下会话文件需要先修复，原文件已保留：\n{}",
            invalid.join("\n")
        )
        .into());
    }
    Ok(changes)
}

fn collect_rollout_paths(root: &Path, paths: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_rollout_paths(&entry.path(), paths)?;
        } else if file_type.is_file() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                paths.push(entry.path());
            }
        }
    }
    Ok(())
}

fn read_first_line(path: &Path) -> Result<(String, String), Box<dyn Error>> {
    let file =
        File::open(path).map_err(|error| format!("读取会话失败 {}：{error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    if reader.read_until(b'\n', &mut bytes)? == 0 {
        return Err(format!("rollout 是空文件：{}", path.display()).into());
    }
    let separator = if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
        "\r\n"
    } else if bytes.ends_with(b"\n") {
        bytes.truncate(bytes.len() - 1);
        "\n"
    } else {
        ""
    };
    let first_line = String::from_utf8(bytes)
        .map_err(|error| format!("rollout 首行不是 UTF-8 {}：{error}", path.display()))?;
    Ok((first_line, separator.to_string()))
}

fn rewrite_first_line(
    path: &Path,
    expected_first_line: &str,
    separator: &str,
    replacement_first_line: &str,
    sequence: usize,
) -> Result<(), Box<dyn Error>> {
    let source = File::open(path)?;
    let metadata = source.metadata()?;
    let mut reader = BufReader::new(source);
    let mut current_prefix = Vec::new();
    reader.read_until(b'\n', &mut current_prefix)?;
    let expected_prefix = format!("{expected_first_line}{separator}").into_bytes();
    if current_prefix != expected_prefix {
        return Err(format!("rollout 在同步期间发生变化：{}", path.display()).into());
    }

    let temporary = path.with_extension(format!(
        "jsonl.cswitch-{}-{sequence}.tmp",
        std::process::id()
    ));
    let write_result = (|| -> Result<(), Box<dyn Error>> {
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        output.write_all(replacement_first_line.as_bytes())?;
        output.write_all(separator.as_bytes())?;
        io::copy(&mut reader, &mut output)?;
        let current = reader.get_ref().metadata()?;
        if current.len() != metadata.len() || current.modified()? != metadata.modified()? {
            return Err(
                format!("会话正文在复制期间发生变化，已停止替换：{}", path.display()).into(),
            );
        }
        output.sync_all()?;
        fs::set_permissions(&temporary, metadata.permissions())?;
        drop(output);
        drop(reader);
        replace_file(&temporary, path)?;
        sync_directory(path.parent().ok_or("rollout 文件没有父目录")?)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn resolve_state_db(codex_home: &Path) -> Result<Option<PathBuf>, Box<dyn Error>> {
    Ok(state_databases(codex_home)?.into_iter().next())
}

fn state_databases(codex_home: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut paths = Vec::new();
    let config_path = codex_home.join("config.toml");
    if config_path.exists() {
        let text = fs::read_to_string(&config_path)?;
        let config = crate::config::parse_config(&text)?;
        if let Some(directory) = config.get("sqlite_home").and_then(toml_edit::Item::as_str) {
            let directory = Path::new(directory);
            let path = if directory.is_absolute() {
                directory.to_path_buf()
            } else {
                codex_home.join(directory)
            }
            .join(STATE_DB_NAME);
            if path.exists() {
                if !path.canonicalize()?.starts_with(codex_home.canonicalize()?) {
                    return Err("sqlite_home 指向当前 .codex 目录之外".into());
                }
                paths.push(path);
            }
        }
    }
    for path in [
        codex_home.join(STATE_DB_NAME),
        codex_home.join("sqlite").join(STATE_DB_NAME),
    ] {
        if path.is_file()
            && !paths
                .iter()
                .any(|existing| existing.canonicalize().ok() == path.canonicalize().ok())
        {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn open_state_db(path: &Path) -> Result<Connection, Box<dyn Error>> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(2))?;
    Ok(connection)
}

fn read_sqlite_provider_counts(path: &Path) -> Result<BTreeMap<String, usize>, Box<dyn Error>> {
    let connection = open_state_db(path)?;
    let mut statement = connection.prepare(
        "SELECT CASE WHEN model_provider IS NULL OR model_provider = '' THEN '(missing)' ELSE model_provider END, COUNT(*) FROM threads GROUP BY 1 ORDER BY 1",
    )?;
    let mut counts = BTreeMap::new();
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (provider, count) = row?;
        counts.insert(provider, usize::try_from(count)?);
    }
    Ok(counts)
}

fn is_sqlite_busy(error: &(dyn Error + 'static)) -> bool {
    error
        .downcast_ref::<rusqlite::Error>()
        .is_some_and(|error| {
            matches!(
                error,
                rusqlite::Error::SqliteFailure(code, _)
                    if matches!(code.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
            )
        })
}

fn assert_sqlite_writable(path: &Path) -> Result<(), Box<dyn Error>> {
    let result = (|| -> Result<(), Box<dyn Error>> {
        let connection = open_state_db(path)?;
        connection.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")?;
        Ok(())
    })();
    result.map_err(|error| {
        format!(
            "state_5.sqlite 正在使用或不可写，请关闭 Codex 后重试 {}：{error}",
            path.display()
        )
        .into()
    })
}

fn update_sqlite_provider(path: &Path, target_provider: &str) -> Result<usize, Box<dyn Error>> {
    let mut connection = open_state_db(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let rows = transaction.execute(
        "UPDATE threads SET model_provider = ?1 WHERE COALESCE(model_provider, '') <> ?1",
        params![target_provider],
    )?;
    transaction.commit()?;
    Ok(rows)
}

fn verify_sqlite_provider(path: &Path, target_provider: &str) -> Result<(), Box<dyn Error>> {
    let connection = open_state_db(path)?;
    let remaining: i64 = connection.query_row(
        "SELECT COUNT(*) FROM threads WHERE COALESCE(model_provider, '') <> ?1",
        params![target_provider],
        |row| row.get(0),
    )?;
    if remaining != 0 {
        return Err(
            format!("SQLite 验证失败，仍有 {remaining} 个任务不属于 {target_provider}").into(),
        );
    }
    Ok(())
}

fn merge_provider_counts(
    changes: &[RolloutChange],
    sqlite_counts: &BTreeMap<String, usize>,
) -> Vec<ProviderCount> {
    let mut combined = BTreeMap::<String, (usize, usize)>::new();
    for change in changes {
        combined
            .entry(change.original_provider.clone())
            .or_default()
            .0 += 1;
    }
    for (provider, count) in sqlite_counts {
        combined.entry(provider.clone()).or_default().1 += count;
    }
    combined
        .into_iter()
        .map(
            |(provider, (rollout_files, sqlite_threads))| ProviderCount {
                provider,
                rollout_files,
                sqlite_threads,
            },
        )
        .collect()
}

fn create_backup(
    codex_home: &Path,
    original_config: Option<&[u8]>,
    original_auth: Option<&[u8]>,
    state_db: Option<&Path>,
    changes: &[RolloutChange],
    providers_detected: &[ProviderCount],
    target_provider: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    let backup_root = codex_home.join(BACKUP_DIR);
    fs::create_dir_all(&backup_root).map_err(|error| -> Box<dyn Error> {
        format!("创建备份目录失败 {}：{error}", backup_root.display()).into()
    })?;
    #[cfg(unix)]
    fs::set_permissions(&backup_root, fs::Permissions::from_mode(0o700)).map_err(
        |error| -> Box<dyn Error> {
            format!("设置备份目录权限失败 {}：{error}", backup_root.display()).into()
        },
    )?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let timestamp = Local::now().format("%Y%m%d-%H%M%S-%6f").to_string();
    let name = format!("{timestamp}-{sequence}");
    let backup_dir = backup_root.join(&name);
    let staging_dir = backup_root.join(format!(".{name}-{}.tmp", std::process::id()));
    let result = (|| -> Result<(), Box<dyn Error>> {
        fs::create_dir(&staging_dir).map_err(|error| -> Box<dyn Error> {
            format!("创建临时备份目录失败 {}：{error}", staging_dir.display()).into()
        })?;
        #[cfg(unix)]
        fs::set_permissions(&staging_dir, fs::Permissions::from_mode(0o700)).map_err(
            |error| -> Box<dyn Error> {
                format!(
                    "设置临时备份目录权限失败 {}：{error}",
                    staging_dir.display()
                )
                .into()
            },
        )?;

        if let Some(config) = original_config {
            write_new_file(&staging_dir.join("config.toml"), config)?;
        }
        if let Some(auth) = original_auth {
            write_new_file(&staging_dir.join("auth.json"), auth)?;
        }
        if let Some(path) = state_db {
            let sqlite_backup = staging_dir.join("sqlite").join(STATE_DB_NAME);
            let sqlite_backup_parent = sqlite_backup.parent().expect("sqlite backup parent");
            fs::create_dir_all(sqlite_backup_parent).map_err(|error| -> Box<dyn Error> {
                format!(
                    "创建 SQLite 备份目录失败 {}：{error}",
                    sqlite_backup_parent.display()
                )
                .into()
            })?;
            backup_sqlite(path, &sqlite_backup)?;
        }

        let manifest = BackupManifest {
            version: 2,
            created_at: Local::now().to_rfc3339(),
            target_provider,
            config_present: original_config.is_some(),
            auth_present: original_auth.is_some(),
            sqlite_path: state_db.map(|path| path.to_string_lossy().into_owned()),
            providers_detected,
            rollout_files: changes
                .iter()
                .map(|change| RolloutBackupEntry {
                    path: change.path.to_string_lossy().into_owned(),
                    original_first_line: &change.original_first_line,
                    updated_first_line: &change.updated_first_line,
                    separator: &change.separator,
                    original_provider: &change.original_provider,
                })
                .collect(),
        };
        write_new_file(
            &staging_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?.as_slice(),
        )?;
        sync_directory(&staging_dir)?;
        fs::rename(&staging_dir, &backup_dir)?;
        sync_directory(&backup_root)?;
        Ok(())
    })();
    if result.is_err() && staging_dir.is_dir() {
        let _ = fs::remove_dir_all(&staging_dir);
    }
    result?;
    Ok(backup_dir)
}

fn prune_backups(codex_home: &Path, preserve: &Path) -> Result<(), Box<dyn Error>> {
    let root = codex_home.join(BACKUP_DIR);
    if !root.is_dir() {
        return Ok(());
    }
    let mut completed = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || path == preserve {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            fs::remove_dir_all(&path)?;
            continue;
        }
        completed.push(path);
    }
    completed.sort();
    let remove_count = completed
        .len()
        .saturating_add(1)
        .saturating_sub(MAX_BACKUPS);
    for path in completed.into_iter().take(remove_count) {
        fs::remove_dir_all(path)?;
    }
    sync_directory(&root)
}

fn backup_sqlite(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    let source_connection = open_state_db(source)?;
    let mut destination_connection = Connection::open(destination)?;
    let backup = Backup::new(&source_connection, &mut destination_connection)?;
    backup.run_to_completion(16, Duration::from_millis(20), None)?;
    Ok(())
}

fn restore_extra_databases(
    codex_home: &Path,
    backup_dir: &Path,
    manifest: &RecoveryManifest,
    errors: &mut Vec<String>,
) {
    for (index, destination) in manifest.extra_sqlite_paths.iter().enumerate() {
        let destination = Path::new(destination);
        let result = (|| -> Result<(), Box<dyn Error>> {
            if !destination
                .canonicalize()?
                .starts_with(codex_home.canonicalize()?)
            {
                return Err("额外 SQLite 恢复路径不属于当前 .codex".into());
            }
            restore_sqlite(
                &backup_dir
                    .join("sqlite")
                    .join(format!("extra-{index}.sqlite")),
                destination,
                manifest.target_provider.as_deref(),
            )
        })();
        if let Err(error) = result {
            errors.push(format!(
                "恢复 SQLite 失败 {}：{error}",
                destination.display()
            ));
        }
    }
}

fn restore_sqlite(
    source: &Path,
    destination: &Path,
    expected_provider: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let backup = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut destination = open_state_db(destination)?;
    let transaction = destination.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut statement = backup.prepare("SELECT id, model_provider FROM threads")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })? {
        let (id, provider) = row?;
        if let Some(expected) = expected_provider {
            if provider.as_deref() == Some(expected) {
                continue;
            }
            let current = transaction.query_row(
                "SELECT model_provider FROM threads WHERE id = ?1",
                [&id],
                |row| row.get::<_, Option<String>>(0),
            );
            match current {
                Ok(current) if current == provider => continue,
                Ok(current) if current.as_deref() == Some(expected) => {}
                Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                Ok(_) => {
                    return Err(format!("任务 {id} 在事务中断后被再次修改，保留当前数据库").into());
                }
                Err(error) => return Err(error.into()),
            }
        }
        transaction.execute(
            "UPDATE threads SET model_provider = ?1 WHERE id = ?2",
            params![provider, id],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn rollback(
    config_path: &Path,
    original_config: Option<&[u8]>,
    auth_path: &Path,
    original_auth: Option<&[u8]>,
    backup_dir: &Path,
    state_db: Option<&Path>,
    sqlite_mutated: bool,
    config_written: bool,
    auth_mutated: bool,
    changes: &[RolloutChange],
    applied_rollouts: &[usize],
) -> Vec<String> {
    let mut errors = Vec::new();
    if auth_mutated {
        let result = restore_optional_file(auth_path, original_auth);
        if let Err(error) = result {
            errors.push(format!("恢复 auth.json 失败：{error}"));
        }
    }
    if config_written {
        let result = restore_optional_file(config_path, original_config);
        if let Err(error) = result {
            errors.push(format!("恢复 config.toml 失败：{error}"));
        }
    }
    if sqlite_mutated && let Some(destination) = state_db {
        let source = backup_dir.join("sqlite").join(STATE_DB_NAME);
        let target = fs::read(backup_dir.join("manifest.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RecoveryManifest>(&bytes).ok())
            .and_then(|manifest| manifest.target_provider);
        if let Err(error) = restore_sqlite(&source, destination, target.as_deref()) {
            errors.push(format!("恢复 SQLite 失败：{error}"));
        }
    }
    if sqlite_mutated {
        let manifest = fs::read(backup_dir.join("manifest.json"))
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<RecoveryManifest>(&bytes)
                    .map_err(|error| error.to_string())
            });
        match manifest {
            Ok(manifest) => restore_extra_databases(
                config_path.parent().expect("config parent"),
                backup_dir,
                &manifest,
                &mut errors,
            ),
            Err(error) => errors.push(format!("读取额外 SQLite 恢复清单失败：{error}")),
        }
    }
    for index in applied_rollouts.iter().rev() {
        let change = &changes[*index];
        if let Err(error) = restore_first_line(
            &change.path,
            &change.original_first_line,
            &change.updated_first_line,
            &change.separator,
            *index,
        ) {
            errors.push(format!(
                "恢复 rollout 失败 {}：{error}",
                change.path.display()
            ));
        }
    }
    errors
}

fn restore_optional_file(path: &Path, content: Option<&[u8]>) -> Result<(), Box<dyn Error>> {
    match content {
        Some(content) => atomic_write(path, content),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        },
    }
}

fn verify_applied_rollouts(changes: &[RolloutChange]) -> Result<(), Box<dyn Error>> {
    for change in changes {
        let (first_line, separator) = read_first_line(&change.path)?;
        if first_line != change.updated_first_line || separator != change.separator {
            return Err(format!("rollout 验证失败：{}", change.path.display()).into());
        }
    }
    Ok(())
}

fn write_new_file(path: &Path, content: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(|error| -> Box<dyn Error> {
        format!("创建备份文件失败 {}：{error}", path.display()).into()
    })?;
    file.write_all(content).map_err(|error| -> Box<dyn Error> {
        format!("写入备份文件失败 {}：{error}", path.display()).into()
    })?;
    file.sync_all().map_err(|error| -> Box<dyn Error> {
        format!("同步备份文件失败 {}：{error}", path.display()).into()
    })?;
    Ok(())
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<(), Box<dyn Error>> {
    let parent = path.parent().ok_or("文件没有父目录")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.cswitch-{}-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file"),
        std::process::id(),
        NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<(), Box<dyn Error>> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        replace_file(&temporary, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("写入 {} 失败：{error}", path.display()).into())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), Box<dyn Error>> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<(), Box<dyn Error>> {
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::tempdir;

    fn create_rollout(path: &Path, provider: Option<&str>, body: &str) {
        fs::create_dir_all(path.parent().expect("rollout parent")).expect("create rollout parent");
        let mut payload = serde_json::json!({"id": "thread-id", "cwd": "C:/fixture"});
        if let Some(provider) = provider {
            payload["model_provider"] = Value::String(provider.to_string());
        }
        let header = serde_json::json!({"timestamp": "2026-01-01T00:00:00Z", "type": "session_meta", "payload": payload});
        fs::write(
            path,
            format!(
                "{}\n{body}\n",
                serde_json::to_string(&header).expect("serialize header")
            ),
        )
        .expect("write rollout");
    }

    fn create_state_db(path: &Path, providers: &[Option<&str>]) {
        fs::create_dir_all(path.parent().expect("db parent")).expect("create db parent");
        let connection = Connection::open(path).expect("open fixture db");
        connection.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT, title TEXT, updated_at INTEGER);").expect("create threads");
        for (index, provider) in providers.iter().enumerate() {
            connection.execute(
                "INSERT INTO threads (id, model_provider, title, updated_at) VALUES (?1, ?2, ?3, ?4)",
                params![format!("thread-{index}"), provider, format!("title-{index}"), 1000 + index as i64],
            ).expect("insert thread");
        }
    }

    #[test]
    fn syncs_every_detected_provider_and_preserves_conversation_data() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let first = codex_home.join("sessions/2026/01/rollout-one.jsonl");
        let second = codex_home.join("sessions/2026/01/rollout-two.jsonl");
        let archived = codex_home.join("archived_sessions/rollout-three.jsonl");
        create_rollout(
            &first,
            Some("openai"),
            r#"{"type":"event_msg","payload":{"message":"first"}}"#,
        );
        create_rollout(
            &second,
            Some("cswitch"),
            r#"{"type":"event_msg","payload":{"message":"second"}}"#,
        );
        create_rollout(
            &archived,
            None,
            r#"{"type":"event_msg","payload":{"message":"archived"}}"#,
        );
        let state_db = codex_home.join("sqlite/state_5.sqlite");
        create_state_db(
            &state_db,
            &[Some("openai"), Some("another"), None, Some("custom")],
        );
        let original = b"model = \"gpt-test\"\n";
        fs::write(codex_home.join("config.toml"), original).expect("write config");
        let updated = b"model_provider = \"custom\"\n";

        let report =
            apply_provider_config(codex_home, Some(original), updated, "custom").expect("sync");

        assert_eq!(report.rollout_files_updated, 3);
        assert_eq!(report.sqlite_rows_updated, 3);
        assert!(
            report
                .providers_detected
                .iter()
                .any(|entry| entry.provider == "openai")
        );
        for path in [&first, &second, &archived] {
            let content = fs::read_to_string(path).expect("read rollout");
            let first_line = content.lines().next().expect("first line");
            let record: Value = serde_json::from_str(first_line).expect("parse rollout");
            assert_eq!(record["payload"]["model_provider"], "custom");
        }
        assert!(
            fs::read_to_string(first)
                .expect("read body")
                .contains("first")
        );
        assert!(
            fs::read_to_string(archived)
                .expect("read archived body")
                .contains("archived")
        );
        let connection = Connection::open(state_db).expect("open synced db");
        let remaining: i64 = connection.query_row("SELECT COUNT(*) FROM threads WHERE model_provider <> 'custom' OR model_provider IS NULL", [], |row| row.get(0)).expect("count remaining");
        assert_eq!(remaining, 0);
        let preserved: (String, i64) = connection
            .query_row(
                "SELECT title, updated_at FROM threads WHERE id = 'thread-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read preserved fields");
        assert_eq!(preserved, ("title-1".to_string(), 1001));
        assert!(Path::new(&report.backup_path).join("config.toml").is_file());
        assert!(
            Path::new(&report.backup_path)
                .join("manifest.json")
                .is_file()
        );
        assert!(
            Path::new(&report.backup_path)
                .join("sqlite/state_5.sqlite")
                .is_file()
        );
        assert!(!codex_home.join(TRANSACTION_FILE).exists());
    }

    #[test]
    fn repairs_history_when_live_config_already_matches() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let rollout = codex_home.join("sessions/rollout-already-custom.jsonl");
        create_rollout(
            &rollout,
            Some("openai"),
            r#"{"type":"event_msg","payload":{"message":"leave me"}}"#,
        );
        let state_db = codex_home.join("sqlite/state_5.sqlite");
        create_state_db(&state_db, &[Some("openai")]);
        let original = b"model_provider = \"custom\"\nmodel = \"keep\"\n";
        let updated = b"model_provider = \"custom\"\nmodel = \"next\"\n";
        fs::write(codex_home.join("config.toml"), original).expect("write config");

        let report =
            apply_provider_config(codex_home, Some(original), updated, "custom").expect("sync");

        assert_eq!(report.rollout_files_updated, 1);
        assert_eq!(report.sqlite_rows_updated, 1);
        assert!(
            fs::read_to_string(&rollout)
                .expect("read repaired rollout")
                .contains(r#""model_provider":"custom""#)
        );
        let repaired: i64 = Connection::open(&state_db)
            .expect("open db")
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE model_provider = 'custom'",
                [],
                |row| row.get(0),
            )
            .expect("count custom threads");
        assert_eq!(repaired, 1);
        assert_eq!(
            fs::read(codex_home.join("config.toml")).expect("read config"),
            updated
        );
        assert!(Path::new(&report.backup_path).join("config.toml").is_file());
        assert!(
            Path::new(&report.backup_path)
                .join("sqlite/state_5.sqlite")
                .is_file()
        );
    }

    #[test]
    fn skips_backup_when_config_auth_and_history_are_unchanged() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let original = b"model_provider = \"custom\"\n";
        fs::write(codex_home.join("config.toml"), original).expect("write config");
        create_rollout(
            &codex_home.join("sessions/rollout-skip.jsonl"),
            Some("custom"),
            r#"{"type":"event_msg","payload":{"message":"ignored"}}"#,
        );

        let report =
            apply_provider_config(codex_home, Some(original), original, "custom").expect("noop");

        assert_eq!(report.rollout_files_updated, 0);
        assert_eq!(report.sqlite_rows_updated, 0);
        assert!(report.backup_path.is_empty());
        assert!(!codex_home.join(BACKUP_DIR).exists());
        assert!(!codex_home.join(TRANSACTION_FILE).exists());
    }

    #[test]
    fn prepared_transaction_is_restored_on_next_start() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let rollout = codex_home.join("sessions/rollout-interrupted.jsonl");
        create_rollout(
            &rollout,
            Some("openai"),
            r#"{"type":"event_msg","payload":{"message":"preserved"}}"#,
        );
        let original_rollout = fs::read(&rollout).expect("read rollout");
        let state_db = codex_home.join("sqlite/state_5.sqlite");
        create_state_db(&state_db, &[Some("openai")]);
        let original_config = b"model = \"official\"\n";
        let original_auth = b"{\"tokens\":{}}";
        fs::write(codex_home.join("config.toml"), original_config).expect("write config");
        fs::write(codex_home.join("auth.json"), original_auth).expect("write auth");

        let changes = collect_rollout_changes(codex_home, "custom").expect("collect changes");
        let sqlite_counts = read_sqlite_provider_counts(&state_db).expect("provider counts");
        let providers = merge_provider_counts(&changes, &sqlite_counts);
        let backup = create_backup(
            codex_home,
            Some(original_config),
            Some(original_auth),
            Some(&state_db),
            &changes,
            &providers,
            "custom",
        )
        .expect("create backup");
        begin_transaction(codex_home, &backup).expect("begin transaction");

        rewrite_first_line(
            &rollout,
            &changes[0].original_first_line,
            &changes[0].separator,
            &changes[0].updated_first_line,
            0,
        )
        .expect("mutate rollout");
        update_sqlite_provider(&state_db, "custom").expect("mutate sqlite");
        fs::write(
            codex_home.join("config.toml"),
            b"model_provider = \"custom\"\n",
        )
        .expect("mutate config");
        fs::write(
            codex_home.join("auth.json"),
            b"{\"OPENAI_API_KEY\":\"fixture\"}",
        )
        .expect("mutate auth");

        recover_pending_state(codex_home).expect("recover interrupted transaction");

        assert_eq!(
            fs::read(&rollout).expect("read restored rollout"),
            original_rollout
        );
        assert_eq!(
            fs::read(codex_home.join("config.toml")).expect("read restored config"),
            original_config
        );
        assert_eq!(
            fs::read(codex_home.join("auth.json")).expect("read restored auth"),
            original_auth
        );
        let connection = Connection::open(state_db).expect("open restored sqlite");
        let provider: String = connection
            .query_row(
                "SELECT model_provider FROM threads WHERE id = 'thread-0'",
                [],
                |row| row.get(0),
            )
            .expect("read restored provider");
        assert_eq!(provider, "openai");
        assert!(!codex_home.join(TRANSACTION_FILE).exists());
    }

    #[test]
    fn committed_transaction_is_kept_when_cleanup_was_interrupted() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let original = b"model = \"official\"\n";
        fs::write(codex_home.join("config.toml"), original).expect("write config");
        let backup = create_backup(codex_home, Some(original), None, None, &[], &[], "custom")
            .expect("create backup");
        let mut journal = begin_transaction(codex_home, &backup).expect("begin transaction");
        fs::write(
            codex_home.join("config.toml"),
            b"model_provider = \"custom\"\n",
        )
        .expect("write committed config");
        journal.phase = TransactionPhase::Committed;
        write_transaction_journal(codex_home, &journal).expect("mark committed");

        recover_pending_state(codex_home).expect("finish committed transaction");

        assert_eq!(
            fs::read(codex_home.join("config.toml")).expect("read active config"),
            b"model_provider = \"custom\"\n"
        );
        assert!(!codex_home.join(TRANSACTION_FILE).exists());
    }

    #[test]
    fn backup_retention_keeps_only_the_ten_newest_directories() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let root = codex_home.join(BACKUP_DIR);
        fs::create_dir_all(&root).expect("create backup root");
        for index in 0..12 {
            fs::create_dir(root.join(format!("20260101-000000-{index:02}")))
                .expect("create old backup");
        }
        let preserve = root.join("20260101-000000-12");
        fs::create_dir(&preserve).expect("create current backup");

        prune_backups(codex_home, &preserve).expect("prune backups");

        let remaining = fs::read_dir(root).expect("read backups").count();
        assert_eq!(remaining, MAX_BACKUPS);
        assert!(preserve.is_dir());
        assert!(
            !codex_home
                .join(BACKUP_DIR)
                .join("20260101-000000-00")
                .exists()
        );
        assert!(
            codex_home
                .join(BACKUP_DIR)
                .join("20260101-000000-03")
                .is_dir()
        );
    }

    #[test]
    fn locked_sqlite_stops_before_any_file_is_changed() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let rollout = codex_home.join("sessions/rollout-locked.jsonl");
        create_rollout(
            &rollout,
            Some("openai"),
            r#"{"type":"event_msg","payload":{"message":"unchanged"}}"#,
        );
        let original_rollout = fs::read(&rollout).expect("read original rollout");
        let state_db = codex_home.join("sqlite/state_5.sqlite");
        create_state_db(&state_db, &[Some("openai")]);
        let lock = Connection::open(&state_db).expect("open lock connection");
        lock.execute_batch("BEGIN EXCLUSIVE")
            .expect("lock database");

        let error =
            apply_provider_config(codex_home, None, b"model_provider = \"custom\"\n", "custom")
                .expect_err("sync should fail");

        assert!(error.to_string().contains("关闭 Codex"), "{error}");
        assert_eq!(
            fs::read(rollout).expect("read rollout after failure"),
            original_rollout
        );
        assert!(!codex_home.join("config.toml").exists());
        lock.execute_batch("ROLLBACK").expect("unlock database");
    }

    #[test]
    fn both_layouts_are_kept_consistent_when_task_counts_differ() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let sqlite_directory_db = codex_home.join("sqlite/state_5.sqlite");
        let legacy_db = codex_home.join("state_5.sqlite");
        create_state_db(&sqlite_directory_db, &[Some("openai")]);
        create_state_db(
            &legacy_db,
            &[Some("openai"), Some("openai"), Some("openai")],
        );
        create_rollout(
            &codex_home.join("sessions/rollout-one.jsonl"),
            Some("openai"),
            r#"{"type":"event_msg","payload":{"message":"one"}}"#,
        );
        create_rollout(
            &codex_home.join("sessions/rollout-two.jsonl"),
            Some("openai"),
            r#"{"type":"event_msg","payload":{"message":"two"}}"#,
        );
        create_rollout(
            &codex_home.join("archived_sessions/rollout-three.jsonl"),
            Some("openai"),
            r#"{"type":"event_msg","payload":{"message":"three"}}"#,
        );

        // Task counts/IDs can differ; both databases must still move together.
        Connection::open(&legacy_db)
            .unwrap()
            .execute(
                "UPDATE threads SET id = 'thread-id' WHERE id = 'thread-0'",
                [],
            )
            .unwrap();
        assert_eq!(
            resolve_state_db(codex_home).expect("resolve state db"),
            Some(legacy_db.clone())
        );
        apply_provider_config(codex_home, None, b"model_provider = \"custom\"\n", "custom")
            .expect("sync selected database");

        let stale_provider: String = Connection::open(&sqlite_directory_db)
            .expect("open stale db")
            .query_row("SELECT model_provider FROM threads LIMIT 1", [], |row| {
                row.get(0)
            })
            .expect("read stale provider");
        let active_remaining: i64 = Connection::open(&legacy_db)
            .expect("open active db")
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE model_provider <> 'custom'",
                [],
                |row| row.get(0),
            )
            .expect("read active providers");
        assert_eq!(stale_provider, "custom");
        assert_eq!(active_remaining, 0);
    }

    #[test]
    fn final_config_failure_restores_rollout_and_sqlite() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let rollout = codex_home.join("sessions/rollout-rollback.jsonl");
        create_rollout(
            &rollout,
            Some("openai"),
            r#"{"type":"event_msg","payload":{"message":"keep me"}}"#,
        );
        let original_rollout = fs::read(&rollout).expect("read original rollout");
        let state_db = codex_home.join("sqlite/state_5.sqlite");
        create_state_db(&state_db, &[Some("openai")]);
        let original_config = b"model = \"gpt-test\"\n";
        let config_path = codex_home.join("config.toml");
        fs::write(&config_path, original_config).expect("write original config");

        let error = apply_provider_config_with_hook(
            codex_home,
            Some(original_config),
            b"model_provider = \"custom\"\n",
            "custom",
            AuthUpdate::Keep,
            || Err("injected config write failure".into()),
        )
        .expect_err("config write should fail");

        assert!(error.to_string().contains("所有改动已恢复"), "{error}");
        assert_eq!(
            fs::read(&rollout).expect("read restored rollout"),
            original_rollout
        );
        assert_eq!(
            fs::read(&config_path).expect("read restored config"),
            original_config
        );
        let connection = Connection::open(state_db).expect("open restored db");
        let provider: String = connection
            .query_row(
                "SELECT model_provider FROM threads WHERE id = 'thread-0'",
                [],
                |row| row.get(0),
            )
            .expect("read restored provider");
        assert_eq!(provider, "openai");
    }

    #[test]
    fn concurrent_sync_is_rejected_before_mutation() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let rollout = codex_home.join("sessions/rollout-concurrent.jsonl");
        create_rollout(
            &rollout,
            Some("openai"),
            r#"{"type":"event_msg","payload":{"message":"unchanged"}}"#,
        );
        let original_rollout = fs::read(&rollout).expect("read original rollout");
        let lock = acquire_operation_lock(codex_home).expect("acquire first lock");

        let error =
            apply_provider_config(codex_home, None, b"model_provider = \"custom\"\n", "custom")
                .expect_err("second sync should fail");

        assert!(error.to_string().contains("另一个 CSwitch"), "{error}");
        assert_eq!(
            fs::read(rollout).expect("read rollout after rejection"),
            original_rollout
        );
        assert!(!codex_home.join("config.toml").exists());
        drop(lock);
    }

    #[test]
    fn legacy_sync_lock_blocks_cswitch_before_mutation() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let rollout = codex_home.join("sessions/rollout-legacy-lock.jsonl");
        create_rollout(
            &rollout,
            Some("openai"),
            r#"{"type":"event_msg","payload":{"message":"unchanged"}}"#,
        );
        let original_rollout = fs::read(&rollout).expect("read original rollout");
        let legacy_lock =
            acquire_sync_lock_file(&codex_home.join("qpp-sync.lock")).expect("legacy sync lock");

        let error =
            apply_provider_config(codex_home, None, b"model_provider = \"custom\"\n", "custom")
                .expect_err("legacy sync should block CSwitch");

        assert!(error.to_string().contains("CSwitch"), "{error}");
        assert_eq!(
            fs::read(rollout).expect("read rollout after rejection"),
            original_rollout
        );
        assert!(!codex_home.join("config.toml").exists());
        drop(legacy_lock);
    }

    #[test]
    fn migrates_legacy_backup_and_recovers_legacy_transaction_once() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        let original = b"model = \"official\"\n";
        let changed = b"model_provider = \"custom\"\n";
        let config_path = codex_home.join("config.toml");
        fs::write(&config_path, original).expect("write original config");
        let backup = create_backup(codex_home, Some(original), None, None, &[], &[], "custom")
            .expect("create backup");
        begin_transaction(codex_home, &backup).expect("begin transaction");
        fs::write(&config_path, changed).expect("write interrupted config");
        fs::rename(
            codex_home.join(BACKUP_DIR),
            codex_home.join(LEGACY_BACKUP_DIR),
        )
        .expect("rename backup root to legacy name");
        fs::rename(
            codex_home.join(TRANSACTION_FILE),
            codex_home.join(LEGACY_TRANSACTION_FILE),
        )
        .expect("rename transaction to legacy name");

        recover_pending_state(codex_home).expect("migrate and recover legacy transaction");
        recover_pending_state(codex_home).expect("repeat recovery");

        assert_eq!(
            fs::read(config_path).expect("read restored config"),
            original
        );
        assert!(codex_home.join(BACKUP_DIR).is_dir());
        assert!(!codex_home.join(LEGACY_BACKUP_DIR).exists());
        assert!(!codex_home.join(TRANSACTION_FILE).exists());
        assert!(!codex_home.join(LEGACY_TRANSACTION_FILE).exists());
    }

    #[test]
    fn conflicting_legacy_and_cswitch_transactions_are_reported() {
        let directory = tempdir().expect("tempdir");
        let codex_home = directory.path();
        fs::write(codex_home.join(TRANSACTION_FILE), b"{}").expect("write current transaction");
        fs::write(codex_home.join(LEGACY_TRANSACTION_FILE), b"{}")
            .expect("write legacy transaction");

        let error = recover_pending_state(codex_home).expect_err("transaction conflict");

        assert!(error.to_string().contains("同时存在旧事务"), "{error}");
        assert!(codex_home.join(TRANSACTION_FILE).is_file());
        assert!(codex_home.join(LEGACY_TRANSACTION_FILE).is_file());
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use tempfile::tempdir;
    #[test]
    fn database_recovery_preserves_new_tasks_and_user_fields() {
        let home = tempdir().unwrap();
        let source = home.path().join("backup.sqlite");
        let target = home.path().join("state.sqlite");
        for path in [&source, &target] {
            Connection::open(path).unwrap().execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,model_provider TEXT,title TEXT); INSERT INTO threads VALUES('old','openai','before');").unwrap();
        }
        Connection::open(&target).unwrap().execute_batch("UPDATE threads SET model_provider='custom',title='edited';INSERT INTO threads VALUES('new','custom','new task');").unwrap();
        restore_sqlite(&source, &target, Some("custom")).unwrap();
        let db = Connection::open(target).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM threads", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.query_row("SELECT title FROM threads WHERE id='old'", [], |row| row
                .get::<_, String>(
                0
            ))
            .unwrap(),
            "edited"
        );
        assert_eq!(
            db.query_row(
                "SELECT model_provider FROM threads WHERE id='old'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "openai"
        );
    }
    #[test]
    fn both_database_layouts_are_synchronized_without_guessing() {
        let home = tempdir().unwrap();
        fs::create_dir(home.path().join("sqlite")).unwrap();
        for path in [
            home.path().join("state_5.sqlite"),
            home.path().join("sqlite/state_5.sqlite"),
        ] {
            Connection::open(path)
                .unwrap()
                .execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,model_provider TEXT);")
                .unwrap();
        }
        for path in state_databases(home.path()).unwrap() {
            Connection::open(path)
                .unwrap()
                .execute("INSERT INTO threads VALUES ('same','openai')", [])
                .unwrap();
        }
        let report = apply_provider_state(
            home.path(),
            None,
            b"model_provider='custom'\n",
            "custom",
            AuthUpdate::Keep,
        )
        .unwrap();
        assert_eq!(report.sqlite_rows_updated, 2);
        for path in state_databases(home.path()).unwrap() {
            verify_sqlite_provider(&path, "custom").unwrap();
        }
    }
    #[test]
    fn settings_recover_with_prepared_transaction() {
        let home = tempdir().unwrap();
        let root = home.path();
        fs::create_dir(root.join("cswitch-profiles")).unwrap();
        let original = b"{\"keepOfficialAuth\":false}";
        fs::write(root.join("cswitch-profiles/settings.json"), original).unwrap();
        let backup = create_backup(root, None, None, None, &[], &[], "custom").unwrap();
        fs::write(backup.join("settings.json"), original).unwrap();
        let path = backup.join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["settingsManaged"] = serde_json::json!(true);
        manifest["settingsPresent"] = serde_json::json!(true);
        fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        begin_transaction(root, &backup).unwrap();
        fs::write(
            root.join("cswitch-profiles/settings.json"),
            b"{\"keepOfficialAuth\":true}",
        )
        .unwrap();
        recover_pending_state(root).unwrap();
        assert_eq!(
            fs::read(root.join("cswitch-profiles/settings.json")).unwrap(),
            original
        );
    }
}

#[cfg(test)]
mod multi_database_regression_tests {
    use super::*;
    #[test]
    fn concurrent_file_edits_are_preserved_and_history_rolls_back() {
        for unchanged in [false, true] {
            for edited in ["config.toml", "auth.json", "cswitch-profiles/settings.json"] {
                let home = tempfile::tempdir().unwrap();
                let root = home.path();
                fs::create_dir(root.join("cswitch-profiles")).unwrap();
                fs::write(root.join("config.toml"), b"model='old'\n").unwrap();
                fs::write(root.join("auth.json"), b"old-auth").unwrap();
                fs::write(root.join("cswitch-profiles/settings.json"), b"old-settings").unwrap();
                Connection::open(root.join(STATE_DB_NAME)).unwrap().execute_batch(
                "CREATE TABLE threads(id TEXT PRIMARY KEY, model_provider TEXT); INSERT INTO threads VALUES('thread','openai');").unwrap();
                let result = apply_provider_config_with_settings_and_hook(
                    root,
                    Some(b"model='old'\n"),
                    if unchanged {
                        b"model='old'\n"
                    } else {
                        b"model='new'\n"
                    },
                    "custom",
                    AuthUpdate::Replace(if unchanged { b"old-auth" } else { b"new-auth" }),
                    Some(if unchanged {
                        b"old-settings"
                    } else {
                        b"new-settings"
                    }),
                    || {
                        fs::write(root.join(edited), b"# concurrent-user-edit")?;
                        Ok(())
                    },
                );
                assert!(result.is_err(), "overwrote {edited}");
                assert_eq!(
                    fs::read(root.join(edited)).unwrap(),
                    b"# concurrent-user-edit"
                );
                let provider: String = Connection::open(root.join(STATE_DB_NAME))
                    .unwrap()
                    .query_row("SELECT model_provider FROM threads", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(provider, "openai");
                assert!(!root.join(TRANSACTION_FILE).exists());
            }
        }
    }
    #[test]
    fn recovery_preserves_settings_modified_after_interruption() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        fs::create_dir(root.join("cswitch-profiles")).unwrap();
        let old_config = b"model='old'\n";
        let new_config = b"model='new'\n";
        let settings_path = root.join("cswitch-profiles/settings.json");
        let external_settings = br#"{"keepOfficialAuth":true,"userEdit":"keep"}"#;
        let backup = create_backup(root, Some(old_config), None, None, &[], &[], "custom").unwrap();
        fs::write(backup.join("config.applied.toml"), new_config).unwrap();
        fs::write(
            backup.join("settings.json"),
            br#"{"keepOfficialAuth":false}"#,
        )
        .unwrap();
        fs::write(
            backup.join("settings.applied.json"),
            br#"{"keepOfficialAuth":true}"#,
        )
        .unwrap();
        let path = backup.join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["settingsManaged"] = true.into();
        manifest["settingsPresent"] = true.into();
        manifest["checkedFiles"] = true.into();
        fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        begin_transaction(root, &backup).unwrap();
        fs::write(root.join("config.toml"), new_config).unwrap();
        fs::write(&settings_path, external_settings).unwrap();
        assert!(recover_pending_state(root).is_err());
        assert_eq!(fs::read(settings_path).unwrap(), external_settings);
        assert_eq!(fs::read(root.join("config.toml")).unwrap(), new_config);
        assert!(root.join(TRANSACTION_FILE).exists());
    }
    #[test]
    fn both_databases_roll_back_when_final_config_write_fails() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        fs::create_dir(root.join("sqlite")).unwrap();
        let paths = [
            root.join("state_5.sqlite"),
            root.join("sqlite/state_5.sqlite"),
        ];
        for (i, path) in paths.iter().enumerate() {
            let db = Connection::open(path).unwrap();
            db.execute_batch(
                "CREATE TABLE threads(id TEXT PRIMARY KEY,model_provider TEXT,title TEXT);",
            )
            .unwrap();
            db.execute(
                "INSERT INTO threads VALUES('thread',?1,'keep')",
                [format!("provider-{i}")],
            )
            .unwrap();
        }
        assert!(
            apply_provider_config_with_hook(
                root,
                None,
                b"model_provider='custom'\n",
                "custom",
                AuthUpdate::Keep,
                || Err("injected final write failure".into())
            )
            .is_err()
        );
        for (i, path) in paths.iter().enumerate() {
            let provider: String = Connection::open(path)
                .unwrap()
                .query_row("SELECT model_provider FROM threads", [], |row| row.get(0))
                .unwrap();
            assert_eq!(provider, format!("provider-{i}"));
        }
        assert!(!root.join("config.toml").exists());
    }
}
