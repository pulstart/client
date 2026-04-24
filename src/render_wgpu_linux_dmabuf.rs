//! Linux DMA-BUF → wgpu::Texture zero-copy import on the Vulkan backend.
//!
//! **Auto-enables inside the wgpu renderer when `VK_KHR_external_memory_fd` +
//! `VK_EXT_external_memory_dma_buf` are present on the physical device.
//! `ST_WGPU_DMABUF=0` is the escape hatch to force-disable.** Validated live
//! against PipeWire DMA-BUF + VAAPI decoder output on 2026-04-23; the path
//! produced correct video frames with the Linear-modifier import code below.
//! The surrounding wgpu path itself is still opt-in via `ST_RENDERER=wgpu`
//! because macOS/Windows zero-copy import and tiled-modifier DMA-BUF are not
//! yet implemented — but within the wgpu path on Linux, this follows the
//! standard "probe + use fast path + fall back on failure" pattern.
//!
//! Linear-modifier only (`DRM_FORMAT_MOD_LINEAR` / `DRM_FORMAT_MOD_INVALID`).
//! This matches the Phase 2.2 server default — advertising tiled modifiers
//! caused horizontal striping because the consumer (EGL) didn't honor them.
//! Here we'd have the same issue: `VK_EXT_image_drm_format_modifier` is not
//! auto-enabled by wgpu-hal, so we can't build a modifier-aware import until
//! a custom `WgpuSetup::Existing` path adds the extension.
//!
//! wgpu-hal 27 *does* auto-enable `VK_KHR_external_memory_fd` and
//! `VK_EXT_external_memory_dma_buf` when the physical device supports them
//! (see `wgpu-hal/src/vulkan/adapter.rs:1105`). So linear-modifier DMA-BUF
//! import works against the default wgpu device with no setup override.

use crate::video_frame::{LinuxDmaBufFormat, LinuxDmaBufFrame, LinuxDmaBufPlane};
use ash::{ext, khr, vk};
use eframe::wgpu::{self, hal};
use std::os::fd::AsRawFd;

