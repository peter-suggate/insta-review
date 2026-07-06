// Frame-accurate WebCodecs player.
//
// Two hardware realities shape this design (both observed on AMD):
//  1. flush() intermittently never resolves — so nothing ever waits on
//     it. The clip is fed as ONE sequential stream per position; random
//     access restarts the stream at a keyframe (~tens of ms).
//  2. Hardware decoders emit frames from a small fixed texture pool.
//     Holding VideoFrames alive exhausts the pool and the decoder
//     silently stops emitting. So we retain only the presented frame
//     plus a small ahead-buffer and close everything else immediately;
//     backward steps re-decode from the previous keyframe instead of
//     hitting a big cache.

const MAX_AHEAD = 8; // decoded frames retained ahead of the playhead
const QUEUE_DEPTH = 16; // max undecoded chunks queued in the decoder
const FEED_WINDOW = 24; // feed at most this far past the anchor
const SEEK_FWD_STREAM = 60; // reuse the stream for forward jumps up to this

export class Player {
  constructor(canvas, { onPresent, onStateChange, onSlow } = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");
    this.onPresent = onPresent || (() => {});
    this.onStateChange = onStateChange || (() => {});
    this.onSlow = onSlow || null;

    this.samples = [];
    this.data = null;
    this.cache = new Map(); // idx -> VideoFrame (tiny working set!)
    this.cur = -1;
    this.playing = false;
    this.rate = 1;
    this.playheadUs = 0;
    this.zoom = false;
    this.stretch = false;
    this.is43 = false;
    this.lastNow = 0;

    // Streaming-decode state.
    this.dec = null;
    this.streamKey = 0; // sample index the stream started at (a keyframe)
    this.feedIdx = 0; // next sample index to feed
    this.flushSent = false;
    this.waiters = new Set(); // {idx, res, rej, timer}
    this.waitIdx = null; // frame a caller is blocked on
    this.holdIdx = null; // frame a caller will read back (filmstrip)
    this.gapFetch = null; // single-flight gap recovery during playback
    this.lastDecodeStats = { outputs: 0, unmatched: 0 };
    this.passLog = []; // kept for telemetry compatibility
  }

  async load({ codec, samples, buffer, stretch43 }) {
    this.reset();
    this.samples = samples;
    this.data = buffer;
    this.decoderConfig = {
      codec: codec.codecString,
      description: b64ToBytes(codec.avccB64),
      codedWidth: codec.width,
      codedHeight: codec.height,
      optimizeForLatency: true,
      // Without an explicit ask Chromium may pick software decode, which
      // tops out well under 60 fps at 1440p. Falls back below (and on
      // decoder errors) if hardware isn't actually available.
      hardwareAcceleration: "prefer-hardware",
    };
    this.is43 = Math.abs(codec.width / codec.height - 4 / 3) < 0.05;
    this.stretch = stretch43 && this.is43;

    let support = await VideoDecoder.isConfigSupported(this.decoderConfig);
    if (!support.supported) {
      delete this.decoderConfig.hardwareAcceleration;
      support = await VideoDecoder.isConfigSupported(this.decoderConfig);
    }
    if (!support.supported) {
      throw new Error(`codec not supported here: ${this.decoderConfig.codec}`);
    }
    this.lastDecodeStats = { outputs: 0, unmatched: 0 };
    this.passLog = [];
    this.newStream(0);
  }

  reset() {
    this.pause();
    this.closeDecoder();
    for (const w of this.waiters) {
      clearTimeout(w.timer);
      w.rej(new Error("player reset"));
    }
    this.waiters.clear();
    for (const frame of this.cache.values()) frame.close();
    this.cache.clear();
    this.samples = [];
    this.data = null;
    this.cur = -1;
    this.playheadUs = 0;
    this.waitIdx = null;
    this.holdIdx = null;
  }

  closeDecoder() {
    if (this.dec) {
      try {
        this.dec.close();
      } catch {}
      this.dec = null;
    }
  }

