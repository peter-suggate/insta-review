//! CS2 demo (.dem) enrichment: extract tick-exact `weapon_fire` /
//! `player_death` truth and align it onto a captured clip's timeline.
//!
//! Layering, deliberately: the demo *parse* (`extract_events`) is a thin
//! shell over `source2-demo` that only collects raw named events — the
//! hard-to-get-wrong part. Everything with logic in it — inferring which
//! player slot is the local player, aligning demo time onto clip time,
//! producing shot events — is pure functions over those raw events, fully
//! unit-tested without a demo file. The parse shell itself gets validated
//! against a real match demo on the Windows box.
//!
//! Clip↔demo alignment has no shared clock, so it's solved from the events
//! themselves: the clip's GSI kill/death markers must line up with some
//! slot's `player_death` involvement at some constant offset. The (slot,
//! offset) pair that matches best identifies the local player *and* the
//! time mapping in one step.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// CS2 tick interval (64-tick demos). Sub-tick timing exists in Source 2
/// but tick resolution (15.6 ms) already beats every other shot-time
/// source we have.
pub const TICK_INTERVAL_S: f64 = 1.0 / 64.0;

#[derive(Debug, thiserror::Error)]
pub enum DemoError {
    #[error("could not open demo: {0}")]
    Io(#[from] std::io::Error),
    #[error("demo parse failed: {0}")]
    Parse(String),
}

/// One raw game event, minimally interpreted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEvent {
    pub tick: u32,
    pub name: String,
    /// Key-value pairs as (name, stringified value) — enough for our
    /// integer/string fields without dragging the event type system along.
    pub keys: Vec<(String, String)>,
}

impl RawEvent {
    pub fn key(&self, name: &str) -> Option<&str> {
        self.keys
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn key_i64(&self, name: &str) -> Option<i64> {
        self.key(name)?.parse().ok()
    }

    pub fn time_s(&self) -> f64 {
        self.tick as f64 * TICK_INTERVAL_S
    }
}

/// Parse the demo and collect events whose names are in `names`.
/// Thin shell — no interpretation here. NOTE: validated against real CS2
/// match demos on Windows; not exercisable in CI (no demo fixture).
pub fn extract_events(demo_path: &Path, names: &[&str]) -> Result<Vec<RawEvent>, DemoError> {
    use source2_demo::prelude::*;

    #[derive(Default)]
    struct Collector {
        wanted: Vec<String>,
        out: Vec<RawEvent>,
    }

    #[observer]
    #[uses_game_events]
    impl Collector {
        #[on_game_event]
        fn collect(&mut self, ctx: &Context, ge: &GameEvent) -> ObserverResult {
            if self.wanted.iter().any(|w| w == ge.name()) {
                self.out.push(RawEvent {
                    tick: ctx.tick(),
                    name: ge.name().to_string(),
                    keys: ge
                        .iter()
                        .map(|(k, v)| (k.to_string(), format!("{v:?}")))
                        .collect(),
                });
            }
            Ok(())
        }
    }

    let file = std::fs::File::open(demo_path)?;
    let reader = std::io::BufReader::new(file);
    let mut parser =
        Parser::from_reader(reader).map_err(|e| DemoError::Parse(e.to_string()))?;
    let collector = parser.register_observer::<Collector>();
    collector.borrow_mut().wanted = names.iter().map(|s| s.to_string()).collect();
    parser
        .run_to_end()
        .map_err(|e| DemoError::Parse(e.to_string()))?;
    let out = std::mem::take(&mut collector.borrow_mut().out);
    Ok(out)
}

/// Strip the `EventValue` debug wrapper (`Int(5)`, `String("ak47")`) that
/// `extract_events` stringifies through. Pure so it's testable.
pub fn unwrap_value(v: &str) -> String {
    let inner = v
        .strip_prefix("Int(")
        .or_else(|| v.strip_prefix("Float("))
        .or_else(|| v.strip_prefix("U64("))
        .or_else(|| v.strip_prefix("Byte("))
        .or_else(|| v.strip_prefix("Bool("))
        .or_else(|| v.strip_prefix("String("))
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(v);
    inner.trim_matches('"').to_string()
}

/// The result of matching a demo player slot's kill/death events against
/// the clip's markers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Alignment {
    /// The local player's userid/slot in the demo.
    pub slot: i64,
    /// demo_time − offset = clip_time.
    pub offset_s: f64,
    /// How many clip markers found a demo event within tolerance.
    pub matched: usize,
    /// Total clip markers considered.
    pub total: usize,
}

