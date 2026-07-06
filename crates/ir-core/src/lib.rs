//! Platform-independent capture engine: clock, encoded ring buffer,
//! pipeline trait, marker log, snapshotting, and the engine loop.

pub mod clock;
pub mod config;
pub mod engine;
pub mod markers;
pub mod pipeline;
pub mod ring;
pub mod snapshot;

pub use clock::CaptureClock;
pub use engine::{Engine, EngineCommand, EngineHandle, MarkerSender, PreviewFrame};
pub use markers::MarkerLog;
pub use pipeline::{CapturePipeline, PacketSink};
pub use ring::{ReplayRing, RingSnapshot, RingStats};
pub use snapshot::{build_clip, Clip};
