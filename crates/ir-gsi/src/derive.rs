//! Stateful differ: consecutive GSI payloads → timeline markers.
//!
//! We diff against our own previous state rather than trusting the
//! payload's `previously` block — simpler and robust to missed posts.

use ir_types::MarkerKind;

use crate::model::GsiPayload;

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
        let local = match (
            next.provider.as_ref().and_then(|p| p.steamid.as_ref()),
            next.player.as_ref().and_then(|p| p.steamid.as_ref()),
        ) {
            (Some(a), Some(b)) => a == b,
            _ => true, // missing ids: assume local rather than dropping data
        };

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
