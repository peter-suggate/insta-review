import { Player } from "./player.js";
import { Timeline } from "./timeline.js";

const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const RATES = [0.1, 0.25, 0.5, 1, 2];

let clipMeta = null;
let gsiOffsetUs = 0;

const timeline = new Timeline($("timeline"), {
  onSeek: (us) => {
    player.pause();
    player.seekToUs(us).catch(console.error);
  },
});

const player = new Player($("video"), {
  onPresent: (idx, us) => {
    timeline.setPlayhead(us);
    $("hud-time").textContent = `${(us / 1e6).toFixed(2)} / ${(
      player.durationUs() / 1e6
    ).toFixed(2)}`;
    $("hud-frame").textContent = `f ${idx}`;
  },
  onStateChange: () => {
    $("hud-rate").textContent = `${player.rate}× ${player.playing ? "▶" : "⏸"}`;
    const flags = [];
    if (player.zoom) flags.push("zoom");
    if (player.stretch) flags.push("4:3→16:9");
    $("hud-flags").textContent = flags.join(" · ");
  },
});

function toast(msg, ms = 2500) {
  const el = $("toast");
  el.textContent = msg;
  el.classList.remove("hidden");
  clearTimeout(el._t);
  el._t = setTimeout(() => el.classList.add("hidden"), ms);
}

async function loadClip(payload) {
  clipMeta = payload;
  gsiOffsetUs = payload.gsiOffset * 1e6;
  $("waiting").classList.add("hidden");
  $("hud").classList.remove("hidden");

  const url = convertFileSrc(`clip/${payload.id}/samples`, "replay");
  const response = await fetch(url);
  if (!response.ok) throw new Error(`fetch samples: ${response.status}`);
  const buffer = await response.arrayBuffer();

  await player.load({
    codec: payload.codec,
    samples: payload.samples,
    buffer,
    stretch43: payload.stretch43,
  });

  timeline.load({
    durationUs: player.durationUs(),
    markers: payload.meta.markers,
    keyframesUs: payload.meta.keyframe_indices.map(
      (i) => payload.samples[i].tUs
    ),
    triggerUs: payload.meta.trigger_at * 1e6,
    gsiOffsetUs,
  });

  // Open paused, rewound before the trigger (the key is pressed just
  // after the moment of interest).
  const openUs = Math.max(
    0,
    (payload.meta.trigger_at - payload.openRewind) * 1e6
  );
  await player.seekToUs(openUs);
  player.onStateChange();
  toast(`clip loaded — ${payload.samples.length} frames`);
  const decode = player.lastDecodeStats || {};
  invoke("player_status", {
    status:
      `loaded clip ${payload.id}: ${payload.samples.length} samples, ` +
      `${(buffer.byteLength / 1048576).toFixed(1)} MiB blob, ` +
      `codec ${payload.codec.codecString}, opened paused at frame ${player.cur} ` +
      `(${(player.playheadUs / 1e6).toFixed(2)}s), ${payload.meta.markers.length} markers, ` +
      `cache ${player.cache.size}, decode outputs ${decode.outputs} unmatched ${decode.unmatched}`,
  }).catch(() => {});
}

// ---- self-test (test pattern clips only; IR_AUTOTEST=1) ---------------
// Reads the burned-in frame counter back from decoded pixels — the same
// ground truth the Rust round-trip test uses. Mirrors pattern.rs layout.

const TEST_FONT = [
  0b111_101_101_101_111, 0b010_110_010_010_111, 0b111_001_111_100_111,
  0b111_001_111_001_111, 0b101_101_111_001_001, 0b111_100_111_001_111,
  0b111_100_111_101_111, 0b111_001_010_010_010, 0b111_101_111_101_111,
  0b111_101_111_001_111,
];

