//! `codex exec` provider. Images are first-class (`-i`); the final message
//! is read from `--output-last-message` rather than parsing chatty stdout.
//! No native schema enforcement — the schema rides in the prompt via
//! `output_instructions`, and the tolerant JSON extraction downstream does
//! the rest.
//!
//! NOTE: not yet live-verified — the dev Mac's codex install was quarantined
//! by AV (2026-07-06); verify on a healthy install (Windows box) and adjust
//! flags via the `llmExtraArgs` setting if the CLI has drifted.

use super::{LlmConfig, LlmError, LlmProvider, LlmRequest};

pub const RESULT_FILE: &str = "last_message.txt";

pub struct Codex;

impl LlmProvider for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn default_binary(&self) -> &'static str {
        "codex"
    }

    fn build_argv(&self, req: &LlmRequest, cfg: &LlmConfig) -> Vec<String> {
        let mut argv: Vec<String> = vec![
            "exec".into(),
            "--skip-git-repo-check".into(),
            "--sandbox".into(),
            "read-only".into(),
            "--output-last-message".into(),
            RESULT_FILE.into(),
        ];
        for image in &req.images {
            argv.push("-i".into());
            argv.push(image.clone());
        }
        if !cfg.model.is_empty() {
            argv.push("-m".into());
            argv.push(cfg.model.clone());
        }
        argv.extend(cfg.extra_args.split_whitespace().map(String::from));
        // codex has no system-prompt flag in exec mode; prepend it.
        let prompt = if req.system_prompt.is_empty() {
            req.user_prompt.clone()
        } else {
            format!("{}\n\n---\n\n{}", req.system_prompt, req.user_prompt)
        };
        argv.push(prompt);
        argv
    }

    fn result_file(&self) -> Option<&'static str> {
        Some(RESULT_FILE)
    }

    fn parse(&self, stdout: &str, result_file: Option<&str>) -> Result<String, LlmError> {
        match result_file {
            Some(text) if !text.trim().is_empty() => Ok(text.trim().to_string()),
            // Fall back to stdout if the file is missing/empty (flag drift).
            _ if !stdout.trim().is_empty() => Ok(stdout.trim().to_string()),
            _ => Err(LlmError::Parse(
                "codex produced no output (empty last_message and stdout)".into(),
            )),
        }
    }

    fn output_instructions(&self, schema: &str) -> String {
        format!(
            "End your reply with exactly one fenced ```json code block containing a \
             single JSON object that validates against this JSON Schema, and nothing \
             after the block:\n{schema}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> LlmRequest {
        LlmRequest {
            run_dir: ".".into(),
            system_prompt: "sys".into(),
            user_prompt: "user".into(),
            images: vec!["frames/a.jpg".into(), "frames/b.jpg".into()],
            json_schema: Some("{}".into()),
        }
    }

    fn cfg() -> LlmConfig {
        LlmConfig {
            provider: "codex".into(),
            model: "gpt-5".into(),
            binary_path: String::new(),
            extra_args: String::new(),
            timeout_secs: 240,
        }
    }

    #[test]
    fn argv_shape() {
        let argv = Codex.build_argv(&req(), &cfg());
        assert_eq!(argv[0], "exec");
        assert_eq!(argv.iter().filter(|a| *a == "-i").count(), 2);
        assert!(argv.windows(2).any(|w| w[0] == "-m" && w[1] == "gpt-5"));
        // System prompt is folded into the trailing prompt.
        assert!(argv.last().unwrap().starts_with("sys"));
        assert!(argv.last().unwrap().ends_with("user"));
    }

    #[test]
    fn parse_prefers_result_file_and_falls_back() {
        assert_eq!(Codex.parse("noise", Some("answer")).unwrap(), "answer");
        assert_eq!(Codex.parse("stdout answer", Some("  ")).unwrap(), "stdout answer");
        assert!(Codex.parse("", None).is_err());
    }
}
