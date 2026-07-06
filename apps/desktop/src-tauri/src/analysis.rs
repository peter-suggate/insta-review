//! Tauri glue for AI coaching analysis: the begin/frame/run/cancel commands
//! and the async pipeline task. All real work (planning, prompts, LLM CLI,
//! persistence) lives in `ir-analysis`; this module owns app state, IPC, and
//! progress events.
//!
//! Flow (driven by coach.js): `analysis_begin` (auto-save + cache check +
//! plan) -> one raw-payload `analysis_frame` per extracted frame ->
//! `analysis_run` (compose -> invoke CLI -> parse -> persist), emitting
//! `analysis-progress` / `analysis-complete` / `analysis-error`.

use std::path::PathBuf;
use std::sync::Arc;

use ir_analysis::cv::{self, FrameStore, LumaFrame};
use ir_analysis::llm::{self, LlmConfig, LlmError, LlmRequest};
use ir_analysis::store::{self, RunDirs};
use ir_analysis::types::{
    AnalysisReport, EventRef, ExtractionPlan, ProviderInfo, SCHEMA_VERSION,
};
use ir_analysis::{prompt, CancelSignal};
use ir_types::{ClipMeta, MarkerKind};
use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State};
use tracing::{info, warn};

use crate::state::{AppSettings, AppState};

