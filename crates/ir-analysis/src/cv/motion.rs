//! Movement/flick/shot analysis over the flow trace + GSI state trace,
//! producing candidate findings for the LLM to validate or reject.
//! Honesty rules: movement is ternary (never a velocity), every candidate
//! carries the measurement uncertainty that produced it, and low-quality
//! intervals produce nothing at all.

use ir_types::ClipGsiSample;
use serde::Serialize;

use crate::config::AnalysisConfig;
use crate::cv::flow::FlowSample;
use crate::types::Severity;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveState {
    Stationary,
    Moving,
    Unreliable,
}

/// Contiguous run of one movement state.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovementInterval {
    pub start_s: f64,
    pub end_s: f64,
    pub state: MoveState,
}

/// A burst of shots inferred from GSI ammo decrements. Timing is coarse:
/// the decrement happened somewhere in `(t - uncertainty_s, t]`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShotEvent {
    pub t: f64,
    pub count: u32,
    pub uncertainty_s: f64,
    pub weapon: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Flick {
    pub t_peak: f64,
    pub peak_dps: f64,
    pub displacement_deg: f64,
    pub overshoot_deg: f64,
    /// Peak → settled, milliseconds; None = never settled in the window.
    pub settle_ms: Option<f64>,
}

/// A CV-detected candidate finding, pre-LLM. Same vocabulary as
/// [`crate::types::Finding`] but carries only what CV can honestly claim.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub kind: String,
    pub severity: Severity,
    pub confidence: f32,
    pub start_s: f64,
    pub end_s: f64,
    pub metrics: serde_json::Value,
    pub note: String,
}

