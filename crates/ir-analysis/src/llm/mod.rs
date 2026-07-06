//! Headless LLM CLI invocation: provider abstraction, binary resolution
//! (Windows `.cmd` shims), timeout/cancel, and error classification.
//!
//! The CLIs run under the user's flat-rate subscription login — never pass
//! flags that restrict auth to API keys (claude's `--bare` does exactly
//! that).

pub mod claude;
pub mod codex;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::sync::Notify;
use tracing::info;

#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// "claude" | "codex".
    pub provider: String,
    /// Empty = provider default model.
    pub model: String,
    /// Empty = resolve provider's default binary name from PATH.
    pub binary_path: String,
    /// Whitespace-split, appended verbatim — the escape hatch for CLI flag
    /// drift without a rebuild.
    pub extra_args: String,
    pub timeout_secs: u64,
}

/// One request to a provider. Image paths are relative to `run_dir`, which
/// becomes the child's cwd.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub run_dir: PathBuf,
    pub system_prompt: String,
    pub user_prompt: String,
    pub images: Vec<String>,
    /// JSON Schema the reply must satisfy. Enforced natively where the CLI
    /// supports it (claude `--json-schema`); otherwise carried by
    /// [`LlmProvider::output_instructions`] in the prompt.
    pub json_schema: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("{0} subscription limit reached — try again later")]
    Quota(String),
    #[error("{0} CLI is not logged in (run it interactively once to authenticate)")]
    Auth(String),
    #[error("LLM timed out after {0}s")]
    Timeout(u64),
    #[error("analysis cancelled")]
    Cancelled,
    #[error("could not find the {name} CLI: {detail}")]
    BinaryNotFound { name: String, detail: String },
    #[error("LLM output could not be parsed: {0}")]
    Parse(String),
    #[error("LLM failed: {0}")]
    Other(String),
}

pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn default_binary(&self) -> &'static str;
    fn build_argv(&self, req: &LlmRequest, cfg: &LlmConfig) -> Vec<String>;
    /// Some providers write the final message to a file in cwd instead of
    /// clean stdout (codex `--output-last-message`).
    fn result_file(&self) -> Option<&'static str> {
        None
    }
    /// Extract the assistant's final text.
    fn parse(&self, stdout: &str, result_file: Option<&str>) -> Result<String, LlmError>;
    /// What to tell the model about the required output format, given the
    /// request's JSON schema. Providers with native schema enforcement need
    /// only a short note; the rest must carry the schema in the prompt.
    fn output_instructions(&self, schema: &str) -> String;
}

pub fn provider_for(name: &str) -> Result<Box<dyn LlmProvider>, LlmError> {
    match name {
        "claude" => Ok(Box::new(claude::Claude)),
        "codex" => Ok(Box::new(codex::Codex)),
        other => Err(LlmError::Other(format!(
            "unknown LLM provider {other:?} (expected \"claude\" or \"codex\")"
        ))),
    }
}

/// Everything about one CLI run, persisted to `llm_meta.json` so a run is
/// reproducible/debuggable after the fact.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmOutcome {
    pub text: String,
    #[serde(skip)]
    pub stdout: String,
    #[serde(skip)]
    pub stderr: String,
    pub argv: Vec<String>,
    pub binary: String,
    pub cli_version: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
}

/// Resolve the command to spawn. npm-installed CLIs on Windows are `.cmd`
/// shims which CreateProcess won't exec directly — wrap those in `cmd /C`.
fn resolve_command(cfg: &LlmConfig, provider: &dyn LlmProvider) -> (String, Vec<String>) {
    let path = if cfg.binary_path.is_empty() {
        provider.default_binary().to_string()
    } else {
        cfg.binary_path.clone()
    };
    #[cfg(windows)]
    {
        let resolved = if path.contains(['/', '\\']) {
            path.clone()
        } else {
            where_first(&path).unwrap_or(path.clone())
        };
        let lower = resolved.to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            return ("cmd".into(), vec!["/C".into(), resolved]);
        }
        return (resolved, vec![]);
    }
    #[cfg(not(windows))]
    (path, vec![])
}