/// One in-flight analysis (single-flight; owned by `AppState.analysis`).
pub struct ActiveAnalysis {
    pub event: EventRef,
    pub clip_mp4: PathBuf,
    pub dirs: RunDirs,
    pub plan: ExtractionPlan,
    pub meta: ClipMeta,
    /// t_us of JPEG evidence frames the webview has delivered so far.
    pub received: Vec<u64>,
    /// Downscaled luma frames for the CV pass.
    pub frame_store: FrameStore,
    pub cancel: Arc<CancelSignal>,
    /// True once `analysis_run` has spawned the pipeline task.
    pub running: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BeginResponse {
    /// Present on a cache hit — the analysis is already done.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<AnalysisReport>,
    /// Present on a cache miss — frames the webview must extract.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<ExtractionPlan>,
}

fn emit_progress(app: &AppHandle, stage: &str, detail: String, current: u64, total: u64) {
    let _ = app.emit(
        "analysis-progress",
        json!({ "stage": stage, "detail": detail, "current": current, "total": total }),
    );
}

fn emit_error(app: &AppHandle, stage: &str, err: &LlmError) {
    let kind = match err {
        LlmError::Quota(_) => "quota",
        LlmError::Auth(_) => "auth",
        LlmError::Timeout(_) => "timeout",
        LlmError::Cancelled => "cancelled",
        LlmError::BinaryNotFound { .. } => "binary",
        LlmError::Parse(_) => "parse",
        LlmError::Other(_) => "other",
    };
    let _ = app.emit(
        "analysis-error",
        json!({ "stage": stage, "kind": kind, "message": err.to_string() }),
    );
}

/// Gate, auto-save, cache-check, plan. Returns either the cached report or
/// the extraction plan the webview must fulfil before calling
/// `analysis_run`.
#[tauri::command]
pub fn analysis_begin(
    app: AppHandle,
    state: State<AppState>,
    event: EventRef,
    force: Option<bool>,
) -> Result<BeginResponse, String> {
    {
        let active = state.analysis.lock().unwrap();
        if active.as_ref().is_some_and(|a| a.running) {
            return Err("an analysis is already running".into());
        }
    }

    // The clip must exist on disk: artifacts live next to it and the cache
    // key is its path.
    let clip_mp4 = crate::commands::save_current_clip(&app, &state)?;
    let meta = {
        let clip = state.clip.lock().unwrap();
        clip.as_ref().ok_or("no clip staged")?.clip.meta.clone()
    };

    // `force` skips the cache — how a quick (local-cv) result is upgraded
    // to a full LLM analysis of the same event.
    if !force.unwrap_or(false) {
        if let Some(report) = store::cached_report(&clip_mp4, &event.id) {
            info!(event = %event.id, "analysis cache hit");
            return Ok(BeginResponse {
                cached: Some(report),
                plan: None,
            });
        }
    }

    let plan = ir_analysis::plan_extraction(&event, &meta.frame_pts);
    if plan.frames.is_empty() {
        return Err("no frames in range for this event".into());
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let dirs = store::prepare_run(&clip_mp4, &event.id, stamp).map_err(|e| e.to_string())?;

    let settings = state.settings.lock().unwrap().clone();
    store::write_json(
        &dirs.run_dir,
        "request.json",
        &json!({
            "event": event,
            "plan": plan,
            "clip": clip_mp4.display().to_string(),
            "settings": {
                "llmProvider": settings.llm_provider,
                "llmModel": settings.llm_model,
                "llmTimeoutSeconds": settings.llm_timeout_seconds,
            },
        }),
    )
    .map_err(|e| e.to_string())?;

    let response_plan = plan.clone();
    *state.analysis.lock().unwrap() = Some(ActiveAnalysis {
        event,
        clip_mp4,
        dirs,
        plan,
        meta,
        received: Vec::new(),
        frame_store: FrameStore::default(),
        cancel: Arc::new(CancelSignal::new()),
        running: false,
    });

    Ok(BeginResponse {
        cached: None,
        plan: Some(response_plan),
    })
}

/// One extracted frame from the webview. Raw-payload invoke: bytes as the
/// body, metadata in headers (the only side channel Tauri gives a raw
/// body). `kind: jpeg` = full-res evidence file; `kind: luma` = downscaled
/// grayscale for CV (`w`/`h` headers required).
#[tauri::command]
pub fn analysis_frame(
    app: AppHandle,
    state: State<AppState>,
    request: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected a raw body".into());
    };
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let t_us: u64 = header("t-us")
        .and_then(|s| s.parse().ok())
        .ok_or("missing/invalid t-us header")?;
    let kind = header("kind").unwrap_or_else(|| "jpeg".into());

    let mut active = state.analysis.lock().unwrap();
    let active = active.as_mut().ok_or("no analysis in progress")?;
    if !active.plan.frames.iter().any(|f| f.t_us == t_us) {
        return Err(format!("frame {t_us} is not in the extraction plan"));
    }

    let (delivered, total) = match kind.as_str() {
        "luma" => {
            let w: u32 = header("w").and_then(|s| s.parse().ok()).ok_or("missing w")?;
            let h: u32 = header("h").and_then(|s| s.parse().ok()).ok_or("missing h")?;
            if (w * h) as usize != bytes.len() {
                return Err(format!("luma size mismatch: {w}x{h} vs {}", bytes.len()));
            }
            active.frame_store.push(LumaFrame {
                t_us,
                w,
                h,
                data: bytes.clone(),
            });
            (
                active.frame_store.len(),
                active.plan.frames.iter().filter(|f| f.want_raw).count(),
            )
        }
        _ => {
            let path = active.dirs.frames_dir.join(store::frame_file_name(t_us));
            std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
            if !active.received.contains(&t_us) {
                active.received.push(t_us);
            }
            (
                active.received.len(),
                active.plan.frames.iter().filter(|f| f.want_jpeg).count(),
            )
        }
    };
    // Dense luma runs arrive at hundreds/second — don't flood the UI.
    if delivered % 24 == 0 || delivered == total {
        emit_progress(
            &app,
            "extracting",
            format!("frame {delivered}/{total}"),
            delivered as u64,
            total as u64,
        );
    }
    Ok(())
}

/// Kick off the pipeline task: compose prompts, invoke the LLM CLI, parse,
/// persist, emit. Returns immediately; results arrive as events.
#[tauri::command]
pub fn analysis_run(
    app: AppHandle,
    state: State<AppState>,
    llm: Option<bool>,
) -> Result<(), String> {
    let llm = llm.unwrap_or(true);
    let (event, clip_mp4, dirs, plan, meta, received, frames, cancel) = {
        let mut active = state.analysis.lock().unwrap();
        let active = active.as_mut().ok_or("no analysis in progress")?;
        if active.running {
            return Err("analysis already running".into());
        }
        if active.received.is_empty() {
            return Err("no frames were delivered".into());
        }
        active.running = true;
        (
            active.event.clone(),
            active.clip_mp4.clone(),
            active.dirs.clone(),
            active.plan.clone(),
            active.meta.clone(),
            active.received.clone(),
            std::mem::take(&mut active.frame_store),
            active.cancel.clone(),
        )
    };
    let settings = state.settings.lock().unwrap().clone();
    let prompts_dir = prompts_dir(&app);
    let analysis_cfg = ir_analysis::config::load_or_init(
        &app.path()
            .app_config_dir()
            .unwrap_or_else(|_| PathBuf::from(".")),
    );

    tauri::async_runtime::spawn(async move {
        let result = run_pipeline(
            &app, &event, &clip_mp4, &dirs, &plan, &meta, &received, frames, &cancel,
            &settings, &analysis_cfg, &prompts_dir, llm,
        )
        .await;
        match result {
            Ok((report, trace)) => {
                let _ = app.emit("analysis-complete", json!({ "report": report, "trace": trace }));
            }
            Err((stage, err)) => {
                warn!(stage, "analysis failed: {err}");
                let _ = store::write_text(
                    &dirs.run_dir,
                    "error.txt",
                    &format!("stage: {stage}\n{err}"),
                );
                emit_error(&app, stage, &err);
            }
        }
        // Single-flight slot free again.
        *app.state::<AppState>().analysis.lock().unwrap() = None;
    });
    Ok(())
}

/// Cancel the in-flight analysis (kills the CLI child if it's running).
#[tauri::command]
pub fn analysis_cancel(state: State<AppState>) {
    if let Some(active) = state.analysis.lock().unwrap().as_ref() {
        active.cancel.notify_one();
    }
}

/// Cached report for an event of the staged clip, if it was saved and
/// analyzed before. Read-only: no auto-save, nothing spawned.
#[tauri::command]
pub fn get_analysis(state: State<AppState>, event_id: String) -> Option<AnalysisReport> {
    let clip = state.clip.lock().unwrap();
    let saved = clip.as_ref()?.saved_path.as_ref()?.clone();
    store::cached_report(&saved, &event_id)
}

/// Thumbs up/down on one finding — persisted next to the report, feeding
/// the future eval harness.
#[tauri::command]
pub fn analysis_feedback(
    state: State<AppState>,
    event_id: String,
    finding_index: usize,
    up: bool,
) -> Result<(), String> {
    let saved = {
        let clip = state.clip.lock().unwrap();
        clip.as_ref()
            .and_then(|c| c.saved_path.clone())
            .ok_or("clip not saved")?
    };
    store::record_feedback(&saved, &event_id, finding_index, up).map_err(|e| e.to_string())
}

/// Serve an analysis frame JPEG for the replay:// route. Path safety lives
/// in `store::find_analysis_frame` (frame-file-name allowlist).
pub fn serve_analysis_frame(app: &AppHandle, event_id: &str, file: &str) -> Option<Vec<u8>> {
    let state = app.state::<AppState>();
    let saved = {
        let clip = state.clip.lock().unwrap();
        clip.as_ref()?.saved_path.clone()?
    };
    let path = store::find_analysis_frame(&saved, event_id, file)?;
    std::fs::read(path).ok()
}

fn prompts_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("prompts")
}

