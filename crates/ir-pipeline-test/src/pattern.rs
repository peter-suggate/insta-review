use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::Bytes;
use ir_core::{CaptureClock, CapturePipeline, PacketSink};
use ir_types::{Codec, CodecConfig, ColorInfo, EncodedPacket, PipelineConfig, PipelineError};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, UsageType,
};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;
use tracing::debug;

use ir_mux::h264 as annexb;

/// Software pipeline producing a synthetic test pattern:
/// - burned-in frame counter (top row) and millisecond timecode (below it) —
///   the ground truth for frame-accuracy tests,
/// - color bars (for color-matrix verification),
/// - a sweeping vertical bar (motion).
pub struct TestPatternPipeline {
    width: u32,
    height: u32,
    /// Pace generation to the wall clock (CLI); tests run flat out.
    realtime: bool,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl TestPatternPipeline {
    pub fn new(width: u32, height: u32, realtime: bool) -> Self {
        Self {
            width: width & !1, // encoder wants even dimensions
            height: height & !1,
            realtime,
            stop: Arc::new(AtomicBool::new(false)),
            join: None,
        }
    }
}

impl CapturePipeline for TestPatternPipeline {
    fn start(
        &mut self,
        cfg: PipelineConfig,
        clock: CaptureClock,
        sink: PacketSink,
    ) -> Result<(), PipelineError> {
        let (width, height, realtime) = (self.width, self.height, self.realtime);
        let stop = self.stop.clone();
        let join = std::thread::Builder::new()
            .name("ir-test-pipeline".into())
            .spawn(move || {
                if let Err(e) = run(width, height, realtime, cfg, clock, &sink, &stop) {
                    sink.error(e);
                }
            })
            .map_err(|e| PipelineError::Other(format!("spawn: {e}")))?;
        self.join = Some(join);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    fn request_keyframe(&mut self) {
        // GOP is already deterministic here; nothing to do.
    }
}

fn run(
    width: u32,
    height: u32,
    realtime: bool,
    cfg: PipelineConfig,
    clock: CaptureClock,
    sink: &PacketSink,
    stop: &AtomicBool,
) -> Result<(), PipelineError> {
    let fps = cfg.max_fps.max(1);
    let gop_frames = ((cfg.gop_seconds * fps as f32).round() as u64).max(1);
    let interval = Duration::from_secs_f64(1.0 / fps as f64);

    let enc_cfg = EncoderConfig::new()
        .bitrate(BitRate::from_bps(8_000_000))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .usage_type(UsageType::ScreenContentRealTime)
        .skip_frames(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(gop_frames as u32));
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), enc_cfg)
        .map_err(|e| PipelineError::Encode(e.to_string()))?;

    let mut configured = false;
    let mut frame_idx: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        let target = interval * frame_idx as u32;
        if realtime {
            let now = clock.now();
            if target > now {
                std::thread::sleep(target - now);
            }
        }
        let pts = target;

        let yuv = render_frame(width, height, frame_idx, pts);
        let yuv = YUVBuffer::from_vec(yuv, width as usize, height as usize);

        if frame_idx.is_multiple_of(gop_frames) {
            encoder.force_intra_frame();
        }
        let bitstream = encoder
            .encode(&yuv)
            .map_err(|e| PipelineError::Encode(e.to_string()))?;
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR);
        let annexb = bitstream.to_vec();
        let nals = annexb::split_nals(&annexb);

        if !configured {
            let sps = nals
                .iter()
                .find(|n| annexb::nal_type(n) == annexb::NAL_SPS)
                .ok_or_else(|| PipelineError::Encode("first frame missing SPS".into()))?;
            let pps = nals
                .iter()
                .find(|n| annexb::nal_type(n) == annexb::NAL_PPS)
                .ok_or_else(|| PipelineError::Encode("first frame missing PPS".into()))?;
            sink.configured(CodecConfig {
                codec: Codec::H264 {
                    avcc: Bytes::from(annexb::build_avcc_record(sps, pps)),
                },
                width,
                height,
                nominal_fps: fps,
                color: ColorInfo::Bt709Limited,
            });
            configured = true;
            debug!(width, height, fps, gop_frames, "test pipeline configured");
        }

        let data = annexb::to_avcc_payload(&nals);
        if !data.is_empty() {
            sink.packet(EncodedPacket {
                pts,
                duration: interval,
                keyframe,
                data: Bytes::from(data),
            });
        }
        frame_idx += 1;
    }
    Ok(())
}

