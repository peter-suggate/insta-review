use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Sender};
use ir_types::{
    CodecConfig, EncodedPacket, GsiSample, Marker, PipelineConfig, PipelineError, PipelineEvent,
};
use tracing::{info, warn};

use crate::clock::CaptureClock;
use crate::markers::{GsiTraceLog, MarkerLog};
use crate::pipeline::{CapturePipeline, PacketSink};
use crate::ring::{ReplayRing, RingStats};
use crate::snapshot::{build_clip, Clip};

pub enum EngineCommand {
    /// Freeze the last `window` of footage + markers into a clip.
    /// `trigger_ts` is when the hotkey fired (stamped by the caller so
    /// queueing delay doesn't shift the mark).
    Snapshot {
        window: Duration,
        trigger_ts: Duration,
        reply: Sender<Option<Clip>>,
    },
    AddMarker(Marker),
    AddGsiSample(GsiSample),
    Stats {
        reply: Sender<RingStats>,
    },
    /// Latest keyframe + codec for the live preview thumbnail.
    Preview {
        reply: Sender<Option<PreviewFrame>>,
    },
    Stop,
}

/// A single decodable frame from the live ring, for UI preview.
#[derive(Debug, Clone)]
pub struct PreviewFrame {
    pub codec: CodecConfig,
    pub keyframe: EncodedPacket,
    /// Footage currently covered by the ring.
    pub span: Duration,
}

/// Handle for talking to a running engine from other threads (hotkey
/// handler, GSI server, UI).
pub struct EngineHandle {
    cmd_tx: Sender<EngineCommand>,
    clock: CaptureClock,
    join: Option<JoinHandle<()>>,
}

/// Cheap clonable handle for feeding markers and GSI state samples from
/// other threads (GSI listener).
#[derive(Clone)]
pub struct MarkerSender {
    cmd_tx: Sender<EngineCommand>,
}

impl MarkerSender {
    pub fn send(&self, marker: Marker) {
        let _ = self.cmd_tx.send(EngineCommand::AddMarker(marker));
    }

    pub fn send_sample(&self, sample: GsiSample) {
        let _ = self.cmd_tx.send(EngineCommand::AddGsiSample(sample));
    }
}

impl EngineHandle {
    pub fn clock(&self) -> CaptureClock {
        self.clock
    }

    pub fn marker_sender(&self) -> MarkerSender {
        MarkerSender {
            cmd_tx: self.cmd_tx.clone(),
        }
    }

    /// Snapshot the last `window`. Returns `None` until the pipeline has
    /// produced at least one full keyframe.
    pub fn snapshot(&self, window: Duration) -> Option<Clip> {
        let trigger_ts = self.clock.now();
        let (reply, rx) = bounded(1);
        self.cmd_tx
            .send(EngineCommand::Snapshot {
                window,
                trigger_ts,
                reply,
            })
            .ok()?;
        rx.recv().ok().flatten()
    }

    pub fn add_marker(&self, marker: Marker) {
        let _ = self.cmd_tx.send(EngineCommand::AddMarker(marker));
    }

    pub fn stats(&self) -> Option<RingStats> {
        let (reply, rx) = bounded(1);
        self.cmd_tx.send(EngineCommand::Stats { reply }).ok()?;
        rx.recv().ok()
    }

    /// Latest keyframe from the live ring. `None` until the pipeline has
    /// configured and produced a keyframe.
    pub fn preview(&self) -> Option<PreviewFrame> {
        let (reply, rx) = bounded(1);
        self.cmd_tx.send(EngineCommand::Preview { reply }).ok()?;
        rx.recv().ok().flatten()
    }

    pub fn stop(mut self) {
        let _ = self.cmd_tx.send(EngineCommand::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub struct Engine;

impl Engine {
    /// Start the pipeline and the engine loop on a dedicated thread.
    pub fn start(
        mut pipeline: Box<dyn CapturePipeline>,
        cfg: PipelineConfig,
        retain: Duration,
        max_bytes: usize,
    ) -> Result<EngineHandle, PipelineError> {
        let clock = CaptureClock::start();
        let (event_tx, event_rx) = unbounded::<PipelineEvent>();
        let (cmd_tx, cmd_rx) = unbounded::<EngineCommand>();

        pipeline.start(cfg, clock, PacketSink::new(event_tx))?;

        let join = std::thread::Builder::new()
            .name("ir-engine".into())
            .spawn(move || {
                let mut ring = ReplayRing::new(retain, max_bytes);
                let mut markers = MarkerLog::new(retain);
                let mut gsi_trace = GsiTraceLog::new(retain);
                loop {
                    crossbeam_channel::select! {
                        recv(event_rx) -> ev => match ev {
                            Ok(PipelineEvent::Configured(codec)) => {
                                info!(?codec.width, ?codec.height, "pipeline configured");
                                ring.set_codec(codec);
                            }
                            Ok(PipelineEvent::Packet(pkt)) => ring.push(pkt),
                            Ok(PipelineEvent::TargetLost) => {
                                // Restart policy lives with the app (M5);
                                // for now surface it and keep draining.
                                warn!("capture target lost");
                            }
                            Ok(PipelineEvent::Error(err)) => warn!(%err, "pipeline error"),
                            Err(_) => break, // pipeline gone
                        },
                        recv(cmd_rx) -> cmd => match cmd {
                            Ok(EngineCommand::Snapshot { window, trigger_ts, reply }) => {
                                let clip = ring.snapshot(window).map(|snap| {
                                    let from = snap.packets[0].pts;
                                    let to = snap.packets.last().unwrap().end_pts();
                                    build_clip(
                                        snap,
                                        markers.range(from, to),
                                        gsi_trace.range(from, to),
                                        trigger_ts,
                                    )
                                });
                                let _ = reply.send(clip);
                            }
                            Ok(EngineCommand::AddMarker(m)) => markers.push(m),
                            Ok(EngineCommand::AddGsiSample(s)) => gsi_trace.push(s),
                            Ok(EngineCommand::Stats { reply }) => {
                                let _ = reply.send(ring.stats());
                            }
                            Ok(EngineCommand::Preview { reply }) => {
                                let frame = ring.codec().cloned().zip(ring.latest_keyframe().cloned()).map(
                                    |(codec, keyframe)| PreviewFrame {
                                        codec,
                                        keyframe,
                                        span: ring.span(),
                                    },
                                );
                                let _ = reply.send(frame);
                            }
                            Ok(EngineCommand::Stop) | Err(_) => break,
                        },
                    }
                }
                pipeline.stop();
            })
            .map_err(|e| PipelineError::Other(format!("spawn engine thread: {e}")))?;

        Ok(EngineHandle {
            cmd_tx,
            clock,
            join: Some(join),
        })
    }
}
