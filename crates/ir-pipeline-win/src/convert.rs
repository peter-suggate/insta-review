//! GPU BGRA→NV12 conversion via the fixed-function ID3D11VideoProcessor.
//! Full-range sRGB input → BT.709 limited-range NV12 output, all on the GPU.

use ir_types::PipelineError;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

fn err(what: &str, e: windows::core::Error) -> PipelineError {
    PipelineError::Capture(format!("{what}: {e}"))
}

/// Round-robin pool size for NV12 output textures. The encoder consumes
/// frames within a few frame times; 8 gives comfortable slack at 60 fps.
const POOL: usize = 8;

pub struct Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    width: u32,
    height: u32,
}

impl Converter {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, PipelineError> {
        // NV12 requires even dimensions.
        let (width, height) = (width & !1, height & !1);
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|e| err("device has no video interface", e))?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|e| err("context has no video interface", e))?;

        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let enumerator = unsafe {
            video_device
                .CreateVideoProcessorEnumerator(&desc)
                .map_err(|e| err("CreateVideoProcessorEnumerator", e))?
        };
        let processor = unsafe {
            video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|e| err("CreateVideoProcessor", e))?
        };

        // Explicit color spaces: full-range sRGB in, BT.709 studio NV12 out.
        if let Ok(vc1) = video_context.cast::<ID3D11VideoContext1>() {
            unsafe {
                vc1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                vc1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
            }
        }

        // NV12 output texture pool + views.
        let tex_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut outputs = Vec::with_capacity(POOL);
        for _ in 0..POOL {
            let texture = unsafe {
                let mut t = None;
                device
                    .CreateTexture2D(&tex_desc, None, Some(&mut t))
                    .map_err(|e| err("CreateTexture2D(NV12)", e))?;
                t.ok_or_else(|| PipelineError::Capture("no NV12 texture".into()))?
            };
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let view = unsafe {
                let mut v = None;
                video_device
                    .CreateVideoProcessorOutputView(&texture, &enumerator, &view_desc, Some(&mut v))
                    .map_err(|e| err("CreateVideoProcessorOutputView", e))?;
                v.ok_or_else(|| PipelineError::Capture("no output view".into()))?
            };
            outputs.push((texture, view));
        }

        Ok(Self {
            video_device,
            video_context,
            processor,
            enumerator,
            outputs,
            next: 0,
            width,
            height,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Convert one BGRA frame into the next pooled NV12 texture.
    /// Must be called from the capture thread (owns the device context).
    pub fn convert(&mut self, bgra: &ID3D11Texture2D) -> Result<ID3D11Texture2D, PipelineError> {
        let video_device_ctx = &self.video_context;
        let (out_tex, out_view) = &self.outputs[self.next];
        self.next = (self.next + 1) % self.outputs.len();

        let in_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        // Input views are cheap; created per frame because WGC rotates its
        // frame-pool textures.
        let in_view = unsafe {
            let mut v = None;
            self.video_device
                .CreateVideoProcessorInputView(bgra, &self.enumerator, &in_view_desc, Some(&mut v))
                .map_err(|e| err("CreateVideoProcessorInputView", e))?;
            v.ok_or_else(|| PipelineError::Capture("no input view".into()))?
        };

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(in_view)),
            ..Default::default()
        };

        let result = unsafe {
            video_device_ctx
                .VideoProcessorBlt(&self.processor, out_view, 0, std::slice::from_ref(&stream))
                .map_err(|e| err("VideoProcessorBlt", e))
        };
        // The struct won't release the ManuallyDrop'd view on drop; do it
        // explicitly or we leak one input view per frame.
        unsafe { std::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        result?;
        Ok(out_tex.clone())
    }
}