#[cfg(windows)]
fn where_first(name: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("where");
    cmd.arg(name);
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

fn command(bin: &str, prefix: &[String]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(prefix);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

async fn cli_version(bin: &str, prefix: &[String]) -> String {
    let run = async {
        let out = command(bin, prefix)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .map(|l| l.trim().to_string())
    };
    tokio::time::timeout(Duration::from_secs(15), run)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "unknown".into())
}

/// Spawn the provider CLI and wait, with timeout, cancellation, and a 1 Hz
/// `heartbeat(elapsed_secs)` for UI liveness during the long call.
pub async fn run_llm(
    provider: &dyn LlmProvider,
    req: &LlmRequest,
    cfg: &LlmConfig,
    cancel: &Notify,
    heartbeat: impl Fn(u64),
) -> Result<LlmOutcome, LlmError> {
    let (bin, prefix) = resolve_command(cfg, provider);
    let argv = provider.build_argv(req, cfg);
    let version = cli_version(&bin, &prefix).await;
    info!(bin, version, "invoking LLM CLI");

    let started = Instant::now();
    let mut child = command(&bin, &prefix)
        .args(&argv)
        .current_dir(&req.run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => LlmError::BinaryNotFound {
                name: bin.clone(),
                detail: "not on PATH — set the LLM binary path in settings".into(),
            },
            _ => LlmError::Other(format!("spawn {bin}: {e}")),
        })?;

    // Drain pipes concurrently — a full pipe would deadlock the child.
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let out_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf).await;
        buf
    });
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    let deadline = tokio::time::sleep(Duration::from_secs(cfg.timeout_secs));
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let status = loop {
        tokio::select! {
            status = child.wait() => {
                break status.map_err(|e| LlmError::Other(format!("wait on {bin}: {e}")))?;
            }
            _ = tick.tick() => heartbeat(started.elapsed().as_secs()),
            _ = cancel.notified() => {
                let _ = child.kill().await;
                return Err(LlmError::Cancelled);
            }
            _ = &mut deadline => {
                let _ = child.kill().await;
                return Err(LlmError::Timeout(cfg.timeout_secs));
            }
        }
    };

    let stdout = String::from_utf8_lossy(&out_task.await.unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_task.await.unwrap_or_default()).into_owned();
    let duration_ms = started.elapsed().as_millis() as u64;

    let mut outcome = LlmOutcome {
        text: String::new(),
        stdout,
        stderr,
        argv,
        binary: bin,
        cli_version: version,
        exit_code: status.code(),
        duration_ms,
    };

    if !status.success() {
        return Err(classify_failure(
            provider.id(),
            status.code(),
            &outcome.stdout,
            &outcome.stderr,
        ));
    }

    let result_file = provider
        .result_file()
        .map(|name| std::fs::read_to_string(req.run_dir.join(name)))
        .transpose()
        .map_err(|e| LlmError::Other(format!("read result file: {e}")))?;
    outcome.text = provider.parse(&outcome.stdout, result_file.as_deref())?;
    Ok(outcome)
}

/// Turn a non-zero exit into a human-actionable error. Pattern matching on
/// CLI output is inherently fuzzy — the raw tail rides along for "details".
fn classify_failure(provider: &str, code: Option<i32>, stdout: &str, stderr: &str) -> LlmError {
    let hay = format!("{stdout}\n{stderr}").to_lowercase();
    const QUOTA: &[&str] = &[
        "rate limit",
        "rate-limit",
        "usage limit",
        "quota",
        "overloaded",
        "credit balance",
        "out of credits",
    ];
    const AUTH: &[&str] = &[
        "not logged in",
        "please log in",
        "please login",
        "unauthorized",
        "authentication",
        "invalid api key",
        "401",
        "403",
    ];
    if QUOTA.iter().any(|p| hay.contains(p)) {
        return LlmError::Quota(provider.into());
    }
    if AUTH.iter().any(|p| hay.contains(p)) {
        return LlmError::Auth(provider.into());
    }
    let tail: String = stderr
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let tail = if tail.trim().is_empty() {
        stdout.chars().rev().take(400).collect::<String>().chars().rev().collect()
    } else {
        tail
    };
    LlmError::Other(format!(
        "{provider} exited with {code:?}: {}",
        tail.trim()
    ))
}
