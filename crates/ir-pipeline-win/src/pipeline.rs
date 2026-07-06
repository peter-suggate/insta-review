//! The `CapturePipeline` implementation for Windows: WGC display capture
//! (via the `windows-capture` crate) feeding the GPU converter and the MF
//! hardware encoder. The WGC callback thread does only GPU-side work
//! (BGRA→NV12 blt) and a channel send; encoding runs on its own thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, Sender};
use ir_core::{CaptureClock, CapturePipeline, PacketSink};
use ir_types::{CaptureTarget, PipelineConfig, PipelineError};
use tracing::{info, warn};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::convert::Converter;
use crate::mf_encoder::{EncodeJob, MfEncoder};

/// Messages from the capture thread to the encoder thread.
pub enum EncoderFeed {
    Frame(EncodeJob),
    Stop,
}

/// Current time on the same axis as WGC frame timestamps (QPC, 100 ns).
fn qpc_100ns() -> i64 {
    unsafe {
        let mut freq = 0i64;
        let _ = QueryPerformanceFrequency(&mut freq);
        let mut counter = 0i64;
        let _ = QueryPerformanceCounter(&mut counter);
        if freq <= 0 {
            return 0;
        }
        (counter as i128 * 10_000_000 / freq as i128) as i64
    }
}

/// Everything the WGC-side handler needs, passed through `Settings::flags`.
struct HandlerInit {
    cfg: PipelineConfig,
    clock: CaptureClock,
    sink: PacketSink,
    force_key: Arc<AtomicBool>,
    frames_dropped: Arc<AtomicU64>,
}

pub struct WindowsPipeline {
    control: Option<CaptureControl<Handler, PipelineError>>,
    force_key: Arc<AtomicBool>,
    /// Diagnostic: frames dropped because the encoder queue was full.
    pub frames_dropped: Arc<AtomicU64>,
}

impl WindowsPipeline {
    pub fn new() -> Self {
        Self {
            control: None,
            force_key: Arc::new(AtomicBool::new(true)), // first frame → IDR
            frames_dropped: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Default for WindowsPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl CapturePipeline for WindowsPipeline {
    fn start(
        &mut self,
        cfg: PipelineConfig,
        clock: CaptureClock,
        sink: PacketSink,
    ) -> Result<(), PipelineError> {
        let monitor = match &cfg.target {
            CaptureTarget::PrimaryDisplay => Monitor::primary(),
            CaptureTarget::Display { id } => id
                .parse::<usize>()
                .ok()
                .map_or_else(Monitor::primary, Monitor::from_index),
            // Per the plan we capture the display even for window targets
            // (WGC window capture of fullscreen games black-screens).
            CaptureTarget::Window { .. } => Monitor::primary(),
        }
        .map_err(|e| PipelineError::TargetNotFound(e.to_string()))?;

        let min_interval = if cfg.max_fps > 0 {
            MinimumUpdateIntervalSettings::Custom(Duration::from_secs_f64(
                1.0 / f64::from(cfg.max_fps),
            ))
        } else {
            MinimumUpdateIntervalSettings::Default
        };

        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            min_interval,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            HandlerInit {
                cfg,
                clock,
                sink,
                force_key: self.force_key.clone(),
                frames_dropped: self.frames_dropped.clone(),
            },
        );

        let control = Handler::start_free_threaded(settings)
            .map_err(|e| PipelineError::Capture(format!("start capture: {e}")))?;
        self.control = Some(control);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(control) = self.control.take() {
            if let Err(e) = control.stop() {
                warn!("stopping capture: {e}");
            }
        }
    }

    fn request_keyframe(&mut self) {
        self.force_key.store(true, Ordering::Relaxed);
    }
}

// SAFETY: all COM members live on a multithread-protected D3D11 device and
// are only touched from the capture thread (plus Drop on the control
// thread after the capture thread has exited).
unsafe impl Send for Handler {}

struct Handler {
    init: HandlerInit,
    qpc_anchor_100ns: i64,
    clock_anchor: Duration,
    device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    converter: Option<Converter>,
    feed: Option<Sender<EncoderFeed>>,
    encoder_join: Option<std::thread::JoinHandle<()>>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = HandlerInit;
    type Error = PipelineError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        // Anchor the WGC timestamp axis (QPC) to the engine clock.
        let clock_anchor = ctx.flags.clock.now();
        let qpc_anchor_100ns = qpc_100ns();
        Ok(Self {
            init: ctx.flags,
            qpc_anchor_100ns,
            clock_anchor,
            device: ctx.device,
            context: ctx.device_context,
            converter: None,
            feed: None,
            encoder_join: None,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let (width, height) = (frame.width(), frame.height());

        // Lazy init on the first frame (dimensions come from the frame).
        if self.converter.is_none() {
            let cfg = &self.init.cfg;
            let gop_frames = ((cfg.gop_seconds * cfg.max_fps as f32).round() as u32).max(1);
            info!(
                width,
                height,
                gop_frames,
                crop = cfg.center_crop_px,
                "windows pipeline: first frame"
            );
            let converter = Converter::new(
                &self.device,
                &self.context,
                width,
                height,
                cfg.center_crop_px,
                cfg.max_fps,
            )?;
            let (out_width, out_height) = converter.dimensions();
            self.converter = Some(converter);
            let mut encoder = MfEncoder::new(
                &self.device,
                self.init.sink.clone(),
                out_width,
                out_height,
                cfg.max_fps,
                gop_frames,
                cfg.quality,
            )?;
            let (tx, rx) = bounded::<EncoderFeed>(8);
            self.feed = Some(tx);
            let sink = self.init.sink.clone();
            self.encoder_join = Some(
                std::thread::Builder::new()
                    .name("ir-mf-encoder".into())
                    .spawn(move || {
                        if let Err(e) = encoder.run(&rx) {
                            sink.error(e);
                        }
                    })
                    .map_err(|e| PipelineError::Other(format!("spawn encoder: {e}")))?,
            );
        }

        let converter = self.converter.as_mut().expect("initialized above");
        if converter.input_dimensions() != (width & !1, height & !1) {
            // Resolution changed (mode switch / rotation): restarting the
            // whole pipeline is the engine's job.
            warn!("capture dimensions changed; signaling target lost");
            self.init.sink.target_lost();
            capture_control.stop();
            return Ok(());
        }

        // WGC SystemRelativeTime → engine clock.
        let ts_100ns = frame
            .timestamp()
            .map(|t| t.Duration)
            .unwrap_or_else(|_| qpc_100ns());
        let rel = ts_100ns.saturating_sub(self.qpc_anchor_100ns).max(0) as u64;
        let pts = self.clock_anchor + Duration::from_nanos(rel * 100);

        let nv12 = converter.convert(frame.as_raw_texture())?;
        let job = EncodeJob {
            texture: nv12,
            pts,
            force_keyframe: self.init.force_key.swap(false, Ordering::Relaxed),
        };
        if let Some(feed) = &self.feed {
            // Never block the WGC callback: drop the frame if the encoder
            // is that far behind.
            if feed.try_send(EncoderFeed::Frame(job)).is_err() {
                self.init.frames_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.init.sink.target_lost();
        Ok(())
    }
}

impl Drop for Handler {
    fn drop(&mut self) {
        if let Some(feed) = self.feed.take() {
            let _ = feed.try_send(EncoderFeed::Stop);
        }
        if let Some(join) = self.encoder_join.take() {
            let _ = join.join();
        }
    }
}
