extern crate ffmpeg_next as ffmpeg;

#[cfg(target_os = "macos")]
use crate::video_frame::MacosVideoToolboxFrame;
#[cfg(target_os = "windows")]
use crate::video_frame::WindowsD3d11Frame;
#[cfg(target_os = "linux")]
use crate::video_frame::{LinuxDmaBufFormat, LinuxDmaBufFrame, LinuxDmaBufPlane};
use crate::video_frame::{
    FfmpegVideoFrameRef, NativeSurfaceCapabilities, NativeSurfaceControl, VideoFormat,
    VideoFrameBuffer,
};
use ffmpeg::codec::{self, packet, Context as CodecContext};
use ffmpeg::decoder::Video as FfmpegVideoDecoder;
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::Video as VideoFrame;
use ffmpeg::Codec;
use st_protocol::{VideoChromaSampling, VideoCodec, VideoCodecSupport};
use std::ffi::CStr;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::raw::{c_int, c_void};
use std::{mem, ptr};
use std::sync::Arc;

/// Owns the hardware device context reference.
struct HwAccel {
    device_ctx: *mut ffmpeg::sys::AVBufferRef,
    hw_pix_fmt: ffmpeg::sys::AVPixelFormat,
    setup: HwSetup,
}

// SAFETY: The AVBufferRef is thread-safe (ref-counted with atomic ops in FFmpeg).
unsafe impl Send for HwAccel {}

impl Drop for HwAccel {
    fn drop(&mut self) {
        if !self.device_ctx.is_null() {
            unsafe {
                ffmpeg::sys::av_buffer_unref(&mut self.device_ctx);
            }
        }
    }
}

impl HwAccel {
    fn needs_transfer(&self, frame: &VideoFrame) -> bool {
        unsafe { (*frame.as_ptr()).format == self.hw_pix_fmt as c_int }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HwSetup {
    DeviceCtx,
    FramesCtx,
    Internal,
}

impl HwSetup {
    fn from_methods(methods: c_int) -> Option<Self> {
        use ffmpeg::sys::{
            AV_CODEC_HW_CONFIG_METHOD_AD_HOC, AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX,
            AV_CODEC_HW_CONFIG_METHOD_HW_FRAMES_CTX, AV_CODEC_HW_CONFIG_METHOD_INTERNAL,
        };

        if methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int != 0 {
            Some(Self::DeviceCtx)
        } else if methods & AV_CODEC_HW_CONFIG_METHOD_HW_FRAMES_CTX as c_int != 0 {
            Some(Self::FramesCtx)
        } else if methods
            & ((AV_CODEC_HW_CONFIG_METHOD_INTERNAL as c_int)
                | (AV_CODEC_HW_CONFIG_METHOD_AD_HOC as c_int))
            != 0
        {
            Some(Self::Internal)
        } else {
            None
        }
    }

    fn needs_device_ctx(self) -> bool {
        matches!(self, Self::DeviceCtx | Self::FramesCtx)
    }

    fn needs_frames_ctx(self) -> bool {
        matches!(self, Self::FramesCtx)
    }
}

#[derive(Clone, Copy)]
struct HwConfig {
    device_type: ffmpeg::sys::AVHWDeviceType,
    pix_fmt: ffmpeg::sys::AVPixelFormat,
    setup: HwSetup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HwFrameAccess {
    #[cfg(target_os = "linux")]
    DmaBuf,
    DirectMap,
    Map,
    Transfer,
}

impl HwFrameAccess {
    fn label(self) -> &'static str {
        match self {
            #[cfg(target_os = "linux")]
            Self::DmaBuf => "dmabuf",
            Self::DirectMap => "direct-map",
            Self::Map => "map",
            Self::Transfer => "transfer",
        }
    }
}

/// Cached software scaler, recreated when source format/dimensions change.
struct ScalerState {
    ctx: scaling::Context,
    width: u32,
    height: u32,
    format: Pixel,
}

struct StampedBorrowedPacket<'a> {
    packet: ffmpeg::sys::AVPacket,
    _data: &'a [u8],
}

impl<'a> StampedBorrowedPacket<'a> {
    fn new(data: &'a [u8], frame_id: Option<u32>) -> Self {
        unsafe {
            let mut packet: ffmpeg::sys::AVPacket = mem::zeroed();
            packet.data = data.as_ptr() as *mut _;
            packet.size = data.len() as c_int;
            let pts = frame_id
                .map(i64::from)
                .unwrap_or(ffmpeg::sys::AV_NOPTS_VALUE);
            packet.pts = pts;
            packet.dts = pts;
            Self {
                packet,
                _data: data,
            }
        }
    }
}

impl packet::Ref for StampedBorrowedPacket<'_> {
    fn as_ptr(&self) -> *const ffmpeg::sys::AVPacket {
        &self.packet
    }
}

