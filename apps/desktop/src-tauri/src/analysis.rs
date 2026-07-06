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
    /// t_us of frames the webview has delivered so far.
    pub received: Vec<u64>,
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

    if let Some(report) = store::cached_report(&clip_mp4, &event.id) {
        info!(event = %event.id, "analysis cache hit");
        return Ok(BeginResponse {
            cached: Some(report),
            plan: None,
        });
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
        cancel: Arc::new(CancelSignal::new()),
        running: false,
    });

    Ok(BeginResponse {
        cached: None,
        plan: Some(response_plan),
    })
}

/// One extracted frame from the webview. Raw-payload invoke: JPEG bytes as
/// the body, timestamp in a `t-us` header (headers are the only side
/// channel Tauri gives a raw body).
#[tauri::command]
pub fn analysis_frame(
    app: AppHandle,
    state: State<AppState>,
    request: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected a raw body".into());
    };
    let t_us: u64 = request
        .headers()
        .get("t-us")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or("missing/invalid t-us header")?;

    let mut active = state.analysis.lock().unwrap();
    let active = active.as_mut().ok_or("no analysis in progress")?;
    if !active.plan.frames.iter().any(|f| f.t_us == t_us) {
        return Err(format!("frame {t_us} is not in the extraction plan"));
    }

    let path = active.dirs.frames_dir.join(store::frame_file_name(t_us));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    if !active.received.contains(&t_us) {
        active.received.push(t_us);
    }
    emit_progress(
        &app,
        "extracting",
        format!("frame {}/{}", active.received.len(), active.plan.frames.len()),
        active.received.len() as u64,
        active.plan.frames.len() as u64,
    );
    Ok(())
}

/// Kick off the pipeline task: compose prompts, invoke the LLM CLI, parse,
/// persist, emit. Returns immediately; results arrive as events.
#[tauri::command]
pub fn analysis_run(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let (event, clip_mp4, dirs, plan, meta, received, cancel) = {
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
            active.cancel.clone(),
        )
    };
    let settings = state.settings.lock().unwrap().clone();
    let prompts_dir = prompts_dir(&app);

    tauri::async_runtime::spawn(async move {
        let result = run_pipeline(
            &app, &event, &clip_mp4, &dirs, &plan, &meta, &received, &cancel, &settings,
            &prompts_dir,
        )
        .await;
        match result {
            Ok(report) => {
                let _ = app.emit("analysis-complete", json!({ "report": report }));
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
    cancel: &CancelSignal,
    settings: &AppSettings,
    prompts_dir: &std::path::Path,
) -> Result<AnalysisReport, (&'static str, LlmError)> {
    let fail = |stage: &'static str| move |e: LlmError| (stage, e);

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

    let templates = prompt::load(prompts_dir);
    let mut vars = std::collections::BTreeMap::new();
    vars.insert("event_kind", event_kind_label(&event.kind));
    vars.insert("event_at", format!("{:.2}", event.at_s));
    vars.insert(
        "context_json",
        serde_json::to_string_pretty(&context).unwrap_or_default(),
    );
    vars.insert("frame_manifest", manifest);
    let system_prompt = prompt::render(&templates.system, &vars);
    let user_prompt = prompt::render(&templates.user, &vars);

    let _ = store::write_text(&dirs.run_dir, "prompt.system.md", &system_prompt);
    let _ = store::write_text(&dirs.run_dir, "prompt.user.md", &user_prompt);

    // ---- invoke ----------------------------------------------------------
    let provider =
        llm::provider_for(&settings.llm_provider).map_err(fail("compose"))?;
    let cfg = LlmConfig {
        provider: settings.llm_provider.clone(),
        model: settings.llm_model.clone(),
        binary_path: settings.llm_binary_path.clone(),
        extra_args: settings.llm_extra_args.clone(),
        timeout_secs: settings.llm_timeout_seconds.max(30) as u64,
    };
    let req = LlmRequest {
        run_dir: dirs.run_dir.clone(),
        system_prompt,
        user_prompt,
        images: received
            .iter()
            .map(|&t| format!("frames/{}", store::frame_file_name(t)))
            .collect(),
    };

    let heartbeat_app = app.clone();
    let provider_name = settings.llm_provider.clone();
    emit_progress(app, "invoking", format!("asking {provider_name}…"), 0, 0);
    let outcome = llm::run_llm(provider.as_ref(), &req, &cfg, cancel, move |elapsed| {
        emit_progress(
            &heartbeat_app,
            "invoking",
            format!("asking {provider_name}… {elapsed}s"),
            elapsed,
            0,
        );
    })
    .await;

    // Persist raw CLI output win or lose — it's the debugging record.
    if let Ok(o) = &outcome {
        let _ = store::write_text(&dirs.run_dir, "llm_stdout.txt", &o.stdout);
        let _ = store::write_text(&dirs.run_dir, "llm_stderr.txt", &o.stderr);
        let _ = store::write_json(&dirs.run_dir, "llm_meta.json", o);
    }
    let outcome = outcome.map_err(fail("invoking"))?;

    // ---- report ----------------------------------------------------------
    emit_progress(app, "parsing", "saving report".into(), 0, 0);
    let report = AnalysisReport {
        schema_version: SCHEMA_VERSION,
        event: event.clone(),
        summary: outcome.text.clone(),
        findings: vec![],
        metrics: serde_json::Value::Null,
        provider: ProviderInfo {
            provider: settings.llm_provider.clone(),
            model: settings.llm_model.clone(),
            cli_version: outcome.cli_version.clone(),
            duration_ms: outcome.duration_ms,
        },
        degradations: vec![],
        analyzer_versions: Default::default(),
    };
    store::persist_report(dirs, &report)
        .map_err(|e| ("persisting", LlmError::Other(e.to_string())))?;
    info!(event = %event.id, clip = %clip_mp4.display(), "analysis complete");
    Ok(report)
}
