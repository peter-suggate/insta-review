//! Local computer vision over extracted clip frames: calibration,
//! band-differential flow, movement/flick/shot analysis. Everything here is
//! deterministic and unit-tested against synthetic motion — the LLM only
//! ever *interprets* these measurements, never produces them.

pub mod flow;
pub mod gsi_motion;
pub mod motion;

use ir_types::ClipGsiSample;
use serde::Serialize;

use crate::config::AnalysisConfig;
use crate::types::{
    AnalysisReport, Evidence, EventRef, Finding, FindingSource, ProviderInfo, SCHEMA_VERSION,
};

/// Pinhole calibration for the clip: pixels-per-radian at full resolution.
/// CS2 vertical FOV is 73.74° at every aspect; horizontal FOV is 90° for
/// 4:3 (stretched to the full frame width by the GPU) and 106.26° native
/// 16:9. `stretched43` comes from the user's setting — the frames alone
/// can't tell (both cases fill the same 16:9 capture).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Calib {
    pub fx: f64,
    pub fy: f64,
    pub stretched43: bool,
}

impl Calib {
    pub fn new(width: u32, height: u32, stretched43: bool) -> Self {
        let (w, h) = (width as f64, height as f64);
        let fy = (h / 2.0) / (73.74f64 / 2.0).to_radians().tan();
        let fx = if stretched43 {
            (w / 2.0) / (90.0f64 / 2.0).to_radians().tan()
        } else {
            (w / 2.0) / (106.26f64 / 2.0).to_radians().tan()
        };
        Self {
            fx,
            fy,
            stretched43,
        }
    }
}

/// One downscaled grayscale frame from the webview extractor.
pub struct LumaFrame {
    pub t_us: u64,
    pub w: u32,
    pub h: u32,
    /// Row-major, `w * h` bytes.
    pub data: Vec<u8>,
}

/// Sorted collection of luma frames delivered for one analysis.
#[derive(Default)]
pub struct FrameStore {
    frames: Vec<LumaFrame>,
}

impl FrameStore {
    pub fn push(&mut self, frame: LumaFrame) {
        let pos = self
            .frames
            .partition_point(|f| f.t_us <= frame.t_us);
        self.frames.insert(pos, frame);
    }

    pub fn frames(&self) -> &[LumaFrame] {
        &self.frames
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// Everything the CV pass produces. Traces feed the debug overlay and
/// `traces.json`; candidates feed the prompt; metrics feed the report.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CvReport {
    pub calib: Calib,
    pub flow: Vec<flow::FlowSample>,
    pub movement: Vec<motion::MovementInterval>,
    pub shots: Vec<motion::ShotEvent>,
    pub flicks: Vec<motion::Flick>,
    pub candidates: Vec<motion::Candidate>,
    /// Measured horizontal speed (u/s) from GSI position deltas; empty on
    /// pre-position cfg installs.
    pub speed: Vec<gsi_motion::SpeedSample>,
    /// View angles from the GSI forward vector.
    pub view: Vec<gsi_motion::ViewSample>,
    /// Integrated optical-flow yaw ÷ GSI view-direction yaw over the same
    /// spans (~1.0 = calibrated); None = not enough rotation to judge.
    pub flow_yaw_ratio: Option<f64>,
    /// Analyzer id → version, for the report's provenance.
    pub versions: std::collections::BTreeMap<String, String>,
}

/// Run the full CV pass. `clip_dims` = full-res (width, height);
/// `gsi_offset_s` shifts GSI receipt times onto the video timeline.
pub fn analyze(
    event: &EventRef,
    frames: &FrameStore,
    gsi: &[ClipGsiSample],
    clip_dims: (u32, u32),
    stretched43: bool,
    gsi_offset_s: f64,
    cfg: &AnalysisConfig,
) -> CvReport {
    let calib = Calib::new(clip_dims.0, clip_dims.1, stretched43);
    let flow = flow::flow_trace(frames.frames(), &calib, clip_dims, &cfg.flow);
    let speed = gsi_motion::speed_trace(gsi, gsi_offset_s);
    let view = gsi_motion::view_trace(gsi, gsi_offset_s);
    let movement = motion::movement_intervals(&flow, gsi, &speed, cfg);
    let shots = motion::shots_from_gsi(gsi, gsi_offset_s);
    let flicks = motion::flicks(&flow, cfg);
    let candidates = motion::candidates(event.at_s, &movement, &shots, &flicks, &speed, cfg);
    let flow_yaw_ratio = gsi_motion::flow_vs_gsi_yaw_ratio(&flow, &view);
    let versions = [
        ("flow".to_string(), "1".to_string()),
        ("movement".to_string(), "2".to_string()),
        ("shots-gsi".to_string(), "1".to_string()),
        ("flick".to_string(), "1".to_string()),
        ("gsi-motion".to_string(), "1".to_string()),
    ]
    .into();
    CvReport {
        calib,
        flow,
        movement,
        shots,
        flicks,
        candidates,
        speed,
        view,
        flow_yaw_ratio,
        versions,
    }
}

/// Self-check degradations derived from the CV pass itself (appended to
/// whatever the report pipeline adds).
pub fn cv_degradations(cv: &CvReport) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(ratio) = cv.flow_yaw_ratio {
        if !(0.5..=2.0).contains(&ratio) {
            out.push(format!(
                "optical-flow rotation disagrees with GSI view data \
                 (flow/GSI yaw ratio {ratio:.2}) — flick/velocity numbers suspect; \
                 check the stretched-4:3 setting"
            ));
        }
    }
    out
}

