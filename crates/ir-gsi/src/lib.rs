//! CS2 Game State Integration: payload model, marker derivation, and the
//! local HTTP listener CS2 posts to. VAC-safe by construction — this is
//! Valve's official, config-file-driven telemetry; nothing touches the
//! game process.

pub mod derive;
pub mod install;
pub mod model;
pub mod server;

pub use derive::{sample, Differ};
pub use model::GsiPayload;
pub use server::{GsiServer, GsiUpdate};

/// The gamestate_integration cfg CS2 needs. Installed into
/// `…/game/csgo/cfg/` (M4 adds automatic Steam-path discovery; until then
/// users copy it manually). NOTE: must be written WITHOUT a UTF-8 BOM or
/// CS2 silently ignores it.
///
/// `player_position` adds the local player's position + forward (view)
/// vector to each payload — the analysis layer derives real velocity and
/// view angles from it. The `output` precision block keeps the vectors at
/// two decimals instead of integers. Existing installs must re-install the
/// cfg (and restart CS2) to pick these up.
pub fn config_file_contents(port: u16, token: &str) -> String {
    format!(
        r#""insta-review"
{{
    "uri" "http://127.0.0.1:{port}"
    "timeout" "1.1"
    "buffer"  "0.0"
    "throttle" "0.1"
    "heartbeat" "10.0"
    "auth"
    {{
        "token" "{token}"
    }}
    "output"
    {{
        "precision_time"     "0.01"
        "precision_position" "0.01"
        "precision_vector"   "0.01"
    }}
    "data"
    {{
        "provider"            "1"
        "map"                 "1"
        "round"               "1"
        "player_id"           "1"
        "player_state"        "1"
        "player_match_stats"  "1"
        "player_weapons"      "1"
        "player_position"     "1"
    }}
}}
"#
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn cfg_subscribes_position_and_is_bom_free() {
        let cfg = super::config_file_contents(3585, "tok");
        assert!(cfg.contains(r#""player_position"     "1""#));
        assert!(cfg.contains(r#""precision_vector"   "0.01""#));
        assert!(cfg.is_ascii(), "cfg must be plain ASCII (no BOM)");
    }
}
