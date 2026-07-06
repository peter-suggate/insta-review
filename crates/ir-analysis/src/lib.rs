//! AI aim/movement coaching for captured clips.
//!
//! Division of labor: the desktop webview decodes and extracts frames (its
//! WebCodecs path is proven and hardware-accelerated); this crate owns
//! everything after — extraction planning, prompt composition, headless LLM
//! CLI invocation (`claude -p` / `codex exec` under the user's flat-rate
//! subscriptions), parsing, and run persistence. Nothing here talks to
//! Tauri, so `ir-cli` can drive the same pipeline headless later.

pub mod config;
pub mod cv;
pub mod llm;
pub mod parse;
pub mod prompt;
pub mod store;
pub mod types;

pub use tokio::sync::Notify as CancelSignal;

use types::{EventRef, ExtractionPlan, FrameRequest};

/// Moments around the event worth showing the LLM, in seconds relative to
/// the event. The lead-up matters most; one frame after confirms the result.
const EVIDENCE_OFFSETS_S: &[f64] = &[-1.5, -1.0, -0.6, -0.35, -0.15, 0.0, 0.35];

/// CV analysis window around the event: enough lead-up to see the peek,
/// the counter-strafe, and the flick; a little after for the result.
const CV_BEFORE_S: f64 = 6.0;
const CV_AFTER_S: f64 = 1.0;

/// Choose the frames to extract for an event: full-res JPEG evidence at
/// sparse offsets, plus a dense downscaled-luma run over the CV window.
/// All snapped to real sample timestamps (`frame_pts`) and deduplicated.
pub fn plan_extraction(event: &EventRef, frame_pts: &[f64]) -> ExtractionPlan {
    let mut jpeg_ts = Vec::new();
    for off in EVIDENCE_OFFSETS_S {
        if let Some(t_us) = nearest_frame_us(frame_pts, event.at_s + off) {
            jpeg_ts.push(t_us);
        }
    }
    let mut frames: Vec<FrameRequest> = frame_pts
        .iter()
        .filter(|&&t| t >= event.at_s - CV_BEFORE_S && t <= event.at_s + CV_AFTER_S)
        .map(|&t| {
            let t_us = (t.max(0.0) * 1_000_000.0).round() as u64;
            FrameRequest {
                t_us,
                want_jpeg: jpeg_ts.contains(&t_us),
                want_raw: true,
            }
        })
        .collect();
    // Evidence frames outside the CV window (clip shorter than the window).
    for t_us in jpeg_ts {
        if !frames.iter().any(|f| f.t_us == t_us) {
            frames.push(FrameRequest {
                t_us,
                want_jpeg: true,
                want_raw: false,
            });
        }
    }
    frames.sort_by_key(|f| f.t_us);
    frames.dedup_by_key(|f| f.t_us);
    ExtractionPlan { frames }
}

fn nearest_frame_us(frame_pts: &[f64], target_s: f64) -> Option<u64> {
    let idx = frame_pts.partition_point(|&t| t < target_s);
    let candidates = [idx.checked_sub(1), Some(idx)];
    let best = candidates
        .into_iter()
        .flatten()
        .filter_map(|i| frame_pts.get(i).map(|&t| (i, t)))
        .min_by(|a, b| {
            (a.1 - target_s)
                .abs()
                .total_cmp(&(b.1 - target_s).abs())
        })?;
    Some((best.1.max(0.0) * 1_000_000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ir_types::MarkerKind;

    fn event(at_s: f64) -> EventRef {
        EventRef {
            id: format!("kill_{}ms", (at_s * 1000.0) as u64),
            at_s,
            kind: MarkerKind::Kill {
                count: 1,
                headshot: false,
            },
        }
    }

    #[test]
    fn plan_covers_cv_window_plus_evidence() {
        // 60 fps, 15 s clip; event at 9.2 s → CV window 3.2..10.2 s.
        let pts: Vec<f64> = (0..900).map(|i| i as f64 / 60.0).collect();
        let plan = plan_extraction(&event(9.2), &pts);
        let raw = plan.frames.iter().filter(|f| f.want_raw).count();
        let jpeg = plan.frames.iter().filter(|f| f.want_jpeg).count();
        assert_eq!(raw, 7 * 60 + 1, "dense CV window @60fps");
        assert_eq!(jpeg, EVIDENCE_OFFSETS_S.len());
        // Strictly increasing, snapped to the grid.
        for pair in plan.frames.windows(2) {
            assert!(pair[0].t_us < pair[1].t_us);
        }
        for f in &plan.frames {
            let snapped = pts
                .iter()
                .any(|&t| ((t * 1_000_000.0).round() as u64) == f.t_us);
            assert!(snapped, "not on the frame grid: {}", f.t_us);
        }
    }

    #[test]
    fn plan_near_clip_start_clamps() {
        let pts: Vec<f64> = (0..120).map(|i| i as f64 / 60.0).collect();
        let plan = plan_extraction(&event(0.2), &pts);
        assert!(!plan.frames.is_empty());
        assert_eq!(plan.frames[0].t_us, 0);
        // Negative-time offsets all clamp to frame 0; only the distinct
        // snapped timestamps remain.
        assert!(plan.frames.iter().filter(|f| f.want_jpeg).count() >= 3);
    }
}
