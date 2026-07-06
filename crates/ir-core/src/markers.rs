use std::collections::VecDeque;
use std::time::Duration;

use ir_types::{GsiSample, Marker};

/// Time-bounded log of timeline markers (GSI events), same retention policy
/// as the video ring.
#[derive(Debug)]
pub struct MarkerLog {
    markers: VecDeque<Marker>,
    retain: Duration,
}

impl MarkerLog {
    pub fn new(retain: Duration) -> Self {
        Self {
            markers: VecDeque::new(),
            retain,
        }
    }

    pub fn push(&mut self, marker: Marker) {
        let cutoff = marker.ts.saturating_sub(self.retain);
        while self.markers.front().is_some_and(|m| m.ts < cutoff) {
            self.markers.pop_front();
        }
        self.markers.push_back(marker);
    }

    /// Markers in `[from, to]`, still in capture-clock time.
    pub fn range(&self, from: Duration, to: Duration) -> Vec<Marker> {
        self.markers
            .iter()
            .filter(|m| m.ts >= from && m.ts <= to)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.markers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.markers.is_empty()
    }
}

/// Time-bounded log of GSI state samples (~10 Hz), same retention policy
/// as the video ring. The continuous-state sibling of [`MarkerLog`].
#[derive(Debug)]
pub struct GsiTraceLog {
    samples: VecDeque<GsiSample>,
    retain: Duration,
}

impl GsiTraceLog {
    pub fn new(retain: Duration) -> Self {
        Self {
            samples: VecDeque::new(),
            retain,
        }
    }

    pub fn push(&mut self, sample: GsiSample) {
        let cutoff = sample.ts.saturating_sub(self.retain);
        while self.samples.front().is_some_and(|s| s.ts < cutoff) {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// Samples in `[from, to]`, still in capture-clock time.
    pub fn range(&self, from: Duration, to: Duration) -> Vec<GsiSample> {
        self.samples
            .iter()
            .filter(|s| s.ts >= from && s.ts <= to)
            .cloned()
            .collect()
    }
}