impl Drop for StampedBorrowedPacket<'_> {
    fn drop(&mut self) {
        unsafe {
            self.packet.data = ptr::null_mut();
            self.packet.size = 0;
            ffmpeg::sys::av_packet_unref(&mut self.packet);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeOutput {
    pub produced_frame: bool,
    pub frame_id: Option<u32>,
    pub dropped_stale_output: bool,
}

fn frame_id_is_newer(candidate: u32, previous: u32) -> bool {
    let delta = candidate.wrapping_sub(previous);
    delta > 0 && delta < 0x8000_0000
}

fn decoded_frame_id(frame: &VideoFrame) -> Option<u32> {
    frame
        .timestamp()
        .or_else(|| frame.pts())
        .and_then(|pts| u32::try_from(pts).ok())
}

pub struct VideoDecoder {
    // NOTE: field order matters for drop — decoder (AVCodecContext) must be
    // freed before hw (AVBufferRef) so the context can unref its copy first.
    codec_id: VideoCodec,
    decoder: FfmpegVideoDecoder,
    scaler: Option<ScalerState>,
    rgba_frame: Option<VideoFrame>,
    hw: Option<Box<HwAccel>>,
    hw_frame_access: Option<HwFrameAccess>,
    native_surface_control: Option<Arc<NativeSurfaceControl>>,
    #[cfg(target_os = "linux")]
    linux_dmabuf_enabled: bool,
    #[cfg(target_os = "macos")]
    macos_videotoolbox_enabled: bool,
    #[cfg(target_os = "windows")]
    windows_d3d11_enabled: bool,
    consecutive_failures: u32,
    waiting_for_recovery: bool,
    last_output_frame_id: Option<u32>,
    decoder_name: String,
    hardware_accelerated: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoCodecSupportReport {
    pub supported: VideoCodecSupport,
    pub hardware: VideoCodecSupport,
    pub yuv444: VideoCodecSupport,
    pub hardware_yuv444: VideoCodecSupport,
}

/// A hardware decoder to probe.
#[derive(Clone, Copy)]
enum ProbeStep {
    HwDevice {
        label: &'static str,
        device_type: ffmpeg::sys::AVHWDeviceType,
    },
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    NamedDecoder {
        label: &'static str,
        decoder_name: &'static str,
    },
}

/// Platform-ordered decode strategies.
#[cfg(target_os = "linux")]
fn probe_steps(codec: VideoCodec) -> Vec<ProbeStep> {
    use ffmpeg::sys::AVHWDeviceType::*;
    match codec {
        VideoCodec::H264 => vec![
            ProbeStep::HwDevice {
                label: "vaapi",
                device_type: AV_HWDEVICE_TYPE_VAAPI,
            },
            ProbeStep::HwDevice {
                label: "cuda",
                device_type: AV_HWDEVICE_TYPE_CUDA,
            },
            ProbeStep::NamedDecoder {
                label: "cuvid",
                decoder_name: "h264_cuvid",
            },
            ProbeStep::HwDevice {
                label: "qsv",
                device_type: AV_HWDEVICE_TYPE_QSV,
            },
            ProbeStep::NamedDecoder {
                label: "v4l2m2m",
                decoder_name: "h264_v4l2m2m",
            },
        ],
        VideoCodec::Hevc => vec![
            ProbeStep::HwDevice {
                label: "vaapi",
                device_type: AV_HWDEVICE_TYPE_VAAPI,
            },
            ProbeStep::HwDevice {
                label: "cuda",
                device_type: AV_HWDEVICE_TYPE_CUDA,
            },
            ProbeStep::NamedDecoder {
                label: "cuvid",
                decoder_name: "hevc_cuvid",
            },
            ProbeStep::HwDevice {
                label: "qsv",
                device_type: AV_HWDEVICE_TYPE_QSV,
            },
            ProbeStep::NamedDecoder {
                label: "v4l2m2m",
                decoder_name: "hevc_v4l2m2m",
            },
        ],
        VideoCodec::Av1 => vec![
            ProbeStep::HwDevice {
                label: "vaapi",
                device_type: AV_HWDEVICE_TYPE_VAAPI,
            },
            ProbeStep::HwDevice {
                label: "cuda",
                device_type: AV_HWDEVICE_TYPE_CUDA,
            },
            ProbeStep::NamedDecoder {
                label: "cuvid",
                decoder_name: "av1_cuvid",
            },
            ProbeStep::HwDevice {
                label: "qsv",
                device_type: AV_HWDEVICE_TYPE_QSV,
            },
        ],
    }
}

#[cfg(target_os = "macos")]
fn probe_steps(_codec: VideoCodec) -> Vec<ProbeStep> {
    use ffmpeg::sys::AVHWDeviceType::*;
    vec![ProbeStep::HwDevice {
        label: "videotoolbox",
        device_type: AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
    }]
}

#[cfg(target_os = "windows")]
fn probe_steps(codec: VideoCodec) -> Vec<ProbeStep> {
    use ffmpeg::sys::AVHWDeviceType::*;
    let cuvid = match codec {
        VideoCodec::H264 => "h264_cuvid",
        VideoCodec::Hevc => "hevc_cuvid",
        VideoCodec::Av1 => "av1_cuvid",
    };
    let amf = match codec {
        VideoCodec::H264 => "h264_amf",
        VideoCodec::Hevc => "hevc_amf",
        VideoCodec::Av1 => "av1_amf",
    };

    vec![
        ProbeStep::HwDevice {
            label: "d3d11va",
            device_type: AV_HWDEVICE_TYPE_D3D11VA,
        },
        ProbeStep::HwDevice {
            label: "dxva2",
            device_type: AV_HWDEVICE_TYPE_DXVA2,
        },
        ProbeStep::HwDevice {
            label: "qsv",
            device_type: AV_HWDEVICE_TYPE_QSV,
        },
        ProbeStep::HwDevice {
            label: "cuda",
            device_type: AV_HWDEVICE_TYPE_CUDA,
        },
        ProbeStep::NamedDecoder {
            label: "cuvid",
            decoder_name: cuvid,
        },
        ProbeStep::NamedDecoder {
            label: "amf",
            decoder_name: amf,
        },
    ]
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn probe_steps(_codec: VideoCodec) -> Vec<ProbeStep> {
    Vec::new()
}

impl VideoDecoder {
    /// Create a new decoder with automatic hardware detection.
    ///
    /// Probing order:
    /// 1. `VIDEO_DECODER_HINT` / codec-specific decoder hint
    /// 2. Platform-ordered hardware device / decoder strategies
    /// 3. Software fallback
    pub fn new(codec: VideoCodec, chroma: VideoChromaSampling) -> Result<Self, String> {
        Self::new_internal(codec, chroma, true)
    }

    #[allow(dead_code)]
    pub fn new_software(codec: VideoCodec) -> Result<Self, String> {
        ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;
        Self::try_sw_decoder(codec)
    }

    fn new_internal(
        codec: VideoCodec,
        chroma: VideoChromaSampling,
        verbose: bool,
    ) -> Result<Self, String> {
        ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;
        let test_bitstream = generate_test_bitstream(codec, chroma).ok();

        // 1. User override
        if let Some(hint) = decoder_hint(codec) {
            if verbose {
                eprintln!("[decode] trying user hint: {hint}");
            }
            match Self::try_hint(codec, &hint) {
                Ok(d) => {
                    if verbose {
                        eprintln!("[decode] using hinted decoder: {}", d.decoder_name);
                    }
                    return Ok(d);
                }
                Err(e) => {
                    if verbose {
                        eprintln!("[decode] hint '{hint}' failed: {e}");
                    }
                }
            }
        }

        // 2. Hardware decoders
        for step in probe_steps(codec) {
            if verbose {
                eprintln!("[decode] probing {}...", probe_step_name(step));
            }
            match Self::try_validated_probe_step(codec, step, test_bitstream.as_deref()) {
                Ok(d) => {
                    if verbose {
                        eprintln!("[decode] using hardware decoder: {}", d.decoder_name);
                    }
                    return Ok(d);
                }
                Err(e) => {
                    if verbose {
                        eprintln!("[decode] {} unavailable: {e}", probe_step_name(step));
                    }
                }
            }
        }

        // 3. Software fallback
        if verbose {
            eprintln!("[decode] using software {} decoder", codec_label(codec));
        }
        Self::try_sw_decoder(codec)
    }

    pub fn detect_supported_codecs() -> VideoCodecSupportReport {
        let mut supported = VideoCodecSupport::empty();
        let mut hardware = VideoCodecSupport::empty();
        let mut yuv444 = VideoCodecSupport::empty();
        let mut hardware_yuv444 = VideoCodecSupport::empty();

        for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
            let (codec_supported, codec_hardware) =
                detect_decode_support(codec, VideoChromaSampling::Yuv420);
            if codec_supported {
                supported.insert(codec);
            }
            if codec_hardware {
                hardware.insert(codec);
            }

            if codec_supports_yuv444(codec) {
                let (codec_yuv444, codec_hardware_yuv444) =
                    detect_decode_support(codec, VideoChromaSampling::Yuv444);
                if codec_yuv444 {
                    yuv444.insert(codec);
                }
                if codec_hardware_yuv444 {
                    hardware_yuv444.insert(codec);
                }
            }
        }

        VideoCodecSupportReport {
            supported,
            hardware,
            yuv444,
            hardware_yuv444,
        }
    }

    fn try_test_decode(&mut self, test_data: &[u8]) -> Result<(), String> {
        let pkt = StampedBorrowedPacket::new(test_data, None);
        self.decoder
            .send_packet(&pkt)
            .map_err(|e| format!("send_packet: {e}"))?;
        let mut frame = VideoFrame::empty();
        // Hardware decoders may need a second send+receive cycle.
        for _ in 0..4 {
            if self.decoder.receive_frame(&mut frame).is_ok() {
                return Ok(());
            }
            // Some decoders need a flush signal before emitting the buffered frame.
            let _ = self.decoder.send_eof();
            if self.decoder.receive_frame(&mut frame).is_ok() {
                return Ok(());
            }
        }
        Err("no frame produced after send_packet".into())
    }

    fn try_validated_probe_step(
        codec: VideoCodec,
        step: ProbeStep,
        test_bitstream: Option<&[u8]>,
    ) -> Result<Self, String> {
        let Some(test_data) = test_bitstream else {
            return Self::try_probe_step(codec, step);
        };

        let mut probe = Self::try_probe_step(codec, step)?;
        probe.try_test_decode(test_data)?;
        drop(probe);
        Self::try_probe_step(codec, step)
    }

    fn try_hint(codec: VideoCodec, hint: &str) -> Result<Self, String> {
        if let Some(device_type) = parse_hw_device_type(hint) {
            let label = device_type_name(device_type).unwrap_or(hint).to_string();
            return Self::try_hw_device(codec, device_type, &label);
        }

        Self::try_named_decoder(hint)
    }

    /// Try a decoder by exact FFmpeg decoder name.
    fn try_named_decoder(name: &str) -> Result<Self, String> {
        let codec =
            codec::decoder::find_by_name(name).ok_or_else(|| format!("'{name}' not found"))?;

        if let Some(config) = find_first_hw_config(&codec) {
            return Self::init_hw(codec, config, name);
        }
        Self::init_sw(codec, name)
    }

    fn try_probe_step(codec: VideoCodec, step: ProbeStep) -> Result<Self, String> {
        match step {
            ProbeStep::HwDevice { label, device_type } => {
                Self::try_hw_device(codec, device_type, label)
            }
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            ProbeStep::NamedDecoder {
                label,
                decoder_name,
            } => Self::try_named_probe(codec, decoder_name, label),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn try_named_probe(
        codec_id: VideoCodec,
        decoder_name: &str,
        label: &str,
    ) -> Result<Self, String> {
        let codec = codec::decoder::find_by_name(decoder_name)
            .ok_or_else(|| format!("'{decoder_name}' not found"))?;
        let decoder_label = format!("{} ({label})", codec_label(codec_id));

        if let Some(config) = find_first_hw_config(&codec) {
            return Self::init_hw(codec, config, &decoder_label);
        }

        Self::init_sw(codec, &decoder_label)
    }

    fn try_hw_device(
        codec_id: VideoCodec,
        device_type: ffmpeg::sys::AVHWDeviceType,
        label: &str,
    ) -> Result<Self, String> {
        let codec = codec::decoder::find(ffmpeg_codec_id(codec_id))
            .ok_or_else(|| format!("{} decoder not found", codec_label(codec_id)))?;
        let config = find_hw_config_for_device(&codec, device_type).ok_or_else(|| {
            format!(
                "codec has no {} hw config",
                device_type_name(device_type).unwrap_or(label)
            )
        })?;
        let decoder_label = format!("{} ({label})", codec_label(codec_id));
        Self::init_hw(codec, config, &decoder_label)
    }

    fn try_sw_decoder(codec_id: VideoCodec) -> Result<Self, String> {
        let codec = codec::decoder::find(ffmpeg_codec_id(codec_id))
            .ok_or_else(|| format!("{} software decoder not found", codec_label(codec_id)))?;
        let label = format!("{} (software)", codec_label(codec_id));
        Self::init_sw(codec, &label)
    }

    // ---- internal constructors ----

    fn init_hw(codec: Codec, config: HwConfig, name: &str) -> Result<Self, String> {
        let mut hw = Box::new(HwAccel {
            device_ctx: ptr::null_mut(),
            hw_pix_fmt: config.pix_fmt,
            setup: config.setup,
        });

        if hw.setup.needs_device_ctx() {
            let ret = unsafe {
                ffmpeg::sys::av_hwdevice_ctx_create(
                    &mut hw.device_ctx,
                    config.device_type,
                    ptr::null(),
                    ptr::null_mut(),
                    0,
                )
            };
            if ret < 0 {
                return Err(format!(
                    "{} device init failed (err {ret})",
                    device_type_name(config.device_type).unwrap_or("hw")
                ));
            }
        }

        let mut ctx = CodecContext::new_with_codec(codec);
        unsafe {
            let raw = ctx.as_mut_ptr();
            apply_flags(raw, true);
            (*raw).opaque = hw.as_mut() as *mut HwAccel as *mut c_void;
            (*raw).get_format = Some(select_hw_pixel_format);
            if !hw.device_ctx.is_null() {
                (*raw).hw_device_ctx = ffmpeg::sys::av_buffer_ref(hw.device_ctx);
                if (*raw).hw_device_ctx.is_null() {
                    (*raw).opaque = ptr::null_mut();
                    return Err("av_buffer_ref failed".into());
                }
            }
        }

        let decoder = match ctx.decoder().video() {
            Ok(d) => d,
            Err(e) => {
                return Err(format!("codec open: {e}"));
            }
        };

        Ok(Self {
            codec_id: codec_label_to_id(codec.id().into()).unwrap_or(VideoCodec::H264),
            decoder,
            scaler: None,
            rgba_frame: None,
            hw: Some(hw),
            hw_frame_access: None,
            native_surface_control: None,
            #[cfg(target_os = "linux")]
            linux_dmabuf_enabled: false,
            #[cfg(target_os = "macos")]
            macos_videotoolbox_enabled: false,
            #[cfg(target_os = "windows")]
            windows_d3d11_enabled: false,
            consecutive_failures: 0,
            waiting_for_recovery: false,
            last_output_frame_id: None,
            decoder_name: name.to_string(),
            hardware_accelerated: true,
        })
    }

    fn init_sw(codec: Codec, name: &str) -> Result<Self, String> {
        let mut ctx = CodecContext::new_with_codec(codec);
        unsafe {
            apply_flags(ctx.as_mut_ptr(), false);
        }

        let decoder = ctx
            .decoder()
            .video()
            .map_err(|e| format!("codec open: {e}"))?;

        Ok(Self {
            codec_id: codec_label_to_id(codec.id().into()).unwrap_or(VideoCodec::H264),
            decoder,
            scaler: None,
            rgba_frame: None,
            hw: None,
            hw_frame_access: None,
            native_surface_control: None,
            #[cfg(target_os = "linux")]
            linux_dmabuf_enabled: false,
            #[cfg(target_os = "macos")]
            macos_videotoolbox_enabled: false,
            #[cfg(target_os = "windows")]
            windows_d3d11_enabled: false,
            consecutive_failures: 0,
            waiting_for_recovery: false,
            last_output_frame_id: None,
            decoder_name: name.to_string(),
            hardware_accelerated: false,
        })
    }

    // ---- decoding ----

    /// Feed an encoded access unit to the decoder and write the latest uploadable frame into `frame_out`.
    pub fn decode_into(
        &mut self,
        nal_data: &[u8],
        packet_frame_id: u32,
        frame_out: &mut VideoFrameBuffer,
    ) -> Result<DecodeOutput, String> {
        self.refresh_native_surface_capabilities();
        let has_recovery_point = packet_has_recovery_point(self.codec_id, nal_data);
        if self.waiting_for_recovery && !has_recovery_point {
            return Ok(DecodeOutput::default());
        }
        if self.waiting_for_recovery && has_recovery_point {
            unsafe {
                ffmpeg::sys::avcodec_flush_buffers(self.decoder.as_mut_ptr());
            }
        }
        let pkt = StampedBorrowedPacket::new(nal_data, Some(packet_frame_id));

        if let Err(e) = self.decoder.send_packet(&pkt) {
            self.consecutive_failures += 1;

            unsafe {
                ffmpeg::sys::avcodec_flush_buffers(self.decoder.as_mut_ptr());
            }

            if has_recovery_point {
                if self.decoder.send_packet(&pkt).is_ok() {
                    self.waiting_for_recovery = false;
                    self.consecutive_failures = 0;
                } else {
                    self.waiting_for_recovery = true;
                }
            } else {
                if !self.waiting_for_recovery {
                    eprintln!(
                        "[decode] {} waiting for recovery frame after send_packet error: {e}",
                        self.decoder_name
                    );
                }
                self.waiting_for_recovery = true;
            }

            if self.consecutive_failures > 20 {
                return Err(format!(
                    "[{}] {} consecutive failures, last: {e}",
                    self.decoder_name, self.consecutive_failures
                ));
            }
            return Ok(DecodeOutput::default());
        } else if self.waiting_for_recovery && has_recovery_point {
            eprintln!("[decode] {} recovered on recovery frame", self.decoder_name);
            self.waiting_for_recovery = false;
        }

        let mut output = DecodeOutput::default();
        let mut decoded = VideoFrame::empty();
        let mut mapped_frame = VideoFrame::empty();
        let mut transferred_frame = VideoFrame::empty();

        while self.decoder.receive_frame(&mut decoded).is_ok() {
            self.consecutive_failures = 0;
            self.waiting_for_recovery = false;
            let decoded_frame_id = decoded_frame_id(&decoded);
            if decoded_frame_id
                .zip(self.last_output_frame_id)
                .map(|(frame_id, last)| !frame_id_is_newer(frame_id, last))
                .unwrap_or(false)
            {
                output.dropped_stale_output = true;
                if std::env::var_os("ST_TRACE").is_some() {
                    eprintln!(
                        "[trace][decode] dropping stale decoder output frame_id={:?} after {:?}",
                        decoded_frame_id,
                        self.last_output_frame_id
                    );
                }
                continue;
            }

            #[cfg(target_os = "linux")]
            if self.linux_dmabuf_enabled {
                if let Some(hw) = self.hw.as_ref() {
                    if hw.needs_transfer(&decoded) {
                        match self.try_fill_linux_dmabuf(&decoded, frame_out) {
                            Ok(()) => {
                                output.produced_frame = true;
                                let frame_id = decoded_frame_id.unwrap_or(packet_frame_id);
                                output.frame_id = Some(frame_id);
                                self.last_output_frame_id = Some(frame_id);
                                continue;
                            }
                            Err(err) => {
                                eprintln!("[decode] disabling dmabuf fast path: {err}");
                                if let Some(control) = self.native_surface_control.as_ref() {
                                    let _ = control.disable_linux_dmabuf();
                                }
                                self.linux_dmabuf_enabled = false;
                            }
                        }
                    }
                }
            }

            #[cfg(target_os = "macos")]
            if self.macos_videotoolbox_enabled {
                if let Some(hw) = self.hw.as_ref() {
                    if hw.needs_transfer(&decoded) {
                        match self.try_fill_macos_videotoolbox(&decoded, frame_out) {
                            Ok(()) => {
                                output.produced_frame = true;
                                let frame_id = decoded_frame_id.unwrap_or(packet_frame_id);
                                output.frame_id = Some(frame_id);
                                self.last_output_frame_id = Some(frame_id);
                                continue;
                            }
                            Err(err) => {
                                eprintln!("[decode] disabling videotoolbox surface path: {err}");
                                if let Some(control) = self.native_surface_control.as_ref() {
                                    let _ = control.disable_macos_videotoolbox();
                                }
                                self.macos_videotoolbox_enabled = false;
                            }
                        }
                    }
                }
            }

            #[cfg(target_os = "windows")]
            if self.windows_d3d11_enabled {
                if let Some(hw) = self.hw.as_ref() {
                    if hw.needs_transfer(&decoded) {
                        match self.try_fill_windows_d3d11(&decoded, frame_out) {
                            Ok(()) => {
                                output.produced_frame = true;
                                let frame_id = decoded_frame_id.unwrap_or(packet_frame_id);
                                output.frame_id = Some(frame_id);
                                self.last_output_frame_id = Some(frame_id);
                                continue;
                            }
                            Err(err) => {
                                eprintln!("[decode] disabling D3D11 surface path: {err}");
                                if let Some(control) = self.native_surface_control.as_ref() {
                                    let _ = control.disable_windows_d3d11();
                                }
                                self.windows_d3d11_enabled = false;
                            }
                        }
                    }
                }
            }

            // Hardware path: prefer direct mappings, then mapped software views,
            // and only fall back to a transfer copy when mapping is unavailable.
            let source: &VideoFrame = if let Some(hw) = self.hw.as_ref() {
                if hw.needs_transfer(&decoded) {
                    match self.hw_upload_source(&decoded, &mut mapped_frame, &mut transferred_frame)
                    {
                        Ok(frame) => frame,
                        Err(err) => {
                            eprintln!("[decode] {err}, skipping frame");
                            continue;
                        }
                    }
                } else {
                    &decoded
                }
            } else {
                &decoded
            };

            let w = source.width();
            let h = source.height();
            if w == 0 || h == 0 {
                continue;
            }

            match source.format() {
                Pixel::YUV420P => copy_yuv420_frame(source, frame_out),
                Pixel::YUV444P => copy_yuv444_frame(source, frame_out),
                Pixel::NV12 => copy_nv12_frame(source, frame_out),
                _ => self.copy_rgba_frame(source, frame_out)?,
            }
            output.produced_frame = true;
            let frame_id = decoded_frame_id.unwrap_or(packet_frame_id);
            output.frame_id = Some(frame_id);
            self.last_output_frame_id = Some(frame_id);
        }

        Ok(output)
    }

    pub fn set_native_surface_control(&mut self, control: Arc<NativeSurfaceControl>) {
        self.native_surface_control = Some(control);
        self.refresh_native_surface_capabilities();
    }

    fn refresh_native_surface_capabilities(&mut self) {
        let Some(control) = self.native_surface_control.as_ref() else {
            return;
        };
        self.set_native_surface_capabilities(control.snapshot());
    }

    fn set_native_surface_capabilities(&mut self, caps: NativeSurfaceCapabilities) {
        #[cfg(target_os = "linux")]
        {
            let changed = self.linux_dmabuf_enabled != caps.linux_dmabuf;
            self.linux_dmabuf_enabled = caps.linux_dmabuf;
            if changed && caps.linux_dmabuf {
                eprintln!("[decode] {} dmabuf import path enabled", self.decoder_name);
            }
        }

        #[cfg(target_os = "macos")]
        {
            let changed = self.macos_videotoolbox_enabled != caps.macos_videotoolbox;
            self.macos_videotoolbox_enabled = caps.macos_videotoolbox;
            if changed && caps.macos_videotoolbox {
                eprintln!(
                    "[decode] {} videotoolbox surface path enabled",
                    self.decoder_name
                );
            }
        }

        #[cfg(target_os = "windows")]
        {
            let changed = self.windows_d3d11_enabled != caps.windows_d3d11;
            self.windows_d3d11_enabled = caps.windows_d3d11;
            if changed && caps.windows_d3d11 {
                eprintln!("[decode] {} D3D11 surface path enabled", self.decoder_name);
            }
        }

        #[cfg(not(target_os = "linux"))]
        let _ = caps.linux_dmabuf;
        #[cfg(not(target_os = "macos"))]
        let _ = caps.macos_videotoolbox;
        #[cfg(not(target_os = "windows"))]
        let _ = caps.windows_d3d11;
    }

    fn hw_upload_source<'a>(
        &mut self,
        decoded: &VideoFrame,
        mapped_frame: &'a mut VideoFrame,
        transferred_frame: &'a mut VideoFrame,
    ) -> Result<&'a VideoFrame, String> {
        let direct_flags =
            ffmpeg::sys::AV_HWFRAME_MAP_READ as c_int | ffmpeg::sys::AV_HWFRAME_MAP_DIRECT as c_int;
        let read_flags = ffmpeg::sys::AV_HWFRAME_MAP_READ as c_int;
        let mut map_errors = Vec::with_capacity(3);

        if let Err(err) = try_map_hw_frame(mapped_frame, decoded, direct_flags) {
            map_errors.push(format!("direct-map={}", ffmpeg_err(err)));
        } else if is_uploadable_frame(mapped_frame) {
            self.note_hw_frame_access(HwFrameAccess::DirectMap, mapped_frame.format());
            return Ok(mapped_frame);
        } else {
            map_errors.push(format!(
                "direct-map=unsupported {}",
                pixel_label(mapped_frame.format())
            ));
        }

        if let Err(err) = try_map_hw_frame(mapped_frame, decoded, read_flags) {
            map_errors.push(format!("map={}", ffmpeg_err(err)));
        } else if is_uploadable_frame(mapped_frame) {
            self.note_hw_frame_access(HwFrameAccess::Map, mapped_frame.format());
            return Ok(mapped_frame);
        } else {
            map_errors.push(format!(
                "map=unsupported {}",
                pixel_label(mapped_frame.format())
            ));
        }

        if let Err(err) = try_transfer_hw_frame(transferred_frame, decoded) {
            map_errors.push(format!("transfer={}", ffmpeg_err(err)));
        } else if is_uploadable_frame(transferred_frame) {
            self.note_hw_frame_access(HwFrameAccess::Transfer, transferred_frame.format());
            return Ok(transferred_frame);
        } else {
            map_errors.push(format!(
                "transfer=unsupported {}",
                pixel_label(transferred_frame.format())
            ));
        }

        Err(format!(
            "hw frame extraction failed for {}: {}",
            self.decoder_name,
            map_errors.join(", ")
        ))
    }

    fn note_hw_frame_access(&mut self, access: HwFrameAccess, format: Pixel) {
        if self.hw_frame_access == Some(access) {
            return;
        }

        self.hw_frame_access = Some(access);
        eprintln!(
            "[decode] {} hw frame access: {} ({})",
            self.decoder_name,
            access.label(),
            pixel_label(format),
        );
    }

    #[cfg(target_os = "linux")]
    fn try_fill_linux_dmabuf(
        &mut self,
        decoded: &VideoFrame,
        frame_out: &mut VideoFrameBuffer,
    ) -> Result<(), String> {
        let dmabuf_format = match hw_sw_format(decoded) {
            Pixel::NV12 => LinuxDmaBufFormat::Nv12,
            Pixel::YUV420P => LinuxDmaBufFormat::Yuv420p8,
            Pixel::YUV444P => LinuxDmaBufFormat::Yuv444p8,
            fmt => {
                return Err(format!(
                    "{} unsupported dmabuf sw format {}",
                    self.decoder_name,
                    pixel_label(fmt)
                ))
            }
        };

        let mut drm_frame = VideoFrame::empty();
        try_map_hw_frame_to_drm(
            &mut drm_frame,
            decoded,
            ffmpeg::sys::AV_HWFRAME_MAP_READ as c_int | ffmpeg::sys::AV_HWFRAME_MAP_DIRECT as c_int,
        )
        .map_err(ffmpeg_err)?;

        let planes = linux_dmabuf_planes(&drm_frame, dmabuf_format)?;
        frame_out.width = drm_frame.width();
        frame_out.height = drm_frame.height();
        frame_out.format = match dmabuf_format {
            LinuxDmaBufFormat::Yuv420p8 => VideoFormat::Yuv420p8,
            LinuxDmaBufFormat::Yuv444p8 => VideoFormat::Yuv444p8,
            LinuxDmaBufFormat::Nv12 => VideoFormat::Nv12,
        };
        frame_out.plane0.clear();
        frame_out.plane1.clear();
        frame_out.plane2.clear();
        let decoder_frame_ref = Some(FfmpegVideoFrameRef::retain(decoded)?);
        frame_out.clear_native_surfaces();
        frame_out.dmabuf = Some(LinuxDmaBufFrame {
            width: drm_frame.width(),
            height: drm_frame.height(),
            format: dmabuf_format,
            planes,
            decoder_frame_ref: decoder_frame_ref.clone(),
        });
        frame_out.decoder_frame_ref = decoder_frame_ref;
        self.note_hw_frame_access(HwFrameAccess::DmaBuf, Pixel::DRM_PRIME);
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn try_fill_macos_videotoolbox(
        &mut self,
        decoded: &VideoFrame,
        frame_out: &mut VideoFrameBuffer,
    ) -> Result<(), String> {
        let format = match hw_sw_format(decoded) {
            Pixel::NV12 => VideoFormat::Nv12,
            Pixel::YUV420P => VideoFormat::Yuv420p8,
            fmt => {
                return Err(format!(
                    "{} unsupported VideoToolbox sw format {}",
                    self.decoder_name,
                    pixel_label(fmt)
                ))
            }
        };

        let raw = unsafe { decoded.as_ptr() };
        let pixel_buffer = unsafe {
            crate::video_frame::MacosCvPixelBuffer::retain((*raw).data[3].cast())
                .ok_or_else(|| "VideoToolbox frame missing CVPixelBufferRef".to_string())?
        };

        frame_out.width = decoded.width();
        frame_out.height = decoded.height();
        frame_out.format = format;
        frame_out.plane0.clear();
        frame_out.plane1.clear();
        frame_out.plane2.clear();
        let decoder_frame_ref = Some(FfmpegVideoFrameRef::retain(decoded)?);
        frame_out.clear_native_surfaces();
        frame_out.videotoolbox = Some(MacosVideoToolboxFrame {
            width: decoded.width(),
            height: decoded.height(),
            format,
            pixel_buffer,
            decoder_frame_ref: decoder_frame_ref.clone(),
        });
        frame_out.decoder_frame_ref = decoder_frame_ref;
        self.note_hw_frame_access(HwFrameAccess::DirectMap, Pixel::VIDEOTOOLBOX);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    fn try_fill_windows_d3d11(
        &mut self,
        decoded: &VideoFrame,
        frame_out: &mut VideoFrameBuffer,
    ) -> Result<(), String> {
        let format = match hw_sw_format(decoded) {
            Pixel::NV12 => VideoFormat::Nv12,
            fmt => {
                return Err(format!(
                    "{} unsupported D3D11 sw format {}",
                    self.decoder_name,
                    pixel_label(fmt)
                ))
            }
        };

        let raw = unsafe { decoded.as_ptr() };
        let texture = unsafe {
            crate::video_frame::WindowsComPtr::retain((*raw).data[0].cast())
                .ok_or_else(|| "D3D11 frame missing ID3D11Texture2D".to_string())?
        };
        let device_ctx = d3d11_device_context(decoded)?;
        let device = unsafe {
            crate::video_frame::WindowsComPtr::retain((*device_ctx).device)
                .ok_or_else(|| "D3D11 frame missing decoder device".to_string())?
        };
        let video_device = unsafe {
            crate::video_frame::WindowsComPtr::retain((*device_ctx).video_device)
                .ok_or_else(|| "D3D11 frame missing video device".to_string())?
        };
        let video_context = unsafe {
            crate::video_frame::WindowsComPtr::retain((*device_ctx).video_context)
                .ok_or_else(|| "D3D11 frame missing video context".to_string())?
        };
        let array_index = unsafe { (*raw).data[1] as usize as u32 };

        frame_out.width = decoded.width();
        frame_out.height = decoded.height();
        frame_out.format = format;
        frame_out.plane0.clear();
        frame_out.plane1.clear();
        frame_out.plane2.clear();
        let decoder_frame_ref = Some(FfmpegVideoFrameRef::retain(decoded)?);
        frame_out.clear_native_surfaces();
        frame_out.d3d11 = Some(WindowsD3d11Frame {
            width: decoded.width(),
            height: decoded.height(),
            format,
            device,
            video_device,
            video_context,
            texture,
            array_index,
            decoder_frame_ref: decoder_frame_ref.clone(),
        });
        frame_out.decoder_frame_ref = decoder_frame_ref;
        self.note_hw_frame_access(HwFrameAccess::DirectMap, Pixel::D3D11);
        Ok(())
    }

    /// Active decoder name (e.g. "h264_vaapi", "h264 (software)").
    pub fn name(&self) -> &str {
        &self.decoder_name
    }

    pub fn is_hardware_accelerated(&self) -> bool {
        self.hardware_accelerated
    }

    pub fn waiting_for_recovery(&self) -> bool {
        self.waiting_for_recovery
    }

    pub fn enter_recovery_mode(&mut self, reason: &str) {
        if !self.waiting_for_recovery {
            eprintln!(
                "[decode] {} waiting for recovery frame after {reason}",
                self.decoder_name
            );
        }
        unsafe {
            ffmpeg::sys::avcodec_flush_buffers(self.decoder.as_mut_ptr());
        }
        self.waiting_for_recovery = true;
        self.consecutive_failures = 0;
    }

    fn copy_rgba_frame(
        &mut self,
        source: &VideoFrame,
        frame_out: &mut VideoFrameBuffer,
    ) -> Result<(), String> {
        let w = source.width();
        let h = source.height();
        let fmt = source.format();
        let need_new = match &self.scaler {
            Some(s) => s.width != w || s.height != h || s.format != fmt,
            None => true,
        };
        if need_new {
            self.scaler = Some(ScalerState {
                ctx: scaling::Context::get(
                    fmt,
                    w,
                    h,
                    Pixel::RGBA,
                    w,
                    h,
                    scaling::Flags::FAST_BILINEAR,
                )
                .map_err(|e| format!("scaler: {e}"))?,
                width: w,
                height: h,
                format: fmt,
            });
            self.rgba_frame = Some(VideoFrame::new(Pixel::RGBA, w, h));
        } else if self.rgba_frame.is_none() {
            self.rgba_frame = Some(VideoFrame::new(Pixel::RGBA, w, h));
        }

        let scaler = &mut self.scaler.as_mut().unwrap().ctx;
        let rgba_frame = self.rgba_frame.as_mut().unwrap();
        scaler
            .run(source, rgba_frame)
            .map_err(|e| format!("scale: {e}"))?;

        frame_out.width = w;
        frame_out.height = h;
        frame_out.format = VideoFormat::Rgba8;
        frame_out.plane1.clear();
        frame_out.plane2.clear();
        frame_out.clear_native_surfaces();
        copy_plane_rows(
            &mut frame_out.plane0,
            rgba_frame.data(0),
            rgba_frame.stride(0),
            w as usize * 4,
            h as usize,
        );

        Ok(())
    }
}

// ---- helpers ----

fn find_first_hw_config(codec: &Codec) -> Option<HwConfig> {
    unsafe {
        for i in 0.. {
            let cfg = ffmpeg::sys::avcodec_get_hw_config(codec.as_ptr(), i);
            if cfg.is_null() {
                return None;
            }
            if let Some(setup) = HwSetup::from_methods((*cfg).methods) {
                return Some(HwConfig {
                    device_type: (*cfg).device_type,
                    pix_fmt: (*cfg).pix_fmt,
                    setup,
                });
            }
        }
    }

    None
}

fn codec_label_to_id(codec_id: ffmpeg::sys::AVCodecID) -> Option<VideoCodec> {
    match codec_id {
        ffmpeg::sys::AVCodecID::AV_CODEC_ID_H264 => Some(VideoCodec::H264),
        ffmpeg::sys::AVCodecID::AV_CODEC_ID_HEVC => Some(VideoCodec::Hevc),
        ffmpeg::sys::AVCodecID::AV_CODEC_ID_AV1 => Some(VideoCodec::Av1),
        _ => None,
    }
}

fn codec_supports_yuv444(codec: VideoCodec) -> bool {
    matches!(codec, VideoCodec::H264 | VideoCodec::Hevc)
}

fn chroma_label(chroma: VideoChromaSampling) -> &'static str {
    match chroma {
        VideoChromaSampling::Yuv420 => "yuv420",
        VideoChromaSampling::Yuv444 => "yuv444",
    }
}

fn detect_decode_support(codec: VideoCodec, chroma: VideoChromaSampling) -> (bool, bool) {
    let test_bitstream = generate_test_bitstream(codec, chroma);

    for step in probe_steps(codec) {
        if let Ok(mut decoder) = VideoDecoder::try_probe_step(codec, step) {
            match &test_bitstream {
                Ok(data) => match decoder.try_test_decode(data) {
                    Ok(()) => {
                        eprintln!(
                            "[probe] {} {} decoder '{}' validated (hw={})",
                            codec_label(codec),
                            chroma_label(chroma),
                            decoder.decoder_name,
                            decoder.hardware_accelerated,
                        );
                        return (true, decoder.is_hardware_accelerated());
                    }
                    Err(e) => {
                        eprintln!(
                            "[probe] {} {} decoder '{}' failed decode test: {e}",
                            codec_label(codec),
                            chroma_label(chroma),
                            decoder.decoder_name,
                        );
                    }
                },
                Err(_) => return (true, decoder.is_hardware_accelerated()),
            }
        }
    }

    if let Ok(mut decoder) = VideoDecoder::try_sw_decoder(codec) {
        match &test_bitstream {
            Ok(data) => {
                if decoder.try_test_decode(data).is_ok() {
                    eprintln!(
                        "[probe] {} {} decoder '{}' validated (hw=false)",
                        codec_label(codec),
                        chroma_label(chroma),
                        decoder.decoder_name,
                    );
                    return (true, false);
                }
            }
            Err(_) => return (true, false),
        }
    }

    (false, false)
}

fn packet_has_recovery_point(codec: VideoCodec, data: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => h264_has_recovery_nal(data),
        VideoCodec::Hevc => hevc_has_recovery_nal(data),
        VideoCodec::Av1 => true,
    }
}

fn h264_has_recovery_nal(data: &[u8]) -> bool {
    annex_b_nal_units(data).any(|nal| {
        let nal_type = nal[0] & 0x1f;
        matches!(nal_type, 5 | 7 | 8)
    })
}

fn hevc_has_recovery_nal(data: &[u8]) -> bool {
    annex_b_nal_units(data).any(|nal| {
        let nal_type = (nal[0] >> 1) & 0x3f;
        matches!(nal_type, 16..=23 | 32 | 33 | 34)
    })
}

fn annex_b_nal_units<'a>(data: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
    let mut units = Vec::new();
    let mut cursor = 0usize;
    while let Some((start, prefix_len)) = find_start_code(data, cursor) {
        let nal_start = start + prefix_len;
        let nal_end = find_start_code(data, nal_start)
            .map(|(next_start, _)| next_start)
            .unwrap_or(data.len());
        if nal_start < nal_end {
            units.push(&data[nal_start..nal_end]);
        }
        cursor = nal_end;
    }
    units.into_iter()
}

fn find_start_code(data: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut i = start;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data.get(i + 2) == Some(&1) {
                return Some((i, 3));
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

fn find_hw_config_for_device(
    codec: &Codec,
    device_type: ffmpeg::sys::AVHWDeviceType,
) -> Option<HwConfig> {
    unsafe {
        for i in 0.. {
            let cfg = ffmpeg::sys::avcodec_get_hw_config(codec.as_ptr(), i);
            if cfg.is_null() {
                return None;
            }
            if (*cfg).device_type != device_type {
                continue;
            }
            if let Some(setup) = HwSetup::from_methods((*cfg).methods) {
                return Some(HwConfig {
                    device_type,
                    pix_fmt: (*cfg).pix_fmt,
                    setup,
                });
            }
        }
    }

    None
}

unsafe extern "C" fn select_hw_pixel_format(
    s: *mut ffmpeg::sys::AVCodecContext,
    fmt: *const ffmpeg::sys::AVPixelFormat,
) -> ffmpeg::sys::AVPixelFormat {
    use ffmpeg::sys::AVPixelFormat::AV_PIX_FMT_NONE;

    if s.is_null() || fmt.is_null() {
        return AV_PIX_FMT_NONE;
    }

    let Some(hw) = ((*(s)).opaque as *mut HwAccel).as_mut() else {
        return AV_PIX_FMT_NONE;
    };

    let mut current = fmt;
    while *current != AV_PIX_FMT_NONE {
        if *current == hw.hw_pix_fmt {
            if hw.setup.needs_frames_ctx() {
                let mut frames_ctx = ptr::null_mut();
                let ret = ffmpeg::sys::avcodec_get_hw_frames_parameters(
                    s,
                    hw.device_ctx,
                    hw.hw_pix_fmt,
                    &mut frames_ctx,
                );
                if ret < 0 || frames_ctx.is_null() {
                    return AV_PIX_FMT_NONE;
                }

                let init_ret = ffmpeg::sys::av_hwframe_ctx_init(frames_ctx);
                if init_ret < 0 {
                    ffmpeg::sys::av_buffer_unref(&mut frames_ctx);
                    return AV_PIX_FMT_NONE;
                }

                ffmpeg::sys::av_buffer_unref(&mut (*s).hw_frames_ctx);
                (*s).hw_frames_ctx = frames_ctx;
            }

            return hw.hw_pix_fmt;
        }

        current = current.add(1);
    }

    AV_PIX_FMT_NONE
}

fn parse_hw_device_type(hint: &str) -> Option<ffmpeg::sys::AVHWDeviceType> {
    use ffmpeg::sys::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;
    let c_hint = std::ffi::CString::new(hint).ok()?;
    let device_type = unsafe { ffmpeg::sys::av_hwdevice_find_type_by_name(c_hint.as_ptr()) };
    (device_type != AV_HWDEVICE_TYPE_NONE).then_some(device_type)
}

fn device_type_name(device_type: ffmpeg::sys::AVHWDeviceType) -> Option<&'static str> {
    unsafe {
        let ptr = ffmpeg::sys::av_hwdevice_get_type_name(device_type);
        if ptr.is_null() {
            None
        } else {
            CStr::from_ptr(ptr).to_str().ok()
        }
    }
}

fn probe_step_name(step: ProbeStep) -> &'static str {
    match step {
        ProbeStep::HwDevice { label, .. } => label,
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        ProbeStep::NamedDecoder { label, .. } => label,
    }
}

fn copy_yuv420_frame(source: &VideoFrame, frame_out: &mut VideoFrameBuffer) {
    frame_out.width = source.width();
    frame_out.height = source.height();
    frame_out.format = VideoFormat::Yuv420p8;
    frame_out.clear_native_surfaces();

    copy_plane_rows(
        &mut frame_out.plane0,
        source.data(0),
        source.stride(0),
        source.plane_width(0) as usize,
        source.plane_height(0) as usize,
    );
    copy_plane_rows(
        &mut frame_out.plane1,
        source.data(1),
        source.stride(1),
        source.plane_width(1) as usize,
        source.plane_height(1) as usize,
    );
    copy_plane_rows(
        &mut frame_out.plane2,
        source.data(2),
        source.stride(2),
        source.plane_width(2) as usize,
        source.plane_height(2) as usize,
    );
}

fn copy_yuv444_frame(source: &VideoFrame, frame_out: &mut VideoFrameBuffer) {
    frame_out.width = source.width();
    frame_out.height = source.height();
    frame_out.format = VideoFormat::Yuv444p8;
    frame_out.clear_native_surfaces();

    copy_plane_rows(
        &mut frame_out.plane0,
        source.data(0),
        source.stride(0),
        source.plane_width(0) as usize,
        source.plane_height(0) as usize,
    );
    copy_plane_rows(
        &mut frame_out.plane1,
        source.data(1),
        source.stride(1),
        source.plane_width(1) as usize,
        source.plane_height(1) as usize,
    );
    copy_plane_rows(
        &mut frame_out.plane2,
        source.data(2),
        source.stride(2),
        source.plane_width(2) as usize,
        source.plane_height(2) as usize,
    );
}

fn copy_nv12_frame(source: &VideoFrame, frame_out: &mut VideoFrameBuffer) {
    frame_out.width = source.width();
    frame_out.height = source.height();
    frame_out.format = VideoFormat::Nv12;
    frame_out.clear_native_surfaces();

    copy_plane_rows(
        &mut frame_out.plane0,
        source.data(0),
        source.stride(0),
        source.plane_width(0) as usize,
        source.plane_height(0) as usize,
    );
    copy_plane_rows(
        &mut frame_out.plane1,
        source.data(1),
        source.stride(1),
        source.plane_width(1) as usize * 2,
        source.plane_height(1) as usize,
    );
    frame_out.plane2.clear();
}

fn copy_plane_rows(dst: &mut Vec<u8>, src: &[u8], stride: usize, row_bytes: usize, rows: usize) {
    let total = row_bytes * rows;
    dst.resize(total, 0);
    for row in 0..rows {
        let start = row * stride;
        let row_start = row * row_bytes;
        dst[row_start..row_start + row_bytes].copy_from_slice(&src[start..start + row_bytes]);
    }
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn dup(oldfd: c_int) -> c_int;
}

#[cfg(target_os = "linux")]
fn linux_dmabuf_planes(
    frame: &VideoFrame,
    format: LinuxDmaBufFormat,
) -> Result<Vec<LinuxDmaBufPlane>, String> {
    unsafe {
        let raw = frame.as_ptr();
        let desc_ptr = (*raw).data[0] as *const ffmpeg::sys::AVDRMFrameDescriptor;
        if desc_ptr.is_null() {
            return Err("drm frame missing descriptor".into());
        }

        let desc = &*desc_ptr;
        if desc.nb_layers < 1 {
            return Err("drm frame has no layers".into());
        }
        let layer = &desc.layers[0];

        match format {
            LinuxDmaBufFormat::Nv12 => {
                if layer.nb_planes < 2 {
                    return Err(format!(
                        "NV12 drm frame missing planes: {}",
                        layer.nb_planes
                    ));
                }
                Ok(vec![
                    build_linux_dmabuf_plane(
                        desc,
                        &layer.planes[0],
                        frame.width(),
                        frame.height(),
                        DRM_FORMAT_R8,
                    )?,
                    build_linux_dmabuf_plane(
                        desc,
                        &layer.planes[1],
                        frame.width().div_ceil(2),
                        frame.height().div_ceil(2),
                        DRM_FORMAT_GR88,
                    )?,
                ])
            }
            LinuxDmaBufFormat::Yuv420p8 => {
                if layer.nb_planes < 3 {
                    return Err(format!(
                        "YUV420 drm frame missing planes: {}",
                        layer.nb_planes
                    ));
                }
                Ok(vec![
                    build_linux_dmabuf_plane(
                        desc,
                        &layer.planes[0],
                        frame.width(),
                        frame.height(),
                        DRM_FORMAT_R8,
                    )?,
                    build_linux_dmabuf_plane(
                        desc,
                        &layer.planes[1],
                        frame.width().div_ceil(2),
                        frame.height().div_ceil(2),
                        DRM_FORMAT_R8,
                    )?,
                    build_linux_dmabuf_plane(
                        desc,
                        &layer.planes[2],
                        frame.width().div_ceil(2),
                        frame.height().div_ceil(2),
                        DRM_FORMAT_R8,
                    )?,
                ])
            }
            LinuxDmaBufFormat::Yuv444p8 => {
                if layer.nb_planes < 3 {
                    return Err(format!(
                        "YUV444 drm frame missing planes: {}",
                        layer.nb_planes
                    ));
                }
                Ok(vec![
                    build_linux_dmabuf_plane(
                        desc,
                        &layer.planes[0],
                        frame.width(),
                        frame.height(),
                        DRM_FORMAT_R8,
                    )?,
                    build_linux_dmabuf_plane(
                        desc,
                        &layer.planes[1],
                        frame.width(),
                        frame.height(),
                        DRM_FORMAT_R8,
                    )?,
                    build_linux_dmabuf_plane(
                        desc,
                        &layer.planes[2],
                        frame.width(),
                        frame.height(),
                        DRM_FORMAT_R8,
                    )?,
                ])
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn build_linux_dmabuf_plane(
    desc: &ffmpeg::sys::AVDRMFrameDescriptor,
    plane: &ffmpeg::sys::AVDRMPlaneDescriptor,
    width: u32,
    height: u32,
    drm_format: u32,
) -> Result<LinuxDmaBufPlane, String> {
    let object_index = usize::try_from(plane.object_index)
        .map_err(|_| format!("invalid drm object index {}", plane.object_index))?;
    let object_count = usize::try_from(desc.nb_objects)
        .map_err(|_| format!("invalid drm object count {}", desc.nb_objects))?;
    let object = desc
        .objects
        .get(..object_count)
        .and_then(|objects| objects.get(object_index))
        .ok_or_else(|| format!("drm plane references missing object {}", plane.object_index))?;

    let offset = u32::try_from(plane.offset)
        .map_err(|_| format!("invalid drm plane offset {}", plane.offset))?;
    let pitch = u32::try_from(plane.pitch)
        .map_err(|_| format!("invalid drm plane pitch {}", plane.pitch))?;

    Ok(LinuxDmaBufPlane {
        fd: duplicate_fd(object.fd)?,
        offset,
        pitch,
        modifier: object.format_modifier,
        width,
        height,
        drm_format,
    })
}

#[cfg(target_os = "linux")]
fn duplicate_fd(fd: c_int) -> Result<OwnedFd, String> {
    let new_fd = unsafe { dup(fd) };
    if new_fd < 0 {
        Err(format!("dup({fd}) failed"))
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(new_fd) })
    }
}

fn hw_sw_format(frame: &VideoFrame) -> Pixel {
    unsafe {
        let hw_frames_ctx = (*frame.as_ptr()).hw_frames_ctx;
        if hw_frames_ctx.is_null() {
            return Pixel::None;
        }

        let frames_ctx = (*hw_frames_ctx).data as *const ffmpeg::sys::AVHWFramesContext;
        if frames_ctx.is_null() {
            return Pixel::None;
        }

        Pixel::from((*frames_ctx).sw_format)
    }
}

#[cfg(target_os = "windows")]
fn d3d11_device_context(frame: &VideoFrame) -> Result<*const FfmpegD3d11vaDeviceContext, String> {
    unsafe {
        let hw_frames_ctx = (*frame.as_ptr()).hw_frames_ctx;
        if hw_frames_ctx.is_null() {
            return Err("D3D11 frame missing hw_frames_ctx".into());
        }

        let frames_ctx = (*hw_frames_ctx).data as *const ffmpeg::sys::AVHWFramesContext;
        if frames_ctx.is_null() {
            return Err("D3D11 frame missing AVHWFramesContext".into());
        }

        let device_ctx = (*frames_ctx).device_ctx;
        if device_ctx.is_null() {
            return Err("D3D11 frame missing AVHWDeviceContext".into());
        }

        let hwctx = (*device_ctx).hwctx as *const FfmpegD3d11vaDeviceContext;
        if hwctx.is_null()
            || (*hwctx).device.is_null()
            || (*hwctx).video_device.is_null()
            || (*hwctx).video_context.is_null()
        {
            return Err("D3D11 frame missing decoder device interfaces".into());
        }

        Ok(hwctx)
    }
}

#[cfg(target_os = "linux")]
fn try_map_hw_frame_to_drm(
    dst: &mut VideoFrame,
    src: &VideoFrame,
    flags: c_int,
) -> Result<(), i32> {
    unsafe {
        ffmpeg::sys::av_frame_unref(dst.as_mut_ptr());
        (*dst.as_mut_ptr()).width = (*src.as_ptr()).width;
        (*dst.as_mut_ptr()).height = (*src.as_ptr()).height;
        (*dst.as_mut_ptr()).format = ffmpeg::sys::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
        if !(*src.as_ptr()).hw_frames_ctx.is_null() {
            (*dst.as_mut_ptr()).hw_frames_ctx =
                ffmpeg::sys::av_buffer_ref((*src.as_ptr()).hw_frames_ctx);
            if (*dst.as_mut_ptr()).hw_frames_ctx.is_null() {
                ffmpeg::sys::av_frame_unref(dst.as_mut_ptr());
                return Err(-12);
            }
        }

        let ret = ffmpeg::sys::av_hwframe_map(dst.as_mut_ptr(), src.as_ptr(), flags);
        if ret < 0 {
            ffmpeg::sys::av_frame_unref(dst.as_mut_ptr());
            return Err(ret);
        }

        let copy_ret = ffmpeg::sys::av_frame_copy_props(dst.as_mut_ptr(), src.as_ptr());
        if copy_ret < 0 {
            ffmpeg::sys::av_frame_unref(dst.as_mut_ptr());
            return Err(copy_ret);
        }
    }

    Ok(())
}

fn try_map_hw_frame(dst: &mut VideoFrame, src: &VideoFrame, flags: c_int) -> Result<(), i32> {
    unsafe {
        ffmpeg::sys::av_frame_unref(dst.as_mut_ptr());
        (*dst.as_mut_ptr()).format = ffmpeg::sys::AVPixelFormat::AV_PIX_FMT_NONE as c_int;
        let ret = ffmpeg::sys::av_hwframe_map(dst.as_mut_ptr(), src.as_ptr(), flags);
        if ret < 0 {
            ffmpeg::sys::av_frame_unref(dst.as_mut_ptr());
            return Err(ret);
        }
    }

    Ok(())
}

fn try_transfer_hw_frame(dst: &mut VideoFrame, src: &VideoFrame) -> Result<(), i32> {
    unsafe {
        ffmpeg::sys::av_frame_unref(dst.as_mut_ptr());
        (*dst.as_mut_ptr()).format = ffmpeg::sys::AVPixelFormat::AV_PIX_FMT_NONE as c_int;
        let ret = ffmpeg::sys::av_hwframe_transfer_data(dst.as_mut_ptr(), src.as_ptr(), 0);
        if ret < 0 {
            ffmpeg::sys::av_frame_unref(dst.as_mut_ptr());
            return Err(ret);
        }
    }

    Ok(())
}

fn is_uploadable_frame(frame: &VideoFrame) -> bool {
    frame.width() > 0
        && frame.height() > 0
        && !matches!(frame.format(), Pixel::None | Pixel::DRM_PRIME)
}

fn pixel_label(pixel: Pixel) -> &'static str {
    match pixel {
        Pixel::None => "none",
        Pixel::DRM_PRIME => "drm_prime",
        Pixel::VIDEOTOOLBOX => "videotoolbox",
        Pixel::D3D11 | Pixel::D3D11VA_VLD => "d3d11",
        Pixel::NV12 => "nv12",
        Pixel::YUV420P => "yuv420p",
        Pixel::YUV444P => "yuv444p",
        Pixel::RGBA => "rgba",
        _ => "other",
    }
}

fn ffmpeg_err(code: i32) -> String {
    unsafe {
        let mut buf = [0u8; 256];
        ffmpeg::sys::av_strerror(code, buf.as_mut_ptr() as *mut i8, buf.len());
        CStr::from_ptr(buf.as_ptr() as *const i8)
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(target_os = "linux")]
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

#[cfg(target_os = "linux")]
const DRM_FORMAT_R8: u32 = fourcc(b'R', b'8', b' ', b' ');
#[cfg(target_os = "linux")]
const DRM_FORMAT_GR88: u32 = fourcc(b'G', b'R', b'8', b'8');

#[cfg(target_os = "windows")]
#[repr(C)]
struct FfmpegD3d11vaDeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
}

fn ffmpeg_codec_id(codec: VideoCodec) -> codec::Id {
    match codec {
        VideoCodec::H264 => codec::Id::H264,
        VideoCodec::Hevc => codec::Id::HEVC,
        VideoCodec::Av1 => codec::Id::AV1,
    }
}

fn codec_label(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "av1",
    }
}

fn decoder_hint(codec: VideoCodec) -> Option<String> {
    if let Ok(hint) = std::env::var("VIDEO_DECODER_HINT") {
        if !hint.is_empty() {
            return Some(hint);
        }
    }

    let key = match codec {
        VideoCodec::H264 => "H264_DECODER_HINT",
        VideoCodec::Hevc => "HEVC_DECODER_HINT",
        VideoCodec::Av1 => "AV1_DECODER_HINT",
    };
    std::env::var(key).ok().filter(|hint| !hint.is_empty())
}

/// Apply low-latency codec flags.
/// Must be called BEFORE the codec is opened.
unsafe fn apply_flags(ctx: *mut ffmpeg::sys::AVCodecContext, is_hw: bool) {
    // Decode and output frames immediately
    (*ctx).flags |= ffmpeg::sys::AV_CODEC_FLAG_LOW_DELAY as i32;
    // Show corrupted frames instead of dropping
    (*ctx).flags |= ffmpeg::sys::AV_CODEC_FLAG_OUTPUT_CORRUPT as i32;
    // Show all frames, even with missing references
    (*ctx).flags2 |= ffmpeg::sys::AV_CODEC_FLAG2_SHOW_ALL;

    if is_hw {
        // Hardware: single thread, GPU does the heavy lifting
        (*ctx).thread_count = 1;
    } else {
        // Software: slice-level threading for parallel decode within each frame
        (*ctx).thread_type = 2; // FF_THREAD_SLICE
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(1);
        (*ctx).thread_count = cpus.min(4);
    }
}

/// Return a minimal single-keyframe test bitstream for the given codec.
/// Uses embedded pre-encoded bitstreams (works on all platforms), with a
/// runtime software-encoder fallback for freshness.
fn generate_test_bitstream(
    codec: VideoCodec,
    chroma: VideoChromaSampling,
) -> Result<Vec<u8>, String> {
    if let Ok(data) = generate_test_bitstream_runtime(codec, chroma) {
        return Ok(data);
    }
    match chroma {
        VideoChromaSampling::Yuv420 => {
            let embedded: &[u8] = match codec {
                VideoCodec::H264 => EMBEDDED_TEST_H264,
                VideoCodec::Hevc => EMBEDDED_TEST_HEVC,
                VideoCodec::Av1 => EMBEDDED_TEST_AV1,
            };
            Ok(embedded.to_vec())
        }
        VideoChromaSampling::Yuv444 => {
            let embedded: &[u8] = match codec {
                VideoCodec::H264 => EMBEDDED_TEST_H264_YUV444,
                VideoCodec::Hevc => EMBEDDED_TEST_HEVC_YUV444,
                VideoCodec::Av1 => {
                    return Err("embedded AV1 YUV444 test bitstream unavailable".into())
                }
            };
            Ok(embedded.to_vec())
        }
    }
}

fn generate_test_bitstream_runtime(
    codec: VideoCodec,
    chroma: VideoChromaSampling,
) -> Result<Vec<u8>, String> {
    let encoder_name: &CStr = match codec {
        VideoCodec::H264 => c"libx264",
        VideoCodec::Hevc => c"libx265",
        VideoCodec::Av1 => {
            if chroma == VideoChromaSampling::Yuv444 {
                return Err("AV1 YUV444 probe generation is not implemented".into());
            }
            c"libsvtav1"
        }
    };

    unsafe {
        let enc = ffmpeg::sys::avcodec_find_encoder_by_name(encoder_name.as_ptr());
        if enc.is_null() {
            return Err(format!("{} encoder not found", encoder_name.to_str().unwrap_or("?")));
        }

        let mut ctx = ffmpeg::sys::avcodec_alloc_context3(enc);
        if ctx.is_null() {
            return Err("avcodec_alloc_context3 failed".into());
        }

        // 256x256: must exceed NVDEC minimum (128x128 for AV1/HEVC CUVID)
        (*ctx).width = 256;
        (*ctx).height = 256;
        (*ctx).pix_fmt = match chroma {
            VideoChromaSampling::Yuv420 => ffmpeg::sys::AVPixelFormat::AV_PIX_FMT_YUV420P,
            VideoChromaSampling::Yuv444 => ffmpeg::sys::AVPixelFormat::AV_PIX_FMT_YUV444P,
        };
        (*ctx).time_base = ffmpeg::sys::AVRational { num: 1, den: 30 };
        (*ctx).max_b_frames = 0;

        if codec == VideoCodec::Av1 {
            // SVT-AV1 doesn't support forced keyframes in VBR mode.
            // Use CQP with a single frame — first frame is always a keyframe.
            (*ctx).bit_rate = 0;
            (*ctx).gop_size = 0;
            let qp = std::ffi::CString::new("qp").unwrap();
            let val = std::ffi::CString::new("30").unwrap();
            ffmpeg::sys::av_opt_set((*ctx).priv_data, qp.as_ptr(), val.as_ptr(), 0);
        } else {
            (*ctx).gop_size = 1;
            (*ctx).bit_rate = 200_000;
        }
        match (codec, chroma) {
            (VideoCodec::H264, VideoChromaSampling::Yuv444) => {
                (*ctx).profile = ffmpeg::sys::FF_PROFILE_H264_HIGH_444_PREDICTIVE;
            }
            (VideoCodec::Hevc, VideoChromaSampling::Yuv444) => {
                (*ctx).profile = ffmpeg::sys::FF_PROFILE_HEVC_REXT;
            }
            _ => {}
        }

        if ffmpeg::sys::avcodec_open2(ctx, enc, ptr::null_mut()) < 0 {
            ffmpeg::sys::avcodec_free_context(&mut ctx);
            return Err("avcodec_open2 failed for test encoder".into());
        }

        let mut frame = ffmpeg::sys::av_frame_alloc();
        if frame.is_null() {
            ffmpeg::sys::avcodec_free_context(&mut ctx);
            return Err("av_frame_alloc failed".into());
        }
        (*frame).width = 256;
        (*frame).height = 256;
        (*frame).format = match chroma {
            VideoChromaSampling::Yuv420 => ffmpeg::sys::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int,
            VideoChromaSampling::Yuv444 => ffmpeg::sys::AVPixelFormat::AV_PIX_FMT_YUV444P as c_int,
        };
        (*frame).pts = 0;

        if ffmpeg::sys::av_frame_get_buffer(frame, 0) < 0 {
            ffmpeg::sys::av_frame_free(&mut frame);
            ffmpeg::sys::avcodec_free_context(&mut ctx);
            return Err("av_frame_get_buffer failed".into());
        }

        // Fill U/V planes with 128 (neutral chroma) — Y=0 is black.
        for plane in 1..3 {
            let linesize = (*frame).linesize[plane] as usize;
            let height = match chroma {
                VideoChromaSampling::Yuv420 => 128usize,
                VideoChromaSampling::Yuv444 => 256usize,
            };
            let plane_ptr = (*frame).data[plane];
            if !plane_ptr.is_null() && linesize > 0 {
                for row in 0..height {
                    ptr::write_bytes(plane_ptr.add(row * linesize), 128, linesize);
                }
            }
        }

        let mut pkt = ffmpeg::sys::av_packet_alloc();
        if pkt.is_null() {
            ffmpeg::sys::av_frame_free(&mut frame);
            ffmpeg::sys::avcodec_free_context(&mut ctx);
            return Err("av_packet_alloc failed".into());
        }

        let mut data = Vec::new();

        // Encode the frame
        ffmpeg::sys::avcodec_send_frame(ctx, frame);
        while ffmpeg::sys::avcodec_receive_packet(ctx, pkt) >= 0 {
            data.extend_from_slice(std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize));
            ffmpeg::sys::av_packet_unref(pkt);
        }

        // Flush encoder
        ffmpeg::sys::avcodec_send_frame(ctx, ptr::null_mut());
        while ffmpeg::sys::avcodec_receive_packet(ctx, pkt) >= 0 {
            data.extend_from_slice(std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize));
            ffmpeg::sys::av_packet_unref(pkt);
        }

        ffmpeg::sys::av_packet_free(&mut pkt);
        ffmpeg::sys::av_frame_free(&mut frame);
        ffmpeg::sys::avcodec_free_context(&mut ctx);

        if data.is_empty() {
            Err("test encoder produced no data".into())
        } else {
            Ok(data)
        }
    }
}

