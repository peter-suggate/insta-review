use crossbeam_channel::Sender;
use ir_types::{CodecConfig, EncodedPacket, PipelineConfig, PipelineError, PipelineEvent};

use crate::clock::CaptureClock;

/// Where a pipeline delivers its output. Wraps an unbounded channel —
/// encoded packets are small, and the engine loop drains promptly.
#[derive(Clone)]
pub struct PacketSink {
    tx: Sender<PipelineEvent>,
}

impl PacketSink {
    pub fn new(tx: Sender<PipelineEvent>) -> Self {
        Self { tx }
    }

    pub fn configured(&self, codec: CodecConfig) {
        let _ = self.tx.send(PipelineEvent::Configured(codec));
    }

    pub fn packet(&self, pkt: EncodedPacket) {
        let _ = self.tx.send(PipelineEvent::Packet(pkt));
    }

    pub fn target_lost(&self) {
        let _ = self.tx.send(PipelineEvent::TargetLost);
    }

    pub fn error(&self, err: PipelineError) {
        let _ = self.tx.send(PipelineEvent::Error(err));
    }
}

/// The platform boundary. Implementations couple capture + hardware encode
/// internally (zero-copy on the GPU) and emit cheap encoded packets; raw
/// frames never cross this trait.
///
/// Contract:
/// - `start` spawns the pipeline's own threads and returns quickly.
/// - The sink receives exactly one `Configured` before the first `Packet`.
/// - Packets have strictly increasing pts stamped against `clock`.
/// - No B-frames: every packet's dts equals its pts.
pub trait CapturePipeline: Send {
    fn start(
        &mut self,
        cfg: PipelineConfig,
        clock: CaptureClock,
        sink: PacketSink,
    ) -> Result<(), PipelineError>;

    fn stop(&mut self);

    /// Ask the encoder to emit an IDR at the next opportunity (used after
    /// target changes; harmless if unsupported).
    fn request_keyframe(&mut self);
}
