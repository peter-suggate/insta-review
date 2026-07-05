use std::path::Path;

use ir_types::PipelineConfig;
use serde::{Deserialize, Serialize};

/// User-facing settings, persisted as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// How far back the review window reaches.
    pub window_seconds: f32,
    /// Ring retention (>= window; extra headroom is nearly free).
    pub retain_seconds: f32,
    /// Hard cap on ring memory.
    pub max_ring_bytes: usize,
    /// How far before the trigger the player opens, paused.
    pub open_rewind_seconds: f32,
    /// Shift applied to GSI marker timestamps (they lag the game).
    pub gsi_offset_seconds: f32,
    pub pipeline: PipelineConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            window_seconds: 15.0,
            retain_seconds: 16.0,
            max_ring_bytes: 512 * 1024 * 1024,
            open_rewind_seconds: 1.5,
            gsi_offset_seconds: -0.25,
            pipeline: PipelineConfig::default(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }
}
