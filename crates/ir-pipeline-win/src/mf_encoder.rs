//! Hardware H.264 encoding via an async Media Foundation transform (MFT).
//! Vendor-agnostic: NVENC, AMD AMF and Intel QSV all register hardware
//! encoder MFTs. Input is D3D11 NV12 textures (zero-copy via the DXGI
//! device manager); output is the H.264 elementary stream, which we
//! repackage as AVCC packets for the ring.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::Bytes;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use ir_core::PacketSink;
use ir_mux::h264;
use ir_types::{Codec, CodecConfig, ColorInfo, EncodedPacket, PipelineError};
use tracing::{debug, warn};
use windows::core::Interface;
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Multithread, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_Quality, eAVEncH264VProfile_High, CODECAPI_AVEncCommonQuality,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFSample, IMFTransform,
    METransformHaveOutput, METransformNeedInput, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample, MFMediaType_Video, MFStartup,
    MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MF_API_VERSION, MF_EVENT_FLAG_NO_WAIT, MF_E_NO_EVENTS_AVAILABLE,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE, MF_SDK_VERSION,
    MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use crate::pipeline::EncoderFeed;

fn err(what: &str, e: windows::core::Error) -> PipelineError {
    PipelineError::Encode(format!("{what}: {e}"))
}

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    let inner = unsafe { &mut var.Anonymous.Anonymous };
    inner.vt = VT_UI4;
    inner.Anonymous.ulVal = v;
    var
}

fn variant_bool(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    let inner = unsafe { &mut var.Anonymous.Anonymous };
    inner.vt = VT_BOOL;
    inner.Anonymous.boolVal = VARIANT_BOOL(if v { -1 } else { 0 });
    var
}

fn mf_startup() -> Result<(), PipelineError> {
    static ONCE: std::sync::Once = std::sync::Once::new();
    static mut RESULT: Option<windows::core::Error> = None;
    unsafe {
        ONCE.call_once(|| {
            if let Err(e) = MFStartup((MF_SDK_VERSION << 16) | MF_API_VERSION, MFSTARTUP_FULL) {
                RESULT = Some(e);
            }
        });
        #[allow(static_mut_refs)]
        match &RESULT {
            Some(e) => Err(err("MFStartup", e.clone())),
            None => Ok(()),
        }
    }
}

/// One frame handed from the capture thread to the encoder thread.
pub struct EncodeJob {
    pub texture: ID3D11Texture2D,
    pub pts: Duration,
    pub force_keyframe: bool,
}

// SAFETY: the texture belongs to a multithread-protected D3D11 device and
// COM here runs in the MTA; windows-rs interface wrappers are conservatively
// !Send but cross-thread use of these objects is defined behavior.
unsafe impl Send for EncodeJob {}

pub struct MfEncoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: ICodecAPI,
    _device_manager: IMFDXGIDeviceManager,
    sink: PacketSink,
    nominal_frame_100ns: i64,
    /// NeedInput credits granted by the MFT that we haven't consumed.
    input_credits: usize,
    pending: VecDeque<EncodeJob>,
    configured: bool,
    width: u32,
    height: u32,
    fps: u32,
}

