use directories::UserDirs;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use tauri::{AppHandle, Emitter};

use crate::app::{
    ProviderState, SavedProvider, activate_provider_inner_with_progress, delete_provider_inner,
    enable_provider_routing_inner, ensure_provider_migration, list_provider_state,
    save_provider_inner, set_keep_official_auth_with_progress, switch_to_official_with_progress,
};
use crate::progress::{self, ProgressReporter};
use crate::{oauth, operation_lock, provider_sync, tray};
use provider_sync::ProviderSyncReport;

static APP_OPERATION: Mutex<()> = Mutex::new(());

pub(crate) fn operation_in_progress() -> bool {
    APP_OPERATION.try_lock().is_err()
}

pub(crate) fn run() -> Result<(), Box<dyn Error>> {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            tray::show_main_window(app)
        }))
        .invoke_handler(tauri::generate_handler![
            list_providers,
            refresh_provider_models,
            save_provider,
            enable_provider_routing,
            activate_provider,
            delete_provider,
            set_keep_official_auth,
            start_official_login,
            add_official_account,
            activate_official_account,
            cancel_official_login
        ])
        .setup(|app| {
            tray::setup(app.handle()).map_err(|error| error.to_string())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if tray::allow_exit() {
                    return;
                }
                api.prevent_close();
                oauth::cancel_login();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())?;
    Ok(())
}

pub(crate) fn handle_tray_menu(app: &AppHandle, id: &str) {
    match id {
        "show" => tray::show_main_window(app),
        "quit" => tray::request_exit(app),
        "official" => {
            tray::show_main_window(app);
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) =
                    start_official_login_task(handle.clone(), OfficialAction::Default).await
                {
                    report_tray_error(&handle, &error);
                }
            });
        }
        other => {
            if let Some(account_id) = other.strip_prefix("account:") {
                tray::show_main_window(app);
                let handle = app.clone();
                let id = account_id.to_string();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) =
                        start_official_login_task(handle.clone(), OfficialAction::Select(id)).await
                    {
                        report_tray_error(&handle, &error);
                    }
                });
            } else if let Some(provider_id) = other.strip_prefix("provider:") {
                tray::show_main_window(app);
                let handle = app.clone();
                let provider_id = provider_id.to_string();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = activate_provider_task(handle.clone(), provider_id).await {
                        report_tray_error(&handle, &error);
                    }
                });
            }
        }
    }
}

#[tauri::command]
async fn list_providers() -> Result<ProviderState, String> {
    tauri::async_runtime::spawn_blocking(list_providers_inner)
        .await
        .map_err(|error| error.to_string())?
}

fn list_providers_inner() -> Result<ProviderState, String> {
    let codex_home = resolve_codex_home().map_err(resolve_home_error)?;
    let _guard = acquire_app_operation()
        .map_err(|error| operation_error(&codex_home, "获取应用操作锁", error))?;
    let _process_guard = operation_lock::acquire(&codex_home)
        .map_err(|error| operation_error(&codex_home, "获取跨进程操作锁", error))?;
    provider_sync::recover_pending_state(&codex_home)
        .map_err(|error| operation_error(&codex_home, "恢复上次未完成的操作", error))?;
    let mut state = list_provider_state(&codex_home)
        .map_err(|error| operation_error(&codex_home, "读取供应商列表", error))?;
    if let Err(error) = crate::gateway::restore_resident(&codex_home) {
        state
            .warnings
            .push(operation_error(&codex_home, "恢复常驻路由", error));
    }
    Ok(state)
}