  // Start (or restart) the sequential stream at a keyframe.
  newStream(startKey) {
    this.closeDecoder();
    const dec = new VideoDecoder({
      output: (frame) => this.onDecodeOutput(frame),
      error: () => {
        // Surfaced via waiter timeouts (which restart the stream). If
        // forcing hardware broke decode, later streams go software.
        if (this.decoderConfig?.hardwareAcceleration) {
          delete this.decoderConfig.hardwareAcceleration;
        }
      },
    });
    dec.configure(this.decoderConfig);
    dec.ondequeue = () => this.pump();
    this.dec = dec;
    this.streamKey = startKey;
    this.feedIdx = startKey;
    this.flushSent = false;
    this.pump();
  }

  // The position decoding should serve: an explicitly awaited frame wins,
  // else just past the playhead.
  wantIdx() {
    if (this.waitIdx != null) return this.waitIdx;
    if (this.cur >= 0) return this.cur + (this.playing ? 1 : 0);
    return this.streamKey;
  }

  // Feed the decoder: sequential, bounded so the number of outstanding
  // decoded-but-unconsumed frames stays inside the decoder's pool.
  pump() {
    const dec = this.dec;
    if (!dec || dec.state !== "configured" || !this.data) return;
    const anchor = Math.max(this.wantIdx(), this.streamKey);
    while (
      this.feedIdx < this.samples.length &&
      dec.decodeQueueSize < QUEUE_DEPTH &&
      this.feedIdx - anchor < FEED_WINDOW
    ) {
      const s = this.samples[this.feedIdx];
      try {
        dec.decode(
          new EncodedVideoChunk({
            type: s.key ? "key" : "delta",
            timestamp: s.tUs,
            data: new Uint8Array(this.data, s.offset, s.size),
          })
        );
      } catch {
        return; // decoder died; a waiter timeout will restart the stream
      }
      this.feedIdx++;
    }
    // End of clip: one fire-and-forget flush pushes out the decoder's
    // final buffered frames. If it wedges, waiter timeouts recover.
    if (this.feedIdx >= this.samples.length && !this.flushSent) {
      this.flushSent = true;
      dec.flush().catch(() => {});
    }
  }

  // Retain a frame only if it's in the tiny window we're about to
  // present (or explicitly held); everything else goes straight back to
  // the decoder's pool.
  keepable(idx) {
    if (idx === this.cur || idx === this.holdIdx) return true;
    const want = this.wantIdx();
    return idx >= want && idx - want <= MAX_AHEAD;
  }

  onDecodeOutput(frame) {
    this.lastDecodeStats.outputs++;
    const idx = this.indexForOutputTime(
      frame.timestamp,
      0,
      this.samples.length - 1
    );
    if (idx >= 0 && !this.cache.has(idx) && this.keepable(idx)) {
      this.cache.set(idx, frame);
    } else {
      if (idx < 0) this.lastDecodeStats.unmatched++;
      frame.close();
    }
    this.evict();
    for (const w of [...this.waiters]) {
      if (this.cache.has(w.idx)) {
        clearTimeout(w.timer);
        this.waiters.delete(w);
        w.res();
      }
    }
    this.pump();
  }

  evict() {
    for (const [idx, frame] of this.cache) {
      if (!this.keepable(idx)) {
        frame.close();
        this.cache.delete(idx);
      }
    }
  }

  waitForFrame(idx, ms) {
    if (this.cache.has(idx)) return Promise.resolve();
    return new Promise((res, rej) => {
      const w = { idx, res, rej };
      w.timer = setTimeout(() => {
        this.waiters.delete(w);
        rej(new Error(`frame ${idx} not decoded within ${ms} ms`));
      }, ms);
      this.waiters.add(w);
    });
  }

