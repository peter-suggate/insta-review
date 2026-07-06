use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use ir_core::{Clip, Engine, EngineHandle};
use ir_types::{Marker, PipelineConfig};

#[derive(Parser)]
#[command(name = "ir-cli", about = "insta-review headless harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct PipelineArgs {
    /// Pipeline backend: `test` anywhere, `windows` on Windows.
    #[arg(long, default_value = default_pipeline())]
    pipeline: String,
    /// Capture at this frame rate.
    #[arg(long, default_value_t = 60)]
    fps: u32,
    /// GOP length in seconds.
    #[arg(long, default_value_t = 1.0)]
    gop: f32,
    /// Encoder quality (CRF-ish; lower is better).
    #[arg(long, default_value_t = 23)]
    quality: u32,
    /// Test pipeline frame size.
    #[arg(long, default_value_t = 1280)]
    width: u32,
    #[arg(long, default_value_t = 720)]
    height: u32,
    /// Encode only a square of this many pixels centered on the frame
    /// (0 = full frame). Windows pipeline only.
    #[arg(long, default_value_t = 0)]
    crop: u32,
}

const fn default_pipeline() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else {
        "test"
    }
}

#[derive(Subcommand)]
enum Command {
    /// Run a capture pipeline into the ring buffer, then save the last
    /// `--window` seconds as an MP4 (on Ctrl-C, or after `--duration`).
    Record {
        #[command(flatten)]
        pipeline: PipelineArgs,
        /// Seconds of footage to keep and save.
        #[arg(long, default_value_t = 15.0)]
        window: f32,
        /// Stop automatically after this many seconds (otherwise Ctrl-C).
        #[arg(long)]
        duration: Option<f32>,
        #[arg(short, long, default_value = "out.mp4")]
        output: PathBuf,
    },
    /// Capture continuously; every hotkey press saves the last `--window`
    /// seconds as MP4 + a .json sidecar (frame pts table + markers).
    SnapshotOnKey {
        #[command(flatten)]
        pipeline: PipelineArgs,
        #[arg(long, default_value_t = 15.0)]
        window: f32,
        /// Global hotkey (Windows). Elsewhere, press Enter instead.
        #[arg(long, default_value = "ctrl+alt+r")]
        hotkey: String,
        /// Directory for saved clips.
        #[arg(short, long, default_value = "clips")]
        out_dir: PathBuf,
        /// Start a CS2 GSI listener on this port and put kill/death/round
        /// markers on saved clips.
        #[arg(long)]
        gsi_port: Option<u16>,
        /// Require this GSI auth token.
        #[arg(long)]
        gsi_token: Option<String>,
        /// Print the gamestate_integration cfg for --gsi-port and exit.
        #[arg(long)]
        print_gsi_cfg: bool,
    },
    /// Enrich a saved clip with tick-exact truth from a CS2 demo file:
    /// aligns the demo onto the clip via its kill/death markers (which
    /// also identifies the local player) and writes `<clip>.demo.json`
    /// with the local player's weapon_fire times on the clip clock.
    /// Analysis picks the file up automatically.
    DemoEnrich {
        /// The saved clip's mp4 (its .json sidecar must sit next to it).
        #[arg(long)]
        clip: PathBuf,
        /// The match's .dem file (downloaded from CS2 match history).
        #[arg(long)]
        demo: PathBuf,
        /// Marker-to-demo-event match tolerance, seconds (GSI latency).
        #[arg(long, default_value_t = 0.7)]
        tolerance: f64,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Record {
            pipeline,
            window,
            duration,
            output,
        } => record(&pipeline, window, duration, &output),
        Command::SnapshotOnKey {
            pipeline,
            window,
            hotkey,
            out_dir,
            gsi_port,
            gsi_token,
            print_gsi_cfg,
        } => snapshot_on_key(
            &pipeline,
            window,
            &hotkey,
            &out_dir,
            gsi_port,
            gsi_token,
            print_gsi_cfg,
        ),
        Command::DemoEnrich {
            clip,
            demo,
            tolerance,
        } => demo_enrich(&clip, &demo, tolerance),
    }
}

