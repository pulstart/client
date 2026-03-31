use crate::video_frame::WindowsD3d11Frame;
use eframe::{egui, Frame as EframeFrame};
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use std::ffi::c_void;
use windows::core::{w, Interface};
use windows::Win32::Foundation::{HINSTANCE, HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Multithread, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIFactory1, IDXGIFactory2, IDXGIOutput, IDXGISwapChain1,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetWindowLongW, SetWindowLongW, SetWindowPos, ShowWindow,
    HMENU, SHOW_WINDOW_CMD, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER,
    SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNA, WINDOW_EX_STYLE,
    WINDOW_LONG_PTR_INDEX, WS_CHILD, WS_CLIPCHILDREN, WS_DISABLED, WS_VISIBLE,
};

pub struct WindowsNativeVideoPresenter {
    enabled: bool,
    logged_success: bool,
    preferred: bool,
    frame: Option<WindowsD3d11Frame>,
    staged_serial: u64,
    rendered_serial: u64,
    renderer: Option<WindowsSwapchainRenderer>,
}

impl WindowsNativeVideoPresenter {
    pub fn new() -> Self {
        Self {
            enabled: true,
            logged_success: false,
            preferred: false,
            frame: None,
            staged_serial: 0,
            rendered_serial: 0,
            renderer: None,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_preferred(&mut self, preferred: bool) {
        self.preferred = preferred;
        if !preferred {
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.hide();
            }
        }
    }

    pub fn occludes_egui_overlay(&self) -> bool {
        self.enabled && self.preferred
    }

    pub fn has_frame(&self) -> bool {
        self.enabled && self.preferred && self.frame.is_some()
    }

    pub fn clear(&mut self) {
        self.frame = None;
        self.staged_serial = 0;
        self.rendered_serial = 0;
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.hide();
        }
    }

    pub fn stage_frame(&mut self, frame: &WindowsD3d11Frame) -> bool {
        if !(self.enabled && self.preferred) {
            return false;
        }

        self.frame = Some(frame.clone());
        self.staged_serial = self.staged_serial.wrapping_add(1);
        true
    }

    pub fn present(
        &mut self,
        frame: &EframeFrame,
        rect: egui::Rect,
        pixels_per_point: f32,
    ) -> bool {
        if !(self.enabled && self.preferred) {
            return false;
        }
        let Some(video_frame) = self.frame.as_ref() else {
            return false;
        };

        let render_result = (|| -> Result<(), String> {
            if self.renderer.is_none() {
                self.renderer = Some(WindowsSwapchainRenderer::new(frame)?);
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
                    eprintln!("[render] Windows native swapchain present path enabled");
                    self.logged_success = true;
                }
                true
            }
            Err(err) => {
                eprintln!("[render] disabling Windows native present path: {err}");
                self.disable();
                false
            }
        }
    }

    pub fn current_rect(&self) -> Option<egui::Rect> {
        self.renderer
            .as_ref()
            .and_then(WindowsSwapchainRenderer::current_rect)
    }

    fn disable(&mut self) {
        self.enabled = false;
        self.logged_success = false;
        self.preferred = false;
        self.frame = None;
        self.renderer = None;
    }
}

struct WindowsSwapchainRenderer {
    parent_hwnd: HWND,
    video_hwnd: HWND,
    parent_had_clipchildren: bool,
    clipchildren_applied: bool,
    device_key: usize,
    decoder_size: (u32, u32),
    output_size: (u32, u32),
    factory: IDXGIFactory2,
    swap_chain: Option<IDXGISwapChain1>,
    device: Option<ID3D11Device>,
    video_device: Option<ID3D11VideoDevice>,
    video_context: Option<ID3D11VideoContext>,
    processor_enum: Option<ID3D11VideoProcessorEnumerator>,
    processor: Option<ID3D11VideoProcessor>,
    output_view: Option<ID3D11VideoProcessorOutputView>,
    last_rect: Option<egui::Rect>,
    last_scale: f32,
}

