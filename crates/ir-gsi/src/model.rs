//! CS2 Game State Integration payload model. Everything is optional —
//! CS2 sends whatever data blocks the .cfg subscribes to, and fields come
//! and go with game state (menus, death, spectating).

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GsiPayload {
    pub provider: Option<Provider>,
    pub map: Option<Map>,
    pub round: Option<Round>,
    pub player: Option<Player>,
    pub auth: Option<Auth>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Auth {
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Provider {
    /// SteamID64 of the account running the game — used to distinguish
    /// "playing" from "observing someone".
    pub steamid: Option<String>,
    pub timestamp: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Map {
    pub name: Option<String>,
    pub phase: Option<String>,
    pub round: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Round {
    /// "freezetime" | "live" | "over"
    pub phase: Option<String>,
    /// "planted" | "exploded" | "defused" (absent otherwise)
    pub bomb: Option<String>,
    pub win_team: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Player {
    /// SteamID64 of the player this block describes (the observed player
    /// when spectating!).
    pub steamid: Option<String>,
    pub name: Option<String>,
    pub state: Option<PlayerState>,
    pub match_stats: Option<MatchStats>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlayerState {
    pub health: Option<i32>,
    pub armor: Option<i32>,
    pub flashed: Option<i32>,
    pub smoked: Option<i32>,
    pub burning: Option<i32>,
    pub round_kills: Option<u32>,
    #[serde(rename = "round_killhs")]
    pub round_kill_headshots: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MatchStats {
    pub kills: Option<i32>,
    pub assists: Option<i32>,
    pub deaths: Option<i32>,
    pub mvps: Option<i32>,
    pub score: Option<i32>,
}