fn build_pipeline(
    args: &PipelineArgs,
) -> Result<Box<dyn ir_core::CapturePipeline>, Box<dyn std::error::Error>> {
    match args.pipeline.as_str() {
        #[cfg(feature = "test-pipeline")]
        "test" => Ok(Box::new(ir_pipeline_test::TestPatternPipeline::new(
            args.width,
            args.height,
            true,
        ))),
        #[cfg(windows)]
        "windows" => Ok(Box::new(ir_pipeline_win::WindowsPipeline::new())),
        other => Err(format!("unknown pipeline: {other}").into()),
    }
}

fn start_engine(
    args: &PipelineArgs,
    window: f32,
) -> Result<EngineHandle, Box<dyn std::error::Error>> {
    let pipeline = build_pipeline(args)?;
    let cfg = PipelineConfig {
        max_fps: args.fps,
        gop_seconds: args.gop,
        quality: args.quality,
        center_crop_px: args.crop,
        ..PipelineConfig::default()
    };
    let retain = Duration::from_secs_f32(window + args.gop.max(1.0));
    Ok(Engine::start(pipeline, cfg, retain, 512 * 1024 * 1024)?)
}

fn save_clip(clip: &Clip, mp4_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mp4 = ir_mux::mux_h264(&clip.codec, &clip.samples)?;
    std::fs::write(mp4_path, &mp4)?;
    let sidecar = mp4_path.with_extension("json");
    std::fs::write(&sidecar, serde_json::to_string_pretty(&clip.meta)?)?;
    let span = clip.meta.frame_pts.last().copied().unwrap_or(0.0);
    println!(
        "wrote {} ({} frames, {:.2} s, {} keyframes, {} markers, {:.1} MiB)",
        mp4_path.display(),
        clip.meta.frame_pts.len(),
        span,
        clip.meta.keyframe_indices.len(),
        clip.meta.markers.len(),
        mp4.len() as f64 / (1024.0 * 1024.0),
    );
    Ok(())
}

fn print_stats(handle: &EngineHandle) {
    if let Some(stats) = handle.stats() {
        println!(
            "ring: {} frames pushed, {} GOPs evicted, {} pre-IDR dropped, {} non-monotonic",
            stats.frames_pushed,
            stats.gops_evicted,
            stats.dropped_pre_idr,
            stats.dropped_non_monotonic
        );
    }
}

fn record(
    args: &PipelineArgs,
    window: f32,
    duration: Option<f32>,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let handle = start_engine(args, window)?;

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
    save_clip(&clip, output)?;
    print_stats(&handle);
    handle.stop();
    Ok(())
}

