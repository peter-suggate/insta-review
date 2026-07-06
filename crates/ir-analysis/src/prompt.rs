//! Prompt templates: plain-text files with `{{key}}` placeholders. Defaults
//! are embedded, but user-editable copies in the app config dir win — tuning
//! coaching is a text edit + Re-analyze, never a rebuild.

use std::collections::BTreeMap;
use std::path::Path;

pub const SYSTEM_FILE: &str = "coach.system.md";
pub const USER_FILE: &str = "coach.user.md";

const DEFAULT_SYSTEM: &str = include_str!("../assets/prompts/coach.system.md");
const DEFAULT_USER: &str = include_str!("../assets/prompts/coach.user.md");

#[derive(Debug, Clone)]
pub struct PromptTemplates {
    pub system: String,
    pub user: String,
}

/// Copy any *missing* default prompt files into `prompts_dir`. Never
/// overwrites — user edits are sacred.
pub fn ensure_defaults(prompts_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(prompts_dir)?;
    for (name, contents) in [(SYSTEM_FILE, DEFAULT_SYSTEM), (USER_FILE, DEFAULT_USER)] {
        let path = prompts_dir.join(name);
        if !path.exists() {
            std::fs::write(&path, contents)?;
        }
    }
    Ok(())
}

/// Load templates from `prompts_dir`, falling back to the embedded defaults
/// per file.
pub fn load(prompts_dir: &Path) -> PromptTemplates {
    let read = |name: &str, fallback: &str| {
        std::fs::read_to_string(prompts_dir.join(name)).unwrap_or_else(|_| fallback.to_string())
    };
    PromptTemplates {
        system: read(SYSTEM_FILE, DEFAULT_SYSTEM),
        user: read(USER_FILE, DEFAULT_USER),
    }
}

/// Substitute `{{key}}` placeholders. Unknown placeholders are left intact
/// so a template typo is visible in the persisted rendered prompt.
pub fn render(template: &str, vars: &BTreeMap<&str, String>) -> String {
    let mut out = template.to_string();
    for (key, value) in vars {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_and_preserves_unknown() {
        let mut vars = BTreeMap::new();
        vars.insert("a", "1".to_string());
        assert_eq!(render("x {{a}} {{b}}", &vars), "x 1 {{b}}");
    }

    #[test]
    fn defaults_have_expected_placeholders() {
        for key in [
            "event_kind",
            "event_at",
            "frame_manifest",
            "context_json",
            "output_instructions",
        ] {
            assert!(
                DEFAULT_USER.contains(&format!("{{{{{key}}}}}")),
                "coach.user.md is missing {{{{{key}}}}}"
            );
        }
    }
}
