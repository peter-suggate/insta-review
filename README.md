# insta-review

Instant gameplay review for CS2. Continuously captures the screen into a
small in-RAM replay buffer (hardware-encoded H.264); a global hotkey saves —
and soon, instantly opens for frame-by-frame review — the last 10–15 seconds.
Built to have near-zero impact on a CPU-bound game: capture is
Windows.Graphics.Capture (no injection, VAC-safe), color conversion runs on
the GPU, and encoding happens on the GPU's dedicated encoder (NVENC/AMF/QSV
via Media Foundation).

CS2 kill/death/round markers come from Valve's official Game State
Integration (a config file; the game POSTs JSON to a local port).

## Status

- **M0 done** — engine core: GOP-structured encoded ring buffer, in-memory
  MP4 muxer, synthetic test pipeline with burned-in frame counters
  (ground truth for frame accuracy), headless CLI.
- **M2 in test** — Windows pipeline (WGC → D3D11 VideoProcessor → H.264 MFT),
  global hotkey, GSI markers. Compiles clean for `x86_64-pc-windows-msvc`;
  needs first run on a real Windows box (below).
- **M3+M4 done** — the desktop app (`apps/desktop`): warm hidden review
  window, hotkey → frame-accurate WebCodecs player in <100 ms, timeline
  with GSI kill/death/round/bomb markers, 4:3-stretch aspect override +
  crosshair zoom, settings drawer, one-click GSI config installer.
  Verified end-to-end on macOS with the test pipeline (automated self-test
  reads the burned-in frame counters back from decoded pixels: stepping
  advances exactly 1 frame, backstep exact, ~25 ms/step).
- Post-v1: audio track (WASAPI loopback — needs the Windows box), pipeline
  auto-restart on display-mode change, packaging/installer polish.

## The desktop app

```
cargo run -p insta-review --release
```

Starts capturing immediately (Windows: the primary display via WGC;
elsewhere: the synthetic test pattern). Press **Ctrl+Alt+R** (configurable)
at any moment: the review window opens paused ~1.5 s before your keypress.

Player keys: `Space` play/pause · `,` `.` frame step · `J K L` ·
`←/→` ±1 s · `1–5` speed 0.1×–2× · `M` next marker · `Z` crosshair zoom ·
`A` 4:3↔16:9 stretch · `S` save clip (MP4 + marker sidecar → Videos/insta-review) ·
`G` settings · `Esc` back to game (focus restored on Windows).

For CS2 markers: `G` → "Install CS2 GSI config…" (writes one cfg file into
the CS2 cfg dir after confirmation), restart CS2 once.

Dev hooks: `IR_AUTOTRIGGER=8` fires the hotkey path 8 s after launch;
`IR_AUTOTEST=1` runs the frame-accuracy self-test against a test-pattern
clip and logs `SELFTEST …` via the `player` log target.

## Testing on the Windows gaming box

One-time setup:

1. Install [rustup](https://rustup.rs) (pick the default MSVC toolchain).
   If you don't have Visual Studio, let rustup install the C++ build tools,
   or `winget install Microsoft.VisualStudio.2022.BuildTools` with the
   "Desktop development with C++" workload.
2. Clone this repo and `cargo build --release`.
   (`openh264` builds from source for the test pipeline; no NASM needed —
   it falls back to plain C.)

### Test 1 — capture works at all (spike S1+S2)

With CS2 running (borderless fullscreen windowed), from another terminal:

```
target\release\ir-cli.exe record --duration 20 --window 10 -o cs2.mp4
```

Expected: `cs2.mp4` shows the last 10 s of gameplay, smooth, correct colors
(not washed out), no yellow capture border (Win11). The log line
`windows pipeline: first frame` confirms WGC + MFT initialized.

### Test 2 — the real loop: hotkey saves while you play (spike S3)

```
target\release\ir-cli.exe snapshot-on-key --window 15 --hotkey ctrl+alt+r --out-dir clips
```

Play; whenever something interesting happens hit **Ctrl+Alt+R** — each press
writes `clips\clip_<ts>_<n>.mp4` + a `.json` sidecar (exact frame pts table,
markers). Key question: does the hotkey fire while CS2 has focus?

### Test 3 — GSI markers

Print the CS2 config and install it (once):

```
target\release\ir-cli.exe snapshot-on-key --print-gsi-cfg --gsi-port 3585 --gsi-token SECRET
```

Save the printed block as
`...\Steam\steamapps\common\Counter-Strike Global Offensive\game\csgo\cfg\gamestate_integration_instareview.cfg`
(plain ASCII, no UTF-8 BOM), restart CS2, then run:

```
target\release\ir-cli.exe snapshot-on-key --window 15 --gsi-port 3585 --gsi-token SECRET --out-dir clips
```

Get a kill in deathmatch, hit the hotkey: the sidecar `.json` should contain
`kill` / `death` / `damage_taken` / round-phase markers with clip-relative
timestamps.

### Test 4 — overhead

Run a fixed 5-minute deathmatch workload twice (with and without
`snapshot-on-key` running) under
[PresentMon](https://github.com/GameTechDev/PresentMon) or CapFrameX and
compare average FPS and 1% lows. Target: < ~3% impact. Also check Task
Manager: `ir-cli` should sit well under one core, with activity visible on
the GPU's *Video Encode* engine, not 3D.

### If something fails

- `no hardware H.264 encoder MFT found` → driver issue; check NVIDIA driver
  is current.
- Hotkey doesn't fire in-game → note whether it works on the desktop; we
  have a `WH_KEYBOARD_LL` fallback planned.
- Washed-out or tinted colors → capture a screenshot of the color bars via
  `--pipeline test` for comparison; color-space fix goes in the converter.
- Anything else: run with `RUST_LOG=debug` and save the output.

## Development on macOS (no CS2 needed)

Everything except the Windows capture crates runs here:

```
cargo test                                  # 25 tests incl. decode-back round trip
cargo run -p ir-cli -- record --pipeline test --duration 20 --window 10 -o out.mp4
cargo run -p ir-cli -- snapshot-on-key --pipeline test --gsi-port 3585 --gsi-token dev
cargo run -p ir-gsi --bin gsi-sim           # replays a canned CS2 deathmatch at the listener
cargo check --workspace --target x86_64-pc-windows-msvc   # cross-check Windows code
```

(`snapshot-on-key` uses Enter as the trigger on non-Windows.)

## Layout

```
crates/ir-types           shared data types (packets, markers, clip meta)
crates/ir-core            clock, replay ring, engine loop, snapshotting
crates/ir-mux             in-memory moov-first MP4 muxer + H.264 bitstream utils
crates/ir-pipeline-test   synthetic frames w/ burned-in counters (openh264)
crates/ir-pipeline-win    WGC → D3D11 VideoProcessor → H.264 MFT (Windows)
crates/ir-winutil         global hotkey, window management (Windows)
crates/ir-gsi             CS2 Game State Integration listener + simulator
crates/ir-cli             headless harness (record / snapshot-on-key)
```
