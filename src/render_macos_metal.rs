#![allow(unexpected_cfgs)]

use crate::video_frame::{MacosVideoToolboxFrame, VideoFormat};
use core_graphics_types::geometry::{CGPoint, CGRect, CGSize};
use eframe::{egui, Frame};
use metal::{
    Buffer, CommandBuffer, CommandQueue, CompileOptions, Device, Library, MTLClearColor,
    MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLResourceOptions, MTLStoreAction,
    MetalLayer, RenderPassDescriptor, RenderPipelineDescriptor, RenderPipelineState, TextureRef,
};
use metal::foreign_types::ForeignType;
use objc::{
    class, msg_send, sel, sel_impl,
    rc::{autoreleasepool, StrongPtr},
    runtime::{Object, NO, YES},
};
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use std::{ffi::c_void, ptr::NonNull};

const NS_WINDOW_BELOW: i64 = -1;
const NS_WINDOW_ABOVE: i64 = 1;
const K_CV_PIXEL_FORMAT_TYPE_420YP_CBCR8_BI_PLANAR_VIDEO_RANGE: u32 = 0x34323076;
const K_CV_PIXEL_FORMAT_TYPE_420YP_CBCR8_PLANAR: u32 = 0x79343230;
const METAL_SHADER_SOURCE: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Vertex {
    float2 position;
    float2 uv;
};

struct VertexOut {
    float4 position [[position]];
    float2 uv;
};

struct FragmentParams {
    uint mode;
};

vertex VertexOut vs_main(uint vertex_id [[vertex_id]], constant Vertex* vertices [[buffer(0)]]) {
    VertexOut out;
    out.position = float4(vertices[vertex_id].position, 0.0, 1.0);
    out.uv = vertices[vertex_id].uv;
    return out;
}

float3 bt709_limited_to_rgb(float y, float u, float v) {
    float yy = max(y - 0.0625, 0.0) * 1.16438356;
    return float3(
        yy + 1.79274107 * v,
        yy - 0.21324861 * u - 0.53290933 * v,
        yy + 2.11240179 * u
    );
}

