//! Locate the CS2 cfg directory (Steam registry + libraryfolders.vdf) and
//! install the gamestate_integration cfg. Writing one file into the CS2
//! cfg dir is the only thing we ever touch outside our own directories —
//! callers must get explicit user consent first.

#[cfg(not(windows))]
use std::path::Path;
use std::path::PathBuf;

/// Extract library paths from a Steam `libraryfolders.vdf`. Line-oriented
/// scan for `"path" "<value>"` — resilient to VDF nesting quirks.
pub fn parse_library_paths(vdf: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in vdf.lines() {
        let mut parts = line.trim().split('"').filter(|s| !s.trim().is_empty());
        if parts.next() == Some("path") {
            if let Some(value) = parts.next() {
                // VDF escapes backslashes.
                out.push(PathBuf::from(value.replace("\\\\", "\\")));
            }
        }
    }
    out
}

/// The Steam install dir, from the registry (Windows) or well-known paths.
pub fn steam_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Ok(key) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .open_subkey("Software\\Valve\\Steam")
        {
            if let Ok(path) = key.get_value::<String, _>("SteamPath") {
                let p = PathBuf::from(path);
                if p.exists() {
                    return Some(p);
                }
            }
        }
        for candidate in ["C:\\Program Files (x86)\\Steam", "C:\\Program Files\\Steam"] {
            let p = PathBuf::from(candidate);
            if p.exists() {
                return Some(p);
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        // Dev convenience only — CS2 doesn't run here.
        let home = std::env::var_os("HOME")?;
        let p = Path::new(&home).join("Library/Application Support/Steam");
        p.exists().then_some(p)
    }
}

const CS2_CFG_SUFFIX: &str = "steamapps/common/Counter-Strike Global Offensive/game/csgo/cfg";

/// Find the CS2 cfg directory across all Steam libraries.
pub fn find_cs2_cfg_dir() -> Result<PathBuf, String> {
    let root = steam_root().ok_or("Steam installation not found")?;
    let mut libraries = vec![root.clone()];
    let vdf_path = root.join("steamapps/libraryfolders.vdf");
    if let Ok(vdf) = std::fs::read_to_string(&vdf_path) {
        libraries.extend(parse_library_paths(&vdf));
    }
    for lib in &libraries {
        let cfg = lib.join(CS2_CFG_SUFFIX);
        if cfg.is_dir() {
            return Ok(cfg);
        }
    }
    Err(format!(
        "CS2 not found in any Steam library (checked {} libraries under {})",
        libraries.len(),
        root.display()
    ))
}

/// Write `gamestate_integration_instareview.cfg` into the CS2 cfg dir.
/// Returns the path written. Plain ASCII, no UTF-8 BOM (CS2 silently
/// ignores the file otherwise).
pub fn install_cfg(port: u16, token: &str) -> Result<PathBuf, String> {
    let dir = find_cs2_cfg_dir()?;
    let path = dir.join("gamestate_integration_instareview.cfg");
    std::fs::write(&path, crate::config_file_contents(port, token))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Where the cfg would go, without writing (for consent prompts).
pub fn cfg_target_path() -> Result<PathBuf, String> {
    Ok(find_cs2_cfg_dir()?.join("gamestate_integration_instareview.cfg"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_library_folders_vdf() {
        let vdf = r#"
"libraryfolders"
{
    "0"
    {
        "path"        "C:\\Program Files (x86)\\Steam"
        "label"       ""
        "apps" { "730" "39468066618" }
    }
    "1"
    {
        "path"        "D:\\SteamLibrary"
        "apps" { }
    }
}
"#;
        let paths = parse_library_paths(vdf);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("C:\\Program Files (x86)\\Steam"),
                PathBuf::from("D:\\SteamLibrary"),
            ]
        );
    }

    #[test]
    fn cfg_contents_have_no_bom_and_are_ascii() {
        let cfg = crate::config_file_contents(3585, "secret");
        assert!(cfg.is_ascii());
        assert!(!cfg.starts_with('\u{feff}'));
        assert!(cfg.contains(r#""uri" "http://127.0.0.1:3585""#));
        assert!(cfg.contains(r#""token" "secret""#));
    }
}
