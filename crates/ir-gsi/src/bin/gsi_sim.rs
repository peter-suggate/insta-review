//! Replays a canned CS2 deathmatch sequence against a GSI listener with
//! realistic pacing — full GSI development without launching CS2 (or
//! owning a Windows box).
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

fn payload(
    token: &str,
    round_phase: &str,
    bomb: Option<&str>,
    health: i32,
    kills: i32,
    killhs: u32,
    deaths: i32,
) -> String {
    let bomb = bomb.map_or(String::new(), |b| format!(r#""bomb": "{b}","#));
    format!(
        r#"{{
  "auth": {{"token": "{token}"}},
  "provider": {{"steamid": "76561198000000001", "timestamp": 0}},
  "map": {{"name": "de_mirage", "phase": "live"}},
  "round": {{{bomb} "phase": "{round_phase}"}},
  "player": {{
    "steamid": "76561198000000001",
    "name": "sim-player",
    "state": {{"health": {health}, "armor": 100, "round_kills": {rk}, "round_killhs": {killhs}}},
    "match_stats": {{"kills": {kills}, "assists": 0, "deaths": {deaths}, "mvps": 0, "score": 0}}
  }}
}}"#,
        rk = kills.max(0)
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:3585".into());
    let token = args.next().unwrap_or_else(|| "dev".into());

    // (delay before sending [ms], phase, bomb, health, kills, killhs, deaths)
    type Step = (u64, &'static str, Option<&'static str>, i32, i32, u32, i32);
    let script: &[Step] = &[
        (0, "freezetime", None, 100, 0, 0, 0),
        (800, "live", None, 100, 0, 0, 0),
        (1500, "live", None, 100, 1, 0, 0), // kill
        (1200, "live", None, 73, 1, 0, 0),  // took 27 damage
        (900, "live", None, 73, 2, 1, 0),   // headshot kill
        (1500, "live", None, 0, 2, 1, 1),   // died
        (1000, "live", Some("planted"), 100, 2, 1, 1),
        (2000, "live", Some("exploded"), 100, 2, 1, 1),
        (700, "over", Some("exploded"), 100, 2, 1, 1),
        (2000, "freezetime", None, 100, 2, 1, 1),
    ];

    println!(
        "gsi-sim → {addr} (token {token:?}), {} events",
        script.len()
    );
    for (delay, phase, bomb, health, kills, killhs, deaths) in script {
        std::thread::sleep(std::time::Duration::from_millis(*delay));
        let body = payload(&token, phase, *bomb, *health, *kills, *killhs, *deaths);
        match post(&addr, &body) {
            Ok(()) => {
                println!("sent: phase={phase} bomb={bomb:?} hp={health} k={kills} d={deaths}")
            }
            Err(e) => eprintln!("post failed: {e}"),
        }
    }
    println!("done");
}