fragment float4 fs_main(
    VertexOut in [[stage_in]],
    texture2d<float> luma_tex [[texture(0)]],
    texture2d<float> chroma_tex [[texture(1)]],
    texture2d<float> chroma_v_tex [[texture(2)]],
    constant FragmentParams& params [[buffer(0)]]
) {
    constexpr sampler texture_sampler(coord::normalized, address::clamp_to_edge, filter::linear);
    float y = luma_tex.sample(texture_sampler, in.uv).r;
    float u;
    float v;
    if (params.mode == 0) {
        u = chroma_tex.sample(texture_sampler, in.uv).r - 0.5;
        v = chroma_v_tex.sample(texture_sampler, in.uv).r - 0.5;
    } else {
        float2 uv = chroma_tex.sample(texture_sampler, in.uv).rg - float2(0.5, 0.5);
        u = uv.x;
        v = uv.y;
    }
    float3 rgb = clamp(bt709_limited_to_rgb(y, u, v), 0.0, 1.0);
    return float4(rgb, 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    position: [f32; 2],
    uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FragmentParams {
    mode: u32,
    _padding: [u32; 3],
}

pub struct MacosMetalVideoPresenter {
    enabled: bool,
    logged_success: bool,
    frame: Option<MacosVideoToolboxFrame>,
    staged_serial: u64,
    rendered_serial: u64,
    renderer: Option<MetalVideoRenderer>,
}

impl MacosMetalVideoPresenter {
    pub fn supported() -> bool {
        Device::system_default().is_some()
    }

    pub fn new() -> Self {
        Self {
            enabled: true,
            logged_success: false,
            frame: None,
            staged_serial: 0,
            rendered_serial: 0,
            renderer: None,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn has_frame(&self) -> bool {
        self.enabled && self.frame.is_some()
    }

    pub fn clear(&mut self) {
        self.frame = None;
        self.staged_serial = 0;
        self.rendered_serial = 0;
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.hide();
        }
    }

    pub fn stage_frame(&mut self, frame: &MacosVideoToolboxFrame) -> bool {
        if !self.enabled {
            return false;
        }

        self.frame = Some(frame.clone());
        self.staged_serial = self.staged_serial.wrapping_add(1);
        true
    }

    pub fn present(&mut self, frame: &Frame, rect: egui::Rect, pixels_per_point: f32) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(video_frame) = self.frame.as_ref() else {
            return false;
        };

        let render_result = (|| -> Result<(), String> {
            if self.renderer.is_none() {
                self.renderer = Some(MetalVideoRenderer::new(frame)?);
            }
            self.renderer
                .as_mut()
                .expect("renderer initialized")
                .present(
                    video_frame,
                    rect,
                    pixels_per_point,
                    self.staged_serial != self.rendered_serial,
                )?;
            self.rendered_serial = self.staged_serial;
            Ok(())
        })();

        match render_result {
            Ok(()) => {
                if !self.logged_success {
                    eprintln!("[render] macOS Metal present path enabled");
                    self.logged_success = true;
                }
                true
            }
            Err(err) => {
                eprintln!("[render] disabling macOS Metal present path: {err}");
                self.disable();
                false
            }
        }
    }

    fn disable(&mut self) {
        self.enabled = false;
        self.logged_success = false;
        self.frame = None;
        self.renderer = None;
    }

    pub fn current_rect(&self) -> Option<egui::Rect> {
        self.renderer.as_ref().and_then(MetalVideoRenderer::current_rect)
    }
}

struct MetalVideoRenderer {
    root_view: NonNull<Object>,
    parent_view: NonNull<Object>,
    background_view: NativeBackgroundView,
    host_view: StrongPtr,
    layer: MetalLayer,
    _device: Device,
    command_queue: CommandQueue,
    pipeline_state: RenderPipelineState,
    vertex_buffer: Buffer,
    texture_cache: CVMetalTextureCacheRef,
    pending_submissions: Vec<PendingSubmission>,
    last_rect: Option<egui::Rect>,
    last_scale: f32,
}

impl MetalVideoRenderer {
    fn new(frame: &Frame) -> Result<Self, String> {
        let root_view = root_ns_view(frame)?;
        configure_window_for_underlay(root_view)?;
        let parent_view = superview(root_view)?;
        let device =
            Device::system_default().ok_or_else(|| "Metal device unavailable".to_string())?;
        let command_queue = device.new_command_queue();
        let library = compile_library(&device)?;
        let pipeline_state = build_pipeline_state(&device, &library)?;
        let vertex_buffer = build_vertex_buffer(&device);
        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_presents_with_transaction(false);
        layer.set_display_sync_enabled(true);
        layer.set_framebuffer_only(true);
        layer.set_maximum_drawable_count(2);
        layer.remove_all_animations();

        let background_view = NativeBackgroundView::new(parent_view, root_view)?;
        let host_view = create_host_view(
            parent_view,
            NonNull::new(*background_view.view)
                .ok_or_else(|| "background NSView unexpectedly null".to_string())?,
            &layer,
        )?;
        let texture_cache = create_texture_cache(&device)?;

        Ok(Self {
            root_view,
            parent_view,
            background_view,
            host_view,
            layer,
            _device: device,
            command_queue,
            pipeline_state,
            vertex_buffer,
            texture_cache,
            pending_submissions: Vec::new(),
            last_rect: None,
            last_scale: 0.0,
        })
    }

    fn present(
        &mut self,
        frame: &MacosVideoToolboxFrame,
        rect: egui::Rect,
        pixels_per_point: f32,
        frame_dirty: bool,
    ) -> Result<(), String> {
        self.reap_pending_submissions();

        let visible = rect.width() >= 1.0 && rect.height() >= 1.0;
        if !visible {
            self.hide();
            return Ok(());
        }

        let view_changed = self.update_host_view(rect, pixels_per_point)?;
        if !frame_dirty && !view_changed {
            return Ok(());
        }

        autoreleasepool(|| self.render_frame(frame))
    }

    fn hide(&mut self) {
        unsafe {
            let () = msg_send![*self.host_view, setHidden: YES];
        }
        self.last_rect = None;
    }

    fn update_host_view(
        &mut self,
        rect: egui::Rect,
        pixels_per_point: f32,
    ) -> Result<bool, String> {
        let local_rect = CGRect::new(
            &CGPoint::new(rect.left() as f64, rect.top() as f64),
            &CGSize::new(rect.width() as f64, rect.height() as f64),
        );
        let parent_rect: CGRect = unsafe {
            msg_send![self.root_view.as_ptr(), convertRect: local_rect toView: self.parent_view.as_ptr()]
        };
        let background_rect = root_view_rect_in_parent(self.root_view, self.parent_view);
        self.background_view.update(background_rect);

        let changed = self.last_rect != Some(rect)
            || (self.last_scale - pixels_per_point).abs() > f32::EPSILON;
        if changed {
            unsafe {
                let () = msg_send![*self.host_view, setFrame: parent_rect];
                let () = msg_send![*self.host_view, setHidden: NO];
            }
            self.layer.set_drawable_size(CGSize::new(
                (rect.width().max(1.0) * pixels_per_point) as f64,
                (rect.height().max(1.0) * pixels_per_point) as f64,
            ));
            self.layer.set_contents_scale(pixels_per_point as f64);
            self.last_rect = Some(rect);
            self.last_scale = pixels_per_point;
        } else {
            unsafe {
                let () = msg_send![*self.host_view, setHidden: NO];
            }
        }

        Ok(changed)
    }

    fn render_frame(&mut self, frame: &MacosVideoToolboxFrame) -> Result<(), String> {
        let drawable = self
            .layer
            .next_drawable()
            .ok_or_else(|| "CAMetalLayer next_drawable unavailable".to_string())?;
        let imported_textures = self.import_textures(frame)?;

        let render_pass_descriptor = RenderPassDescriptor::new();
        let color_attachment = render_pass_descriptor
            .color_attachments()
            .object_at(0)
            .ok_or_else(|| "Metal render pass missing color attachment".to_string())?;
        color_attachment.set_texture(Some(drawable.texture()));
        color_attachment.set_load_action(MTLLoadAction::Clear);
        color_attachment.set_clear_color(MTLClearColor::new(0.0, 0.0, 0.0, 1.0));
        color_attachment.set_store_action(MTLStoreAction::Store);

        let command_buffer_ref = self.command_queue.new_command_buffer();
        let retained: *mut Object = unsafe { msg_send![command_buffer_ref, retain] };
        let command_buffer = unsafe { CommandBuffer::from_ptr(retained.cast()) };
        let encoder = command_buffer.new_render_command_encoder(render_pass_descriptor);
        encoder.set_render_pipeline_state(&self.pipeline_state);
        encoder.set_vertex_buffer(0, Some(&self.vertex_buffer), 0);

        let fragment_params = FragmentParams {
            mode: match frame.format {
                VideoFormat::Yuv420p8 => 0,
                VideoFormat::Nv12 => 1,
                VideoFormat::Rgba8 => return Err("unexpected RGBA VideoToolbox frame".into()),
            },
            _padding: [0; 3],
        };
        encoder.set_fragment_bytes(
            0,
            std::mem::size_of::<FragmentParams>() as u64,
            (&fragment_params as *const FragmentParams).cast(),
        );
        encoder.set_fragment_texture(0, Some(imported_textures.textures[0]));
        encoder.set_fragment_texture(1, Some(imported_textures.textures[1]));
        encoder.set_fragment_texture(2, Some(imported_textures.textures[2]));
        encoder.draw_primitives(MTLPrimitiveType::TriangleStrip, 0, 4);
        encoder.end_encoding();

        command_buffer.present_drawable(drawable);
        command_buffer.commit();

        self.pending_submissions.push(PendingSubmission {
            command_buffer,
            cv_textures: imported_textures.cv_textures,
        });

        Ok(())
    }

    fn current_rect(&self) -> Option<egui::Rect> {
        let host_rect_in_parent: CGRect = unsafe { msg_send![*self.host_view, frame] };
        let host_rect_in_root = parent_rect_in_root_view(self.root_view, self.parent_view, host_rect_in_parent);
        let width = host_rect_in_root.size.width as f32;
        let height = host_rect_in_root.size.height as f32;
        if width < 1.0 || height < 1.0 {
            return None;
        }

        Some(egui::Rect::from_min_size(
            egui::pos2(
                host_rect_in_root.origin.x as f32,
                host_rect_in_root.origin.y as f32,
            ),
            egui::vec2(width, height),
        ))
    }

    fn import_textures(
        &self,
        frame: &MacosVideoToolboxFrame,
    ) -> Result<ImportedTextures<'_>, String> {
        let pixel_buffer = frame.pixel_buffer.as_ptr();
        let plane_count = unsafe { CVPixelBufferGetPlaneCount(pixel_buffer) };
        let pixel_format = unsafe { CVPixelBufferGetPixelFormatType(pixel_buffer) };

        match frame.format {
            VideoFormat::Nv12 => {
                if plane_count < 2
                    || pixel_format != K_CV_PIXEL_FORMAT_TYPE_420YP_CBCR8_BI_PLANAR_VIDEO_RANGE
                {
                    return Err(format!(
                        "unsupported VideoToolbox NV12 layout: planes={plane_count} format=0x{pixel_format:08x}"
                    ));
                }
                let luma = self.import_plane(pixel_buffer, 0, MTLPixelFormat::R8Unorm)?;
                let chroma = self.import_plane(pixel_buffer, 1, MTLPixelFormat::RG8Unorm)?;
                Ok(ImportedTextures {
                    textures: [luma.texture, chroma.texture, chroma.texture],
                    cv_textures: vec![luma.cv_texture, chroma.cv_texture],
                })
            }
            VideoFormat::Yuv420p8 => {
                if plane_count < 3 || pixel_format != K_CV_PIXEL_FORMAT_TYPE_420YP_CBCR8_PLANAR {
                    return Err(format!(
                        "unsupported VideoToolbox YUV420 layout: planes={plane_count} format=0x{pixel_format:08x}"
                    ));
                }
                let y = self.import_plane(pixel_buffer, 0, MTLPixelFormat::R8Unorm)?;
                let u = self.import_plane(pixel_buffer, 1, MTLPixelFormat::R8Unorm)?;
                let v = self.import_plane(pixel_buffer, 2, MTLPixelFormat::R8Unorm)?;
                Ok(ImportedTextures {
                    textures: [y.texture, u.texture, v.texture],
                    cv_textures: vec![y.cv_texture, u.cv_texture, v.cv_texture],
                })
            }
            VideoFormat::Rgba8 => Err("unexpected RGBA VideoToolbox frame".into()),
        }
    }

    fn import_plane(
        &self,
        pixel_buffer: CVPixelBufferRef,
        plane: usize,
        pixel_format: MTLPixelFormat,
    ) -> Result<ImportedPlane<'_>, String> {
        let width = unsafe { CVPixelBufferGetWidthOfPlane(pixel_buffer, plane) };
        let height = unsafe { CVPixelBufferGetHeightOfPlane(pixel_buffer, plane) };
        let mut cv_texture: CVMetalTextureRef = std::ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCacheCreateTextureFromImage(
                std::ptr::null(),
                self.texture_cache,
                pixel_buffer,
                std::ptr::null(),
                pixel_format as u64,
                width,
                height,
                plane,
                &mut cv_texture,
            )
        };
        if status != 0 || cv_texture.is_null() {
            return Err(format!(
                "CVMetalTextureCacheCreateTextureFromImage failed for plane {plane}: {status}"
            ));
        }

        let texture = unsafe { CVMetalTextureGetTexture(cv_texture) };
        if texture.is_null() {
            unsafe {
                cf_release(cv_texture.cast());
            }
            return Err(format!(
                "CVMetalTextureGetTexture returned null for plane {plane}"
            ));
        }

        Ok(ImportedPlane {
            texture: unsafe { &*(texture.cast::<TextureRef>()) },
            cv_texture: NonNull::new(cv_texture.cast())
                .ok_or_else(|| "CVMetalTextureRef unexpectedly null".to_string())?,
        })
    }

    fn reap_pending_submissions(&mut self) {
        let mut completed = 0usize;
        for submission in &self.pending_submissions {
            let status = submission.command_buffer.status();
            if matches!(
                status,
                metal::MTLCommandBufferStatus::Completed | metal::MTLCommandBufferStatus::Error
            ) {
                release_cv_textures(&submission.cv_textures);
                completed += 1;
            } else {
                break;
            }
        }

        if completed > 0 {
            self.pending_submissions.drain(..completed);
        }
    }
}

