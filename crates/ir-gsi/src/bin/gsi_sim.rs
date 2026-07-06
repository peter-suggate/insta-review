//! Replays a canned CS2 deathmatch sequence against a GSI listener with
//! realistic pacing — full GSI development without launching CS2 (or
//! owning a Windows box).
//!
//! Posts every ~100 ms (the real cfg throttle), interpolating position and
//! view direction between scripted beats so the position-derived speed and
//! view traces look like actual play: runs, counter-strafe stops, a flick,
//! shooting on the move, and a respawn teleport.
//!
//! Usage: gsi-sim [host:port] [token]

use std::io::{Read, Write};
use std::net::TcpStream;

fn post(addr: &str, body: &str) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(addr)?;
    write!(
        stream,
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    Ok(())
}

struct World {
    phase: &'static str,
    bomb: Option<&'static str>,
    health: i32,
    kills: i32,
    killhs: u32,
    deaths: i32,
    ammo: i32,
    x: f64,
    y: f64,
    yaw_deg: f64,
}

fn payload(token: &str, w: &World) -> String {
    let bomb = w
        .bomb
        .map_or(String::new(), |b| format!(r#""bomb": "{b}","#));
    let (fx, fy) = (w.yaw_deg.to_radians().cos(), w.yaw_deg.to_radians().sin());
    format!(
        r#"{{
  "auth": {{"token": "{token}"}},
  "provider": {{"steamid": "76561198000000001", "timestamp": 0}},
  "map": {{"name": "de_mirage", "phase": "live"}},
  "round": {{{bomb} "phase": "{phase}"}},
  "player": {{
    "steamid": "76561198000000001",
    "name": "sim-player",
    "state": {{"health": {health}, "armor": 100, "round_kills": {rk}, "round_killhs": {killhs}}},
    "match_stats": {{"kills": {kills}, "assists": 0, "deaths": {deaths}, "mvps": 0, "score": 0}},
    "position": "{px:.2}, {py:.2}, 64.00",
    "forward": "{fx:.4}, {fy:.4}, 0.0000",
    "weapons": {{
      "weapon_0": {{"name": "weapon_knife", "state": "holstered", "type": "Knife"}},
      "weapon_1": {{"name": "weapon_ak47", "state": "active",
                    "ammo_clip": {ammo}, "ammo_clip_max": 30, "type": "Rifle"}}
    }}
  }}
}}"#,
        phase = w.phase,
        health = w.health,
        rk = w.kills.max(0),
        killhs = w.killhs,
        kills = w.kills,
        deaths = w.deaths,
        px = w.x,
        py = w.y,
        fx = fx,
        fy = fy,
        ammo = w.ammo,
    )
}

/// One scripted beat: state that becomes true at its start, held for
/// `hold_ms` while the player moves at `ups` and turns toward `yaw_to`.
#[allow(clippy::too_many_arguments)]
struct Beat {
    hold_ms: u64,
    phase: &'static str,
    bomb: Option<&'static str>,
    health: i32,
    kills: i32,
    killhs: u32,
    deaths: i32,
    ammo: i32,
    ups: f64,
    yaw_to: f64,
    /// Position discontinuity at beat start (respawn teleport).
    teleport: bool,
    label: &'static str,
}

#[allow(clippy::too_many_arguments)]
fn beat(
    hold_ms: u64,
    phase: &'static str,
    bomb: Option<&'static str>,
    health: i32,
    kills: i32,
    killhs: u32,
    deaths: i32,
    ammo: i32,
    ups: f64,
    yaw_to: f64,
    teleport: bool,
    label: &'static str,
) -> Beat {
    Beat {
        hold_ms,
        phase,
        bomb,
        health,
        kills,
        killhs,
        deaths,
        ammo,
        ups,
        yaw_to,
        teleport,
        label,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:3585".into());
    let token = args.next().unwrap_or_else(|| "dev".into());

    let script = [
        beat(800, "freezetime", None, 100, 0, 0, 0, 30, 0.0, 0.0, false, "freeze"),
        beat(1200, "live", None, 100, 0, 0, 0, 30, 250.0, 5.0, false, "running out"),
        beat(200, "live", None, 100, 0, 0, 0, 30, 0.0, 5.0, false, "stops (counter-strafe)"),
        beat(300, "live", None, 100, 0, 0, 0, 27, 0.0, 5.0, false, "3-shot burst, planted"),
        beat(500, "live", None, 100, 1, 0, 0, 24, 0.0, 8.0, false, "kill burst, still planted"),
        beat(900, "live", None, 73, 1, 0, 0, 24, 250.0, 10.0, false, "strafing, takes 27 dmg"),
        beat(500, "live", None, 73, 1, 0, 0, 18, 180.0, 12.0, false, "sprays WHILE MOVING"),
        beat(200, "live", None, 73, 1, 0, 0, 18, 0.0, 72.0, false, "flick 60° right"),
        beat(400, "live", None, 73, 2, 1, 0, 15, 0.0, 70.0, false, "headshot kill, settled"),
        beat(1200, "live", None, 0, 2, 1, 1, 15, 0.0, 70.0, false, "died"),
        beat(900, "live", None, 100, 2, 1, 1, 30, 0.0, 0.0, true, "respawn (teleport)"),
        beat(1000, "live", Some("planted"), 100, 2, 1, 1, 30, 100.0, 0.0, false, "walking"),
        beat(1500, "live", Some("exploded"), 100, 2, 1, 1, 30, 0.0, 0.0, false, "boom"),
        beat(700, "over", Some("exploded"), 100, 2, 1, 1, 30, 0.0, 0.0, false, "round over"),
        beat(1000, "freezetime", None, 100, 2, 1, 1, 30, 0.0, 0.0, false, "next round"),
    ];

    println!("gsi-sim → {addr} (token {token:?}), {} beats @10 Hz", script.len());
    let mut w = World {
        phase: "freezetime",
        bomb: None,
        health: 100,
        kills: 0,
        killhs: 0,
        deaths: 0,
        ammo: 30,
        x: 0.0,
        y: 0.0,
        yaw_deg: 0.0,
    };
    const TICK_MS: u64 = 100;
    for b in &script {
        w.phase = b.phase;
        w.bomb = b.bomb;
        w.health = b.health;
        w.kills = b.kills;
        w.killhs = b.killhs;
        w.deaths = b.deaths;
        w.ammo = b.ammo;
        if b.teleport {
            w.x += 1800.0;
            w.y -= 900.0;
        }
        let ticks = (b.hold_ms / TICK_MS).max(1);
        let yaw_step = (b.yaw_to - w.yaw_deg) / ticks as f64;
        println!("beat: {} ({} ticks, {:.0} u/s)", b.label, ticks, b.ups);
        for _ in 0..ticks {
            std::thread::sleep(std::time::Duration::from_millis(TICK_MS));
            w.x += b.ups * (TICK_MS as f64 / 1000.0);
            w.yaw_deg += yaw_step;
            if let Err(e) = post(&addr, &payload(&token, &w)) {
                eprintln!("post failed: {e}");
            }
        }
    }
    println!("done");
}
