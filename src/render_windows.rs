use crate::video_frame::{WindowsComPtr, WindowsD3d11Frame};
use eframe::{egui, egui_glow, glow};
use glow::HasContext as _;
use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex};

pub struct WindowsD3d11Importer {
    interop: WglDxInterop,
    state: Option<InteropState>,
}

#[derive(Clone, Default)]
pub struct WindowsDirectVideoPresenter {
    inner: Arc<Mutex<WindowsDirectVideoPresenterState>>,
}

struct InteropState {
    device_raw: *mut c_void,
    gl_texture: glow::Texture,
    width: u32,
    height: u32,
    dx_handle: Handle,
    _device: WindowsComPtr,
    video_device: WindowsComPtr,
    video_context: WindowsComPtr,
    _shared_texture: WindowsComPtr,
    processor_enum: WindowsComPtr,
    processor: WindowsComPtr,
    output_view: WindowsComPtr,
    registered_object: Handle,
}

#[derive(Default)]
struct WindowsDirectVideoPresenterState {
    enabled: bool,
    logged_success: bool,
    frame: Option<WindowsD3d11Frame>,
    importer: Option<WindowsD3d11Importer>,
    blit_pipeline: Option<DirectTextureBlitPipeline>,
    output_texture: Option<glow::Texture>,
    output_size: (u32, u32),
}

struct DirectTextureBlitPipeline {
    program: glow::Program,
    vao: glow::VertexArray,
    _vbo: glow::Buffer,
}

type Handle = *mut c_void;
type Hglrc = *mut c_void;
type HRESULT = i32;
type BOOL = i32;
type UINT = u32;
type GLenum = u32;
type GLuint = u32;

const WGL_ACCESS_WRITE_DISCARD_NV: u32 = 0x0000_0002;

const DXGI_FORMAT_R8G8B8A8_UNORM: u32 = 0x1c;
const D3D11_USAGE_DEFAULT: u32 = 0;
const D3D11_BIND_SHADER_RESOURCE: u32 = 0x8;
const D3D11_BIND_RENDER_TARGET: u32 = 0x20;
const D3D11_RESOURCE_MISC_SHARED: u32 = 0x2;
const D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE: u32 = 0;
const D3D11_VIDEO_USAGE_PLAYBACK_NORMAL: u32 = 0;
const D3D11_VPIV_DIMENSION_TEXTURE2D: u32 = 1;
const D3D11_VPOV_DIMENSION_TEXTURE2D: u32 = 1;

impl WindowsD3d11Importer {
    pub fn probe(_gl: &glow::Context) -> bool {
        (unsafe { !wglGetCurrentContext().is_null() }) && WglDxInterop::load().is_ok()
    }

    pub fn new(_gl: &glow::Context) -> Result<Self, String> {
        Ok(Self {
            interop: WglDxInterop::load()?,
            state: None,
        })
    }

    pub fn release_registration(&mut self) {
        self.drop_state();
    }

    pub fn has_registration(&self) -> bool {
        self.state.is_some()
    }

    pub fn import_and_render(
        &mut self,
        output_texture: glow::Texture,
        frame: &WindowsD3d11Frame,
    ) -> Result<(), String> {
        self.ensure_state(output_texture, frame)?;
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| "missing D3D11 interop state".to_string())?;

        let input_view = create_video_processor_input_view(
            state.video_device.as_ptr(),
            frame.texture.as_ptr(),
            state.processor_enum.as_ptr(),
            frame.array_index,
        )?;

        let handles = [state.registered_object];
        let lock_ok = unsafe { (self.interop.lock_objects)(state.dx_handle, 1, handles.as_ptr()) };
        if lock_ok == 0 {
            return Err("wglDXLockObjectsNV failed".into());
        }

        let blit_result = video_processor_blt(
            state.video_context.as_ptr(),
            state.processor.as_ptr(),
            state.output_view.as_ptr(),
            input_view.as_ptr(),
        );

