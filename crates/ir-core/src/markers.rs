use std::collections::VecDeque;
use std::time::Duration;

use ir_types::Marker;

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