impl Drop for MetalVideoRenderer {
    fn drop(&mut self) {
        self.hide();
        unsafe {
            let () = msg_send![*self.host_view, removeFromSuperview];
            let () = msg_send![*self.background_view.view, removeFromSuperview];
        }
        for submission in &self.pending_submissions {
            submission.command_buffer.wait_until_completed();
            release_cv_textures(&submission.cv_textures);
        }
        self.pending_submissions.clear();
        unsafe {
            CVMetalTextureCacheFlush(self.texture_cache, 0);
            cf_release(self.texture_cache.cast());
        }
    }
}

struct PendingSubmission {
    command_buffer: CommandBuffer,
    cv_textures: Vec<NonNull<c_void>>,
}

struct ImportedTextures<'a> {
    textures: [&'a TextureRef; 3],
    cv_textures: Vec<NonNull<c_void>>,
}

struct ImportedPlane<'a> {
    texture: &'a TextureRef,
    cv_texture: NonNull<c_void>,
}

struct NativeBackgroundView {
    view: StrongPtr,
    _layer: StrongPtr,
    accent_primary: StrongPtr,
    accent_secondary: StrongPtr,
}

impl NativeBackgroundView {
    fn new(parent_view: NonNull<Object>, root_view: NonNull<Object>) -> Result<Self, String> {
        let frame = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(0.0, 0.0));
        let view: *mut Object = unsafe {
            let alloc: *mut Object = msg_send![class!(NSView), alloc];
            msg_send![alloc, initWithFrame: frame]
        };
        let view = NonNull::new(view)
            .ok_or_else(|| "failed to create NSView for macOS background".to_string())?;
        let layer = new_ca_layer("failed to create background CALayer")?;
        let accent_primary = new_ca_layer("failed to create primary accent CALayer")?;
        let accent_secondary = new_ca_layer("failed to create secondary accent CALayer")?;

