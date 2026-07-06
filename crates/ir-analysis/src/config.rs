//! Analysis tuning knobs as data. Compiled-in defaults; a user-editable
//! `analysis-config.json` in the app config dir overrides them — tuning a
//! threshold never needs a rebuild. All thresholds live here, none inline.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AnalysisConfig {
    pub flow: FlowConfig,
    pub movement: MovementConfig,
    pub flick: FlickConfig,
    pub counter_strafe: CounterStrafeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FlowConfig {
    /// Horizontal correlation search, as a fraction of downscaled width.
    pub search_frac: f64,
    /// Band row ranges as fractions of frame height.
    pub horizon_band: (f64, f64),
    pub mid_band: (f64, f64),
    pub ground_band: (f64, f64),
    /// Ground-band column range (right side excluded: viewmodel).
    pub ground_cols: (f64, f64),
    /// Minimum profile standard deviation (0-255 luma) to trust a band —
    /// below this the scene is flat (smoke/flash/wall) and correlation lies.
    pub min_texture: f64,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            search_frac: 0.2,
            horizon_band: (0.33, 0.55),
            mid_band: (0.55, 0.78),
            ground_band: (0.78, 0.97),
            ground_cols: (0.05, 0.62),
            min_texture: 4.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MovementConfig {
    /// |ground dx − horizon dx| (downscaled px/frame) that indicates
    /// strafing parallax.
    pub translation_px: f64,
    /// Sustained frames required before an interval counts as moving.
    pub min_frames: usize,
    /// Correlation quality below this marks the frame `unreliable`.
    pub min_quality: f64,
    /// GSI flashed level (0-255) above which frames are `unreliable`.
    pub flashed_max: u8,
    /// GSI-measured horizontal speed (u/s) above which the player counts
    /// as moving. First-shot accuracy decays well below run speed; ~30 is
    /// "not standing still".
    pub moving_ups: f64,
    /// Max distance (s) to each bracketing GSI speed sample for the
    /// measurement to be trusted at a given instant.
    pub gsi_bracket_s: f64,
}

impl Default for MovementConfig {
    fn default() -> Self {
        Self {
            translation_px: 1.5,
            min_frames: 4,
            min_quality: 0.3,
            flashed_max: 100,
            moving_ups: 30.0,
            gsi_bracket_s: 0.4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FlickConfig {
    /// Angular speed (°/s) a flick must exceed at its peak.
    pub peak_dps: f64,
    /// Net view displacement (°) a flick must cover.
    pub min_displacement_deg: f64,
    /// |v| (°/s) below which the crosshair counts as settled.
    pub settle_dps: f64,
    /// Consecutive frames below `settle_dps` to call it settled.
    pub settle_frames: usize,
    /// Overshoot (°) worth reporting.
    pub overshoot_deg: f64,
}

impl Default for FlickConfig {
    fn default() -> Self {
        Self {
            peak_dps: 150.0,
            min_displacement_deg: 5.0,
            settle_dps: 15.0,
            settle_frames: 3,
            overshoot_deg: 2.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CounterStrafeConfig {
    /// Stop-to-shot gap (ms) below which the shot fired before full
    /// accuracy reset.
    pub settle_ms: f64,
    /// Stop-to-shot gap (ms) up to which we call it a good counter-strafe.
    pub good_ms: f64,
    /// Weapons where movement accuracy doesn't matter (no findings).
    pub exempt_weapons: Vec<String>,
}

impl Default for CounterStrafeConfig {
    fn default() -> Self {
        Self {
            settle_ms: 66.0,
            good_ms: 250.0,
            exempt_weapons: vec![
                "weapon_knife".into(),
                "weapon_taser".into(),
                "weapon_hegrenade".into(),
                "weapon_flashbang".into(),
                "weapon_smokegrenade".into(),
                "weapon_molotov".into(),
                "weapon_incgrenade".into(),
                "weapon_decoy".into(),
            ],
        }
    }
}

pub const CONFIG_FILE: &str = "analysis-config.json";

/// Load the user's config, writing the defaults first if missing (so
/// there's always a file to edit). Unknown/missing fields fall back to
/// defaults per `serde(default)`.
pub fn load_or_init(config_dir: &Path) -> AnalysisConfig {
    let path = config_dir.join(CONFIG_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => {
            let cfg = AnalysisConfig::default();
            if let Ok(json) = serde_json::to_string_pretty(&cfg) {
                let _ = std::fs::create_dir_all(config_dir);
                let _ = std::fs::write(&path, json);
            }
            cfg
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_init_writes_then_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let a = load_or_init(tmp.path());
        assert!(tmp.path().join(CONFIG_FILE).exists());
        // Edit one knob on disk; reload honours it, rest stay default.
        let text = std::fs::read_to_string(tmp.path().join(CONFIG_FILE)).unwrap();
        let text = text.replace("\"translationPx\": 1.5", "\"translationPx\": 9.0");
        std::fs::write(tmp.path().join(CONFIG_FILE), text).unwrap();
        let b = load_or_init(tmp.path());
        assert_eq!(b.movement.translation_px, 9.0);
        assert_eq!(b.flick.peak_dps, a.flick.peak_dps);
    }
}
