use crate::video_frame::{MacosVideoToolboxFrame, VideoFormat};
use eframe::{egui, egui_glow, glow};
use glow::HasContext as _;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

pub struct MacosVideoToolboxImporter {
    luma_tex: glow::Texture,
    chroma_tex: glow::Texture,
    chroma_v_tex: glow::Texture,
}

pub struct RectYuvPipeline {
    program: glow::Program,
    framebuffer: glow::Framebuffer,
    vao: glow::VertexArray,
    _vbo: glow::Buffer,
    mode_uniform: glow::UniformLocation,
    luma_size_uniform: glow::UniformLocation,
    chroma_size_uniform: glow::UniformLocation,
}

#[derive(Clone, Default)]
pub struct MacosDirectVideoPresenter {
    inner: Arc<Mutex<MacosDirectVideoPresenterState>>,
}

#[derive(Default)]
struct MacosDirectVideoPresenterState {
    enabled: bool,
    logged_success: bool,
    frame: Option<MacosVideoToolboxFrame>,
    importer: Option<MacosVideoToolboxImporter>,
    pipeline: Option<RectYuvPipeline>,
}

type CGLContextObj = *mut c_void;
type IOSurfaceRef = *mut c_void;
type CVPixelBufferRef = *mut c_void;
type GLsizei = i32;
type GLenum = u32;
type GLuint = u32;
type CGLError = i32;

const GL_TEXTURE_RECTANGLE_ARB: u32 = 0x84F5;

impl MacosVideoToolboxImporter {
    pub fn supports_extensions(gl: &glow::Context) -> bool {
        let version = gl.version();
        gl.supported_extensions()
            .contains("GL_ARB_texture_rectangle")
            || gl
                .supported_extensions()
                .contains("GL_EXT_texture_rectangle")
            || (version.major >= 3 && !version.is_embedded)
    }

    pub fn new(gl: &glow::Context) -> Result<Self, String> {
        if !Self::supports_extensions(gl) {
            return Err("macOS rectangle-texture import unavailable".into());
        }
        if unsafe { CGLGetCurrentContext().is_null() } {
            return Err("macOS current OpenGL context unavailable".into());
        }

        Ok(Self {
            luma_tex: unsafe { create_rectangle_texture(gl)? },
            chroma_tex: unsafe { create_rectangle_texture(gl)? },
            chroma_v_tex: unsafe { create_rectangle_texture(gl)? },
        })
    }

    pub fn import_and_render(
        &mut self,
        gl: &glow::Context,
        output_texture: glow::Texture,
        pipeline: &mut RectYuvPipeline,
        frame: &MacosVideoToolboxFrame,
    ) -> Result<(), String> {
        let (luma_size, chroma_size) = self.import_planes(gl, frame)?;

        pipeline.render_textures(
            gl,
            output_texture,
            frame.width,
            frame.height,
            frame.format,
            self.luma_tex,
            self.chroma_tex,
            self.chroma_v_tex,
            luma_size,
            chroma_size,
        )
    }

    pub fn import_and_render_to_current(
        &mut self,
        gl: &glow::Context,
        pipeline: &mut RectYuvPipeline,
        frame: &MacosVideoToolboxFrame,
    ) -> Result<(), String> {
        let (luma_size, chroma_size) = self.import_planes(gl, frame)?;
        pipeline.render_textures_to_current(
            gl,
            frame.format,
            self.luma_tex,
            self.chroma_tex,
            self.chroma_v_tex,
            luma_size,
            chroma_size,
        )
    }

