extern crate ffmpeg_next as ffmpeg;

#[cfg(target_os = "macos")]
use crate::video_frame::MacosVideoToolboxFrame;
#[cfg(target_os = "windows")]
use crate::video_frame::WindowsD3d11Frame;
#[cfg(target_os = "linux")]
use crate::video_frame::{LinuxDmaBufFormat, LinuxDmaBufFrame, LinuxDmaBufPlane};
use crate::video_frame::{
    NativeSurfaceCapabilities, NativeSurfaceControl, VideoFormat, VideoFrameBuffer,
};
use ffmpeg::codec::packet::Borrow as BorrowedPacket;
use ffmpeg::codec::{self, Context as CodecContext};
use ffmpeg::decoder::Video as FfmpegVideoDecoder;
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::Video as VideoFrame;
use ffmpeg::Codec;
use st_protocol::VideoCodec;
use std::ffi::CStr;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::raw::{c_int, c_void};
use std::ptr;
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
    decoder_name: String,
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
            label: "amf",
            device_type: AV_HWDEVICE_TYPE_AMF,
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
    pub fn new(codec: VideoCodec) -> Result<Self, String> {
        ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;

        // 1. User override
        if let Some(hint) = decoder_hint(codec) {
            eprintln!("[decode] trying user hint: {hint}");
            match Self::try_hint(codec, &hint) {
                Ok(d) => {
                    eprintln!("[decode] using hinted decoder: {}", d.decoder_name);
                    return Ok(d);
                }
                Err(e) => eprintln!("[decode] hint '{hint}' failed: {e}"),
            }
        }

        // 2. Hardware decoders
        for step in probe_steps(codec) {
            eprintln!("[decode] probing {}...", probe_step_name(step));
            match Self::try_probe_step(codec, step) {
                Ok(d) => {
                    eprintln!("[decode] using hardware decoder: {}", d.decoder_name);
                    return Ok(d);
                }
                Err(e) => eprintln!("[decode] {} unavailable: {e}", probe_step_name(step)),
            }
        }

        // 3. Software fallback
        eprintln!("[decode] using software {} decoder", codec_label(codec));
        Self::try_sw_decoder(codec)
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
            decoder_name: name.to_string(),
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
            decoder_name: name.to_string(),
        })
    }

    // ---- decoding ----

    /// Feed an encoded access unit to the decoder and write the latest uploadable frame into `frame_out`.
    pub fn decode_into(
        &mut self,
        nal_data: &[u8],
        frame_out: &mut VideoFrameBuffer,
    ) -> Result<bool, String> {
        self.refresh_native_surface_capabilities();
        let has_recovery_point = packet_has_recovery_point(self.codec_id, nal_data);
        if self.waiting_for_recovery && !has_recovery_point {
            return Ok(false);
        }
        if self.waiting_for_recovery && has_recovery_point {
            unsafe {
                ffmpeg::sys::avcodec_flush_buffers(self.decoder.as_mut_ptr());
            }
        }
        let pkt = BorrowedPacket::new(nal_data);

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
            return Ok(false);
        } else if self.waiting_for_recovery && has_recovery_point {
            eprintln!("[decode] {} recovered on recovery frame", self.decoder_name);
            self.waiting_for_recovery = false;
        }

        let mut produced_frame = false;
        let mut decoded = VideoFrame::empty();
        let mut mapped_frame = VideoFrame::empty();
        let mut transferred_frame = VideoFrame::empty();

        while self.decoder.receive_frame(&mut decoded).is_ok() {
            self.consecutive_failures = 0;
            self.waiting_for_recovery = false;

            #[cfg(target_os = "linux")]
            if self.linux_dmabuf_enabled {
                if let Some(hw) = self.hw.as_ref() {
                    if hw.needs_transfer(&decoded) {
                        match self.try_fill_linux_dmabuf(&decoded, frame_out) {
                            Ok(()) => {
                                produced_frame = true;
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
                                produced_frame = true;
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
                                produced_frame = true;
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
                Pixel::NV12 => copy_nv12_frame(source, frame_out),
                _ => self.copy_rgba_frame(source, frame_out)?,
            }
            produced_frame = true;
        }

        Ok(produced_frame)
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
            LinuxDmaBufFormat::Nv12 => VideoFormat::Nv12,
        };
        frame_out.plane0.clear();
        frame_out.plane1.clear();
        frame_out.plane2.clear();
        frame_out.clear_native_surfaces();
        frame_out.dmabuf = Some(LinuxDmaBufFrame {
            width: drm_frame.width(),
            height: drm_frame.height(),
            format: dmabuf_format,
            planes,
        });
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
        frame_out.clear_native_surfaces();
        frame_out.videotoolbox = Some(MacosVideoToolboxFrame {
            width: decoded.width(),
            height: decoded.height(),
            format,
            pixel_buffer,
        });
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
        });
        self.note_hw_frame_access(HwFrameAccess::DirectMap, Pixel::D3D11);
        Ok(())
    }

    /// Active decoder name (e.g. "h264_vaapi", "h264 (software)").
    pub fn name(&self) -> &str {
        &self.decoder_name
    }

    pub fn waiting_for_recovery(&self) -> bool {
        self.waiting_for_recovery
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

/// Apply low-latency codec flags (moonlight-qt style).
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
