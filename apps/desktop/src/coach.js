// Coaching panel: drives the analyze flow (begin -> extract -> run) and
// renders progress and the report in the right-side drawer. The video stays
// visible next to it.

import { extractFrames } from "./extractor.js";

const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

// Color carries judgment only: red = bad, amber = warning, green = good.
// Neutral/contextual things (info findings, measurements) stay gray.
const SEVERITY_COLOR = {
  major: "#ff5a5a",
  minor: "#ffb050",
  info: "#8a8a94",
  positive: "#50c878",
};
const SEVERITY_ORDER = { major: 0, minor: 1, info: 2, positive: 3 };

export class Coach {
  constructor({ onToast, onSeek, onTrace, onState, onVisibility } = {}) {
    this.onToast = onToast || (() => {});
    this.onSeek = onSeek || (() => {});
    this.onTrace = onTrace || (() => {});
    // Coarse lifecycle signal for ambient UI (status pill, marker badges):
    // { state: "idle" | "running" | "done" | "error", ... }.
    this.onState = onState || (() => {});
    // Fired after the drawer is shown/hidden — the stage relayouts around it.
    this.onVisibility = onVisibility || (() => {});
    this.clip = null; // { samples, buffer, decoderConfig, gsiOffset }
    this.busy = false;
    this.currentEventId = null;
    this.currentAtS = null;
    this.currentMarker = null;
    this.minConfidence = 0.5;
    invoke("get_settings")
      .then((s) => (this.minConfidence = s.analysisMinConfidence ?? 0.5))
      .catch(() => {});

    $("coach-close").addEventListener("click", () => this.close());
    $("coach-cancel").addEventListener("click", () => {
      invoke("analysis_cancel").catch(() => {});
    });

    listen("analysis-progress", ({ payload }) => this.progress(payload));
    listen("analysis-complete", ({ payload }) => {
      this.busy = false;
      this.render(payload.report);
      if (payload.trace) this.onTrace(payload.trace);
    });
    listen("analysis-error", ({ payload }) => {
      this.busy = false;
      this.error(payload);
    });
  }

  // Called on every clip-ready: remembers what the extractor needs and
  // resets the panel (findings belong to the previous clip).
  attachClip({ samples, buffer, codec, gsiOffset }) {
    this.clip = {
      samples,
      buffer,
      gsiOffset,
      decoderConfig: {
        codec: codec.codecString,
        description: b64ToBytes(codec.avccB64),
        codedWidth: codec.width,
        codedHeight: codec.height,
        optimizeForLatency: true,
      },
    };
    this.close();
    $("coach-body").textContent = "";
    this.onState({ state: "idle" });
  }

  // marker: { at (clip seconds, uncorrected), kind } straight from ClipMeta.
  // Default is the instant local-CV analysis; `llm: true` (with `force` to
  // bypass a cached quick result) runs the full coach.
  async openForEvent(marker, { force = false, llm = false } = {}) {
    if (!this.clip) return;
    if (this.busy) {
      this.onToast("analysis already running");
      return;
    }
    const atS = Math.max(0, marker.at + this.clip.gsiOffset);
    const event = {
      id: `${marker.kind.type}_${Math.round(atS * 1000)}ms`,
      atS,
      kind: marker.kind,
    };
    this.currentEventId = event.id;
    this.currentAtS = atS;
    this.currentMarker = marker;

    this.open(`${marker.kind.type} @ ${atS.toFixed(1)}s`);
    this.busy = true;
    this.onState({ state: "running", detail: "starting…" });
    try {
      const res = await invoke("analysis_begin", { event, force });
      if (res.cached) {
        this.busy = false;
        this.render(res.cached, { cached: true });
        return;
      }
      this.progress({ stage: "extracting", detail: "extracting frames…" });
      const sent = await extractFrames({ ...this.clip, wants: res.plan.frames });
      if (!sent) throw new Error("no frames could be extracted");
      await invoke("analysis_run", { llm });
      // Completion arrives via analysis-complete / analysis-error events.
    } catch (e) {
      this.busy = false;
      this.error({ kind: "other", stage: "starting", message: `${e.message || e}` });
    }
  }

  open(title) {
    $("coach-title").textContent = `Coach — ${title}`;
    $("coach-provider").textContent = "";
    $("coach-error").classList.add("hidden");
    $("coach-body").textContent = "";
    $("coach").classList.remove("hidden");
    this.onVisibility(true);
  }

  close() {
    if (this.busy) {
      invoke("analysis_cancel").catch(() => {});
      this.busy = false;
    }
    $("coach").classList.add("hidden");
    this.onVisibility(false);
  }

