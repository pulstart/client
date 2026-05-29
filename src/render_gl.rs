#[cfg(target_os = "macos")]
use crate::render_macos::{MacosDirectVideoPresenter, MacosVideoToolboxImporter, RectYuvPipeline};
#[cfg(target_os = "macos")]
use crate::render_macos_metal::MacosMetalVideoPresenter;
#[cfg(target_os = "windows")]
use crate::render_windows::{WindowsD3d11Importer, WindowsDirectVideoPresenter};
#[cfg(target_os = "windows")]
use crate::render_windows_native::WindowsNativeVideoPresenter;
#[cfg(target_os = "macos")]
use crate::video_frame::MacosVideoToolboxFrame;
#[cfg(target_os = "linux")]
use crate::video_frame::{LinuxDmaBufFormat, LinuxDmaBufFrame, LinuxDmaBufPlane};
use crate::video_frame::{
    NativeSurfaceCapabilities, NativeSurfaceControl, VideoFormat, VideoFrameBuffer,
};
#[cfg(target_os = "linux")]
use eframe::egui_glow;
use eframe::{egui, glow};
use glow::{HasContext as _, PixelUnpackData};
#[cfg(target_os = "linux")]
use khronos_egl as egl;
#[cfg(target_os = "macos")]
use std::collections::VecDeque;
#[cfg(target_os = "linux")]
use std::ffi::c_void;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::Mutex;

#[cfg(target_os = "macos")]
const MACOS_FRAME_KEEPALIVE_DEPTH: usize = 6;

fn direct_present_enabled() -> bool {
    std::env::var("ST_CLIENT_DISABLE_DIRECT_PRESENT")
        .map(|raw| raw == "0")
        .unwrap_or(true)
}

pub struct NativeVideoTexture {
    texture: Option<glow::Texture>,
    texture_id: Option<egui::TextureId>,
    width: u32,
    height: u32,
    yuv_pipeline: Option<YuvPipeline>,
    #[cfg(target_os = "linux")]
    linux_dmabuf_supported: bool,
    #[cfg(target_os = "linux")]
    dmabuf_importer: Option<LinuxDmabufImporter>,
    #[cfg(target_os = "linux")]
    linux_direct_presenter: Option<LinuxDirectVideoPresenter>,
    #[cfg(target_os = "macos")]
    macos_videotoolbox_supported: bool,
    #[cfg(target_os = "macos")]
    macos_videotoolbox_importer: Option<MacosVideoToolboxImporter>,
    #[cfg(target_os = "macos")]
    rect_yuv_pipeline: Option<RectYuvPipeline>,
    #[cfg(target_os = "macos")]
    macos_metal_presenter: Option<MacosMetalVideoPresenter>,
    #[cfg(target_os = "macos")]
    macos_direct_presenter: Option<MacosDirectVideoPresenter>,
    #[cfg(target_os = "macos")]
    macos_recent_frames: VecDeque<MacosVideoToolboxFrame>,
    #[cfg(target_os = "windows")]
    windows_d3d11_supported: bool,
    #[cfg(target_os = "windows")]
    windows_d3d11_importer: Option<WindowsD3d11Importer>,
    #[cfg(target_os = "windows")]
    windows_direct_presenter: Option<WindowsDirectVideoPresenter>,
    #[cfg(target_os = "windows")]
    windows_native_presenter: Option<WindowsNativeVideoPresenter>,
    #[cfg(target_os = "windows")]
    windows_overlayless_preferred: bool,
}

struct YuvPipeline {
    program: glow::Program,
    framebuffer: glow::Framebuffer,
    vao: glow::VertexArray,
    _vbo: glow::Buffer,
    luma_tex: glow::Texture,
    chroma_tex: glow::Texture,
    chroma_v_tex: glow::Texture,
    mode_uniform: glow::UniformLocation,
    last_format: Option<VideoFormat>,
    last_luma_size: (u32, u32),
    last_chroma_size: (u32, u32),
    luma_pbo: PboRing,
    chroma_pbo: PboRing,
    chroma_v_pbo: PboRing,
    pbo_enabled: bool,
}

/// Triple-buffered pixel buffer object ring. Lets glTexSubImage2D pull from
/// previously-written GPU memory instead of blocking on a fresh client-mem
/// upload each frame. When the mapped write is unsynchronized and each buffer
/// is orphaned before the map, the driver can trivially avoid GPU/CPU fencing.
const PBO_RING_SIZE: usize = 3;

struct PboRing {
    buffers: [Option<glow::Buffer>; PBO_RING_SIZE],
    capacities: [usize; PBO_RING_SIZE],
    current: usize,
}

impl PboRing {
    fn new() -> Self {
        Self {
            buffers: [None, None, None],
            capacities: [0; PBO_RING_SIZE],
            current: 0,
        }
    }

    /// Upload `data` through the next ring slot and return Ok(true) when the
    /// data landed in a PBO (caller should then issue tex_sub_image_2d with
    /// BufferOffset(0)). Returns Ok(false) when PBO path isn't usable and the
    /// caller must fall back to a direct slice upload.
    unsafe fn upload(&mut self, gl: &glow::Context, data: &[u8]) -> Result<bool, String> {
        let idx = self.current;
        let buffer = match self.buffers[idx] {
            Some(b) => b,
            None => {
                let b = gl
                    .create_buffer()
                    .map_err(|e| format!("create_buffer (pbo): {e}"))?;
                self.buffers[idx] = Some(b);
                b
            }
        };
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, Some(buffer));
        if self.capacities[idx] < data.len() {
            gl.buffer_data_size(
                glow::PIXEL_UNPACK_BUFFER,
                data.len() as i32,
                glow::STREAM_DRAW,
            );
            self.capacities[idx] = data.len();
        } else {
            // Orphan the previous contents to avoid an implicit GPU fence.
            gl.buffer_data_size(
                glow::PIXEL_UNPACK_BUFFER,
                self.capacities[idx] as i32,
                glow::STREAM_DRAW,
            );
        }
        let ptr = gl.map_buffer_range(
            glow::PIXEL_UNPACK_BUFFER,
            0,
            data.len() as i32,
            glow::MAP_WRITE_BIT | glow::MAP_INVALIDATE_BUFFER_BIT | glow::MAP_UNSYNCHRONIZED_BIT,
        );
        if ptr.is_null() {
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            return Ok(false);
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        gl.unmap_buffer(glow::PIXEL_UNPACK_BUFFER);
        self.current = (idx + 1) % PBO_RING_SIZE;
        Ok(true)
    }

    unsafe fn unbind(gl: &glow::Context) {
        gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
    }

    #[allow(dead_code)]
    unsafe fn destroy(&mut self, gl: &glow::Context) {
        for slot in &mut self.buffers {
            if let Some(b) = slot.take() {
                gl.delete_buffer(b);
            }
        }
        self.capacities = [0; PBO_RING_SIZE];
        self.current = 0;
    }
}

