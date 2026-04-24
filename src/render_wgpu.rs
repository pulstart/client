//! wgpu-backed video renderer.
//!
//! Renders YUV420p / YUV444p / NV12 / RGBA8 video frames into an egui-registered
//! RGBA8 output texture via a WGSL BT.709 limited-range conversion. This is the
//! CPU-upload path (`queue.write_texture` per plane), mirroring the default
//! `NativeVideoTexture` (Glow) code path. Zero-copy native-surface import
//! (DMA-BUF / VideoToolbox / D3D11) is not yet implemented here; callers must
//! fall back to the CPU planes path when the wgpu renderer is active. See
//! `NativeSurfaceCapabilities` — this backend reports all native surfaces as
//! unsupported so `VideoFrameBuffer` skips building them.

#[cfg(target_os = "linux")]
use crate::video_frame::LinuxDmaBufFormat;
use crate::video_frame::{
    NativeSurfaceCapabilities, NativeSurfaceControl, VideoFormat, VideoFrameBuffer,
};
use eframe::egui;
use eframe::wgpu;
use std::num::NonZeroU64;

pub struct WgpuVideoTexture {
    state: Option<PipelineState>,
    texture_id: Option<egui::TextureId>,
    width: u32,
    height: u32,
    #[cfg(target_os = "linux")]
    dmabuf_support: DmaBufSupportState,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
enum DmaBufSupportState {
    /// First upload will probe the active wgpu device.
    Unprobed,
    /// Probed and usable (ST_WGPU_DMABUF=1 + required Vulkan extensions
    /// enabled). Path is taken whenever the incoming frame carries a
    /// linear-modifier DMA-BUF.
    Enabled,
    /// Probed and disabled (env not set, wrong backend, or extensions
    /// missing). Stays in CPU upload mode for the session.
    Disabled,
}

struct PipelineState {
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    bind_group_layout: wgpu::BindGroupLayout,
    uniform_buf: wgpu::Buffer,
    output: Option<OutputSurface>,
    planes: Option<Planes>,
    bind_group: Option<wgpu::BindGroup>,
    last_mode: u32,
}

struct OutputSurface {
    // Keep the texture so tests (and future direct-readback paths) can
    // `copy_texture_to_buffer` without re-rendering. wgpu ref-counts the
    // underlying GPU resource, so holding both `texture` and `view` is cheap.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

struct Planes {
    luma: PlaneTexture,
    chroma: PlaneTexture,
    chroma_v: PlaneTexture,
    format: VideoFormat,
}

struct PlaneTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

impl WgpuVideoTexture {
    pub fn new() -> Self {
        Self {
            state: None,
            texture_id: None,
            width: 0,
            height: 0,
            #[cfg(target_os = "linux")]
            dmabuf_support: DmaBufSupportState::Unprobed,
        }
    }

    pub fn has_frame(&self) -> bool {
        self.texture_id.is_some() && self.width != 0 && self.height != 0
    }

    pub fn texture_id(&self) -> Option<egui::TextureId> {
        self.texture_id
    }

    pub fn size_vec2(&self) -> egui::Vec2 {
        egui::vec2(self.width as f32, self.height as f32)
    }

    #[allow(dead_code)]
    pub fn current_native_video_rect(&self) -> Option<egui::Rect> {
        None
    }

    pub fn set_windows_overlayless_preferred(&mut self, _preferred: bool) {}

    pub fn occludes_egui_overlay(&self) -> bool {
        false
    }

    pub fn clear_frame(&mut self) {
        self.width = 0;
        self.height = 0;
    }

    pub fn stage_direct_frame(&mut self, _video: &VideoFrameBuffer) -> bool {
        false
    }

    pub fn paint_direct_if_available(
        &mut self,
        _frame: &eframe::Frame,
        _ui: &egui::Ui,
        _rect: egui::Rect,
    ) -> bool {
        false
    }

