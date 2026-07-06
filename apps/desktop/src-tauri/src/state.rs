use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicU32};
use std::sync::Mutex;

use ir_core::{Clip, EngineHandle};
use serde::{Deserialize, Serialize};

/// User settings, persisted as JSON in the app config dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppSettings {
    /// Global hotkey that freezes the buffer and opens review.
    pub hotkey: String,
    /// Seconds of footage the review covers.
    pub window_seconds: f32,
    pub fps: u32,
    /// GOP length in seconds; smaller = cheaper backstep, slightly larger
    /// files.
    pub gop_seconds: f32,
    /// CRF-ish encoder quality (lower is better).
    pub quality: u32,
    pub max_ring_mib: u32,
    /// The player opens paused this many seconds before the trigger.
    pub open_rewind_seconds: f32,
    /// "auto" (windows there, test elsewhere), "windows", or "test".
    pub pipeline: String,
    /// Encode only a square of this many pixels centered on the screen
    /// (the crosshair). 0 = full frame. Big convert/encode bandwidth saving
    /// on iGPU machines.
    pub capture_crop_px: u32,
    pub gsi_enabled: bool,
    pub gsi_port: u16,
    pub gsi_token: String,
    /// Shift applied to GSI markers on the timeline (they lag the game).
    pub gsi_offset_seconds: f32,
    /// Where saved clips go. None = ~/Videos/insta-review (or platform
    /// equivalent).
    pub clips_dir: Option<PathBuf>,
    /// Render 4:3 clips stretched to 16:9 (as seen in-game).
    pub stretch_43: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            hotkey: "ctrl+alt+r".into(),
            window_seconds: 15.0,
            fps: 60,
            gop_seconds: 0.5,
            quality: 23,
            max_ring_mib: 512,
            open_rewind_seconds: 1.5,
            pipeline: "auto".into(),
            capture_crop_px: 0,
            gsi_enabled: true,
            gsi_port: 3585,
            gsi_token: "insta-review".into(),
            gsi_offset_seconds: -0.25,
            clips_dir: None,
            stretch_43: true,
        }
    }
}

impl AppSettings {
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)
    }
}

/// Per-sample index entry the player uses to slice the blob.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SampleIndex {
    /// Byte offset into the samples blob.
    pub offset: u64,
    pub size: u32,
    pub key: bool,
    /// Presentation time in microseconds, clip-relative.
    pub t_us: u64,
}

/// The clip currently loaded in the review window.
pub struct CurrentClip {
    pub id: u32,
    pub clip: Clip,
    /// All AVCC sample bytes, concatenated (served via replay://).
    pub blob: Vec<u8>,
}

#[derive(Default)]
pub struct AppState {
    pub engine: Mutex<Option<EngineHandle>>,
    pub gsi: Mutex<Option<ir_gsi::GsiServer>>,
    pub clip: Mutex<Option<CurrentClip>>,
    pub settings: Mutex<AppSettings>,
    pub settings_path: Mutex<PathBuf>,
    pub clip_counter: AtomicU32,
    /// Foreground window (the game) at trigger time; 0 = none.
    /// Read on Windows only (focus restore).
    #[allow(dead_code)]
    pub game_hwnd: AtomicIsize,
}

/// Build the blob + index from a snapshot clip.
pub fn index_clip(clip: &Clip) -> (Vec<u8>, Vec<SampleIndex>) {
    let total: usize = clip.samples.iter().map(|s| s.data.len()).sum();
    let mut blob = Vec::with_capacity(total);
    let mut index = Vec::with_capacity(clip.samples.len());
    for (i, sample) in clip.samples.iter().enumerate() {
        index.push(SampleIndex {
            offset: blob.len() as u64,
            size: sample.data.len() as u32,
            key: sample.keyframe,
            t_us: (clip.meta.frame_pts[i] * 1_000_000.0).round() as u64,
        });
        blob.extend_from_slice(&sample.data);
    }
    (blob, index)
}