#[cfg(target_os = "linux")]
struct LinuxDmabufImporter {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    image_target_texture_2d: GlEglImageTargetTexture2DOes,
    luma_tex: glow::Texture,
    chroma_tex: glow::Texture,
    chroma_v_tex: glow::Texture,
    /// Whether the EGL display advertises `EGL_ANDROID_native_fence_sync`.
    /// When true and a DMA-BUF frame carries an acquire fence fd, the import
    /// path inserts a GPU-side wait via `eglCreateSyncKHR` +
    /// `EGL_SYNC_NATIVE_FENCE_ANDROID` + `eglWaitSyncKHR` before sampling.
    native_fence_sync_supported: bool,
}

#[cfg(target_os = "linux")]
type GlEglImageTargetTexture2DOes = unsafe extern "system" fn(u32, *const c_void);

#[cfg(target_os = "linux")]
#[derive(Clone, Default)]
struct LinuxDirectVideoPresenter {
    inner: Arc<Mutex<LinuxDirectVideoPresenterState>>,
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct LinuxDirectVideoPresenterState {
    enabled: bool,
    logged_success: bool,
    frame: Option<LinuxDmaBufFrame>,
    importer: Option<LinuxDmabufImporter>,
    pipeline: Option<YuvPipeline>,
}

#[cfg(target_os = "linux")]
impl LinuxDirectVideoPresenter {
    fn new() -> Self {
        let state = LinuxDirectVideoPresenterState {
            enabled: true,
            ..Default::default()
        };
        Self {
            inner: Arc::new(Mutex::new(state)),
        }
    }

    fn is_enabled(&self) -> bool {
        self.inner.lock().unwrap().enabled
    }

    fn has_frame(&self) -> bool {
        let state = self.inner.lock().unwrap();
        state.enabled && state.frame.is_some()
    }

    fn clear(&self) {
        let mut state = self.inner.lock().unwrap();
        state.frame = None;
    }

    fn stage_frame(&self, frame: &LinuxDmaBufFrame) -> bool {
        let mut state = self.inner.lock().unwrap();
        if !state.enabled {
            return false;
        }

        match frame.try_clone() {
            Ok(frame) => {
                state.frame = Some(frame);
                true
            }
            Err(err) => {
                eprintln!("[render] disabling Linux direct present path: {err}");
                state.disable();
                false
            }
        }
    }

    fn paint_callback(&self, rect: egui::Rect) -> egui::PaintCallback {
        let inner = Arc::clone(&self.inner);
        egui::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(move |info, painter| {
                let mut state = inner.lock().unwrap();
                state.render(info, painter);
            })),
        }
    }
}

#[cfg(target_os = "linux")]
impl LinuxDirectVideoPresenterState {
    fn render(&mut self, info: egui::PaintCallbackInfo, painter: &egui_glow::Painter) {
        if !self.enabled {
            return;
        }
        let Some(frame) = self.frame.as_ref() else {
            return;
        };
        let gl = painter.gl();

        if self.importer.is_none() {
            match LinuxDmabufImporter::new(gl.as_ref()) {
                Ok(importer) => self.importer = Some(importer),
                Err(err) => {
                    eprintln!("[render] disabling Linux direct present path: {err}");
                    self.disable();
                    return;
                }
            }
        }
        if self.pipeline.is_none() {
            match YuvPipeline::new(gl.as_ref()) {
                Ok(pipeline) => self.pipeline = Some(pipeline),
                Err(err) => {
                    eprintln!("[render] disabling Linux direct present path: {err}");
                    self.disable();
                    return;
                }
            }
        }

        let clip_rect = info.clip_rect_in_pixels();
        unsafe {
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(
                clip_rect.left_px,
                clip_rect.from_bottom_px,
                clip_rect.width_px,
                clip_rect.height_px,
            );
        }

        let result = self
            .importer
            .as_mut()
            .expect("importer set")
            .import_and_render_to_current(
                gl.as_ref(),
                self.pipeline.as_mut().expect("pipeline set"),
                frame,
            );

        unsafe {
            gl.disable(glow::SCISSOR_TEST);
        }

        if let Err(err) = result {
            eprintln!("[render] disabling Linux direct present path: {err}");
            self.disable();
        } else if !self.logged_success {
            eprintln!("[render] Linux direct present path enabled");
            self.logged_success = true;
        }
    }

    fn disable(&mut self) {
        self.enabled = false;
        self.logged_success = false;
        self.frame = None;
        self.importer = None;
        self.pipeline = None;
    }
}

impl NativeVideoTexture {
    pub fn new(gl: Option<&Arc<glow::Context>>) -> Self {
        let direct_present = direct_present_enabled();
        Self {
            texture: None,
            texture_id: None,
            width: 0,
            height: 0,
            yuv_pipeline: None,
            #[cfg(target_os = "linux")]
            linux_dmabuf_supported: gl.map(|gl| LinuxDmabufImporter::probe(gl)).unwrap_or(false),
            #[cfg(target_os = "linux")]
            dmabuf_importer: None,
            #[cfg(target_os = "linux")]
            linux_direct_presenter: direct_present.then(LinuxDirectVideoPresenter::new),
            #[cfg(target_os = "macos")]
            macos_videotoolbox_supported: MacosMetalVideoPresenter::supported()
                || gl
                    .map(|gl| MacosVideoToolboxImporter::supports_extensions(gl))
                    .unwrap_or(false),
            #[cfg(target_os = "macos")]
            macos_videotoolbox_importer: None,
            #[cfg(target_os = "macos")]
            rect_yuv_pipeline: None,
            #[cfg(target_os = "macos")]
            macos_metal_presenter: direct_present.then(MacosMetalVideoPresenter::new),
            #[cfg(target_os = "macos")]
            macos_direct_presenter: direct_present.then(MacosDirectVideoPresenter::new),
            #[cfg(target_os = "macos")]
            macos_recent_frames: VecDeque::with_capacity(MACOS_FRAME_KEEPALIVE_DEPTH),
            #[cfg(target_os = "windows")]
            windows_d3d11_supported: gl
                .map(|gl| WindowsD3d11Importer::probe(gl))
                .unwrap_or(false),
            #[cfg(target_os = "windows")]
            windows_d3d11_importer: None,
            #[cfg(target_os = "windows")]
            windows_direct_presenter: direct_present.then(WindowsDirectVideoPresenter::new),
            #[cfg(target_os = "windows")]
            windows_native_presenter: direct_present.then(WindowsNativeVideoPresenter::new),
            #[cfg(target_os = "windows")]
            windows_overlayless_preferred: false,
        }
    }