/// Render one I420 frame. Layout (y from top):
/// - rows [0, 15%): color bars (8 bands)
/// - burned-in counter: frame index at (8,8), pts milliseconds at (8, 8+glyph)
/// - a white sweep bar moving 4 px/frame below the bars
fn render_frame(width: u32, height: u32, frame_idx: u64, pts: Duration) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut buf = vec![0u8; w * h + 2 * (w / 2) * (h / 2)];
    let (y_plane, uv) = buf.split_at_mut(w * h);
    let (u_plane, v_plane) = uv.split_at_mut((w / 2) * (h / 2));

    // Background: dark gray.
    y_plane.fill(40);
    u_plane.fill(128);
    v_plane.fill(128);

    // Color bars (75% SMPTE-ish, BT.709 limited-range values).
    const BARS: [(u8, u8, u8); 8] = [
        (180, 128, 128), // white
        (168, 44, 136),  // yellow
        (145, 147, 44),  // cyan
        (133, 63, 52),   // green
        (63, 193, 204),  // magenta
        (51, 109, 212),  // red
        (28, 212, 120),  // blue
        (16, 128, 128),  // black
    ];
    let bars_h = h * 15 / 100;
    for (i, &(y, u, v)) in BARS.iter().enumerate() {
        let x0 = w * i / 8;
        let x1 = w * (i + 1) / 8;
        for row in 0..bars_h {
            y_plane[row * w + x0..row * w + x1].fill(y);
        }
        for row in 0..bars_h / 2 {
            u_plane[row * (w / 2) + x0 / 2..row * (w / 2) + x1 / 2].fill(u);
            v_plane[row * (w / 2) + x0 / 2..row * (w / 2) + x1 / 2].fill(v);
        }
    }

    // Sweep bar: 4 px/frame, wraps; drawn below the color bars.
    let sweep_x = (frame_idx as usize * 4) % w.saturating_sub(4);
    for row in bars_h..h {
        y_plane[row * w + sweep_x..row * w + sweep_x + 4].fill(235);
    }

    // Burned-in numbers (white on black), drawn over everything.
    let scale = (w / 160).clamp(2, 8);
    draw_number(y_plane, w, h, 8, bars_h + 8, scale, frame_idx, 7);
    draw_number(
        y_plane,
        w,
        h,
        8,
        bars_h + 8 + 6 * scale + 4,
        scale,
        pts.as_millis() as u64,
        7,
    );

    buf
}

/// 3x5 digit font, one u16 per digit, row-major, MSB first per row.
const FONT: [u16; 10] = [
    0b111_101_101_101_111, // 0
    0b010_110_010_010_111, // 1
    0b111_001_111_100_111, // 2
    0b111_001_111_001_111, // 3
    0b101_101_111_001_001, // 4
    0b111_100_111_001_111, // 5
    0b111_100_111_101_111, // 6
    0b111_001_010_010_010, // 7
    0b111_101_111_101_111, // 8
    0b111_101_111_001_111, // 9
];

/// Draw `value` zero-padded to `digits` at (x, y), `scale` px per font pixel,
/// each digit in a black cell for contrast.
#[allow(clippy::too_many_arguments)]
fn draw_number(
    y_plane: &mut [u8],
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    scale: usize,
    value: u64,
    digits: u32,
) {
    let cell_w = 4 * scale; // 3 glyph cols + 1 spacing
    let cell_h = 6 * scale; // 5 glyph rows + 1 spacing
    for d in 0..digits {
        let digit = (value / 10u64.pow(digits - 1 - d)) % 10;
        let glyph = FONT[digit as usize];
        let cx = x + d as usize * cell_w;
        // Black cell background.
        for row in y..(y + cell_h).min(h) {
            for col in cx..(cx + cell_w).min(w) {
                y_plane[row * w + col] = 16;
            }
        }
        for gr in 0..5 {
            for gc in 0..3 {
                if glyph >> (14 - (gr * 3 + gc)) & 1 == 1 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            let py = y + gr * scale + sy;
                            let px = cx + gc * scale + sx;
                            if py < h && px < w {
                                y_plane[py * w + px] = 235;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Read back the burned-in frame index from a (possibly lossy-decoded) luma
/// plane. Inverse of `draw_number` at the renderer's layout. Returns `None`
/// if any digit cell doesn't match a glyph — a sign of severe corruption.
pub fn read_frame_index(y_plane: &[u8], stride: usize, width: usize, height: usize) -> Option<u64> {
    let bars_h = height * 15 / 100;
    let scale = (width / 160).clamp(2, 8);
    let (x, y) = (8usize, bars_h + 8);
    let cell_w = 4 * scale;
    let mut value: u64 = 0;
    for d in 0..7 {
        let cx = x + d * cell_w;
        let mut pattern: u16 = 0;
        for gr in 0..5 {
            for gc in 0..3 {
                // Sample the center of each glyph pixel block.
                let py = y + gr * scale + scale / 2;
                let px = cx + gc * scale + scale / 2;
                let lit = y_plane[py * stride + px] > 125;
                if lit {
                    pattern |= 1 << (14 - (gr * 3 + gc));
                }
            }
        }
        let digit = FONT.iter().position(|&g| g == pattern)?;
        value = value * 10 + digit as u64;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_back_matches_rendered_index() {
        for idx in [0u64, 1, 59, 1234, 999_9999] {
            let buf = render_frame(640, 360, idx, Duration::from_millis(idx * 16));
            let got = read_frame_index(&buf, 640, 640, 360);
            assert_eq!(got, Some(idx));
        }
    }

    #[test]
    fn renders_valid_i420_size() {
        let buf = render_frame(320, 240, 42, Duration::from_millis(700));
        assert_eq!(buf.len(), 320 * 240 * 3 / 2);
    }

    #[test]
    fn frames_differ_over_time() {
        let a = render_frame(320, 240, 1, Duration::from_millis(16));
        let b = render_frame(320, 240, 2, Duration::from_millis(33));
        assert_ne!(a, b);
    }
}
