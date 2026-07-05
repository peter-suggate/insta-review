//! CS2 Game State Integration: payload model, marker derivation, and the
//! local HTTP listener CS2 posts to. VAC-safe by construction — this is
//! Valve's official, config-file-driven telemetry; nothing touches the
//! game process.

pub mod derive;
pub mod install;
pub mod model;
pub mod server;

pub use derive::Differ;
pub use model::GsiPayload;
pub use server::GsiServer;

/// The gamestate_integration cfg CS2 needs. Installed into
/// `…/game/csgo/cfg/` (M4 adds automatic Steam-path discovery; until then
/// users copy it manually). NOTE: must be written WITHOUT a UTF-8 BOM or
/// CS2 silently ignores it.
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
    "data"
    {{
        "provider"            "1"
        "map"                 "1"
        "round"               "1"
        "player_id"           "1"
        "player_state"        "1"
        "player_match_stats"  "1"
        "player_weapons"      "1"
    }}
}}
"#
    )
}