    pub fn has_frame(&self) -> bool {
        if self.width == 0 || self.height == 0 {
            return false;
        }

        #[cfg(target_os = "linux")]
        if self
            .linux_direct_presenter
            .as_ref()
            .map(|presenter| presenter.has_frame())
            .unwrap_or(false)
        {
            return true;
        }

        #[cfg(target_os = "macos")]
        if self
            .macos_metal_presenter
            .as_ref()
            .map(|presenter| presenter.has_frame())
            .unwrap_or(false)
        {
            return true;
        }

        #[cfg(target_os = "macos")]
        if self
            .macos_direct_presenter
            .as_ref()
            .map(|presenter| presenter.has_frame())
            .unwrap_or(false)
        {
            return true;
        }

        #[cfg(target_os = "windows")]
        if self
            .windows_native_presenter
            .as_ref()
            .map(|presenter| presenter.has_frame())
            .unwrap_or(false)
        {
            return true;
        }

        #[cfg(target_os = "windows")]
        if self
            .windows_direct_presenter
            .as_ref()
            .map(|presenter| presenter.has_frame())
            .unwrap_or(false)
        {
            return true;
        }

        self.texture_id.is_some()
    }

    pub fn texture_id(&self) -> Option<egui::TextureId> {
        self.texture_id
    }

    pub fn size_vec2(&self) -> egui::Vec2 {
        egui::vec2(self.width as f32, self.height as f32)
    }

    #[allow(dead_code)]
    #[cfg(target_os = "macos")]
    pub fn current_native_video_rect(&self) -> Option<egui::Rect> {
        self.macos_metal_presenter
            .as_ref()
            .and_then(MacosMetalVideoPresenter::current_rect)
    }

    #[allow(dead_code)]
    #[cfg(target_os = "windows")]
    pub fn current_native_video_rect(&self) -> Option<egui::Rect> {
        self.windows_native_presenter
            .as_ref()
            .and_then(WindowsNativeVideoPresenter::current_rect)
    }

    #[allow(dead_code)]
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn current_native_video_rect(&self) -> Option<egui::Rect> {
        None
    }