  visible() {
    return !$("coach").classList.contains("hidden");
  }

  // Re-show the drawer with whatever it last held (report or error) —
  // used by the status pill after the user dismissed the drawer.
  reopen() {
    if (
      $("coach-body").childElementCount ||
      !$("coach-error").classList.contains("hidden")
    ) {
      $("coach").classList.remove("hidden");
      this.onVisibility(true);
    }
  }

  progress({ stage, detail, current, total }) {
    this.onState({ state: "running", detail: detail || stage });
    const box = $("coach-progress");
    box.classList.remove("hidden");
    $("coach-stage").textContent = detail || stage;
    const fill = $("coach-bar-fill");
    if (total > 0) {
      fill.style.width = `${Math.round((100 * current) / total)}%`;
      fill.classList.remove("pulse");
    } else {
      fill.style.width = "100%";
      fill.classList.add("pulse");
    }
  }

  render(report, { cached = false } = {}) {
    this.onState({
      state: "done",
      findings: report.findings?.length ?? 0,
      cached,
      atS: this.currentAtS,
    });
    $("coach-progress").classList.add("hidden");
    $("coach-error").classList.add("hidden");
    const p = report.provider;
    $("coach-provider").textContent =
      `${p.provider}${p.model ? ` (${p.model})` : ""} · ${(p.durationMs / 1000).toFixed(0)}s` +
      (cached ? " · cached" : "");

    const body = $("coach-body");
    body.textContent = "";

    if (report.degradations?.length) {
      const warn = el("div", "coach-degraded");
      warn.textContent = `⚠ ${report.degradations.join("; ")}`;
      body.appendChild(warn);
    }

    const summary = el("div", "coach-summary");
    summary.textContent = report.summary;
    body.appendChild(summary);

    const chart = this.fightChart(report);
    if (chart) body.appendChild(chart);
    // Cached reports carry the flow trace in metrics — restore the timeline
    // overlay from it (fresh runs also get it via analysis-complete).
    if (report.metrics?.flowTrace?.length) {
      this.onTrace({ flow: report.metrics.flowTrace });
    }

    const findings = [...(report.findings || [])].sort(
      (a, b) =>
        (SEVERITY_ORDER[a.severity] ?? 9) - (SEVERITY_ORDER[b.severity] ?? 9) ||
        b.confidence - a.confidence
    );
    const shown = findings.filter((f) => f.confidence >= this.minConfidence);
    const low = findings.filter((f) => f.confidence < this.minConfidence);

    for (const f of shown) body.appendChild(this.findingEl(f, report));
    if (low.length) {
      const fold = document.createElement("details");
      fold.className = "coach-low";
      const label = document.createElement("summary");
      label.textContent = `${low.length} low-confidence finding${low.length > 1 ? "s" : ""}`;
      fold.appendChild(label);
      for (const f of low) fold.appendChild(this.findingEl(f, report));
      body.appendChild(fold);
    }

    if (report.frames?.length) {
      const strip = el("div", "coach-frames");
      for (const t of report.frames) {
        const img = document.createElement("img");
        img.src = convertFileSrc(
          `analysis/${report.event.id}/frames/${frameFile(t)}`,
          "replay"
        );
        img.title = `${t.toFixed(2)}s — click to seek`;
        img.addEventListener("click", () => this.onSeek(t));
        strip.appendChild(img);
      }
      body.appendChild(strip);
    }

    // A quick (local-cv) result can be upgraded to a full LLM analysis.
    if (report.provider?.provider === "local-cv" && this.currentMarker) {
      const ask = el("button", "coach-ask");
      ask.textContent = "🧠 Ask coach (LLM)";
      ask.title = "Send these measurements and frames to the AI coach";
      ask.addEventListener("click", () => {
        this.openForEvent(this.currentMarker, { force: true, llm: true }).catch(
          (e) => this.onToast(`analyze failed: ${e}`)
        );
      });
      body.appendChild(ask);
    }

    invoke("player_status", {
      status:
        `COACH ${report.event.id}: ${p.provider} ${p.cliVersion} in ` +
        `${(p.durationMs / 1000).toFixed(1)}s${cached ? " (cached)" : ""} — ` +
        `${report.findings?.length ?? 0} findings, summary: ${report.summary.slice(0, 120)}…`,
    }).catch(() => {});
  }