impl WindowsSwapchainRenderer {
    fn new(frame: &EframeFrame) -> Result<Self, String> {
        let parent_hwnd = root_hwnd(frame)?;
        let parent_style = unsafe { GetWindowLongW(parent_hwnd, WINDOW_LONG_PTR_INDEX(-16)) };
        let parent_had_clipchildren = (parent_style & WS_CLIPCHILDREN.0 as i32) != 0;
        let video_hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                None,
                WS_CHILD | WS_DISABLED | WS_VISIBLE,
                0,
                0,
                0,
                0,
                Some(parent_hwnd),
                Some(HMENU(std::ptr::null_mut())),
                Some(HINSTANCE(std::ptr::null_mut())),
                None,
            )
            .map_err(|err| format!("CreateWindowExW(STATIC) failed: {err}"))?
        };
        unsafe {
            let _ = ShowWindow(video_hwnd, SHOW_WINDOW_CMD(SW_HIDE.0));
        }

        let factory1: IDXGIFactory1 =
            unsafe { CreateDXGIFactory1().map_err(|err| format!("CreateDXGIFactory1 failed: {err}"))? };
        let factory = factory1
            .cast::<IDXGIFactory2>()
            .map_err(|err| format!("IDXGIFactory1->IDXGIFactory2 cast failed: {err}"))?;

        Ok(Self {
            parent_hwnd,
            video_hwnd,
            parent_had_clipchildren,
            clipchildren_applied: false,
            device_key: 0,
            decoder_size: (0, 0),
            output_size: (0, 0),
            factory,
            swap_chain: None,
            device: None,
            video_device: None,
            video_context: None,
            processor_enum: None,
            processor: None,
            output_view: None,
            last_rect: None,
            last_scale: 0.0,
        })
    }

    fn present(
        &mut self,
        frame: &WindowsD3d11Frame,
        rect: egui::Rect,
        pixels_per_point: f32,
        frame_dirty: bool,
    ) -> Result<(), String> {
        let visible = rect.width() >= 1.0 && rect.height() >= 1.0;
        if !visible {
            self.hide();
            return Ok(());
        }

        let layout_changed = self.update_host_window(rect, pixels_per_point)?;
        let output_size = pixel_size(rect, pixels_per_point);
        if output_size.0 == 0 || output_size.1 == 0 {
            self.hide();
            return Ok(());
        }

        self.ensure_render_state(frame, output_size)?;

        if !frame_dirty && !layout_changed {
            return Ok(());
        }

        let device = self.device.as_ref().ok_or_else(|| "missing render device".to_string())?;
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = multithread.SetMultithreadProtected(true);
            }
        }
        let video_device = self
            .video_device
            .as_ref()
            .ok_or_else(|| "missing video device".to_string())?;
        let processor_enum = self
            .processor_enum
            .as_ref()
            .ok_or_else(|| "missing video processor enumerator".to_string())?;
        let processor = self
            .processor
            .as_ref()
            .ok_or_else(|| "missing video processor".to_string())?;
        let output_view = self
            .output_view
            .as_ref()
            .ok_or_else(|| "missing video processor output view".to_string())?;
        let video_context = self
            .video_context
            .as_ref()
            .ok_or_else(|| "missing video context".to_string())?;
        let input_texture: ID3D11Texture2D =
            unsafe { clone_interface(frame.texture.as_ptr(), "decoder texture")? };
        let input_view = create_input_view(video_device, processor_enum, &input_texture, frame.array_index)?;

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input_view.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        unsafe {
            video_context
                .VideoProcessorBlt(processor, output_view, 0, std::slice::from_ref(&stream))
                .map_err(|err| format!("ID3D11VideoContext::VideoProcessorBlt failed: {err}"))?;
        }

        let swap_chain = self
            .swap_chain
            .as_ref()
            .ok_or_else(|| "missing swap chain".to_string())?;
        unsafe {
            swap_chain
                .Present(0, DXGI_PRESENT(0))
                .ok()
                .map_err(|err| format!("IDXGISwapChain::Present failed: {err}"))?;
        }
        Ok(())
    }

    fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(self.video_hwnd, SHOW_WINDOW_CMD(SW_HIDE.0));
        }
        let _ = self.set_parent_clipchildren(false);
        self.last_rect = None;
        self.last_scale = 0.0;
    }

    fn current_rect(&self) -> Option<egui::Rect> {
        self.last_rect
    }

    fn update_host_window(
        &mut self,
        rect: egui::Rect,
        pixels_per_point: f32,
    ) -> Result<bool, String> {
        let (width, height) = pixel_size(rect, pixels_per_point);
        if width == 0 || height == 0 {
            return Ok(false);
        }

        let changed = self.last_rect != Some(rect)
            || (self.last_scale - pixels_per_point).abs() > f32::EPSILON;
        if !changed {
            unsafe {
                let _ = ShowWindow(self.video_hwnd, SHOW_WINDOW_CMD(SW_SHOWNA.0));
            }
            return Ok(false);
        }

        self.set_parent_clipchildren(true)?;
        let x = (rect.left() * pixels_per_point).round() as i32;
        let y = (rect.top() * pixels_per_point).round() as i32;
        unsafe {
            SetWindowPos(
                self.video_hwnd,
                None,
                x,
                y,
                width as i32,
                height as i32,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOZORDER | SWP_SHOWWINDOW,
            )
            .map_err(|err| format!("SetWindowPos(video host) failed: {err}"))?;
            let _ = ShowWindow(self.video_hwnd, SHOW_WINDOW_CMD(SW_SHOWNA.0));
        }
        self.last_rect = Some(rect);
        self.last_scale = pixels_per_point;
        Ok(true)
    }

    fn ensure_render_state(
        &mut self,
        frame: &WindowsD3d11Frame,
        output_size: (u32, u32),
    ) -> Result<(), String> {
        let device_key = frame.device.as_ptr() as usize;
        let decoder_size = (frame.width, frame.height);
        let need_device_recreate = self.swap_chain.is_none() || self.device_key != device_key;
        let need_resize = need_device_recreate
            || self.output_size != output_size
            || self.decoder_size != decoder_size
            || self.output_view.is_none()
            || self.processor.is_none()
            || self.processor_enum.is_none();

        if need_device_recreate {
            self.device_key = device_key;
            self.device = Some(unsafe { clone_interface(frame.device.as_ptr(), "decoder device")? });
            self.video_device =
                Some(unsafe { clone_interface(frame.video_device.as_ptr(), "video device")? });
            self.video_context =
                Some(unsafe { clone_interface(frame.video_context.as_ptr(), "video context")? });
            self.swap_chain = Some(create_swap_chain(
                &self.factory,
                self.device.as_ref().expect("device set"),
                self.video_hwnd,
                output_size,
            )?);
            self.output_view = None;
            self.processor_enum = None;
            self.processor = None;
        }

        if need_resize && !need_device_recreate {
            let swap_chain = self
                .swap_chain
                .as_ref()
                .ok_or_else(|| "missing swap chain".to_string())?;
            unsafe {
                swap_chain
                    .ResizeBuffers(
                        0,
                        output_size.0,
                        output_size.1,
                        DXGI_FORMAT_UNKNOWN,
                        DXGI_SWAP_CHAIN_FLAG(0),
                    )
                    .map_err(|err| format!("IDXGISwapChain::ResizeBuffers failed: {err}"))?;
            }
            self.output_view = None;
            self.processor_enum = None;
            self.processor = None;
        }

        if need_resize {
            self.decoder_size = decoder_size;
            self.output_size = output_size;
            self.rebuild_video_processor()?;
        }

        Ok(())
    }

    fn rebuild_video_processor(&mut self) -> Result<(), String> {
        let video_device = self
            .video_device
            .as_ref()
            .ok_or_else(|| "missing video device".to_string())?;
        let video_context = self
            .video_context
            .as_ref()
            .ok_or_else(|| "missing video context".to_string())?;
        let swap_chain = self
            .swap_chain
            .as_ref()
            .ok_or_else(|| "missing swap chain".to_string())?;
        let (input_width, input_height) = self.decoder_size;
        let (output_width, output_height) = self.output_size;

        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: input_width,
            InputHeight: input_height,
            OutputFrameRate: windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: output_width,
            OutputHeight: output_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe {
            video_device
                .CreateVideoProcessorEnumerator(&desc)
                .map_err(|err| format!("CreateVideoProcessorEnumerator failed: {err}"))?
        };
        let processor = unsafe {
            video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|err| format!("CreateVideoProcessor failed: {err}"))?
        };
        let source_rect = RECT {
            left: 0,
            top: 0,
            right: input_width as i32,
            bottom: input_height as i32,
        };
        let target_rect = RECT {
            left: 0,
            top: 0,
            right: output_width as i32,
            bottom: output_height as i32,
        };
        unsafe {
            video_context.VideoProcessorSetOutputTargetRect(
                &processor,
                true,
                Some(&target_rect as *const RECT),
            );
            video_context.VideoProcessorSetStreamSourceRect(
                &processor,
                0,
                true,
                Some(&source_rect as *const RECT),
            );
        }

        let backbuffer: ID3D11Texture2D = unsafe {
            swap_chain
                .GetBuffer(0)
                .map_err(|err| format!("IDXGISwapChain::GetBuffer failed: {err}"))?
        };
        let output_view = create_output_view(video_device, &enumerator, &backbuffer)?;
        self.processor_enum = Some(enumerator);
        self.processor = Some(processor);
        self.output_view = Some(output_view);
        Ok(())
    }

    fn set_parent_clipchildren(&mut self, enabled: bool) -> Result<(), String> {
        if enabled == self.clipchildren_applied {
            return Ok(());
        }

        let style = unsafe { GetWindowLongW(self.parent_hwnd, WINDOW_LONG_PTR_INDEX(-16)) };
        let next_style = if enabled {
            style | WS_CLIPCHILDREN.0 as i32
        } else if self.parent_had_clipchildren {
            style
        } else {
            style & !(WS_CLIPCHILDREN.0 as i32)
        };

        if next_style != style {
            unsafe {
                let _ = SetWindowLongW(self.parent_hwnd, WINDOW_LONG_PTR_INDEX(-16), next_style);
                SetWindowPos(
                    self.parent_hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE
                        | SWP_NOSIZE
                        | SWP_NOZORDER
                        | SWP_NOACTIVATE
                        | SWP_NOOWNERZORDER
                        | SWP_FRAMECHANGED,
                )
                .map_err(|err| format!("SetWindowPos(parent clipchildren) failed: {err}"))?;
            }
        }

        self.clipchildren_applied = enabled;
        Ok(())
    }
}

