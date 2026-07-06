// Coaching panel: drives the analyze flow (begin -> extract -> run) and
// renders progress and the report in the right-side drawer. The video stays
// visible next to it.

import { extractFrames } from "./extractor.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

export class Coach {
  constructor({ onToast } = {}) {
    this.onToast = onToast || (() => {});
    this.clip = null; // { samples, buffer, decoderConfig, gsiOffset }
    this.busy = false;

    $("coach-close").addEventListener("click", () => this.close());
    $("coach-cancel").addEventListener("click", () => {
      invoke("analysis_cancel").catch(() => {});
    });

    listen("analysis-progress", ({ payload }) => this.progress(payload));
    listen("analysis-complete", ({ payload }) => {
      this.busy = false;
      this.render(payload.report);
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
      const wants = res.plan.frames.filter((f) => f.wantJpeg);
      const sent = await extractFrames({ ...this.clip, wants });
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
    $("coach-body").textContent = report.summary;
    invoke("player_status", {
      status:
        `COACH ${report.event.id}: ${p.provider} ${p.cliVersion} in ` +
        `${(p.durationMs / 1000).toFixed(1)}s${cached ? " (cached)" : ""} — ` +
        `${report.summary.length} chars: ${report.summary.slice(0, 160)}…`,
    }).catch(() => {});
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

function b64ToBytes(b64) {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
