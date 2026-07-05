//! Software test pipeline: synthetic frames with a burned-in frame counter,
//! millisecond timecode, color bars, and a moving sweep bar, encoded to
//! H.264 with openh264. Runs anywhere (CI included) and is the ground truth
//! for verifying frame-accurate capture, muxing, and player stepping.

mod pattern;

pub use pattern::{read_frame_index, TestPatternPipeline};