impl Drop for WindowsSwapchainRenderer {
    fn drop(&mut self) {
        let _ = self.set_parent_clipchildren(false);
        unsafe {
            let _ = ShowWindow(self.video_hwnd, SHOW_WINDOW_CMD(SW_HIDE.0));
            let _ = DestroyWindow(self.video_hwnd);
        }
    }
}

fn root_hwnd(frame: &EframeFrame) -> Result<HWND, String> {
    let handle = frame
        .window_handle()
        .map_err(|err| format!("window_handle unavailable: {err}"))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Ok(HWND(win32.hwnd.get() as *mut c_void)),
        other => Err(format!("unsupported Windows window handle: {other:?}")),
    }
}

unsafe fn clone_interface<T>(ptr: *mut c_void, label: &str) -> Result<T, String>
where
    T: Interface + Clone,
{
    T::from_raw_borrowed(&ptr)
        .cloned()
        .ok_or_else(|| format!("{label} pointer was null"))
}

fn pixel_size(rect: egui::Rect, pixels_per_point: f32) -> (u32, u32) {
    let width = (rect.width().max(0.0) * pixels_per_point).round() as i32;
    let height = (rect.height().max(0.0) * pixels_per_point).round() as i32;
    (width.max(0) as u32, height.max(0) as u32)
}