        unsafe {
            let () = msg_send![view.as_ptr(), setAutoresizingMask: 0usize];
            let () = msg_send![view.as_ptr(), setHidden: YES];
            let () = msg_send![view.as_ptr(), setWantsLayer: YES];
            let () = msg_send![*layer, setOpaque: YES];

            set_layer_color(
                *layer,
                ns_color(7.0 / 255.0, 10.0 / 255.0, 14.0 / 255.0, 1.0),
            );
            let () = msg_send![*accent_primary, setOpaque: NO];
            set_layer_color(
                *accent_primary,
                ns_color(54.0 / 255.0, 156.0 / 255.0, 1.0, 20.0 / 255.0),
            );
            let () = msg_send![*accent_secondary, setOpaque: NO];
            set_layer_color(
                *accent_secondary,
                ns_color(34.0 / 255.0, 198.0 / 255.0, 140.0 / 255.0, 16.0 / 255.0),
            );

            let () = msg_send![*layer, addSublayer: *accent_primary];
            let () = msg_send![*layer, addSublayer: *accent_secondary];
            let () = msg_send![view.as_ptr(), setLayer: *layer];
            let () = msg_send![
                parent_view.as_ptr(),
                addSubview: view.as_ptr()
                positioned: NS_WINDOW_BELOW
                relativeTo: root_view.as_ptr()
            ];
        }

