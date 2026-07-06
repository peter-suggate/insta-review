//! Shared plain data types for insta-review. No platform dependencies.

use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// One encoded video access unit (frame), timestamped against the
/// [`CaptureClock`](https://docs.rs) epoch. `dts == pts` everywhere: pipelines
/// must be configured with B-frames disabled.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// Presentation time relative to the capture clock epoch.
    pub pts: Duration,
    /// Display duration. Finalized by the ring when the next packet arrives
    /// (real capture has variable frame timing); until then a nominal value.
    pub duration: Duration,
    /// True when this packet starts a decodable GOP (IDR).
    pub keyframe: bool,
    /// AVCC length-prefixed NAL units (no Annex-B start codes).
    pub data: Bytes,
}

impl EncodedPacket {
    pub fn end_pts(&self) -> Duration {
        self.pts + self.duration
    }
}

/// Codec-level stream configuration, emitted once per pipeline session
/// before the first packet.
#[derive(Debug, Clone)]
pub struct CodecConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// Nominal capture rate; individual packet timing is authoritative.
    pub nominal_fps: u32,
    pub color: ColorInfo,
}

#[derive(Debug, Clone)]
pub enum Codec {
    /// H.264. `avcc` is the AVCDecoderConfigurationRecord (SPS/PPS extradata)
    /// as stored in the MP4 `avcC` box and consumed by WebCodecs `description`.
    H264 { avcc: Bytes },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorInfo {
    /// BT.709, limited (video) range — the standard for game capture.
    Bt709Limited,
}

/// What to capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureTarget {
    /// The primary display (single-monitor setup default).
    PrimaryDisplay,
    /// A display by platform-specific identifier.
    Display { id: String },
    /// A window matched by (sub)string of its title or process name.
    Window { name_match: String },
}

/// Pipeline session configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    pub target: CaptureTarget,
    /// Cap on capture rate; pipelines may deliver less (compositor-paced).
    pub max_fps: u32,
    /// Closed-GOP length in seconds. Trim granularity and player seek cost.
    pub gop_seconds: f32,
    /// Encoder quality knob, roughly CQP/CRF-like; pipeline-specific mapping.
    pub quality: u32,
    /// 0 = capture the full frame. Otherwise encode only a square of this
    /// many pixels centered on the frame (the crosshair) — a large
    /// convert/encode/memory-bandwidth saving on iGPU boxes. Clamped to the
    /// frame; the capture itself is still full-frame (WGC has no source
    /// rect), so this reduces every stage after capture.
    #[serde(default)]
    pub center_crop_px: u32,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            target: CaptureTarget::PrimaryDisplay,
            max_fps: 60,
            gop_seconds: 1.0,
            quality: 23,
            center_crop_px: 0,
        }
    }
}

/// A timeline annotation (from GSI or the trigger itself).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    /// Time relative to the capture clock epoch.
    pub ts: Duration,
    pub kind: MarkerKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MarkerKind {
    Kill {
        count: u32,
        headshot: bool,
    },
    Death,
    DamageTaken {
        amount: u32,
    },
    RoundPhase {
        phase: String,
    },
    Bomb {
        event: String,
    },
    /// Inferred from ammo decrements — approximate by nature.
    ShotFired,
}

/// Instantaneous player state sampled from a GSI payload (~10 Hz).
/// Continuous state, not events — the analysis layer diffs it (ammo →
/// shot counts, position → velocity, flashed/smoked → reliability gates).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GsiState {
    /// Active weapon name ("weapon_ak47"); empty when unknown.
    #[serde(default)]
    pub weapon: String,
    pub ammo_clip: Option<u32>,
    pub health: Option<u32>,
    /// 0-255 flash blindness.
    #[serde(default)]
    pub flashed: u8,
    /// 0-255 smoke occlusion.
    #[serde(default)]
    pub smoked: u8,
    /// World position in game units (player_position component; absent on
    /// older cfg installs — reinstall the GSI cfg to get it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[f64; 3]>,
    /// View-direction unit vector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward: Option<[f64; 3]>,
}

/// A [`GsiState`] stamped against the capture clock (engine side; the
/// sidecar carries the rebased [`ClipGsiSample`]).
#[derive(Debug, Clone, PartialEq)]
pub struct GsiSample {
    pub ts: Duration,
    pub state: GsiState,
}

/// A [`GsiState`] rebased to clip-relative seconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipGsiSample {
    pub at: f64,
    #[serde(flatten)]
    pub state: GsiState,
}

/// Current sidecar schema version. Pre-`gsi_trace` sidecars deserialize
/// with `meta_version` 0 and an empty trace — still fully readable.
pub const CLIP_META_VERSION: u32 = 2;

/// Everything the review player needs besides the sample bytes themselves.
/// Small: crosses the Tauri IPC as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipMeta {
    /// See [`CLIP_META_VERSION`].
    #[serde(default)]
    pub meta_version: u32,
    pub width: u32,
    pub height: u32,
    pub nominal_fps: u32,
    /// Exact presentation time of every frame, seconds, clip-relative.
    /// The source of truth for frame stepping.
    pub frame_pts: Vec<f64>,
    /// Indices into `frame_pts` that are keyframes (seek entry points).
    pub keyframe_indices: Vec<u32>,
    /// Markers rebased to clip-relative seconds.
    pub markers: Vec<ClipMarker>,
    /// Low-rate GSI state trace (weapon/ammo/health/flash), clip-relative.
    /// Consumed by analysis; the player ignores it.
    #[serde(default)]
    pub gsi_trace: Vec<ClipGsiSample>,
    /// When the hotkey was pressed, clip-relative seconds.
    pub trigger_at: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipMarker {
    pub at: f64,
    pub kind: MarkerKind,
}

/// Events flowing out of a capture pipeline.
#[derive(Debug)]
pub enum PipelineEvent {
    /// Emitted once, before the first packet of a session.
    Configured(CodecConfig),
    Packet(EncodedPacket),
    /// The capture target went away (window closed, display mode change).
    /// The engine should restart the pipeline.
    TargetLost,
    Error(PipelineError),
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("capture target not found: {0}")]
    TargetNotFound(String),
    #[error("capture failed: {0}")]
    Capture(String),
    #[error("encoder failed: {0}")]
    Encode(String),
    #[error("{0}")]
    Other(String),
}