/// Compact per-sample trace for UI overlays and report metrics: movement
/// state resolved per flow sample, measured speed (u/s) where GSI covers
/// it, values rounded to keep JSON small.
pub fn flow_trace_json(cv: &CvReport) -> Vec<serde_json::Value> {
    cv.flow
        .iter()
        .map(|s| {
            let moving = cv
                .movement
                .iter()
                .find(|iv| s.t >= iv.start_s && s.t <= iv.end_s)
                .map(|iv| iv.state)
                .unwrap_or(motion::MoveState::Unreliable);
            let ups = gsi_motion::speed_at(&cv.speed, s.t, 0.4);
            serde_json::json!({
                "t": (s.t * 1000.0).round() / 1000.0,
                "yawDps": (s.yaw_dps * 10.0).round() / 10.0,
                "moving": moving,
                "ups": ups.map(|u| u.round()),
            })
        })
        .collect()
}

/// Deterministic "quick analysis" report from the CV measurements alone —
/// no LLM. Same schema as the coached report so caching, export, and the
/// drawer treat both identically; `provider: "local-cv"` distinguishes it
/// (and is the UI's cue to offer the LLM upgrade).
pub fn local_report(
    event: &EventRef,
    cv: &CvReport,
    duration_ms: u64,
    frames_s: Vec<f64>,
    degradations: Vec<String>,
) -> AnalysisReport {
    let findings = cv
        .candidates
        .iter()
        .map(|c| Finding {
            kind: c.kind.clone(),
            severity: c.severity,
            confidence: c.confidence,
            time_range: (c.start_s, c.end_s),
            evidence: vec![Evidence {
                t: c.end_s,
                frame_label: None,
                note: c.note.clone(),
            }],
            metrics: c.metrics.clone(),
            coaching: c.note.clone(),
            source: FindingSource::Cv,
        })
        .collect();

    let fight: Vec<&motion::ShotEvent> = cv
        .shots
        .iter()
        .filter(|s| s.t >= event.at_s - 3.0 && s.t <= event.at_s + 0.5)
        .collect();
    let shots_total: u32 = fight.iter().map(|s| s.count).sum();
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let count = |kind: &str| cv.candidates.iter().filter(|c| c.kind == kind).count();

    let mut lines = vec![if fight.is_empty() {
        "No shots detected in the 3 s before the event (GSI ammo trace).".to_string()
    } else {
        format!(
            "{shots_total} shot{} in {} burst{} in the 3 s before the event.",
            plural(shots_total as usize),
            fight.len(),
            plural(fight.len())
        )
    }];
    for (kind, label) in [
        ("moving_while_shooting", "Bursts fired while moving"),
        ("fired_before_settled", "Shots before movement settled"),
        ("good_counter_strafe", "Good counter-strafes"),
        ("flick_overshoot", "Flicks that overshot"),
        ("clean_flick", "Clean flicks"),
    ] {
        let n = count(kind);
        if n > 0 {
            lines.push(format!("{label}: {n}."));
        }
    }
    lines.push(
        "Local measurements only (optical flow + GSI ammo trace) — no AI interpretation."
            .to_string(),
    );

    AnalysisReport {
        schema_version: SCHEMA_VERSION,
        event: event.clone(),
        summary: lines.join(" "),
        findings,
        metrics: serde_json::json!({
            "candidates": cv.candidates,
            "shots": cv.shots,
            "flicks": cv.flicks,
            "movementIntervals": cv.movement,
            "flowTrace": flow_trace_json(cv),
        }),
        provider: ProviderInfo {
            provider: "local-cv".into(),
            model: String::new(),
            cli_version: "local".into(),
            duration_ms,
        },
        degradations,
        analyzer_versions: cv.versions.clone(),
        frames: frames_s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cv::motion::{MoveSource, MoveState};
    use ir_types::{GsiState, MarkerKind};

    const DS_W: u32 = 480;
    const DS_H: u32 = 270;
    const FULL: (u32, u32) = (1920, 1080);

    /// Deterministic textured background: value at (world_x, y).
    fn tex(x: i64, y: i64) -> u8 {
        let h = x.wrapping_mul(31).wrapping_add(y.wrapping_mul(17));
        let h = (h ^ (h >> 7)).wrapping_mul(2654435761u32 as i64);
        (((h >> 8) & 0xFF) as u8) / 2 + 64
    }

    /// Frame with the whole scene shifted `shift_x` px (camera pan) and the
    /// ground band shifted an EXTRA `ground_extra` px (translation parallax).
    fn frame(t_us: u64, shift_x: i64, ground_extra: i64) -> LumaFrame {
        let (w, h) = (DS_W as usize, DS_H as usize);
        let mut data = vec![0u8; w * h];
        let ground_start = (h as f64 * 0.78) as usize;
        for y in 0..h {
            let extra = if y >= ground_start { ground_extra } else { 0 };
            for x in 0..w {
                // Content moves left when the camera pans right: world
                // coordinate = screen x + accumulated shift.
                data[y * w + x] = tex(x as i64 + shift_x + extra, y as i64);
            }
        }
        LumaFrame {
            t_us,
            w: DS_W,
            h: DS_H,
            data,
        }
    }

    fn store(frames: Vec<LumaFrame>) -> FrameStore {
        let mut s = FrameStore::default();
        for f in frames {
            s.push(f);
        }
        s
    }

    fn event(at_s: f64) -> EventRef {
        EventRef {
            id: "kill_1ms".into(),
            at_s,
            kind: MarkerKind::Kill {
                count: 1,
                headshot: false,
            },
        }
    }

    fn cfg() -> AnalysisConfig {
        AnalysisConfig::default()
    }

    #[test]
    fn calib_matches_cs2_fovs() {
        let c = Calib::new(1920, 1080, true);
        assert!((c.fx - 960.0).abs() < 1.0, "fx {}", c.fx);
        assert!((c.fy - 720.0).abs() < 1.0, "fy {}", c.fy);
        let n = Calib::new(1920, 1080, false);
        assert!((n.fx - 720.0).abs() < 2.0, "16:9 fx {}", n.fx);
        assert!((n.fy - 720.0).abs() < 1.0, "16:9 fy {}", n.fy);
    }

    #[test]
    fn pan_right_gives_positive_yaw_of_right_magnitude() {
        // 3 ds-px/frame at 60 fps, 480-wide ds of a 1920 clip → 12 full px
        // per frame. yaw/frame = atan(12/960) = 0.716° → ~43 °/s.
        let frames: Vec<LumaFrame> = (0..30)
            .map(|i| frame(i * 16_667, i as i64 * 3, 0))
            .collect();
        let flow = flow::flow_trace(&frames, &Calib::new(FULL.0, FULL.1, true), FULL, &cfg().flow);
        let mid = &flow[flow.len() / 2];
        assert!(mid.yaw_dps > 0.0, "pan right must be yaw+ ({})", mid.yaw_dps);
        assert!(
            (mid.yaw_dps - 42.9).abs() < 5.0,
            "expected ~43°/s, got {}",
            mid.yaw_dps
        );
        assert!(mid.quality > 0.5, "quality {}", mid.quality);
        // Pure rotation: no translation indicator.
        assert!(
            mid.translation_px.abs() < 0.75,
            "rotation leaked into translation: {}",
            mid.translation_px
        );
    }

    #[test]
    fn ground_parallax_classifies_as_moving() {
        // Static aim, ground sliding 3 px/frame = pure strafe parallax.
        let frames: Vec<LumaFrame> = (0..40)
            .map(|i| frame(i * 16_667, 0, i as i64 * 3))
            .collect();
        let flow = flow::flow_trace(&frames, &Calib::new(FULL.0, FULL.1, true), FULL, &cfg().flow);
        let intervals = motion::movement_intervals(&flow, &[], &[], &cfg());
        assert!(
            intervals
                .iter()
                .any(|iv| iv.state == MoveState::Moving && (iv.end_s - iv.start_s) > 0.3),
            "expected a sustained moving interval, got {intervals:?}"
        );
    }

    #[test]
    fn measured_speed_beats_visual_classifier_both_ways() {
        let gsi = |positions: Vec<[f64; 3]>| -> Vec<ClipGsiSample> {
            positions
                .into_iter()
                .enumerate()
                .map(|(i, p)| ClipGsiSample {
                    at: i as f64 * 0.1,
                    state: GsiState {
                        position: Some(p),
                        ..Default::default()
                    },
                })
                .collect()
        };

        // (a) Visually static frames, but GSI measures a 250 u/s run:
        // the fused verdict must be Moving, from measurement.
        let frames_static: Vec<LumaFrame> = (0..40).map(|i| frame(i * 16_667, 0, 0)).collect();
        let running = gsi((0..9).map(|i| [i as f64 * 25.0, 0.0, 0.0]).collect());
        let r = analyze(&event(0.4), &store(frames_static), &running, FULL, true, 0.0, &cfg());
        assert!(
            r.movement
                .iter()
                .any(|iv| iv.state == MoveState::Moving && iv.source != MoveSource::Visual),
            "GSI-measured run must classify as moving: {:?}",
            r.movement
        );

        // (b) Ground parallax fools the flow classifier, but GSI measures
        // standing still: no sustained Moving verdict may survive.
        let frames_parallax: Vec<LumaFrame> =
            (0..40).map(|i| frame(i * 16_667, 0, i as i64 * 3)).collect();
        let standing = gsi(vec![[100.0, 200.0, 0.0]; 9]);
        let r = analyze(&event(0.4), &store(frames_parallax), &standing, FULL, true, 0.0, &cfg());
        assert!(
            !r.movement
                .iter()
                .any(|iv| iv.state == MoveState::Moving && iv.end_s - iv.start_s > 0.2),
            "measured stillness must override visual parallax: {:?}",
            r.movement
        );
    }

    #[test]
    fn static_scene_is_stationary() {
        let frames: Vec<LumaFrame> = (0..30).map(|i| frame(i * 16_667, 0, 0)).collect();
        let flow = flow::flow_trace(&frames, &Calib::new(FULL.0, FULL.1, true), FULL, &cfg().flow);
        let intervals = motion::movement_intervals(&flow, &[], &[], &cfg());
        assert!(
            intervals.iter().all(|iv| iv.state == MoveState::Stationary),
            "{intervals:?}"
        );
        let mid = &flow[flow.len() / 2];
        assert!(mid.yaw_dps.abs() < 2.0 && mid.pitch_dps.abs() < 2.0);
    }

    #[test]
    fn shots_from_ammo_diffs_ignore_reloads_and_switches() {
        let s = |at: f64, weapon: &str, ammo: u32| ClipGsiSample {
            at,
            state: GsiState {
                weapon: weapon.into(),
                ammo_clip: Some(ammo),
                health: Some(100),
                flashed: 0,
                smoked: 0,
                ..Default::default()
            },
        };
        let trace = vec![
            s(0.0, "weapon_ak47", 30),
            s(0.1, "weapon_ak47", 27), // 3 shots
            s(0.2, "weapon_ak47", 27),
            s(0.3, "weapon_ak47", 25), // 2 shots
            s(0.4, "weapon_ak47", 30), // reload — not shots
            s(0.5, "weapon_deagle", 7), // switch — not shots
        ];
        let shots = motion::shots_from_gsi(&trace, 0.0);
        assert_eq!(shots.len(), 2);
        assert_eq!(shots[0].count, 3);
        assert_eq!(shots[1].count, 2);
    }

    #[test]
    fn moving_while_shooting_candidate_fires() {
        // Strafing the whole time + a shot burst mid-window.
        let frames: Vec<LumaFrame> = (0..60)
            .map(|i| frame(i * 16_667, 0, i as i64 * 3))
            .collect();
        let gsi = vec![
            ClipGsiSample {
                at: 0.3,
                state: GsiState {
                    weapon: "weapon_ak47".into(),
                    ammo_clip: Some(30),
                    health: Some(100),
                    flashed: 0,
                    smoked: 0,
                    ..Default::default()
                },
            },
            ClipGsiSample {
                at: 0.5,
                state: GsiState {
                    weapon: "weapon_ak47".into(),
                    ammo_clip: Some(26),
                    health: Some(100),
                    flashed: 0,
                    smoked: 0,
                    ..Default::default()
                },
            },
        ];
        let report = analyze(&event(0.8), &store(frames), &gsi, FULL, true, 0.0, &cfg());
        assert!(
            report
                .candidates
                .iter()
                .any(|c| c.kind == "moving_while_shooting"),
            "candidates: {:?}",
            report.candidates.iter().map(|c| &c.kind).collect::<Vec<_>>()
        );
    }

    #[test]
    fn local_report_carries_cv_findings() {
        let frames: Vec<LumaFrame> = (0..60)
            .map(|i| frame(i * 16_667, 0, i as i64 * 3))
            .collect();
        let gsi = vec![
            ClipGsiSample {
                at: 0.3,
                state: GsiState {
                    weapon: "weapon_ak47".into(),
                    ammo_clip: Some(30),
                    health: Some(100),
                    flashed: 0,
                    smoked: 0,
                    ..Default::default()
                },
            },
            ClipGsiSample {
                at: 0.5,
                state: GsiState {
                    weapon: "weapon_ak47".into(),
                    ammo_clip: Some(26),
                    health: Some(100),
                    flashed: 0,
                    smoked: 0,
                    ..Default::default()
                },
            },
        ];
        let cv = analyze(&event(0.8), &store(frames), &gsi, FULL, true, 0.0, &cfg());
        let report = local_report(&event(0.8), &cv, 7, vec![0.5, 0.8], vec![]);
        assert_eq!(report.provider.provider, "local-cv");
        assert!(report
            .findings
            .iter()
            .any(|f| f.kind == "moving_while_shooting"
                && f.source == crate::types::FindingSource::Cv));
        assert!(report.summary.contains("burst"));
        assert!(report.summary.contains("no AI interpretation"));
        assert_eq!(report.frames, vec![0.5, 0.8]);
    }

    #[test]
    fn flick_detection_with_overshoot() {
        // Hand-built velocity trace: fast right flick, overshoot back, settle.
        let mk = |t: f64, yaw: f64| flow::FlowSample {
            t,
            dt: 1.0 / 60.0,
            yaw_dps: yaw,
            pitch_dps: 0.0,
            translation_px: 0.0,
            quality: 0.9,
        };
        let mut trace = vec![];
        let mut t = 0.0;
        for &v in &[5.0, 10.0, 300.0, 600.0, 500.0, 200.0, -180.0, -120.0, -40.0, 8.0, 5.0, 4.0, 3.0] {
            trace.push(mk(t, v));
            t += 1.0 / 60.0;
        }
        let flicks = motion::flicks(&trace, &cfg());
        assert_eq!(flicks.len(), 1, "{flicks:?}");
        let f = &flicks[0];
        assert!(f.peak_dps > 500.0);
        assert!(f.displacement_deg > 5.0);
        assert!(f.overshoot_deg > 2.0, "overshoot {}", f.overshoot_deg);
        assert!(f.settle_ms.is_some());
    }
}