const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Returns false only when the user force-disables via `ST_WGPU_DMABUF=0`
/// (or similar). Default is true — any Vulkan device with the required
/// extensions will take the zero-copy path.
pub fn dmabuf_requested() -> bool {
    match std::env::var("ST_WGPU_DMABUF") {
        Ok(v) => !matches!(v.as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DmaBufCapabilities {
    pub external_memory_fd: bool,
    pub external_memory_dma_buf: bool,
    pub is_vulkan: bool,
}

impl DmaBufCapabilities {
    pub fn is_supported(&self) -> bool {
        self.is_vulkan && self.external_memory_fd && self.external_memory_dma_buf
    }
}

/// Introspects the active wgpu device. Returns all-false if the backend isn't
/// Vulkan (e.g. the GL fallback was picked), or if the required extensions
/// aren't enabled on this physical device.
pub fn probe(device: &wgpu::Device) -> DmaBufCapabilities {
    let mut caps = DmaBufCapabilities::default();
    unsafe {
        let hal_dev = match device.as_hal::<hal::api::Vulkan>() {
            Some(d) => d,
            None => return caps,
        };
        caps.is_vulkan = true;
        let instance = hal_dev.shared_instance();
        let ash_inst = instance.raw_instance();
        let phys = hal_dev.raw_physical_device();
        let exts = match ash_inst.enumerate_device_extension_properties(phys) {
            Ok(v) => v,
            Err(_) => return caps,
        };
        for ext in &exts {
            let Ok(name) = ext.extension_name_as_c_str() else {
                continue;
            };
            if name == khr::external_memory_fd::NAME {
                caps.external_memory_fd = true;
            }
            if name == ext::external_memory_dma_buf::NAME {
                caps.external_memory_dma_buf = true;
            }
        }
    }
    caps
}

/// Linear-only check. Anything else means the compositor/server chose a tiled
/// modifier we can't import without `VK_EXT_image_drm_format_modifier`.
fn modifier_is_linear(modifier: u64) -> bool {
    modifier == DRM_FORMAT_MOD_LINEAR || modifier == DRM_FORMAT_MOD_INVALID
}

fn plane_vk_format(plane_index: usize, format: LinuxDmaBufFormat) -> Option<vk::Format> {
    match format {
        LinuxDmaBufFormat::Yuv420p8 | LinuxDmaBufFormat::Yuv444p8 => Some(vk::Format::R8_UNORM),
        LinuxDmaBufFormat::Nv12 => match plane_index {
            0 => Some(vk::Format::R8_UNORM),
            1 => Some(vk::Format::R8G8_UNORM),
            _ => None,
        },
    }
}

fn plane_wgpu_format(plane_index: usize, format: LinuxDmaBufFormat) -> wgpu::TextureFormat {
    match format {
        LinuxDmaBufFormat::Yuv420p8 | LinuxDmaBufFormat::Yuv444p8 => wgpu::TextureFormat::R8Unorm,
        LinuxDmaBufFormat::Nv12 => match plane_index {
            0 => wgpu::TextureFormat::R8Unorm,
            _ => wgpu::TextureFormat::Rg8Unorm,
        },
    }
}

/// Output of a successful plane import. The texture's lifetime is the wgpu
/// resource; when dropped, wgpu runs the drop callback which destroys the
/// VkImage and frees the imported VkDeviceMemory (which closes the duplicated
/// DMA-BUF FD that Vulkan took ownership of).
pub struct ImportedPlane {
    /// Retained to keep the imported VkImage + VkDeviceMemory alive. Drops
    /// fire the hal DropCallback which destroys both.
    #[allow(dead_code)]
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
    #[allow(dead_code)]
    pub format: wgpu::TextureFormat,
}

pub fn import_frame(
    device: &wgpu::Device,
    frame: &LinuxDmaBufFrame,
) -> Result<Vec<ImportedPlane>, String> {
    let expected_planes = match frame.format {
        LinuxDmaBufFormat::Yuv420p8 | LinuxDmaBufFormat::Yuv444p8 => 3,
        LinuxDmaBufFormat::Nv12 => 2,
    };
    if frame.planes.len() < expected_planes {
        return Err(format!(
            "dmabuf frame has {} planes, need {expected_planes}",
            frame.planes.len()
        ));
    }
    let mut out = Vec::with_capacity(expected_planes);
    for (idx, plane) in frame.planes.iter().enumerate().take(expected_planes) {
        out.push(import_plane(device, idx, plane, frame.format)?);
    }
    Ok(out)
}

fn import_plane(
    device: &wgpu::Device,
    plane_index: usize,
    plane: &LinuxDmaBufPlane,
    format: LinuxDmaBufFormat,
) -> Result<ImportedPlane, String> {
    if !modifier_is_linear(plane.modifier) {
        return Err(format!(
            "dmabuf modifier 0x{:016x} is not LINEAR; linear-only import is the only supported path",
            plane.modifier
        ));
    }
    let vk_fmt = plane_vk_format(plane_index, format)
        .ok_or_else(|| format!("no Vulkan format for plane {plane_index} ({format:?})"))?;
    let wgpu_fmt = plane_wgpu_format(plane_index, format);

    // SAFETY: we only reach here when `probe()` reported the Vulkan backend
    // with both external_memory extensions present. All handles are freed
    // via the drop_callback on any success, and via explicit cleanup on
    // every error path before we lose ownership.
    unsafe {
        let hal_dev = device
            .as_hal::<hal::api::Vulkan>()
            .ok_or_else(|| "wgpu device is not Vulkan".to_string())?;
        let ash_inst = hal_dev.shared_instance().raw_instance();
        let ash_dev = hal_dev.raw_device().clone();

        let ext_mem_fd = khr::external_memory_fd::Device::new(ash_inst, &ash_dev);

        // Duplicate the FD. vkAllocateMemory takes ownership of whatever FD
        // it imports, so we must not hand it the plane's own FD — that would
        // race with LinuxDmaBufFrame::drop closing the same number.
        let dup_fd = libc::fcntl(plane.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0);
        if dup_fd < 0 {
            return Err(format!(
                "dup dmabuf fd: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_fmt)
            .extent(vk::Extent3D {
                width: plane.width,
                height: plane.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext_info);

        let image = match ash_dev.create_image(&image_info, None) {
            Ok(img) => img,
            Err(e) => {
                libc::close(dup_fd);
                return Err(format!("vkCreateImage: {e:?}"));
            }
        };

        let mem_req = ash_dev.get_image_memory_requirements(image);

        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        if let Err(e) = ext_mem_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            dup_fd,
            &mut fd_props,
        ) {
            ash_dev.destroy_image(image, None);
            libc::close(dup_fd);
            return Err(format!("vkGetMemoryFdPropertiesKHR: {e:?}"));
        }

        let combined = mem_req.memory_type_bits & fd_props.memory_type_bits;
        if combined == 0 {
            ash_dev.destroy_image(image, None);
            libc::close(dup_fd);
            return Err("no compatible memory type for dmabuf import".into());
        }
        let mem_type_index = combined.trailing_zeros();

        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup_fd);

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_req.size)
            .memory_type_index(mem_type_index)
            .push_next(&mut dedicated)
            .push_next(&mut import);

        let memory = match ash_dev.allocate_memory(&alloc_info, None) {
            Ok(m) => m,
            Err(e) => {
                ash_dev.destroy_image(image, None);
                libc::close(dup_fd);
                return Err(format!("vkAllocateMemory: {e:?}"));
            }
        };
        // From here on Vulkan owns the duplicated FD; do not close it.

        if let Err(e) = ash_dev.bind_image_memory(image, memory, 0) {
            ash_dev.free_memory(memory, None);
            ash_dev.destroy_image(image, None);
            return Err(format!("vkBindImageMemory: {e:?}"));
        }

        let hal_desc = hal::TextureDescriptor {
            label: Some("st-wgpu.dmabuf-plane"),
            size: wgpu::Extent3d {
                width: plane.width,
                height: plane.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu_fmt,
            usage: wgpu::wgt::TextureUses::RESOURCE,
            memory_flags: hal::MemoryFlags::empty(),
            view_formats: vec![],
        };

        let ash_dev_for_drop = ash_dev.clone();
        let drop_cb: hal::DropCallback = Box::new(move || {
            ash_dev_for_drop.destroy_image(image, None);
            ash_dev_for_drop.free_memory(memory, None);
        });

        let hal_texture = hal_dev.texture_from_raw(image, &hal_desc, Some(drop_cb));
        // Drop the hal_dev guard before touching `device` again.
        drop(hal_dev);

        let wgpu_desc = wgpu::TextureDescriptor {
            label: Some("st-wgpu.dmabuf-plane"),
            size: wgpu::Extent3d {
                width: plane.width,
                height: plane.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu_fmt,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };

        let texture = device.create_texture_from_hal::<hal::api::Vulkan>(hal_texture, &wgpu_desc);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Ok(ImportedPlane {
            texture,
            view,
            width: plane.width,
            height: plane.height,
            format: wgpu_fmt,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmabuf_requested_respects_env() {
        // Default-on: env unset → true. ST_WGPU_DMABUF=0 → false.
        // SAFETY: test is isolated and restores state.
        let prev = std::env::var("ST_WGPU_DMABUF").ok();
        unsafe {
            std::env::remove_var("ST_WGPU_DMABUF");
        }
        assert!(dmabuf_requested(), "default should auto-enable");
        unsafe {
            std::env::set_var("ST_WGPU_DMABUF", "1");
        }
        assert!(dmabuf_requested());
        unsafe {
            std::env::set_var("ST_WGPU_DMABUF", "0");
        }
        assert!(!dmabuf_requested(), "=0 must force-disable");
        unsafe {
            std::env::set_var("ST_WGPU_DMABUF", "off");
        }
        assert!(!dmabuf_requested());
        match prev {
            Some(v) => unsafe { std::env::set_var("ST_WGPU_DMABUF", v) },
            None => unsafe { std::env::remove_var("ST_WGPU_DMABUF") },
        }
    }

    #[test]
    fn modifier_linear_check() {
        assert!(modifier_is_linear(DRM_FORMAT_MOD_LINEAR));
        assert!(modifier_is_linear(DRM_FORMAT_MOD_INVALID));
        assert!(!modifier_is_linear(0x0100_0000_0000_0001)); // synthetic tiled
    }

    #[test]
    fn plane_format_mapping() {
        assert_eq!(
            plane_vk_format(0, LinuxDmaBufFormat::Yuv420p8),
            Some(vk::Format::R8_UNORM)
        );
        assert_eq!(
            plane_vk_format(2, LinuxDmaBufFormat::Yuv420p8),
            Some(vk::Format::R8_UNORM)
        );
        assert_eq!(
            plane_vk_format(0, LinuxDmaBufFormat::Nv12),
            Some(vk::Format::R8_UNORM)
        );
        assert_eq!(
            plane_vk_format(1, LinuxDmaBufFormat::Nv12),
            Some(vk::Format::R8G8_UNORM)
        );
        assert_eq!(plane_vk_format(2, LinuxDmaBufFormat::Nv12), None);
    }
}
