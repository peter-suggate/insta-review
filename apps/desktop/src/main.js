import { Coach } from "./coach.js";
import { Player, b64ToBytes } from "./player.js";
import { Timeline } from "./timeline.js";

const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const RATES = [0.1, 0.25, 0.5, 1, 2];

// Background work (filmstrip) defers to anything the user is doing.
let lastInteraction = 0;
for (const ev of ["keydown", "pointerdown", "pointermove", "wheel"])
  window.addEventListener(ev, () => (lastInteraction = performance.now()), {
    passive: true,
    capture: true,
  });

let clipMeta = null;
let gsiOffsetUs = 0;
let capturedAtMs = null;
let openPointUs = 0;
let loadedClipId = 0;
let loadInFlight = false;
let queuedPayload = null;

function formatAge(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 5) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ago`;
  return `${Math.floor(m / 60)}h ${m % 60}m ago`;
}

function updateCapturedHud() {
  const el = $("hud-captured");
  if (capturedAtMs == null) {
    el.textContent = "";
    el.classList.remove("fresh");
    return;
  }
  const age = Date.now() - capturedAtMs;
  const clock = new Date(capturedAtMs).toLocaleTimeString([], {
    hour12: false,
  });
  el.textContent = `captured ${clock} · ${formatAge(age)}`;
  // Fresh clips glow accent so a re-shown stale clip is obviously not new.
  el.classList.toggle("fresh", age < 15000);
}
setInterval(updateCapturedHud, 1000);

const timeline = new Timeline($("timeline"), {
  onSeek: (us) => {
    player.pause();
    player.seekToUs(us).catch(console.error);
  },
  onMarkerClick: (marker) => {
    player.pause();
    coach.openForEvent(marker).catch((e) => toast(`analyze failed: ${e}`, 5000));
  },
});

const player = new Player($("video"), {
  onSlow: (msg) =>
    invoke("player_status", { status: `SLOW ${msg}` }).catch(() => {}),
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
    syncControls();
  },
});

// ---- transport controls ------------------------------------------------

const speedsEl = $("speeds");
for (const r of RATES) {
  const b = document.createElement("button");
  b.dataset.rate = r;
  b.textContent = `${r}×`;
  b.disabled = true;
  b.addEventListener("click", () => player.setRate(r));
  speedsEl.appendChild(b);
}

function syncControls() {
  $("btn-play").textContent = player.playing ? "⏸" : "▶";
  for (const b of speedsEl.children)
    b.classList.toggle("active", Number(b.dataset.rate) === player.rate);
}

// The "go" action: back to the primed point, then play at the chosen rate.
function replay() {
  if (!clipMeta) return;
  player.pause();
  player
    .seekToUs(openPointUs)
    .then(() => player.play())
    .catch(console.error);
}

// Discard the clip and return to the live capturing view. Capture never
// stopped; the next hotkey press stages a fresh clip.
function clearAll() {
  if (!clipMeta) return;
  clipMeta = null; // aborts the in-flight filmstrip build too
  player.reset();
  const video = $("video");
  video.getContext("2d").clearRect(0, 0, video.width, video.height);
  timeline.load({
    durationUs: 0,
    markers: [],
    keyframesUs: [],
    triggerUs: 0,
    gsiOffsetUs: 0,
  });
  capturedAtMs = null;
  updateCapturedHud();
  $("hud").classList.add("hidden");
  $("waiting").classList.remove("hidden");
  for (const b of document.querySelectorAll("#controls button")) b.disabled = true;
  previewFails = 0; // let the live preview resume
  invoke("clear_clip").catch(() => {});
  toast("cleared — still capturing");
}

$("btn-clear").addEventListener("click", clearAll);

$("btn-replay").addEventListener("click", replay);
$("btn-play").addEventListener("click", () => player.toggle());
$("btn-step-back").addEventListener("click", () => player.step(-1));
$("btn-step-fwd").addEventListener("click", () => player.step(1));
// Buttons must not steal focus, or Space would re-trigger the last-clicked
// button instead of acting as the global play/pause shortcut.
$("controls").addEventListener("mousedown", (e) => e.preventDefault());

function toast(msg, ms = 2500) {
  const el = $("toast");
  el.textContent = msg;
  el.classList.remove("hidden");
  clearTimeout(el._t);
  el._t = setTimeout(() => el.classList.add("hidden"), ms);
}

const coach = new Coach({
  onToast: toast,
  onSeek: (tS) => {
    player.pause();
    player.seekToUs(tS * 1e6).catch(console.error);
  },
});

// Kill/death marker nearest the playhead (display times, i.e. GSI-shifted).
function eventMarkerNearPlayhead() {
  const candidates = (clipMeta?.meta.markers || []).filter((m) =>
    ["kill", "death"].includes(m.kind.type)
  );
  if (!candidates.length) return null;
  const nowS = player.playheadUs / 1e6 - gsiOffsetUs / 1e6;
  return candidates.reduce((best, m) =>
    Math.abs(m.at - nowS) < Math.abs(best.at - nowS) ? m : best
  );
}

async function loadClip(payload) {
  // Breadcrumbs into the app log: if a load wedges, the last one names
  // the stage that hung.
  const mark = (s) =>
    invoke("player_status", { status: `clip ${payload.id}: ${s}` }).catch(
      () => {}
    );
  mark("load started");
  clipMeta = payload;
  closePreviewDecoder(); // one decoder session at a time
  gsiOffsetUs = payload.gsiOffset * 1e6;
  capturedAtMs = payload.capturedAtMs ?? null;
  updateCapturedHud();
  $("waiting").classList.add("hidden");
  $("hud").classList.remove("hidden");

  const url = convertFileSrc(`clip/${payload.id}/samples`, "replay");
  const response = await fetch(url);
  if (!response.ok) throw new Error(`fetch samples: ${response.status}`);
  const buffer = await response.arrayBuffer();
  mark(`fetched ${buffer.byteLength} bytes`);

  await player.load({
    codec: payload.codec,
    samples: payload.samples,
    buffer,
    stretch43: payload.stretch43,
  });
  mark("decoder configured");
  // One-off diagnosis: is hardware decode even on the table here?
  for (const ha of ["prefer-hardware", "prefer-software"]) {
    try {
      const s = await VideoDecoder.isConfigSupported({
        ...player.decoderConfig,
        hardwareAcceleration: ha,
      });
      mark(`${ha} supported: ${s.supported}`);
    } catch (e) {
      mark(`${ha} probe error: ${e.message || e}`);
    }
  }

  coach.attachClip({
    samples: payload.samples,
    buffer,
    codec: payload.codec,
    gsiOffset: payload.gsiOffset,
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

  // Open paused just before the moment of interest: 1 s before the most
  // recent death (the hotkey is pressed after dying), else 1 s before the
  // latest kill, else the start of the clip. R/replay returns here too.
  const latestMarkerUs = (type) =>
    payload.meta.markers
      .filter((m) => m.kind?.type === type)
      .map((m) => m.at * 1e6 + gsiOffsetUs)
      .filter((t) => t >= 0 && t <= player.durationUs())
      .sort((a, b) => a - b)
      .pop();
  const focusUs = latestMarkerUs("death") ?? latestMarkerUs("kill");
  const openUs = focusUs != null ? Math.max(0, focusUs - 1e6) : 0;
  await player.seekToUs(openUs);
  openPointUs = openUs;
  for (const b of document.querySelectorAll("#controls button"))
    b.disabled = false;
  player.onStateChange();
  toast(
    `clip loaded — ${payload.samples.length} frames` +
      (capturedAtMs != null
        ? `, captured ${new Date(capturedAtMs).toLocaleTimeString([], {
            hour12: false,
          })}`
        : "")
  );
  const decode = player.lastDecodeStats || {};
  invoke("player_status", {
    status:
      `loaded clip ${payload.id}: ${payload.samples.length} samples, ` +
      `${(buffer.byteLength / 1048576).toFixed(1)} MiB blob, ` +
      `codec ${payload.codec.codecString}, opened paused at frame ${player.cur} ` +
      `(${(player.playheadUs / 1e6).toFixed(2)}s), ${payload.meta.markers.length} markers, ` +
      `cache ${player.cache.size}, decode outputs ${decode.outputs} unmatched ${decode.unmatched}`,
  }).catch(() => {});

  // Filmstrip afterwards; it paints progressively and defers to all
  // player work, so it never delays interactivity.
  buildFilmstrip(payload).catch((e) =>
    invoke("player_status", {
      status: `filmstrip failed: ${e.message || e}`,
    }).catch(() => {})
  );
}

// Decode one keyframe per timeline slot (nearest to the slot's center)
// through the player's own decoder/cache — no second decoder session.
async function buildFilmstrip(payload) {
  const dpr = window.devicePixelRatio || 1;
  const stripCanvas = $("timeline");
  const h = Math.max(1, Math.round(stripCanvas.clientHeight * dpr));
  const stripW = stripCanvas.clientWidth * dpr;
  const keys = payload.meta.keyframe_indices;
  const durationUs = payload.samples[payload.samples.length - 1]?.tUs || 0;
  if (!keys.length || !durationUs) return;

  let ar = payload.codec.width / payload.codec.height;
  if (payload.stretch43 && Math.abs(ar - 4 / 3) < 0.05) ar = 16 / 9;
  const slotW = Math.max(24, Math.round((h / dpr) * ar) * dpr);
  const count = Math.max(1, Math.min(keys.length, Math.ceil(stripW / slotW)));

  const picks = [];
  for (let i = 0; i < count; i++) {
    const target = ((i + 0.5) / count) * durationUs;
    let best = keys[0];
    for (const k of keys)
      if (
        Math.abs(payload.samples[k].tUs - target) <
        Math.abs(payload.samples[best].tUs - target)
      )
        best = k;
    picks.push(best);
  }

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const bitmaps = [];
  const stripT0 = performance.now();
  for (const keyIdx of picks) {
    // Strictly lowest priority: never decode while the user is playing,
    // scrubbing, waiting on a frame, or has touched anything recently.
    while (
      player.playing ||
      player.waiters.size > 0 ||
      performance.now() - lastInteraction < 500
    ) {
      if (clipMeta !== payload) break;
      await sleep(200);
    }
    if (clipMeta !== payload) {
      // Superseded mid-build; the new clip's filmstrip owns the strip.
      for (const b of bitmaps) b.close();
      return;
    }
    try {
      player.holdIdx = keyIdx; // survive eviction until the bitmap is made
      await player.ensureFrame(keyIdx);
      const frame = player.cache.get(keyIdx);
      if (!frame) throw new Error(`keyframe ${keyIdx} not cached`);
      bitmaps.push(
        await createImageBitmap(frame, {
          resizeWidth: slotW,
          resizeHeight: h,
        })
      );
      player.holdIdx = null;
    } catch (e) {
      player.holdIdx = null;
      invoke("player_status", {
        status: `filmstrip stopped at ${bitmaps.length}/${picks.length}: ${e.message || e}`,
      }).catch(() => {});
      break; // ship what we have
    }
    // Paint progressively and yield so queued player work wins the slot.
    timeline.setThumbnails(bitmaps.slice(), picks.length);
    await sleep(30);
  }
  invoke("player_status", {
    status: `filmstrip: ${bitmaps.length}/${picks.length} thumbs in ${(
      performance.now() - stripT0
    ).toFixed(0)} ms`,
  }).catch(() => {});
}

// ---- live preview on the idle screen -----------------------------------
// While no clip is loaded, poll the ring's newest keyframe (~1 Hz) and
// decode it into a thumbnail so it's visible that capture is running.

// Persistent preview decoder — decoder init costs ~seconds on some
// machines, so recreating it every poll is not an option. It only runs
// while no clip is loaded, so it never competes with the player.
let previewDec = null;
let previewCfgKey = "";
let previewOut = null;

function closePreviewDecoder() {
  if (previewDec) {
    try {
      previewDec.close();
    } catch {}
    previewDec = null;
  }
  if (previewOut) {
    previewOut.close();
    previewOut = null;
  }
  previewCfgKey = "";
}

async function previewDecode(p) {
  const cfgKey = `${p.codecString}|${p.avccB64}|${p.width}x${p.height}`;
  if (!previewDec || previewDec.state !== "configured" || previewCfgKey !== cfgKey) {
    closePreviewDecoder();
    previewDec = new VideoDecoder({
      output: (f) => (previewOut ? f.close() : (previewOut = f)),
      error: () => {}, // surfaced by flush() rejection
    });
    previewDec.configure({
      codec: p.codecString,
      description: b64ToBytes(p.avccB64),
      codedWidth: p.width,
      codedHeight: p.height,
      optimizeForLatency: true,
    });
    previewCfgKey = cfgKey;
  }
  previewOut = null;
  try {
    previewDec.decode(
      new EncodedVideoChunk({
        type: "key",
        timestamp: 0,
        data: b64ToBytes(p.dataB64),
      })
    );
    await Promise.race([
      previewDec.flush(),
      new Promise((_, rej) =>
        setTimeout(() => rej(new Error("preview decode timeout")), 3000)
      ),
    ]);
    const frame = previewOut;
    previewOut = null;
    if (!frame) throw new Error("keyframe produced no output");
    return frame;
  } catch (e) {
    closePreviewDecoder(); // recreate fresh on the next poll
    throw e;
  }
}

let previewBusy = false;
let previewFails = 0;

async function pollPreview() {
  // Only while the waiting overlay is up; once a clip loads it takes over.
  // Back off for good after repeated failures rather than churning the
  // decoder pool every second.
  if (clipMeta || loadInFlight || document.hidden || previewBusy) return;
  if (previewFails >= 3) return;
  previewBusy = true;
  try {
    const p = await invoke("preview_frame");
    if (clipMeta || loadInFlight) return; // clip landed while fetching
    if (!p) {
      $("preview").classList.add("hidden");
      return;
    }
    const canvas = $("preview-canvas");
    // Thumbnail-size backing store; drawImage scales the frame down.
    const w = Math.round(440 * (window.devicePixelRatio || 1));
    const h = Math.round((w * p.height) / p.width);
    const frame = await previewDecode(p);
    canvas.width = w;
    canvas.height = h;
    canvas.getContext("2d").drawImage(frame, 0, 0, w, h);
    frame.close();
    previewFails = 0;
    $("preview-status").textContent =
      `live — last ${p.spanSeconds.toFixed(0)}s buffered`;
    $("preview").classList.remove("hidden");
  } catch (e) {
    previewFails++;
    invoke("player_status", {
      status: `preview decode failed (${previewFails}): ${e.message || e}`,
    }).catch(() => {});
    if (previewFails >= 3) $("preview").classList.add("hidden");
  } finally {
    previewBusy = false;
  }
}
setInterval(pollPreview, 1000);
pollPreview();

// Personalize the idle hint with the real hotkey + window.
invoke("get_settings")
  .then((s) => {
    $("waiting-msg").textContent =
      `capturing — press ${s.hotkey.toUpperCase()} in game ` +
      `to review the last ${Math.round(s.windowSeconds)}s`;
  })
  .catch(() => {});

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
    const tRead = performance.now();
    const c0 = readCounter(player.currentFrame());
    const readMs = performance.now() - tRead;
    const t0 = performance.now();
    await player.step(1);
    const step1Ms = performance.now() - t0;
    const c1 = readCounter(player.currentFrame());
    const t1 = performance.now();
    await player.step(1);
    const step2Ms = performance.now() - t1;
    const c2 = readCounter(player.currentFrame());
    const stepMs = (step1Ms + step2Ms) / 2;
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

    const passes = player.passLog || [];
    const chunks = passes.reduce((a, p) => a + p.n, 0);
    const passMs = passes.reduce((a, p) => a + p.ms, 0);
    const passDetail = passes
      .map((p) => `${p.n}:${p.ms.toFixed(0)}`)
      .join(" ");
    report(
      `SELFTEST counters ${c0}→${c1}→${c2}, back→${c3}: ` +
        `fwd ${fwdOk ? "OK" : "FAIL"}, back ${backOk ? "OK" : "FAIL"}, ` +
        `steps ${step1Ms.toFixed(0)}/${step2Ms.toFixed(0)} ms (pixel read ${readMs.toFixed(0)} ms), ` +
        `played ${played} frames in 1.2 s, ` +
        `decode ${chunks} chunks in ${passMs.toFixed(0)} ms across ${passes.length} passes ` +
        `(${passMs > 0 ? ((chunks * 1000) / passMs).toFixed(0) : "?"} chunks/s) ` +
        `[chunks:ms per pass — ${passDetail}]`
    );
    void stepMs;
  } catch (e) {
    report(`SELFTEST ERROR: ${e.message || e}`);
  }
}

// Clip loads are serialized and newest-wins: clip-ready events can arrive
// late or bunched (WebView2 suspends throttled/minimized windows), and a
// concurrent double-load leaves the player on a stale frame.
async function loadClipLatest(payload) {
  if (payload.id <= loadedClipId) return;
  if (loadInFlight) {
    if (!queuedPayload || payload.id > queuedPayload.id) queuedPayload = payload;
    return;
  }
  loadInFlight = true;
  try {
    await loadClip(payload);
    loadedClipId = payload.id;
    if (payload.autotest) selfTest();
    // Dev hook (IR_AUTOANALYZE=1): analyze the trigger moment without a
    // keyboard — synthesizes a death event if the clip has no markers.
    else if (payload.autoanalyze) {
      const marker = eventMarkerNearPlayhead() || {
        at: payload.meta.trigger_at - payload.gsiOffset,
        kind: { type: "death" },
      };
      coach.openForEvent(marker).catch(console.error);
    }
  } catch (e) {
    console.error(e);
    toast(`failed to load clip: ${e.message || e}`, 6000);
    invoke("player_status", {
      status: `ERROR loading clip: ${e.message || e}`,
    }).catch(() => {});
  } finally {
    loadInFlight = false;
    if (queuedPayload) {
      const next = queuedPayload;
      queuedPayload = null;
      loadClipLatest(next);
    }
  }
}

listen("clip-ready", (event) => loadClipLatest(event.payload));

// Catch-up: ask for the staged clip whenever we (re)gain visibility, in
// case clip-ready fired while the webview was suspended.
async function syncCurrentClip() {
  try {
    const payload = await invoke("current_clip");
    if (payload) await loadClipLatest(payload);
  } catch {}
}
window.addEventListener("focus", syncCurrentClip);
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) syncCurrentClip();
});
syncCurrentClip();

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
    case "r":
    case "R":
    case "Enter":
      replay();
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
    case "e":
    case "E": {
      const marker = eventMarkerNearPlayhead();
      if (!marker) {
        toast("no kill/death marker in this clip");
        break;
      }
      player.pause();
      coach.openForEvent(marker).catch((err) => toast(`analyze failed: ${err}`, 5000));
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
    case "c":
    case "C":
      clearAll();
      break;
    case "g":
    case "G":
      toggleSettings(true);
      break;
    case "Escape":
      if (coach.visible()) {
        coach.close();
        break;
      }
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
  ["pipeline", "Pipeline (auto/windows/test)", "text"],
  ["captureCropPx", "Capture crop around center (px, 0 = full screen)", "number"],
  ["gsiEnabled", "CS2 GSI markers", "checkbox"],
  ["gsiPort", "GSI port", "number"],
  ["gsiToken", "GSI token", "text"],
  ["gsiOffsetSeconds", "GSI marker offset (s)", "number"],
  ["stretch43", "Stretch 4:3 clips to 16:9", "checkbox"],
  ["llmProvider", "Coach LLM (claude/codex)", "text"],
  ["llmModel", "LLM model (blank = default)", "text"],
  ["llmBinaryPath", "LLM binary path (blank = PATH)", "text"],
  ["llmExtraArgs", "LLM extra args", "text"],
  ["llmTimeoutSeconds", "LLM timeout (s)", "number"],
  ["analysisMinConfidence", "Min finding confidence", "number"],
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
  for (const k of ["fps", "quality", "gsiPort", "captureCropPx", "llmTimeoutSeconds"])
    out[k] = Math.round(out[k]);
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