// Embedded pre-encoded 256x256 black-frame test bitstreams.
// Generated with: ffmpeg -f lavfi -i color=black:s=256x256 -frames:v 1 -c:v <encoder> ...
// 256x256 is required: NVDEC CUVID needs at least 128x128 for HEVC/AV1.
// These are used as fallback when software encoder libraries are not available
// (common on Windows where libx265/libsvtav1 may be missing from the FFmpeg build).
const EMBEDDED_TEST_H264: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9, 0x41, 0x00, 0x86, 0xc0, 0x44,
    0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf0, 0x3c, 0x50, 0xa6, 0x58, 0x00,
    0x00, 0x00, 0x01, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0, 0x00, 0x00, 0x01, 0x06, 0x05, 0xff,
    0xff, 0xaa, 0xdc, 0x45, 0xe9, 0xbd, 0xe6, 0xd9, 0x48, 0xb7, 0x96, 0x2c, 0xd8, 0x20, 0xd9,
    0x23, 0xee, 0xef, 0x78, 0x32, 0x36, 0x34, 0x20, 0x2d, 0x20, 0x63, 0x6f, 0x72, 0x65, 0x20,
    0x31, 0x36, 0x35, 0x20, 0x72, 0x33, 0x32, 0x32, 0x32, 0x20, 0x62, 0x33, 0x35, 0x36, 0x30,
    0x35, 0x61, 0x20, 0x2d, 0x20, 0x48, 0x2e, 0x32, 0x36, 0x34, 0x2f, 0x4d, 0x50, 0x45, 0x47,
    0x2d, 0x34, 0x20, 0x41, 0x56, 0x43, 0x20, 0x63, 0x6f, 0x64, 0x65, 0x63, 0x20, 0x2d, 0x20,
    0x43, 0x6f, 0x70, 0x79, 0x6c, 0x65, 0x66, 0x74, 0x20, 0x32, 0x30, 0x30, 0x33, 0x2d, 0x32,
    0x30, 0x32, 0x35, 0x20, 0x2d, 0x20, 0x68, 0x74, 0x74, 0x70, 0x3a, 0x2f, 0x2f, 0x77, 0x77,
    0x77, 0x2e, 0x76, 0x69, 0x64, 0x65, 0x6f, 0x6c, 0x61, 0x6e, 0x2e, 0x6f, 0x72, 0x67, 0x2f,
    0x78, 0x32, 0x36, 0x34, 0x2e, 0x68, 0x74, 0x6d, 0x6c, 0x20, 0x2d, 0x20, 0x6f, 0x70, 0x74,
    0x69, 0x6f, 0x6e, 0x73, 0x3a, 0x20, 0x63, 0x61, 0x62, 0x61, 0x63, 0x3d, 0x31, 0x20, 0x72,
    0x65, 0x66, 0x3d, 0x33, 0x20, 0x64, 0x65, 0x62, 0x6c, 0x6f, 0x63, 0x6b, 0x3d, 0x31, 0x3a,
    0x30, 0x3a, 0x30, 0x20, 0x61, 0x6e, 0x61, 0x6c, 0x79, 0x73, 0x65, 0x3d, 0x30, 0x78, 0x33,
    0x3a, 0x30, 0x78, 0x31, 0x31, 0x33, 0x20, 0x6d, 0x65, 0x3d, 0x68, 0x65, 0x78, 0x20, 0x73,
    0x75, 0x62, 0x6d, 0x65, 0x3d, 0x37, 0x20, 0x70, 0x73, 0x79, 0x3d, 0x31, 0x20, 0x70, 0x73,
    0x79, 0x5f, 0x72, 0x64, 0x3d, 0x31, 0x2e, 0x30, 0x30, 0x3a, 0x30, 0x2e, 0x30, 0x30, 0x20,
    0x6d, 0x69, 0x78, 0x65, 0x64, 0x5f, 0x72, 0x65, 0x66, 0x3d, 0x31, 0x20, 0x6d, 0x65, 0x5f,
    0x72, 0x61, 0x6e, 0x67, 0x65, 0x3d, 0x31, 0x36, 0x20, 0x63, 0x68, 0x72, 0x6f, 0x6d, 0x61,
    0x5f, 0x6d, 0x65, 0x3d, 0x31, 0x20, 0x74, 0x72, 0x65, 0x6c, 0x6c, 0x69, 0x73, 0x3d, 0x31,
    0x20, 0x38, 0x78, 0x38, 0x64, 0x63, 0x74, 0x3d, 0x31, 0x20, 0x63, 0x71, 0x6d, 0x3d, 0x30,
    0x20, 0x64, 0x65, 0x61, 0x64, 0x7a, 0x6f, 0x6e, 0x65, 0x3d, 0x32, 0x31, 0x2c, 0x31, 0x31,
    0x20, 0x66, 0x61, 0x73, 0x74, 0x5f, 0x70, 0x73, 0x6b, 0x69, 0x70, 0x3d, 0x31, 0x20, 0x63,
    0x68, 0x72, 0x6f, 0x6d, 0x61, 0x5f, 0x71, 0x70, 0x5f, 0x6f, 0x66, 0x66, 0x73, 0x65, 0x74,
    0x3d, 0x2d, 0x32, 0x20, 0x74, 0x68, 0x72, 0x65, 0x61, 0x64, 0x73, 0x3d, 0x38, 0x20, 0x6c,
    0x6f, 0x6f, 0x6b, 0x61, 0x68, 0x65, 0x61, 0x64, 0x5f, 0x74, 0x68, 0x72, 0x65, 0x61, 0x64,
    0x73, 0x3d, 0x31, 0x20, 0x73, 0x6c, 0x69, 0x63, 0x65, 0x64, 0x5f, 0x74, 0x68, 0x72, 0x65,
    0x61, 0x64, 0x73, 0x3d, 0x30, 0x20, 0x6e, 0x72, 0x3d, 0x30, 0x20, 0x64, 0x65, 0x63, 0x69,
    0x6d, 0x61, 0x74, 0x65, 0x3d, 0x31, 0x20, 0x69, 0x6e, 0x74, 0x65, 0x72, 0x6c, 0x61, 0x63,
    0x65, 0x64, 0x3d, 0x30, 0x20, 0x62, 0x6c, 0x75, 0x72, 0x61, 0x79, 0x5f, 0x63, 0x6f, 0x6d,
    0x70, 0x61, 0x74, 0x3d, 0x30, 0x20, 0x63, 0x6f, 0x6e, 0x73, 0x74, 0x72, 0x61, 0x69, 0x6e,
    0x65, 0x64, 0x5f, 0x69, 0x6e, 0x74, 0x72, 0x61, 0x3d, 0x30, 0x20, 0x62, 0x66, 0x72, 0x61,
    0x6d, 0x65, 0x73, 0x3d, 0x33, 0x20, 0x62, 0x5f, 0x70, 0x79, 0x72, 0x61, 0x6d, 0x69, 0x64,
    0x3d, 0x32, 0x20, 0x62, 0x5f, 0x61, 0x64, 0x61, 0x70, 0x74, 0x3d, 0x31, 0x20, 0x62, 0x5f,
    0x62, 0x69, 0x61, 0x73, 0x3d, 0x30, 0x20, 0x64, 0x69, 0x72, 0x65, 0x63, 0x74, 0x3d, 0x31,
    0x20, 0x77, 0x65, 0x69, 0x67, 0x68, 0x74, 0x62, 0x3d, 0x31, 0x20, 0x6f, 0x70, 0x65, 0x6e,
    0x5f, 0x67, 0x6f, 0x70, 0x3d, 0x30, 0x20, 0x77, 0x65, 0x69, 0x67, 0x68, 0x74, 0x70, 0x3d,
    0x32, 0x20, 0x6b, 0x65, 0x79, 0x69, 0x6e, 0x74, 0x3d, 0x32, 0x35, 0x30, 0x20, 0x6b, 0x65,
    0x79, 0x69, 0x6e, 0x74, 0x5f, 0x6d, 0x69, 0x6e, 0x3d, 0x32, 0x35, 0x20, 0x73, 0x63, 0x65,
    0x6e, 0x65, 0x63, 0x75, 0x74, 0x3d, 0x34, 0x30, 0x20, 0x69, 0x6e, 0x74, 0x72, 0x61, 0x5f,
    0x72, 0x65, 0x66, 0x72, 0x65, 0x73, 0x68, 0x3d, 0x30, 0x20, 0x72, 0x63, 0x5f, 0x6c, 0x6f,
    0x6f, 0x6b, 0x61, 0x68, 0x65, 0x61, 0x64, 0x3d, 0x34, 0x30, 0x20, 0x72, 0x63, 0x3d, 0x63,
    0x72, 0x66, 0x20, 0x6d, 0x62, 0x74, 0x72, 0x65, 0x65, 0x3d, 0x31, 0x20, 0x63, 0x72, 0x66,
    0x3d, 0x32, 0x33, 0x2e, 0x30, 0x20, 0x71, 0x63, 0x6f, 0x6d, 0x70, 0x3d, 0x30, 0x2e, 0x36,
    0x30, 0x20, 0x71, 0x70, 0x6d, 0x69, 0x6e, 0x3d, 0x30, 0x20, 0x71, 0x70, 0x6d, 0x61, 0x78,
    0x3d, 0x36, 0x39, 0x20, 0x71, 0x70, 0x73, 0x74, 0x65, 0x70, 0x3d, 0x34, 0x20, 0x69, 0x70,
    0x5f, 0x72, 0x61, 0x74, 0x69, 0x6f, 0x3d, 0x31, 0x2e, 0x34, 0x30, 0x20, 0x61, 0x71, 0x3d,
    0x31, 0x3a, 0x31, 0x2e, 0x30, 0x30, 0x00, 0x80, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00,
    0x27, 0xff, 0xfe, 0xf5, 0xb1, 0x7c, 0x0a, 0x6a, 0xe9, 0xea, 0x8a, 0x0c, 0xe8, 0x32, 0x2e,
    0xe1, 0x97, 0x24, 0xc9, 0xae, 0xb6, 0x22, 0xe2, 0x6d, 0x00, 0xf5, 0x0b, 0x06, 0x2a, 0x5d,
    0xe1, 0xc5, 0x92, 0x70, 0x8b, 0xee, 0x00, 0x00, 0x07, 0x00, 0x10, 0xea, 0x64, 0xb8, 0xb4,
    0xae, 0xcb, 0x99, 0x71, 0xd0, 0xf1,
];