    fn import_planes(
        &mut self,
        gl: &glow::Context,
        frame: &MacosVideoToolboxFrame,
    ) -> Result<((u32, u32), (u32, u32)), String> {
        let context = unsafe { CGLGetCurrentContext() };
        if context.is_null() {
            return Err("CGL current context unavailable".into());
        }

        let pixel_buffer = frame.pixel_buffer.as_ptr();
        let plane_count = unsafe { CVPixelBufferGetPlaneCount(pixel_buffer) };
        if plane_count == 0 {
            return Err("VideoToolbox frame has no IOSurface planes".into());
        }

        let io_surface = unsafe { CVPixelBufferGetIOSurface(pixel_buffer) };
        if io_surface.is_null() {
            return Err("VideoToolbox frame is not IOSurface-backed".into());
        }

        let luma_size = plane_size(pixel_buffer, 0)?;
        bind_iosurface_plane(
            gl,
            context,
            self.luma_tex,
            glow::R8,
            glow::RED,
            luma_size.0,
            luma_size.1,
            io_surface,
            0,
        )?;

        let chroma_size = match frame.format {
            VideoFormat::Nv12 => {
                let size = plane_size(pixel_buffer, 1)?;
                bind_iosurface_plane(
                    gl,
                    context,
                    self.chroma_tex,
                    glow::RG8,
                    glow::RG,
                    size.0,
                    size.1,
                    io_surface,
                    1,
                )?;
                size
            }
            VideoFormat::Yuv420p8 => {
                let u_size = plane_size(pixel_buffer, 1)?;
                bind_iosurface_plane(
                    gl,
                    context,
                    self.chroma_tex,
                    glow::R8,
                    glow::RED,
                    u_size.0,
                    u_size.1,
                    io_surface,
                    1,
                )?;
                let v_size = plane_size(pixel_buffer, 2)?;
                bind_iosurface_plane(
                    gl,
                    context,
                    self.chroma_v_tex,
                    glow::R8,
                    glow::RED,
                    v_size.0,
                    v_size.1,
                    io_surface,
                    2,
                )?;
                u_size
            }
            VideoFormat::Rgba8 => return Err("unexpected RGBA VideoToolbox frame".into()),
        };

        Ok((luma_size, chroma_size))
    }
}

impl RectYuvPipeline {
    pub fn new(gl: &glow::Context) -> Result<Self, String> {
        let program = unsafe { create_rect_yuv_program(gl)? };
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

        let vertices: [f32; 16] = [
            -1.0, -1.0, 0.0, 1.0, 1.0, -1.0, 1.0, 1.0, -1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0,
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
        let luma_size_uniform = unsafe { gl.get_uniform_location(program, "u_luma_size") }
            .ok_or_else(|| "missing u_luma_size uniform".to_string())?;
        let chroma_size_uniform = unsafe { gl.get_uniform_location(program, "u_chroma_size") }
            .ok_or_else(|| "missing u_chroma_size uniform".to_string())?;

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

        Ok(Self {
            program,
            framebuffer,
            vao,
            _vbo: vbo,
            mode_uniform,
            luma_size_uniform,
            chroma_size_uniform,
        })
    }

    pub fn render_textures(
        &mut self,
        gl: &glow::Context,
        output_texture: glow::Texture,
        width: u32,
        height: u32,
        format: VideoFormat,
        luma_tex: glow::Texture,
        chroma_tex: glow::Texture,
        chroma_v_tex: glow::Texture,
        luma_size: (u32, u32),
        chroma_size: (u32, u32),
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
                return Err("VideoToolbox framebuffer incomplete".into());
            }

            gl.viewport(0, 0, width as i32, height as i32);
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.use_program(Some(self.program));
            gl.uniform_1_i32(
                Some(&self.mode_uniform),
                match format {
                    VideoFormat::Yuv420p8 => 0,
                    VideoFormat::Nv12 => 1,
                    VideoFormat::Rgba8 => 0,
                },
            );
            gl.uniform_2_f32(
                Some(&self.luma_size_uniform),
                luma_size.0 as f32,
                luma_size.1 as f32,
            );
            gl.uniform_2_f32(
                Some(&self.chroma_size_uniform),
                chroma_size.0 as f32,
                chroma_size.1 as f32,
            );

            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(GL_TEXTURE_RECTANGLE_ARB, Some(luma_tex));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(GL_TEXTURE_RECTANGLE_ARB, Some(chroma_tex));
            gl.active_texture(glow::TEXTURE2);
            gl.bind_texture(
                GL_TEXTURE_RECTANGLE_ARB,
                match format {
                    VideoFormat::Yuv420p8 => Some(chroma_v_tex),
                    VideoFormat::Nv12 | VideoFormat::Rgba8 => Some(chroma_tex),
                },
            );

            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
            gl.use_program(None);
            gl.bind_texture(GL_TEXTURE_RECTANGLE_ARB, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }

        Ok(())
    }