        Ok(Self {
            view: unsafe { StrongPtr::new(view.as_ptr()) },
            _layer: layer,
            accent_primary,
            accent_secondary,
        })
    }

    fn update(&mut self, frame: CGRect) {
        let width = frame.size.width.max(1.0);
        let height = frame.size.height.max(1.0);
        let min_dimension = width.min(height);

        let primary_radius = min_dimension * 0.16;
        let primary_frame = circle_frame(
            CGPoint::new(width * 0.18, height * 0.22),
            primary_radius,
        );
        let secondary_radius = min_dimension * 0.20;
        let secondary_frame = circle_frame(
            CGPoint::new(width * 0.84, height * 0.82),
            secondary_radius,
        );

        unsafe {
            let () = msg_send![class!(CATransaction), begin];
            let () = msg_send![class!(CATransaction), setDisableActions: YES];
            let () = msg_send![*self.view, setFrame: frame];
            let () = msg_send![*self.view, setHidden: NO];
            let () = msg_send![*self.accent_primary, setFrame: primary_frame];
            let () = msg_send![*self.accent_primary, setCornerRadius: primary_radius];
            let () = msg_send![*self.accent_secondary, setFrame: secondary_frame];
            let () = msg_send![*self.accent_secondary, setCornerRadius: secondary_radius];
            let () = msg_send![class!(CATransaction), commit];
        }
    }
}

