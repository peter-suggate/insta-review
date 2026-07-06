//! Stateful differ: consecutive GSI payloads → timeline markers.
//! Plus the stateless per-payload state sample (weapon/ammo/health/flash)
//! the analysis layer consumes as a continuous trace.
//!
//! We diff against our own previous state rather than trusting the
//! payload's `previously` block — simpler and robust to missed posts.

use ir_types::{GsiState, MarkerKind};

use crate::model::GsiPayload;

/// Is this payload about the local player? When spectating, `player` is
/// whoever is being observed. Missing ids: assume local, don't drop data.
fn is_local(payload: &GsiPayload) -> bool {
    match (
        payload.provider.as_ref().and_then(|p| p.steamid.as_ref()),
        payload.player.as_ref().and_then(|p| p.steamid.as_ref()),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// Parse a GSI vector string (`"-1024.00, 512.50, 64.00"`) into `[x, y, z]`.
/// Anything malformed is None — position data is an enhancement, never a
/// reason to drop a sample.
fn parse_vec3(s: &str) -> Option<[f64; 3]> {
    let mut parts = s.split(',').map(|p| p.trim().parse::<f64>());
    let v = [parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?];
    parts.next().is_none().then_some(v)
}

/// Instantaneous state sample from one payload, or None when the payload
/// has no local-player block (menus, spectating).
pub fn sample(payload: &GsiPayload) -> Option<GsiState> {
    if !is_local(payload) {
        return None;
    }
    let player = payload.player.as_ref()?;
    let active = player
        .weapons
        .as_ref()
        .and_then(|ws| ws.values().find(|w| w.state.as_deref() == Some("active")));
    let state = player.state.as_ref();
    Some(GsiState {
        weapon: active
            .and_then(|w| w.name.clone())
            .unwrap_or_default(),
        ammo_clip: active
            .and_then(|w| w.ammo_clip)
            .and_then(|a| u32::try_from(a).ok()),
        health: state
            .and_then(|s| s.health)
            .and_then(|h| u32::try_from(h).ok()),
        flashed: state
            .and_then(|s| s.flashed)
            .map_or(0, |f| f.clamp(0, 255) as u8),
        smoked: state
            .and_then(|s| s.smoked)
            .map_or(0, |f| f.clamp(0, 255) as u8),
        position: player.position.as_deref().and_then(parse_vec3),
        forward: player.forward.as_deref().and_then(parse_vec3),
    })
}

#[derive(Debug, Default)]
pub struct Differ {
    prev: Option<GsiPayload>,
}

impl Differ {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next payload; returns markers derived from the transition.
    pub fn push(&mut self, next: &GsiPayload) -> Vec<MarkerKind> {
        let mut out = Vec::new();
        let Some(prev) = self.prev.replace(next.clone()) else {
            return out; // first payload: baseline only
        };

        // Only derive player markers when the payload describes the local
        // player (when spectating, `player` is whoever is being observed).
        let local = is_local(next);

        if local {
            let prev_player = prev.player.as_ref();
            let next_player = next.player.as_ref();

            // Kills: cumulative match_stats.kills increase.
            let kills = |p: Option<&crate::model::Player>| {
                p.and_then(|p| p.match_stats.as_ref())
                    .and_then(|m| m.kills)
                    .unwrap_or(0)
            };
            let kill_delta = kills(next_player) - kills(prev_player);
            if kill_delta > 0 {
                let hs = |p: Option<&crate::model::Player>| {
                    p.and_then(|p| p.state.as_ref())
                        .and_then(|s| s.round_kill_headshots)
                        .unwrap_or(0)
                };
                out.push(MarkerKind::Kill {
                    count: kill_delta as u32,
                    headshot: hs(next_player) > hs(prev_player),
                });
            }

            // Deaths.
            let deaths = |p: Option<&crate::model::Player>| {
                p.and_then(|p| p.match_stats.as_ref())
                    .and_then(|m| m.deaths)
                    .unwrap_or(0)
            };
            if deaths(next_player) > deaths(prev_player) {
                out.push(MarkerKind::Death);
            }

            // Damage taken: health decrease while both payloads have health
            // (a respawn/round-reset increase is not damage).
            let health = |p: Option<&crate::model::Player>| {
                p.and_then(|p| p.state.as_ref()).and_then(|s| s.health)
            };
            if let (Some(before), Some(after)) = (health(prev_player), health(next_player)) {
                if after < before && after > 0 {
                    out.push(MarkerKind::DamageTaken {
                        amount: (before - after) as u32,
                    });
                }
            }
        }

        // Round phase transitions.
        let phase = |p: &GsiPayload| p.round.as_ref().and_then(|r| r.phase.clone());
        if let Some(next_phase) = phase(next) {
            if phase(&prev).as_deref() != Some(next_phase.as_str()) {
                out.push(MarkerKind::RoundPhase { phase: next_phase });
            }
        }

        // Bomb events.
        let bomb = |p: &GsiPayload| p.round.as_ref().and_then(|r| r.bomb.clone());
        if let Some(next_bomb) = bomb(next) {
            if bomb(&prev).as_deref() != Some(next_bomb.as_str()) {
                out.push(MarkerKind::Bomb { event: next_bomb });
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(json: serde_json::Value) -> GsiPayload {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn derives_kill_death_damage_and_phases() {
        let mut differ = Differ::new();

        let base = payload(serde_json::json!({
            "provider": {"steamid": "7656"},
            "round": {"phase": "freezetime"},
            "player": {
                "steamid": "7656",
                "state": {"health": 100, "round_kills": 0, "round_killhs": 0},
                "match_stats": {"kills": 4, "deaths": 2}
            }
        }));
        assert!(differ.push(&base).is_empty(), "first payload is baseline");

        // Round goes live.
        let live = payload(serde_json::json!({
            "provider": {"steamid": "7656"},
            "round": {"phase": "live"},
            "player": {
                "steamid": "7656",
                "state": {"health": 100, "round_kills": 0, "round_killhs": 0},
                "match_stats": {"kills": 4, "deaths": 2}
            }
        }));
        assert_eq!(
            differ.push(&live),
            vec![MarkerKind::RoundPhase {
                phase: "live".into()
            }]
        );

        // Headshot kill + took 27 damage in the same heartbeat.
        let kill = payload(serde_json::json!({
            "provider": {"steamid": "7656"},
            "round": {"phase": "live"},
            "player": {
                "steamid": "7656",
                "state": {"health": 73, "round_kills": 1, "round_killhs": 1},
                "match_stats": {"kills": 5, "deaths": 2}
            }
        }));
        let markers = differ.push(&kill);
        assert!(markers.contains(&MarkerKind::Kill {
            count: 1,
            headshot: true
        }));
        assert!(markers.contains(&MarkerKind::DamageTaken { amount: 27 }));

        // Death: health hits 0 and deaths increments.
        let death = payload(serde_json::json!({
            "provider": {"steamid": "7656"},
            "round": {"phase": "live"},
            "player": {
                "steamid": "7656",
                "state": {"health": 0, "round_kills": 1, "round_killhs": 1},
                "match_stats": {"kills": 5, "deaths": 3}
            }
        }));
        let markers = differ.push(&death);
        assert_eq!(markers, vec![MarkerKind::Death]);

        // New round: health back to 100 must NOT be damage; phase changes.
        let reset = payload(serde_json::json!({
            "provider": {"steamid": "7656"},
            "round": {"phase": "freezetime"},
            "player": {
                "steamid": "7656",
                "state": {"health": 100, "round_kills": 0, "round_killhs": 0},
                "match_stats": {"kills": 5, "deaths": 3}
            }
        }));
        assert_eq!(
            differ.push(&reset),
            vec![MarkerKind::RoundPhase {
                phase: "freezetime".into()
            }]
        );
    }

    #[test]
    fn spectated_player_events_are_ignored() {
        let mut differ = Differ::new();
        differ.push(&payload(serde_json::json!({
            "provider": {"steamid": "ME"},
            "player": {"steamid": "SOMEONE_ELSE", "match_stats": {"kills": 0}}
        })));
        let markers = differ.push(&payload(serde_json::json!({
            "provider": {"steamid": "ME"},
            "player": {"steamid": "SOMEONE_ELSE", "match_stats": {"kills": 3}}
        })));
        assert!(markers.is_empty());
    }

    #[test]
    fn sample_extracts_active_weapon_and_state() {
        let p = payload(serde_json::json!({
            "provider": {"steamid": "7656"},
            "player": {
                "steamid": "7656",
                "state": {"health": 73, "flashed": 120, "smoked": 0},
                "position": "-1024.50, 512.00, 64.03",
                "forward": "0.71, -0.71, 0.00",
                "weapons": {
                    "weapon_0": {"name": "weapon_knife", "state": "holstered"},
                    "weapon_1": {"name": "weapon_ak47", "state": "active",
                                  "ammo_clip": 17, "ammo_clip_max": 30, "type": "Rifle"}
                }
            }
        }));
        let s = sample(&p).unwrap();
        assert_eq!(s.weapon, "weapon_ak47");
        assert_eq!(s.ammo_clip, Some(17));
        assert_eq!(s.health, Some(73));
        assert_eq!(s.flashed, 120);
        assert_eq!(s.position, Some([-1024.5, 512.0, 64.03]));
        assert_eq!(s.forward, Some([0.71, -0.71, 0.0]));
    }

    #[test]
    fn malformed_vectors_are_none_not_fatal() {
        assert_eq!(parse_vec3("1.0, 2.0, 3.0"), Some([1.0, 2.0, 3.0]));
        assert_eq!(parse_vec3("1.0, 2.0"), None);
        assert_eq!(parse_vec3("1.0, 2.0, 3.0, 4.0"), None);
        assert_eq!(parse_vec3("a, b, c"), None);
        let p = payload(serde_json::json!({
            "provider": {"steamid": "X"},
            "player": {"steamid": "X", "state": {"health": 100}, "position": "garbage"}
        }));
        let s = sample(&p).unwrap();
        assert_eq!(s.position, None);
        assert_eq!(s.health, Some(100));
    }

    #[test]
    fn sample_ignores_spectated_player() {
        let p = payload(serde_json::json!({
            "provider": {"steamid": "ME"},
            "player": {"steamid": "OTHER", "state": {"health": 50}}
        }));
        assert!(sample(&p).is_none());
    }

    #[test]
    fn bomb_events() {
        let mut differ = Differ::new();
        differ.push(&payload(serde_json::json!({"round": {"phase": "live"}})));
        let markers = differ.push(&payload(
            serde_json::json!({"round": {"phase": "live", "bomb": "planted"}}),
        ));
        assert_eq!(
            markers,
            vec![MarkerKind::Bomb {
                event: "planted".into()
            }]
        );
    }
}
