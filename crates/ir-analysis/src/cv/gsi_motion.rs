//! Measured motion from the GSI `player_position` trace: horizontal speed
//! (units/second) from position deltas and view angles from the forward
//! vector, at GSI cadence (~10 Hz, receipt-time latency).
//!
//! Where these exist they outrank the optical-flow *classifier*: a
//! measurement beats an inference. Where they don't (older cfg installs,
//! menus, spectating), everything degrades gracefully to flow.

use ir_types::ClipGsiSample;
use serde::Serialize;

use crate::cv::flow::FlowSample;

/// Horizontal speed between two position samples, stamped at the midpoint.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedSample {
    pub t: f64,
    /// Game units/second, horizontal plane (jumps/falls excluded from the
    /// accuracy question).
    pub ups: f64,
}

/// View angles from the forward vector.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewSample {
    pub t: f64,
    /// Degrees, atan2(y, x) — only deltas are meaningful to us.
    pub yaw_deg: f64,
    pub pitch_deg: f64,
}

/// Position deltas → speed. Gaps > 0.5 s (dropped payloads, round reset)
/// and implausible jumps (teleport/respawn, > 1000 u/s) produce no sample.
pub fn speed_trace(gsi: &[ClipGsiSample], gsi_offset_s: f64) -> Vec<SpeedSample> {
    let mut out = Vec::new();
    for pair in gsi.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let dt = b.at - a.at;
        if dt <= 0.0 || dt > 0.5 {
            continue;
        }
        let (Some(pa), Some(pb)) = (a.state.position, b.state.position) else {
            continue;
        };
        let ups = ((pb[0] - pa[0]).powi(2) + (pb[1] - pa[1]).powi(2)).sqrt() / dt;
        if ups > 1000.0 {
            continue;
        }
        out.push(SpeedSample {
            t: ((a.at + b.at) / 2.0 + gsi_offset_s).max(0.0),
            ups,
        });
    }
    out
}

pub fn view_trace(gsi: &[ClipGsiSample], gsi_offset_s: f64) -> Vec<ViewSample> {
    gsi.iter()
        .filter_map(|s| {
            let f = s.state.forward?;
            Some(ViewSample {
                t: (s.at + gsi_offset_s).max(0.0),
                yaw_deg: f[1].atan2(f[0]).to_degrees(),
                pitch_deg: f[2].clamp(-1.0, 1.0).asin().to_degrees(),
            })
        })
        .collect()
}

/// Interpolated speed at `t`, only when bracketing samples are close enough
/// to trust (`bracket_s` each side). None = not measured there.
pub fn speed_at(trace: &[SpeedSample], t: f64, bracket_s: f64) -> Option<f64> {
    let idx = trace.partition_point(|s| s.t < t);
    match (idx.checked_sub(1).and_then(|i| trace.get(i)), trace.get(idx)) {
        (Some(a), Some(b)) if t - a.t <= bracket_s && b.t - t <= bracket_s => {
            let span = (b.t - a.t).max(1e-9);
            Some(a.ups + (b.ups - a.ups) * (t - a.t) / span)
        }
        (Some(a), None) if t - a.t <= bracket_s / 2.0 => Some(a.ups),
        (None, Some(b)) if b.t - t <= bracket_s / 2.0 => Some(b.ups),
        _ => None,
    }
}

/// Latest time ≤ `before_t` where speed crossed below `threshold` —
/// the measured "stopped moving" moment (linear-interpolated crossing).
pub fn last_stop_crossing(trace: &[SpeedSample], before_t: f64, threshold: f64) -> Option<f64> {
    trace
        .windows(2)
        .rev()
        .filter(|w| w[1].t <= before_t + 1e-9)
        .find(|w| w[0].ups > threshold && w[1].ups <= threshold)
        .map(|w| {
            let frac = (w[0].ups - threshold) / (w[0].ups - w[1].ups).max(1e-9);
            w[0].t + frac * (w[1].t - w[0].t)
        })
}

fn wrap_deg(d: f64) -> f64 {
    let mut d = d % 360.0;
    if d > 180.0 {
        d -= 360.0;
    }
    if d < -180.0 {
        d += 360.0;
    }
    d
}