const EMBEDDED_TEST_H264_YUV444: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x67, 0xf4, 0x00, 0x0d,
    0x91, 0x96, 0x81, 0x00, 0x86, 0xc0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00,
    0x03, 0x00, 0xca, 0x3c, 0x50, 0xaa, 0x80, 0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x0f,
    0x19, 0x20, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x3a, 0x24, 0x50, 0x00, 0x12, 0x05,
    0xe4, 0xf2, 0x79, 0x3c, 0x9e, 0x4f, 0x27, 0x93, 0xc9, 0xe4, 0xf2, 0x79, 0x3c, 0x9e,
    0x4f, 0x27, 0x93, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd,
    0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd,
    0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd,
    0x7a, 0xf5, 0xeb, 0xe0, 0x00, 0x00, 0x01, 0x65, 0x02, 0x08, 0x88, 0x43, 0xa2, 0x45,
    0x00, 0x01, 0x20, 0x5e, 0x4f, 0x27, 0x93, 0xc9, 0xe4, 0xf2, 0x79, 0x3c, 0x9e, 0x4f,
    0x27, 0x93, 0xc9, 0xe4, 0xf2, 0x79, 0x3d, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd,
    0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd,
    0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd,
    0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbe, 0x00, 0x00, 0x01, 0x65, 0x01, 0x02, 0x22,
    0x10, 0xe8, 0x91, 0x40, 0x00, 0x48, 0x17, 0x93, 0xc9, 0xe4, 0xf2, 0x79, 0x3c, 0x9e,
    0x4f, 0x27, 0x93, 0xc9, 0xe4, 0xf2, 0x79, 0x3c, 0x9e, 0x4f, 0x5e, 0xbd, 0x7a, 0xf5,
    0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5,
    0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5,
    0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x80, 0x00, 0x00, 0x01,
    0x65, 0x01, 0x82, 0x22, 0x10, 0xe8, 0x91, 0x40, 0x00, 0x48, 0x17, 0x93, 0xc9, 0xe4,
    0xf2, 0x79, 0x3c, 0x9e, 0x4f, 0x27, 0x93, 0xc9, 0xe4, 0xf2, 0x79, 0x3c, 0x9e, 0x4f,
    0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf,
    0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf,
    0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf, 0x5e, 0xbd, 0x7a, 0xf5, 0xeb, 0xd7, 0xaf,
    0x80,
];

