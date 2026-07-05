use tauri::{AppHandle, Manager, State};
use tracing::info;

use crate::state::{AppSettings, AppState};

#[tauri::command]
pub fn close_review(app: AppHandle, state: State<AppState>) {
    if let Some(window) = app.get_webview_window("review") {
        let _ = window.hide();
    }
    #[cfg(windows)]
    {
        let hwnd = state
            .game_hwnd
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        if hwnd != 0 && !ir_winutil::restore_foreground(hwnd) {
            tracing::warn!("could not restore game focus");
        }
    }
    #[cfg(not(windows))]
    let _ = &state;
}

/// Write the staged clip to the clips directory. Returns the mp4 path.
#[tauri::command]
pub fn save_clip(app: AppHandle, state: State<AppState>) -> Result<String, String> {
    let clip_guard = state.clip.lock().unwrap();
    let current = clip_guard.as_ref().ok_or("no clip staged")?;

    let dir = {
        let settings = state.settings.lock().unwrap();
        match &settings.clips_dir {
            Some(dir) => dir.clone(),
            None => app
                .path()
                .video_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join("insta-review"),
        }
    };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let path = dir.join(format!("clip_{stamp}_{:03}.mp4", current.id));

    let mp4 =
        ir_mux::mux_h264(&current.clip.codec, &current.clip.samples).map_err(|e| e.to_string())?;
    std::fs::write(&path, &mp4).map_err(|e| e.to_string())?;
    let sidecar = path.with_extension("json");
    std::fs::write(
        &sidecar,
        serde_json::to_string_pretty(&current.clip.meta).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    info!(path = %path.display(), "clip saved");
    Ok(path.display().to_string())
}

#[tauri::command]
pub fn get_settings(state: State<AppState>) -> AppSettings {
    state.settings.lock().unwrap().clone()
}

/// Persist new settings. Capture/GSI/hotkey pick them up on
/// `restart_capture` (the UI calls it right after).
#[tauri::command]
pub fn set_settings(
    app: AppHandle,
    state: State<AppState>,
    settings: AppSettings,
) -> Result<(), String> {
    let path = state.settings_path.lock().unwrap().clone();
    settings.save(&path).map_err(|e| e.to_string())?;

    let old_hotkey = {
        let mut guard = state.settings.lock().unwrap();
        let old = guard.hotkey.clone();
        *guard = settings.clone();
        old
    };
    if old_hotkey != settings.hotkey {
        crate::rebind_hotkey(&app, &old_hotkey, &settings.hotkey)?;
    }
    Ok(())
}

#[tauri::command]
pub fn restart_capture(app: AppHandle) -> Result<(), String> {
    crate::engine::restart_capture(&app)
}

/// Ring diagnostics for the settings drawer.
#[tauri::command]
pub fn capture_stats(state: State<AppState>) -> Option<serde_json::Value> {
    let engine = state.engine.lock().unwrap();
    engine.as_ref().and_then(|e| e.stats()).map(|s| {
        serde_json::json!({
            "framesPushed": s.frames_pushed,
            "gopsEvicted": s.gops_evicted,
            "droppedPreIdr": s.dropped_pre_idr,
            "droppedNonMonotonic": s.dropped_non_monotonic,
        })
    })
}

/// Where the GSI cfg would be written (for the consent prompt).
#[tauri::command]
pub fn gsi_cfg_target() -> Result<String, String> {
    ir_gsi::install::cfg_target_path().map(|p| p.display().to_string())
}

/// Install the CS2 GSI cfg. UI must have shown the target path and gotten
/// explicit consent first.
#[tauri::command]
pub fn install_gsi_cfg(state: State<AppState>) -> Result<String, String> {
    let (port, token) = {
        let s = state.settings.lock().unwrap();
        (s.gsi_port, s.gsi_token.clone())
    };
    ir_gsi::install::install_cfg(port, &token).map(|p| p.display().to_string())
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Frontend telemetry into the app log — how the player is doing
/// (essential when the window can't be seen, e.g. headless verification).
#[tauri::command]
pub fn player_status(status: String) {
    info!(target: "player", "{status}");
}
