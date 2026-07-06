// Timeline strip: playhead, keyframe ticks, trigger line, GSI markers
// (drawn as soft bands + icons because GSI timing is approximate).

const MARKER_STYLE = {
  kill: { color: "#50c878", label: "K" },
  death: { color: "#ff5a5a", label: "D" },
  damage_taken: { color: "#ffb050", label: "·" },
  round_phase: { color: "#5090ff", label: "R" },
  bomb: { color: "#ffe050", label: "B" },
  shot_fired: { color: "#9a9aa4", label: "s" },
};

export class Timeline {
  constructor(canvas, { onSeek, onMarkerClick } = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");
    this.onSeek = onSeek || (() => {});
    this.onMarkerClick = onMarkerClick || null;
    this.durationUs = 0;
    this.playheadUs = 0;
    this.markers = [];
    this.keyframesUs = [];
    this.triggerUs = 0;
    this.gsiOffsetUs = 0;
    this.thumbs = null; // filmstrip ImageBitmaps, left to right

    const seekFromEvent = (e) => {
      const rect = canvas.getBoundingClientRect();
      const frac = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
      this.onSeek(frac * this.durationUs);
    };
    let dragging = false;
    canvas.addEventListener("pointerdown", (e) => {
      // A click on a kill/death icon (top strip) analyzes instead of seeking.
      const hit = this.markerAt(e);
      if (hit && this.onMarkerClick) {
        this.onMarkerClick(hit);
        return;
      }
      dragging = true;
      canvas.setPointerCapture(e.pointerId);
      seekFromEvent(e);
    });
    canvas.addEventListener("pointermove", (e) => {
      if (dragging) seekFromEvent(e);
    });
    canvas.addEventListener("pointerup", () => (dragging = false));
  }

  load({ durationUs, markers, keyframesUs, triggerUs, gsiOffsetUs }) {
    this.durationUs = durationUs;
    this.markers = markers;
    this.keyframesUs = keyframesUs;
    this.triggerUs = triggerUs;
    this.gsiOffsetUs = gsiOffsetUs;
    this.thumbs = null; // stale filmstrip belongs to the previous clip
    this.analysis = null; // traces belong to the previous clip too
    this.draw();
  }

  // Progressive: list may be shorter than total while the filmstrip is
  // still decoding; slots stay sized for the final count.
  setThumbnails(list, total) {
    this.thumbs = list && list.length ? { list, total: total || list.length } : null;
    this.draw();
  }

  // CV trace from a completed analysis: { flow: [{t, yawDps, moving}] }.
  // Drawn as a movement-state strip + a view-velocity curve, aligned with
  // the playhead so scrubbing lines the numbers up with the footage.
  setAnalysis(trace) {
    this.analysis = trace;
    this.draw();
  }

  setPlayhead(us) {
    this.playheadUs = us;
    this.draw();
  }

  // Kill/death marker whose icon (top 16 css px) is within ±6 px of the
  // pointer, or null.
  markerAt(e) {
    const rect = this.canvas.getBoundingClientRect();
    const px = e.clientX - rect.left;
    const py = e.clientY - rect.top;
    if (py > 16 || !this.durationUs) return null;
    let best = null;
    let bestDist = 7; // > 6 px = no hit
    for (const marker of this.markers) {
      if (!["kill", "death"].includes(marker.kind.type)) continue;
      const t = marker.at * 1e6 + this.gsiOffsetUs;
      const mx = (t / this.durationUs) * rect.width;
      const d = Math.abs(mx - px);
      if (d < bestDist) {
        bestDist = d;
        best = marker;
      }
    }
    return best;
  }

  nextMarkerAfter(us) {
    const sorted = [...this.markers].sort((a, b) => a.at - b.at);
    for (const marker of sorted) {
      const t = marker.at * 1e6 + this.gsiOffsetUs;
      if (t > us + 1000) return t;
    }
    return sorted.length ? sorted[0].at * 1e6 + this.gsiOffsetUs : null;
  }