        let unlock_ok =
            unsafe { (self.interop.unlock_objects)(state.dx_handle, 1, handles.as_ptr()) } != 0;
        if !unlock_ok {
            return Err("wglDXUnlockObjectsNV failed".into());
        }

        blit_result
    }

    fn ensure_state(
        &mut self,
        output_texture: glow::Texture,
        frame: &WindowsD3d11Frame,
    ) -> Result<(), String> {
        let needs_recreate = match self.state.as_ref() {
            Some(state) => {
                state.device_raw != frame.device.as_ptr()
                    || state.gl_texture != output_texture
                    || state.width != frame.width
                    || state.height != frame.height
            }
            None => true,
        };

        if needs_recreate {
            self.drop_state();
            self.state = Some(InteropState::new(&self.interop, output_texture, frame)?);
        }

        Ok(())
    }

    fn drop_state(&mut self) {
        if let Some(state) = self.state.take() {
            if !state.registered_object.is_null() {
                unsafe {
                    (self.interop.unregister_object)(state.dx_handle, state.registered_object);
                }
            }
            if !state.dx_handle.is_null() {
                unsafe {
                    (self.interop.close_device)(state.dx_handle);
                }
            }
        }
    }
}

impl Drop for WindowsD3d11Importer {
    fn drop(&mut self) {
        self.drop_state();
    }
}

