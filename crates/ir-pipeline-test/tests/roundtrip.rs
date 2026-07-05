//! M0 exit criterion, fully automated: run the engine with the test-pattern
//! pipeline, snapshot the ring, mux to MP4, then DECODE the result and read
//! the burned-in frame counters back. Gapless counters prove no frame was
//! silently dropped anywhere in capture → ring → snapshot → mux.

use std::time::Duration;

use ir_core::Engine;
use ir_pipeline_test::{annexb, read_frame_index, TestPatternPipeline};
use ir_types::{Codec, PipelineConfig};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

#[test]
fn engine_ring_mux_decode_roundtrip_is_gapless() {
    let (width, height, fps) = (640u32, 360u32, 60u32);
    // Not realtime: generates a 60 fps virtual timeline as fast as it can.
    let pipeline = Box::new(TestPatternPipeline::new(width, height, false));
    let cfg = PipelineConfig {
        max_fps: fps,
        gop_seconds: 0.5,
        ..PipelineConfig::default()
    };
    let handle =
        Engine::start(pipeline, cfg, Duration::from_secs(6), usize::MAX).expect("engine starts");

    // Wait until ~12 virtual seconds have been pushed (ring retains 6).
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let stats = handle.stats().expect("engine alive");
        if stats.frames_pushed >= (12 * fps) as u64 {
            break;
        }
    }

    let clip = handle
        .snapshot(Duration::from_secs(5))
        .expect("snapshot available");
    handle.stop();

    // Structural checks on the clip itself.
    assert!(clip.samples[0].keyframe, "clip starts on a keyframe");
    assert!(
        clip.meta.frame_pts.windows(2).all(|w| w[1] > w[0]),
        "pts strictly increasing"
    );
    let span = *clip.meta.frame_pts.last().unwrap();
    assert!(span >= 5.0 - 0.6, "covers the window (got {span:.2} s)");

    // Mux, then decode the mux output and read counters back.
    let mp4 = ir_mux::mux_h264(&clip.codec, &clip.samples).expect("mux");
    assert!(mp4.len() > 10_000);

    let Codec::H264 { avcc } = &clip.codec.codec;
    let mut annexb_stream = annexb::parameter_sets_annexb(avcc);
    for sample in &clip.samples {
        annexb_stream.extend_from_slice(&annexb::avcc_to_annexb(&sample.data));
    }

    let mut decoder = Decoder::new().expect("decoder");
    let mut counters: Vec<u64> = Vec::new();
    for nal in nal_units(&annexb_stream) {
        if let Ok(Some(frame)) = decoder.decode(nal) {
            let (w, h) = frame.dimensions();
            let stride = frame.strides().0;
            counters.push(
                read_frame_index(frame.y(), stride, w, h)
                    .expect("counter readable in decoded frame"),
            );
        }
    }
    for frame in decoder.flush_remaining().expect("flush") {
        let (w, h) = frame.dimensions();
        let stride = frame.strides().0;
        counters.push(
            read_frame_index(frame.y(), stride, w, h).expect("counter readable in flushed frame"),
        );
    }

    assert_eq!(
        counters.len(),
        clip.samples.len(),
        "decoded frame count matches muxed sample count"
    );
    for pair in counters.windows(2) {
        assert_eq!(pair[1], pair[0] + 1, "gapless frame counters");
    }
}

/// Split an Annex-B stream into individual NAL units *including* their start
/// codes (openh264's decoder wants one NAL per call).
fn nal_units(annexb: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 {
            if annexb[i + 2] == 1 {
                starts.push(i);
                i += 3;
                continue;
            }
            if i + 4 <= annexb.len() && annexb[i + 2] == 0 && annexb[i + 3] == 1 {
                starts.push(i);
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &s)| {
            let end = starts.get(n + 1).copied().unwrap_or(annexb.len());
            &annexb[s..end]
        })
        .collect()
}