    pub fn native_surface_capabilities(&self) -> NativeSurfaceCapabilities {
        let mut caps = NativeSurfaceCapabilities::default();
        #[cfg(target_os = "linux")]
        {
            caps.linux_dmabuf = matches!(self.dmabuf_support, DmaBufSupportState::Enabled);
        }
        caps
    }

    #[cfg(target_os = "linux")]
    fn maybe_probe_dmabuf(&mut self, device: &wgpu::Device) {
        if !matches!(self.dmabuf_support, DmaBufSupportState::Unprobed) {
            return;
        }
        if !crate::render_wgpu_linux_dmabuf::dmabuf_requested() {
            eprintln!(
                "[render] wgpu DMA-BUF import force-disabled by ST_WGPU_DMABUF=0; \
                 staying on CPU upload path"
            );
            self.dmabuf_support = DmaBufSupportState::Disabled;
            return;
        }
        let caps = crate::render_wgpu_linux_dmabuf::probe(device);
        if caps.is_supported() {
            eprintln!(
                "[render] wgpu DMA-BUF zero-copy import enabled \
                 (VK_KHR_external_memory_fd + VK_EXT_external_memory_dma_buf)"
            );
            self.dmabuf_support = DmaBufSupportState::Enabled;
        } else {
            eprintln!(
                "[render] wgpu DMA-BUF import unavailable on this adapter ({caps:?}); \
                 staying on CPU upload path"
            );
            self.dmabuf_support = DmaBufSupportState::Disabled;
        }
    }

    pub fn upload(
        &mut self,
        frame: &mut eframe::Frame,
        video: &VideoFrameBuffer,
        _native_surfaces: &NativeSurfaceControl,
    ) -> Result<(), String> {
        let render_state = frame
            .wgpu_render_state()
            .ok_or_else(|| "wgpu renderer unavailable".to_string())?;

        #[cfg(target_os = "linux")]
        self.maybe_probe_dmabuf(&render_state.device);

        // On Linux, decide before borrowing `state` whether we should take
        // the dmabuf path — otherwise the borrow checker complains about
        // reading `self.dmabuf_support` mid-borrow.
        #[cfg(target_os = "linux")]
        let try_dmabuf =
            matches!(self.dmabuf_support, DmaBufSupportState::Enabled) && video.dmabuf.is_some();
        #[cfg(not(target_os = "linux"))]
        let try_dmabuf = false;

        let existing_id = self.texture_id;
        let mut dmabuf_failed = false;
        let new_id = {
            let state = self.ensure_state(&render_state.device, &render_state.queue)?;
            state.ensure_output(&render_state.device, video.width, video.height);

            let mut used_dmabuf = false;
            #[cfg(target_os = "linux")]
            if try_dmabuf {
                let dmabuf = video.dmabuf.as_ref().expect("checked above");
                match state.render_from_dmabuf(&render_state.device, &render_state.queue, dmabuf) {
                    Ok(()) => used_dmabuf = true,
                    Err(err) => {
                        eprintln!(
                            "[render] wgpu DMA-BUF import failed ({err}); \
                             disabling fast path for the rest of the session"
                        );
                        dmabuf_failed = true;
                    }
                }
            }
            let _ = try_dmabuf;
            if !used_dmabuf {
                state.upload_and_render(&render_state.device, &render_state.queue, video)?;
            }
            let output_view = state
                .output
                .as_ref()
                .map(|out| &out.view)
                .ok_or_else(|| "wgpu output surface missing after upload".to_string())?;
            // (Re)register the output view with egui. egui keys bind groups
            // by TextureId + View, so reuse the same id across frames while
            // the surface is stable; the Renderer is happy to rebind the
            // underlying view when the output texture is recreated on resize.
            match existing_id {
                Some(id) => {
                    render_state
                        .renderer
                        .write()
                        .update_egui_texture_from_wgpu_texture(
                            &render_state.device,
                            output_view,
                            wgpu::FilterMode::Linear,
                            id,
                        );
                    id
                }
                None => render_state.renderer.write().register_native_texture(
                    &render_state.device,
                    output_view,
                    wgpu::FilterMode::Linear,
                ),
            }
        };

        self.texture_id = Some(new_id);
        self.width = video.width;
        self.height = video.height;
        #[cfg(target_os = "linux")]
        if dmabuf_failed {
            self.dmabuf_support = DmaBufSupportState::Disabled;
        }
        let _ = dmabuf_failed;
        Ok(())
    }