/// Best constant offset mapping `demo_times` onto `clip_times`:
/// try every pairwise offset, count matches within `tol_s`, refine the
/// winner as the mean of matched pairs. None if nothing matches.
pub fn align_times(demo_times: &[f64], clip_times: &[f64], tol_s: f64) -> Option<(f64, usize)> {
    let mut best: Option<(f64, usize)> = None;
    for &d in demo_times {
        for &c in clip_times {
            let candidate = d - c;
            let matched: Vec<f64> = clip_times
                .iter()
                .filter_map(|&ct| {
                    demo_times
                        .iter()
                        .map(|&dt| dt - candidate - ct)
                        .find(|delta| delta.abs() <= tol_s)
                })
                .collect();
            let count = matched.len();
            if count > best.map_or(0, |(_, c)| c) {
                let refined =
                    candidate + matched.iter().sum::<f64>() / matched.len().max(1) as f64;
                best = Some((refined, count));
            }
        }
    }
    best
}

/// Infer the local player's demo slot + time offset by aligning each
/// slot's `player_death` involvement (as victim for clip Death markers,
/// as attacker for clip Kill markers) against the clip's marker times.
///
/// `clip_kills`/`clip_deaths` are clip-relative seconds (GSI markers, so
/// tolerance should absorb their latency: ~0.7 s works).
pub fn infer_alignment(
    deaths: &[RawEvent],
    clip_kills: &[f64],
    clip_deaths: &[f64],
    tol_s: f64,
) -> Option<Alignment> {
    let total = clip_kills.len() + clip_deaths.len();
    if total == 0 {
        return None;
    }
    let mut slots: Vec<i64> = deaths
        .iter()
        .flat_map(|e| [e.key_i64("userid"), e.key_i64("attacker")])
        .flatten()
        .collect();
    slots.sort_unstable();
    slots.dedup();

    let mut best: Option<Alignment> = None;
    for &slot in &slots {
        // Events from this slot's perspective, on one merged timeline —
        // the clip's kill+death markers share one clock, so alignment uses
        // them together for maximum constraint.
        let demo_times: Vec<f64> = deaths
            .iter()
            .filter(|e| e.key_i64("attacker") == Some(slot))
            .map(RawEvent::time_s)
            .collect();
        let demo_deaths: Vec<f64> = deaths
            .iter()
            .filter(|e| e.key_i64("userid") == Some(slot))
            .map(RawEvent::time_s)
            .collect();
        let (offset_kills, matched_kills) = align_times(&demo_times, clip_kills, tol_s)
            .map_or((None, 0), |(o, c)| (Some(o), c));
        let (offset_deaths, matched_deaths) = align_times(&demo_deaths, clip_deaths, tol_s)
            .map_or((None, 0), |(o, c)| (Some(o), c));

        // Offsets from kills and deaths must agree (same clock!). When
        // they don't, the two sides are unrelated coincidences — only the
        // stronger side's matches count as evidence.
        let (offset, matched) = match (offset_kills, offset_deaths) {
            (Some(a), Some(b)) if (a - b).abs() <= tol_s => {
                ((a + b) / 2.0, matched_kills + matched_deaths)
            }
            (Some(a), Some(b)) => {
                if matched_kills >= matched_deaths {
                    (a, matched_kills)
                } else {
                    (b, matched_deaths)
                }
            }
            (Some(a), None) => (a, matched_kills),
            (None, Some(b)) => (b, matched_deaths),
            (None, None) => continue,
        };
        if best.as_ref().is_none_or(|b| matched > b.matched) {
            best = Some(Alignment {
                slot,
                offset_s: offset,
                matched,
                total,
            });
        }
    }
    // A single coincidental match proves nothing; demand either most
    // markers matched or at least two.
    best.filter(|b| b.matched >= 2 || b.matched == b.total)
}

/// One tick-exact shot from the demo, already on the clip's clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DemoShot {
    /// Clip-relative seconds.
    pub t: f64,
    pub weapon: String,
}

/// The enrichment sidecar written next to the clip
/// (`clip_X.demo.json`): everything analysis needs, pre-aligned.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DemoEnrichment {
    pub schema_version: u32,
    pub demo_file: String,
    pub slot: i64,
    pub offset_s: f64,
    pub matched_events: usize,
    pub total_events: usize,
    pub shots: Vec<DemoShot>,
}

pub const ENRICHMENT_VERSION: u32 = 1;