fn root_ns_view(frame: &Frame) -> Result<NonNull<Object>, String> {
    let handle = frame
        .window_handle()
        .map_err(|err| format!("window_handle unavailable: {err}"))?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(appkit) => Ok(appkit.ns_view.cast()),
        other => Err(format!("unsupported macOS window handle: {other:?}")),
    }
}

fn configure_window_for_underlay(view: NonNull<Object>) -> Result<(), String> {
    let window: *mut Object = unsafe { msg_send![view.as_ptr(), window] };
    let window =
        NonNull::new(window).ok_or_else(|| "AppKit window unavailable for Metal presenter".to_string())?;
    let clear_color: *mut Object = unsafe { msg_send![class!(NSColor), clearColor] };

    unsafe {
        let () = msg_send![window.as_ptr(), setOpaque: NO];
        if !clear_color.is_null() {
            let () = msg_send![window.as_ptr(), setBackgroundColor: clear_color];
        }

        let root_layer: *mut Object = msg_send![view.as_ptr(), layer];
        if let Some(root_layer) = NonNull::new(root_layer) {
            let () = msg_send![root_layer.as_ptr(), setOpaque: NO];
            if !clear_color.is_null() {
                let cg_color: *mut c_void = msg_send![clear_color, CGColor];
                if !cg_color.is_null() {
                    let () = msg_send![root_layer.as_ptr(), setBackgroundColor: cg_color];
                }
            }
        }
    }

    Ok(())
}

fn superview(view: NonNull<Object>) -> Result<NonNull<Object>, String> {
    let superview: *mut Object = unsafe { msg_send![view.as_ptr(), superview] };
    NonNull::new(superview).ok_or_else(|| "AppKit superview unavailable".to_string())
}

fn root_view_rect_in_parent(root_view: NonNull<Object>, parent_view: NonNull<Object>) -> CGRect {
    let root_bounds: CGRect = unsafe { msg_send![root_view.as_ptr(), bounds] };
    unsafe { msg_send![root_view.as_ptr(), convertRect: root_bounds toView: parent_view.as_ptr()] }
}

fn parent_rect_in_root_view(
    root_view: NonNull<Object>,
    parent_view: NonNull<Object>,
    rect: CGRect,
) -> CGRect {
    unsafe { msg_send![root_view.as_ptr(), convertRect: rect fromView: parent_view.as_ptr()] }
}

fn create_host_view(
    parent_view: NonNull<Object>,
    background_view: NonNull<Object>,
    layer: &MetalLayer,
) -> Result<StrongPtr, String> {
    let frame = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(0.0, 0.0));
    let view: *mut Object = unsafe {
        let alloc: *mut Object = msg_send![class!(NSView), alloc];
        msg_send![alloc, initWithFrame: frame]
    };
    let view = NonNull::new(view)
        .ok_or_else(|| "failed to create NSView for Metal presenter".to_string())?;
    unsafe {
        let () = msg_send![view.as_ptr(), setAutoresizingMask: 0usize];
        let () = msg_send![view.as_ptr(), setHidden: YES];
        let () = msg_send![view.as_ptr(), setWantsLayer: YES];
        let () = msg_send![view.as_ptr(), setLayer: layer.as_ref()];
        let () = msg_send![
            parent_view.as_ptr(),
            addSubview: view.as_ptr()
            positioned: NS_WINDOW_ABOVE
            relativeTo: background_view.as_ptr()
        ];
        Ok(StrongPtr::new(view.as_ptr()))
    }
}

fn new_ca_layer(error_message: &'static str) -> Result<StrongPtr, String> {
    let layer: *mut Object = unsafe { msg_send![class!(CALayer), new] };
    let layer = NonNull::new(layer).ok_or_else(|| error_message.to_string())?;
    Ok(unsafe { StrongPtr::new(layer.as_ptr()) })
}

fn ns_color(red: f64, green: f64, blue: f64, alpha: f64) -> *mut Object {
    unsafe {
        msg_send![
            class!(NSColor),
            colorWithSRGBRed: red
            green: green
            blue: blue
            alpha: alpha
        ]
    }
}

fn set_layer_color(layer: *mut Object, color: *mut Object) {
    if color.is_null() {
        return;
    }

    let cg_color: *mut c_void = unsafe { msg_send![color, CGColor] };
    if cg_color.is_null() {
        return;
    }

    unsafe {
        let () = msg_send![layer, setBackgroundColor: cg_color];
    }
}

