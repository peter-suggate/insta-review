//! `claude -p` provider. Images are read by the CLI's own Read tool: the
//! prompt references paths relative to the run dir (the child's cwd), and
//! the tool set is restricted to Read only.

use super::{LlmConfig, LlmError, LlmProvider, LlmRequest};

pub struct Claude;

impl LlmProvider for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn default_binary(&self) -> &'static str {
        "claude"
    }

    fn build_argv(&self, req: &LlmRequest, cfg: &LlmConfig) -> Vec<String> {
        let mut argv: Vec<String> = vec![
            "-p".into(),
            "--output-format".into(),
            "json".into(),
            // Read-only file access, no permission prompts, and a hermetic
            // run: no user/project settings, CLAUDE.md, or session files.
            "--tools".into(),
            "Read".into(),
            "--allowedTools".into(),
            "Read".into(),
            "--permission-mode".into(),
            "dontAsk".into(),
            "--setting-sources".into(),
            String::new(),
            "--no-session-persistence".into(),
        ];
        if !req.system_prompt.is_empty() {
            argv.push("--append-system-prompt".into());
            argv.push(req.system_prompt.clone());
        }
        if !cfg.model.is_empty() {
            argv.push("--model".into());
            argv.push(cfg.model.clone());
        }
        argv.extend(cfg.extra_args.split_whitespace().map(String::from));
        argv.push(req.user_prompt.clone());
        argv
    }

    fn parse(&self, stdout: &str, _result_file: Option<&str>) -> Result<String, LlmError> {
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|e| {
            LlmError::Parse(format!(
                "claude stdout is not the expected JSON envelope ({e}); first 200 chars: {}",
                stdout.chars().take(200).collect::<String>()
            ))
        })?;
        if envelope["is_error"].as_bool() == Some(true) {
            return Err(LlmError::Other(format!(
                "claude reported an error: {}",
                envelope["result"].as_str().unwrap_or("(no detail)")
            )));
        }
        // Structured output (when --json-schema is used) wins over prose.
        if let Some(structured) = envelope.get("structured_output") {
            if !structured.is_null() {
                return Ok(structured.to_string());
            }
        }
        envelope["result"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| LlmError::Parse("claude envelope has no `result` field".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_envelope() {
        let out = r#"{"type":"result","subtype":"success","is_error":false,"result":"coaching text"}"#;
        assert_eq!(Claude.parse(out, None).unwrap(), "coaching text");
    }

    #[test]
    fn error_envelope_is_error() {
        let out = r#"{"type":"result","is_error":true,"result":"boom"}"#;
        assert!(Claude.parse(out, None).is_err());
    }

    #[test]
    fn prompt_is_last_arg_and_no_bare() {
        let req = LlmRequest {
            run_dir: ".".into(),
            system_prompt: "sys".into(),
            user_prompt: "user".into(),
            images: vec![],
        };
        let cfg = LlmConfig {
            provider: "claude".into(),
            model: String::new(),
            binary_path: String::new(),
            extra_args: String::new(),
            timeout_secs: 240,
        };
        let argv = Claude.build_argv(&req, &cfg);
        assert_eq!(argv.last().unwrap(), "user");
        assert!(!argv.iter().any(|a| a == "--bare"));
    }
}
