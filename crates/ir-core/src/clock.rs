use std::time::{Duration, Instant};

/// The single timeline everything is stamped against: frame pts, the hotkey
/// press, and GSI events. One epoch per engine session.
///
/// `Instant` is QPC-backed on Windows and mach_absolute_time-backed on macOS,
/// so platform pipelines that receive hardware timestamps in those units can
/// convert losslessly.
#[derive(Debug, Clone, Copy)]
pub struct CaptureClock {
    epoch: Instant,
}

impl CaptureClock {
    pub fn start() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }

    /// Time since the epoch.
    pub fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    /// Convert an `Instant` (e.g. captured in a callback) to clock time.
    /// Saturates to zero for instants before the epoch.
    pub fn at(&self, instant: Instant) -> Duration {
        instant.saturating_duration_since(self.epoch)
    }
}