fn circle_frame(center: CGPoint, radius: f64) -> CGRect {
    CGRect::new(
        &CGPoint::new(center.x - radius, center.y - radius),
        &CGSize::new(radius * 2.0, radius * 2.0),
    )
}

fn compile_library(device: &Device) -> Result<Library, String> {
    device.new_library_with_source(METAL_SHADER_SOURCE, &CompileOptions::new())
}

fn build_pipeline_state(device: &Device, library: &Library) -> Result<RenderPipelineState, String> {
    let vertex = library
        .get_function("vs_main", None)
        .map_err(|err| format!("Metal vertex function unavailable: {err}"))?;
    let fragment = library
        .get_function("fs_main", None)
        .map_err(|err| format!("Metal fragment function unavailable: {err}"))?;
    let descriptor = RenderPipelineDescriptor::new();
    descriptor.set_vertex_function(Some(&vertex));
    descriptor.set_fragment_function(Some(&fragment));
    let attachment = descriptor
        .color_attachments()
        .object_at(0)
        .ok_or_else(|| "Metal pipeline color attachment unavailable".to_string())?;
    attachment.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
    device
        .new_render_pipeline_state(&descriptor)
        .map_err(|err| format!("Metal pipeline creation failed: {err}"))
}

fn build_vertex_buffer(device: &Device) -> Buffer {
    let vertices = [
        Vertex {
            position: [-1.0, -1.0],
            uv: [0.0, 1.0],
        },
        Vertex {
            position: [1.0, -1.0],
            uv: [1.0, 1.0],
        },
        Vertex {
            position: [-1.0, 1.0],
            uv: [0.0, 0.0],
        },
        Vertex {
            position: [1.0, 1.0],
            uv: [1.0, 0.0],
        },
    ];
    device.new_buffer_with_data(
        vertices.as_ptr().cast(),
        std::mem::size_of_val(&vertices) as u64,
        MTLResourceOptions::CPUCacheModeDefaultCache | MTLResourceOptions::StorageModeManaged,
    )
}

fn create_texture_cache(device: &Device) -> Result<CVMetalTextureCacheRef, String> {
    let mut texture_cache = std::ptr::null_mut();
    let status = unsafe {
        CVMetalTextureCacheCreate(
            std::ptr::null(),
            std::ptr::null(),
            device.as_ptr() as *mut c_void,
            std::ptr::null(),
            &mut texture_cache,
        )
    };
    if status != 0 || texture_cache.is_null() {
        Err(format!("CVMetalTextureCacheCreate failed: {status}"))
    } else {
        Ok(texture_cache)
    }
}

fn release_cv_textures(textures: &[NonNull<c_void>]) {
    for texture in textures {
        unsafe {
            cf_release(texture.as_ptr());
        }
    }
}

type CVPixelBufferRef = *mut c_void;
type CVMetalTextureCacheRef = *mut c_void;
type CVMetalTextureRef = *mut c_void;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVMetalTextureCacheCreate(
        allocator: *const c_void,
        cache_attributes: *const c_void,
        metal_device: *mut c_void,
        texture_attributes: *const c_void,
        cache_out: *mut CVMetalTextureCacheRef,
    ) -> i32;
    fn CVMetalTextureCacheCreateTextureFromImage(
        allocator: *const c_void,
        texture_cache: CVMetalTextureCacheRef,
        source_image: CVPixelBufferRef,
        texture_attributes: *const c_void,
        pixel_format: u64,
        width: usize,
        height: usize,
        plane_index: usize,
        texture_out: *mut CVMetalTextureRef,
    ) -> i32;
    fn CVMetalTextureCacheFlush(texture_cache: CVMetalTextureCacheRef, options: usize);
    fn CVMetalTextureGetTexture(texture: CVMetalTextureRef) -> *mut c_void;
    fn CVPixelBufferGetPlaneCount(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetWidthOfPlane(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> usize;
    fn CVPixelBufferGetPixelFormatType(pixel_buffer: CVPixelBufferRef) -> u32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

unsafe fn cf_release(ptr: *mut c_void) {
    if !ptr.is_null() {
        CFRelease(ptr.cast_const());
    }
}
