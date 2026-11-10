//! Renderer dispatcher. Picks the glow or wgpu backend based on what the
//! active eframe `CreationContext` exposes, then forwards the common
//! `VideoFrameBuffer` upload/paint surface of both implementations so
//! `main.rs` only deals with one type.

use crate::render_gl::{GlowSlotPool, NativeVideoTexture};
use crate::render_wgpu::WgpuVideoTexture;
use crate::video_frame::{NativeSurfaceCapabilities, NativeSurfaceControl, VideoFrameBuffer};
use eframe::egui;

// One instance per session, dispatched on the per-frame paint path; boxing a
// variant would add heap indirection there for no real memory benefit.
#[allow(clippy::large_enum_variant)]
pub enum VideoTexture {
    Glow(NativeVideoTexture),
    Wgpu(WgpuVideoTexture),
}

impl VideoTexture {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        if cc.wgpu_render_state.is_some() {
            Self::Wgpu(WgpuVideoTexture::new())
        } else {
            Self::Glow(NativeVideoTexture::new(cc.gl.as_ref()))
        }
    }

    /// A texture for a secondary window. It only ever shows the output texture
    /// (no platform presenters, no native surfaces), so it also works from a
    /// window other than the root.
    pub fn new_secondary(wgpu: bool, glow_slots: GlowSlotPool) -> Self {
        if wgpu {
            Self::Wgpu(WgpuVideoTexture::new_secondary())
        } else {
            Self::Glow(NativeVideoTexture::new_secondary(glow_slots))
        }
    }

    /// Gives back what a secondary window's texture holds: its egui
    /// registration (wgpu) or its GL objects and egui slot (glow).
    pub fn release(&mut self, frame: &eframe::Frame) {
        match self {
            Self::Wgpu(t) => t.release(),
            Self::Glow(t) => {
                if let Some(gl) = frame.gl() {
                    t.release(gl);
                }
            }
        }
    }

    /// Glow uploads need the root window's `eframe::Frame`; wgpu uploads only
    /// need the render state.
    pub fn needs_root_frame(&self) -> bool {
        matches!(self, Self::Glow(_))
    }

    pub fn upload_with_render_state(
        &mut self,
        render_state: &eframe::egui_wgpu::RenderState,
        video: &VideoFrameBuffer,
    ) -> Result<(), String> {
        match self {
            Self::Wgpu(t) => t.upload_with(render_state, video),
            Self::Glow(_) => Err("glow uploads need the root frame".to_string()),
        }
    }

    pub fn has_frame(&self) -> bool {
        match self {
            Self::Glow(t) => t.has_frame(),
            Self::Wgpu(t) => t.has_frame(),
        }
    }

    pub fn texture_id(&self) -> Option<egui::TextureId> {
        match self {
            Self::Glow(t) => t.texture_id(),
            Self::Wgpu(t) => t.texture_id(),
        }
    }

    pub fn size_vec2(&self) -> egui::Vec2 {
        match self {
            Self::Glow(t) => t.size_vec2(),
            Self::Wgpu(t) => t.size_vec2(),
        }
    }

    pub fn clear_frame(&mut self) {
        match self {
            Self::Glow(t) => t.clear_frame(),
            Self::Wgpu(t) => t.clear_frame(),
        }
    }

    pub fn stage_direct_frame(&mut self, video: &VideoFrameBuffer) -> bool {
        match self {
            Self::Glow(t) => t.stage_direct_frame(video),
            Self::Wgpu(t) => t.stage_direct_frame(video),
        }
    }

    pub fn paint_direct_if_available(
        &mut self,
        frame: &eframe::Frame,
        ui: &egui::Ui,
        rect: egui::Rect,
    ) -> bool {
        match self {
            Self::Glow(t) => t.paint_direct_if_available(frame, ui, rect),
            Self::Wgpu(t) => t.paint_direct_if_available(frame, ui, rect),
        }
    }

    pub fn native_surface_capabilities(&self) -> NativeSurfaceCapabilities {
        match self {
            Self::Glow(t) => t.native_surface_capabilities(),
            Self::Wgpu(t) => t.native_surface_capabilities(),
        }
    }

    pub fn upload(
        &mut self,
        frame: &mut eframe::Frame,
        video: &VideoFrameBuffer,
        native_surfaces: &NativeSurfaceControl,
    ) -> Result<(), String> {
        match self {
            Self::Glow(t) => t.upload(frame, video, native_surfaces),
            Self::Wgpu(t) => t.upload(frame, video, native_surfaces),
        }
    }

    #[allow(dead_code)]
    pub fn current_native_video_rect(&self) -> Option<egui::Rect> {
        match self {
            Self::Glow(t) => t.current_native_video_rect(),
            Self::Wgpu(t) => t.current_native_video_rect(),
        }
    }

    pub fn set_windows_overlayless_preferred(&mut self, preferred: bool) {
        match self {
            Self::Glow(t) => t.set_windows_overlayless_preferred(preferred),
            Self::Wgpu(t) => t.set_windows_overlayless_preferred(preferred),
        }
    }

    pub fn occludes_egui_overlay(&self) -> bool {
        match self {
            Self::Glow(t) => t.occludes_egui_overlay(),
            Self::Wgpu(t) => t.occludes_egui_overlay(),
        }
    }
}
