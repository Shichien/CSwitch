//! Window-only preferences; never reads or writes provider/auth configuration.
use super::window_geometry;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Mutex};
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedWindow {
    position: [i32; 2],
    size: [u32; 2], // normal inner size in physical pixels
    maximized: bool,
    #[serde(default = "default_scale")]
    scale_factor: f64,
}

fn default_scale() -> f64 {
    1.0
}

#[derive(Default)]
struct FitGate {
    disabled: bool,
    expected: Vec<[u32; 2]>,
}
impl FitGate {
    fn resized(&mut self, size: [u32; 2]) {
        if let Some(index) = self.expected.iter().position(|expected| *expected == size) {
            self.expected.drain(..index);
        } else {
            self.disabled = true;
        }
    }
    fn take(&mut self, current: [u32; 2]) -> bool {
        self.resized(current);
        let allowed = !self.disabled;
        self.disabled = true;
        allowed
    }
}

#[derive(Default)]
struct State {
    path: Option<PathBuf>,
    normal: Option<SavedWindow>,
    gate: FitGate,
    generation: u64,
    ready: bool,
}
#[derive(Default)]
pub(super) struct WindowState(Mutex<State>);

pub(super) fn initialize(app: &AppHandle) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };
    let path = app
        .path()
        .app_config_dir()
        .ok()
        .map(|p| p.join("window-state.json"));
    let saved = path
        .as_ref()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice::<SavedWindow>(&bytes).ok())
        .filter(|s| s.size[0] > 0 && s.size[1] > 0);
    let monitors = window.available_monitors()?;
    // Prefer the display containing the old window center. If unplugged, use
    // the current/primary monitor and clamp the whole normal rectangle to it.
    let monitor = saved
        .as_ref()
        .and_then(|s| {
            monitors.iter().find(|m| {
                let area = m.work_area();
                let center = [
                    i64::from(s.position[0]) + i64::from(s.size[0]) / 2,
                    i64::from(s.position[1]) + i64::from(s.size[1]) / 2,
                ];
                (0..2).all(|i| {
                    let origin = [area.position.x, area.position.y][i] as i64;
                    center[i] >= origin
                        && center[i] < origin + [area.size.width, area.size.height][i] as i64
                })
            })
        })
        .cloned()
        .or_else(|| window.current_monitor().ok().flatten())
        .or_else(|| window.primary_monitor().ok().flatten());
    let inner = window.inner_size()?;
    let original = [inner.width, inner.height];
    {
        let state = app.state::<WindowState>();
        let mut state = state.0.lock().unwrap();
        state.path = path;
        state.gate.disabled = saved.is_some();
        state.gate.expected.push(original);
    }
    if let Some(monitor) = monitor {
        let area = monitor.work_area();
        let outer = window.outer_size()?;
        let frame = [
            outer.width.saturating_sub(inner.width),
            outer.height.saturating_sub(inner.height),
        ];
        if let Some(mut geometry) = window_geometry::initial_geometry(
            [area.position.x, area.position.y],
            [area.size.width, area.size.height],
            monitor.scale_factor(),
            frame,
        ) {
            if let Some(saved) = &saved {
                geometry = window_geometry::restore_geometry(
                    saved.position,
                    window_geometry::rescale_size(
                        saved.size,
                        saved.scale_factor,
                        monitor.scale_factor(),
                    ),
                    [area.position.x, area.position.y],
                    [area.size.width, area.size.height],
                    monitor.scale_factor(),
                    frame,
                )
                .expect("initial geometry already validated monitor bounds");
            }
            app.state::<WindowState>()
                .0
                .lock()
                .unwrap()
                .gate
                .expected
                .push([geometry.width, geometry.height]);
            window.set_min_size(Some(PhysicalSize::new(
                geometry.min_width,
                geometry.min_height,
            )))?;
            // Move first: crossing displays can trigger OS DPI auto-scaling.
            // Apply the desired physical size after that transition.
            window.set_position(PhysicalPosition::new(geometry.x, geometry.y))?;
            window.set_size(PhysicalSize::new(geometry.width, geometry.height))?;
            app.state::<WindowState>().0.lock().unwrap().normal = Some(SavedWindow {
                position: [geometry.x, geometry.y],
                size: [geometry.width, geometry.height],
                maximized: saved.as_ref().is_some_and(|s| s.maximized),
                scale_factor: monitor.scale_factor(),
            });
            if saved.as_ref().is_some_and(|s| s.maximized) {
                window.maximize()?;
            }
        }
    }
    app.state::<WindowState>().0.lock().unwrap().ready = true;
    Ok(())
}