  draw() {
    const canvas = this.canvas;
    const dpr = window.devicePixelRatio || 1;
    const cssW = canvas.clientWidth,
      cssH = canvas.clientHeight;
    if (canvas.width !== cssW * dpr || canvas.height !== cssH * dpr) {
      canvas.width = cssW * dpr;
      canvas.height = cssH * dpr;
    }
    const ctx = this.ctx;
    const w = canvas.width,
      h = canvas.height;
    const x = (us) => (this.durationUs ? (us / this.durationUs) * w : 0);

    ctx.fillStyle = "#131318";
    ctx.fillRect(0, 0, w, h);

    // Filmstrip: thumbnails stretched to tile the full width, dimmed so
    // the overlays (playhead, markers) stay readable.
    if (this.thumbs) {
      const slotW = w / this.thumbs.total;
      this.thumbs.list.forEach((bitmap, i) => {
        if (bitmap) ctx.drawImage(bitmap, i * slotW, 0, slotW, h);
      });
      ctx.fillStyle = "rgba(0, 0, 0, 0.28)";
      ctx.fillRect(0, 0, w, h);
    }

    // Keyframe ticks along the bottom.
    ctx.fillStyle = "#2c2c36";
    for (const kUs of this.keyframesUs) {
      ctx.fillRect(x(kUs), h - 8 * dpr, Math.max(1, dpr), 8 * dpr);
    }

    // Markers: soft band + icon (GSI timing is approximate).
    for (const marker of this.markers) {
      const t = marker.at * 1e6 + this.gsiOffsetUs;
      if (t < 0 || t > this.durationUs) continue;
      const style = MARKER_STYLE[marker.kind.type] || {
        color: "#888",
        label: "?",
      };
      const bandHalf = x(300_000) - x(0); // ±300 ms
      ctx.fillStyle = style.color + "22";
      ctx.fillRect(x(t) - bandHalf, 0, bandHalf * 2, h);
      ctx.fillStyle = style.color;
      ctx.fillRect(x(t) - dpr, 0, 2 * dpr, h);
      ctx.font = `${11 * dpr}px system-ui`;
      ctx.textAlign = "center";
      ctx.fillText(style.label, x(t), 13 * dpr);
    }

    // CV analysis overlay: movement strip + yaw-velocity curve.
    if (this.analysis?.flow?.length) {
      const flow = this.analysis.flow;
      const stripY = 15 * dpr,
        stripH = 3 * dpr;
      const stateColor = {
        stationary: "#50c87855",
        moving: "#ffb050cc",
        unreliable: "#55555588",
      };
      for (let i = 0; i < flow.length; i++) {
        const t0 = i > 0 ? flow[i - 1].t : flow[i].t - 0.017;
        ctx.fillStyle = stateColor[flow[i].moving] || stateColor.unreliable;
        const x0 = x(t0 * 1e6);
        ctx.fillRect(x0, stripY, Math.max(x(flow[i].t * 1e6) - x0, dpr), stripH);
      }
      // Yaw velocity curve, centered vertically, clamped scale.
      const mid = h * 0.55;
      const amp = h * 0.3;
      const vmax = Math.max(200, ...flow.map((s) => Math.abs(s.yawDps)));
      ctx.strokeStyle = "#5090ffaa";
      ctx.lineWidth = dpr;
      ctx.beginPath();
      for (let i = 0; i < flow.length; i++) {
        const fx = x(flow[i].t * 1e6);
        const fy = mid - (flow[i].yawDps / vmax) * amp;
        if (i === 0) ctx.moveTo(fx, fy);
        else ctx.lineTo(fx, fy);
      }
      ctx.stroke();
    }

    // Trigger line (kept dim: it's context, not the thing you drag).
    ctx.fillStyle = "#8a8a94";
    ctx.fillRect(x(this.triggerUs) - dpr, 0, 2 * dpr, h);
    ctx.font = `${10 * dpr}px system-ui`;
    ctx.textAlign = "left";
    ctx.fillText("hotkey", x(this.triggerUs) + 4 * dpr, h - 4 * dpr);

    // Playhead: white with a knob so it can't be confused with the green
    // kill markers, plus a dark outline to survive bright thumbnails.
    const px = x(this.playheadUs);
    ctx.fillStyle = "rgba(0, 0, 0, 0.6)";
    ctx.fillRect(px - 2 * dpr, 0, 4 * dpr, h);
    ctx.fillStyle = "#ffffff";
    ctx.fillRect(px - dpr, 0, 2 * dpr, h);
    ctx.beginPath();
    ctx.moveTo(px - 5 * dpr, 0);
    ctx.lineTo(px + 5 * dpr, 0);
    ctx.lineTo(px, 7 * dpr);
    ctx.closePath();
    ctx.fill();
  }
}