    fn ensure_state(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<&mut PipelineState, String> {
        if self.state.is_none() {
            self.state = Some(PipelineState::new(device, queue)?);
        }
        Ok(self.state.as_mut().expect("state initialized"))
    }
}

impl PipelineState {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Self, String> {
        // Three sampled textures (luma, chroma_u/uv, chroma_v) + one sampler
        // + one uniform (mode selector).
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("st-wgpu-video.bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(16),
                    },
                    count: None,
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("st-wgpu-video.wgsl"),
            source: wgpu::ShaderSource::Wgsl(VIDEO_WGSL.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("st-wgpu-video.pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("st-wgpu-video.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: OUTPUT_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("st-wgpu-video.sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("st-wgpu-video.uniform"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // Initialize as planar (mode=0).
        queue.write_buffer(&uniform_buf, 0, &[0u8; 16]);

        Ok(Self {
            pipeline,
            sampler,
            bind_group_layout,
            uniform_buf,
            output: None,
            planes: None,
            bind_group: None,
            last_mode: 0,
        })
    }

    fn ensure_output(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let needs_alloc = match &self.output {
            Some(out) => out.width != width || out.height != height,
            None => true,
        };
        if !needs_alloc {
            return;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("st-wgpu-video.output"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OUTPUT_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.output = Some(OutputSurface {
            texture,
            view,
            width,
            height,
        });
        // Output view identity changed → consumers must re-register with egui.
    }

    fn upload_and_render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        video: &VideoFrameBuffer,
    ) -> Result<(), String> {
        let (luma_fmt, chroma_fmt, chroma_v_fmt, plane1_bpp, plane2_bpp, mode) =
            plane_layout(video.format)?;

        let chroma_w = video.chroma_width();
        let chroma_h = video.chroma_height();

        let need_realloc = match &self.planes {
            Some(p) => {
                p.format != video.format
                    || p.luma.width != video.width
                    || p.luma.height != video.height
                    || p.chroma.width != chroma_w
                    || p.chroma.height != chroma_h
                    || p.luma.format != luma_fmt
                    || p.chroma.format != chroma_fmt
                    || p.chroma_v.format != chroma_v_fmt
            }
            None => true,
        };

        if need_realloc {
            let luma = PlaneTexture::new(device, luma_fmt, video.width, video.height, "luma");
            let chroma = PlaneTexture::new(device, chroma_fmt, chroma_w, chroma_h, "chroma");
            // For NV12 and RGBA we still allocate a 1x1 placeholder for the
            // chroma_v binding so the bind group has stable arity. The shader
            // branches on `mode` and does not sample it in those modes.
            let chroma_v = PlaneTexture::new(device, chroma_v_fmt, chroma_w, chroma_h, "chroma_v");
            self.planes = Some(Planes {
                luma,
                chroma,
                chroma_v,
                format: video.format,
            });
            self.bind_group = None;
        }

        let planes = self.planes.as_ref().expect("planes initialized");

        match video.format {
            VideoFormat::Rgba8 => {
                let expected = (video.width as usize)
                    .checked_mul(video.height as usize)
                    .and_then(|px| px.checked_mul(4))
                    .ok_or_else(|| "rgba size overflow".to_string())?;
                if video.plane0.len() < expected {
                    return Err(format!(
                        "RGBA frame too small: {} < {}",
                        video.plane0.len(),
                        expected
                    ));
                }
                upload_plane(
                    queue,
                    &planes.luma,
                    &video.plane0[..expected],
                    video.width * 4,
                )?;
            }
            VideoFormat::Yuv420p8 | VideoFormat::Yuv444p8 => {
                upload_plane(queue, &planes.luma, &video.plane0, video.width)?;
                upload_plane(queue, &planes.chroma, &video.plane1, chroma_w)?;
                upload_plane(queue, &planes.chroma_v, &video.plane2, chroma_w)?;
            }
            VideoFormat::Nv12 => {
                upload_plane(queue, &planes.luma, &video.plane0, video.width)?;
                upload_plane(queue, &planes.chroma, &video.plane1, chroma_w * 2)?;
            }
        }

        let _ = (plane1_bpp, plane2_bpp);

        if mode != self.last_mode {
            let bytes = [
                mode.to_le_bytes()[0],
                mode.to_le_bytes()[1],
                mode.to_le_bytes()[2],
                mode.to_le_bytes()[3],
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ];
            queue.write_buffer(&self.uniform_buf, 0, &bytes);
            self.last_mode = mode;
        }

        if self.bind_group.is_none() {
            self.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("st-wgpu-video.bg"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&planes.luma.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&planes.chroma.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&planes.chroma_v.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: self.uniform_buf.as_entire_binding(),
                    },
                ],
            }));
        }