    pub fn render_textures_to_current(
        &mut self,
        gl: &glow::Context,
        format: VideoFormat,
        luma_tex: glow::Texture,
        chroma_tex: glow::Texture,
        chroma_v_tex: glow::Texture,
        luma_size: (u32, u32),
        chroma_size: (u32, u32),
    ) -> Result<(), String> {
        unsafe {
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.use_program(Some(self.program));
            gl.uniform_1_i32(
                Some(&self.mode_uniform),
                match format {
                    VideoFormat::Yuv420p8 => 0,
                    VideoFormat::Nv12 => 1,
                    VideoFormat::Rgba8 => 0,
                },
            );
            gl.uniform_2_f32(
                Some(&self.luma_size_uniform),
                luma_size.0 as f32,
                luma_size.1 as f32,
            );
            gl.uniform_2_f32(
                Some(&self.chroma_size_uniform),
                chroma_size.0 as f32,
                chroma_size.1 as f32,
            );

            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(GL_TEXTURE_RECTANGLE_ARB, Some(luma_tex));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(GL_TEXTURE_RECTANGLE_ARB, Some(chroma_tex));
            gl.active_texture(glow::TEXTURE2);
            gl.bind_texture(
                GL_TEXTURE_RECTANGLE_ARB,
                match format {
                    VideoFormat::Yuv420p8 => Some(chroma_v_tex),
                    VideoFormat::Nv12 | VideoFormat::Rgba8 => Some(chroma_tex),
                },
            );

            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
            gl.use_program(None);
            gl.bind_texture(GL_TEXTURE_RECTANGLE_ARB, None);
        }

        Ok(())
    }
}