    #[cfg(target_os = "windows")]
    pub fn set_windows_overlayless_preferred(&mut self, preferred: bool) {
        self.windows_overlayless_preferred = preferred;
        if let Some(presenter) = self.windows_native_presenter.as_mut() {
            presenter.set_preferred(preferred);
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub fn set_windows_overlayless_preferred(&mut self, _preferred: bool) {}

    pub fn occludes_egui_overlay(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            return self
                .windows_native_presenter
                .as_ref()
                .map(|presenter| presenter.occludes_egui_overlay())
                .unwrap_or(false);
        }

        #[allow(unreachable_code)]
        false
    }

    pub fn clear_frame(&mut self) {
        self.width = 0;
        self.height = 0;
        #[cfg(target_os = "linux")]
        if let Some(presenter) = self.linux_direct_presenter.as_ref() {
            presenter.clear();
        }
        #[cfg(target_os = "macos")]
        if let Some(presenter) = self.macos_metal_presenter.as_mut() {
            presenter.clear();
        }
        #[cfg(target_os = "macos")]
        if let Some(presenter) = self.macos_direct_presenter.as_ref() {
            presenter.clear();
        }
        #[cfg(target_os = "macos")]
        {
            self.macos_recent_frames.clear();
        }
        #[cfg(target_os = "windows")]
        if let Some(presenter) = self.windows_native_presenter.as_mut() {
            presenter.clear();
        }
        #[cfg(target_os = "windows")]
        if let Some(presenter) = self.windows_direct_presenter.as_ref() {
            presenter.clear();
        }
    }

    #[cfg(target_os = "macos")]
    fn retain_recent_macos_frame(&mut self, frame: &MacosVideoToolboxFrame) {
        self.macos_recent_frames.push_back(frame.clone());
        while self.macos_recent_frames.len() > MACOS_FRAME_KEEPALIVE_DEPTH {
            self.macos_recent_frames.pop_front();
        }
    }

    pub fn stage_direct_frame(&mut self, video: &VideoFrameBuffer) -> bool {
        #[cfg(target_os = "linux")]
        {
            if self.linux_dmabuf_supported {
                if let Some(frame) = video.dmabuf.as_ref() {
                    if let Some(presenter) = self.linux_direct_presenter.as_ref() {
                        if presenter.is_enabled() && presenter.stage_frame(frame) {
                            self.width = video.width;
                            self.height = video.height;
                            return true;
                        }
                    }
                }
            }
        }

        #[cfg(target_os = "macos")]
        {
            if !self.macos_videotoolbox_supported {
                return false;
            }
            let Some(frame) = video.videotoolbox.as_ref() else {
                return false;
            };
            if let Some(presenter) = self.macos_metal_presenter.as_mut() {
                if presenter.is_enabled() && presenter.stage_frame(frame) {
                    self.retain_recent_macos_frame(frame);
                    self.width = video.width;
                    self.height = video.height;
                    return true;
                }
            }
            let Some(presenter) = self.macos_direct_presenter.as_ref() else {
                return false;
            };
            if !presenter.is_enabled() {
                return false;
            }

            presenter.stage_frame(frame);
            self.retain_recent_macos_frame(frame);
            self.width = video.width;
            self.height = video.height;
            return true;
        }

        #[cfg(target_os = "windows")]
        {
            if !self.windows_d3d11_supported {
                return false;
            }
            let Some(frame) = video.d3d11.as_ref() else {
                return false;
            };
            if self.windows_overlayless_preferred {
                if let Some(presenter) = self.windows_native_presenter.as_mut() {
                    if presenter.is_enabled() && presenter.stage_frame(frame) {
                        self.width = video.width;
                        self.height = video.height;
                        return true;
                    }
                }
            }
            let Some(presenter) = self.windows_direct_presenter.as_ref() else {
                return false;
            };
            if !presenter.is_enabled() {
                return false;
            }

            presenter.stage_frame(frame);
            self.width = video.width;
            self.height = video.height;
            return true;
        }

        #[allow(unreachable_code)]
        false
    }

    pub fn paint_direct_if_available(
        &mut self,
        #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] frame: &eframe::Frame,
        ui: &egui::Ui,
        rect: egui::Rect,
    ) -> bool {
        #[cfg(target_os = "linux")]
        {
            let Some(presenter) = self.linux_direct_presenter.as_ref() else {
                return false;
            };
            if !presenter.has_frame() {
                return false;
            }

            ui.painter().add(presenter.paint_callback(rect));
            return true;
        }

        #[cfg(target_os = "macos")]
        {
            if let Some(presenter) = self.macos_metal_presenter.as_mut() {
                if presenter.has_frame()
                    && presenter.present(frame, rect, ui.ctx().pixels_per_point())
                {
                    return true;
                }
            }

            let Some(presenter) = self.macos_direct_presenter.as_ref() else {
                return false;
            };
            if !presenter.has_frame() {
                return false;
            }

            ui.painter().add(presenter.paint_callback(rect));
            return true;
        }

        #[cfg(target_os = "windows")]
        {
            if let Some(presenter) = self.windows_native_presenter.as_mut() {
                if presenter.has_frame()
                    && presenter.present(frame, rect, ui.ctx().pixels_per_point())
                {
                    return true;
                }
            }

            let Some(presenter) = self.windows_direct_presenter.as_ref() else {
                return false;
            };
            if !presenter.has_frame() {
                return false;
            }

            ui.painter().add(presenter.paint_callback(rect));
            return true;
        }

        #[allow(unreachable_code)]
        false
    }

    pub fn native_surface_capabilities(&self) -> NativeSurfaceCapabilities {
        NativeSurfaceCapabilities {
            #[cfg(target_os = "linux")]
            linux_dmabuf: self.linux_dmabuf_supported,
            #[cfg(not(target_os = "linux"))]
            linux_dmabuf: false,
            #[cfg(target_os = "macos")]
            macos_videotoolbox: self.macos_videotoolbox_supported,
            #[cfg(not(target_os = "macos"))]
            macos_videotoolbox: false,
            #[cfg(target_os = "windows")]
            windows_d3d11: self.windows_d3d11_supported,
            #[cfg(not(target_os = "windows"))]
            windows_d3d11: false,
        }
    }

    pub fn upload(
        &mut self,
        frame: &mut eframe::Frame,
        video: &VideoFrameBuffer,
        native_surfaces: &NativeSurfaceControl,
    ) -> Result<(), String> {
        let gl = frame
            .gl()
            .cloned()
            .ok_or_else(|| "Glow renderer unavailable".to_string())?;

        #[cfg(target_os = "windows")]
        if let Some(d3d11) = video.d3d11.as_ref() {
            let output_texture = self.ensure_output_texture_handle(frame, gl.as_ref())?;
            if self.windows_d3d11_importer.is_none() {
                match WindowsD3d11Importer::new(gl.as_ref()) {
                    Ok(importer) => self.windows_d3d11_importer = Some(importer),
                    Err(err) => {
                        eprintln!("[render] disabling D3D11 surface path: {err}");
                        let _ = native_surfaces.disable_windows_d3d11();
                        return Ok(());
                    }
                }
            }
            let render_result = self
                .windows_d3d11_importer
                .as_mut()
                .ok_or_else(|| "failed to initialize D3D11 importer".to_string())?
                .import_and_render(output_texture, d3d11);
            match render_result {
                Ok(()) => {
                    self.width = video.width;
                    self.height = video.height;
                    return Ok(());
                }
                Err(err) => {
                    eprintln!("[render] disabling D3D11 surface path: {err}");
                    if let Some(importer) = self.windows_d3d11_importer.as_mut() {
                        importer.release_registration();
                    }
                    self.windows_d3d11_importer = None;
                    let _ = native_surfaces.disable_windows_d3d11();
                    return Ok(());
                }
            }
        }

        #[cfg(target_os = "windows")]
        if let Some(importer) = self.windows_d3d11_importer.as_mut() {
            if importer.has_registration() {
                importer.release_registration();
                self.width = 0;
                self.height = 0;
            }
        }

        let output_texture =
            self.ensure_output_texture(frame, gl.as_ref(), video.width, video.height)?;

        #[cfg(target_os = "linux")]
        if let Some(dmabuf) = video.dmabuf.as_ref() {
            if self.dmabuf_importer.is_none() {
                match LinuxDmabufImporter::new(gl.as_ref()) {
                    Ok(importer) => self.dmabuf_importer = Some(importer),
                    Err(err) => {
                        eprintln!("[render] disabling dmabuf import path: {err}");
                        let _ = native_surfaces.disable_linux_dmabuf();
                        return Ok(());
                    }
                }
            }
            if self.yuv_pipeline.is_none() {
                self.yuv_pipeline = Some(YuvPipeline::new(gl.as_ref())?);
            }
            let importer = self
                .dmabuf_importer
                .as_mut()
                .ok_or_else(|| "failed to initialize dmabuf importer".to_string())?;
            let pipeline = self
                .yuv_pipeline
                .as_mut()
                .ok_or_else(|| "failed to initialize YUV pipeline".to_string())?;
            match importer.import_and_render(gl.as_ref(), output_texture, pipeline, dmabuf) {
                Ok(()) => {
                    self.width = video.width;
                    self.height = video.height;
                    return Ok(());
                }
                Err(err) => {
                    eprintln!("[render] disabling dmabuf import path: {err}");
                    self.dmabuf_importer = None;
                    let _ = native_surfaces.disable_linux_dmabuf();
                    return Ok(());
                }
            }
        }

        #[cfg(target_os = "macos")]
        if let Some(videotoolbox) = video.videotoolbox.as_ref() {
            if self.macos_videotoolbox_importer.is_none() {
                match MacosVideoToolboxImporter::new(gl.as_ref()) {
                    Ok(importer) => self.macos_videotoolbox_importer = Some(importer),
                    Err(err) => {
                        eprintln!("[render] disabling VideoToolbox surface path: {err}");
                        let _ = native_surfaces.disable_macos_videotoolbox();
                        return Ok(());
                    }
                }
            }
            if self.rect_yuv_pipeline.is_none() {
                match RectYuvPipeline::new(gl.as_ref()) {
                    Ok(pipeline) => self.rect_yuv_pipeline = Some(pipeline),
                    Err(err) => {
                        eprintln!("[render] disabling VideoToolbox surface path: {err}");
                        self.macos_videotoolbox_importer = None;
                        let _ = native_surfaces.disable_macos_videotoolbox();
                        return Ok(());
                    }
                }
            }
            let importer = self
                .macos_videotoolbox_importer
                .as_mut()
                .ok_or_else(|| "failed to initialize VideoToolbox importer".to_string())?;
            let pipeline = self
                .rect_yuv_pipeline
                .as_mut()
                .ok_or_else(|| "failed to initialize rectangle YUV pipeline".to_string())?;
            match importer.import_and_render(gl.as_ref(), output_texture, pipeline, videotoolbox) {
                Ok(()) => {
                    self.retain_recent_macos_frame(videotoolbox);
                    self.width = video.width;
                    self.height = video.height;
                    return Ok(());
                }
                Err(err) => {
                    eprintln!("[render] disabling VideoToolbox surface path: {err}");
                    self.macos_videotoolbox_importer = None;
                    let _ = native_surfaces.disable_macos_videotoolbox();
                    return Ok(());
                }
            }
        }

        match video.format {
            VideoFormat::Rgba8 => upload_rgba(
                gl.as_ref(),
                output_texture,
                video.width,
                video.height,
                &video.plane0,
            )?,
            VideoFormat::Yuv420p8 | VideoFormat::Yuv444p8 | VideoFormat::Nv12 => {
                let pipeline = self.ensure_yuv_pipeline(gl.as_ref())?;
                pipeline.upload_and_render(gl.as_ref(), output_texture, video)?;
            }
        }

        self.width = video.width;
        self.height = video.height;
        Ok(())
    }

    fn ensure_output_texture(
        &mut self,
        frame: &mut eframe::Frame,
        gl: &glow::Context,
        width: u32,
        height: u32,
    ) -> Result<glow::Texture, String> {
        let texture = self.ensure_output_texture_handle(frame, gl)?;
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            if self.width != width || self.height != height {
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    width as i32,
                    height as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    PixelUnpackData::Slice(None),
                );
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
        }

        Ok(texture)
    }