  // Ensure one frame is decoded and cached (seek / step / filmstrip).
  async ensureFrame(idx) {
    if (!this.samples.length) return;
    idx = Math.max(0, Math.min(idx, this.samples.length - 1));
    if (this.cache.has(idx)) return;
    const t0 = performance.now();
    let lastErr = null;
    for (let attempt = 0; attempt < 3; attempt++) {
      // Reuse the live stream only for frames a short way ahead of the
      // feed point; anything behind or far ahead restarts at a keyframe.
      const streamUseful =
        attempt === 0 &&
        this.dec &&
        this.dec.state === "configured" &&
        idx >= this.feedIdx - (this.dec.decodeQueueSize || 0) &&
        idx - this.feedIdx < SEEK_FWD_STREAM;
      if (!streamUseful) this.newStream(this.keyIndexFor(idx));
      this.waitIdx = idx;
      this.evict(); // want moved: release frames the pool needs back
      this.pump();
      try {
        await this.waitForFrame(idx, 1600);
        lastErr = null;
        break;
      } catch (e) {
        lastErr = e; // wedged or dropped — restart the stream and retry
      } finally {
        if (this.waitIdx === idx) this.waitIdx = null;
      }
    }
    const ms = performance.now() - t0;
    if (ms > 1000) {
      this.onSlow?.(
        `ensureFrame(${idx}) took ${ms.toFixed(0)} ms ` +
          `(stream ${this.streamKey}..${this.feedIdx}, ` +
          `q=${this.dec ? this.dec.decodeQueueSize : "-"}, ` +
          `cached=${this.cache.size}, outputs=${this.lastDecodeStats.outputs})`
      );
    }
    if (lastErr) throw lastErr;
  }

  durationUs() {
    const last = this.samples[this.samples.length - 1];
    return last ? last.tUs : 0;
  }

