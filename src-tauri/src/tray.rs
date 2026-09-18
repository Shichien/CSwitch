use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::app::list_provider_state_read_only as list_provider_state;
use crate::desktop::resolve_codex_home;

pub const TRAY_ID: &str = "cswitch";
static ALLOW_EXIT: AtomicBool = AtomicBool::new(false);

pub fn allow_exit() -> bool {
    ALLOW_EXIT.load(Ordering::SeqCst)
}

pub fn request_exit(app: &AppHandle) {
    crate::oauth::cancel_login();
    if crate::desktop::operation_in_progress() {
        show_main_window(app);
        use tauri::Emitter;
        let _ = app.emit(
            crate::progress::OPERATION_ERROR_EVENT,
            "正在结束当前操作，请等待完成后再退出",
        );
        return;
    }
    ALLOW_EXIT.store(true, Ordering::SeqCst);
    app.exit(0);
}

pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn setup(app: &AppHandle) -> Result<(), Box<dyn Error>> {
    let menu = build_menu(app)?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("CSwitch")
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| crate::desktop::handle_tray_menu(app, event.id.as_ref()));
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    refresh(app);
    Ok(())
}

pub fn refresh(app: &AppHandle) {
    let Ok(menu) = build_menu(app) else {
        return;
    };
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let _ = tray.set_menu(Some(menu));
    let _ = tray.set_tooltip(Some(tooltip()));
}

fn tooltip() -> String {
    let Ok(home) = resolve_codex_home() else {
        return "CSwitch".to_string();
    };
    let Ok(state) = list_provider_state(&home) else {
        return "CSwitch".to_string();
    };
    if state.official_active {
        return "CSwitch · 官方登录".to_string();
    }
    if let Some(active) = state.providers.iter().find(|provider| provider.active) {
        return format!("CSwitch · {}", active.name);
    }
    "CSwitch".to_string()
}

fn build_menu(app: &AppHandle) -> Result<Menu<tauri::Wry>, Box<dyn Error>> {
    let show = MenuItem::with_id(app, "show", "打开主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出并停止路由", true, None::<&str>)?;
    let top_sep = PredefinedMenuItem::separator(app)?;
    let bottom_sep = PredefinedMenuItem::separator(app)?;

    let state = resolve_codex_home()
        .ok()
        .and_then(|home| list_provider_state(&home).ok());
    let official_active = state
        .as_ref()
        .is_some_and(|current| current.official_active);
    let official = CheckMenuItem::with_id(
        app,
        "official",
        "官方登录",
        true,
        official_active,
        None::<&str>,
    )?;
    let provider_items = match state.as_ref() {
        Some(current) => current
            .providers
            .iter()
            .map(|provider| {
                CheckMenuItem::with_id(
                    app,
                    format!("provider:{}", provider.id),
                    &provider.name,
                    true,
                    provider.active,
                    None::<&str>,
                )
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };

    let mut entries: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&show, &top_sep, &official];
    for item in &provider_items {
        entries.push(item);
    }
    entries.push(&bottom_sep);
    entries.push(&quit);
    Ok(Menu::with_items(app, &entries)?)
}