fn snapshot_on_key(
    args: &PipelineArgs,
    window: f32,
    hotkey: &str,
    out_dir: &Path,
    gsi_port: Option<u16>,
    gsi_token: Option<String>,
    print_gsi_cfg: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if print_gsi_cfg {
        let port = gsi_port.unwrap_or(3585);
        let token = gsi_token.as_deref().unwrap_or("dev");
        print!("{}", ir_gsi::config_file_contents(port, token));
        println!();
        println!("# Save as gamestate_integration_instareview.cfg (no BOM!) in");
        println!(
            "# …\\Steam\\steamapps\\common\\Counter-Strike Global Offensive\\game\\csgo\\cfg\\"
        );
        return Ok(());
    }

    std::fs::create_dir_all(out_dir)?;
    let handle = start_engine(args, window)?;

    // GSI listener → markers + state samples on the engine's clock at
    // receipt time.
    let _gsi = match gsi_port {
        Some(port) => {
            let clock = handle.clock();
            let marker_tx = handle.marker_sender();
            Some(ir_gsi::GsiServer::start(port, gsi_token, move |update| {
                let ts = clock.now();
                for kind in update.markers {
                    marker_tx.send(Marker { ts, kind });
                }
                if let Some(state) = update.sample {
                    marker_tx.send_sample(ir_types::GsiSample { ts, state });
                }
            })?)
        }
        None => None,
    };

    // Trigger source: global hotkey on Windows, Enter elsewhere.
    let (trigger_tx, trigger_rx) = std::sync::mpsc::channel::<()>();
    #[cfg(windows)]
    let _hotkey = {
        let hk = ir_winutil::Hotkey::parse(hotkey).map_err(|e| e.to_string())?;
        println!("capturing — press {hotkey} to save the last {window} s (Ctrl-C quits)");
        ir_winutil::HotkeyListener::start(hk, trigger_tx.clone()).map_err(|e| e.to_string())?
    };
    #[cfg(not(windows))]
    {
        let _ = hotkey;
        println!("capturing — press Enter to save the last {window} s (Ctrl-C quits)");
        let tx = trigger_tx.clone();
        std::thread::spawn(move || {
            let mut line = String::new();
            while std::io::stdin().read_line(&mut line).is_ok() {
                if tx.send(()).is_err() {
                    break;
                }
                line.clear();
            }
        });
    }

    // Ctrl-C ends the session.
    let (quit_tx, quit_rx) = std::sync::mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        let _ = quit_tx.send(());
    })?;

    let mut clip_index = 0u32;
    loop {
        // Wake on either a trigger or quit.
        if quit_rx.try_recv().is_ok() {
            break;
        }
        match trigger_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => match handle.snapshot(Duration::from_secs_f32(window)) {
                Some(clip) => {
                    clip_index += 1;
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_secs();
                    let path = out_dir.join(format!("clip_{stamp}_{clip_index:03}.mp4"));
                    if let Err(e) = save_clip(&clip, &path) {
                        eprintln!("save failed: {e}");
                    }
                }
                None => eprintln!("buffer not ready yet (no keyframe captured)"),
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    print_stats(&handle);
    handle.stop();
    Ok(())
}

use tracing::info;

/// `demo-enrich`: align a CS2 demo onto a saved clip and write the
/// tick-exact shot enrichment sidecar.
fn demo_enrich(
    clip: &std::path::Path,
    demo: &std::path::Path,
    tolerance: f64,
) -> Result<(), Box<dyn std::error::Error>> {
    let sidecar = clip.with_extension("json");
    let meta: ir_types::ClipMeta = serde_json::from_str(&std::fs::read_to_string(&sidecar)?)?;
    let clip_kills: Vec<f64> = meta
        .markers
        .iter()
        .filter(|m| matches!(m.kind, ir_types::MarkerKind::Kill { .. }))
        .map(|m| m.at)
        .collect();
    let clip_deaths: Vec<f64> = meta
        .markers
        .iter()
        .filter(|m| matches!(m.kind, ir_types::MarkerKind::Death))
        .map(|m| m.at)
        .collect();
    if clip_kills.is_empty() && clip_deaths.is_empty() {
        return Err("clip has no kill/death markers to align against".into());
    }

    info!(demo = %demo.display(), "parsing demo (this reads the whole file)…");
    let events = ir_demo::extract_events(demo, &["player_death", "weapon_fire"])?;
    let deaths: Vec<ir_demo::RawEvent> = events
        .iter()
        .filter(|e| e.name == "player_death")
        .cloned()
        .collect();
    let fires: Vec<ir_demo::RawEvent> = events
        .into_iter()
        .filter(|e| e.name == "weapon_fire")
        .collect();
    info!(deaths = deaths.len(), fires = fires.len(), "demo events extracted");

    let alignment = ir_demo::infer_alignment(&deaths, &clip_kills, &clip_deaths, tolerance)
        .ok_or("could not align demo to clip (wrong demo, or too few matching events)")?;
    info!(
        slot = alignment.slot,
        offset_s = alignment.offset_s,
        matched = format!("{}/{}", alignment.matched, alignment.total),
        "aligned"
    );

    let duration = meta.frame_pts.last().copied().unwrap_or(0.0);
    let shots = ir_demo::shots_on_clip_clock(&fires, &alignment, duration);
    let enrichment = ir_demo::DemoEnrichment {
        schema_version: ir_demo::ENRICHMENT_VERSION,
        demo_file: demo.display().to_string(),
        slot: alignment.slot,
        offset_s: alignment.offset_s,
        matched_events: alignment.matched,
        total_events: alignment.total,
        shots,
    };
    let out = clip.with_extension("demo.json");
    std::fs::write(&out, serde_json::to_string_pretty(&enrichment)?)?;
    info!(
        shots = enrichment.shots.len(),
        out = %out.display(),
        "enrichment written — analysis will use tick-exact shot times"
    );
    Ok(())
}
