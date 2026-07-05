// Frame-accurate WebCodecs player.
//
// Strategy: samples are AVCC chunks in one ArrayBuffer; every presentation
// decodes (at most) one GOP — from the nearest keyframe up to the target —
// into a frame cache keyed by sample index. Short GOPs (0.5–1 s) make that
// a few milliseconds on the hardware decoder. The cache holds a window of
// frames around the playhead; VideoFrames are closed on eviction.

const CACHE_RADIUS = 120; // frames kept around the playhead
const LOOKAHEAD = 24; // frames prefetched during playback

export class Player {
  constructor(canvas, { onPresent, onStateChange } = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");
    this.onPresent = onPresent || (() => {});
    this.onStateChange = onStateChange || (() => {});

    this.samples = [];
    this.data = null;
    this.cache = new Map(); // idx -> VideoFrame
    this.cur = -1;
    this.playing = false;
    this.rate = 1;
    this.playheadUs = 0;
    this.zoom = false;
    this.stretch = false;
    this.is43 = false;
    this.decodeBusy = null; // in-flight decode promise
    this.raf = 0;
    this.lastNow = 0;
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
    };
    this.is43 = Math.abs(codec.width / codec.height - 4 / 3) < 0.05;
    this.stretch = stretch43 && this.is43;

    const support = await VideoDecoder.isConfigSupported(this.decoderConfig);
    if (!support.supported) {
      throw new Error(`codec not supported here: ${this.decoderConfig.codec}`);
    }
  }

  reset() {
    this.pause();
    for (const frame of this.cache.values()) frame.close();
    this.cache.clear();
    this.samples = [];
    this.data = null;
    this.cur = -1;
    this.playheadUs = 0;
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

  // Decode so that frames [from..to] are cached. Serialized: one decode
  // pass at a time.
  async ensureFrames(from, to) {
    to = Math.min(to, this.samples.length - 1);
    from = Math.max(0, from);
    let missing = false;
    for (let i = from; i <= to; i++) {
      if (!this.cache.has(i)) {
        missing = true;
        break;
      }
    }
    if (!missing) return;
    while (this.decodeBusy) await this.decodeBusy;
    // Re-check after waiting.
    missing = false;
    for (let i = from; i <= to; i++) {
      if (!this.cache.has(i)) {
        missing = true;
        break;
      }
    }
    if (!missing) return;
    const start = this.keyIndexFor(from);
    this.evictAnchor = to;
    this.decodeBusy = this.decodeRange(start, to).finally(() => {
      this.decodeBusy = null;
    });
    await this.decodeBusy;
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

  decodeRange(start, end) {
    return new Promise((resolve, reject) => {
      this.lastDecodeStats = { outputs: 0, unmatched: 0 };
      const dec = new VideoDecoder({
        output: (frame) => {
          this.lastDecodeStats.outputs++;
          const idx = this.indexForOutputTime(frame.timestamp, start, end);
          if (idx >= 0 && !this.cache.has(idx)) {
            this.cache.set(idx, frame);
          } else {
            if (idx < 0) this.lastDecodeStats.unmatched++;
            frame.close();
          }
        },
        error: (e) => reject(e),
      });
      dec.configure(this.decoderConfig);
      for (let i = start; i <= end; i++) {
        const s = this.samples[i];
        dec.decode(
          new EncodedVideoChunk({
            type: s.key ? "key" : "delta",
            timestamp: s.tUs,
            data: new Uint8Array(this.data, s.offset, s.size),
          })
        );
      }
      dec.flush().then(
        () => {
          dec.close();
          this.evictFar();
          resolve();
        },
        (e) => {
          try {
            dec.close();
          } catch {}
          reject(e);
        }
      );
    });
  }

  evictFar() {
    // Before the first present (cur == -1) anchor on the frame we just
    // decoded toward, or eviction would discard it immediately.
    const anchor = this.cur >= 0 ? this.cur : (this.evictAnchor ?? 0);
    for (const [idx, frame] of this.cache) {
      if (Math.abs(idx - anchor) > CACHE_RADIUS) {
        frame.close();
        this.cache.delete(idx);
      }
    }
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
    this.onPresent(idx, this.playheadUs);
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
    await this.ensureFrames(idx, idx);
    this.present(idx);
    this.playheadUs = us;
  }

  async step(dir) {
    this.pause();
    const target = Math.max(0, Math.min(this.cur + dir, this.samples.length - 1));
    if (target === this.cur) return;
    await this.ensureFrames(target, target);
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
    this.playheadUs += dtUs;

    if (this.playheadUs >= this.durationUs()) {
      this.playheadUs = this.durationUs();
      const lastIdx = this.samples.length - 1;
      if (this.cache.has(lastIdx)) this.present(lastIdx);
      this.pause();
      return;
    }

    const idx = this.indexForTime(this.playheadUs);
    if (idx !== this.cur && this.cache.has(idx)) {
      const continuous = this.playheadUs;
      this.present(idx);
      // present() snaps playheadUs to the frame pts; restore the
      // continuous value for smooth pacing.
      this.playheadUs = Math.max(continuous, this.samples[idx].tUs);
    }
    // Prefetch ahead without blocking the loop.
    if (!this.decodeBusy) {
      const ahead = Math.min(idx + LOOKAHEAD, this.samples.length - 1);
      let missing = false;
      for (let i = idx; i <= ahead; i++) {
        if (!this.cache.has(i)) {
          missing = true;
          break;
        }
      }
      if (missing) this.ensureFrames(idx, ahead).catch(() => {});
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
