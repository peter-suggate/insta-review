//! Engine + GSI lifecycle and the snapshot-on-hotkey path.

use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use ir_core::Engine;
use ir_types::{Codec, Marker, PipelineConfig};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use tracing::{info, warn};

use crate::state::{index_clip, AppSettings, AppState, CurrentClip};

pub fn build_pipeline(settings: &AppSettings) -> Result<Box<dyn ir_core::CapturePipeline>, String> {
    let choice = match settings.pipeline.as_str() {
        "auto" => {
            if cfg!(windows) {
                "windows"
            } else {
                "test"
            }
        }
        other => other,
    };
    match choice {
        #[cfg(windows)]
        "windows" => Ok(Box::new(ir_pipeline_win::WindowsPipeline::new())),
        #[cfg(feature = "test-pipeline")]
        "test" => Ok(Box::new(ir_pipeline_test::TestPatternPipeline::new(
            1280, 720, true,
        ))),
        other => Err(format!("pipeline {other:?} not available in this build")),
    }
}

/// (Re)start capture + GSI from current settings. Stops any running
/// instances first.
pub fn restart_capture(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();

    if let Some(old) = state.engine.lock().unwrap().take() {
        old.stop();
    }
    if let Some(old) = state.gsi.lock().unwrap().take() {
        old.stop();
    }

    let pipeline = build_pipeline(&settings)?;
    let cfg = PipelineConfig {
        max_fps: settings.fps,
        gop_seconds: settings.gop_seconds,
        quality: settings.quality,
        center_crop_px: settings.capture_crop_px,
        ..PipelineConfig::default()
    };
    let retain = Duration::from_secs_f32(settings.window_seconds + settings.gop_seconds.max(1.0));
    let handle = Engine::start(
        pipeline,
        cfg,
        retain,
        settings.max_ring_mib as usize * 1024 * 1024,
    )
    .map_err(|e| e.to_string())?;

    if settings.gsi_enabled {
        let clock = handle.clock();
        let marker_tx = handle.marker_sender();
        match ir_gsi::GsiServer::start(
            settings.gsi_port,
            Some(settings.gsi_token.clone()),
            move |kind| {
                marker_tx.send(Marker {
                    ts: clock.now(),
                    kind,
                });
            },
        ) {
            Ok(server) => *state.gsi.lock().unwrap() = Some(server),
            Err(e) => warn!("GSI listener failed to start: {e}"),
        }
    }

    *state.engine.lock().unwrap() = Some(handle);
    info!("capture started");
    Ok(())
}

/// Pause capture while the user reviews: encoding competes with the
/// player's decoder for the same GPU video engine, and recording the
/// review window itself is useless footage anyway.
pub fn stop_capture(app: &AppHandle) {
    let state = app.state::<AppState>();
    let old = state.engine.lock().unwrap().take();
    if let Some(old) = old {
        old.stop();
        info!("capture paused while reviewing");
    }
}

/// WebCodecs codec string ("avc1.PPCCLL") from the avcC record.
pub fn codec_string(avcc: &[u8]) -> String {
    if avcc.len() >= 4 {
        format!("avc1.{:02X}{:02X}{:02X}", avcc[1], avcc[2], avcc[3])
    } else {
        "avc1.640028".into()
    }
}

/// The hotkey path: freeze the buffer, stage the clip, show the review
/// window. Heavy work (blob build) happens off the hotkey thread.
pub fn trigger_snapshot(app: &AppHandle) {
    // The keyboard hook can double-fire a single press (observed ~200 ms
    // apart); a second trigger this close is never intentional.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = app
        .state::<AppState>()
        .last_trigger_ms
        .swap(now_ms, Ordering::Relaxed);
    if now_ms.saturating_sub(last) < 400 {
        info!("trigger debounced (double fire)");
        return;
    }

    let app = app.clone();
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let settings = state.settings.lock().unwrap().clone();

        // Remember the game window before we steal focus (Windows).
        #[cfg(windows)]
        state
            .game_hwnd
            .store(ir_winutil::foreground_window(), Ordering::Relaxed);

        let clip = {
            let engine = state.engine.lock().unwrap();
            let Some(engine) = engine.as_ref() else {
                warn!("trigger with no engine running");
                return;
            };
            engine.snapshot(Duration::from_secs_f32(settings.window_seconds))
        };
        let Some(clip) = clip else {
            warn!("trigger before first keyframe; nothing to review");
            return;
        };

        let (blob, index) = index_clip(&clip);
        let id = state.clip_counter.fetch_add(1, Ordering::Relaxed) + 1;

        let Codec::H264 { avcc } = &clip.codec.codec;
        let codec_string = codec_string(avcc);
        let payload = json!({
            "id": id,
            "capturedAtMs": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            "meta": clip.meta,
            "codec": {
                "codecString": codec_string,
                "avccB64": base64::engine::general_purpose::STANDARD.encode(avcc),
                "width": clip.codec.width,
                "height": clip.codec.height,
            },
            "samples": index,
            "openRewind": settings.open_rewind_seconds,
            "gsiOffset": settings.gsi_offset_seconds,
            "stretch43": settings.stretch_43,
            // Dev hook: frontend runs a scripted step/play self-test and
            // reports via player_status.
            "autotest": std::env::var("IR_AUTOTEST").is_ok(),
        });

        *state.clip.lock().unwrap() = Some(CurrentClip {
            id,
            clip,
            blob,
            payload: payload.clone(),
        });

        if let Err(e) = app.emit("clip-ready", payload) {
            warn!("emit clip-ready: {e}");
        }
        if let Some(window) = app.get_webview_window("review") {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
        }
        info!(id, "clip staged for review");
    });
}