  // Compact "fight strip": the ~4 s around the event as one seekable chart —
  // movement band, view-velocity curve, shot ticks (with GSI uncertainty
  // windows), flick markers, finding spans, and the event line.
  fightChart(report) {
    const m = report.metrics || {};
    const intervals = m.movementIntervals || [];
    const shots = m.shots || [];
    const flicks = m.flicks || [];
    const flow = m.flowTrace || [];
    if (!intervals.length && !shots.length && !flow.length) return null;

    const at = report.event.atS;
    const t0 = at - 3.5;
    const t1 = at + 0.75;
    const W = 312;
    const H = 104;
    const BAND_Y = 8;
    const BAND_H = 9;
    const CURVE_TOP = 26;
    const CURVE_BOT = 78;
    const SPAN_Y = 82;
    const AXIS_Y = 94;
    const x = (t) => ((t - t0) / (t1 - t0)) * W;
    const clampT = (t) => Math.max(t0, Math.min(t1, t));
    // Movement is context, not judgment — grayscale: light = moving,
    // near-background = still. Verdicts (finding spans, event line) get color.
    const stateColor = { stationary: "#2e2e38", moving: "#8a8a94", unreliable: "#1f1f26" };

    const svg = svgEl("svg", { viewBox: `0 0 ${W} ${H}`, class: "coach-chart" });
    svgTitle(svg, "click to seek the video");

    // Movement band.
    for (const iv of intervals) {
      if (iv.endS < t0 || iv.startS > t1) continue;
      const x0 = x(clampT(iv.startS));
      const band = svgEl("rect", {
        x: x0.toFixed(1),
        y: BAND_Y,
        width: Math.max(x(clampT(iv.endS)) - x0, 1).toFixed(1),
        height: BAND_H,
        fill: stateColor[iv.state] || stateColor.unreliable,
        opacity: 0.85,
      });
      svgTitle(band, `${iv.state} ${iv.startS.toFixed(2)}–${iv.endS.toFixed(2)}s`);
      svg.appendChild(band);
    }
    const cap = svgEl("text", { x: 1, y: BAND_Y - 2, fill: "#8a8a94", "font-size": 7 });
    cap.textContent = "movement";
    svg.appendChild(cap);
    // Tiny legend for the grayscale band.
    for (const [i, [label, color]] of [
      ["moving", "#8a8a94"],
      ["still", "#2e2e38"],
    ].entries()) {
      const lx = W - 82 + i * 44;
      svg.appendChild(
        svgEl("rect", { x: lx, y: BAND_Y - 8, width: 7, height: 6, fill: color })
      );
      const t = svgEl("text", { x: lx + 10, y: BAND_Y - 2, fill: "#8a8a94", "font-size": 7 });
      t.textContent = label;
      svg.appendChild(t);
    }

    // View-velocity curve around a zero line.
    const mid = (CURVE_TOP + CURVE_BOT) / 2;
    svg.appendChild(svgEl("line", { x1: 0, y1: mid, x2: W, y2: mid, stroke: "#26262e" }));
    const inWin = flow.filter((s) => s.t >= t0 && s.t <= t1);
    if (inWin.length > 1) {
      const vmax = Math.max(150, ...inWin.map((s) => Math.abs(s.yawDps)));
      const amp = (CURVE_BOT - CURVE_TOP) / 2;
      const d = inWin
        .map(
          (s, i) =>
            `${i ? "L" : "M"}${x(s.t).toFixed(1)},${(mid - (s.yawDps / vmax) * amp).toFixed(1)}`
        )
        .join("");
      svg.appendChild(
        svgEl("path", { d, fill: "none", stroke: "#9a9aa4", "stroke-width": 1 })
      );
      const vl = svgEl("text", {
        x: 1,
        y: CURVE_TOP + 5,
        fill: "#8a8a94",
        "font-size": 7,
        opacity: 0.85,
      });
      vl.textContent = `view ±${Math.round(vmax)}°/s`;
      svg.appendChild(vl);
    }

    // Shots: uncertainty window + tick + count.
    for (const s of shots) {
      if (s.t < t0 || s.t > t1) continue;
      const unc = s.uncertaintyS || 0;
      const wx = x(clampT(s.t - unc));
      svg.appendChild(
        svgEl("rect", {
          x: wx.toFixed(1),
          y: BAND_Y,
          width: Math.max(x(s.t) - wx, 1).toFixed(1),
          height: CURVE_BOT - BAND_Y,
          fill: "#ffffff",
          opacity: 0.06,
        })
      );
      const tick = svgEl("line", {
        x1: x(s.t),
        y1: BAND_Y,
        x2: x(s.t),
        y2: CURVE_BOT,
        stroke: "#e8e8ec",
        "stroke-width": 1,
        opacity: 0.7,
      });
      svgTitle(
        tick,
        `${s.count} shot${s.count === 1 ? "" : "s"} ` +
          `(${(s.weapon || "").replace("weapon_", "")}) @ ${s.t.toFixed(2)}s ` +
          `±${(unc * 1000).toFixed(0)}ms`
      );
      svg.appendChild(tick);
      const n = svgEl("text", {
        x: x(s.t),
        y: CURVE_TOP - 4,
        fill: "#e8e8ec",
        "font-size": 8,
        "text-anchor": "middle",
      });
      n.textContent = `×${s.count}`;
      svg.appendChild(n);
    }

    // Flick markers.
    for (const f of flicks) {
      if (f.tPeak < t0 || f.tPeak > t1) continue;
      const fx = x(f.tPeak);
      const tri = svgEl("path", {
        d: `M${(fx - 4).toFixed(1)},${CURVE_TOP} L${(fx + 4).toFixed(1)},${CURVE_TOP} L${fx.toFixed(1)},${CURVE_TOP + 6} Z`,
        fill: "#9a9aa4",
      });
      svgTitle(
        tri,
        `flick ${f.displacementDeg?.toFixed(1)}° @ ${f.peakDps?.toFixed(0)}°/s` +
          (f.overshootDeg > 0.5 ? `, overshoot ${f.overshootDeg.toFixed(1)}°` : "") +
          (f.settleMs != null ? `, settled in ${f.settleMs.toFixed(0)}ms` : "")
      );
      svg.appendChild(tri);
    }

    // Finding spans, colored by severity.
    for (const f of report.findings || []) {
      const [a, b] = f.timeRange || [];
      if (a == null || b < t0 || a > t1) continue;
      const sx = x(clampT(a));
      const span = svgEl("rect", {
        x: sx.toFixed(1),
        y: SPAN_Y,
        width: Math.max(x(clampT(b)) - sx, 2).toFixed(1),
        height: 3,
        rx: 1.5,
        fill: SEVERITY_COLOR[f.severity] || "#888",
      });
      svgTitle(span, f.kind.replaceAll("_", " "));
      svg.appendChild(span);
    }

    // Event line + label.
    const evColor = report.event.kind?.type === "death" ? "#ff5a5a" : "#50c878";
    svg.appendChild(
      svgEl("line", {
        x1: x(at),
        y1: BAND_Y - 4,
        x2: x(at),
        y2: AXIS_Y - 6,
        stroke: evColor,
        "stroke-width": 1.5,
      })
    );
    const evLabel = svgEl("text", {
      x: Math.min(x(at) + 3, W - 26),
      y: AXIS_Y - 8,
      fill: evColor,
      "font-size": 8,
    });
    evLabel.textContent = report.event.kind?.type || "event";
    svg.appendChild(evLabel);

    // Axis: 1 s ticks, labeled relative to the event.
    for (let dt = -3; dt <= 0.5; dt += 1) {
      const t = at + dt;
      if (t < t0 || t > t1) continue;
      svg.appendChild(
        svgEl("line", { x1: x(t), y1: AXIS_Y - 5, x2: x(t), y2: AXIS_Y - 2, stroke: "#8a8a94" })
      );
      const lbl = svgEl("text", {
        x: x(t),
        y: AXIS_Y + 7,
        fill: "#8a8a94",
        "font-size": 7,
        "text-anchor": "middle",
      });
      lbl.textContent = dt === 0 ? "event" : `${dt}s`;
      svg.appendChild(lbl);
    }

    svg.addEventListener("click", (e) => {
      const r = svg.getBoundingClientRect();
      const t = t0 + ((e.clientX - r.left) / r.width) * (t1 - t0);
      this.onSeek(Math.max(0, t));
    });
    return svg;
  }