#[tauri::command]
async fn save_provider(
    app: AppHandle,
    provider_id: Option<String>,
    name: String,
    api_url: String,
    api_key: String,
) -> Result<SavedProvider, String> {
    let progress = reporter(&app, "save", "保存供应商");
    let codex_home = resolve_codex_home().map_err(resolve_home_error)?;
    let task_home = codex_home.clone();
    let task_progress = progress.clone();
    let started = Arc::new(AtomicBool::new(false));
    let task_started = started.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _guard = acquire_app_operation()
            .map_err(|error| operation_error(&task_home, "获取应用操作锁", error))?;
        task_started.store(true, Ordering::SeqCst);
        task_progress.stage(1, 2, "验证供应商", "正在探测协议并读取模型列表。");
        let _process_guard = operation_lock::acquire(&task_home)
            .map_err(|error| operation_error(&task_home, "获取跨进程操作锁", error))?;
        provider_sync::recover_pending_state(&task_home)
            .map_err(|error| operation_error(&task_home, "恢复上次未完成的操作", error))?;
        save_provider_inner(
            &task_home,
            provider_id.as_deref(),
            &name,
            &api_url,
            &api_key,
        )
        .map_err(|error| operation_error(&task_home, "验证并保存供应商", error))
    })
    .await
    .map_err(|error| operation_error(&codex_home, "等待 API 配置任务", error))?;
    match &result {
        Ok(_) => {
            progress.finish(2);
            notify_providers_changed(&app);
        }
        Err(_) if started.load(Ordering::SeqCst) => fail_operation(&app, "save"),
        Err(_) => {}
    }
    result
}

#[tauri::command]
async fn refresh_provider_models(
    app: AppHandle,
    provider_id: String,
) -> Result<ProviderState, String> {
    let home = resolve_codex_home().map_err(resolve_home_error)?;
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _guard = acquire_app_operation()
            .map_err(|error| operation_error(&home, "获取应用操作锁", error))?;
        let _process_guard = operation_lock::acquire(&home)
            .map_err(|error| operation_error(&home, "获取跨进程操作锁", error))?;
        crate::app::refresh_provider_models_inner(&home, &provider_id)
            .map_err(|error| operation_error(&home, "刷新模型列表", error))
    })
    .await
    .map_err(|error| error.to_string())?;
    if result.is_ok() {
        notify_providers_changed(&app);
    }
    result
}

#[tauri::command]
async fn activate_provider(
    app: AppHandle,
    provider_id: String,
) -> Result<ProviderSyncReport, String> {
    activate_provider_task(app, provider_id).await
}

#[tauri::command]
fn enable_provider_routing(app: AppHandle, provider_id: String) -> Result<(), String> {
    let codex_home = resolve_codex_home().map_err(resolve_home_error)?;
    let _guard = acquire_app_operation()
        .map_err(|error| operation_error(&codex_home, "获取应用操作锁", error))?;
    let _process_guard = operation_lock::acquire(&codex_home)
        .map_err(|error| operation_error(&codex_home, "获取跨进程操作锁", error))?;
    provider_sync::recover_pending_state(&codex_home)
        .map_err(|error| operation_error(&codex_home, "恢复上次未完成的操作", error))?;
    enable_provider_routing_inner(&codex_home, &provider_id)
        .map_err(|error| operation_error(&codex_home, "启用本地路由", error))?;
    notify_providers_changed(&app);
    Ok(())
}

#[tauri::command]
async fn set_keep_official_auth(app: AppHandle, enabled: bool) -> Result<ProviderState, String> {
    let title = if enabled {
        "开启保留官方登录"
    } else {
        "关闭保留官方登录"
    };
    let progress = reporter(&app, "keep-auth", title);
    let codex_home = resolve_codex_home().map_err(resolve_home_error)?;
    let task_home = codex_home.clone();
    let started = Arc::new(AtomicBool::new(false));
    let task_started = started.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _guard = acquire_app_operation()
            .map_err(|error| operation_error(&task_home, "获取应用操作锁", error))?;
        task_started.store(true, Ordering::SeqCst);
        let _process_guard = operation_lock::acquire(&task_home)
            .map_err(|error| operation_error(&task_home, "获取跨进程操作锁", error))?;
        provider_sync::recover_pending_state(&task_home)
            .map_err(|error| operation_error(&task_home, "恢复上次未完成的操作", error))?;
        set_keep_official_auth_with_progress(&task_home, enabled, &progress)
            .map_err(|error| operation_error(&task_home, "更新官方登录保留设置", error))
    })
    .await
    .map_err(|error| operation_error(&codex_home, "等待官方登录保留设置任务", error))?;
    match &result {
        Ok(_) => tray::refresh(&app),
        Err(_) if started.load(Ordering::SeqCst) => fail_operation(&app, "keep-auth"),
        Err(_) => {}
    }
    result
}

