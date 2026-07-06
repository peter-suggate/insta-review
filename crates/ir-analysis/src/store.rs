//! Run-dir layout and persistence. Every analysis run keeps everything it
//! saw and produced — frames, rendered prompts, raw CLI output, parsed
//! report — so runs are debuggable and a future eval harness can replay
//! them offline.
//!
//! ```text
//! clip_1751772000_001.export.json  # import manifest (clip + all events)
//! clip_1751772000_001.analysis/
//!   kill_9200ms/
//!     report.json              # latest success = the cache
//!     frames/                  # promoted from the latest successful run
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

/// Copy the run's evidence frames up to `event_dir/frames/`, mirroring how
/// `persist_report` promotes the report: the event dir is the import
/// surface, self-contained regardless of which run produced it.
pub fn promote_frames(dirs: &RunDirs) -> std::io::Result<usize> {
    let dst_dir = dirs.event_dir.join("frames");
    std::fs::create_dir_all(&dst_dir)?;
    let mut copied = 0;
    for entry in std::fs::read_dir(&dirs.frames_dir)?.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().ends_with(".jpg") {
            std::fs::copy(entry.path(), dst_dir.join(&name))?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// One-file import surface: `clip_X.export.json` next to the clip indexes
/// the mp4, its meta sidecar, and every analyzed event's report, promoted
/// frames, and feedback — all as manifest-relative paths with forward
/// slashes, so the clip directory can be moved or shared wholesale.
/// Rewritten on save and after every successful analysis; idempotent.
pub fn write_export_manifest(clip_mp4: &Path) -> std::io::Result<PathBuf> {
    let file_name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
    let clip_name = file_name(clip_mp4)
        .ok_or_else(|| std::io::Error::other("clip path has no file name"))?;
    let ana_dir = analysis_dir(clip_mp4);
    let ana_name = file_name(&ana_dir).unwrap_or_default();

    let mut events = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&ana_dir) {
        let mut event_dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        event_dirs.sort();
        for event_dir in event_dirs {
            let Some(event_id) = file_name(&event_dir) else { continue };
            // Only events with a schema-compatible report are importable.
            let Ok(text) = std::fs::read_to_string(event_dir.join("report.json")) else {
                continue;
            };
            let Ok(report) = serde_json::from_str::<AnalysisReport>(&text) else {
                continue;
            };
            if report.schema_version != SCHEMA_VERSION {
                continue;
            }
            let mut frames: Vec<String> = std::fs::read_dir(event_dir.join("frames"))
                .map(|it| {
                    it.flatten()
                        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                        .filter(|n| n.ends_with(".jpg"))
                        .map(|n| format!("{ana_name}/{event_id}/frames/{n}"))
                        .collect()
                })
                .unwrap_or_default();
            frames.sort();
            let feedback = event_dir
                .join("feedback.json")
                .is_file()
                .then(|| format!("{ana_name}/{event_id}/feedback.json"));
            events.push(serde_json::json!({
                "id": event_id,
                "atS": report.event.at_s,
                "kind": report.event.kind,
                "summary": report.summary,
                "report": format!("{ana_name}/{event_id}/report.json"),
                "frames": frames,
                "feedback": feedback,
            }));
        }
    }

    let meta_sidecar = clip_mp4.with_extension("json");
    let manifest = serde_json::json!({
        "kind": "insta-review-export",
        "schemaVersion": SCHEMA_VERSION,
        "exportedAtS": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "clip": clip_name,
        "meta": meta_sidecar.is_file().then(|| file_name(&meta_sidecar)).flatten(),
        "events": events,
    });
    let path = clip_mp4.with_extension("export.json");
    std::fs::write(&path, serde_json::to_string_pretty(&manifest)?)?;
    Ok(path)
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

/// Tick-exact shot events from a demo-enrichment sidecar
/// (`<clip>.demo.json`, written by `ir-cli demo-enrich`), or None when
/// absent/unreadable. Uncertainty = one 64-tick interval.
pub fn load_demo_shots(clip_mp4: &Path) -> Option<Vec<crate::cv::motion::ShotEvent>> {
    let text = std::fs::read_to_string(clip_mp4.with_extension("demo.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let shots = v.get("shots")?.as_array()?;
    Some(
        shots
            .iter()
            .filter_map(|s| {
                Some(crate::cv::motion::ShotEvent {
                    t: s.get("t")?.as_f64()?,
                    count: 1,
                    uncertainty_s: 1.0 / 64.0,
                    weapon: s
                        .get("weapon")
                        .and_then(|w| w.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect(),
    )
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
    fn export_manifest_indexes_promoted_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let mp4 = tmp.path().join("clip_1_001.mp4");
        std::fs::write(mp4.with_extension("json"), "{}").unwrap();

        let dirs = prepare_run(&mp4, "death_9200ms", 1).unwrap();
        std::fs::write(dirs.frames_dir.join("f_009200ms.jpg"), b"jpg").unwrap();
        persist_report(&dirs, &report("death_9200ms")).unwrap();
        assert_eq!(promote_frames(&dirs).unwrap(), 1);
        record_feedback(&mp4, "death_9200ms", 0, true).unwrap();

        let path = write_export_manifest(&mp4).unwrap();
        assert_eq!(path, mp4.with_extension("export.json"));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["kind"], "insta-review-export");
        assert_eq!(v["clip"], "clip_1_001.mp4");
        assert_eq!(v["meta"], "clip_1_001.json");
        let ev = &v["events"][0];
        assert_eq!(ev["id"], "death_9200ms");
        assert_eq!(ev["report"], "clip_1_001.analysis/death_9200ms/report.json");
        assert_eq!(
            ev["frames"][0],
            "clip_1_001.analysis/death_9200ms/frames/f_009200ms.jpg"
        );
        assert_eq!(
            ev["feedback"],
            "clip_1_001.analysis/death_9200ms/feedback.json"
        );

        // Stale-schema events are excluded from the manifest.
        let dirs2 = prepare_run(&mp4, "kill_2ms", 2).unwrap();
        let mut old = report("kill_2ms");
        old.schema_version = SCHEMA_VERSION + 1;
        persist_report(&dirs2, &old).unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(write_export_manifest(&mp4).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(v["events"].as_array().unwrap().len(), 1);
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
    fn demo_shots_load_from_enrichment_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let mp4 = tmp.path().join("clip_1_001.mp4");
        assert!(load_demo_shots(&mp4).is_none());
        std::fs::write(
            tmp.path().join("clip_1_001.demo.json"),
            r#"{"schemaVersion":1,"shots":[
                {"t":1.5,"weapon":"weapon_ak47"},
                {"t":9.2,"weapon":"weapon_ak47"}]}"#,
        )
        .unwrap();
        let shots = load_demo_shots(&mp4).unwrap();
        assert_eq!(shots.len(), 2);
        assert!((shots[1].t - 9.2).abs() < 1e-9);
        assert!(shots[0].uncertainty_s < 0.02);
        assert_eq!(shots[0].weapon, "weapon_ak47");
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
