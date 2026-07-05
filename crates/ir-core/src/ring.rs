use std::collections::VecDeque;
use std::time::Duration;

use ir_types::{CodecConfig, EncodedPacket};

/// One closed GOP: a keyframe followed by its dependent frames.
#[derive(Debug, Clone)]
pub struct Gop {
    packets: Vec<EncodedPacket>,
    bytes: usize,
}

impl Gop {
    fn new(keyframe: EncodedPacket) -> Self {
        debug_assert!(keyframe.keyframe);
        let bytes = keyframe.data.len();
        Self {
            packets: vec![keyframe],
            bytes,
        }
    }

    fn push(&mut self, pkt: EncodedPacket) {
        self.bytes += pkt.data.len();
        self.packets.push(pkt);
    }

    pub fn start_pts(&self) -> Duration {
        self.packets[0].pts
    }

    pub fn end_pts(&self) -> Duration {
        self.packets.last().expect("gop is never empty").end_pts()
    }

    pub fn packets(&self) -> &[EncodedPacket] {
        &self.packets
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RingStats {
    pub frames_pushed: u64,
    pub gops_evicted: u64,
    /// Packets dropped because they arrived before the first keyframe
    /// (not decodable) — expected briefly at session start.
    pub dropped_pre_idr: u64,
    /// Packets dropped for non-monotonic pts — indicates a pipeline bug.
    pub dropped_non_monotonic: u64,
}

/// A consistent, cheap copy of the buffer taken at hotkey time.
/// `Bytes` payloads are refcount clones, not memcpys.
#[derive(Debug, Clone)]
pub struct RingSnapshot {
    pub codec: CodecConfig,
    /// Whole GOPs, oldest first; first packet is always a keyframe.
    pub packets: Vec<EncodedPacket>,
}

/// GOP-structured ring of encoded packets (the OBS replay-buffer pattern):
/// eviction happens only at keyframe boundaries so the front of the buffer
/// is always decodable.
#[derive(Debug)]
pub struct ReplayRing {
    codec: Option<CodecConfig>,
    /// Complete GOPs, oldest first.
    sealed: VecDeque<Gop>,
    /// GOP currently being filled (started at the most recent keyframe).
    open: Option<Gop>,
    /// Target retention window (plus one GOP of slack by construction).
    retain: Duration,
    /// Hard memory cap over packet payload bytes.
    max_bytes: usize,
    total_bytes: usize,
    stats: RingStats,
}

impl ReplayRing {
    pub fn new(retain: Duration, max_bytes: usize) -> Self {
        Self {
            codec: None,
            sealed: VecDeque::new(),
            open: None,
            retain,
            max_bytes,
            total_bytes: 0,
            stats: RingStats::default(),
        }
    }

    pub fn set_codec(&mut self, codec: CodecConfig) {
        // A new session configuration invalidates buffered packets.
        self.sealed.clear();
        self.open = None;
        self.total_bytes = 0;
        self.codec = Some(codec);
    }

    pub fn codec(&self) -> Option<&CodecConfig> {
        self.codec.as_ref()
    }

    pub fn stats(&self) -> RingStats {
        self.stats
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Duration currently covered, oldest kept frame to newest.
    pub fn span(&self) -> Duration {
        match (self.oldest_pts(), self.newest_end_pts()) {
            (Some(a), Some(b)) => b.saturating_sub(a),
            _ => Duration::ZERO,
        }
    }

    fn oldest_pts(&self) -> Option<Duration> {
        self.sealed
            .front()
            .map(Gop::start_pts)
            .or_else(|| self.open.as_ref().map(Gop::start_pts))
    }

    fn newest_end_pts(&self) -> Option<Duration> {
        self.open
            .as_ref()
            .map(Gop::end_pts)
            .or_else(|| self.sealed.back().map(Gop::end_pts))
    }

    fn last_packet_mut(&mut self) -> Option<&mut EncodedPacket> {
        self.open.as_mut().and_then(|g| g.packets.last_mut())
    }

    pub fn push(&mut self, pkt: EncodedPacket) {
        // Finalize the previous packet's duration from real inter-frame
        // timing; capture is variable-rate and the pts table must be exact.
        if let Some(prev) = self.last_packet_mut() {
            if pkt.pts <= prev.pts {
                self.stats.dropped_non_monotonic += 1;
                return;
            }
            prev.duration = pkt.pts - prev.pts;
        }

        if pkt.keyframe {
            if let Some(gop) = self.open.take() {
                self.sealed.push_back(gop);
            }
            self.open = Some(Gop::new(pkt));
        } else {
            match self.open.as_mut() {
                Some(gop) => gop.push(pkt),
                None => {
                    // Nothing decodable can start mid-GOP.
                    self.stats.dropped_pre_idr += 1;
                    return;
                }
            }
        }

        self.stats.frames_pushed += 1;
        self.total_bytes = self.sealed.iter().map(|g| g.bytes).sum::<usize>()
            + self.open.as_ref().map_or(0, |g| g.bytes);
        self.evict();
    }

    fn evict(&mut self) {
        let Some(newest) = self.newest_end_pts() else {
            return;
        };
        // Drop front GOPs as long as the buffer still covers `retain`
        // afterwards, or while over the byte cap. The open GOP is never
        // evicted.
        while let Some(front) = self.sealed.front() {
            let next_oldest = self
                .sealed
                .get(1)
                .map(Gop::start_pts)
                .or_else(|| self.open.as_ref().map(Gop::start_pts));
            let Some(next_oldest) = next_oldest else {
                break;
            };

            let over_bytes = self.total_bytes > self.max_bytes;
            let still_covered = newest.saturating_sub(next_oldest) >= self.retain;
            if over_bytes || still_covered {
                self.total_bytes -= front.bytes;
                self.sealed.pop_front();
                self.stats.gops_evicted += 1;
            } else {
                break;
            }
        }
    }

    /// Cheap consistent copy of the last `window` of footage (whole GOPs,
    /// front-aligned to a keyframe). Returns `None` before the pipeline has
    /// configured the stream or produced a keyframe.
    pub fn snapshot(&self, window: Duration) -> Option<RingSnapshot> {
        let codec = self.codec.clone()?;
        let newest = self.newest_end_pts()?;
        let cutoff = newest.saturating_sub(window);

        let mut gops: Vec<&Gop> = self.sealed.iter().chain(self.open.as_ref()).collect();
        // Keep the newest run of GOPs whose combined span covers the window:
        // find the last GOP starting at or before the cutoff and start there.
        let start = gops
            .iter()
            .rposition(|g| g.start_pts() <= cutoff)
            .unwrap_or(0);
        gops.drain(..start);

        let packets: Vec<EncodedPacket> = gops
            .iter()
            .flat_map(|g| g.packets().iter().cloned())
            .collect();
        if packets.is_empty() {
            return None;
        }
        Some(RingSnapshot { codec, packets })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use ir_types::{Codec, ColorInfo};

    fn codec() -> CodecConfig {
        CodecConfig {
            codec: Codec::H264 {
                avcc: Bytes::from_static(&[1, 2, 3]),
            },
            width: 640,
            height: 480,
            nominal_fps: 60,
            color: ColorInfo::Bt709Limited,
        }
    }

    fn pkt(ms: u64, keyframe: bool, len: usize) -> EncodedPacket {
        EncodedPacket {
            pts: Duration::from_millis(ms),
            duration: Duration::from_millis(16),
            keyframe,
            data: Bytes::from(vec![0u8; len]),
        }
    }

    /// 60 fps, 1 s GOP, `secs` seconds of packets, 100 bytes each.
    fn fill(ring: &mut ReplayRing, secs: u64) {
        for i in 0..secs * 60 {
            let ms = i * 1000 / 60;
            ring.push(pkt(ms, i % 60 == 0, 100));
        }
    }

    #[test]
    fn drops_packets_before_first_keyframe() {
        let mut ring = ReplayRing::new(Duration::from_secs(10), usize::MAX);
        ring.set_codec(codec());
        ring.push(pkt(0, false, 100));
        ring.push(pkt(16, false, 100));
        assert_eq!(ring.stats().dropped_pre_idr, 2);
        assert!(ring.snapshot(Duration::from_secs(10)).is_none());
    }

    #[test]
    fn snapshot_starts_on_keyframe_and_covers_window() {
        let mut ring = ReplayRing::new(Duration::from_secs(15), usize::MAX);
        ring.set_codec(codec());
        fill(&mut ring, 30);

        let snap = ring.snapshot(Duration::from_secs(10)).unwrap();
        assert!(snap.packets[0].keyframe);
        let span = snap.packets.last().unwrap().end_pts() - snap.packets[0].pts;
        assert!(span >= Duration::from_secs(10), "span {span:?}");
        // Whole-GOP alignment means at most one GOP of overshoot.
        assert!(span <= Duration::from_secs(11) + Duration::from_millis(50));
    }

    #[test]
    fn evicts_to_retention_window() {
        let mut ring = ReplayRing::new(Duration::from_secs(15), usize::MAX);
        ring.set_codec(codec());
        fill(&mut ring, 120);

        let span = ring.span();
        assert!(span >= Duration::from_secs(15), "span {span:?}");
        assert!(span <= Duration::from_secs(17), "span {span:?}");
        assert!(ring.stats().gops_evicted > 100);
    }

    #[test]
    fn respects_byte_cap() {
        // 100 bytes * 60 fps = 6000 B/s; cap at ~3 GOPs worth.
        let mut ring = ReplayRing::new(Duration::from_secs(3600), 18_000);
        ring.set_codec(codec());
        fill(&mut ring, 60);
        assert!(ring.total_bytes() <= 18_000);
        // Open GOP is never evicted, so something is always retained.
        assert!(ring.snapshot(Duration::from_secs(3600)).is_some());
    }

    #[test]
    fn durations_are_finalized_from_real_timing() {
        let mut ring = ReplayRing::new(Duration::from_secs(10), usize::MAX);
        ring.set_codec(codec());
        ring.push(pkt(0, true, 100));
        ring.push(pkt(30, false, 100)); // jittery 30 ms gap
        ring.push(pkt(46, false, 100));
        let snap = ring.snapshot(Duration::from_secs(10)).unwrap();
        assert_eq!(snap.packets[0].duration, Duration::from_millis(30));
        assert_eq!(snap.packets[1].duration, Duration::from_millis(16));
    }

    #[test]
    fn non_monotonic_pts_dropped() {
        let mut ring = ReplayRing::new(Duration::from_secs(10), usize::MAX);
        ring.set_codec(codec());
        ring.push(pkt(100, true, 100));
        ring.push(pkt(100, false, 100));
        ring.push(pkt(50, false, 100));
        assert_eq!(ring.stats().dropped_non_monotonic, 2);
        assert_eq!(ring.stats().frames_pushed, 1);
    }

    #[test]
    fn set_codec_resets_buffer() {
        let mut ring = ReplayRing::new(Duration::from_secs(10), usize::MAX);
        ring.set_codec(codec());
        fill(&mut ring, 5);
        assert!(ring.total_bytes() > 0);
        ring.set_codec(codec());
        assert_eq!(ring.total_bytes(), 0);
        assert!(ring.snapshot(Duration::from_secs(10)).is_none());
    }
}