/// The local slot's `weapon_fire` events mapped onto the clip clock,
/// clamped to the clip window.
pub fn shots_on_clip_clock(
    fires: &[RawEvent],
    alignment: &Alignment,
    clip_duration_s: f64,
) -> Vec<DemoShot> {
    let mut shots: Vec<DemoShot> = fires
        .iter()
        .filter(|e| e.key_i64("userid") == Some(alignment.slot))
        .filter_map(|e| {
            let t = e.time_s() - alignment.offset_s;
            (-0.5..=clip_duration_s + 0.5).contains(&t).then(|| DemoShot {
                t: t.max(0.0),
                weapon: e
                    .key("weapon")
                    .map(unwrap_value)
                    .unwrap_or_default(),
            })
        })
        .collect();
    shots.sort_by(|a, b| a.t.total_cmp(&b.t));
    shots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(tick: u32, name: &str, keys: &[(&str, &str)]) -> RawEvent {
        RawEvent {
            tick,
            name: name.into(),
            keys: keys
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// player_death at `t` seconds with victim/attacker slots.
    fn death(t: f64, victim: i64, attacker: i64) -> RawEvent {
        let vict = victim.to_string();
        let atk = attacker.to_string();
        ev(
            (t / TICK_INTERVAL_S) as u32,
            "player_death",
            &[("userid", vict.as_str()), ("attacker", atk.as_str())],
        )
    }

    fn fire(t: f64, slot: i64, weapon: &str) -> RawEvent {
        let s = slot.to_string();
        let w = format!("String(\"{weapon}\")");
        ev(
            (t / TICK_INTERVAL_S) as u32,
            "weapon_fire",
            &[("userid", s.as_str()), ("weapon", w.as_str())],
        )
    }

    #[test]
    fn unwrap_value_strips_debug_wrappers() {
        assert_eq!(unwrap_value("Int(5)"), "5");
        assert_eq!(unwrap_value("String(\"weapon_ak47\")"), "weapon_ak47");
        assert_eq!(unwrap_value("Bool(true)"), "true");
        assert_eq!(unwrap_value("plain"), "plain");
    }

    #[test]
    fn align_times_finds_constant_offset_amid_noise() {
        // Demo events at clip times {2.0, 5.5, 9.1} + offset 1234.5, with
        // GSI-ish jitter, plus unrelated demo events.
        let clip = [2.0, 5.5, 9.1];
        let demo = [100.0, 1236.6, 1240.1, 1243.55, 1300.0];
        let (offset, matched) = align_times(&demo, &clip, 0.7).unwrap();
        assert_eq!(matched, 3);
        assert!((offset - 1234.5).abs() < 0.35, "offset {offset}");
    }

    #[test]
    fn infers_local_slot_from_kills_and_deaths() {
        const OFF: f64 = 987.0;
        // Slot 3 is the local player: kills at clip 2.0/6.0, death at 9.0.
        // Slot 5 is a decoy with its own uncorrelated activity.
        let events = vec![
            death(OFF + 2.05, 7, 3),
            death(OFF + 6.02, 8, 3),
            death(OFF + 9.03, 3, 5),
            death(OFF + 100.0, 5, 8),
            death(OFF + 200.0, 6, 5),
        ];
        let a = infer_alignment(&events, &[2.0, 6.0], &[9.0], 0.7).unwrap();
        assert_eq!(a.slot, 3);
        assert_eq!(a.matched, 3);
        assert!((a.offset_s - OFF).abs() < 0.2, "offset {}", a.offset_s);
    }

    #[test]
    fn one_coincidental_match_is_rejected() {
        let events = vec![death(50.0, 2, 4), death(400.0, 4, 9)];
        assert!(infer_alignment(&events, &[2.0, 6.0], &[9.0], 0.7).is_none());
    }

    #[test]
    fn shots_map_onto_clip_clock_for_the_local_slot_only() {
        let alignment = Alignment {
            slot: 3,
            offset_s: 987.0,
            matched: 3,
            total: 3,
        };
        let fires = vec![
            fire(988.5, 3, "weapon_ak47"),  // clip t = 1.5
            fire(989.0, 5, "weapon_deagle"), // other player: dropped
            fire(996.2, 3, "weapon_ak47"),  // clip t = 9.2
            fire(1100.0, 3, "weapon_ak47"), // outside the clip: dropped
        ];
        let shots = shots_on_clip_clock(&fires, &alignment, 15.0);
        assert_eq!(shots.len(), 2);
        assert!((shots[0].t - 1.5).abs() < 0.02);
        assert!((shots[1].t - 9.2).abs() < 0.02);
        assert_eq!(shots[0].weapon, "weapon_ak47");
    }
}