  indexForTime(us) {
    // last sample with tUs <= us
    let lo = 0,
      hi = this.samples.length - 1,
      ans = 0;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (this.samples[mid].tUs <= us) {
        ans = mid;
        lo = mid + 1;
      } else hi = mid - 1;
    }
    return ans;
  }

  keyIndexFor(i) {
    for (let k = Math.min(i, this.samples.length - 1); k >= 0; k--) {
      if (this.samples[k].key) return k;
    }
    return 0;
  }

  // Map a decoder-output timestamp back to a sample index, tolerating
  // sub-millisecond rounding differences some decoders introduce.
  indexForOutputTime(tUs, start, end) {
    let lo = start,
      hi = end,
      best = -1,
      bestDist = Infinity;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      const d = this.samples[mid].tUs - tUs;
      if (Math.abs(d) < bestDist) {
        bestDist = Math.abs(d);
        best = mid;
      }
      if (d < 0) lo = mid + 1;
      else hi = mid - 1;
    }
    return bestDist <= 1000 ? best : -1; // within 1 ms
  }

  currentFrame() {
    return this.cache.get(this.cur) || null;
  }

  present(idx) {
    const frame = this.cache.get(idx);
    if (!frame) return false;
    this.cur = idx;
    this.playheadUs = this.samples[idx].tUs;
    this.draw(frame);
    // Frames behind the new playhead go back to the decoder pool now —
    // the canvas keeps the pixels; only `cur` stays for redraw.
    for (const [i, f] of this.cache) {
      if (i < idx && i !== this.holdIdx) {
        f.close();
        this.cache.delete(i);
      }
    }
    this.onPresent(idx, this.playheadUs);
    this.pump(); // playhead moved — the stream may feed further ahead
    return true;
  }

  redraw() {
    const frame = this.cache.get(this.cur);
    if (frame) this.draw(frame);
  }

  draw(frame) {
    const canvas = this.canvas;
    const dpr = window.devicePixelRatio || 1;
    const cssW = canvas.clientWidth,
      cssH = canvas.clientHeight;
    if (canvas.width !== cssW * dpr || canvas.height !== cssH * dpr) {
      canvas.width = cssW * dpr;
      canvas.height = cssH * dpr;
    }
    const ctx = this.ctx;
    const cw = canvas.width,
      ch = canvas.height;
    ctx.fillStyle = "#000";
    ctx.fillRect(0, 0, cw, ch);

    let ar = frame.displayWidth / frame.displayHeight;
    if (this.stretch) ar = 16 / 9;
    let dw = cw,
      dh = cw / ar;
    if (dh > ch) {
      dh = ch;
      dw = ch * ar;
    }
    if (this.zoom) {
      dw *= 2;
      dh *= 2;
    }
    const dx = (cw - dw) / 2,
      dy = (ch - dh) / 2;
    ctx.drawImage(frame, dx, dy, dw, dh);
  }

  async seekToUs(us) {
    us = Math.max(0, Math.min(us, this.durationUs()));
    const idx = this.indexForTime(us);
    await this.ensureFrame(idx);
    this.present(idx);
    this.playheadUs = us;
  }

  async step(dir) {
    this.pause();
    const target = Math.max(
      0,
      Math.min(this.cur + dir, this.samples.length - 1)
    );
    if (target === this.cur) return;
    await this.ensureFrame(target);
    this.present(target);
  }

  play() {
    if (this.playing || !this.samples.length) return;
    if (this.cur >= this.samples.length - 1) {
      // restart from the top when playing at the end
      this.playheadUs = 0;
      this.cur = -1;
    }
    this.playing = true;
    this.lastNow = performance.now();
    this.onStateChange();
    // A timer, not requestAnimationFrame: rAF is throttled/suspended for
    // occluded or locked-screen windows, which would freeze playback.
    this.timer = setInterval(() => this.tick(performance.now()), 8);
  }

  tick(now) {
    if (!this.playing) return;
    const dtUs = (now - this.lastNow) * 1000 * this.rate;
    this.lastNow = now;
    let target = Math.max(0, this.playheadUs + dtUs);

    // Buffer, don't skip: advance only across decoded frames. If the next
    // frame isn't cached yet, hold the playhead at the gap — a decode
    // hiccup pauses the video briefly instead of silently dropping the
    // rest of the clip.
    const wantIdx = this.indexForTime(Math.min(target, this.durationUs()));
    let presentIdx = -1;
    let gap = -1;
    for (let i = Math.max(0, this.cur + 1); i <= wantIdx; i++) {
      if (this.cache.has(i)) presentIdx = i;
      else {
        gap = i;
        break;
      }
    }
    if (gap >= 0) {
      target = Math.min(target, Math.max(0, this.samples[gap].tUs - 1));
    }

    if (presentIdx >= 0 && presentIdx !== this.cur) {
      this.present(presentIdx);
      // present() snaps playheadUs to the frame pts; restore the
      // continuous value for smooth pacing.
      this.playheadUs = Math.max(target, this.samples[presentIdx].tUs);
    } else {
      this.playheadUs = target;
    }

    // Finished only once the LAST frame has actually been shown.
    const lastIdx = this.samples.length - 1;
    if (this.playheadUs >= this.durationUs() && this.cur >= lastIdx) {
      this.playheadUs = this.durationUs();
      this.pause();
      return;
    }

    // Keep the stream feeding ahead of the playhead; recover gaps (a gap
    // means the stream is behind or wedged — ensureFrame restarts it).
    if (gap >= 0) {
      if (!this.gapFetch) {
        this.gapFetch = this.ensureFrame(gap)
          .catch(() => {})
          .finally(() => (this.gapFetch = null));
      }
    } else {
      this.pump();
    }
  }

  pause() {
    if (this.playing) {
      this.playing = false;
      clearInterval(this.timer);
      this.onStateChange();
    }
  }

  toggle() {
    if (this.playing) this.pause();
    else this.play();
  }

  setRate(rate) {
    this.rate = rate;
    this.onStateChange();
  }

  toggleZoom() {
    this.zoom = !this.zoom;
    this.redraw();
    this.onStateChange();
  }

  toggleStretch() {
    if (!this.is43) return;
    this.stretch = !this.stretch;
    this.redraw();
    this.onStateChange();
  }
}

export function b64ToBytes(b64) {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