        let output = self
            .output
            .as_ref()
            .ok_or_else(|| "output surface missing".to_string())?;
        let bind_group = self
            .bind_group
            .as_ref()
            .ok_or_else(|| "bind group missing".to_string())?;

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("st-wgpu-video.encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("st-wgpu-video.pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &output.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
        queue.submit(std::iter::once(encoder.finish()));
        Ok(())
    }

    /// DMA-BUF fast path: import each plane as a `wgpu::Texture` backed by
    /// the caller's DMA-BUF FDs (no CPU copy), build a per-frame bind group,
    /// and run the standard render pass into the output surface. Falls back
    /// by returning Err — caller switches to CPU upload.
    #[cfg(target_os = "linux")]
    fn render_from_dmabuf(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &crate::video_frame::LinuxDmaBufFrame,
    ) -> Result<(), String> {
        let mode: u32 = match frame.format {
            LinuxDmaBufFormat::Yuv420p8 | LinuxDmaBufFormat::Yuv444p8 => 0,
            LinuxDmaBufFormat::Nv12 => 1,
        };
        let imported = crate::render_wgpu_linux_dmabuf::import_frame(device, frame)?;

        // For NV12 the shader does not sample slot 2. Bind the chroma view
        // there so the bind group has stable arity regardless of format —
        // matches the CPU path's placeholder `chroma_v` approach.
        let (luma_view, chroma_view, chroma_v_view) = match frame.format {
            LinuxDmaBufFormat::Nv12 => (&imported[0].view, &imported[1].view, &imported[1].view),
            _ => (&imported[0].view, &imported[1].view, &imported[2].view),
        };

        if mode != self.last_mode {
            let mut bytes = [0u8; 16];
            bytes[..4].copy_from_slice(&mode.to_le_bytes());
            queue.write_buffer(&self.uniform_buf, 0, &bytes);
            self.last_mode = mode;
        }

        // Imported textures are per-frame; a fresh bind group each time is
        // cheap (just references into existing views + sampler + uniform).
        let dmabuf_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("st-wgpu-video.bg.dmabuf"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(luma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(chroma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(chroma_v_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.uniform_buf.as_entire_binding(),
                },
            ],
        });

        let output = self
            .output
            .as_ref()
            .ok_or_else(|| "output surface missing".to_string())?;

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("st-wgpu-video.encoder.dmabuf"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("st-wgpu-video.pass.dmabuf"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &output.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &dmabuf_bg, &[]);
            pass.draw(0..4, 0..1);
        }
        queue.submit(std::iter::once(encoder.finish()));

        // Invalidate any cached CPU bind group — the next CPU frame will
        // rebuild it. Imported plane textures (and their VkImage/VkMemory)
        // are freed when `imported` goes out of scope via the drop callback.
        self.bind_group = None;
        drop(imported);
        Ok(())
    }
}

impl PlaneTexture {
    fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        tag: &str,
    ) -> Self {
        let label = format!("st-wgpu-video.plane.{tag}");
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(&label),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            width,
            height,
            format,
        }
    }
}

fn upload_plane(
    queue: &wgpu::Queue,
    plane: &PlaneTexture,
    data: &[u8],
    bytes_per_row: u32,
) -> Result<(), String> {
    let expected = (bytes_per_row as usize)
        .checked_mul(plane.height as usize)
        .ok_or_else(|| "plane size overflow".to_string())?;
    if data.len() < expected {
        return Err(format!(
            "plane buffer too small: {} < {}",
            data.len(),
            expected
        ));
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &plane.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &data[..expected],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(plane.height),
        },
        wgpu::Extent3d {
            width: plane.width,
            height: plane.height,
            depth_or_array_layers: 1,
        },
    );
    Ok(())
}

/// Returns (luma_fmt, chroma_fmt, chroma_v_fmt, plane1_bpp, plane2_bpp, mode).
/// `mode` is the uniform sent to the fragment shader:
///   0 = planar YUV (sample U from `chroma`, V from `chroma_v`)
///   1 = NV12 (sample UV from `chroma.rg`)
///   2 = pre-converted RGBA (passthrough, sample luma.rgba)
fn plane_layout(
    format: VideoFormat,
) -> Result<
    (
        wgpu::TextureFormat,
        wgpu::TextureFormat,
        wgpu::TextureFormat,
        u32,
        u32,
        u32,
    ),
    String,
> {
    Ok(match format {
        VideoFormat::Yuv420p8 | VideoFormat::Yuv444p8 => (
            wgpu::TextureFormat::R8Unorm,
            wgpu::TextureFormat::R8Unorm,
            wgpu::TextureFormat::R8Unorm,
            1,
            1,
            0,
        ),
        VideoFormat::Nv12 => (
            wgpu::TextureFormat::R8Unorm,
            wgpu::TextureFormat::Rg8Unorm,
            wgpu::TextureFormat::R8Unorm,
            2,
            1,
            1,
        ),
        VideoFormat::Rgba8 => (
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::R8Unorm,
            wgpu::TextureFormat::R8Unorm,
            1,
            1,
            2,
        ),
    })
}

const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

const VIDEO_WGSL: &str = r#"
@group(0) @binding(0) var t_luma: texture_2d<f32>;
@group(0) @binding(1) var t_chroma: texture_2d<f32>;
@group(0) @binding(2) var t_chroma_v: texture_2d<f32>;
@group(0) @binding(3) var s: sampler;
struct Params {
    mode: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};
@group(0) @binding(4) var<uniform> params: Params;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Fullscreen triangle strip: 4 verts covering clip-space [-1,1]^2.
    //   vi=0: (-1, -1)  uv (0, 1)
    //   vi=1: ( 1, -1)  uv (1, 1)
    //   vi=2: (-1,  1)  uv (0, 0)
    //   vi=3: ( 1,  1)  uv (1, 0)
    let x = f32((vi & 1u)) * 2.0 - 1.0;
    let y = f32(((vi >> 1u) & 1u)) * 2.0 - 1.0;
    let u = f32((vi & 1u));
    let v = 1.0 - f32(((vi >> 1u) & 1u));
    var out: VsOut;
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>(u, v);
    return out;
}