function readCounter(frame) {
  if (!frame) return null;
  const w = frame.displayWidth,
    h = frame.displayHeight;
  const off = new OffscreenCanvas(w, h);
  const ctx = off.getContext("2d");
  ctx.drawImage(frame, 0, 0);
  const img = ctx.getImageData(0, 0, w, h).data;
  const barsH = Math.floor((h * 15) / 100);
  const scale = Math.min(8, Math.max(2, Math.floor(w / 160)));
  let value = 0;
  for (let d = 0; d < 7; d++) {
    const cx = 8 + d * 4 * scale;
    let pattern = 0;
    for (let gr = 0; gr < 5; gr++) {
      for (let gc = 0; gc < 3; gc++) {
        const px = cx + gc * scale + (scale >> 1);
        const py = barsH + 8 + gr * scale + (scale >> 1);
        if (img[(py * w + px) * 4] > 125) pattern |= 1 << (14 - (gr * 3 + gc));
      }
    }
    const digit = TEST_FONT.indexOf(pattern);
    if (digit < 0) return null;
    value = value * 10 + digit;
  }
  return value;
}

async function selfTest() {
  const report = (s) => invoke("player_status", { status: s }).catch(() => {});
  try {
    const c0 = readCounter(player.currentFrame());
    const t0 = performance.now();
    await player.step(1);
    const c1 = readCounter(player.currentFrame());
    await player.step(1);
    const c2 = readCounter(player.currentFrame());
    const stepMs = (performance.now() - t0) / 2;
    await player.step(-1);
    const c3 = readCounter(player.currentFrame());
    const fwdOk = c1 === c0 + 1 && c2 === c0 + 2;
    const backOk = c3 === c0 + 1;

    const before = player.cur;
    player.setRate(1);
    player.play();
    await new Promise((r) => setTimeout(r, 1200));
    player.pause();
    const played = player.cur - before;

    report(
      `SELFTEST counters ${c0}→${c1}→${c2}, back→${c3}: ` +
        `fwd ${fwdOk ? "OK" : "FAIL"}, back ${backOk ? "OK" : "FAIL"}, ` +
        `step ${stepMs.toFixed(1)} ms avg, played ${played} frames in 1.2 s`
    );
  } catch (e) {
    report(`SELFTEST ERROR: ${e.message || e}`);
  }
}

listen("clip-ready", (event) => {
  loadClip(event.payload)
    .then(() => {
      if (event.payload.autotest) return selfTest();
    })
    .catch((e) => {
    console.error(e);
    toast(`failed to load clip: ${e.message || e}`, 6000);
    invoke("player_status", {
      status: `ERROR loading clip: ${e.message || e}`,
    }).catch(() => {});
  });
});

// ---- keyboard ----------------------------------------------------------

function rateStep(dir) {
  const i = RATES.findIndex((r) => r >= player.rate);
  const next = Math.max(0, Math.min(RATES.length - 1, (i < 0 ? 3 : i) + dir));
  player.setRate(RATES[next]);
}

document.addEventListener("keydown", (e) => {
  if (!$("settings").classList.contains("hidden")) {
    if (e.key === "Escape") toggleSettings(false);
    return;
  }
  if (!clipMeta && !["g", "G", "Escape"].includes(e.key)) return;

  switch (e.key) {
    case " ":
      e.preventDefault();
      player.toggle();
      break;
    case ",":
      player.step(-1);
      break;
    case ".":
      player.step(1);
      break;
    case "ArrowLeft":
      player.pause();
      player.seekToUs(player.playheadUs - 1e6);
      break;
    case "ArrowRight":
      player.pause();
      player.seekToUs(player.playheadUs + 1e6);
      break;
    case "j":
    case "J":
      rateStep(-1);
      if (!player.playing) player.play();
      break;
    case "k":
    case "K":
      player.toggle();
      break;
    case "l":
    case "L":
      rateStep(1);
      if (!player.playing) player.play();
      break;
    case "1":
    case "2":
    case "3":
    case "4":
    case "5":
      player.setRate(RATES[Number(e.key) - 1]);
      break;
    case "m":
    case "M": {
      const next = timeline.nextMarkerAfter(player.playheadUs);
      if (next != null) {
        player.pause();
        player.seekToUs(next);
      }
      break;
    }
    case "z":
    case "Z":
      player.toggleZoom();
      break;
    case "a":
    case "A":
      player.toggleStretch();
      break;
    case "s":
    case "S":
      invoke("save_clip")
        .then((path) => toast(`saved ${path}`))
        .catch((e) => toast(`save failed: ${e}`, 5000));
      break;
    case "g":
    case "G":
      toggleSettings(true);
      break;
    case "Escape":
      player.pause();
      invoke("close_review");
      break;
  }
});