    fn ensure_output_texture_handle(
        &mut self,
        frame: &mut eframe::Frame,
        gl: &glow::Context,
    ) -> Result<glow::Texture, String> {
        if self.texture.is_none() {
            let texture = unsafe { create_texture(gl)? };
            let texture_id = frame.register_native_glow_texture(texture);
            self.texture = Some(texture);
            self.texture_id = Some(texture_id);
        }

        self.texture
            .ok_or_else(|| "failed to create output texture".to_string())
    }

    fn ensure_yuv_pipeline(&mut self, gl: &glow::Context) -> Result<&mut YuvPipeline, String> {
        if self.yuv_pipeline.is_none() {
            self.yuv_pipeline = Some(YuvPipeline::new(gl)?);
        }
        self.yuv_pipeline
            .as_mut()
            .ok_or_else(|| "failed to initialize YUV pipeline".to_string())
    }
}

impl YuvPipeline {
    fn new(gl: &glow::Context) -> Result<Self, String> {
        ensure_yuv_gl_support(gl)?;
        let program = unsafe { create_yuv_program(gl)? };
        let framebuffer = unsafe {
            gl.create_framebuffer()
                .map_err(|err| format!("create_framebuffer: {err}"))?
        };
        let vao = unsafe {
            gl.create_vertex_array()
                .map_err(|err| format!("create_vertex_array: {err}"))?
        };
        let vbo = unsafe {
            gl.create_buffer()
                .map_err(|err| format!("create_buffer: {err}"))?
        };
        let luma_tex = unsafe { create_input_texture(gl)? };
        let chroma_tex = unsafe { create_input_texture(gl)? };
        let chroma_v_tex = unsafe { create_input_texture(gl)? };

        let vertices: [f32; 16] = [
            -1.0, -1.0, 0.0, 0.0, 1.0, -1.0, 1.0, 0.0, -1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0,
        ];
        let vertex_bytes = unsafe {
            std::slice::from_raw_parts(
                vertices.as_ptr() as *const u8,
                std::mem::size_of_val(&vertices),
            )
        };

        let luma_uniform = unsafe { gl.get_uniform_location(program, "u_luma") }
            .ok_or_else(|| "missing u_luma uniform".to_string())?;
        let chroma_uniform = unsafe { gl.get_uniform_location(program, "u_chroma") }
            .ok_or_else(|| "missing u_chroma uniform".to_string())?;
        let chroma_v_uniform = unsafe { gl.get_uniform_location(program, "u_chroma_v") }
            .ok_or_else(|| "missing u_chroma_v uniform".to_string())?;
        let mode_uniform = unsafe { gl.get_uniform_location(program, "u_mode") }
            .ok_or_else(|| "missing u_mode uniform".to_string())?;

        unsafe {
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, vertex_bytes, glow::STATIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);

            gl.use_program(Some(program));
            gl.uniform_1_i32(Some(&luma_uniform), 0);
            gl.uniform_1_i32(Some(&chroma_uniform), 1);
            gl.uniform_1_i32(Some(&chroma_v_uniform), 2);
            gl.use_program(None);
        }

