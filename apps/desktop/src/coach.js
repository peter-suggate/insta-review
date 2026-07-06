// Coaching panel: drives the analyze flow (begin -> extract -> run) and
// renders progress and the report in the right-side drawer. The video stays
// visible next to it.

import { extractFrames } from "./extractor.js";

const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

const SEVERITY_COLOR = {
  major: "#ff5a5a",
  minor: "#ffb050",
  info: "#5090ff",
  positive: "#50c878",
};
const SEVERITY_ORDER = { major: 0, minor: 1, info: 2, positive: 3 };

export class Coach {
  constructor({ onToast, onSeek, onTrace } = {}) {
    this.onToast = onToast || (() => {});
    this.onSeek = onSeek || (() => {});
    this.onTrace = onTrace || (() => {});
    this.clip = null; // { samples, buffer, decoderConfig, gsiOffset }
    this.busy = false;
    this.currentEventId = null;
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
  }

  // marker: { at (clip seconds, uncorrected), kind } straight from ClipMeta.
  async openForEvent(marker) {
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

    this.open(`${marker.kind.type} @ ${atS.toFixed(1)}s`);
    this.busy = true;
    try {
      const res = await invoke("analysis_begin", { event });
      if (res.cached) {
        this.busy = false;
        this.render(res.cached, { cached: true });
        return;
      }
      this.progress({ stage: "extracting", detail: "extracting frames…" });
      const sent = await extractFrames({ ...this.clip, wants: res.plan.frames });
      if (!sent) throw new Error("no frames could be extracted");
      await invoke("analysis_run");
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
  }

  close() {
    if (this.busy) {
      invoke("analysis_cancel").catch(() => {});
      this.busy = false;
    }
    $("coach").classList.add("hidden");
  }

  visible() {
    return !$("coach").classList.contains("hidden");
  }

  progress({ stage, detail, current, total }) {
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

    invoke("player_status", {
      status:
        `COACH ${report.event.id}: ${p.provider} ${p.cliVersion} in ` +
        `${(p.durationMs / 1000).toFixed(1)}s${cached ? " (cached)" : ""} — ` +
        `${report.findings?.length ?? 0} findings, summary: ${report.summary.slice(0, 120)}…`,
    }).catch(() => {});
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

function frameFile(tS) {
  return `f_${String(Math.round(tS * 1000)).padStart(6, "0")}ms.jpg`;
}

function b64ToBytes(b64) {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