#[tauri::command]
fn delete_provider(app: AppHandle, provider_id: String) -> Result<(), String> {
    let codex_home = resolve_codex_home().map_err(resolve_home_error)?;
    let _guard = acquire_app_operation()
        .map_err(|error| operation_error(&codex_home, "获取应用操作锁", error))?;
    let _process_guard = operation_lock::acquire(&codex_home)
        .map_err(|error| operation_error(&codex_home, "获取跨进程操作锁", error))?;
    provider_sync::recover_pending_state(&codex_home)
        .map_err(|error| operation_error(&codex_home, "恢复上次未完成的操作", error))?;
    delete_provider_inner(&codex_home, &provider_id)
        .map_err(|error| operation_error(&codex_home, "删除供应商", error))?;
    notify_providers_changed(&app);
    Ok(())
}

#[tauri::command]
async fn start_official_login(app: AppHandle) -> Result<ProviderSyncReport, String> {
    start_official_login_task(app, OfficialAction::Default).await
}

#[tauri::command]
fn cancel_official_login() {
    oauth::cancel_login();
}

async fn activate_provider_task(
    app: AppHandle,
    provider_id: String,
) -> Result<ProviderSyncReport, String> {
    let codex_home = resolve_codex_home().map_err(resolve_home_error)?;
    let task_home = codex_home.clone();
    let handle = app.clone();
    let started = Arc::new(AtomicBool::new(false));
    let task_started = started.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _guard = acquire_app_operation()
            .map_err(|error| operation_error(&task_home, "获取应用操作锁", error))?;
        task_started.store(true, Ordering::SeqCst);
        let _process_guard = operation_lock::acquire(&task_home)
            .map_err(|error| operation_error(&task_home, "获取跨进程操作锁", error))?;
        provider_sync::recover_pending_state(&task_home)
            .map_err(|error| operation_error(&task_home, "恢复上次未完成的操作", error))?;
        let title = list_provider_state(&task_home)
            .ok()
            .and_then(|state| {
                state
                    .providers
                    .into_iter()
                    .find(|provider| provider.id == provider_id)
            })
            .map(|provider| format!("切换到 {}", provider.name))
            .unwrap_or_else(|| "切换供应商".to_string());
        let progress = reporter(&handle, "activate", &title);
        activate_provider_inner_with_progress(
            &task_home,
            &provider_id,
            crate::codex_process::close_if_running,
            &progress,
        )
        .map_err(|error| operation_error(&task_home, "切换供应商", error))
    })
    .await
    .map_err(|error| operation_error(&codex_home, "等待供应商切换任务", error))?;
    match &result {
        Ok(_) => notify_providers_changed(&app),
        Err(_) if started.load(Ordering::SeqCst) => fail_operation(&app, "activate"),
        Err(_) => {}
    }
    result
}

enum OfficialAction {
    Default,
    Add,
    Select(String),
}

#[tauri::command]
async fn add_official_account(app: AppHandle) -> Result<ProviderSyncReport, String> {
    start_official_login_task(app, OfficialAction::Add).await
}

#[tauri::command]
async fn activate_official_account(
    app: AppHandle,
    account_id: String,
) -> Result<ProviderSyncReport, String> {
    start_official_login_task(app, OfficialAction::Select(account_id)).await
}