const EMBEDDED_TEST_HEVC: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03,
    0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x3c, 0x95, 0x98, 0x09, 0x00, 0x00,
    0x00, 0x01, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03,
    0x00, 0x00, 0x03, 0x00, 0x3c, 0xa0, 0x08, 0x08, 0x04, 0x05, 0x96, 0x56, 0x69, 0x24, 0xca,
    0xf0, 0x16, 0x80, 0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x0f, 0x04, 0x00, 0x00,
    0x00, 0x01, 0x44, 0x01, 0xc1, 0x72, 0xb4, 0x62, 0x40, 0x00, 0x00, 0x01, 0x4e, 0x01, 0x05,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xf8, 0x2c, 0xa2, 0xde, 0x09, 0xb5, 0x17,
    0x47, 0xdb, 0xbb, 0x55, 0xa4, 0xfe, 0x7f, 0xc2, 0xfc, 0x4e, 0x78, 0x32, 0x36, 0x35, 0x20,
    0x28, 0x62, 0x75, 0x69, 0x6c, 0x64, 0x20, 0x32, 0x31, 0x35, 0x29, 0x20, 0x2d, 0x20, 0x34,
    0x2e, 0x31, 0x3a, 0x5b, 0x4c, 0x69, 0x6e, 0x75, 0x78, 0x5d, 0x5b, 0x47, 0x43, 0x43, 0x20,
    0x31, 0x35, 0x2e, 0x31, 0x2e, 0x31, 0x5d, 0x5b, 0x36, 0x34, 0x20, 0x62, 0x69, 0x74, 0x5d,
    0x20, 0x38, 0x62, 0x69, 0x74, 0x2b, 0x31, 0x30, 0x62, 0x69, 0x74, 0x2b, 0x31, 0x32, 0x62,
    0x69, 0x74, 0x20, 0x2d, 0x20, 0x48, 0x2e, 0x32, 0x36, 0x35, 0x2f, 0x48, 0x45, 0x56, 0x43,
    0x20, 0x63, 0x6f, 0x64, 0x65, 0x63, 0x20, 0x2d, 0x20, 0x43, 0x6f, 0x70, 0x79, 0x72, 0x69,
    0x67, 0x68, 0x74, 0x20, 0x32, 0x30, 0x31, 0x33, 0x2d, 0x32, 0x30, 0x31, 0x38, 0x20, 0x28,
    0x63, 0x29, 0x20, 0x4d, 0x75, 0x6c, 0x74, 0x69, 0x63, 0x6f, 0x72, 0x65, 0x77, 0x61, 0x72,
    0x65, 0x2c, 0x20, 0x49, 0x6e, 0x63, 0x20, 0x2d, 0x20, 0x68, 0x74, 0x74, 0x70, 0x3a, 0x2f,
    0x2f, 0x78, 0x32, 0x36, 0x35, 0x2e, 0x6f, 0x72, 0x67, 0x20, 0x2d, 0x20, 0x6f, 0x70, 0x74,
    0x69, 0x6f, 0x6e, 0x73, 0x3a, 0x20, 0x63, 0x70, 0x75, 0x69, 0x64, 0x3d, 0x31, 0x31, 0x31,
    0x31, 0x30, 0x33, 0x39, 0x20, 0x66, 0x72, 0x61, 0x6d, 0x65, 0x2d, 0x74, 0x68, 0x72, 0x65,
    0x61, 0x64, 0x73, 0x3d, 0x35, 0x20, 0x77, 0x70, 0x70, 0x20, 0x6e, 0x6f, 0x2d, 0x70, 0x6d,
    0x6f, 0x64, 0x65, 0x20, 0x6e, 0x6f, 0x2d, 0x70, 0x6d, 0x65, 0x20, 0x6e, 0x6f, 0x2d, 0x70,
    0x73, 0x6e, 0x72, 0x20, 0x6e, 0x6f, 0x2d, 0x73, 0x73, 0x69, 0x6d, 0x20, 0x6c, 0x6f, 0x67,
    0x2d, 0x6c, 0x65, 0x76, 0x65, 0x6c, 0x3d, 0x30, 0x20, 0x62, 0x69, 0x74, 0x64, 0x65, 0x70,
    0x74, 0x68, 0x3d, 0x38, 0x20, 0x69, 0x6e, 0x70, 0x75, 0x74, 0x2d, 0x63, 0x73, 0x70, 0x3d,
    0x31, 0x20, 0x66, 0x70, 0x73, 0x3d, 0x33, 0x30, 0x2f, 0x31, 0x20, 0x69, 0x6e, 0x70, 0x75,
    0x74, 0x2d, 0x72, 0x65, 0x73, 0x3d, 0x32, 0x35, 0x36, 0x78, 0x32, 0x35, 0x36, 0x20, 0x69,
    0x6e, 0x74, 0x65, 0x72, 0x6c, 0x61, 0x63, 0x65, 0x3d, 0x30, 0x20, 0x74, 0x6f, 0x74, 0x61,
    0x6c, 0x2d, 0x66, 0x72, 0x61, 0x6d, 0x65, 0x73, 0x3d, 0x30, 0x20, 0x6c, 0x65, 0x76, 0x65,
    0x6c, 0x2d, 0x69, 0x64, 0x63, 0x3d, 0x30, 0x20, 0x68, 0x69, 0x67, 0x68, 0x2d, 0x74, 0x69,
    0x65, 0x72, 0x3d, 0x31, 0x20, 0x75, 0x68, 0x64, 0x2d, 0x62, 0x64, 0x3d, 0x30, 0x20, 0x72,
    0x65, 0x66, 0x3d, 0x33, 0x20, 0x6e, 0x6f, 0x2d, 0x61, 0x6c, 0x6c, 0x6f, 0x77, 0x2d, 0x6e,
    0x6f, 0x6e, 0x2d, 0x63, 0x6f, 0x6e, 0x66, 0x6f, 0x72, 0x6d, 0x61, 0x6e, 0x63, 0x65, 0x20,
    0x72, 0x65, 0x70, 0x65, 0x61, 0x74, 0x2d, 0x68, 0x65, 0x61, 0x64, 0x65, 0x72, 0x73, 0x20,
    0x61, 0x6e, 0x6e, 0x65, 0x78, 0x62, 0x20, 0x6e, 0x6f, 0x2d, 0x61, 0x75, 0x64, 0x20, 0x6e,
    0x6f, 0x2d, 0x65, 0x6f, 0x62, 0x20, 0x6e, 0x6f, 0x2d, 0x65, 0x6f, 0x73, 0x20, 0x6e, 0x6f,
    0x2d, 0x68, 0x72, 0x64, 0x20, 0x69, 0x6e, 0x66, 0x6f, 0x20, 0x68, 0x61, 0x73, 0x68, 0x3d,
    0x30, 0x20, 0x74, 0x65, 0x6d, 0x70, 0x6f, 0x72, 0x61, 0x6c, 0x2d, 0x6c, 0x61, 0x79, 0x65,
    0x72, 0x73, 0x3d, 0x30, 0x20, 0x6f, 0x70, 0x65, 0x6e, 0x2d, 0x67, 0x6f, 0x70, 0x20, 0x6d,
    0x69, 0x6e, 0x2d, 0x6b, 0x65, 0x79, 0x69, 0x6e, 0x74, 0x3d, 0x32, 0x35, 0x20, 0x6b, 0x65,
    0x79, 0x69, 0x6e, 0x74, 0x3d, 0x32, 0x35, 0x30, 0x20, 0x67, 0x6f, 0x70, 0x2d, 0x6c, 0x6f,
    0x6f, 0x6b, 0x61, 0x68, 0x65, 0x61, 0x64, 0x3d, 0x30, 0x20, 0x62, 0x66, 0x72, 0x61, 0x6d,
    0x65, 0x73, 0x3d, 0x34, 0x20, 0x62, 0x2d, 0x61, 0x64, 0x61, 0x70, 0x74, 0x3d, 0x32, 0x20,
    0x62, 0x2d, 0x70, 0x79, 0x72, 0x61, 0x6d, 0x69, 0x64, 0x20, 0x62, 0x66, 0x72, 0x61, 0x6d,
    0x65, 0x2d, 0x62, 0x69, 0x61, 0x73, 0x3d, 0x30, 0x20, 0x72, 0x63, 0x2d, 0x6c, 0x6f, 0x6f,
    0x6b, 0x61, 0x68, 0x65, 0x61, 0x64, 0x3d, 0x32, 0x30, 0x20, 0x6c, 0x6f, 0x6f, 0x6b, 0x61,
    0x68, 0x65, 0x61, 0x64, 0x2d, 0x73, 0x6c, 0x69, 0x63, 0x65, 0x73, 0x3d, 0x30, 0x20, 0x73,
    0x63, 0x65, 0x6e, 0x65, 0x63, 0x75, 0x74, 0x3d, 0x34, 0x30, 0x20, 0x6e, 0x6f, 0x2d, 0x68,
    0x69, 0x73, 0x74, 0x2d, 0x73, 0x63, 0x65, 0x6e, 0x65, 0x63, 0x75, 0x74, 0x20, 0x72, 0x61,
    0x64, 0x6c, 0x3d, 0x30, 0x20, 0x6e, 0x6f, 0x2d, 0x73, 0x70, 0x6c, 0x69, 0x63, 0x65, 0x20,
    0x6e, 0x6f, 0x2d, 0x69, 0x6e, 0x74, 0x72, 0x61, 0x2d, 0x72, 0x65, 0x66, 0x72, 0x65, 0x73,
    0x68, 0x20, 0x63, 0x74, 0x75, 0x3d, 0x36, 0x34, 0x20, 0x6d, 0x69, 0x6e, 0x2d, 0x63, 0x75,
    0x2d, 0x73, 0x69, 0x7a, 0x65, 0x3d, 0x38, 0x20, 0x6e, 0x6f, 0x2d, 0x72, 0x65, 0x63, 0x74,
    0x20, 0x6e, 0x6f, 0x2d, 0x61, 0x6d, 0x70, 0x20, 0x6d, 0x61, 0x78, 0x2d, 0x74, 0x75, 0x2d,
    0x73, 0x69, 0x7a, 0x65, 0x3d, 0x33, 0x32, 0x20, 0x74, 0x75, 0x2d, 0x69, 0x6e, 0x74, 0x65,
    0x72, 0x2d, 0x64, 0x65, 0x70, 0x74, 0x68, 0x3d, 0x31, 0x20, 0x74, 0x75, 0x2d, 0x69, 0x6e,
    0x74, 0x72, 0x61, 0x2d, 0x64, 0x65, 0x70, 0x74, 0x68, 0x3d, 0x31, 0x20, 0x6c, 0x69, 0x6d,
    0x69, 0x74, 0x2d, 0x74, 0x75, 0x3d, 0x30, 0x20, 0x72, 0x64, 0x6f, 0x71, 0x2d, 0x6c, 0x65,
    0x76, 0x65, 0x6c, 0x3d, 0x30, 0x20, 0x64, 0x79, 0x6e, 0x61, 0x6d, 0x69, 0x63, 0x2d, 0x72,
    0x64, 0x3d, 0x30, 0x2e, 0x30, 0x30, 0x20, 0x6e, 0x6f, 0x2d, 0x73, 0x73, 0x69, 0x6d, 0x2d,
    0x72, 0x64, 0x20, 0x73, 0x69, 0x67, 0x6e, 0x68, 0x69, 0x64, 0x65, 0x20, 0x6e, 0x6f, 0x2d,
    0x74, 0x73, 0x6b, 0x69, 0x70, 0x20, 0x6e, 0x72, 0x2d, 0x69, 0x6e, 0x74, 0x72, 0x61, 0x3d,
    0x30, 0x20, 0x6e, 0x72, 0x2d, 0x69, 0x6e, 0x74, 0x65, 0x72, 0x3d, 0x30, 0x20, 0x6e, 0x6f,
    0x2d, 0x63, 0x6f, 0x6e, 0x73, 0x74, 0x72, 0x61, 0x69, 0x6e, 0x65, 0x64, 0x2d, 0x69, 0x6e,
    0x74, 0x72, 0x61, 0x20, 0x73, 0x74, 0x72, 0x6f, 0x6e, 0x67, 0x2d, 0x69, 0x6e, 0x74, 0x72,
    0x61, 0x2d, 0x73, 0x6d, 0x6f, 0x6f, 0x74, 0x68, 0x69, 0x6e, 0x67, 0x20, 0x6d, 0x61, 0x78,
    0x2d, 0x6d, 0x65, 0x72, 0x67, 0x65, 0x3d, 0x33, 0x20, 0x6c, 0x69, 0x6d, 0x69, 0x74, 0x2d,
    0x72, 0x65, 0x66, 0x73, 0x3d, 0x31, 0x20, 0x6e, 0x6f, 0x2d, 0x6c, 0x69, 0x6d, 0x69, 0x74,
    0x2d, 0x6d, 0x6f, 0x64, 0x65, 0x73, 0x20, 0x6d, 0x65, 0x3d, 0x31, 0x20, 0x73, 0x75, 0x62,
    0x6d, 0x65, 0x3d, 0x32, 0x20, 0x6d, 0x65, 0x72, 0x61, 0x6e, 0x67, 0x65, 0x3d, 0x35, 0x37,
    0x20, 0x74, 0x65, 0x6d, 0x70, 0x6f, 0x72, 0x61, 0x6c, 0x2d, 0x6d, 0x76, 0x70, 0x20, 0x6e,
    0x6f, 0x2d, 0x66, 0x72, 0x61, 0x6d, 0x65, 0x2d, 0x64, 0x75, 0x70, 0x20, 0x6e, 0x6f, 0x2d,
    0x68, 0x6d, 0x65, 0x20, 0x77, 0x65, 0x69, 0x67, 0x68, 0x74, 0x70, 0x20, 0x6e, 0x6f, 0x2d,
    0x77, 0x65, 0x69, 0x67, 0x68, 0x74, 0x62, 0x20, 0x6e, 0x6f, 0x2d, 0x61, 0x6e, 0x61, 0x6c,
    0x79, 0x7a, 0x65, 0x2d, 0x73, 0x72, 0x63, 0x2d, 0x70, 0x69, 0x63, 0x73, 0x20, 0x64, 0x65,
    0x62, 0x6c, 0x6f, 0x63, 0x6b, 0x3d, 0x30, 0x3a, 0x30, 0x20, 0x73, 0x61, 0x6f, 0x20, 0x6e,
    0x6f, 0x2d, 0x73, 0x61, 0x6f, 0x2d, 0x6e, 0x6f, 0x6e, 0x2d, 0x64, 0x65, 0x62, 0x6c, 0x6f,
    0x63, 0x6b, 0x20, 0x72, 0x64, 0x3d, 0x33, 0x20, 0x73, 0x65, 0x6c, 0x65, 0x63, 0x74, 0x69,
    0x76, 0x65, 0x2d, 0x73, 0x61, 0x6f, 0x3d, 0x34, 0x20, 0x65, 0x61, 0x72, 0x6c, 0x79, 0x2d,
    0x73, 0x6b, 0x69, 0x70, 0x20, 0x72, 0x73, 0x6b, 0x69, 0x70, 0x20, 0x6e, 0x6f, 0x2d, 0x66,
    0x61, 0x73, 0x74, 0x2d, 0x69, 0x6e, 0x74, 0x72, 0x61, 0x20, 0x6e, 0x6f, 0x2d, 0x74, 0x73,
    0x6b, 0x69, 0x70, 0x2d, 0x66, 0x61, 0x73, 0x74, 0x20, 0x6e, 0x6f, 0x2d, 0x63, 0x75, 0x2d,
    0x6c, 0x6f, 0x73, 0x73, 0x6c, 0x65, 0x73, 0x73, 0x20, 0x62, 0x2d, 0x69, 0x6e, 0x74, 0x72,
    0x61, 0x20, 0x6e, 0x6f, 0x2d, 0x73, 0x70, 0x6c, 0x69, 0x74, 0x72, 0x64, 0x2d, 0x73, 0x6b,
    0x69, 0x70, 0x20, 0x72, 0x64, 0x70, 0x65, 0x6e, 0x61, 0x6c, 0x74, 0x79, 0x3d, 0x30, 0x20,
    0x70, 0x73, 0x79, 0x2d, 0x72, 0x64, 0x3d, 0x32, 0x2e, 0x30, 0x30, 0x20, 0x70, 0x73, 0x79,
    0x2d, 0x72, 0x64, 0x6f, 0x71, 0x3d, 0x30, 0x2e, 0x30, 0x30, 0x20, 0x6e, 0x6f, 0x2d, 0x72,
    0x64, 0x2d, 0x72, 0x65, 0x66, 0x69, 0x6e, 0x65, 0x20, 0x6e, 0x6f, 0x2d, 0x6c, 0x6f, 0x73,
    0x73, 0x6c, 0x65, 0x73, 0x73, 0x20, 0x63, 0x62, 0x71, 0x70, 0x6f, 0x66, 0x66, 0x73, 0x3d,
    0x30, 0x20, 0x63, 0x72, 0x71, 0x70, 0x6f, 0x66, 0x66, 0x73, 0x3d, 0x30, 0x20, 0x72, 0x63,
    0x3d, 0x63, 0x72, 0x66, 0x20, 0x63, 0x72, 0x66, 0x3d, 0x32, 0x38, 0x2e, 0x30, 0x20, 0x71,
    0x63, 0x6f, 0x6d, 0x70, 0x3d, 0x30, 0x2e, 0x36, 0x30, 0x20, 0x71, 0x70, 0x73, 0x74, 0x65,
    0x70, 0x3d, 0x34, 0x20, 0x73, 0x74, 0x61, 0x74, 0x73, 0x2d, 0x77, 0x72, 0x69, 0x74, 0x65,
    0x3d, 0x30, 0x20, 0x73, 0x74, 0x61, 0x74, 0x73, 0x2d, 0x72, 0x65, 0x61, 0x64, 0x3d, 0x30,
    0x20, 0x69, 0x70, 0x72, 0x61, 0x74, 0x69, 0x6f, 0x3d, 0x31, 0x2e, 0x34, 0x30, 0x20, 0x70,
    0x62, 0x72, 0x61, 0x74, 0x69, 0x6f, 0x3d, 0x31, 0x2e, 0x33, 0x30, 0x20, 0x61, 0x71, 0x2d,
    0x6d, 0x6f, 0x64, 0x65, 0x3d, 0x32, 0x20, 0x61, 0x71, 0x2d, 0x73, 0x74, 0x72, 0x65, 0x6e,
    0x67, 0x74, 0x68, 0x3d, 0x31, 0x2e, 0x30, 0x30, 0x20, 0x63, 0x75, 0x74, 0x72, 0x65, 0x65,
    0x20, 0x7a, 0x6f, 0x6e, 0x65, 0x2d, 0x63, 0x6f, 0x75, 0x6e, 0x74, 0x3d, 0x30, 0x20, 0x6e,
    0x6f, 0x2d, 0x73, 0x74, 0x72, 0x69, 0x63, 0x74, 0x2d, 0x63, 0x62, 0x72, 0x20, 0x71, 0x67,
    0x2d, 0x73, 0x69, 0x7a, 0x65, 0x3d, 0x33, 0x32, 0x20, 0x6e, 0x6f, 0x2d, 0x72, 0x63, 0x2d,
    0x67, 0x72, 0x61, 0x69, 0x6e, 0x20, 0x71, 0x70, 0x6d, 0x61, 0x78, 0x3d, 0x36, 0x39, 0x20,
    0x71, 0x70, 0x6d, 0x69, 0x6e, 0x3d, 0x30, 0x20, 0x6e, 0x6f, 0x2d, 0x63, 0x6f, 0x6e, 0x73,
    0x74, 0x2d, 0x76, 0x62, 0x76, 0x20, 0x73, 0x61, 0x72, 0x3d, 0x31, 0x20, 0x6f, 0x76, 0x65,
    0x72, 0x73, 0x63, 0x61, 0x6e, 0x3d, 0x30, 0x20, 0x76, 0x69, 0x64, 0x65, 0x6f, 0x66, 0x6f,
    0x72, 0x6d, 0x61, 0x74, 0x3d, 0x35, 0x20, 0x72, 0x61, 0x6e, 0x67, 0x65, 0x3d, 0x30, 0x20,
    0x63, 0x6f, 0x6c, 0x6f, 0x72, 0x70, 0x72, 0x69, 0x6d, 0x3d, 0x32, 0x20, 0x74, 0x72, 0x61,
    0x6e, 0x73, 0x66, 0x65, 0x72, 0x3d, 0x32, 0x20, 0x63, 0x6f, 0x6c, 0x6f, 0x72, 0x6d, 0x61,
    0x74, 0x72, 0x69, 0x78, 0x3d, 0x32, 0x20, 0x63, 0x68, 0x72, 0x6f, 0x6d, 0x61, 0x6c, 0x6f,
    0x63, 0x3d, 0x30, 0x20, 0x64, 0x69, 0x73, 0x70, 0x6c, 0x61, 0x79, 0x2d, 0x77, 0x69, 0x6e,
    0x64, 0x6f, 0x77, 0x3d, 0x30, 0x20, 0x63, 0x6c, 0x6c, 0x3d, 0x30, 0x2c, 0x30, 0x20, 0x6d,
    0x69, 0x6e, 0x2d, 0x6c, 0x75, 0x6d, 0x61, 0x3d, 0x30, 0x20, 0x6d, 0x61, 0x78, 0x2d, 0x6c,
    0x75, 0x6d, 0x61, 0x3d, 0x32, 0x35, 0x35, 0x20, 0x6c, 0x6f, 0x67, 0x32, 0x2d, 0x6d, 0x61,
    0x78, 0x2d, 0x70, 0x6f, 0x63, 0x2d, 0x6c, 0x73, 0x62, 0x3d, 0x38, 0x20, 0x76, 0x75, 0x69,
    0x2d, 0x74, 0x69, 0x6d, 0x69, 0x6e, 0x67, 0x2d, 0x69, 0x6e, 0x66, 0x6f, 0x20, 0x76, 0x75,
    0x69, 0x2d, 0x68, 0x72, 0x64, 0x2d, 0x69, 0x6e, 0x66, 0x6f, 0x20, 0x73, 0x6c, 0x69, 0x63,
    0x65, 0x73, 0x3d, 0x31, 0x20, 0x6e, 0x6f, 0x2d, 0x6f, 0x70, 0x74, 0x2d, 0x71, 0x70, 0x2d,
    0x70, 0x70, 0x73, 0x20, 0x6e, 0x6f, 0x2d, 0x6f, 0x70, 0x74, 0x2d, 0x72, 0x65, 0x66, 0x2d,
    0x6c, 0x69, 0x73, 0x74, 0x2d, 0x6c, 0x65, 0x6e, 0x67, 0x74, 0x68, 0x2d, 0x70, 0x70, 0x73,
    0x20, 0x6e, 0x6f, 0x2d, 0x6d, 0x75, 0x6c, 0x74, 0x69, 0x2d, 0x70, 0x61, 0x73, 0x73, 0x2d,
    0x6f, 0x70, 0x74, 0x2d, 0x72, 0x70, 0x73, 0x20, 0x73, 0x63, 0x65, 0x6e, 0x65, 0x63, 0x75,
    0x74, 0x2d, 0x62, 0x69, 0x61, 0x73, 0x3d, 0x30, 0x2e, 0x30, 0x35, 0x20, 0x6e, 0x6f, 0x2d,
    0x6f, 0x70, 0x74, 0x2d, 0x63, 0x75, 0x2d, 0x64, 0x65, 0x6c, 0x74, 0x61, 0x2d, 0x71, 0x70,
    0x20, 0x6e, 0x6f, 0x2d, 0x61, 0x71, 0x2d, 0x6d, 0x6f, 0x74, 0x69, 0x6f, 0x6e, 0x20, 0x6e,
    0x6f, 0x2d, 0x68, 0x64, 0x72, 0x31, 0x30, 0x20, 0x6e, 0x6f, 0x2d, 0x68, 0x64, 0x72, 0x31,
    0x30, 0x2d, 0x6f, 0x70, 0x74, 0x20, 0x6e, 0x6f, 0x2d, 0x64, 0x68, 0x64, 0x72, 0x31, 0x30,
    0x2d, 0x6f, 0x70, 0x74, 0x20, 0x6e, 0x6f, 0x2d, 0x69, 0x64, 0x72, 0x2d, 0x72, 0x65, 0x63,
    0x6f, 0x76, 0x65, 0x72, 0x79, 0x2d, 0x73, 0x65, 0x69, 0x20, 0x61, 0x6e, 0x61, 0x6c, 0x79,
    0x73, 0x69, 0x73, 0x2d, 0x72, 0x65, 0x75, 0x73, 0x65, 0x2d, 0x6c, 0x65, 0x76, 0x65, 0x6c,
    0x3d, 0x30, 0x20, 0x61, 0x6e, 0x61, 0x6c, 0x79, 0x73, 0x69, 0x73, 0x2d, 0x73, 0x61, 0x76,
    0x65, 0x2d, 0x72, 0x65, 0x75, 0x73, 0x65, 0x2d, 0x6c, 0x65, 0x76, 0x65, 0x6c, 0x3d, 0x30,
    0x20, 0x61, 0x6e, 0x61, 0x6c, 0x79, 0x73, 0x69, 0x73, 0x2d, 0x6c, 0x6f, 0x61, 0x64, 0x2d,
    0x72, 0x65, 0x75, 0x73, 0x65, 0x2d, 0x6c, 0x65, 0x76, 0x65, 0x6c, 0x3d, 0x30, 0x20, 0x73,
    0x63, 0x61, 0x6c, 0x65, 0x2d, 0x66, 0x61, 0x63, 0x74, 0x6f, 0x72, 0x3d, 0x30, 0x20, 0x72,
    0x65, 0x66, 0x69, 0x6e, 0x65, 0x2d, 0x69, 0x6e, 0x74, 0x72, 0x61, 0x3d, 0x30, 0x20, 0x72,
    0x65, 0x66, 0x69, 0x6e, 0x65, 0x2d, 0x69, 0x6e, 0x74, 0x65, 0x72, 0x3d, 0x30, 0x20, 0x72,
    0x65, 0x66, 0x69, 0x6e, 0x65, 0x2d, 0x6d, 0x76, 0x3d, 0x31, 0x20, 0x72, 0x65, 0x66, 0x69,
    0x6e, 0x65, 0x2d, 0x63, 0x74, 0x75, 0x2d, 0x64, 0x69, 0x73, 0x74, 0x6f, 0x72, 0x74, 0x69,
    0x6f, 0x6e, 0x3d, 0x30, 0x20, 0x6e, 0x6f, 0x2d, 0x6c, 0x69, 0x6d, 0x69, 0x74, 0x2d, 0x73,
    0x61, 0x6f, 0x20, 0x63, 0x74, 0x75, 0x2d, 0x69, 0x6e, 0x66, 0x6f, 0x3d, 0x30, 0x20, 0x6e,
    0x6f, 0x2d, 0x6c, 0x6f, 0x77, 0x70, 0x61, 0x73, 0x73, 0x2d, 0x64, 0x63, 0x74, 0x20, 0x72,
    0x65, 0x66, 0x69, 0x6e, 0x65, 0x2d, 0x61, 0x6e, 0x61, 0x6c, 0x79, 0x73, 0x69, 0x73, 0x2d,
    0x74, 0x79, 0x70, 0x65, 0x3d, 0x30, 0x20, 0x63, 0x6f, 0x70, 0x79, 0x2d, 0x70, 0x69, 0x63,
    0x3d, 0x31, 0x20, 0x6d, 0x61, 0x78, 0x2d, 0x61, 0x75, 0x73, 0x69, 0x7a, 0x65, 0x2d, 0x66,
    0x61, 0x63, 0x74, 0x6f, 0x72, 0x3d, 0x31, 0x2e, 0x30, 0x20, 0x6e, 0x6f, 0x2d, 0x64, 0x79,
    0x6e, 0x61, 0x6d, 0x69, 0x63, 0x2d, 0x72, 0x65, 0x66, 0x69, 0x6e, 0x65, 0x20, 0x6e, 0x6f,
    0x2d, 0x73, 0x69, 0x6e, 0x67, 0x6c, 0x65, 0x2d, 0x73, 0x65, 0x69, 0x20, 0x6e, 0x6f, 0x2d,
    0x68, 0x65, 0x76, 0x63, 0x2d, 0x61, 0x71, 0x20, 0x6e, 0x6f, 0x2d, 0x73, 0x76, 0x74, 0x20,
    0x6e, 0x6f, 0x2d, 0x66, 0x69, 0x65, 0x6c, 0x64, 0x20, 0x71, 0x70, 0x2d, 0x61, 0x64, 0x61,
    0x70, 0x74, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x2d, 0x72, 0x61, 0x6e, 0x67, 0x65, 0x3d, 0x31,
    0x2e, 0x30, 0x30, 0x20, 0x73, 0x63, 0x65, 0x6e, 0x65, 0x63, 0x75, 0x74, 0x2d, 0x61, 0x77,
    0x61, 0x72, 0x65, 0x2d, 0x71, 0x70, 0x3d, 0x30, 0x63, 0x6f, 0x6e, 0x66, 0x6f, 0x72, 0x6d,
    0x61, 0x6e, 0x63, 0x65, 0x2d, 0x77, 0x69, 0x6e, 0x64, 0x6f, 0x77, 0x2d, 0x6f, 0x66, 0x66,
    0x73, 0x65, 0x74, 0x73, 0x20, 0x72, 0x69, 0x67, 0x68, 0x74, 0x3d, 0x30, 0x20, 0x62, 0x6f,
    0x74, 0x74, 0x6f, 0x6d, 0x3d, 0x30, 0x20, 0x64, 0x65, 0x63, 0x6f, 0x64, 0x65, 0x72, 0x2d,
    0x6d, 0x61, 0x78, 0x2d, 0x72, 0x61, 0x74, 0x65, 0x3d, 0x30, 0x20, 0x6e, 0x6f, 0x2d, 0x76,
    0x62, 0x76, 0x2d, 0x6c, 0x69, 0x76, 0x65, 0x2d, 0x6d, 0x75, 0x6c, 0x74, 0x69, 0x2d, 0x70,
    0x61, 0x73, 0x73, 0x20, 0x6e, 0x6f, 0x2d, 0x6d, 0x63, 0x73, 0x74, 0x66, 0x20, 0x6e, 0x6f,
    0x2d, 0x73, 0x62, 0x72, 0x63, 0x20, 0x6e, 0x6f, 0x2d, 0x66, 0x72, 0x61, 0x6d, 0x65, 0x2d,
    0x72, 0x63, 0x80, 0x00, 0x00, 0x01, 0x28, 0x01, 0xaf, 0x08, 0x48, 0x4e, 0x75, 0x80, 0xf9,
    0x29, 0xe3, 0xff, 0xec, 0xb5, 0x95, 0x7f, 0xd0, 0xd4, 0xd6, 0x11, 0x90, 0x54, 0xc0, 0xa9,
    0x0d, 0xc8, 0x06, 0xe8, 0xe7, 0x87, 0x80, 0x93, 0x0f, 0x34, 0xe0, 0x05, 0xc4, 0x87, 0x00,
    0x00, 0x03, 0x02, 0x02,
];

