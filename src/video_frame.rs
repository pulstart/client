use ffmpeg_next as ffmpeg;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::ffi::c_void;
#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoFormat {
    Rgba8,
    Yuv420p8,
    Yuv444p8,
    Nv12,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeSurfaceCapabilities {
    pub linux_dmabuf: bool,
    pub macos_videotoolbox: bool,
    pub windows_d3d11: bool,
}

pub struct NativeSurfaceControl {
    mask: AtomicU8,
}

impl NativeSurfaceControl {
    const LINUX_DMABUF_BIT: u8 = 1 << 0;
    const MACOS_VIDEOTOOLBOX_BIT: u8 = 1 << 1;
    const WINDOWS_D3D11_BIT: u8 = 1 << 2;

    pub fn new(caps: NativeSurfaceCapabilities) -> Self {
        Self {
            mask: AtomicU8::new(Self::mask_from_caps(caps)),
        }
    }

    pub fn reset(&self, caps: NativeSurfaceCapabilities) {
        self.mask
            .store(Self::mask_from_caps(caps), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> NativeSurfaceCapabilities {
        Self::caps_from_mask(self.mask.load(Ordering::Relaxed))
    }

    #[cfg(target_os = "linux")]
    pub fn disable_linux_dmabuf(&self) -> bool {
        self.disable_bit(Self::LINUX_DMABUF_BIT)
    }

    #[cfg(target_os = "macos")]
    pub fn disable_macos_videotoolbox(&self) -> bool {
        self.disable_bit(Self::MACOS_VIDEOTOOLBOX_BIT)
    }

    #[cfg(target_os = "windows")]
    pub fn disable_windows_d3d11(&self) -> bool {
        self.disable_bit(Self::WINDOWS_D3D11_BIT)
    }

    fn disable_bit(&self, bit: u8) -> bool {
        let previous = self.mask.fetch_and(!bit, Ordering::Relaxed);
        previous & bit != 0
    }

    fn mask_from_caps(caps: NativeSurfaceCapabilities) -> u8 {
        let mut mask = 0u8;
        if caps.linux_dmabuf {
            mask |= Self::LINUX_DMABUF_BIT;
        }
        if caps.macos_videotoolbox {
            mask |= Self::MACOS_VIDEOTOOLBOX_BIT;
        }
        if caps.windows_d3d11 {
            mask |= Self::WINDOWS_D3D11_BIT;
        }
        mask
    }

    fn caps_from_mask(mask: u8) -> NativeSurfaceCapabilities {
        NativeSurfaceCapabilities {
            linux_dmabuf: mask & Self::LINUX_DMABUF_BIT != 0,
            macos_videotoolbox: mask & Self::MACOS_VIDEOTOOLBOX_BIT != 0,
            windows_d3d11: mask & Self::WINDOWS_D3D11_BIT != 0,
        }
    }
}

pub type FfmpegVideoFrameHold = Arc<FfmpegVideoFrameRef>;

pub struct FfmpegVideoFrameRef {
    ptr: *mut ffmpeg::sys::AVFrame,
}

unsafe impl Send for FfmpegVideoFrameRef {}
unsafe impl Sync for FfmpegVideoFrameRef {}

impl FfmpegVideoFrameRef {
    pub fn retain(frame: &ffmpeg::util::frame::Video) -> Result<FfmpegVideoFrameHold, String> {
        unsafe {
            let mut ptr = ffmpeg::sys::av_frame_alloc();
            if ptr.is_null() {
                return Err("av_frame_alloc failed".into());
            }
            let ret = ffmpeg::sys::av_frame_ref(ptr, frame.as_ptr());
            if ret < 0 {
                ffmpeg::sys::av_frame_free(&mut ptr);
                return Err(format!("av_frame_ref failed: {ret}"));
            }
            Ok(Arc::new(Self { ptr }))
        }
    }
}

impl Drop for FfmpegVideoFrameRef {
    fn drop(&mut self) {
        unsafe {
            if !self.ptr.is_null() {
                ffmpeg::sys::av_frame_free(&mut self.ptr);
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxDmaBufFormat {
    Yuv420p8,
    Yuv444p8,
    Nv12,
}

#[cfg(target_os = "linux")]
pub struct LinuxDmaBufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub pitch: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub drm_format: u32,
}

#[cfg(target_os = "linux")]
pub struct LinuxDmaBufFrame {
    pub width: u32,
    pub height: u32,
    pub format: LinuxDmaBufFormat,
    pub planes: Vec<LinuxDmaBufPlane>,
    pub decoder_frame_ref: Option<FfmpegVideoFrameHold>,
    /// Optional native-fence fd signalling when the producer (typically the
    /// hardware decoder) is done writing these planes. When present and the
    /// EGL context advertises `EGL_ANDROID_native_fence_sync`, the renderer
    /// will import the fd via `eglCreateSyncKHR(EGL_SYNC_NATIVE_FENCE_ANDROID)`
    /// and `eglWaitSyncKHR` instead of relying on the driver's implicit stall
    /// when the DMA-BUF is first sampled. Ownership transfers to EGL on import;
    /// today no decoder in the pipeline produces this, so it stays `None` and
    /// the implicit-sync path is preserved.
    pub acquire_fence_fd: Option<OwnedFd>,
}

#[cfg(target_os = "linux")]
impl LinuxDmaBufPlane {
    fn try_clone(&self) -> Result<Self, String> {
        Ok(Self {
            fd: dup_owned_fd(&self.fd).map_err(|err| format!("dup dmabuf fd: {err}"))?,
            offset: self.offset,
            pitch: self.pitch,
            modifier: self.modifier,
            width: self.width,
            height: self.height,
            drm_format: self.drm_format,
        })
    }
}

#[cfg(target_os = "linux")]
impl LinuxDmaBufFrame {
    pub fn try_clone(&self) -> Result<Self, String> {
        let mut planes = Vec::with_capacity(self.planes.len());
        for plane in &self.planes {
            planes.push(plane.try_clone()?);
        }

        let acquire_fence_fd = match self.acquire_fence_fd.as_ref() {
            Some(fd) => Some(dup_owned_fd(fd).map_err(|err| format!("dup fence fd: {err}"))?),
            None => None,
        };

        Ok(Self {
            width: self.width,
            height: self.height,
            format: self.format,
            planes,
            decoder_frame_ref: self.decoder_frame_ref.clone(),
            acquire_fence_fd,
        })
    }
}

#[cfg(target_os = "macos")]
pub struct MacosCvPixelBuffer {
    ptr: *mut c_void,
}

#[cfg(target_os = "macos")]
unsafe impl Send for MacosCvPixelBuffer {}

#[cfg(target_os = "macos")]
impl Clone for MacosCvPixelBuffer {
    fn clone(&self) -> Self {
        unsafe {
            cf_retain(self.ptr);
        }
        Self { ptr: self.ptr }
    }
}

#[cfg(target_os = "macos")]
impl MacosCvPixelBuffer {
    pub unsafe fn retain(ptr: *mut c_void) -> Option<Self> {
        if ptr.is_null() {
            return None;
        }
        cf_retain(ptr);
        Some(Self { ptr })
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }
}

#[cfg(target_os = "macos")]
impl Drop for MacosCvPixelBuffer {
    fn drop(&mut self) {
        unsafe {
            cf_release(self.ptr);
        }
    }
}

#[cfg(target_os = "macos")]
pub struct MacosVideoToolboxFrame {
    pub width: u32,
    pub height: u32,
    pub format: VideoFormat,
    pub pixel_buffer: MacosCvPixelBuffer,
}

#[cfg(target_os = "macos")]
impl Clone for MacosVideoToolboxFrame {
    fn clone(&self) -> Self {
        Self {
            width: self.width,
            height: self.height,
            format: self.format,
            pixel_buffer: self.pixel_buffer.clone(),
        }
    }
}

#[cfg(target_os = "windows")]
pub struct WindowsComPtr {
    ptr: *mut c_void,
}

#[cfg(target_os = "windows")]
unsafe impl Send for WindowsComPtr {}

#[cfg(target_os = "windows")]
impl Clone for WindowsComPtr {
    fn clone(&self) -> Self {
        unsafe {
            com_add_ref(self.ptr);
        }
        Self { ptr: self.ptr }
    }
}

#[cfg(target_os = "windows")]
impl WindowsComPtr {
    pub unsafe fn retain(ptr: *mut c_void) -> Option<Self> {
        if ptr.is_null() {
            return None;
        }
        com_add_ref(ptr);
        Some(Self { ptr })
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsComPtr {
    fn drop(&mut self) {
        unsafe {
            com_release(self.ptr);
        }
    }
}

#[cfg(target_os = "windows")]
pub struct WindowsD3d11Frame {
    pub width: u32,
    pub height: u32,
    pub format: VideoFormat,
    pub device: WindowsComPtr,
    pub video_device: WindowsComPtr,
    pub video_context: WindowsComPtr,
    pub texture: WindowsComPtr,
    pub array_index: u32,
    pub decoder_frame_ref: Option<FfmpegVideoFrameHold>,
}

#[cfg(target_os = "windows")]
impl Clone for WindowsD3d11Frame {
    fn clone(&self) -> Self {
        Self {
            width: self.width,
            height: self.height,
            format: self.format,
            device: self.device.clone(),
            video_device: self.video_device.clone(),
            video_context: self.video_context.clone(),
            texture: self.texture.clone(),
            array_index: self.array_index,
            decoder_frame_ref: self.decoder_frame_ref.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FrameDebugTiming {
    pub frame_id: u32,
    /// Server wall-clock micros (compared to client wall clock via clock sync).
    pub server_capture_micros: u64,
    pub server_send_micros: u64,
    /// Client wall-clock micros at reassembly — paired with server times for the
    /// cross-machine `send→assemble` / `total` latency stages.
    pub client_assembled_micros: u64,
    /// Client monotonic-clock micros for the client-only stages, so an NTP step
    /// on the wall clock can't corrupt decode/queue/present durations.
    pub client_assembled_mono: u64,
    pub client_decode_start_mono: u64,
    pub client_decode_done_mono: u64,
}

pub struct VideoFrameBuffer {
    pub width: u32,
    pub height: u32,
    pub format: VideoFormat,
    pub plane0: Vec<u8>,
    pub plane1: Vec<u8>,
    pub plane2: Vec<u8>,
    #[cfg(target_os = "linux")]
    pub dmabuf: Option<LinuxDmaBufFrame>,
    #[cfg(target_os = "macos")]
    pub videotoolbox: Option<MacosVideoToolboxFrame>,
    #[cfg(target_os = "windows")]
    pub d3d11: Option<WindowsD3d11Frame>,
    pub decoder_frame_ref: Option<FfmpegVideoFrameHold>,
    pub debug_timing: Option<FrameDebugTiming>,
    pub dirty: bool,
}

impl Default for VideoFrameBuffer {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            format: VideoFormat::Rgba8,
            plane0: Vec::new(),
            plane1: Vec::new(),
            plane2: Vec::new(),
            #[cfg(target_os = "linux")]
            dmabuf: None,
            #[cfg(target_os = "macos")]
            videotoolbox: None,
            #[cfg(target_os = "windows")]
            d3d11: None,
            decoder_frame_ref: None,
            debug_timing: None,
            dirty: false,
        }
    }
}

impl VideoFrameBuffer {
    pub fn clear(&mut self) {
        self.width = 0;
        self.height = 0;
        self.dirty = false;
        self.plane0.clear();
        self.plane1.clear();
        self.plane2.clear();
        self.debug_timing = None;
        self.clear_native_surfaces();
    }

    pub fn clear_native_surfaces(&mut self) {
        #[cfg(target_os = "linux")]
        {
            self.dmabuf = None;
        }
        #[cfg(target_os = "macos")]
        {
            self.videotoolbox = None;
        }
        #[cfg(target_os = "windows")]
        {
            self.d3d11 = None;
        }
        self.decoder_frame_ref = None;
    }

    pub fn chroma_width(&self) -> u32 {
        match self.format {
            VideoFormat::Yuv444p8 => self.width,
            VideoFormat::Rgba8 | VideoFormat::Yuv420p8 | VideoFormat::Nv12 => {
                self.width.div_ceil(2)
            }
        }
    }

    pub fn chroma_height(&self) -> u32 {
        match self.format {
            VideoFormat::Yuv444p8 => self.height,
            VideoFormat::Rgba8 | VideoFormat::Yuv420p8 | VideoFormat::Nv12 => {
                self.height.div_ceil(2)
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

#[cfg(target_os = "macos")]
unsafe fn cf_retain(ptr: *mut c_void) {
    let _ = CFRetain(ptr.cast_const());
}

#[cfg(target_os = "macos")]
unsafe fn cf_release(ptr: *mut c_void) {
    if !ptr.is_null() {
        CFRelease(ptr.cast_const());
    }
}

#[cfg(target_os = "linux")]
fn dup_owned_fd(fd: &OwnedFd) -> io::Result<OwnedFd> {
    let duplicated = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
    }
}

#[cfg(target_os = "windows")]
#[repr(C)]
struct ComVtable {
    query_interface: unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[cfg(target_os = "windows")]
#[repr(C)]
struct ComObject {
    vtable: *const ComVtable,
}

#[cfg(target_os = "windows")]
unsafe fn com_add_ref(ptr: *mut c_void) {
    if !ptr.is_null() {
        let object = ptr as *mut ComObject;
        ((*(*object).vtable).add_ref)(ptr);
    }
}

#[cfg(target_os = "windows")]
unsafe fn com_release(ptr: *mut c_void) {
    if !ptr.is_null() {
        let object = ptr as *mut ComObject;
        ((*(*object).vtable).release)(ptr);
    }
}
