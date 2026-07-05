//! Property tests: the ring's invariants must hold for arbitrary packet
//! streams (jittery timing, variable sizes, odd GOP lengths).

use std::time::Duration;

use bytes::Bytes;
use ir_core::ReplayRing;
use ir_types::{Codec, CodecConfig, ColorInfo, EncodedPacket};
use proptest::prelude::*;

fn codec() -> CodecConfig {
    CodecConfig {
        codec: Codec::H264 {
            avcc: Bytes::from_static(&[1]),
        },
        width: 640,
        height: 480,
        nominal_fps: 60,
        color: ColorInfo::Bt709Limited,
    }
}

/// A generated stream: GOP lengths, per-frame pts increments (ms), sizes.
#[derive(Debug, Clone)]
struct Stream {
    gop_lens: Vec<usize>,
    jitter_ms: Vec<u64>,
    sizes: Vec<usize>,
}

fn stream_strategy() -> impl Strategy<Value = Stream> {
    (
        prop::collection::vec(1usize..90, 2..30),
        prop::collection::vec(1u64..50, 1..2000),
        prop::collection::vec(1usize..5000, 1..2000),
    )
        .prop_map(|(gop_lens, jitter_ms, sizes)| Stream {
            gop_lens,
            jitter_ms,
            sizes,
        })
}

fn packets(s: &Stream) -> Vec<EncodedPacket> {
    let total: usize = s.gop_lens.iter().sum();
    let mut out = Vec::with_capacity(total);
    let mut pts_ms: u64 = 0;
    let mut i = 0;
    for &gop in &s.gop_lens {
        for k in 0..gop {
            pts_ms += s.jitter_ms[i % s.jitter_ms.len()];
            out.push(EncodedPacket {
                pts: Duration::from_millis(pts_ms),
                duration: Duration::from_millis(16),
                keyframe: k == 0,
                data: Bytes::from(vec![0u8; s.sizes[i % s.sizes.len()]]),
            });
            i += 1;
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// With no byte pressure: the snapshot always starts on a keyframe, has
    /// strictly increasing pts, and covers min(retain, pushed span).
    #[test]
    fn coverage_and_decodability(s in stream_strategy(), retain_s in 1u64..30) {
        let retain = Duration::from_secs(retain_s);
        let mut ring = ReplayRing::new(retain, usize::MAX);
        ring.set_codec(codec());
        let pkts = packets(&s);
        let first_pts = pkts[0].pts;
        let mut last_end = first_pts;
        for p in pkts {
            last_end = last_end.max(p.end_pts());
            ring.push(p);
        }
        let pushed_span = last_end - first_pts;

        if let Some(snap) = ring.snapshot(retain) {
            prop_assert!(snap.packets[0].keyframe);
            for w in snap.packets.windows(2) {
                prop_assert!(w[1].pts > w[0].pts);
            }
            // Coverage: eviction never cuts into the retention window.
            let snap_span = snap.packets.last().unwrap().end_pts() - snap.packets[0].pts;
            let want = retain.min(pushed_span);
            // One frame of slack at the newest edge (open packet duration
            // isn't finalized until the next frame arrives).
            prop_assert!(
                snap_span + Duration::from_millis(50) >= want,
                "span {snap_span:?} < wanted {want:?}"
            );
        }
    }

    /// With a byte cap: memory stays bounded by cap + one GOP (the open GOP
    /// is never evicted; a sealed front GOP is only kept if within budget).
    #[test]
    fn byte_cap_respected(s in stream_strategy(), cap_kb in 1usize..200) {
        let cap = cap_kb * 1024;
        let mut ring = ReplayRing::new(Duration::from_secs(3600), cap);
        ring.set_codec(codec());

        let pkts = packets(&s);
        // Largest possible GOP payload in this stream bounds the overshoot.
        let mut max_gop_bytes = 0usize;
        let mut cur = 0usize;
        for p in &pkts {
            if p.keyframe {
                max_gop_bytes = max_gop_bytes.max(cur);
                cur = 0;
            }
            cur += p.data.len();
        }
        max_gop_bytes = max_gop_bytes.max(cur);

        for p in pkts {
            ring.push(p);
            prop_assert!(
                ring.total_bytes() <= cap + max_gop_bytes,
                "total {} > cap {} + max gop {}",
                ring.total_bytes(), cap, max_gop_bytes
            );
        }
    }
}
