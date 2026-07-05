use std::time::Duration;

use ir_types::{ClipMarker, ClipMeta, CodecConfig, EncodedPacket, Marker};

use crate::ring::RingSnapshot;

/// A frozen, reviewable clip: the samples plus everything the player needs.
/// MP4 muxing (for save/share) is a separate concern — see `ir-mux`.
#[derive(Debug, Clone)]
pub struct Clip {
    pub codec: CodecConfig,
    /// Whole GOPs, first packet is a keyframe. Pts are still capture-clock
    /// absolute; `meta.frame_pts` carries the clip-relative table.
    pub samples: Vec<EncodedPacket>,
    pub meta: ClipMeta,
}

/// Assemble a clip from a ring snapshot, the markers in its window, and the
/// trigger (hotkey) time. All times are rebased to clip-relative seconds.
pub fn build_clip(snap: RingSnapshot, markers: Vec<Marker>, trigger_ts: Duration) -> Clip {
    let base = snap.packets[0].pts;
    let rel = |ts: Duration| ts.saturating_sub(base).as_secs_f64();

    let frame_pts: Vec<f64> = snap.packets.iter().map(|p| rel(p.pts)).collect();
    let keyframe_indices: Vec<u32> = snap
        .packets
        .iter()
        .enumerate()
        .filter(|(_, p)| p.keyframe)
        .map(|(i, _)| i as u32)
        .collect();
    let markers: Vec<ClipMarker> = markers
        .into_iter()
        .map(|m| ClipMarker {
            at: rel(m.ts),
            kind: m.kind,
        })
        .collect();

    let meta = ClipMeta {
        width: snap.codec.width,
        height: snap.codec.height,
        nominal_fps: snap.codec.nominal_fps,
        frame_pts,
        keyframe_indices,
        markers,
        trigger_at: rel(trigger_ts),
    };

    Clip {
        codec: snap.codec,
        samples: snap.packets,
        meta,
    }
}