        let pbo_enabled = pbo_upload_supported(gl);
        Ok(Self {
            program,
            framebuffer,
            vao,
            _vbo: vbo,
            luma_tex,
            chroma_tex,
            chroma_v_tex,
            mode_uniform,
            last_format: None,
            last_luma_size: (0, 0),
            last_chroma_size: (0, 0),
            luma_pbo: PboRing::new(),
            chroma_pbo: PboRing::new(),
            chroma_v_pbo: PboRing::new(),
            pbo_enabled,
        })
    }

    fn upload_and_render(
        &mut self,
        gl: &glow::Context,
        output_texture: glow::Texture,
        video: &VideoFrameBuffer,
    ) -> Result<(), String> {
        let chroma_size = (video.chroma_width(), video.chroma_height());
        let reallocate = self.last_format != Some(video.format)
            || self.last_luma_size != (video.width, video.height)
            || self.last_chroma_size != chroma_size;

        unsafe {
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        }

        let pbo_enabled = self.pbo_enabled;

        upload_plane_texture(
            gl,
            self.luma_tex,
            glow::R8 as i32,
            glow::RED,
            video.width,
            video.height,
            &video.plane0,
            reallocate,
            pbo_enabled.then_some(&mut self.luma_pbo),
        )?;

        match video.format {
            VideoFormat::Yuv420p8 => {
                upload_plane_texture(
                    gl,
                    self.chroma_tex,
                    glow::R8 as i32,
                    glow::RED,
                    chroma_size.0,
                    chroma_size.1,
                    &video.plane1,
                    reallocate,
                    pbo_enabled.then_some(&mut self.chroma_pbo),
                )?;
                upload_plane_texture(
                    gl,
                    self.chroma_v_tex,
                    glow::R8 as i32,
                    glow::RED,
                    chroma_size.0,
                    chroma_size.1,
                    &video.plane2,
                    reallocate,
                    pbo_enabled.then_some(&mut self.chroma_v_pbo),
                )?;
            }
            VideoFormat::Yuv444p8 => {
                upload_plane_texture(
                    gl,
                    self.chroma_tex,
                    glow::R8 as i32,
                    glow::RED,
                    chroma_size.0,
                    chroma_size.1,
                    &video.plane1,
                    reallocate,
                    pbo_enabled.then_some(&mut self.chroma_pbo),
                )?;
                upload_plane_texture(
                    gl,
                    self.chroma_v_tex,
                    glow::R8 as i32,
                    glow::RED,
                    chroma_size.0,
                    chroma_size.1,
                    &video.plane2,
                    reallocate,
                    pbo_enabled.then_some(&mut self.chroma_v_pbo),
                )?;
            }
            VideoFormat::Nv12 => {
                upload_plane_texture(
                    gl,
                    self.chroma_tex,
                    glow::RG8 as i32,
                    glow::RG,
                    chroma_size.0,
                    chroma_size.1,
                    &video.plane1,
                    reallocate,
                    pbo_enabled.then_some(&mut self.chroma_pbo),
                )?;
            }
            VideoFormat::Rgba8 => return Err("unexpected RGBA frame in YUV pipeline".into()),
        }

        self.render_textures(
            gl,
            output_texture,
            video.width,
            video.height,
            video.format,
            self.luma_tex,
            self.chroma_tex,
            self.chroma_v_tex,
        )?;

        self.last_format = Some(video.format);
        self.last_luma_size = (video.width, video.height);
        self.last_chroma_size = chroma_size;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_textures(
        &mut self,
        gl: &glow::Context,
        output_texture: glow::Texture,
        width: u32,
        height: u32,
        format: VideoFormat,
        luma_tex: glow::Texture,
        chroma_tex: glow::Texture,
        chroma_v_tex: glow::Texture,
    ) -> Result<(), String> {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(output_texture),
                0,
            );

            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                return Err("YUV framebuffer incomplete".into());
            }

            gl.viewport(0, 0, width as i32, height as i32);
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.use_program(Some(self.program));
            gl.uniform_1_i32(
                Some(&self.mode_uniform),
                match format {
                    VideoFormat::Yuv420p8 | VideoFormat::Yuv444p8 => 0,
                    VideoFormat::Nv12 => 1,
                    VideoFormat::Rgba8 => 0,
                },
            );

            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(luma_tex));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(chroma_tex));
            gl.active_texture(glow::TEXTURE2);
            gl.bind_texture(
                glow::TEXTURE_2D,
                match format {
                    VideoFormat::Yuv420p8 | VideoFormat::Yuv444p8 => Some(chroma_v_tex),
                    VideoFormat::Nv12 | VideoFormat::Rgba8 => Some(chroma_tex),
                },
            );

            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
            gl.use_program(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }

        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn render_textures_to_current(
        &mut self,
        gl: &glow::Context,
        format: VideoFormat,
        luma_tex: glow::Texture,
        chroma_tex: glow::Texture,
        chroma_v_tex: glow::Texture,
    ) -> Result<(), String> {
        unsafe {
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.use_program(Some(self.program));
            gl.uniform_1_i32(
                Some(&self.mode_uniform),
                match format {
                    VideoFormat::Yuv420p8 | VideoFormat::Yuv444p8 => 0,
                    VideoFormat::Nv12 => 1,
                    VideoFormat::Rgba8 => 0,
                },
            );

            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(luma_tex));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(chroma_tex));
            gl.active_texture(glow::TEXTURE2);
            gl.bind_texture(
                glow::TEXTURE_2D,
                match format {
                    VideoFormat::Yuv420p8 | VideoFormat::Yuv444p8 => Some(chroma_v_tex),
                    VideoFormat::Nv12 | VideoFormat::Rgba8 => Some(chroma_tex),
                },
            );

            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
        }

        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl LinuxDmabufImporter {
    fn probe(gl: &glow::Context) -> bool {
        linux_dmabuf_support(gl).is_ok()
    }

    fn new(gl: &glow::Context) -> Result<Self, String> {
        let support = linux_dmabuf_support(gl)?;
        Ok(Self {
            egl: support.egl,
            image_target_texture_2d: support.image_target_texture_2d,
            luma_tex: unsafe { create_input_texture(gl)? },
            chroma_tex: unsafe { create_input_texture(gl)? },
            chroma_v_tex: unsafe { create_input_texture(gl)? },
            native_fence_sync_supported: support.native_fence_sync,
        })
    }

    fn import_and_render(
        &mut self,
        gl: &glow::Context,
        output_texture: glow::Texture,
        pipeline: &mut YuvPipeline,
        frame: &LinuxDmaBufFrame,
    ) -> Result<(), String> {
        let display = self
            .egl
            .get_current_display()
            .ok_or_else(|| "EGL current display unavailable".to_string())?;
        let _context = self
            .egl
            .get_current_context()
            .ok_or_else(|| "EGL current context unavailable".to_string())?;

        self.wait_acquire_fence(display, frame);
        let images = self.import_images(display, frame)?;
        let render_result = (|| {
            bind_egl_image(gl, self.image_target_texture_2d, self.luma_tex, images[0])?;
            bind_egl_image(gl, self.image_target_texture_2d, self.chroma_tex, images[1])?;
            if !matches!(frame.format, LinuxDmaBufFormat::Nv12) {
                bind_egl_image(
                    gl,
                    self.image_target_texture_2d,
                    self.chroma_v_tex,
                    images[2],
                )?;
            }

            pipeline.render_textures(
                gl,
                output_texture,
                frame.width,
                frame.height,
                dmabuf_video_format(frame.format),
                self.luma_tex,
                self.chroma_tex,
                self.chroma_v_tex,
            )
        })();

        for image in images {
            let _ = self.egl.destroy_image(display, image);
        }

        render_result
    }

    fn import_and_render_to_current(
        &mut self,
        gl: &glow::Context,
        pipeline: &mut YuvPipeline,
        frame: &LinuxDmaBufFrame,
    ) -> Result<(), String> {
        let display = self
            .egl
            .get_current_display()
            .ok_or_else(|| "EGL current display unavailable".to_string())?;
        let _context = self
            .egl
            .get_current_context()
            .ok_or_else(|| "EGL current context unavailable".to_string())?;

        self.wait_acquire_fence(display, frame);
        let images = self.import_images(display, frame)?;
        let render_result = (|| {
            bind_egl_image(gl, self.image_target_texture_2d, self.luma_tex, images[0])?;
            bind_egl_image(gl, self.image_target_texture_2d, self.chroma_tex, images[1])?;
            if !matches!(frame.format, LinuxDmaBufFormat::Nv12) {
                bind_egl_image(
                    gl,
                    self.image_target_texture_2d,
                    self.chroma_v_tex,
                    images[2],
                )?;
            }

            pipeline.render_textures_to_current(
                gl,
                dmabuf_video_format(frame.format),
                self.luma_tex,
                self.chroma_tex,
                self.chroma_v_tex,
            )
        })();

        for image in images {
            let _ = self.egl.destroy_image(display, image);
        }

        render_result
    }

    fn import_images(
        &self,
        display: egl::Display,
        frame: &LinuxDmaBufFrame,
    ) -> Result<Vec<egl::Image>, String> {
        let expected = match frame.format {
            LinuxDmaBufFormat::Nv12 => 2,
            LinuxDmaBufFormat::Yuv420p8 | LinuxDmaBufFormat::Yuv444p8 => 3,
        };
        if frame.planes.len() < expected {
            return Err(format!(
                "dmabuf frame has {} planes, need {expected}",
                frame.planes.len()
            ));
        }

        let mut images = Vec::with_capacity(expected);
        for plane in frame.planes.iter().take(expected) {
            images.push(create_dmabuf_image(&self.egl, display, plane)?);
        }
        Ok(images)
    }

    /// If the frame carries an acquire fence and the EGL context supports
    /// `EGL_ANDROID_native_fence_sync`, import the fence and queue a GPU-side
    /// wait before we sample the dmabuf. Ownership of the fd transfers to EGL
    /// on successful `eglCreateSync`; on any failure we leave the fence fd
    /// owned by the frame so its `Drop` closes it (implicit-sync fallback).
    fn wait_acquire_fence(&self, display: egl::Display, frame: &LinuxDmaBufFrame) {
        if !self.native_fence_sync_supported {
            return;
        }
        let Some(fence_fd) = frame.acquire_fence_fd.as_ref() else {
            return;
        };
        let dup_fd = match fence_fd.try_clone() {
            Ok(f) => f,
            Err(err) => {
                eprintln!("[render] dup fence fd failed ({err}); falling back to implicit sync");
                return;
            }
        };
        let raw = dup_fd.as_raw_fd();
        let attribs: [egl::Attrib; 3] = [
            EGL_SYNC_NATIVE_FENCE_FD_ANDROID as egl::Attrib,
            raw as egl::Attrib,
            egl::ATTRIB_NONE,
        ];
        let sync = match unsafe {
            self.egl
                .create_sync(display, EGL_SYNC_NATIVE_FENCE_ANDROID, &attribs)
        } {
            Ok(sync) => {
                // EGL owns the fd on success — must not also close it.
                std::mem::forget(dup_fd);
                sync
            }
            Err(err) => {
                eprintln!("[render] eglCreateSync(native_fence) failed ({err:?}); implicit sync");
                return;
            }
        };
        if let Err(err) = self.egl.wait_sync(display, sync, 0) {
            eprintln!("[render] eglWaitSync failed ({err:?}); continuing");
        }
        let _ = unsafe { self.egl.destroy_sync(display, sync) };
    }
}

