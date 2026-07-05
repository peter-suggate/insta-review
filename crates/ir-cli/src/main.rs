use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use ir_core::Engine;
use ir_types::PipelineConfig;

#[derive(Parser)]
#[command(name = "ir-cli", about = "insta-review headless harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a capture pipeline into the ring buffer, then save the last
    /// `--window` seconds as an MP4 (on Ctrl-C, or after `--duration`).
    Record {
        /// Pipeline backend. Only `test` exists so far.
        #[arg(long, default_value = "test")]
        pipeline: String,
        /// Seconds of footage to keep and save.
        #[arg(long, default_value_t = 15.0)]
        window: f32,
        /// Stop automatically after this many seconds (otherwise Ctrl-C).
        #[arg(long)]
        duration: Option<f32>,
        /// Capture at this frame rate.
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// GOP length in seconds.
        #[arg(long, default_value_t = 1.0)]
        gop: f32,
        #[arg(long, default_value_t = 1280)]
        width: u32,
        #[arg(long, default_value_t = 720)]
        height: u32,
        #[arg(short, long, default_value = "out.mp4")]
        output: PathBuf,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Record {
            pipeline,
            window,
            duration,
            fps,
            gop,
            width,
            height,
            output,
        } => record(
            &pipeline, window, duration, fps, gop, width, height, &output,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn record(
    pipeline: &str,
    window: f32,
    duration: Option<f32>,
    fps: u32,
    gop: f32,
    width: u32,
    height: u32,
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let pipeline: Box<dyn ir_core::CapturePipeline> = match pipeline {
        "test" => Box::new(ir_pipeline_test::TestPatternPipeline::new(
            width, height, true,
        )),
        other => return Err(format!("unknown pipeline: {other}").into()),
    };

    let cfg = PipelineConfig {
        max_fps: fps,
        gop_seconds: gop,
        ..PipelineConfig::default()
    };
    let retain = Duration::from_secs_f32(window + gop.max(1.0));
    let handle = Engine::start(pipeline, cfg, retain, 512 * 1024 * 1024)?;

    // Wait for the duration, or for Ctrl-C.
    match duration {
        Some(secs) => {
            println!("recording for {secs} s…");
            std::thread::sleep(Duration::from_secs_f32(secs));
        }
        None => {
            println!("recording — Ctrl-C to save the last {window} s…");
            let (tx, rx) = std::sync::mpsc::channel();
            ctrlc::set_handler(move || {
                let _ = tx.send(());
            })?;
            rx.recv()?;
        }
    }

    let clip = handle
        .snapshot(Duration::from_secs_f32(window))
        .ok_or("nothing captured yet (no keyframe in buffer)")?;
    let stats = handle.stats();

    let mp4 = ir_mux::mux_h264(&clip.codec, &clip.samples)?;
    std::fs::write(output, &mp4)?;

    let span = clip.meta.frame_pts.last().copied().unwrap_or(0.0);
    println!(
        "wrote {} ({} frames, {:.2} s, {} keyframes, {:.1} MiB)",
        output.display(),
        clip.meta.frame_pts.len(),
        span,
        clip.meta.keyframe_indices.len(),
        mp4.len() as f64 / (1024.0 * 1024.0),
    );
    if let Some(stats) = stats {
        println!(
            "ring: {} frames pushed, {} GOPs evicted, {} pre-IDR dropped, {} non-monotonic",
            stats.frames_pushed,
            stats.gops_evicted,
            stats.dropped_pre_idr,
            stats.dropped_non_monotonic
        );
    }
    handle.stop();
    Ok(())
}
