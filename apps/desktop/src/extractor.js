// Frame extraction for coaching analysis. Decodes the staged clip's AVCC
// samples on a private VideoDecoder — the review player's cache and
// playhead are untouched — and delivers full-resolution JPEGs to Rust via
// raw-payload invokes (bytes as body, timestamp in a `t-us` header).

const { invoke } = window.__TAURI__.core;

// Match a decoder-output timestamp to a requested one, tolerating the
// sub-millisecond rounding some decoders introduce (same trick as player.js).
const msKey = (us) => Math.round(us / 1000);

export async function extractFrames({ samples, buffer, decoderConfig, wants }) {
  if (!wants.length) return 0;

  // Nearest sample per requested timestamp; key the lookup by the sample's
  // own tUs (that's what the decoder echoes back).
  const bySampleTs = new Map(); // msKey(sample.tUs) -> plan tUs
  const indices = [];
  for (const w of wants) {
    const idx = nearestIndex(samples, w.tUs);
    const key = msKey(samples[idx].tUs);
    if (!bySampleTs.has(key)) {
      bySampleTs.set(key, w.tUs);
      indices.push(idx);
    }
  }
  indices.sort((a, b) => a - b);

  // One pass from the keyframe before the first target through the last
  // target — the default plan spans ~2 s and GOPs are 0.5 s, so decoding
  // straight through is cheaper than per-GOP passes.
  let start = indices[0];
  while (start > 0 && !samples[start].key) start--;
  const end = indices[indices.length - 1];

  const captured = []; // [planTUs, VideoFrame], kept open until encoded
  await new Promise((resolve, reject) => {
    const dec = new VideoDecoder({
      output: (frame) => {
        const planTUs = bySampleTs.get(msKey(frame.timestamp));
        if (planTUs !== undefined) captured.push([planTUs, frame]);
        else frame.close();
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

  try {
    for (const [planTUs, frame] of captured) {
      const canvas = new OffscreenCanvas(frame.displayWidth, frame.displayHeight);
      canvas.getContext("2d").drawImage(frame, 0, 0);
      const blob = await canvas.convertToBlob({ type: "image/jpeg", quality: 0.9 });
      const bytes = new Uint8Array(await blob.arrayBuffer());
      await invoke("analysis_frame", bytes, {
        headers: { "t-us": String(planTUs) },
      });
    }
  } finally {
    for (const [, frame] of captured) {
      try {
        frame.close();
      } catch {}
    }
  }
  return captured.length;
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