fn upload_rgba(
    gl: &glow::Context,
    texture: glow::Texture,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<(), String> {
    let expected_len = width as usize * height as usize * 4;
    if rgba.len() < expected_len {
        return Err(format!(
            "RGBA frame buffer too small: got {}, need {}",
            rgba.len(),
            expected_len
        ));
    }

    unsafe {
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_sub_image_2d(
            glow::TEXTURE_2D,
            0,
            0,
            0,
            width as i32,
            height as i32,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            PixelUnpackData::Slice(Some(&rgba[..expected_len])),
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn upload_plane_texture(
    gl: &glow::Context,
    texture: glow::Texture,
    internal_format: i32,
    format: u32,
    width: u32,
    height: u32,
    data: &[u8],
    reallocate: bool,
    pbo: Option<&mut PboRing>,
) -> Result<(), String> {
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));

        // Reallocations need glTexImage2D to resize the texture, so always use
        // the direct slice path there. Steady-state updates take the PBO path
        // when available so the driver can DMA from previously-written GPU
        // memory instead of blocking on a fresh client-mem upload.
        let pbo_used = if !reallocate {
            if let Some(ring) = pbo {
                match ring.upload(gl, data) {
                    Ok(true) => true,
                    Ok(false) => {
                        PboRing::unbind(gl);
                        false
                    }
                    Err(_) => {
                        PboRing::unbind(gl);
                        false
                    }
                }
            } else {
                false
            }
        } else {
            false
        };

        if reallocate {
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal_format,
                width as i32,
                height as i32,
                0,
                format,
                glow::UNSIGNED_BYTE,
                PixelUnpackData::Slice(Some(data)),
            );
        } else if pbo_used {
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                width as i32,
                height as i32,
                format,
                glow::UNSIGNED_BYTE,
                PixelUnpackData::BufferOffset(0),
            );
            PboRing::unbind(gl);
        } else {
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                width as i32,
                height as i32,
                format,
                glow::UNSIGNED_BYTE,
                PixelUnpackData::Slice(Some(data)),
            );
        }
        gl.bind_texture(glow::TEXTURE_2D, None);
    }

    Ok(())
}

/// PBO-based uploads require GL 2.1+ / GLES 3.0+ and the `map_buffer_range`
/// entrypoint. `YuvPipeline::new` already ensures the GL version, but probe
/// again here so the caller can cleanly disable the path on weird drivers.
fn pbo_upload_supported(gl: &glow::Context) -> bool {
    if std::env::var_os("ST_DISABLE_PBO").is_some() {
        return false;
    }
    let v = gl.version();
    if v.is_embedded {
        v.major >= 3
    } else {
        v.major > 2 || (v.major == 2 && v.minor >= 1)
    }
}

fn ensure_yuv_gl_support(gl: &glow::Context) -> Result<(), String> {
    let version = gl.version();
    // Desktop GL 3.0+ and GLES 3.0+ both expose the YUV sampling path.
    let supported = version.major >= 3;
    if supported {
        Ok(())
    } else {
        Err(format!(
            "OpenGL 3.0+ required for YUV rendering, found {version:?}"
        ))
    }
}

unsafe fn create_texture(gl: &glow::Context) -> Result<glow::Texture, String> {
    let texture = gl
        .create_texture()
        .map_err(|err| format!("create_texture: {err}"))?;
    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MIN_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MAG_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_S,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_T,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.bind_texture(glow::TEXTURE_2D, None);
    Ok(texture)
}

unsafe fn create_input_texture(gl: &glow::Context) -> Result<glow::Texture, String> {
    create_texture(gl)
}

unsafe fn create_yuv_program(gl: &glow::Context) -> Result<glow::Program, String> {
    let version = gl.version();
    let shader_header = if version.is_embedded {
        "#version 300 es\nprecision mediump float;\n"
    } else {
        "#version 330 core\n"
    };
    let vertex_src = format!(
        "{shader_header}layout(location = 0) in vec2 a_pos;\nlayout(location = 1) in vec2 a_uv;\nout vec2 v_uv;\nvoid main() {{\n    v_uv = a_uv;\n    gl_Position = vec4(a_pos, 0.0, 1.0);\n}}\n"
    );
    let fragment_src = format!(
        "{shader_header}in vec2 v_uv;\nout vec4 frag_color;\nuniform sampler2D u_luma;\nuniform sampler2D u_chroma;\nuniform sampler2D u_chroma_v;\nuniform int u_mode;\nvec3 bt709_limited_to_rgb(float y, float u, float v) {{\n    float yy = max(y - 0.0625, 0.0) * 1.16438356;\n    return vec3(\n        yy + 1.79274107 * v,\n        yy - 0.21324861 * u - 0.53290933 * v,\n        yy + 2.11240179 * u\n    );\n}}\nvoid main() {{\n    float y = texture(u_luma, v_uv).r;\n    float u;\n    float v;\n    if (u_mode == 0) {{\n        u = texture(u_chroma, v_uv).r - 0.5;\n        v = texture(u_chroma_v, v_uv).r - 0.5;\n    }} else {{\n        vec2 uv = texture(u_chroma, v_uv).rg - vec2(0.5, 0.5);\n        u = uv.x;\n        v = uv.y;\n    }}\n    vec3 rgb = clamp(bt709_limited_to_rgb(y, u, v), 0.0, 1.0);\n    frag_color = vec4(rgb, 1.0);\n}}\n"
    );

    let vertex = gl
        .create_shader(glow::VERTEX_SHADER)
        .map_err(|err| format!("create vertex shader: {err}"))?;
    gl.shader_source(vertex, &vertex_src);
    gl.compile_shader(vertex);
    if !gl.get_shader_compile_status(vertex) {
        let log = gl.get_shader_info_log(vertex);
        gl.delete_shader(vertex);
        return Err(format!("vertex shader compile failed: {log}"));
    }

    let fragment = gl
        .create_shader(glow::FRAGMENT_SHADER)
        .map_err(|err| format!("create fragment shader: {err}"))?;
    gl.shader_source(fragment, &fragment_src);
    gl.compile_shader(fragment);
    if !gl.get_shader_compile_status(fragment) {
        let log = gl.get_shader_info_log(fragment);
        gl.delete_shader(vertex);
        gl.delete_shader(fragment);
        return Err(format!("fragment shader compile failed: {log}"));
    }

    let program = gl
        .create_program()
        .map_err(|err| format!("create program: {err}"))?;
    gl.attach_shader(program, vertex);
    gl.attach_shader(program, fragment);
    gl.link_program(program);
    gl.detach_shader(program, vertex);
    gl.detach_shader(program, fragment);
    gl.delete_shader(vertex);
    gl.delete_shader(fragment);

    if !gl.get_program_link_status(program) {
        let log = gl.get_program_info_log(program);
        gl.delete_program(program);
        return Err(format!("shader link failed: {log}"));
    }

    Ok(program)
}

