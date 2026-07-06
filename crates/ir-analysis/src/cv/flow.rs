//! Band-differential global flow on downscaled luma frames.
//!
//! Physics: camera *rotation* produces near-uniform image flow at every
//! depth; camera *translation* produces depth-dependent parallax. CS2
//! scenes almost always have near ground at the bottom of the frame and
//! far geometry near the horizon, so:
//!   - horizon-band dx        ≈ rotation (the view-velocity trace),
//!   - ground dx − horizon dx ≈ translation (the strafing indicator).
//!
//! This is a *classifier*, not a speedometer — consumers get a ternary
//! moving/stationary/unreliable state, never a velocity claim.
//!
//! Per band we correlate 1D column-sum luma profiles between consecutive
//! frames (integral projection): robust, dependency-free, and ~0.1 ms per
//! frame pair at 480×270.

use crate::config::FlowConfig;
use crate::cv::{Calib, LumaFrame};

/// Flow between one consecutive frame pair, stamped at the later frame.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowSample {
    /// Clip-relative seconds of the later frame.
    pub t: f64,
    /// Seconds since the previous frame.
    pub dt: f64,
    /// View rotation rate, degrees/second. Positive = looking right.
    pub yaw_dps: f64,
    /// Positive = looking up.
    pub pitch_dps: f64,
    /// Ground-band dx minus horizon-band dx, downscaled px/frame —
    /// the lateral-translation (strafe) indicator.
    pub translation_px: f64,
    /// Correlation quality 0-1 (min of the bands used); low = flat scene,
    /// whiteout, or a cut.
    pub quality: f64,
}

/// Column-sum luma profile of `rows` (fractions of height), mean-removed.
fn column_profile(f: &LumaFrame, rows: (f64, f64), cols: (f64, f64)) -> Vec<f64> {
    let (w, h) = (f.w as usize, f.h as usize);
    let (r0, r1) = (
        ((rows.0 * h as f64) as usize).min(h - 1),
        ((rows.1 * h as f64) as usize).clamp(1, h),
    );
    let (c0, c1) = (
        ((cols.0 * w as f64) as usize).min(w - 1),
        ((cols.1 * w as f64) as usize).clamp(1, w),
    );
    let mut profile = vec![0.0f64; c1 - c0];
    for r in r0..r1 {
        let row = &f.data[r * w..(r + 1) * w];
        for (i, c) in (c0..c1).enumerate() {
            profile[i] += row[c] as f64;
        }
    }
    let n = (r1 - r0).max(1) as f64;
    let mean = profile.iter().sum::<f64>() / profile.len() as f64;
    for v in &mut profile {
        *v = (*v - mean) / n;
    }
    profile
}

/// Row-sum luma profile (for vertical flow), central region only.
fn row_profile(f: &LumaFrame) -> Vec<f64> {
    let (w, h) = (f.w as usize, f.h as usize);
    let (r0, r1) = (h / 5, h * 4 / 5);
    let (c0, c1) = (w / 5, w * 4 / 5);
    let mut profile = vec![0.0f64; r1 - r0];
    for (i, r) in (r0..r1).enumerate() {
        let row = &f.data[r * w..(r + 1) * w];
        profile[i] = row[c0..c1].iter().map(|&b| b as f64).sum::<f64>() / (c1 - c0) as f64;
    }
    let mean = profile.iter().sum::<f64>() / profile.len() as f64;
    for v in &mut profile {
        *v -= mean;
    }
    profile
}

fn std_dev(profile: &[f64]) -> f64 {
    (profile.iter().map(|v| v * v).sum::<f64>() / profile.len() as f64).sqrt()
}