fn create_swap_chain(
    factory: &IDXGIFactory2,
    device: &ID3D11Device,
    hwnd: HWND,
    output_size: (u32, u32),
) -> Result<IDXGISwapChain1, String> {
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: output_size.0,
        Height: output_size.1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: DXGI_SWAP_CHAIN_FLAG(0).0 as u32,
    };
    unsafe {
        factory
            .CreateSwapChainForHwnd(device, hwnd, &desc, None, None::<&IDXGIOutput>)
            .map_err(|err| format!("CreateSwapChainForHwnd failed: {err}"))
    }
}

fn create_input_view(
    video_device: &ID3D11VideoDevice,
    processor_enum: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
    array_index: u32,
) -> Result<ID3D11VideoProcessorInputView, String> {
    let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: 0,
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPIV {
                MipSlice: 0,
                ArraySlice: array_index,
            },
        },
    };

    let mut view = None;
    unsafe {
        video_device
            .CreateVideoProcessorInputView(texture, processor_enum, &desc, Some(&mut view))
            .map_err(|err| format!("CreateVideoProcessorInputView failed: {err}"))?;
    }
    view.ok_or_else(|| "CreateVideoProcessorInputView returned null view".into())
}

fn create_output_view(
    video_device: &ID3D11VideoDevice,
    processor_enum: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11VideoProcessorOutputView, String> {
    let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
        },
    };

    let mut view = None;
    unsafe {
        video_device
            .CreateVideoProcessorOutputView(texture, processor_enum, &desc, Some(&mut view))
            .map_err(|err| format!("CreateVideoProcessorOutputView failed: {err}"))?;
    }
    view.ok_or_else(|| "CreateVideoProcessorOutputView returned null view".into())
}