fn bt709_limited_to_rgb(y: f32, u: f32, v: f32) -> vec3<f32> {
    let yy = max(y - 0.0625, 0.0) * 1.16438356;
    return vec3<f32>(
        yy + 1.79274107 * v,
        yy - 0.21324861 * u - 0.53290933 * v,
        yy + 2.11240179 * u
    );
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    if (params.mode == 2u) {
        return textureSample(t_luma, s, in.uv);
    }
    let y = textureSample(t_luma, s, in.uv).r;
    var u: f32;
    var v: f32;
    if (params.mode == 0u) {
        u = textureSample(t_chroma, s, in.uv).r - 0.5;
        v = textureSample(t_chroma_v, s, in.uv).r - 0.5;
    } else {
        let uv = textureSample(t_chroma, s, in.uv).rg - vec2<f32>(0.5, 0.5);
        u = uv.x;
        v = uv.y;
    }
    let rgb = clamp(bt709_limited_to_rgb(y, u, v), vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(rgb, 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_frame::VideoFrameBuffer;

    /// Boot a real wgpu device for testing. Returns `None` if no adapter is
    /// available (e.g. headless CI without a GPU), so the test becomes a no-op
    /// instead of flaking.
    fn try_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY | wgpu::Backends::GL,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok()?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("st-wgpu-video.test-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::default(),
        }))
        .ok()?;
        Some((device, queue))
    }

    /// Copy the current output texture to a CPU-mappable buffer and return
    /// its RGBA8 pixel bytes with row padding stripped.
    fn readback_output(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipeline: &PipelineState,
    ) -> Vec<u8> {
        let out = pipeline.output.as_ref().expect("output surface");
        let width = out.width;
        let height = out.height;
        let bytes_per_row = (width * 4).div_ceil(256) * 256;
        let buffer_size = (bytes_per_row as u64) * (height as u64);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("st-wgpu-video.test-readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("st-wgpu-video.test-copy"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &out.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv().expect("map result").expect("map ok");
        let data = slice.get_mapped_range().to_vec();
        let row_bytes = (width * 4) as usize;
        let mut out_bytes = Vec::with_capacity(row_bytes * height as usize);
        for y in 0..height as usize {
            let start = y * bytes_per_row as usize;
            out_bytes.extend_from_slice(&data[start..start + row_bytes]);
        }
        out_bytes
    }

    fn sample_center(pixels: &[u8], width: u32, height: u32) -> (u8, u8, u8, u8) {
        let cx = (width / 2) as usize;
        let cy = (height / 2) as usize;
        let i = (cy * width as usize + cx) * 4;
        (pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3])
    }

    /// Smoke-test: neutral Y=128, U=V=128 YUV420p frame should render to
    /// approximately mid-gray RGB (~130,130,130) via the WGSL BT.709
    /// limited-range shader. Catches WGSL compile failures, bind-group
    /// layout mismatches, and texture format regressions.
    #[test]
    fn wgpu_yuv420_renders_neutral_gray() {
        let Some((device, queue)) = try_device() else {
            eprintln!("no wgpu adapter available; skipping");
            return;
        };
        let width = 64u32;
        let height = 64u32;
        let chroma_w = width / 2;
        let chroma_h = height / 2;
        let mut video = VideoFrameBuffer::default();
        video.width = width;
        video.height = height;
        video.format = VideoFormat::Yuv420p8;
        video.plane0 = vec![128u8; (width * height) as usize];
        video.plane1 = vec![128u8; (chroma_w * chroma_h) as usize];
        video.plane2 = vec![128u8; (chroma_w * chroma_h) as usize];

        let mut pipeline = PipelineState::new(&device, &queue).expect("pipeline");
        pipeline.ensure_output(&device, width, height);
        pipeline
            .upload_and_render(&device, &queue, &video)
            .expect("upload_and_render");
        let pixels = readback_output(&device, &queue, &pipeline);
        let (r, g, b, a) = sample_center(&pixels, width, height);
        assert_eq!(a, 255, "alpha must be opaque");
        for (label, channel) in [("r", r), ("g", g), ("b", b)] {
            assert!(
                (channel as i32 - 130).abs() <= 6,
                "{label}={channel} out of expected range around 130"
            );
        }
    }

    /// NV12 neutral frame (Y=128, UV=128,128) must also resolve to near-gray.
    /// Exercises the `mode=1` shader branch and the Rg8Unorm chroma binding.
    #[test]
    fn wgpu_nv12_renders_neutral_gray() {
        let Some((device, queue)) = try_device() else {
            return;
        };
        let width = 64u32;
        let height = 64u32;
        let chroma_w = width / 2;
        let chroma_h = height / 2;
        let mut video = VideoFrameBuffer::default();
        video.width = width;
        video.height = height;
        video.format = VideoFormat::Nv12;
        video.plane0 = vec![128u8; (width * height) as usize];
        video.plane1 = vec![128u8; (chroma_w * chroma_h * 2) as usize];

        let mut pipeline = PipelineState::new(&device, &queue).expect("pipeline");
        pipeline.ensure_output(&device, width, height);
        pipeline
            .upload_and_render(&device, &queue, &video)
            .expect("upload_and_render nv12");
        let pixels = readback_output(&device, &queue, &pipeline);
        let (r, g, b, a) = sample_center(&pixels, width, height);
        assert_eq!(a, 255);
        for (label, channel) in [("r", r), ("g", g), ("b", b)] {
            assert!(
                (channel as i32 - 130).abs() <= 6,
                "nv12 {label}={channel} out of expected range around 130"
            );
        }
    }

    /// YUV444p neutral frame — same chroma resolution as luma. Catches
    /// regressions where the chroma-subsampling math would pass for 4:2:0
    /// but alloc the wrong texture size for 4:4:4.
    #[test]
    fn wgpu_yuv444_renders_neutral_gray() {
        let Some((device, queue)) = try_device() else {
            return;
        };
        let width = 64u32;
        let height = 64u32;
        let mut video = VideoFrameBuffer::default();
        video.width = width;
        video.height = height;
        video.format = VideoFormat::Yuv444p8;
        video.plane0 = vec![128u8; (width * height) as usize];
        video.plane1 = vec![128u8; (width * height) as usize];
        video.plane2 = vec![128u8; (width * height) as usize];

        let mut pipeline = PipelineState::new(&device, &queue).expect("pipeline");
        pipeline.ensure_output(&device, width, height);
        pipeline
            .upload_and_render(&device, &queue, &video)
            .expect("upload_and_render yuv444");
        let pixels = readback_output(&device, &queue, &pipeline);
        let (r, g, b, a) = sample_center(&pixels, width, height);
        assert_eq!(a, 255);
        for (label, channel) in [("r", r), ("g", g), ("b", b)] {
            assert!(
                (channel as i32 - 130).abs() <= 6,
                "yuv444 {label}={channel} out of expected range around 130"
            );
        }
    }

    /// RGBA8 mode (mode=2) must pass the source pixels through without any
    /// BT.709 conversion. A solid red input must come out as solid red.
    #[test]
    fn wgpu_rgba_passthrough_preserves_red() {
        let Some((device, queue)) = try_device() else {
            return;
        };
        let width = 32u32;
        let height = 32u32;
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            rgba.extend_from_slice(&[255, 0, 0, 255]);
        }
        let mut video = VideoFrameBuffer::default();
        video.width = width;
        video.height = height;
        video.format = VideoFormat::Rgba8;
        video.plane0 = rgba;

        let mut pipeline = PipelineState::new(&device, &queue).expect("pipeline");
        pipeline.ensure_output(&device, width, height);
        pipeline
            .upload_and_render(&device, &queue, &video)
            .expect("upload_and_render rgba");
        let pixels = readback_output(&device, &queue, &pipeline);
        let (r, g, b, a) = sample_center(&pixels, width, height);
        assert_eq!(a, 255);
        assert!(r >= 250, "rgba red passthrough: r={r} (expected ~255)");
        assert!(g <= 5, "rgba red passthrough: g={g} (expected ~0)");
        assert!(b <= 5, "rgba red passthrough: b={b} (expected ~0)");
    }

    /// Render a YUV420p frame, then resize to a different resolution and
    /// render again. Verifies that plane textures + output texture are
    /// correctly reallocated and that leftover bind groups from the first
    /// size don't get reused. Catches bugs where stale bind groups point
    /// at freed textures.
    #[test]
    fn wgpu_resize_handles_new_dimensions() {
        let Some((device, queue)) = try_device() else {
            return;
        };
        let mut pipeline = PipelineState::new(&device, &queue).expect("pipeline");

        // First render at 64x64.
        {
            let (w, h) = (64u32, 64u32);
            let (cw, ch) = (w / 2, h / 2);
            let mut video = VideoFrameBuffer::default();
            video.width = w;
            video.height = h;
            video.format = VideoFormat::Yuv420p8;
            video.plane0 = vec![128u8; (w * h) as usize];
            video.plane1 = vec![128u8; (cw * ch) as usize];
            video.plane2 = vec![128u8; (cw * ch) as usize];
            pipeline.ensure_output(&device, w, h);
            pipeline
                .upload_and_render(&device, &queue, &video)
                .expect("first render");
            let pixels = readback_output(&device, &queue, &pipeline);
            let (r, _, _, _) = sample_center(&pixels, w, h);
            assert!((r as i32 - 130).abs() <= 6);
        }

        // Now resize to 128x96 and render. If any cached state points at the
        // old textures, this will crash or produce wrong output.
        {
            let (w, h) = (128u32, 96u32);
            let (cw, ch) = (w / 2, h / 2);
            let mut video = VideoFrameBuffer::default();
            video.width = w;
            video.height = h;
            video.format = VideoFormat::Yuv420p8;
            video.plane0 = vec![128u8; (w * h) as usize];
            video.plane1 = vec![128u8; (cw * ch) as usize];
            video.plane2 = vec![128u8; (cw * ch) as usize];
            pipeline.ensure_output(&device, w, h);
            pipeline
                .upload_and_render(&device, &queue, &video)
                .expect("resized render");
            let pixels = readback_output(&device, &queue, &pipeline);
            let (r, g, b, _) = sample_center(&pixels, w, h);
            for (label, channel) in [("r", r), ("g", g), ("b", b)] {
                assert!(
                    (channel as i32 - 130).abs() <= 6,
                    "resize {label}={channel} out of expected range around 130"
                );
            }
        }
    }

    /// BT.709 limited-range red in YUV is approximately (63, 102, 240). The
    /// shader must decode that back to mostly-red (R dominant, G and B near
    /// zero). Catches coefficient transpositions or U/V swaps that the
    /// neutral-gray test alone cannot detect.
    #[test]
    fn wgpu_yuv420_red_decodes_to_dominant_red() {
        let Some((device, queue)) = try_device() else {
            return;
        };
        let width = 64u32;
        let height = 64u32;
        let cw = width / 2;
        let ch = height / 2;
        let mut video = VideoFrameBuffer::default();
        video.width = width;
        video.height = height;
        video.format = VideoFormat::Yuv420p8;
        video.plane0 = vec![63u8; (width * height) as usize];
        video.plane1 = vec![102u8; (cw * ch) as usize];
        video.plane2 = vec![240u8; (cw * ch) as usize];

        let mut pipeline = PipelineState::new(&device, &queue).expect("pipeline");
        pipeline.ensure_output(&device, width, height);
        pipeline
            .upload_and_render(&device, &queue, &video)
            .expect("upload_and_render red");
        let pixels = readback_output(&device, &queue, &pipeline);
        let (r, g, b, a) = sample_center(&pixels, width, height);
        assert_eq!(a, 255);
        assert!(
            r > 200 && g < 20 && b < 20,
            "expected dominant red, got ({r},{g},{b})"
        );
    }
}