/// Best alignment shift of `cur` against `prev` by normalized
/// cross-correlation over lags `[-search, +search]`, with parabolic
/// sub-sample refinement. Returns `(shift, peak_ncc)`.
///
/// Sign convention: positive shift means `cur[i] ≈ prev[i + shift]` —
/// scene content moved LEFT by `shift` px, i.e. the camera panned RIGHT.
pub fn correlate(prev: &[f64], cur: &[f64], search: usize) -> (f64, f64) {
    let n = prev.len().min(cur.len());
    if n < 8 {
        return (0.0, 0.0);
    }
    let search = search.min(n / 2 - 1);
    let mut best_lag = 0i64;
    let mut best = f64::MIN;
    let mut scores = vec![0.0f64; 2 * search + 1];
    for (idx, lag) in (-(search as i64)..=(search as i64)).enumerate() {
        let mut dot = 0.0;
        let mut pp = 0.0;
        let mut cc = 0.0;
        for (i, &c) in cur.iter().enumerate().take(n) {
            let j = i as i64 + lag;
            if j < 0 || j >= n as i64 {
                continue;
            }
            let p = prev[j as usize];
            dot += p * c;
            pp += p * p;
            cc += c * c;
        }
        let score = if pp > 0.0 && cc > 0.0 {
            dot / (pp.sqrt() * cc.sqrt())
        } else {
            0.0
        };
        scores[idx] = score;
        if score > best {
            best = score;
            best_lag = lag;
        }
    }
    // Parabolic refinement around the peak.
    let idx = (best_lag + search as i64) as usize;
    let refined = if idx > 0 && idx + 1 < scores.len() {
        let (a, b, c) = (scores[idx - 1], scores[idx], scores[idx + 1]);
        let denom = a - 2.0 * b + c;
        if denom.abs() > 1e-9 {
            best_lag as f64 + 0.5 * (a - c) / denom
        } else {
            best_lag as f64
        }
    } else {
        best_lag as f64
    };
    (refined, best.clamp(0.0, 1.0))
}

/// Flow trace over a sorted run of consecutive luma frames.
/// `clip_dims` is the full-resolution (width, height) `calib` was built for.
pub fn flow_trace(
    frames: &[LumaFrame],
    calib: &Calib,
    clip_dims: (u32, u32),
    cfg: &FlowConfig,
) -> Vec<FlowSample> {
    let mut out = Vec::new();
    if frames.len() < 2 {
        return out;
    }
    let full_cols = (0.05, 0.95);
    let mut prev = BandProfiles::of(&frames[0], cfg, full_cols);
    for pair in frames.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let cur = BandProfiles::of(b, cfg, full_cols);
        let dt = (b.t_us as f64 - a.t_us as f64) / 1e6;
        if dt <= 0.0 || dt > 0.25 {
            // A gap (dropped frames / non-contiguous request) — no flow.
            prev = cur;
            continue;
        }
        let search = ((b.w as f64) * cfg.search_frac) as usize;
        let vsearch = (b.h as f64 * 0.2) as usize;

        let (h_dx, h_q) = correlate(&prev.horizon, &cur.horizon, search);
        let (g_dx, g_q) = correlate(&prev.ground, &cur.ground, search);
        let (dy, v_q) = correlate(&prev.rows, &cur.rows, vsearch);

        // Texture gates: a flat band correlates with anything.
        let h_tex = std_dev(&cur.horizon) >= cfg.min_texture;
        let g_tex = std_dev(&cur.ground) >= cfg.min_texture;
        let quality = if h_tex {
            h_q.min(v_q) * if g_tex { 1.0 } else { 0.7 }
        } else {
            0.0
        };

        // ds px → full-res px → radians → °/s.
        // Horizontal: positive correlate shift = content moved left =
        // camera panned right = yaw positive.
        let sx = clip_dims.0 as f64 / b.w as f64;
        let yaw_dps = (h_dx * sx / calib.fx).atan().to_degrees() / dt;
        // Vertical: positive shift = content moved up (toward smaller row
        // indices) = camera looked down, so negate for pitch-up-positive.
        let sy = clip_dims.1 as f64 / b.h as f64;
        let pitch_dps = -(dy * sy / calib.fy).atan().to_degrees() / dt;

        out.push(FlowSample {
            t: b.t_us as f64 / 1e6,
            dt,
            yaw_dps,
            pitch_dps,
            translation_px: if g_tex && g_q > 0.2 { g_dx - h_dx } else { 0.0 },
            quality,
        });
        prev = cur;
    }
    out
}

struct BandProfiles {
    horizon: Vec<f64>,
    ground: Vec<f64>,
    rows: Vec<f64>,
}

impl BandProfiles {
    fn of(f: &LumaFrame, cfg: &FlowConfig, full_cols: (f64, f64)) -> Self {
        Self {
            horizon: column_profile(f, cfg.horizon_band, full_cols),
            ground: column_profile(f, cfg.ground_band, cfg.ground_cols),
            rows: row_profile(f),
        }
    }
}