#[cfg(target_os = "linux")]
struct LinuxDmabufSupport {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    image_target_texture_2d: GlEglImageTargetTexture2DOes,
    native_fence_sync: bool,
}

#[cfg(target_os = "linux")]
fn linux_dmabuf_support(gl: &glow::Context) -> Result<LinuxDmabufSupport, String> {
    if !gl.supported_extensions().contains("GL_OES_EGL_image") {
        return Err("GL_OES_EGL_image not available".into());
    }

    let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
        .map_err(|err| format!("load libEGL: {err:?}"))?;
    let display = egl
        .get_current_display()
        .ok_or_else(|| "current EGL display unavailable".to_string())?;
    let _context = egl
        .get_current_context()
        .ok_or_else(|| "current EGL context unavailable".to_string())?;
    let extensions = egl
        .query_string(Some(display), egl::EXTENSIONS)
        .map_err(|err| format!("eglQueryString(EXTENSIONS): {err:?}"))?
        .to_string_lossy();
    if !extensions.contains("EGL_EXT_image_dma_buf_import") {
        return Err("EGL_EXT_image_dma_buf_import not available".into());
    }

    // Explicit-sync acquire waits need BOTH the base fence-sync machinery and
    // the native-fence-fd extension. Both live in the same EGL extension
    // string; checking both keeps older drivers that only ship one on the
    // implicit-sync path.
    let native_fence_sync = (extensions.contains("EGL_KHR_fence_sync")
        || extensions.contains("EGL_KHR_reusable_sync"))
        && extensions.contains("EGL_ANDROID_native_fence_sync");

    let image_target_texture_2d = egl
        .get_proc_address("glEGLImageTargetTexture2DOES")
        .ok_or_else(|| "glEGLImageTargetTexture2DOES unavailable".to_string())?;

    Ok(LinuxDmabufSupport {
        egl,
        image_target_texture_2d: unsafe {
            std::mem::transmute::<extern "system" fn(), GlEglImageTargetTexture2DOes>(
                image_target_texture_2d,
            )
        },
        native_fence_sync,
    })
}

#[cfg(target_os = "linux")]
fn create_dmabuf_image(
    egl: &egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    plane: &LinuxDmaBufPlane,
) -> Result<egl::Image, String> {
    let mut attrs = vec![
        EGL_WIDTH as egl::Attrib,
        plane.width as egl::Attrib,
        EGL_HEIGHT as egl::Attrib,
        plane.height as egl::Attrib,
        EGL_LINUX_DRM_FOURCC_EXT as egl::Attrib,
        plane.drm_format as egl::Attrib,
        EGL_DMA_BUF_PLANE0_FD_EXT as egl::Attrib,
        plane.fd.as_raw_fd() as egl::Attrib,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT as egl::Attrib,
        plane.offset as egl::Attrib,
        EGL_DMA_BUF_PLANE0_PITCH_EXT as egl::Attrib,
        plane.pitch as egl::Attrib,
    ];
    if plane.modifier != DRM_FORMAT_MOD_INVALID && plane.modifier != DRM_FORMAT_MOD_LINEAR {
        attrs.push(EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT as egl::Attrib);
        attrs.push((plane.modifier as u32) as egl::Attrib);
        attrs.push(EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT as egl::Attrib);
        attrs.push((plane.modifier >> 32) as egl::Attrib);
    }
    attrs.push(egl::ATTRIB_NONE);

    egl.create_image(
        display,
        unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) },
        EGL_LINUX_DMA_BUF_EXT,
        unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) },
        &attrs,
    )
    .map_err(|err| format!("eglCreateImageKHR(dmabuf): {err:?}"))
}

#[cfg(target_os = "linux")]
fn bind_egl_image(
    gl: &glow::Context,
    image_target_texture_2d: GlEglImageTargetTexture2DOes,
    texture: glow::Texture,
    image: egl::Image,
) -> Result<(), String> {
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        image_target_texture_2d(glow::TEXTURE_2D, image.as_ptr() as *const c_void);
        gl.bind_texture(glow::TEXTURE_2D, None);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn dmabuf_video_format(format: LinuxDmaBufFormat) -> VideoFormat {
    match format {
        LinuxDmaBufFormat::Yuv420p8 => VideoFormat::Yuv420p8,
        LinuxDmaBufFormat::Yuv444p8 => VideoFormat::Yuv444p8,
        LinuxDmaBufFormat::Nv12 => VideoFormat::Nv12,
    }
}

#[cfg(target_os = "linux")]
const EGL_WIDTH: u32 = 0x3057;
#[cfg(target_os = "linux")]
const EGL_HEIGHT: u32 = 0x3056;
#[cfg(target_os = "linux")]
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
#[cfg(target_os = "linux")]
const EGL_LINUX_DRM_FOURCC_EXT: u32 = 0x3271;
#[cfg(target_os = "linux")]
const EGL_DMA_BUF_PLANE0_FD_EXT: u32 = 0x3272;
#[cfg(target_os = "linux")]
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: u32 = 0x3273;
#[cfg(target_os = "linux")]
const EGL_DMA_BUF_PLANE0_PITCH_EXT: u32 = 0x3274;
#[cfg(target_os = "linux")]
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: u32 = 0x3443;
#[cfg(target_os = "linux")]
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: u32 = 0x3444;
#[cfg(target_os = "linux")]
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
#[cfg(target_os = "linux")]
const EGL_SYNC_NATIVE_FENCE_ANDROID: egl::Enum = 0x3144;
#[cfg(target_os = "linux")]
const EGL_SYNC_NATIVE_FENCE_FD_ANDROID: u32 = 0x3145;
#[cfg(target_os = "linux")]
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