pub(super) fn fit(app: &AppHandle, height: f64) -> Result<(), String> {
    if !height.is_finite() || height <= 0.0 {
        return Err("Invalid initial window height".into());
    }
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };
    let inner = window.inner_size().map_err(|e| e.to_string())?;
    if !app
        .state::<WindowState>()
        .0
        .lock()
        .unwrap()
        .gate
        .take([inner.width, inner.height])
    {
        return Ok(());
    }
    if window.is_maximized().unwrap_or(false)
        || window.is_minimized().unwrap_or(false)
        || window.is_fullscreen().unwrap_or(false)
    {
        return Ok(());
    }
    let Some(monitor) = window.current_monitor().map_err(|e| e.to_string())? else {
        return Ok(());
    };
    let area = monitor.work_area();
    let outer = window.outer_size().map_err(|e| e.to_string())?;
    let frame_height = outer.height.saturating_sub(inner.height);
    let available = ((f64::from(area.size.height) * 0.8).floor() as u32)
        .saturating_sub(frame_height)
        .max(1);
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let minimum = ((220.0 * scale).floor() as u32).min(available).max(1);
    let height = ((height * scale).ceil() as u32).clamp(minimum, available);
    window
        .set_size(PhysicalSize::new(inner.width, height))
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub(super) fn event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if window.label() != "main" {
        return;
    }
    let app = window.app_handle();
    let Some(state) = app.try_state::<WindowState>() else {
        return;
    };
    if !state.0.lock().unwrap().ready {
        return;
    }
    if let tauri::WindowEvent::Resized(size) = event {
        state
            .0
            .lock()
            .unwrap()
            .gate
            .resized([size.width, size.height]);
    }
    if matches!(
        event,
        tauri::WindowEvent::Moved(_)
            | tauri::WindowEvent::Resized(_)
            | tauri::WindowEvent::ScaleFactorChanged { .. }
    ) {
        capture(window);
        let generation = {
            let mut state = state.0.lock().unwrap();
            state.generation += 1;
            state.generation
        };
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
            let state = app.state::<WindowState>();
            let state = state.0.lock().unwrap();
            if state.generation == generation {
                save(&state);
            }
        });
    }
    if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
        capture(window);
        flush(app);
    }
}

fn capture(window: &tauri::Window) {
    if window.is_minimized().unwrap_or(true) || window.is_fullscreen().unwrap_or(true) {
        return;
    }
    let Ok(maximized) = window.is_maximized() else {
        return;
    };
    let normal = if !maximized {
        match (window.outer_position(), window.inner_size()) {
            (Ok(p), Ok(s)) if s.width > 0 && s.height > 0 => Some(SavedWindow {
                position: [p.x, p.y],
                size: [s.width, s.height],
                maximized,
                scale_factor: window.scale_factor().unwrap_or(1.0),
            }),
            _ => None,
        }
    } else {
        None
    };
    let state = window.state::<WindowState>();
    let mut state = state.0.lock().unwrap();
    if let Some(normal) = normal {
        state.normal = Some(normal);
    }
    if let Some(normal) = &mut state.normal {
        normal.maximized = maximized;
    }
}

pub(super) fn flush(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        capture(&window.as_ref().window());
    }
    if let Some(state) = app.try_state::<WindowState>() {
        save(&state.0.lock().unwrap());
    }
}
fn save(state: &State) {
    let (Some(path), Some(normal)) = (&state.path, &state.normal) else {
        return;
    };
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let temp = path.with_extension("json.tmp");
        use std::io::Write;
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(normal)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(temp, path)?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("Could not save window geometry: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_state_without_dpi_is_compatible() {
        let saved: SavedWindow =
            serde_json::from_str(r#"{"position":[0,40],"size":[640,480],"maximized":false}"#)
                .unwrap();
        assert_eq!(saved.scale_factor, 1.0);
    }

    #[test]
    fn returning_to_old_program_size_counts_as_user_resize() {
        let mut gate = FitGate {
            expected: vec![[800, 600], [537, 240]],
            ..Default::default()
        };
        gate.resized([537, 240]);
        gate.resized([800, 600]);
        assert!(!gate.take([800, 600]));
    }

    #[test]
    fn persistence_roundtrips_and_replaces_only_window_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window-state.json");
        let unrelated = dir.path().join("config.json");
        std::fs::write(&unrelated, "untouched").unwrap();
        let mut state = State {
            path: Some(path.clone()),
            normal: Some(SavedWindow {
                position: [-100, 40],
                size: [640, 480],
                maximized: true,
                scale_factor: 1.25,
            }),
            ..Default::default()
        };
        save(&state);
        state.normal.as_mut().unwrap().size = [720, 600];
        save(&state);
        let restored: SavedWindow = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(restored.size, [720, 600]);
        assert_eq!(restored.position, [-100, 40]);
        assert!(restored.maximized);
        assert_eq!(std::fs::read_to_string(unrelated).unwrap(), "untouched");
    }

    #[test]
    fn program_resize_does_not_consume_initial_fit() {
        let mut gate = FitGate {
            expected: vec![[800, 600], [537, 240]],
            ..Default::default()
        };
        gate.resized([800, 600]);
        gate.resized([537, 240]);
        gate.resized([537, 240]);
        assert!(gate.take([537, 240]));
        assert!(!gate.take([537, 240]));
    }
    #[test]
    fn user_resize_while_loading_disables_fit_even_if_they_return_to_initial_size() {
        let mut gate = FitGate {
            expected: vec![[537, 240]],
            ..Default::default()
        };
        gate.resized([600, 400]);
        gate.resized([537, 240]);
        assert!(!gate.take([537, 240]));
    }
    #[test]
    fn unprocessed_user_resize_and_restored_window_cannot_fit() {
        let mut gate = FitGate {
            expected: vec![[537, 240]],
            ..Default::default()
        };
        assert!(!gate.take([600, 400]));
        let mut restored = FitGate {
            disabled: true,
            expected: vec![[537, 240]],
        };
        assert!(!restored.take([537, 240]));
    }
}
