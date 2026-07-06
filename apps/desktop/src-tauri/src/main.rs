//! insta-review desktop app: hosts the capture engine, global hotkey, GSI
//! listener, and the review window (WebCodecs player frontend).

#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod commands;
mod engine;
mod state;

use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tracing::{info, warn};

use crate::state::{AppSettings, AppState};

/// Serve the staged clip's sample bytes: replay://localhost/clip/<id>/samples
fn replay_protocol(
    app: &AppHandle,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let respond = |status: u16, body: Vec<u8>, content_type: &str| {
        tauri::http::Response::builder()
            .status(status)
            .header("Content-Type", content_type)
            .header("Access-Control-Allow-Origin", "*")
            .body(body)
            .unwrap()
    };

    // convertFileSrc may percent-encode the path on some platforms.
    let path = request.uri().path().replace("%2F", "/").replace("%2f", "/");
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["clip", id, "samples"] => {
            let state = app.state::<AppState>();
            let clip = state.clip.lock().unwrap();
            match clip.as_ref() {
                Some(current) if Ok(current.id) == id.parse() => {
                    respond(200, current.blob.clone(), "application/octet-stream")
                }
                _ => respond(404, b"no such clip".to_vec(), "text/plain"),
            }
        }
        _ => respond(404, b"not found".to_vec(), "text/plain"),
    }
}

fn rebind_hotkey(app: &AppHandle, old: &str, new: &str) -> Result<(), String> {
    let shortcuts = app.global_shortcut();
    let _ = shortcuts.unregister(old);
    shortcuts
        .register(new)
        .map_err(|e| format!("register hotkey {new:?}: {e}"))?;
    info!(hotkey = new, "hotkey bound");
    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tauri::Builder::default()
        // Must be the first plugin: a second launch hands off to the running
        // instance (we surface its review window) and exits.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            info!("second launch detected; surfacing the review window");
            if let Some(window) = app.get_webview_window("review") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        engine::trigger_snapshot(app);
                    }
                })
                .build(),
        )
        .register_uri_scheme_protocol("replay", |ctx, request| {
            replay_protocol(&ctx.app_handle().clone(), request)
        })
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::close_review,
            commands::save_clip,
            commands::get_settings,
            commands::set_settings,
            commands::restart_capture,
            commands::capture_stats,
            commands::gsi_cfg_target,
            commands::install_gsi_cfg,
            commands::quit_app,
            commands::player_status,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let state = app.state::<AppState>();

            // Settings.
            let settings_path = app
                .path()
                .app_config_dir()
                .expect("config dir")
                .join("settings.json");
            let settings = AppSettings::load(&settings_path);
            *state.settings_path.lock().unwrap() = settings_path;
            *state.settings.lock().unwrap() = settings.clone();

            // Capture + GSI.
            if let Err(e) = engine::restart_capture(&handle) {
                warn!("capture failed to start: {e} (fix settings and restart capture)");
            }

            // Hotkey.
            if let Err(e) = rebind_hotkey(&handle, "", &settings.hotkey) {
                warn!("{e}");
            }

            // Dev hook: IR_AUTOTRIGGER=8 fires the snapshot path N seconds
            // after launch (no keyboard needed under automation).
            if let Ok(secs) = std::env::var("IR_AUTOTRIGGER") {
                if let Ok(secs) = secs.parse::<f32>() {
                    let handle = handle.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs_f32(secs));
                        info!("IR_AUTOTRIGGER firing");
                        engine::trigger_snapshot(&handle);
                    });
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the review window hides it; the app keeps capturing.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running insta-review");
}