const EMBEDDED_TEST_HEVC_YUV444: &[u8] = &[
    0x00, 0x00, 0x00, 0x01, 0x46, 0x01, 0x10, 0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c,
    0x01, 0xff, 0xff, 0x04, 0x08, 0x00, 0x00, 0x03, 0x00, 0x9e, 0x08, 0x00, 0x00, 0x03,
    0x00, 0x00, 0x3c, 0x95, 0x94, 0x09, 0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x04,
    0x08, 0x00, 0x00, 0x03, 0x00, 0x9e, 0x08, 0x00, 0x00, 0x03, 0x00, 0x00, 0x3c, 0x90,
    0x01, 0x01, 0x00, 0x80, 0xb2, 0xca, 0xca, 0x94, 0x98, 0x5e, 0x02, 0xd0, 0x10, 0x00,
    0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x01, 0x90, 0x80, 0x00, 0x00, 0x00, 0x01,
    0x44, 0x01, 0xc0, 0x73, 0x18, 0x30, 0x18, 0x90, 0x00, 0x00, 0x01, 0x28, 0x01, 0xac,
    0x74, 0x41, 0x21, 0x51, 0x11, 0x0e, 0x0d, 0x9f, 0xfe, 0xef, 0x55, 0xa5, 0x60, 0xd6,
    0x8f, 0x12, 0x16, 0x0d, 0x4c, 0xbf, 0x20, 0x16, 0x35, 0x32, 0x10, 0x9a, 0x19, 0xe4,
    0x1c, 0x18, 0x50, 0x1a, 0x9c, 0x00, 0x8d, 0x80, 0x00, 0x00, 0x03, 0x02, 0x72, 0x00,
    0x00, 0x08, 0xf8, 0x00, 0x00, 0x16, 0x70,
];

const EMBEDDED_TEST_AV1: &[u8] = &[
    0x12, 0x00, 0x0a, 0x0b, 0x00, 0x00, 0x00, 0x03, 0xbf, 0xff, 0xf8, 0xd5, 0xf3, 0x00, 0x80,
    0x32, 0x17, 0x10, 0x00, 0x83, 0xe0, 0x02, 0x08, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x28,
    0xd2, 0x17, 0x10, 0xe2, 0x15, 0xde, 0x13, 0x7f, 0x59, 0x04,
];
