//! Run-dir layout and persistence. Every analysis run keeps everything it
//! saw and produced — frames, rendered prompts, raw CLI output, parsed
//! report — so runs are debuggable and a future eval harness can replay
//! them offline.
//!
//! ```text
//! clip_1751772000_001.analysis/
//!   kill_9200ms/
//!     report.json              # latest success = the cache
//!     runs/run_1751772100/
//!       request.json  frames/  prompt.system.md  prompt.user.md
//!       llm_stdout.txt  llm_stderr.txt  llm_meta.json  report.json
//! ```

use std::path::{Path, PathBuf};

use crate::types::{AnalysisReport, SCHEMA_VERSION};

#[derive(Debug, Clone)]
pub struct RunDirs {
    pub event_dir: PathBuf,
    pub run_dir: PathBuf,
    pub frames_dir: PathBuf,
}

/// `clip_..._001.mp4` -> `clip_..._001.analysis`.
pub fn analysis_dir(clip_mp4: &Path) -> PathBuf {
    clip_mp4.with_extension("analysis")
}

/// Create the directory tree for a new run of `event_id`.
pub fn prepare_run(clip_mp4: &Path, event_id: &str, run_stamp: u64) -> std::io::Result<RunDirs> {
    let event_dir = analysis_dir(clip_mp4).join(sanitize(event_id));
    let run_dir = event_dir.join("runs").join(format!("run_{run_stamp}"));
    let frames_dir = run_dir.join("frames");
    std::fs::create_dir_all(&frames_dir)?;
    Ok(RunDirs {
        event_dir,
        run_dir,
        frames_dir,
    })
}

/// Latest successful report for the event, or None (missing, unreadable, or
/// written by an incompatible schema — all treated as cache misses).
pub fn cached_report(clip_mp4: &Path, event_id: &str) -> Option<AnalysisReport> {
    let path = analysis_dir(clip_mp4)
        .join(sanitize(event_id))
        .join("report.json");
    let text = std::fs::read_to_string(path).ok()?;
    let report: AnalysisReport = serde_json::from_str(&text).ok()?;
    (report.schema_version == SCHEMA_VERSION).then_some(report)
}

/// Write the run's report and promote it to the event-level cache.
pub fn persist_report(dirs: &RunDirs, report: &AnalysisReport) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(report)?;
    std::fs::write(dirs.run_dir.join("report.json"), &json)?;
    std::fs::write(dirs.event_dir.join("report.json"), &json)
}

pub fn write_json<T: serde::Serialize>(
    dir: &Path,
    name: &str,
    value: &T,
) -> std::io::Result<()> {
    std::fs::write(dir.join(name), serde_json::to_string_pretty(value)?)
}

pub fn write_text(dir: &Path, name: &str, text: &str) -> std::io::Result<()> {
    std::fs::write(dir.join(name), text)
}

/// Frame file name for a given sample timestamp: `f_009200ms.jpg`.
pub fn frame_file_name(t_us: u64) -> String {
    format!("f_{:06}ms.jpg", t_us / 1000)
}

/// Resolve an analysis frame image for serving to the UI: newest run of the
/// event that has it. Inputs are sanitized — `file` must look like a frame
/// file name, so no path can escape the analysis dir.
pub fn find_analysis_frame(clip_mp4: &Path, event_id: &str, file: &str) -> Option<PathBuf> {
    if !file
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        || !file.ends_with(".jpg")
    {
        return None;
    }
    let runs_dir = analysis_dir(clip_mp4).join(sanitize(event_id)).join("runs");
    let mut runs: Vec<PathBuf> = std::fs::read_dir(&runs_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    runs.sort();
    runs.iter()
        .rev()
        .map(|run| run.join("frames").join(file))
        .find(|p| p.is_file())
}

/// Merge one thumbs verdict into the event's `feedback.json`
/// (`{"findings": {"<index>": true/false}}`) — day-one label collection for
/// the future eval harness.
pub fn record_feedback(
    clip_mp4: &Path,
    event_id: &str,
    finding_index: usize,
    up: bool,
) -> std::io::Result<()> {
    let path = analysis_dir(clip_mp4)
        .join(sanitize(event_id))
        .join("feedback.json");
    let mut value: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    value["findings"][finding_index.to_string()] = serde_json::json!(up);
    std::fs::write(&path, serde_json::to_string_pretty(&value)?)
}

/// Event ids come from the frontend; keep them filesystem-safe.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EventRef, ProviderInfo};
    use ir_types::MarkerKind;

    fn report(event_id: &str) -> AnalysisReport {
        AnalysisReport {
            schema_version: SCHEMA_VERSION,
            event: EventRef {
                id: event_id.into(),
                at_s: 9.2,
                kind: MarkerKind::Death,
            },
            summary: "test".into(),
            findings: vec![],
            metrics: serde_json::Value::Null,
            provider: ProviderInfo {
                provider: "claude".into(),
                model: String::new(),
                cli_version: "test".into(),
                duration_ms: 1,
            },
            degradations: vec![],
            analyzer_versions: Default::default(),
            frames: vec![],
        }
    }

    #[test]
    fn cache_roundtrip_and_version_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let mp4 = tmp.path().join("clip_1_001.mp4");
        let dirs = prepare_run(&mp4, "death_9200ms", 42).unwrap();
        assert!(cached_report(&mp4, "death_9200ms").is_none());

        persist_report(&dirs, &report("death_9200ms")).unwrap();
        assert!(cached_report(&mp4, "death_9200ms").is_some());

        // A schema bump invalidates the cache.
        let mut old = report("death_9200ms");
        old.schema_version = SCHEMA_VERSION + 1;
        persist_report(&dirs, &old).unwrap();
        assert!(cached_report(&mp4, "death_9200ms").is_none());
    }

    #[test]
    fn sanitize_strips_separators() {
        assert_eq!(sanitize("kill_9200ms"), "kill_9200ms");
        assert_eq!(sanitize("../evil/id"), "___evil_id");
    }

    #[test]
    fn find_analysis_frame_picks_newest_run_and_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let mp4 = tmp.path().join("clip_1_001.mp4");
        let old = prepare_run(&mp4, "kill_1ms", 1).unwrap();
        let new = prepare_run(&mp4, "kill_1ms", 2).unwrap();
        std::fs::write(old.frames_dir.join("f_000100ms.jpg"), b"old").unwrap();
        std::fs::write(new.frames_dir.join("f_000100ms.jpg"), b"new").unwrap();

        let found = find_analysis_frame(&mp4, "kill_1ms", "f_000100ms.jpg").unwrap();
        assert_eq!(std::fs::read(found).unwrap(), b"new");
        assert!(find_analysis_frame(&mp4, "kill_1ms", "../report.json").is_none());
        assert!(find_analysis_frame(&mp4, "kill_1ms", "f.png").is_none());
    }

    #[test]
    fn feedback_merges() {
        let tmp = tempfile::tempdir().unwrap();
        let mp4 = tmp.path().join("clip_1_001.mp4");
        prepare_run(&mp4, "kill_1ms", 1).unwrap();
        record_feedback(&mp4, "kill_1ms", 0, true).unwrap();
        record_feedback(&mp4, "kill_1ms", 2, false).unwrap();
        let text = std::fs::read_to_string(
            analysis_dir(&mp4).join("kill_1ms").join("feedback.json"),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["findings"]["0"], true);
        assert_eq!(v["findings"]["2"], false);
    }
}