  // Zone gauge for the first recognizable numeric metric of a finding —
  // stop→shot gap, overshoot, or settle time against their thresholds.
  metricGauge(metrics) {
    const specs = [
      ["stopToShotMs", { max: 320, fmt: (v) => `${v.toFixed(0)} ms stop→shot`,
        zones: [[66, "#ff5a5a"], [250, "#50c878"], [320, "#55555f"]] }],
      ["overshootDeg", { max: 6, fmt: (v) => `${v.toFixed(1)}° overshoot`,
        zones: [[2.5, "#50c878"], [6, "#ffb050"]] }],
      ["settleMs", { max: 300, fmt: (v) => `settled in ${v.toFixed(0)} ms`,
        zones: [[150, "#50c878"], [300, "#ffb050"]] }],
    ];
    for (const [key, spec] of specs) {
      const v = metrics?.[key];
      if (typeof v !== "number") continue;
      const W = 110;
      const H = 8;
      const svg = svgEl("svg", { viewBox: `0 0 ${W} ${H}`, width: W, height: H });
      let zx = 0;
      for (const [to, color] of spec.zones) {
        const zEnd = (Math.min(to, spec.max) / spec.max) * W;
        svg.appendChild(
          svgEl("rect", { x: zx, y: 2, width: zEnd - zx, height: H - 4, fill: color, opacity: 0.35 })
        );
        zx = zEnd;
      }
      svg.appendChild(
        svgEl("rect", {
          x: (Math.min(v / spec.max, 1) * (W - 2)).toFixed(1),
          y: 0,
          width: 2,
          height: H,
          fill: "#e8e8ec",
        })
      );
      const wrap = el("div", "coach-gauge");
      wrap.appendChild(svg);
      const label = document.createElement("span");
      label.textContent = spec.fmt(v);
      wrap.appendChild(label);
      return wrap;
    }
    return null;
  }

