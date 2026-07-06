//! Analysis domain types. Serialized camelCase: they cross the Tauri IPC
//! and persist as sidecar JSON (`report.json`, versioned).

use std::collections::BTreeMap;

use ir_types::MarkerKind;
use serde::{Deserialize, Serialize};

/// Bump on breaking changes to [`AnalysisReport`]; readers treat a mismatch
/// as a cache miss. Additive fields don't bump.
pub const SCHEMA_VERSION: u32 = 1;

/// The kill/death event being analyzed. `id` is the stable cache key
/// (`"kill_9200ms"`); `at_s` is clip-relative with the GSI display offset
/// already applied (best estimate of when the event is visible on screen).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRef {
    pub id: String,
    pub at_s: f64,
    pub kind: MarkerKind,
}

/// One frame the webview extractor must deliver.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameRequest {
    /// Exact sample timestamp (snapped to `ClipMeta.frame_pts`).
    pub t_us: u64,
    /// Full-res JPEG for LLM evidence.
    pub want_jpeg: bool,
    /// Downscaled raw pixels for CV (unused until the CV milestone).
    pub want_raw: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractionPlan {
    pub frames: Vec<FrameRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Positive,
    Info,
    Minor,
    Major,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSource {
    Cv,
    Llm,
    CvConfirmedByLlm,
}

/// Where a finding points on the clip timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    /// Clip-relative seconds.
    pub t: f64,
    /// Label burned into the frames the LLM saw (`"F3"`, `"D1"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_label: Option<String>,
    #[serde(default)]
    pub note: String,
}

/// One coaching finding. `kind` is an open string (well-known values:
/// `crosshair_low`, `moving_while_shooting`, `counter_strafe_late`,
/// `flick_overshoot`, `fired_before_settled`, `spray_too_long`,
/// `overexposed_after_damage`, `died_flashed`, `good_counter_strafe`,
/// `clean_flick`) so new analyzers need no schema change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub kind: String,
    pub severity: Severity,
    /// 0..1, post-gating (min of CV and LLM confidence where both exist).
    pub confidence: f32,
    pub time_range: (f64, f64),
    pub evidence: Vec<Evidence>,
    /// Kind-specific numbers (overshoot degrees, stop-to-shot ms, ...).
    #[serde(default)]
    pub metrics: serde_json::Value,
    /// 1-3 sentences, actionable.
    pub coaching: String,
    pub source: FindingSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub provider: String,
    pub model: String,
    pub cli_version: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisReport {
    pub schema_version: u32,
    pub event: EventRef,
    /// Overall coaching text. In the walking-skeleton milestone this is the
    /// whole result; structured `findings` arrive with the schema milestone.
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// CV metric passthrough for display/debugging.
    #[serde(default)]
    pub metrics: serde_json::Value,
    pub provider: ProviderInfo,
    /// Self-check failures that capped confidence ("ocr/gsi shot mismatch").
    #[serde(default)]
    pub degradations: Vec<String>,
    #[serde(default)]
    pub analyzer_versions: BTreeMap<String, String>,
    /// Clip-relative seconds of the frames the model saw — the UI's
    /// evidence thumbnails. Additive field: no schema bump.
    #[serde(default)]
    pub frames: Vec<f64>,
}
