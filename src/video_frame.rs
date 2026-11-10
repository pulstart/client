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
    /// The renderer rotates DMA-BUF frames itself (wgpu); Glow's direct
    /// presenters do not.
    pub linux_dmabuf_rotates: bool,
}

pub struct NativeSurfaceControl {
    mask: AtomicU8,
}

impl NativeSurfaceControl {
    const LINUX_DMABUF_BIT: u8 = 1 << 0;
    const MACOS_VIDEOTOOLBOX_BIT: u8 = 1 << 1;
    const WINDOWS_D3D11_BIT: u8 = 1 << 2;
    const LINUX_DMABUF_ROTATES_BIT: u8 = 1 << 3;

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

    /// What may be decoded into native surfaces for frames shown at
    /// `rotation`: presenters that cannot rotate take CPU planes instead. The
    /// decoder asks with the rotation of the config it is decoding, so a
    /// frame's surfaces and its own `rotation` always agree.
    pub fn snapshot_for(&self, rotation: u8) -> NativeSurfaceCapabilities {
        let mut caps = self.snapshot();
        if rotation & 3 != 0 {
            caps.linux_dmabuf &= caps.linux_dmabuf_rotates;
            caps.macos_videotoolbox = false;
            caps.windows_d3d11 = false;
        }
        caps
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
        if caps.linux_dmabuf_rotates {
            mask |= Self::LINUX_DMABUF_ROTATES_BIT;
        }
        mask
    }

    fn caps_from_mask(mask: u8) -> NativeSurfaceCapabilities {
        NativeSurfaceCapabilities {
            linux_dmabuf: mask & Self::LINUX_DMABUF_BIT != 0,
            macos_videotoolbox: mask & Self::MACOS_VIDEOTOOLBOX_BIT != 0,
            windows_d3d11: mask & Self::WINDOWS_D3D11_BIT != 0,
            linux_dmabuf_rotates: mask & Self::LINUX_DMABUF_ROTATES_BIT != 0,
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
    /// Clockwise quarter turns that show the picture upright.
    pub rotation: u8,
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
            rotation: 0,
            dirty: false,
        }
    }
}

/// Source texture coordinates `(u_s, v_s)` for output coordinates `(u, v)` when
/// the picture is shown rotated `rotation` clockwise quarter turns, as
/// `[a, b, c, d, e, f]` with `u_s = a*u + b*v + c`, `v_s = d*u + e*v + f`.
pub fn uv_rotation(rotation: u8) -> [f32; 6] {
    match rotation & 3 {
        0 => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
        1 => [0.0, 1.0, 0.0, -1.0, 0.0, 1.0],
        2 => [-1.0, 0.0, 1.0, 0.0, -1.0, 1.0],
        _ => [0.0, -1.0, 1.0, 1.0, 0.0, 0.0],
    }
}

/// Size of a `width`×`height` picture after `rotation` quarter turns.
pub fn rotated_size(rotation: u8, width: u32, height: u32) -> (u32, u32) {
    if rotation & 1 == 1 {
        (height, width)
    } else {
        (width, height)
    }
}

impl VideoFrameBuffer {
    /// Picture size as shown, after `rotation`.
    pub fn display_size(&self) -> (u32, u32) {
        rotated_size(self.rotation, self.width, self.height)
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Source pixel shown at display pixel `(xd, yd)` for a `w`×`h` source
    /// turned `rotation` clockwise quarter turns, from the geometry alone.
    fn geometric_source(rotation: u8, w: u32, h: u32, xd: u32, yd: u32) -> (u32, u32) {
        match rotation {
            0 => (xd, yd),
            1 => (yd, h - 1 - xd),
            2 => (w - 1 - xd, h - 1 - yd),
            _ => (w - 1 - yd, xd),
        }
    }

    #[test]
    fn uv_rotation_samples_the_pixel_a_clockwise_turn_puts_there() {
        let (w, h) = (4u32, 2u32);
        for rotation in 0..4u8 {
            let [a, b, c, d, e, f] = uv_rotation(rotation);
            let (dw, dh) = rotated_size(rotation, w, h);
            for yd in 0..dh {
                for xd in 0..dw {
                    let u = (xd as f32 + 0.5) / dw as f32;
                    let v = (yd as f32 + 0.5) / dh as f32;
                    let (us, vs) = (a * u + b * v + c, d * u + e * v + f);
                    let sampled = (
                        (us * w as f32).floor() as u32,
                        (vs * h as f32).floor() as u32,
                    );
                    assert_eq!(
                        sampled,
                        geometric_source(rotation, w, h, xd, yd),
                        "rotation {rotation} at ({xd},{yd})"
                    );
                }
            }
        }
    }

    #[test]
    fn rotation_swaps_the_size_only_for_odd_quarter_turns() {
        assert_eq!(rotated_size(0, 1920, 1080), (1920, 1080));
        assert_eq!(rotated_size(1, 1920, 1080), (1080, 1920));
        assert_eq!(rotated_size(2, 1920, 1080), (1920, 1080));
        assert_eq!(rotated_size(3, 1920, 1080), (1080, 1920));
        let frame = VideoFrameBuffer {
            width: 2560,
            height: 1440,
            rotation: 1,
            ..Default::default()
        };
        assert_eq!(frame.display_size(), (1440, 2560));
    }

    #[test]
    fn rotation_hands_unrotatable_native_surfaces_to_cpu_planes() {
        let control = NativeSurfaceControl::new(NativeSurfaceCapabilities {
            linux_dmabuf: true,
            macos_videotoolbox: true,
            windows_d3d11: true,
            linux_dmabuf_rotates: false,
        });
        assert!(control.snapshot_for(0).macos_videotoolbox);
        let caps = control.snapshot_for(1);
        assert!(!caps.linux_dmabuf && !caps.macos_videotoolbox && !caps.windows_d3d11);
        assert!(!control.snapshot_for(2).linux_dmabuf);
        assert!(
            control.snapshot_for(0).linux_dmabuf,
            "upright takes natives again"
        );

        let wgpu_like = NativeSurfaceControl::new(NativeSurfaceCapabilities {
            linux_dmabuf: true,
            linux_dmabuf_rotates: true,
            ..Default::default()
        });
        assert!(
            wgpu_like.snapshot_for(1).linux_dmabuf,
            "wgpu rotates DMA-BUFs itself"
        );
    }
}