/// Compare integrated optical-flow yaw against GSI view-direction deltas
/// over the same spans: `Some(flow_total / gsi_total)` when there's enough
/// rotation to judge (≥ 20° across ≥ 5 spans). ~1.0 = flow is calibrated;
/// far off = wrong FOV/stretch assumption or broken flow.
pub fn flow_vs_gsi_yaw_ratio(flow: &[FlowSample], view: &[ViewSample]) -> Option<f64> {
    let mut flow_total = 0.0;
    let mut gsi_total = 0.0;
    let mut spans = 0;
    for pair in view.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if b.t - a.t > 0.4 || b.t <= a.t {
            continue;
        }
        let gsi_delta = wrap_deg(b.yaw_deg - a.yaw_deg).abs();
        if gsi_delta < 2.0 {
            continue; // too small to measure against
        }
        let flow_delta: f64 = flow
            .iter()
            .filter(|s| s.t > a.t && s.t <= b.t)
            .map(|s| s.yaw_dps * s.dt)
            .sum();
        flow_total += flow_delta.abs();
        gsi_total += gsi_delta;
        spans += 1;
    }
    (spans >= 5 && gsi_total >= 20.0).then(|| flow_total / gsi_total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ir_types::GsiState;

    fn s(at: f64, pos: [f64; 3], fwd: [f64; 3]) -> ClipGsiSample {
        ClipGsiSample {
            at,
            state: GsiState {
                position: Some(pos),
                forward: Some(fwd),
                ..Default::default()
            },
        }
    }

    #[test]
    fn speed_from_position_deltas() {
        // 25 units per 0.1 s = 250 u/s (a run), then stationary.
        let trace = vec![
            s(0.0, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
            s(0.1, [25.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
            s(0.2, [50.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
            s(0.3, [50.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        let speeds = speed_trace(&trace, 0.0);
        assert_eq!(speeds.len(), 3);
        assert!((speeds[0].ups - 250.0).abs() < 1e-6);
        assert!((speeds[2].ups - 0.0).abs() < 1e-6);
        // Interpolation midway between the fast and stopped samples.
        let mid = speed_at(&speeds, 0.2, 0.4).unwrap();
        assert!(mid > 0.0 && mid < 250.0);
    }

    #[test]
    fn teleports_and_gaps_are_skipped() {
        let trace = vec![
            s(0.0, [0.0, 0.0, 0.0], [1.0, 0.0, 0.0]),
            s(0.1, [500.0, 0.0, 0.0], [1.0, 0.0, 0.0]), // 5000 u/s: respawn
            s(1.5, [500.0, 25.0, 0.0], [1.0, 0.0, 0.0]), // 1.4 s gap
        ];
        assert!(speed_trace(&trace, 0.0).is_empty());
    }

    #[test]
    fn view_angles_and_wrap() {
        let trace = vec![
            s(0.0, [0.0; 3], [1.0, 0.0, 0.0]),
            s(0.1, [0.0; 3], [0.0, 1.0, 0.0]),
            s(0.2, [0.0; 3], [0.0, 0.0, 1.0]),
        ];
        let view = view_trace(&trace, 0.0);
        assert!((view[0].yaw_deg - 0.0).abs() < 1e-9);
        assert!((view[1].yaw_deg - 90.0).abs() < 1e-9);
        assert!((view[2].pitch_deg - 90.0).abs() < 1e-9);
        assert!((wrap_deg(179.0 - -179.0) - -2.0).abs() < 1e-9);
    }

    #[test]
    fn stop_crossing_is_interpolated() {
        let speeds = vec![
            SpeedSample { t: 0.0, ups: 250.0 },
            SpeedSample { t: 0.1, ups: 250.0 },
            SpeedSample { t: 0.2, ups: 0.0 }, // crossed 30 u/s within (0.1, 0.2)
            SpeedSample { t: 0.3, ups: 0.0 },
        ];
        let stop = last_stop_crossing(&speeds, 0.3, 30.0).unwrap();
        assert!(stop > 0.1 && stop < 0.2, "stop {stop}");
    }

    #[test]
    fn yaw_ratio_near_one_for_agreeing_traces() {
        // GSI: 10°/100ms turns. Flow: matching 100 °/s.
        let view: Vec<ViewSample> = (0..10)
            .map(|i| ViewSample {
                t: i as f64 * 0.1,
                yaw_deg: i as f64 * 10.0,
                pitch_deg: 0.0,
            })
            .collect();
        let flow: Vec<FlowSample> = (0..60)
            .map(|i| FlowSample {
                t: i as f64 / 60.0,
                dt: 1.0 / 60.0,
                yaw_dps: 100.0,
                pitch_dps: 0.0,
                translation_px: 0.0,
                quality: 0.9,
            })
            .collect();
        let ratio = flow_vs_gsi_yaw_ratio(&flow, &view).unwrap();
        assert!((ratio - 1.0).abs() < 0.1, "ratio {ratio}");
    }
}
