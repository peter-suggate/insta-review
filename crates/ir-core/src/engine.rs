use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Sender};
use ir_types::{Marker, PipelineConfig, PipelineError, PipelineEvent};
use tracing::{info, warn};

use crate::clock::CaptureClock;
use crate::markers::MarkerLog;
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
    Stats {
        reply: Sender<RingStats>,
    },
    Stop,
}

/// Handle for talking to a running engine from other threads (hotkey
/// handler, GSI server, UI).
pub struct EngineHandle {
    cmd_tx: Sender<EngineCommand>,
    clock: CaptureClock,
    join: Option<JoinHandle<()>>,
}

impl EngineHandle {
    pub fn clock(&self) -> CaptureClock {
        self.clock
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
                                    build_clip(snap, markers.range(from, to), trigger_ts)
                                });
                                let _ = reply.send(clip);
                            }
                            Ok(EngineCommand::AddMarker(m)) => markers.push(m),
                            Ok(EngineCommand::Stats { reply }) => {
                                let _ = reply.send(ring.stats());
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