// SAFETY: see `EncodeJob` — the encoder is constructed on the capture
// thread and then owned exclusively by the encoder thread; MF transforms
// are free-threaded (MTA) objects.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device: &ID3D11Device,
        sink: PacketSink,
        width: u32,
        height: u32,
        fps: u32,
        gop_frames: u32,
        quality: u32,
    ) -> Result<Self, PipelineError> {
        mf_startup()?;

        // The MFT shares our D3D device from its own threads.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }

        // Find a hardware H.264 encoder (NV12 in, H.264 out).
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input_info),
                Some(&output_info),
                &mut activates,
                &mut count,
            )
            .map_err(|e| err("MFTEnumEx", e))?;
        }
        if count == 0 || activates.is_null() {
            return Err(PipelineError::Encode(
                "no hardware H.264 encoder MFT found".into(),
            ));
        }
        let transform: IMFTransform = unsafe {
            let result = (*activates)
                .as_ref()
                .ok_or_else(|| PipelineError::Encode("null MFT activate".into()))
                .and_then(|first| first.ActivateObject().map_err(|e| err("ActivateObject", e)));
            // Release every enumerated activate (take ownership so Drop
            // runs), then free the COM task-memory array itself.
            for i in 0..count as usize {
                drop(std::ptr::read(activates.add(i)));
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
            result?
        };

        // Async MFT unlock + low latency.
        let attrs = unsafe {
            transform
                .GetAttributes()
                .map_err(|e| err("GetAttributes", e))?
        };
        unsafe {
            attrs
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(|e| err("set ASYNC_UNLOCK", e))?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }

        // Hand the MFT our D3D device.
        let mut reset_token = 0u32;
        let mut device_manager: Option<IMFDXGIDeviceManager> = None;
        let device_manager = unsafe {
            MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager)
                .map_err(|e| err("MFCreateDXGIDeviceManager", e))?;
            let dm = device_manager
                .ok_or_else(|| PipelineError::Encode("no DXGI device manager".into()))?;
            dm.ResetDevice(device, reset_token)
                .map_err(|e| err("ResetDevice", e))?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, dm.as_raw() as usize)
                .map_err(|e| err("SET_D3D_MANAGER", e))?;
            dm
        };

        // Output type first (required order for encoders), then input type.
        unsafe {
            let out_type = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok();
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).ok();
            out_type
                .SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)
                .ok();
            out_type
                .SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)
                .ok();
            out_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .ok();
            // Generous bitrate floor; quality mode (below) takes precedence
            // on encoders that honor it.
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, 50_000_000).ok();
            out_type
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                .ok();
            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(|e| err("SetOutputType", e))?;

            let in_type = MFCreateMediaType().map_err(|e| err("MFCreateMediaType", e))?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok();
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok();
            in_type
                .SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)
                .ok();
            in_type
                .SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)
                .ok();
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .ok();
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(|e| err("SetInputType", e))?;
        }

        // Encoder knobs. Not all vendors honor all of these; failures are
        // logged, not fatal — the bitstream itself is validated downstream.
        let codec_api: ICodecAPI = transform.cast().map_err(|e| err("ICodecAPI", e))?;
        unsafe {
            let set = |api: &windows::core::GUID, v: VARIANT, name: &str| {
                if let Err(e) = codec_api.SetValue(api, &v) {
                    warn!("encoder ignored {name}: {e}");
                }
            };
            set(
                &CODECAPI_AVEncCommonRateControlMode,
                variant_u32(eAVEncCommonRateControlMode_Quality.0 as u32),
                "RateControlMode=Quality",
            );
            // quality: our config is CRF-ish (lower = better); MF wants
            // 0..100 (higher = better).
            let mf_quality = (100u32.saturating_sub(quality * 2)).clamp(30, 100);
            set(
                &CODECAPI_AVEncCommonQuality,
                variant_u32(mf_quality),
                "CommonQuality",
            );
            set(
                &CODECAPI_AVEncMPVGOPSize,
                variant_u32(gop_frames),
                "GOPSize",
            );
            set(
                &CODECAPI_AVEncMPVDefaultBPictureCount,
                variant_u32(0),
                "BPictureCount=0",
            );
            set(
                &CODECAPI_AVLowLatencyMode,
                variant_bool(true),
                "LowLatencyMode",
            );
        }

        let events: IMFMediaEventGenerator = transform
            .cast()
            .map_err(|e| err("IMFMediaEventGenerator", e))?;

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| err("BEGIN_STREAMING", e))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| err("START_OF_STREAM", e))?;
        }

        debug!(width, height, fps, gop_frames, "MF encoder ready");
        Ok(Self {
            transform,
            events,
            codec_api,
            _device_manager: device_manager,
            sink,
            nominal_frame_100ns: 10_000_000 / fps.max(1) as i64,
            input_credits: 0,
            pending: VecDeque::new(),
            configured: false,
            width,
            height,
            fps,
        })
    }

    /// Encoder thread main loop: pump MFT events, feed queued frames on
    /// NeedInput, emit packets on HaveOutput. Returns on `feed` disconnect
    /// or Stop.
    pub fn run(&mut self, feed: &Receiver<EncoderFeed>) -> Result<(), PipelineError> {
        loop {
            // Drain all currently-available MFT events.
            loop {
                let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Ok(ev) => ev,
                    Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                    Err(e) => return Err(err("GetEvent", e)),
                };
                let kind = unsafe { event.GetType().map_err(|e| err("event GetType", e))? };
                match kind as i32 {
                    t if t == METransformNeedInput.0 => self.input_credits += 1,
                    t if t == METransformHaveOutput.0 => self.drain_one_output()?,
                    other => debug!(event = other, "ignoring MFT event"),
                }
            }

            // Feed as many queued frames as we have credits for.
            while self.input_credits > 0 {
                let Some(job) = self.pending.pop_front() else {
                    break;
                };
                self.submit(job)?;
                self.input_credits -= 1;
            }

            // Wait briefly for more work from the capture thread.
            match feed.recv_timeout(Duration::from_millis(2)) {
                Ok(EncoderFeed::Frame(job)) => {
                    // Bound the queue: if the encoder is behind, drop the
                    // oldest queued frame rather than grow without limit.
                    if self.pending.len() >= 4 {
                        self.pending.pop_front();
                        warn!("encoder behind; dropped a queued frame");
                    }
                    self.pending.push_back(job);
                }
                Ok(EncoderFeed::Stop) => return Ok(()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }

    fn submit(&mut self, job: EncodeJob) -> Result<(), PipelineError> {
        if job.force_keyframe {
            unsafe {
                let _ = self
                    .codec_api
                    .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
            }
        }
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &job.texture, 0, false)
                .map_err(|e| err("MFCreateDXGISurfaceBuffer", e))?;
            let sample: IMFSample = MFCreateSample().map_err(|e| err("MFCreateSample", e))?;
            sample.AddBuffer(&buffer).map_err(|e| err("AddBuffer", e))?;
            sample
                .SetSampleTime((job.pts.as_nanos() / 100) as i64)
                .map_err(|e| err("SetSampleTime", e))?;
            sample
                .SetSampleDuration(self.nominal_frame_100ns)
                .map_err(|e| err("SetSampleDuration", e))?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|e| err("ProcessInput", e))?;
        }
        Ok(())
    }

    fn drain_one_output(&mut self) -> Result<(), PipelineError> {
        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let result = unsafe { self.transform.ProcessOutput(0, &mut out, &mut status) };
        // Take ownership of whatever came back before error handling.
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut out[0].pSample) };
        let events = unsafe { std::mem::ManuallyDrop::take(&mut out[0].pEvents) };
        drop(events);
        result.map_err(|e| err("ProcessOutput", e))?;
        let Some(sample) = sample else {
            return Ok(());
        };

        let pts_100ns = unsafe { sample.GetSampleTime().unwrap_or(0) };
        let bytes = unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| err("ConvertToContiguousBuffer", e))?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut ptr, None, Some(&mut len))
                .map_err(|e| err("Lock", e))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            let _ = buffer.Unlock();
            data
        };

        self.emit(pts_100ns, &bytes);
        Ok(())
    }

    /// Parse the Annex-B output, derive keyframe-ness from the actual NAL
    /// types (research: MFTs sometimes mis-tag CleanPoint), configure the
    /// stream on the first IDR, and hand the AVCC packet to the sink.
    fn emit(&mut self, pts_100ns: i64, annexb_stream: &[u8]) {
        let nals = h264::split_nals(annexb_stream);
        let is_idr = nals.iter().any(|n| h264::nal_type(n) == 5);

        if !self.configured {
            let sps = nals.iter().find(|n| h264::nal_type(n) == h264::NAL_SPS);
            let pps = nals.iter().find(|n| h264::nal_type(n) == h264::NAL_PPS);
            if let (Some(sps), Some(pps)) = (sps, pps) {
                self.sink.configured(CodecConfig {
                    codec: Codec::H264 {
                        avcc: Bytes::from(h264::build_avcc_record(sps, pps)),
                    },
                    width: self.width,
                    height: self.height,
                    nominal_fps: self.fps,
                    color: ColorInfo::Bt709Limited,
                });
                self.configured = true;
            } else {
                // The ring drops pre-IDR packets anyway; nothing to send yet.
                if is_idr {
                    warn!("IDR without SPS/PPS before stream configured");
                }
                return;
            }
        }

        let data = h264::to_avcc_payload(&nals);
        if data.is_empty() {
            return;
        }
        self.sink.packet(EncodedPacket {
            pts: Duration::from_nanos(pts_100ns.max(0) as u64 * 100),
            duration: Duration::from_nanos(self.nominal_frame_100ns as u64 * 100),
            keyframe: is_idr,
            data: Bytes::from(data),
        });
    }
}