async fn start_official_login_task(
    app: AppHandle,
    action: OfficialAction,
) -> Result<ProviderSyncReport, String> {
    let title = if matches!(action, OfficialAction::Add) {
        "添加官方账号"
    } else {
        "切换官方账号"
    };
    let progress = reporter(&app, "official", title);
    let codex_home = resolve_codex_home().map_err(resolve_home_error)?;
    let attempt = oauth::begin_login()
        .map_err(|error| operation_error(&codex_home, "开始官方登录", error))?;
    let task_home = codex_home.clone();
    let started = Arc::new(AtomicBool::new(false));
    let task_started = started.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _attempt = attempt;
        let _guard = acquire_app_operation()
            .map_err(|error| operation_error(&task_home, "获取应用操作锁", error))?;
        task_started.store(true, Ordering::SeqCst);
        let _process_guard = operation_lock::acquire(&task_home)
            .map_err(|error| operation_error(&task_home, "获取跨进程操作锁", error))?;
        provider_sync::recover_pending_state(&task_home)
            .map_err(|error| operation_error(&task_home, "恢复上次未完成的操作", error))?;
        ensure_provider_migration(&task_home)
            .map_err(|error| operation_error(&task_home, "迁移已有供应商", error))?;
        match action {
            OfficialAction::Default => switch_to_official_with_progress(&task_home, &progress),
            OfficialAction::Add => crate::app::add_official_account(&task_home, &progress),
            OfficialAction::Select(id) => {
                crate::app::switch_official_account(&task_home, &id, &progress)
            }
        }
        .map_err(|error| operation_error(&task_home, title, error))
    })
    .await
    .map_err(|error| operation_error(&codex_home, "等待官方登录任务", error))?;
    match &result {
        Ok(_) => notify_providers_changed(&app),
        Err(_) if started.load(Ordering::SeqCst) => fail_operation(&app, "official"),
        Err(_) => {}
    }
    result
}

fn reporter(app: &AppHandle, operation: &str, title: &str) -> ProgressReporter {
    let handle = app.clone();
    ProgressReporter::for_operation(operation, title, move |payload| {
        let _ = handle.emit(progress::EVENT, payload);
    })
}

fn notify_providers_changed(app: &AppHandle) {
    tray::refresh(app);
    let _ = app.emit(progress::PROVIDERS_CHANGED_EVENT, ());
}

fn fail_operation(app: &AppHandle, operation: &str) {
    let _ = app.emit(
        progress::EVENT,
        progress::OperationProgress {
            operation: operation.to_string(),
            title: String::new(),
            stage: "失败".to_string(),
            detail: String::new(),
            current: 1,
            total: 1,
            done: true,
        },
    );
}

fn report_tray_error(app: &AppHandle, error: &str) {
    if error != "官方登录已取消" {
        let _ = app.emit(progress::OPERATION_ERROR_EVENT, error);
    }
}

fn acquire_app_operation() -> Result<MutexGuard<'static, ()>, Box<dyn Error>> {
    match APP_OPERATION.try_lock() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::WouldBlock) => Err("另一个 CSwitch 操作正在进行".into()),
        Err(TryLockError::Poisoned(_)) => Err("CSwitch 操作锁状态异常，请重启程序".into()),
    }
}

fn resolve_home_error(error: impl std::fmt::Display) -> String {
    format!("失败阶段：定位 Codex 目录\n原因：{error}")
}

pub(crate) fn operation_error(
    codex_home: &Path,
    stage: &str,
    error: impl std::fmt::Display,
) -> String {
    let reason = error.to_string();
    if reason == "官方登录已取消" {
        return reason;
    }
    format!(
        "Codex 目录：{}\n失败阶段：{stage}\n原因：{reason}",
        codex_home.display()
    )
}

pub(crate) fn resolve_codex_home() -> Result<PathBuf, Box<dyn Error>> {
    let user_dirs = UserDirs::new().ok_or("未找到用户主目录")?;
    Ok(user_dirs.home_dir().join(".codex"))
}
