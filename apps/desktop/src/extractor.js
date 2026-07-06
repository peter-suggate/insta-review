// Frame extraction for coaching analysis. Decodes the staged clip's AVCC
// samples on a private VideoDecoder — the review player's cache and
// playhead are untouched — and delivers frames to Rust via raw-payload
// invokes (bytes as body, metadata in headers).
//
// Two products per plan entry:
//   wantJpeg — full-res JPEG evidence file for the LLM,
//   wantRaw  — downscaled grayscale for the CV pass (dense, hundreds of
//              frames), sent as `kind: luma` with w/h headers.
//
// VideoFrames are consumed synchronously inside the decoder callback and
// closed immediately — a dense plan must never hold hundreds of GPU frames.

const { invoke } = window.__TAURI__.core;

const LUMA_W = 480;
const LUMA_H = 270;

// Match a decoder-output timestamp to a requested one, tolerating the
// sub-millisecond rounding some decoders introduce (same trick as player.js).
const msKey = (us) => Math.round(us / 1000);

export async function extractFrames({ samples, buffer, decoderConfig, wants }) {
  if (!wants.length) return 0;

  // Nearest sample per requested timestamp, keyed by the sample's own tUs
  // (that's what the decoder echoes back).
  const bySampleTs = new Map(); // msKey(sample.tUs) -> plan entry
  const indices = [];
  for (const w of wants) {
    const idx = nearestIndex(samples, w.tUs);
    const key = msKey(samples[idx].tUs);
    if (!bySampleTs.has(key)) {
      bySampleTs.set(key, w);
      indices.push(idx);
    }
  }
  indices.sort((a, b) => a - b);

  // One pass from the keyframe before the first target through the last.
  // Dense CV plans cover the span anyway; GOPs are 0.5 s so the lead-in
  // waste for sparse plans is small.
  let start = indices[0];
  while (start > 0 && !samples[start].key) start--;
  const end = indices[indices.length - 1];

  const lumaCanvas = new OffscreenCanvas(LUMA_W, LUMA_H);
  const lumaCtx = lumaCanvas.getContext("2d", { willReadFrequently: true });
  const jpegJobs = []; // [planTUs, OffscreenCanvas] — encoded after decode
  let sendChain = Promise.resolve();
  let sent = 0;
  let sendError = null;

  const send = (bytes, headers) => {
    // Serialize invokes; capture the first failure (don't fire hundreds
    // of doomed calls after a real error).
    sendChain = sendChain.then(() => {
      if (sendError) return;
      return invoke("analysis_frame", bytes, { headers }).then(
        () => sent++,
        (e) => (sendError = e)
      );
    });
  };

  await new Promise((resolve, reject) => {
    const dec = new VideoDecoder({
      output: (frame) => {
        const want = bySampleTs.get(msKey(frame.timestamp));
        if (want) {
          const tUs = want.tUs;
          if (want.wantRaw) {
            lumaCtx.drawImage(frame, 0, 0, LUMA_W, LUMA_H);
            const rgba = lumaCtx.getImageData(0, 0, LUMA_W, LUMA_H).data;
            const luma = new Uint8Array(LUMA_W * LUMA_H);
            for (let i = 0; i < luma.length; i++) {
              const o = i * 4;
              // Rec.601 integer luma — cheap and plenty for correlation.
              luma[i] = (77 * rgba[o] + 150 * rgba[o + 1] + 29 * rgba[o + 2]) >> 8;
            }
            send(luma, {
              kind: "luma",
              "t-us": String(tUs),
              w: String(LUMA_W),
              h: String(LUMA_H),
            });
          }
          if (want.wantJpeg) {
            const full = new OffscreenCanvas(frame.displayWidth, frame.displayHeight);
            full.getContext("2d").drawImage(frame, 0, 0);
            jpegJobs.push([tUs, full]);
          }
        }
        frame.close();
      },
      error: reject,
    });
    dec.configure(decoderConfig);
    for (let i = start; i <= end; i++) {
      const s = samples[i];
      dec.decode(
        new EncodedVideoChunk({
          type: s.key ? "key" : "delta",
          timestamp: s.tUs,
          data: new Uint8Array(buffer, s.offset, s.size),
        })
      );
    }
    dec.flush().then(
      () => {
        dec.close();
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

  for (const [tUs, canvas] of jpegJobs) {
    const blob = await canvas.convertToBlob({ type: "image/jpeg", quality: 0.9 });
    send(new Uint8Array(await blob.arrayBuffer()), { "t-us": String(tUs) });
  }
  await sendChain;
  if (sendError) throw new Error(`frame delivery failed: ${sendError}`);
  return sent;
}

function nearestIndex(samples, us) {
  let lo = 0,
    hi = samples.length - 1,
    best = 0,
    bestDist = Infinity;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const d = samples[mid].tUs - us;
    if (Math.abs(d) < bestDist) {
      bestDist = Math.abs(d);
      best = mid;
    }
    if (d < 0) lo = mid + 1;
    else hi = mid - 1;
  }
  return best;
}