impl WindowsDirectVideoPresenter {
    pub fn new() -> Self {
        let mut state = WindowsDirectVideoPresenterState::default();
        state.enabled = true;
        Self {
            inner: Arc::new(Mutex::new(state)),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.lock().unwrap().enabled
    }

    pub fn has_frame(&self) -> bool {
        let state = self.inner.lock().unwrap();
        state.enabled && state.frame.is_some()
    }

    pub fn clear(&self) {
        let mut state = self.inner.lock().unwrap();
        state.frame = None;
    }

    pub fn stage_frame(&self, frame: &WindowsD3d11Frame) {
        let mut state = self.inner.lock().unwrap();
        if state.enabled {
            state.frame = Some(frame.clone());
        }
    }

    pub fn paint_callback(&self, rect: egui::Rect) -> egui::PaintCallback {
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

impl InteropState {
    fn new(
        interop: &WglDxInterop,
        gl_texture: glow::Texture,
        frame: &WindowsD3d11Frame,
    ) -> Result<Self, String> {
        let device = unsafe {
            WindowsComPtr::retain(frame.device.as_ptr())
                .ok_or_else(|| "missing D3D11 device".to_string())?
        };
        let video_device = unsafe {
            WindowsComPtr::retain(frame.video_device.as_ptr())
                .ok_or_else(|| "missing D3D11 video device".to_string())?
        };
        let video_context = unsafe {
            WindowsComPtr::retain(frame.video_context.as_ptr())
                .ok_or_else(|| "missing D3D11 video context".to_string())?
        };

        let dx_handle = unsafe { (interop.open_device)(device.as_ptr()) };
        if dx_handle.is_null() {
            return Err("wglDXOpenDeviceNV failed".into());
        }

        let shared_texture = create_output_texture(device.as_ptr(), frame.width, frame.height)?;
        let processor_enum =
            create_video_processor_enumerator(video_device.as_ptr(), frame.width, frame.height)?;
        let processor = create_video_processor(video_device.as_ptr(), processor_enum.as_ptr())?;
        let output_view = create_video_processor_output_view(
            video_device.as_ptr(),
            shared_texture.as_ptr(),
            processor_enum.as_ptr(),
        )?;

        // Set explicit source and output rects so the video processor only
        // reads the actual content area.  Without these, decoder textures
        // that are larger than the coded frame (GPU alignment padding) can
        // produce a green line at the bottom.
        let content_rect = WinRect {
            left: 0,
            top: 0,
            right: frame.width as i32,
            bottom: frame.height as i32,
        };
        let video_context_ptr = video_context.as_ptr() as *mut ID3D11VideoContext;
        unsafe {
            ((*(*video_context_ptr).vtable).video_processor_set_stream_source_rect)(
                video_context_ptr,
                processor.as_ptr(),
                0,
                1,
                &content_rect,
            );
            ((*(*video_context_ptr).vtable).video_processor_set_output_target_rect)(
                video_context_ptr,
                processor.as_ptr(),
                1,
                &content_rect,
            );
        }

        let registered_object = unsafe {
            (interop.register_object)(
                dx_handle,
                shared_texture.as_ptr(),
                texture_name(gl_texture),
                glow::TEXTURE_2D,
                WGL_ACCESS_WRITE_DISCARD_NV,
            )
        };
        if registered_object.is_null() {
            unsafe {
                (interop.close_device)(dx_handle);
            }
            return Err("wglDXRegisterObjectNV failed".into());
        }

        Ok(Self {
            device_raw: device.as_ptr(),
            gl_texture,
            width: frame.width,
            height: frame.height,
            dx_handle,
            _device: device,
            video_device,
            video_context,
            _shared_texture: shared_texture,
            processor_enum,
            processor,
            output_view,
            registered_object,
        })
    }
}

// The presenter state is guarded by a mutex and only used by the egui paint callback
// on the active GL thread, but egui requires the callback capture to be Send.
unsafe impl Send for WindowsDirectVideoPresenterState {}

impl WindowsDirectVideoPresenterState {
    fn render(&mut self, info: egui::PaintCallbackInfo, painter: &egui_glow::Painter) {
        if !self.enabled {
            return;
        }
        let Some(frame) = self.frame.clone() else {
            return;
        };
        let gl = painter.gl();

        if self.importer.is_none() {
            match WindowsD3d11Importer::new(gl.as_ref()) {
                Ok(importer) => self.importer = Some(importer),
                Err(err) => {
                    eprintln!("[render] disabling Windows direct present path: {err}");
                    self.disable();
                    return;
                }
            }
        }
        if self.blit_pipeline.is_none() {
            match DirectTextureBlitPipeline::new(gl.as_ref()) {
                Ok(pipeline) => self.blit_pipeline = Some(pipeline),
                Err(err) => {
                    eprintln!("[render] disabling Windows direct present path: {err}");
                    self.disable();
                    return;
                }
            }
        }

        let output_texture =
            match self.ensure_output_texture(gl.as_ref(), frame.width, frame.height) {
                Ok(texture) => texture,
                Err(err) => {
                    eprintln!("[render] disabling Windows direct present path: {err}");
                    self.disable();
                    return;
                }
            };

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

        let result = (|| {
            self.importer
                .as_mut()
                .expect("importer set")
                .import_and_render(output_texture, &frame)?;
            self.blit_pipeline
                .as_mut()
                .expect("pipeline set")
                .render_to_current(gl.as_ref(), output_texture)
        })();

        unsafe {
            gl.disable(glow::SCISSOR_TEST);
        }

        if let Err(err) = result {
            eprintln!("[render] disabling Windows direct present path: {err}");
            self.disable();
        } else if !self.logged_success {
            eprintln!("[render] Windows direct present path enabled");
            self.logged_success = true;
        }
    }

    fn ensure_output_texture(
        &mut self,
        gl: &glow::Context,
        width: u32,
        height: u32,
    ) -> Result<glow::Texture, String> {
        if self.output_texture.is_none() {
            self.output_texture = Some(unsafe { create_gl_texture(gl)? });
        }

        let texture = self
            .output_texture
            .ok_or_else(|| "missing direct-present output texture".to_string())?;
        if self.output_size != (width, height) {
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    width as i32,
                    height as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
                gl.bind_texture(glow::TEXTURE_2D, None);
            }
            self.output_size = (width, height);
        }

        Ok(texture)
    }

    fn disable(&mut self) {
        self.enabled = false;
        self.logged_success = false;
        self.frame = None;
        self.importer = None;
        self.blit_pipeline = None;
        self.output_texture = None;
        self.output_size = (0, 0);
    }
}

impl DirectTextureBlitPipeline {
    fn new(gl: &glow::Context) -> Result<Self, String> {
        let program = unsafe { create_direct_texture_program(gl)? };
        let vao = unsafe {
            gl.create_vertex_array()
                .map_err(|err| format!("create_vertex_array: {err}"))?
        };
        let vbo = unsafe {
            gl.create_buffer()
                .map_err(|err| format!("create_buffer: {err}"))?
        };
        let vertices: [f32; 16] = [
            -1.0, -1.0, 0.0, 1.0, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0,
        ];
        let vertex_bytes = unsafe {
            std::slice::from_raw_parts(
                vertices.as_ptr() as *const u8,
                std::mem::size_of_val(&vertices),
            )
        };
        let texture_uniform = unsafe { gl.get_uniform_location(program, "u_texture") }
            .ok_or_else(|| "missing u_texture uniform".to_string())?;

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
            gl.uniform_1_i32(Some(&texture_uniform), 0);
            gl.use_program(None);
        }

        Ok(Self {
            program,
            vao,
            _vbo: vbo,
        })
    }

    fn render_to_current(
        &mut self,
        gl: &glow::Context,
        texture: glow::Texture,
    ) -> Result<(), String> {
        unsafe {
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.use_program(Some(self.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
        }
        Ok(())
    }
}

fn create_output_texture(
    device: *mut c_void,
    width: u32,
    height: u32,
) -> Result<WindowsComPtr, String> {
    let device = device as *mut ID3D11Device;
    let desc = D3d11Texture2dDesc {
        width,
        height,
        mip_levels: 1,
        array_size: 1,
        format: DXGI_FORMAT_R8G8B8A8_UNORM,
        sample_desc: DxgiSampleDesc {
            count: 1,
            quality: 0,
        },
        usage: D3D11_USAGE_DEFAULT,
        bind_flags: D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET,
        cpu_access_flags: 0,
        misc_flags: D3D11_RESOURCE_MISC_SHARED,
    };

    let mut texture = ptr::null_mut();
    let hr = unsafe {
        ((*(*device).vtable).create_texture_2d)(device, &desc, ptr::null(), &mut texture)
    };
    check_hr(hr, "ID3D11Device::CreateTexture2D")?;
    unsafe {
        WindowsComPtr::retain(texture.cast())
            .ok_or_else(|| "CreateTexture2D returned null texture".to_string())
    }
}

fn create_video_processor_enumerator(
    video_device: *mut c_void,
    width: u32,
    height: u32,
) -> Result<WindowsComPtr, String> {
    let video_device = video_device as *mut ID3D11VideoDevice;
    let desc = D3d11VideoProcessorContentDesc {
        input_frame_format: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        input_frame_rate: DxgiRational {
            numerator: 60,
            denominator: 1,
        },
        input_width: width,
        input_height: height,
        output_frame_rate: DxgiRational {
            numerator: 60,
            denominator: 1,
        },
        output_width: width,
        output_height: height,
        usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };

    let mut enumerator = ptr::null_mut();
    let hr = unsafe {
        ((*(*video_device).vtable).create_video_processor_enumerator)(
            video_device,
            &desc,
            &mut enumerator,
        )
    };
    check_hr(hr, "ID3D11VideoDevice::CreateVideoProcessorEnumerator")?;
    unsafe {
        WindowsComPtr::retain(enumerator.cast())
            .ok_or_else(|| "CreateVideoProcessorEnumerator returned null".to_string())
    }
}

fn create_video_processor(
    video_device: *mut c_void,
    processor_enum: *mut c_void,
) -> Result<WindowsComPtr, String> {
    let video_device = video_device as *mut ID3D11VideoDevice;
    let mut processor = ptr::null_mut();
    let hr = unsafe {
        ((*(*video_device).vtable).create_video_processor)(
            video_device,
            processor_enum.cast(),
            0,
            &mut processor,
        )
    };
    check_hr(hr, "ID3D11VideoDevice::CreateVideoProcessor")?;
    unsafe {
        WindowsComPtr::retain(processor.cast())
            .ok_or_else(|| "CreateVideoProcessor returned null".to_string())
    }
}

fn create_video_processor_input_view(
    video_device: *mut c_void,
    texture: *mut c_void,
    processor_enum: *mut c_void,
    array_index: u32,
) -> Result<WindowsComPtr, String> {
    let video_device = video_device as *mut ID3D11VideoDevice;
    let desc = D3d11VideoProcessorInputViewDesc {
        fourcc: 0,
        view_dimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        texture2d: D3d11Tex2dVpiv {
            mip_slice: 0,
            array_slice: array_index,
        },
    };

    let mut view = ptr::null_mut();
    let hr = unsafe {
        ((*(*video_device).vtable).create_video_processor_input_view)(
            video_device,
            texture,
            processor_enum.cast(),
            &desc,
            &mut view,
        )
    };
    check_hr(hr, "ID3D11VideoDevice::CreateVideoProcessorInputView")?;
    unsafe {
        WindowsComPtr::retain(view.cast())
            .ok_or_else(|| "CreateVideoProcessorInputView returned null".to_string())
    }
}

fn create_video_processor_output_view(
    video_device: *mut c_void,
    texture: *mut c_void,
    processor_enum: *mut c_void,
) -> Result<WindowsComPtr, String> {
    let video_device = video_device as *mut ID3D11VideoDevice;
    let desc = D3d11VideoProcessorOutputViewDesc {
        view_dimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        texture: D3d11Tex2dArrayVpov {
            mip_slice: 0,
            first_array_slice: 0,
            array_size: 1,
        },
    };

    let mut view = ptr::null_mut();
    let hr = unsafe {
        ((*(*video_device).vtable).create_video_processor_output_view)(
            video_device,
            texture,
            processor_enum.cast(),
            &desc,
            &mut view,
        )
    };
    check_hr(hr, "ID3D11VideoDevice::CreateVideoProcessorOutputView")?;
    unsafe {
        WindowsComPtr::retain(view.cast())
            .ok_or_else(|| "CreateVideoProcessorOutputView returned null".to_string())
    }
}

fn video_processor_blt(
    video_context: *mut c_void,
    processor: *mut c_void,
    output_view: *mut c_void,
    input_view: *mut c_void,
) -> Result<(), String> {
    let video_context = video_context as *mut ID3D11VideoContext;
    let stream = D3d11VideoProcessorStream {
        enable: 1,
        output_index: 0,
        input_frame_or_field: 0,
        past_frames: 0,
        future_frames: 0,
        past_surfaces: ptr::null_mut(),
        input_surface: input_view,
        future_surfaces: ptr::null_mut(),
        past_surfaces_right: ptr::null_mut(),
        input_surface_right: ptr::null_mut(),
        future_surfaces_right: ptr::null_mut(),
    };

    let hr = unsafe {
        ((*(*video_context).vtable).video_processor_blt)(
            video_context,
            processor,
            output_view,
            0,
            1,
            &stream,
        )
    };
    check_hr(hr, "ID3D11VideoContext::VideoProcessorBlt")
}

fn check_hr(hr: HRESULT, op: &str) -> Result<(), String> {
    if hr >= 0 {
        Ok(())
    } else {
        Err(format!("{op} failed: 0x{:08x}", hr as u32))
    }
}

unsafe fn create_gl_texture(gl: &glow::Context) -> Result<glow::Texture, String> {
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

unsafe fn create_direct_texture_program(gl: &glow::Context) -> Result<glow::Program, String> {
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
        "{shader_header}in vec2 v_uv;\nout vec4 frag_color;\nuniform sampler2D u_texture;\nvoid main() {{\n    frag_color = texture(u_texture, v_uv);\n}}\n"
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

fn texture_name(texture: glow::Texture) -> GLuint {
    texture.0.get()
}

struct WglDxInterop {
    open_device: WglDxOpenDeviceNv,
    close_device: WglDxCloseDeviceNv,
    register_object: WglDxRegisterObjectNv,
    unregister_object: WglDxUnregisterObjectNv,
    lock_objects: WglDxLockObjectsNv,
    unlock_objects: WglDxUnlockObjectsNv,
}

impl WglDxInterop {
    fn load() -> Result<Self, String> {
        unsafe {
            Ok(Self {
                open_device: load_wgl_proc(b"wglDXOpenDeviceNV\0")?,
                close_device: load_wgl_proc(b"wglDXCloseDeviceNV\0")?,
                register_object: load_wgl_proc(b"wglDXRegisterObjectNV\0")?,
                unregister_object: load_wgl_proc(b"wglDXUnregisterObjectNV\0")?,
                lock_objects: load_wgl_proc(b"wglDXLockObjectsNV\0")?,
                unlock_objects: load_wgl_proc(b"wglDXUnlockObjectsNV\0")?,
            })
        }
    }
}

unsafe fn load_wgl_proc<T>(name: &[u8]) -> Result<T, String> {
    let ptr = wglGetProcAddress(name.as_ptr().cast());
    if ptr.is_null() || ptr as usize <= 3 || ptr as isize == -1 {
        return Err(format!(
            "{} unavailable",
            String::from_utf8_lossy(&name[..name.len().saturating_sub(1)])
        ));
    }
    Ok(std::mem::transmute_copy(&ptr))
}

type WglDxOpenDeviceNv = unsafe extern "system" fn(*mut c_void) -> Handle;
type WglDxCloseDeviceNv = unsafe extern "system" fn(Handle) -> BOOL;
type WglDxRegisterObjectNv =
    unsafe extern "system" fn(Handle, *mut c_void, GLuint, GLenum, GLenum) -> Handle;
type WglDxUnregisterObjectNv = unsafe extern "system" fn(Handle, Handle) -> BOOL;
type WglDxLockObjectsNv = unsafe extern "system" fn(Handle, i32, *const Handle) -> BOOL;
type WglDxUnlockObjectsNv = unsafe extern "system" fn(Handle, i32, *const Handle) -> BOOL;

#[repr(C)]
struct ID3D11Device {
    vtable: *const ID3D11DeviceVtbl,
}

#[repr(C)]
struct ID3D11DeviceVtbl {
    query_interface: usize,
    add_ref: usize,
    release: usize,
    create_buffer: usize,
    create_texture_1d: usize,
    create_texture_2d: unsafe extern "system" fn(
        *mut ID3D11Device,
        *const D3d11Texture2dDesc,
        *const c_void,
        *mut *mut c_void,
    ) -> HRESULT,
}

#[repr(C)]
struct ID3D11VideoDevice {
    vtable: *const ID3D11VideoDeviceVtbl,
}

#[repr(C)]
struct ID3D11VideoDeviceVtbl {
    query_interface: usize,
    add_ref: usize,
    release: usize,
    create_video_decoder: usize,
    create_video_processor: unsafe extern "system" fn(
        *mut ID3D11VideoDevice,
        *mut c_void,
        UINT,
        *mut *mut c_void,
    ) -> HRESULT,
    create_authenticated_channel: usize,
    create_crypto_session: usize,
    create_video_decoder_output_view: usize,
    create_video_processor_input_view: unsafe extern "system" fn(
        *mut ID3D11VideoDevice,
        *mut c_void,
        *mut c_void,
        *const D3d11VideoProcessorInputViewDesc,
        *mut *mut c_void,
    ) -> HRESULT,
    create_video_processor_output_view: unsafe extern "system" fn(
        *mut ID3D11VideoDevice,
        *mut c_void,
        *mut c_void,
        *const D3d11VideoProcessorOutputViewDesc,
        *mut *mut c_void,
    ) -> HRESULT,
    create_video_processor_enumerator: unsafe extern "system" fn(
        *mut ID3D11VideoDevice,
        *const D3d11VideoProcessorContentDesc,
        *mut *mut c_void,
    ) -> HRESULT,
}

#[repr(C)]
struct ID3D11VideoContext {
    vtable: *const ID3D11VideoContextVtbl,
}

#[repr(C)]
struct WinRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct ID3D11VideoContextVtbl {
    query_interface: usize,
    add_ref: usize,
    release: usize,
    get_device: usize,
    get_private_data: usize,
    set_private_data: usize,
    set_private_data_interface: usize,
    get_decoder_buffer: usize,
    release_decoder_buffer: usize,
    decoder_begin_frame: usize,
    decoder_end_frame: usize,
    submit_decoder_buffers: usize,
    decoder_extension: usize,
    video_processor_set_output_target_rect: unsafe extern "system" fn(
        *mut ID3D11VideoContext,
        *mut c_void,
        BOOL,
        *const WinRect,
    ),
    _vp_prefix_1: [usize; 16],
    video_processor_set_stream_source_rect: unsafe extern "system" fn(
        *mut ID3D11VideoContext,
        *mut c_void,
        UINT,
        BOOL,
        *const WinRect,
    ),
    _vp_prefix_2: [usize; 22],
    video_processor_blt: unsafe extern "system" fn(
        *mut ID3D11VideoContext,
        *mut c_void,
        *mut c_void,
        UINT,
        UINT,
        *const D3d11VideoProcessorStream,
    ) -> HRESULT,
}

#[repr(C)]
struct DxgiSampleDesc {
    count: UINT,
    quality: UINT,
}

#[repr(C)]
struct DxgiRational {
    numerator: UINT,
    denominator: UINT,
}

#[repr(C)]
struct D3d11Texture2dDesc {
    width: UINT,
    height: UINT,
    mip_levels: UINT,
    array_size: UINT,
    format: UINT,
    sample_desc: DxgiSampleDesc,
    usage: UINT,
    bind_flags: UINT,
    cpu_access_flags: UINT,
    misc_flags: UINT,
}

#[repr(C)]
struct D3d11VideoProcessorContentDesc {
    input_frame_format: UINT,
    input_frame_rate: DxgiRational,
    input_width: UINT,
    input_height: UINT,
    output_frame_rate: DxgiRational,
    output_width: UINT,
    output_height: UINT,
    usage: UINT,
}

#[repr(C)]
struct D3d11Tex2dVpiv {
    mip_slice: UINT,
    array_slice: UINT,
}

#[repr(C)]
struct D3d11VideoProcessorInputViewDesc {
    fourcc: UINT,
    view_dimension: UINT,
    texture2d: D3d11Tex2dVpiv,
}

#[repr(C)]
struct D3d11Tex2dArrayVpov {
    mip_slice: UINT,
    first_array_slice: UINT,
    array_size: UINT,
}

#[repr(C)]
struct D3d11VideoProcessorOutputViewDesc {
    view_dimension: UINT,
    texture: D3d11Tex2dArrayVpov,
}

#[repr(C)]
struct D3d11VideoProcessorStream {
    enable: BOOL,
    output_index: UINT,
    input_frame_or_field: UINT,
    past_frames: UINT,
    future_frames: UINT,
    past_surfaces: *mut *mut c_void,
    input_surface: *mut c_void,
    future_surfaces: *mut *mut c_void,
    past_surfaces_right: *mut *mut c_void,
    input_surface_right: *mut c_void,
    future_surfaces_right: *mut *mut c_void,
}

#[link(name = "opengl32")]
unsafe extern "system" {
    fn wglGetCurrentContext() -> Hglrc;
    fn wglGetProcAddress(name: *const i8) -> *const c_void;
}