  findingEl(f, report) {
    const item = el("div", "coach-finding");

    const head = el("div", "coach-finding-head");
    const dot = el("span", "coach-dot");
    dot.style.background = SEVERITY_COLOR[f.severity] || "#888";
    dot.title = f.severity;
    head.appendChild(dot);

    const title = el("span", "coach-kind");
    title.textContent = f.kind.replaceAll("_", " ");
    head.appendChild(title);

    const conf = el("span", "coach-conf");
    conf.textContent = `${Math.round(f.confidence * 100)}%`;
    head.appendChild(conf);

    const [startS, endS] = f.timeRange || [null, null];
    if (startS != null) {
      const chip = el("button", "coach-chip");
      chip.textContent =
        endS > startS ? `${startS.toFixed(1)}–${endS.toFixed(1)}s` : `${startS.toFixed(1)}s`;
      chip.addEventListener("click", () => this.onSeek(startS));
      head.appendChild(chip);
    }
    item.appendChild(head);

    const text = el("div", "coach-coaching");
    text.textContent = f.coaching;
    item.appendChild(text);

    const gauge = this.metricGauge(f.metrics);
    if (gauge) item.appendChild(gauge);

    if (f.evidence?.length) {
      const row = el("div", "coach-evidence");
      for (const e of f.evidence) {
        const chip = el("button", "coach-chip");
        chip.textContent = `${e.t.toFixed(2)}s`;
        if (e.note) chip.title = e.note;
        chip.addEventListener("click", () => this.onSeek(e.t));
        row.appendChild(chip);
      }
      item.appendChild(row);
    }

    const thumbs = el("div", "coach-thumbs");
    const index = report.findings.indexOf(f);
    for (const [glyph, up] of [
      ["👍", true],
      ["👎", false],
    ]) {
      const btn = el("button", "coach-thumb");
      btn.textContent = glyph;
      btn.addEventListener("click", () => {
        invoke("analysis_feedback", {
          eventId: report.event.id,
          findingIndex: index,
          up,
        })
          .then(() => {
            thumbs.querySelectorAll("button").forEach((b) => b.classList.remove("active"));
            btn.classList.add("active");
          })
          .catch((err) => this.onToast(`feedback failed: ${err}`));
      });
      thumbs.appendChild(btn);
    }
    item.appendChild(thumbs);

    return item;
  }

  error({ kind, stage, message }) {
    if (kind === "cancelled") this.onState({ state: "idle" });
    else this.onState({ state: "error", kind, message });
    $("coach-progress").classList.add("hidden");
    const el = $("coach-error");
    el.classList.remove("hidden");
    el.textContent = kind === "cancelled" ? "cancelled" : `${message} (${stage})`;
    invoke("player_status", {
      status: `COACH ERROR (${stage}/${kind}): ${message}`,
    }).catch(() => {});
  }
}

function el(tag, className) {
  const node = document.createElement(tag);
  node.className = className;
  return node;
}

const SVG_NS = "http://www.w3.org/2000/svg";
function svgEl(tag, attrs = {}) {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  return node;
}
function svgTitle(parent, text) {
  const t = svgEl("title");
  t.textContent = text;
  parent.appendChild(t);
}

function frameFile(tS) {
  return `f_${String(Math.round(tS * 1000)).padStart(6, "0")}ms.jpg`;
}

function b64ToBytes(b64) {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
