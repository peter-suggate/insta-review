//! The model-facing output contract: what the LLM must return, the JSON
//! Schema that enforces it (claude `--json-schema`; embedded in the prompt
//! for providers without schema support), and the parsing/mapping into
//! [`Finding`]s — including the tolerant extraction path for chatty output.

use serde::{Deserialize, Serialize};

use crate::types::{Evidence, Finding, FindingSource, Severity};

/// What the model returns. Deliberately flatter than [`Finding`] — models
/// fill simple shapes more reliably.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoachOutput {
    pub summary: String,
    /// Did the frames visually confirm the kill/death (kill feed, death
    /// state)? False caps every confidence — see [`to_findings`].
    pub event_confirmed: bool,
    #[serde(default)]
    pub findings: Vec<CoachFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoachFinding {
    pub kind: String,
    pub severity: Severity,
    pub confidence: f32,
    pub start_s: f64,
    pub end_s: f64,
    pub coaching: String,
    #[serde(default)]
    pub evidence: Vec<CoachEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoachEvidence {
    pub t: f64,
    #[serde(default)]
    pub note: String,
}

/// JSON Schema for [`CoachOutput`], as the string claude's `--json-schema`
/// wants. Kept in lockstep with the structs by the roundtrip test below.
pub fn coach_output_schema() -> String {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "eventConfirmed", "findings"],
        "properties": {
            "summary": { "type": "string" },
            "eventConfirmed": { "type": "boolean" },
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind", "severity", "confidence", "startS", "endS", "coaching"],
                    "properties": {
                        "kind": { "type": "string" },
                        "severity": { "enum": ["positive", "info", "minor", "major"] },
                        "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                        "startS": { "type": "number" },
                        "endS": { "type": "number" },
                        "coaching": { "type": "string" },
                        "evidence": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["t"],
                                "properties": {
                                    "t": { "type": "number" },
                                    "note": { "type": "string" }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
    .to_string()
}

/// Parse the model's text into a [`CoachOutput`]: direct JSON first, then
/// the last fenced ```json block, then the first balanced top-level object.
/// The error carries the serde detail — it goes back to the model verbatim
/// on the repair retry.
pub fn parse_coach_output(text: &str) -> Result<CoachOutput, String> {
    let direct = serde_json::from_str::<CoachOutput>(text.trim());
    if let Ok(out) = direct {
        return Ok(out);
    }
    let candidate = last_fenced_json(text)
        .or_else(|| first_balanced_object(text))
        .ok_or_else(|| "no JSON object found in the reply".to_string())?;
    serde_json::from_str::<CoachOutput>(candidate)
        .map_err(|e| format!("JSON does not match the required shape: {e}"))
}

/// Map the model's output into report [`Finding`]s. An unconfirmed event
/// caps every confidence at 0.4 (the model could not visually verify what
/// it was coaching on) and records the degradation.
pub fn to_findings(output: &CoachOutput) -> (Vec<Finding>, Vec<String>) {
    let mut degradations = Vec::new();
    let cap = if output.event_confirmed {
        1.0
    } else {
        degradations
            .push("event not visually confirmed (no kill feed / death state in frames)".into());
        0.4
    };
    let findings = output
        .findings
        .iter()
        .map(|f| Finding {
            kind: f.kind.clone(),
            severity: f.severity,
            confidence: f.confidence.clamp(0.0, 1.0).min(cap),
            time_range: (f.start_s, f.end_s.max(f.start_s)),
            evidence: f
                .evidence
                .iter()
                .map(|e| Evidence {
                    t: e.t,
                    frame_label: None,
                    note: e.note.clone(),
                })
                .collect(),
            metrics: serde_json::Value::Null,
            coaching: f.coaching.clone(),
            source: FindingSource::Llm,
        })
        .collect();
    (findings, degradations)
}

fn last_fenced_json(text: &str) -> Option<&str> {
    let mut result = None;
    let mut rest = text;
    while let Some(start) = rest.find("```json") {
        let body = &rest[start + 7..];
        match body.find("```") {
            Some(end) => {
                result = Some(body[..end].trim());
                rest = &body[end + 3..];
            }
            None => break,
        }
    }
    result
}

/// First `{...}` with balanced braces, ignoring braces inside JSON strings.
fn first_balanced_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{"summary":"s","eventConfirmed":true,"findings":[
        {"kind":"moving_while_shooting","severity":"major","confidence":0.8,
         "startS":7.5,"endS":8.2,"coaching":"stop before firing",
         "evidence":[{"t":7.8,"note":"strafing during burst"}]}]}"#;

    #[test]
    fn direct_json_parses() {
        let out = parse_coach_output(GOOD).unwrap();
        assert!(out.event_confirmed);
        assert_eq!(out.findings[0].kind, "moving_while_shooting");
    }

    #[test]
    fn fenced_json_in_chatty_output() {
        let text = format!("Here is my analysis.\n```json\n{GOOD}\n```\nHope that helps!");
        assert!(parse_coach_output(&text).is_ok());
    }

    #[test]
    fn bare_object_in_prose() {
        let text = format!("Analysis follows: {GOOD} — done.");
        assert!(parse_coach_output(&text).is_ok());
    }

    #[test]
    fn braces_inside_strings_do_not_confuse_extraction() {
        let tricky = r#"note: {"summary":"uses { and } and \" fine","eventConfirmed":true,"findings":[]} end"#;
        let out = parse_coach_output(tricky).unwrap();
        assert!(out.summary.contains('{'));
    }

    #[test]
    fn wrong_shape_reports_serde_detail() {
        let err = parse_coach_output(r#"{"summary":"x"}"#).unwrap_err();
        assert!(err.contains("eventConfirmed"), "unhelpful error: {err}");
    }

    #[test]
    fn unconfirmed_event_caps_confidence() {
        let mut out = parse_coach_output(GOOD).unwrap();
        out.event_confirmed = false;
        let (findings, degradations) = to_findings(&out);
        assert_eq!(findings[0].confidence, 0.4);
        assert_eq!(degradations.len(), 1);
    }

    #[test]
    fn schema_accepts_what_the_structs_serialize() {
        // Keep the hand-written schema honest: everything CoachOutput
        // serializes must be valid against it (checked structurally: field
        // names + severity casing).
        let out = parse_coach_output(GOOD).unwrap();
        let value = serde_json::to_value(&out).unwrap();
        let schema: serde_json::Value =
            serde_json::from_str(&coach_output_schema()).unwrap();
        let props = schema["properties"].as_object().unwrap();
        for key in value.as_object().unwrap().keys() {
            assert!(props.contains_key(key), "schema is missing {key}");
        }
        let fprops = schema["properties"]["findings"]["items"]["properties"]
            .as_object()
            .unwrap();
        for key in value["findings"][0].as_object().unwrap().keys() {
            assert!(fprops.contains_key(key), "finding schema is missing {key}");
        }
        assert!(schema["properties"]["findings"]["items"]["properties"]["severity"]["enum"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("major")));
    }
}
