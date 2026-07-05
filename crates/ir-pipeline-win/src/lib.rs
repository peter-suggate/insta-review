//! Windows capture pipeline: Windows.Graphics.Capture display capture →
//! GPU BGRA→NV12 conversion (ID3D11VideoProcessor) → hardware H.264
//! encoder MFT (NVENC/AMF/QSV via Media Foundation), zero CPU copies until
//! the encoded bitstream. Emits AVCC packets on the engine's `PacketSink`.
//!
//! The crate compiles to an empty shell on non-Windows targets so the
//! workspace builds everywhere.

#[cfg(windows)]
mod convert;
#[cfg(windows)]
mod mf_encoder;
#[cfg(windows)]
mod pipeline;

#[cfg(windows)]
pub use pipeline::WindowsPipeline;