impl MacosDirectVideoPresenter {
    pub fn new() -> Self {
        let mut state = MacosDirectVideoPresenterState::default();
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

    pub fn stage_frame(&self, frame: &MacosVideoToolboxFrame) {
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

impl MacosDirectVideoPresenterState {
    fn render(&mut self, info: egui::PaintCallbackInfo, painter: &egui_glow::Painter) {
        if !self.enabled {
            return;
        }
        let Some(frame) = self.frame.as_ref() else {
            return;
        };
        let gl = painter.gl();

        if self.importer.is_none() {
            match MacosVideoToolboxImporter::new(gl.as_ref()) {
                Ok(importer) => self.importer = Some(importer),
                Err(err) => {
                    eprintln!("[render] disabling macOS direct present path: {err}");
                    self.disable();
                    return;
                }
            }
        }
        if self.pipeline.is_none() {
            match RectYuvPipeline::new(gl.as_ref()) {
                Ok(pipeline) => self.pipeline = Some(pipeline),
                Err(err) => {
                    eprintln!("[render] disabling macOS direct present path: {err}");
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
            eprintln!("[render] disabling macOS direct present path: {err}");
            self.disable();
        } else if !self.logged_success {
            eprintln!("[render] macOS direct present path enabled");
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

fn plane_size(pixel_buffer: CVPixelBufferRef, plane: usize) -> Result<(u32, u32), String> {
    let plane_count = unsafe { CVPixelBufferGetPlaneCount(pixel_buffer) };
    if plane >= plane_count {
        return Err(format!(
            "VideoToolbox pixel buffer has {plane_count} planes, need {}",
            plane + 1
        ));
    }

    let width = unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, plane) };
    let height = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, plane) };
    if width == 0 || height == 0 {
        return Err(format!("VideoToolbox plane {plane} is empty"));
    }
    Ok((width as u32, height as u32))
}

fn bind_iosurface_plane(
    gl: &glow::Context,
    context: CGLContextObj,
    texture: glow::Texture,
    internal_format: u32,
    format: u32,
    width: u32,
    height: u32,
    io_surface: IOSurfaceRef,
    plane: GLuint,
) -> Result<(), String> {
    let err = unsafe {
        gl.bind_texture(GL_TEXTURE_RECTANGLE_ARB, Some(texture));
        let err = CGLTexImageIOSurface2D(
            context,
            GL_TEXTURE_RECTANGLE_ARB,
            internal_format,
            width as GLsizei,
            height as GLsizei,
            format,
            glow::UNSIGNED_BYTE,
            io_surface,
            plane,
        );
        gl.bind_texture(GL_TEXTURE_RECTANGLE_ARB, None);
        err
    };

    if err == 0 {
        Ok(())
    } else {
        Err(format!("CGLTexImageIOSurface2D failed: {err}"))
    }
}

unsafe fn create_rectangle_texture(gl: &glow::Context) -> Result<glow::Texture, String> {
    let texture = gl
        .create_texture()
        .map_err(|err| format!("create_texture: {err}"))?;
    gl.bind_texture(GL_TEXTURE_RECTANGLE_ARB, Some(texture));
    gl.tex_parameter_i32(
        GL_TEXTURE_RECTANGLE_ARB,
        glow::TEXTURE_MIN_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_parameter_i32(
        GL_TEXTURE_RECTANGLE_ARB,
        glow::TEXTURE_MAG_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_parameter_i32(
        GL_TEXTURE_RECTANGLE_ARB,
        glow::TEXTURE_WRAP_S,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        GL_TEXTURE_RECTANGLE_ARB,
        glow::TEXTURE_WRAP_T,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.bind_texture(GL_TEXTURE_RECTANGLE_ARB, None);
    Ok(texture)
}

unsafe fn create_rect_yuv_program(gl: &glow::Context) -> Result<glow::Program, String> {
    let vertex_src = "#version 330 core
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
out vec2 v_uv;
void main() {
    v_uv = a_uv;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
";
    let fragment_src = "#version 330 core
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2DRect u_luma;
uniform sampler2DRect u_chroma;
uniform sampler2DRect u_chroma_v;
uniform int u_mode;
uniform vec2 u_luma_size;
uniform vec2 u_chroma_size;
vec3 bt709_limited_to_rgb(float y, float u, float v) {
    float yy = max(y - 0.0625, 0.0) * 1.16438356;
    return vec3(
        yy + 1.79274107 * v,
        yy - 0.21324861 * u - 0.53290933 * v,
        yy + 2.11240179 * u
    );
}
void main() {
    vec2 luma_uv = v_uv * u_luma_size;
    vec2 chroma_uv = v_uv * u_chroma_size;
    float y = texture(u_luma, luma_uv).r;
    float u;
    float v;
    if (u_mode == 0) {
        u = texture(u_chroma, chroma_uv).r - 0.5;
        v = texture(u_chroma_v, chroma_uv).r - 0.5;
    } else {
        vec2 uv = texture(u_chroma, chroma_uv).rg - vec2(0.5, 0.5);
        u = uv.x;
        v = uv.y;
    }
    vec3 rgb = clamp(bt709_limited_to_rgb(y, u, v), 0.0, 1.0);
    frag_color = vec4(rgb, 1.0);
}
";

    let vertex = gl
        .create_shader(glow::VERTEX_SHADER)
        .map_err(|err| format!("create vertex shader: {err}"))?;
    gl.shader_source(vertex, vertex_src);
    gl.compile_shader(vertex);
    if !gl.get_shader_compile_status(vertex) {
        let log = gl.get_shader_info_log(vertex);
        gl.delete_shader(vertex);
        return Err(format!("vertex shader compile failed: {log}"));
    }

    let fragment = gl
        .create_shader(glow::FRAGMENT_SHADER)
        .map_err(|err| format!("create fragment shader: {err}"))?;
    gl.shader_source(fragment, fragment_src);
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

#[link(name = "OpenGL", kind = "framework")]
unsafe extern "C" {
    fn CGLGetCurrentContext() -> CGLContextObj;
    fn CGLTexImageIOSurface2D(
        ctx: CGLContextObj,
        target: GLenum,
        internal_format: GLenum,
        width: GLsizei,
        height: GLsizei,
        format: GLenum,
        ty: GLenum,
        io_surface: IOSurfaceRef,
        plane: GLuint,
    ) -> CGLError;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetPlaneCount(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetWidthOfPlane(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> usize;
    fn CVPixelBufferGetIOSurface(pixel_buffer: CVPixelBufferRef) -> IOSurfaceRef;
}