/// Copy missing default prompt files on startup so users can edit them.
pub fn ensure_prompt_defaults(app: &AppHandle) {
    let dir = prompts_dir(app);
    if let Err(e) = prompt::ensure_defaults(&dir) {
        warn!("could not write default prompts to {}: {e}", dir.display());
    } else {
        info!(dir = %dir.display(), "coaching prompts ready (editable)");
    }
}

fn event_kind_label(kind: &MarkerKind) -> String {
    match kind {
        MarkerKind::Kill { headshot: true, .. } => "kill (headshot)".into(),
        MarkerKind::Kill { .. } => "kill".into(),
        MarkerKind::Death => "death".into(),
        MarkerKind::DamageTaken { amount } => format!("damage taken ({amount})"),
        MarkerKind::RoundPhase { phase } => format!("round phase: {phase}"),
        MarkerKind::Bomb { event } => format!("bomb: {event}"),
        MarkerKind::ShotFired => "shot fired".into(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    app: &AppHandle,
    event: &EventRef,
    clip_mp4: &std::path::Path,
    dirs: &RunDirs,
    plan: &ExtractionPlan,
    meta: &ClipMeta,
    received: &[u64],
    frames: FrameStore,
    cancel: &CancelSignal,
    settings: &AppSettings,
    analysis_cfg: &ir_analysis::config::AnalysisConfig,
    prompts_dir: &std::path::Path,
    llm: bool,
) -> Result<(AnalysisReport, serde_json::Value), (&'static str, LlmError)> {
    let fail = |stage: &'static str| move |e: LlmError| (stage, e);
    let started = std::time::Instant::now();

    // ---- measure (local CV) ----------------------------------------------
    emit_progress(
        app,
        "measuring",
        format!("analyzing {} frames", frames.len()),
        0,
        0,
    );
    let cv_report = cv::analyze(
        event,
        &frames,
        &meta.gsi_trace,
        (meta.width, meta.height),
        settings.stretch_43,
        settings.gsi_offset_seconds as f64,
        analysis_cfg,
    );
    let _ = store::write_json(&dirs.run_dir, "traces.json", &cv_report);
    info!(
        flow_samples = cv_report.flow.len(),
        candidates = cv_report.candidates.len(),
        shots = cv_report.shots.len(),
        flicks = cv_report.flicks.len(),
        "CV pass done"
    );

    // Compact per-sample trace for the timeline overlay and drawer chart.
    let overlay = json!({ "flow": cv::flow_trace_json(&cv_report) });

    // ---- quick mode: deterministic CV report, no LLM ----------------------
    if !llm {
        let mut degradations = cv::cv_degradations(&cv_report);
        if !frames.is_empty() && cv_report.flow.is_empty() {
            degradations.push("CV produced no flow trace (frames too sparse?)".into());
        }
        let report = cv::local_report(
            event,
            &cv_report,
            started.elapsed().as_millis() as u64,
            received.iter().map(|&t| t as f64 / 1e6).collect(),
            degradations,
        );
        store::persist_report(dirs, &report)
            .map_err(|e| ("persisting", LlmError::Other(e.to_string())))?;
        if let Err(e) = store::promote_frames(dirs) {
            warn!("could not promote frames for export: {e}");
        }
        if let Err(e) = store::write_export_manifest(clip_mp4) {
            warn!("could not write export manifest: {e}");
        }
        info!(
            event = %event.id,
            findings = report.findings.len(),
            "quick (local-cv) analysis complete"
        );
        return Ok((report, overlay));
    }

    // ---- compose ---------------------------------------------------------
    emit_progress(app, "composing", "rendering prompts".into(), 0, 0);

    let is_43 = (meta.width as f64 / meta.height as f64 - 4.0 / 3.0).abs() < 0.05;
    let markers: Vec<_> = meta
        .markers
        .iter()
        .map(|m| {
            json!({
                "atS": (m.at as f32 + settings.gsi_offset_seconds).max(0.0),
                "kind": event_kind_label(&m.kind),
            })
        })
        .collect();
    let context = json!({
        "clip": {
            "widthPx": meta.width,
            "heightPx": meta.height,
            "nominalFps": meta.nominal_fps,
            "durationS": meta.frame_pts.last().copied().unwrap_or(0.0),
            "stretched43": is_43,
            "note": if is_43 {
                "frames are 4:3 rendered stretched; the crosshair is at the exact frame center"
            } else {
                "the crosshair is at the exact frame center"
            },
        },
        "event": { "kind": event_kind_label(&event.kind), "atS": event.at_s },
        "timelineMarkers": markers,
        "markerAccuracy": "marker times come from CS2 game-state integration and are approximate (within ~0.3s)",
        "hotkeyPressedAtS": meta.trigger_at,
        "cv": {
            "note": if cv_report.speed.is_empty() {
                "machine measurements from optical flow on the clip and the GSI ammo/state trace; \
                 movement is a classifier (stationary/moving/unreliable), never a speed; \
                 shot times from ammo decrements carry the stated uncertainty"
            } else {
                "machine measurements; movement intervals marked source=measured come from the \
                 GSI position trace (real u/s velocity), source=visual from an optical-flow \
                 classifier; shot times from ammo decrements carry the stated uncertainty"
            },
            "movementIntervals": cv_report.movement,
            "shots": cv_report.shots,
            "flicks": cv_report.flicks,
            "speedDataAvailable": !cv_report.speed.is_empty(),
            "flowVsGsiYawRatio": cv_report.flow_yaw_ratio,
        },
        "cvCandidates": cv_report.candidates,
    });

    let manifest: String = plan
        .frames
        .iter()
        .filter(|f| received.contains(&f.t_us))
        .map(|f| {
            let t = f.t_us as f64 / 1e6;
            let rel = t - event.at_s;
            let rel = if rel <= 0.0 {
                format!("{:.2}s before the event", -rel)
            } else {
                format!("{rel:.2}s after the event")
            };
            format!("- frames/{} — t={t:.2}s ({rel})\n", store::frame_file_name(f.t_us))
        })
        .collect();

    let provider = llm::provider_for(&settings.llm_provider).map_err(fail("compose"))?;
    let schema = ir_analysis::parse::coach_output_schema();

    let templates = prompt::load(prompts_dir);
    let mut vars = std::collections::BTreeMap::new();
    vars.insert("event_kind", event_kind_label(&event.kind));
    vars.insert("event_at", format!("{:.2}", event.at_s));
    vars.insert(
        "context_json",
        serde_json::to_string_pretty(&context).unwrap_or_default(),
    );
    vars.insert("frame_manifest", manifest);
    vars.insert("output_instructions", provider.output_instructions(&schema));
    let system_prompt = prompt::render(&templates.system, &vars);
    let user_prompt = prompt::render(&templates.user, &vars);

    let _ = store::write_text(&dirs.run_dir, "prompt.system.md", &system_prompt);
    let _ = store::write_text(&dirs.run_dir, "prompt.user.md", &user_prompt);

    // ---- invoke ----------------------------------------------------------
    let cfg = LlmConfig {
        provider: settings.llm_provider.clone(),
        model: settings.llm_model.clone(),
        binary_path: settings.llm_binary_path.clone(),
        extra_args: settings.llm_extra_args.clone(),
        timeout_secs: settings.llm_timeout_seconds.max(30) as u64,
    };
    let mut req = LlmRequest {
        run_dir: dirs.run_dir.clone(),
        system_prompt,
        user_prompt,
        images: received
            .iter()
            .map(|&t| format!("frames/{}", store::frame_file_name(t)))
            .collect(),
        json_schema: Some(schema),
    };

    let invoke = |req: LlmRequest, attempt: u32| {
        let heartbeat_app = app.clone();
        let provider_name = settings.llm_provider.clone();
        let retry_tag = if attempt > 1 { " (retry)" } else { "" };
        emit_progress(
            app,
            "invoking",
            format!("asking {provider_name}{retry_tag}…"),
            0,
            0,
        );
        let provider = &provider;
        let cfg = &cfg;
        async move {
            let outcome =
                llm::run_llm(provider.as_ref(), &req, cfg, cancel, move |elapsed| {
                    emit_progress(
                        &heartbeat_app,
                        "invoking",
                        format!("asking {provider_name}{retry_tag}… {elapsed}s"),
                        elapsed,
                        0,
                    );
                })
                .await;
            // Persist raw CLI output win or lose — it's the debugging record.
            let suffix = if attempt > 1 { "2" } else { "" };
            if let Ok(o) = &outcome {
                let _ = store::write_text(&dirs.run_dir, &format!("llm_stdout{suffix}.txt"), &o.stdout);
                let _ = store::write_text(&dirs.run_dir, &format!("llm_stderr{suffix}.txt"), &o.stderr);
                let _ = store::write_json(&dirs.run_dir, &format!("llm_meta{suffix}.json"), o);
            }
            outcome
        }
    };

    let outcome = invoke(req.clone(), 1).await.map_err(fail("invoking"))?;

    // ---- parse (with one repair retry on shape mismatch) -----------------
    emit_progress(app, "parsing", "parsing findings".into(), 0, 0);
    let (output, outcome) = match ir_analysis::parse::parse_coach_output(&outcome.text) {
        Ok(output) => (output, outcome),
        Err(parse_err) => {
            warn!("coach output parse failed, retrying once: {parse_err}");
            req.user_prompt = format!(
                "{}\n\nYour previous reply could not be used: {parse_err}\n\
                 Respond with ONLY the required JSON object.",
                req.user_prompt
            );
            let retry = invoke(req, 2).await.map_err(fail("invoking"))?;
            match ir_analysis::parse::parse_coach_output(&retry.text) {
                Ok(output) => (output, retry),
                Err(e) => return Err(("parsing", LlmError::Parse(e))),
            }
        }
    };
    let (findings, mut degradations) = ir_analysis::parse::to_findings(&output);
    degradations.extend(cv::cv_degradations(&cv_report));
    if !frames.is_empty() && cv_report.flow.is_empty() {
        degradations.push("CV produced no flow trace (frames too sparse?)".into());
    }

    // ---- report ----------------------------------------------------------
    let report = AnalysisReport {
        schema_version: SCHEMA_VERSION,
        event: event.clone(),
        summary: output.summary.clone(),
        findings,
        metrics: json!({
            "candidates": cv_report.candidates,
            "shots": cv_report.shots,
            "flicks": cv_report.flicks,
            "movementIntervals": cv_report.movement,
            "flowTrace": overlay["flow"].clone(),
        }),
        provider: ProviderInfo {
            provider: settings.llm_provider.clone(),
            model: settings.llm_model.clone(),
            cli_version: outcome.cli_version.clone(),
            duration_ms: outcome.duration_ms,
        },
        degradations,
        analyzer_versions: cv_report.versions.clone(),
        frames: received.iter().map(|&t| t as f64 / 1e6).collect(),
    };
    store::persist_report(dirs, &report)
        .map_err(|e| ("persisting", LlmError::Other(e.to_string())))?;
    // Best-effort export surface (frames beside the cached report + the
    // clip-level import manifest) — a failure here shouldn't fail the run.
    if let Err(e) = store::promote_frames(dirs) {
        warn!("could not promote frames for export: {e}");
    }
    if let Err(e) = store::write_export_manifest(clip_mp4) {
        warn!("could not write export manifest: {e}");
    }
    info!(
        event = %event.id,
        findings = report.findings.len(),
        clip = %clip_mp4.display(),
        "analysis complete"
    );
    Ok((report, overlay))
}