/// Classify each flow sample, then merge into intervals with hysteresis
/// (gaps shorter than `min_frames` don't flip the state).
pub fn movement_intervals(
    flow: &[FlowSample],
    gsi: &[ClipGsiSample],
    cfg: &AnalysisConfig,
) -> Vec<MovementInterval> {
    if flow.is_empty() {
        return vec![];
    }
    let m = &cfg.movement;
    let flashed_at = |t: f64| {
        gsi.iter()
            .min_by(|a, b| (a.at - t).abs().total_cmp(&(b.at - t).abs()))
            .map(|s| s.state.flashed)
            .unwrap_or(0)
    };
    // Per-sample raw state.
    let raw: Vec<MoveState> = flow
        .iter()
        .map(|s| {
            if s.quality < m.min_quality || flashed_at(s.t) > m.flashed_max {
                MoveState::Unreliable
            } else if s.translation_px.abs() > m.translation_px {
                MoveState::Moving
            } else {
                MoveState::Stationary
            }
        })
        .collect();

    // Debounce Moving: require min_frames sustained; shorter blips revert
    // to stationary (false accusations are worse than false negatives).
    let mut states = raw.clone();
    let mut i = 0;
    while i < states.len() {
        if states[i] == MoveState::Moving {
            let mut j = i;
            while j < states.len() && states[j] == MoveState::Moving {
                j += 1;
            }
            if j - i < m.min_frames {
                for s in &mut states[i..j] {
                    *s = MoveState::Stationary;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }

    // Merge into intervals.
    let mut intervals: Vec<MovementInterval> = Vec::new();
    for (s, state) in flow.iter().zip(states) {
        match intervals.last_mut() {
            Some(last) if last.state == state => last.end_s = s.t,
            _ => intervals.push(MovementInterval {
                start_s: s.t - s.dt,
                end_s: s.t,
                state,
            }),
        }
    }
    intervals
}

fn state_at(intervals: &[MovementInterval], t: f64) -> Option<MoveState> {
    intervals
        .iter()
        .find(|iv| t >= iv.start_s && t <= iv.end_s)
        .map(|iv| iv.state)
}

/// Last moving→stationary transition at or before `t`.
fn last_stop_before(intervals: &[MovementInterval], t: f64) -> Option<f64> {
    intervals
        .windows(2)
        .rev()
        .find(|w| {
            w[0].state == MoveState::Moving
                && w[1].state == MoveState::Stationary
                && w[1].start_s <= t
        })
        .map(|w| w[1].start_s)
}

/// Shot bursts from GSI ammo decrements. `gsi_offset_s` shifts receipt
/// times onto the video timeline (GSI lags the game).
pub fn shots_from_gsi(gsi: &[ClipGsiSample], gsi_offset_s: f64) -> Vec<ShotEvent> {
    let mut out = Vec::new();
    for pair in gsi.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        // Same weapon, ammo decreased → shots. Weapon switch or increase
        // (reload/respawn) is not fire.
        if a.state.weapon != b.state.weapon || a.state.weapon.is_empty() {
            continue;
        }
        if let (Some(before), Some(after)) = (a.state.ammo_clip, b.state.ammo_clip) {
            if after < before {
                out.push(ShotEvent {
                    t: (b.at + gsi_offset_s).max(0.0),
                    count: before - after,
                    uncertainty_s: (b.at - a.at).max(0.05),
                    weapon: b.state.weapon.clone(),
                });
            }
        }
    }
    out
}

/// Extract flicks from the view-velocity trace.
pub fn flicks(flow: &[FlowSample], cfg: &AnalysisConfig) -> Vec<Flick> {
    let f = &cfg.flick;
    let speed = |s: &FlowSample| s.yaw_dps.hypot(s.pitch_dps);
    let mut out = Vec::new();
    let mut i = 0;
    while i < flow.len() {
        if speed(&flow[i]) < f.peak_dps {
            i += 1;
            continue;
        }
        // Expand the burst around the fast region: back to the last slow
        // sample, forward until settled.
        let mut start = i;
        while start > 0 && speed(&flow[start - 1]) > f.settle_dps {
            start -= 1;
        }
        let mut peak_idx = i;
        let mut end = i;
        while end + 1 < flow.len() {
            end += 1;
            if speed(&flow[end]) > speed(&flow[peak_idx]) {
                peak_idx = end;
            }
            // Settled: settle_frames consecutive slow samples.
            if end + f.settle_frames <= flow.len()
                && flow[end..end + f.settle_frames]
                    .iter()
                    .all(|s| speed(s) < f.settle_dps)
            {
                break;
            }
        }

        let displacement: f64 = flow[start..=end.min(flow.len() - 1)]
            .iter()
            .map(|s| s.yaw_dps * s.dt)
            .sum();
        if displacement.abs() >= f.min_displacement_deg {
            // Overshoot: yaw integrated against the main direction after
            // the peak.
            let dir = displacement.signum();
            let overshoot: f64 = flow[peak_idx..=end.min(flow.len() - 1)]
                .iter()
                .filter(|s| s.yaw_dps.signum() != dir)
                .map(|s| (s.yaw_dps * s.dt).abs())
                .sum();
            let settled = end + f.settle_frames <= flow.len();
            out.push(Flick {
                t_peak: flow[peak_idx].t,
                peak_dps: speed(&flow[peak_idx]),
                displacement_deg: displacement.abs(),
                overshoot_deg: overshoot,
                settle_ms: settled.then(|| (flow[end].t - flow[peak_idx].t) * 1000.0),
            });
        }
        i = end + 1;
    }
    out
}

/// Candidate findings around the event from movement × shots × flicks.
pub fn candidates(
    event_at_s: f64,
    intervals: &[MovementInterval],
    shots: &[ShotEvent],
    flicks: &[Flick],
    cfg: &AnalysisConfig,
) -> Vec<Candidate> {
    let cs = &cfg.counter_strafe;
    let mut out = Vec::new();

    // Shots in the 3 s leading to the event (the fight, not old noise).
    let fight: Vec<&ShotEvent> = shots
        .iter()
        .filter(|s| s.t >= event_at_s - 3.0 && s.t <= event_at_s + 0.5)
        .collect();

    for shot in &fight {
        if cs.exempt_weapons.iter().any(|w| shot.weapon.starts_with(w)) {
            continue;
        }
        // GSI shot timing is coarse — confidence discounts for it.
        let timing_penalty = (1.0 - shot.uncertainty_s.min(0.5)) as f32;
        match state_at(intervals, shot.t) {
            Some(MoveState::Moving) => {
                out.push(Candidate {
                    kind: "moving_while_shooting".into(),
                    severity: Severity::Major,
                    confidence: (0.75 * timing_penalty).max(0.3),
                    start_s: shot.t - shot.uncertainty_s,
                    end_s: shot.t,
                    metrics: serde_json::json!({
                        "shots": shot.count,
                        "weapon": shot.weapon,
                        "shotTimeUncertaintyS": shot.uncertainty_s,
                    }),
                    note: format!(
                        "{} shot(s) fired while the movement classifier reads 'moving'",
                        shot.count
                    ),
                });
            }
            Some(MoveState::Stationary) => {
                if let Some(stop_t) = last_stop_before(intervals, shot.t) {
                    let gap_ms = (shot.t - stop_t) * 1000.0;
                    if gap_ms < cs.settle_ms {
                        out.push(Candidate {
                            kind: "fired_before_settled".into(),
                            severity: Severity::Minor,
                            confidence: (0.6 * timing_penalty).max(0.25),
                            start_s: stop_t,
                            end_s: shot.t,
                            metrics: serde_json::json!({
                                "stopToShotMs": gap_ms,
                                "shotTimeUncertaintyS": shot.uncertainty_s,
                            }),
                            note: format!("first shot ~{gap_ms:.0} ms after stopping"),
                        });
                    } else if gap_ms <= cs.good_ms {
                        out.push(Candidate {
                            kind: "good_counter_strafe".into(),
                            severity: Severity::Positive,
                            confidence: (0.65 * timing_penalty).max(0.25),
                            start_s: stop_t,
                            end_s: shot.t,
                            metrics: serde_json::json!({ "stopToShotMs": gap_ms }),
                            note: format!("stopped ~{gap_ms:.0} ms before firing"),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // Flicks in the 1.5 s before the event.
    for flick in flicks
        .iter()
        .filter(|f| f.t_peak >= event_at_s - 1.5 && f.t_peak <= event_at_s + 0.2)
    {
        if flick.overshoot_deg >= cfg.flick.overshoot_deg {
            out.push(Candidate {
                kind: "flick_overshoot".into(),
                severity: Severity::Minor,
                confidence: 0.6,
                start_s: flick.t_peak - 0.15,
                end_s: flick.t_peak + flick.settle_ms.unwrap_or(200.0) / 1000.0,
                metrics: serde_json::json!({
                    "overshootDeg": flick.overshoot_deg,
                    "peakDps": flick.peak_dps,
                    "displacementDeg": flick.displacement_deg,
                }),
                note: format!(
                    "flick of {:.1}° overshot by ~{:.1}°",
                    flick.displacement_deg, flick.overshoot_deg
                ),
            });
        } else if flick.settle_ms.is_some_and(|ms| ms < 150.0) {
            out.push(Candidate {
                kind: "clean_flick".into(),
                severity: Severity::Positive,
                confidence: 0.6,
                start_s: flick.t_peak - 0.15,
                end_s: flick.t_peak + 0.15,
                metrics: serde_json::json!({
                    "peakDps": flick.peak_dps,
                    "displacementDeg": flick.displacement_deg,
                    "settleMs": flick.settle_ms,
                }),
                note: format!(
                    "{:.1}° flick settled in {:.0} ms",
                    flick.displacement_deg,
                    flick.settle_ms.unwrap_or(0.0)
                ),
            });
        }
    }

    out
}
