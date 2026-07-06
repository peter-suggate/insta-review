//! AI aim/movement coaching for captured clips.
//!
//! Division of labor: the desktop webview decodes and extracts frames (its
//! WebCodecs path is proven and hardware-accelerated); this crate owns
//! everything after — extraction planning, prompt composition, headless LLM
//! CLI invocation (`claude -p` / `codex exec` under the user's flat-rate
//! subscriptions), parsing, and run persistence. Nothing here talks to
//! Tauri, so `ir-cli` can drive the same pipeline headless later.

pub mod llm;
pub mod prompt;
pub mod store;
pub mod types;

pub use tokio::sync::Notify as CancelSignal;

use types::{EventRef, ExtractionPlan, FrameRequest};

/// Moments around the event worth showing the LLM, in seconds relative to
/// the event. The lead-up matters most; one frame after confirms the result.
const EVIDENCE_OFFSETS_S: &[f64] = &[-1.5, -1.0, -0.6, -0.35, -0.15, 0.0, 0.35];

/// Choose the frames to extract for an event, snapped to real sample
/// timestamps (`frame_pts`, clip-relative seconds) and deduplicated.
pub fn plan_extraction(event: &EventRef, frame_pts: &[f64]) -> ExtractionPlan {
    let mut frames: Vec<FrameRequest> = Vec::new();
    for off in EVIDENCE_OFFSETS_S {
        let target = event.at_s + off;
        let Some(t_us) = nearest_frame_us(frame_pts, target) else {
            continue;
        };
        if frames.iter().any(|f| f.t_us == t_us) {
            continue;
        }
        frames.push(FrameRequest {
            t_us,
            want_jpeg: true,
            want_raw: false,
        });
    }
    frames.sort_by_key(|f| f.t_us);
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
    fn plan_snaps_to_frames_and_dedupes() {
        // 60 fps frame grid.
        let pts: Vec<f64> = (0..900).map(|i| i as f64 / 60.0).collect();
        let plan = plan_extraction(&event(9.2), &pts);
        assert_eq!(plan.frames.len(), EVIDENCE_OFFSETS_S.len());
        // Snapped exactly onto the grid, strictly increasing.
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
    }
}