window.addEventListener("resize", () => {
  player.redraw();
  timeline.draw();
});

// ---- settings drawer ---------------------------------------------------

const FIELDS = [
  ["hotkey", "Hotkey", "text"],
  ["windowSeconds", "Review window (s)", "number"],
  ["fps", "Capture FPS", "number"],
  ["gopSeconds", "GOP (s)", "number"],
  ["quality", "Quality (lower=better)", "number"],
  ["openRewindSeconds", "Rewind on open (s)", "number"],
  ["pipeline", "Pipeline (auto/windows/test)", "text"],
  ["gsiEnabled", "CS2 GSI markers", "checkbox"],
  ["gsiPort", "GSI port", "number"],
  ["gsiToken", "GSI token", "text"],
  ["gsiOffsetSeconds", "GSI marker offset (s)", "number"],
  ["stretch43", "Stretch 4:3 clips to 16:9", "checkbox"],
];

async function toggleSettings(show) {
  const panel = $("settings");
  if (!show) {
    panel.classList.add("hidden");
    return;
  }
  const settings = await invoke("get_settings");
  const fields = $("settings-fields");
  fields.innerHTML = "";
  for (const [key, label, type] of FIELDS) {
    const row = document.createElement("div");
    row.className = "row";
    const value = settings[key];
    row.innerHTML = `<label for="f-${key}">${label}</label>`;
    const input = document.createElement("input");
    input.id = `f-${key}`;
    input.type = type;
    if (type === "checkbox") input.checked = !!value;
    else input.value = value ?? "";
    if (type === "number") input.step = "any";
    row.appendChild(input);
    fields.appendChild(row);
  }
  invoke("capture_stats").then((stats) => {
    $("settings-stats").textContent = stats
      ? `capture: ${stats.framesPushed} frames, ${stats.gopsEvicted} GOPs evicted, ` +
        `${stats.droppedPreIdr} pre-IDR dropped, ${stats.droppedNonMonotonic} non-monotonic`
      : "capture not running";
  });
  panel.classList.remove("hidden");
}

function collectSettings() {
  const out = {};
  for (const [key, , type] of FIELDS) {
    const input = $(`f-${key}`);
    if (type === "checkbox") out[key] = input.checked;
    else if (type === "number") out[key] = Number(input.value);
    else out[key] = input.value;
  }
  // Integer fields.
  for (const k of ["fps", "quality", "gsiPort"]) out[k] = Math.round(out[k]);
  return out;
}

$("btn-apply").addEventListener("click", async () => {
  try {
    await invoke("set_settings", { settings: collectSettings() });
    await invoke("restart_capture");
    toast("settings applied, capture restarted");
    toggleSettings(false);
  } catch (e) {
    toast(`apply failed: ${e}`, 6000);
  }
});

$("btn-close-settings").addEventListener("click", () => toggleSettings(false));
$("btn-quit").addEventListener("click", () => invoke("quit_app"));

// Two-step consent for the one file we write outside our own dirs.
let gsiArmed = false;
$("btn-install-gsi").addEventListener("click", async () => {
  const btn = $("btn-install-gsi");
  if (!gsiArmed) {
    try {
      const target = await invoke("gsi_cfg_target");
      btn.textContent = `Write ${target}? Click again to confirm`;
      gsiArmed = true;
      setTimeout(() => {
        gsiArmed = false;
        btn.textContent = "Install CS2 GSI config…";
      }, 8000);
    } catch (e) {
      toast(`${e}`, 6000);
    }
    return;
  }
  try {
    const path = await invoke("install_gsi_cfg");
    toast(`GSI config installed: ${path} — restart CS2`, 6000);
  } catch (e) {
    toast(`install failed: ${e}`, 6000);
  }
  gsiArmed = false;
  btn.textContent = "Install CS2 GSI config…";
});
