mod audio;
mod debug_state;
mod decode;
mod display;
mod input;
mod pipeline;
mod render_gl;
#[cfg(target_os = "macos")]
mod render_macos;
#[cfg(target_os = "macos")]
mod render_macos_metal;
#[cfg(target_os = "windows")]
mod render_windows;
mod transport;
mod video_frame;

use eframe::egui;
use input::{LocalCaptureMode, LocalKeyboardState, RemoteCursorTexture, SharedInputState};
use render_gl::NativeVideoTexture;
use st_protocol::{
    ClientDisplayInfo, ClockSyncPing, ControlMessage, ControllerState, InputPacket, KeyboardKey,
    KeyboardStateInput, MouseAbsoluteInput, MouseButtonsInput, MouseRelativeInput, MouseWheelInput,
    StreamConfig, TransportFeedback, MOUSE_BUTTON_EXTRA1, MOUSE_BUTTON_EXTRA2, MOUSE_BUTTON_MIDDLE,
    MOUSE_BUTTON_PRIMARY, MOUSE_BUTTON_SECONDARY,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use transport::AudioPacket;
use video_frame::{NativeSurfaceCapabilities, NativeSurfaceControl, VideoFrameBuffer};

use crate::debug_state::{unix_time_micros, ConnectionDebugSnapshot, ConnectionDebugState};

const UDP_PORT: u16 = 5000;
const MAX_REMOTE_CURSOR_TEXTURES: usize = 8;

fn trace_enabled() -> bool {
    std::env::var_os("ST_TRACE").is_some()
}

#[derive(Clone, PartialEq)]
enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DebugOverlayTab {
    General,
    Cursor,
}

struct CursorOverlayGeometry {
    texture_id: egui::TextureId,
    serial: u64,
    rect: egui::Rect,
    source_size: egui::Vec2,
    hotspot: egui::Vec2,
    cursor_pos: egui::Pos2,
    scale_x: f32,
    scale_y: f32,
    display_scale: f32,
    using_pointer_pos: bool,
}

struct StreamApp {
    server_addr: String,
    audio_enabled: bool,
    debug_enabled: bool,
    display_refresh_millihz: Option<u32>,
    audio_enabled_flag: Arc<AtomicBool>,
    debug_enabled_flag: Arc<AtomicBool>,
    state: Arc<Mutex<ConnectionState>>,
    frame: Arc<Mutex<VideoFrameBuffer>>,
    debug_state: Arc<ConnectionDebugState>,
    video_texture: NativeVideoTexture,
    native_surfaces: Arc<NativeSurfaceControl>,
    disconnect_flag: Arc<AtomicBool>,
    connection_epoch: Arc<AtomicU64>,
    shared_input: Arc<SharedInputState>,
    control_tx: Option<crossbeam_channel::Sender<ControlMessage>>,
    input_tx: Option<crossbeam_channel::Sender<InputPacket>>,
    capture_mode: LocalCaptureMode,
    pointer_buttons: u8,
    keyboard_state: LocalKeyboardState,
    pending_capture_click: bool,
    last_video_rect: Option<egui::Rect>,
    remote_cursor_textures: BTreeMap<u64, RemoteCursorTexture>,
    latest_remote_cursor_serial: Option<u64>,
    seen_cursor_shape_version: u64,
    debug_overlay_tab: DebugOverlayTab,
    menu_open: bool,
    local_overlay_hit_rects: Vec<egui::Rect>,
    last_pointer_move: Option<Instant>,
    applied_cursor_visible: Option<bool>,
    applied_cursor_grab: Option<egui::CursorGrab>,
}

// ---------------------------------------------------------------------------
// Server address persistence
// ---------------------------------------------------------------------------

fn state_dir() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local/state")
        });
    base.join("st")
}

fn load_last_server() -> Option<String> {
    std::fs::read_to_string(state_dir().join("last_server"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_last_server(addr: &str) {
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("last_server"), addr);
}

fn load_audio_enabled() -> bool {
    std::fs::read_to_string(state_dir().join("audio_enabled"))
        .ok()
        .map(|s| s.trim() != "0")
        .unwrap_or(true)
}

fn save_audio_enabled(enabled: bool) {
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("audio_enabled"), if enabled { "1" } else { "0" });
}

fn load_debug_enabled() -> bool {
    std::fs::read_to_string(state_dir().join("debug_enabled"))
        .ok()
        .map(|s| s.trim() != "0")
        .unwrap_or(false)
}

fn save_debug_enabled(enabled: bool) {
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("debug_enabled"), if enabled { "1" } else { "0" });
}

// ---------------------------------------------------------------------------
// StreamApp
// ---------------------------------------------------------------------------

impl StreamApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let saved = load_last_server().unwrap_or_else(|| "127.0.0.1:8080".to_string());
        let audio = load_audio_enabled();
        let debug_enabled = load_debug_enabled();
        let display_refresh_millihz = display::detect_max_refresh_millihz();
        if let Some(rate) = display_refresh_millihz {
            println!(
                "[display] max client refresh detected: {:.3} Hz",
                rate as f32 / 1000.0
            );
        }
        let video_texture = NativeVideoTexture::new(cc.gl.as_ref());
        let native_surfaces = Arc::new(NativeSurfaceControl::new(
            video_texture.native_surface_capabilities(),
        ));
        Self {
            server_addr: saved,
            audio_enabled: audio,
            debug_enabled,
            display_refresh_millihz,
            audio_enabled_flag: Arc::new(AtomicBool::new(audio)),
            debug_enabled_flag: Arc::new(AtomicBool::new(debug_enabled)),
            state: Arc::new(Mutex::new(ConnectionState::Disconnected)),
            frame: Arc::new(Mutex::new(VideoFrameBuffer::default())),
            debug_state: Arc::new(ConnectionDebugState::new()),
            video_texture,
            native_surfaces,
            disconnect_flag: Arc::new(AtomicBool::new(false)),
            connection_epoch: Arc::new(AtomicU64::new(0)),
            shared_input: Arc::new(SharedInputState::new()),
            control_tx: None,
            input_tx: None,
            capture_mode: LocalCaptureMode::Idle,
            pointer_buttons: 0,
            keyboard_state: LocalKeyboardState::default(),
            pending_capture_click: false,
            last_video_rect: None,
            remote_cursor_textures: BTreeMap::new(),
            latest_remote_cursor_serial: None,
            seen_cursor_shape_version: 0,
            debug_overlay_tab: DebugOverlayTab::General,
            menu_open: false,
            local_overlay_hit_rects: Vec::new(),
            last_pointer_move: None,
            applied_cursor_visible: None,
            applied_cursor_grab: None,
        }
    }

    fn connect(&mut self, ctx: egui::Context) {
        save_last_server(&self.server_addr);

        self.disconnect_flag.store(true, Ordering::SeqCst);
        let disconnect_flag = Arc::new(AtomicBool::new(false));
        self.disconnect_flag = Arc::clone(&disconnect_flag);
        let connection_epoch = Arc::clone(&self.connection_epoch);
        let session_epoch = connection_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.menu_open = false;
        self.pointer_buttons = 0;
        self.keyboard_state.clear();
        self.pending_capture_click = false;
        self.capture_mode = LocalCaptureMode::Idle;
        self.last_video_rect = None;
        self.remote_cursor_textures.clear();
        self.latest_remote_cursor_serial = None;
        self.seen_cursor_shape_version = 0;
        self.shared_input.reset();
        self.audio_enabled_flag
            .store(self.audio_enabled, Ordering::SeqCst);
        self.native_surfaces
            .reset(self.video_texture.native_surface_capabilities());
        *self.state.lock().unwrap() = ConnectionState::Connecting;

        let (control_tx, control_rx) = crossbeam_channel::bounded::<ControlMessage>(64);
        let (input_tx, input_rx) = crossbeam_channel::bounded::<InputPacket>(256);
        self.control_tx = Some(control_tx);
        self.input_tx = Some(input_tx);

        let addr = self.server_addr.clone();
        let state = Arc::clone(&self.state);
        let frame_buf = Arc::clone(&self.frame);
        let disconnect = disconnect_flag;
        let connection_epoch = Arc::clone(&self.connection_epoch);
        let audio_flag = Arc::clone(&self.audio_enabled_flag);
        let debug_enabled_flag = Arc::clone(&self.debug_enabled_flag);
        let debug_state = Arc::clone(&self.debug_state);
        let native_surfaces = Arc::clone(&self.native_surfaces);
        let display_refresh_millihz = self.display_refresh_millihz;
        let shared_input = Arc::clone(&self.shared_input);
        debug_state.reset_for_connect(&addr, display_refresh_millihz);

        std::thread::spawn(move || {
            run_connection(
                addr,
                display_refresh_millihz,
                state,
                frame_buf,
                debug_state,
                disconnect,
                connection_epoch,
                session_epoch,
                audio_flag,
                debug_enabled_flag,
                native_surfaces,
                shared_input,
                control_rx,
                input_rx,
                ctx,
            );
        });
    }

    fn disconnect(&mut self) {
        self.disconnect_flag.store(true, Ordering::SeqCst);
        self.connection_epoch.fetch_add(1, Ordering::SeqCst);
        self.clear_remote_keyboard();
        self.release_pointer_capture();
        self.video_texture.clear_frame();
        self.debug_state.reset_for_connect(&self.server_addr, None);
        self.shared_input.reset();
        self.control_tx = None;
        self.input_tx = None;
        self.pointer_buttons = 0;
        self.keyboard_state.clear();
        self.pending_capture_click = false;
        self.capture_mode = LocalCaptureMode::Idle;
        self.last_video_rect = None;
        self.remote_cursor_textures.clear();
        self.latest_remote_cursor_serial = None;
        self.seen_cursor_shape_version = 0;
        self.menu_open = false;
        self.local_overlay_hit_rects.clear();
        let mut s = self.state.lock().unwrap();
        if matches!(*s, ConnectionState::Connecting | ConnectionState::Connected) {
            *s = ConnectionState::Disconnected;
        }
    }

    fn release_pointer_capture(&mut self) {
        self.capture_mode = LocalCaptureMode::Idle;
        self.pointer_buttons = 0;
        self.pending_capture_click = false;
    }

    fn clear_local_session_interaction(&mut self) {
        self.clear_remote_keyboard();
        self.release_pointer_capture();
        self.control_tx = None;
        self.input_tx = None;
        self.remote_cursor_textures.clear();
        self.latest_remote_cursor_serial = None;
        self.seen_cursor_shape_version = 0;
        self.last_video_rect = None;
        self.menu_open = false;
        self.local_overlay_hit_rects.clear();
    }

    fn force_release_capture(&mut self) {
        self.capture_mode = LocalCaptureMode::ForceReleased;
        self.pending_capture_click = false;
        self.pointer_buttons = 0;
        self.clear_remote_keyboard();
        if let Some(client_id) = self.shared_input.snapshot().client_id {
            self.send_input_packet(InputPacket::MouseButtons(MouseButtonsInput {
                client_id,
                buttons: 0,
            }));
        }
    }

    fn keyboard_forward_active(&self, snapshot: &input::SharedInputSnapshot) -> bool {
        snapshot.controller_state == ControllerState::OwnedByYou
            && snapshot.capabilities.keyboard
            && matches!(
                self.capture_mode,
                LocalCaptureMode::HoverAbsolute | LocalCaptureMode::CapturedRelative
            )
    }

    fn send_keyboard_snapshot(&self, client_id: u32) {
        self.send_input_packet(InputPacket::KeyboardState(KeyboardStateInput {
            client_id,
            pressed: self.keyboard_state.pressed(),
        }));
    }

    fn clear_remote_keyboard(&mut self) {
        if self.keyboard_state.clear() {
            if let Some(client_id) = self.shared_input.snapshot().client_id {
                self.send_keyboard_snapshot(client_id);
            }
        }
    }

    fn pointer_over_local_overlay(&self, pos: egui::Pos2) -> bool {
        self.local_overlay_hit_rects
            .iter()
            .any(|rect| rect.contains(pos))
    }

    fn remote_cursor_texture_for_serial(&self, serial: u64) -> Option<&RemoteCursorTexture> {
        if serial != 0 {
            if let Some(texture) = self.remote_cursor_textures.get(&serial) {
                return Some(texture);
            }
        }
        self.latest_remote_cursor_serial
            .and_then(|serial| self.remote_cursor_textures.get(&serial))
    }

    fn overlay_cursor_active(&self, ctx: &egui::Context) -> bool {
        let snapshot = self.shared_input.snapshot();
        if snapshot.controller_state != ControllerState::OwnedByYou
            || !snapshot.capabilities.separate_cursor
            || !snapshot.cursor_state.visible
            || self
                .remote_cursor_texture_for_serial(snapshot.cursor_state.serial)
                .is_none()
        {
            return false;
        }

        match self.capture_mode {
            LocalCaptureMode::HoverAbsolute => ctx
                .input(|i| i.pointer.latest_pos())
                .zip(self.last_video_rect)
                .map(|(pos, rect)| rect.contains(pos))
                .unwrap_or(false),
            LocalCaptureMode::CapturedRelative => true,
            _ => false,
        }
    }

    fn native_cursor_fallback_active(&self) -> bool {
        let snapshot = self.shared_input.snapshot();
        self.capture_mode == LocalCaptureMode::CapturedRelative
            && snapshot.controller_state == ControllerState::OwnedByYou
            && snapshot.capabilities.separate_cursor
            && self
                .remote_cursor_texture_for_serial(snapshot.cursor_state.serial)
                .is_none()
            && (snapshot.cursor_state.visible || snapshot.cursor_state.serial == 0)
    }

    fn apply_pointer_capture_mode(&mut self, ctx: &egui::Context) {
        let overlay_cursor_active = self.overlay_cursor_active(ctx);
        let (cursor_visible, cursor_grab) =
            if self.capture_mode == LocalCaptureMode::CapturedRelative {
                if self.native_cursor_fallback_active() {
                    (true, egui::CursorGrab::Confined)
                } else {
                    (false, egui::CursorGrab::Locked)
                }
            } else if overlay_cursor_active {
                (false, egui::CursorGrab::None)
            } else {
                (true, egui::CursorGrab::None)
            };

        if self.applied_cursor_grab != Some(cursor_grab) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(cursor_grab));
            self.applied_cursor_grab = Some(cursor_grab);
        }
        if self.applied_cursor_visible != Some(cursor_visible) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(cursor_visible));
            self.applied_cursor_visible = Some(cursor_visible);
        }
    }

    fn send_control_message(&self, message: ControlMessage) {
        if let Some(tx) = &self.control_tx {
            let _ = tx.try_send(message);
        }
    }

    fn send_input_packet(&self, packet: InputPacket) {
        if let Some(tx) = &self.input_tx {
            let _ = tx.try_send(packet);
        }
    }

    fn sync_remote_cursor_texture(&mut self, ctx: &egui::Context) {
        let snapshot = self.shared_input.snapshot();
        if snapshot.cursor_shape_version == self.seen_cursor_shape_version {
            return;
        }

        self.seen_cursor_shape_version = snapshot.cursor_shape_version;
        if let Some(shape) = snapshot.cursor_shape {
            self.latest_remote_cursor_serial = Some(shape.serial);
            self.remote_cursor_textures.insert(
                shape.serial,
                RemoteCursorTexture {
                    hotspot: egui::vec2(shape.hotspot_x as f32, shape.hotspot_y as f32),
                    size: egui::vec2(shape.width as f32, shape.height as f32),
                    texture: ctx.load_texture(
                        format!("remote_cursor_{}", shape.serial),
                        egui::ColorImage::from_rgba_premultiplied(
                            [shape.width as usize, shape.height as usize],
                            &shape.rgba,
                        ),
                        egui::TextureOptions::NEAREST,
                    ),
                },
            );

            while self.remote_cursor_textures.len() > MAX_REMOTE_CURSOR_TEXTURES {
                let Some(oldest_serial) = self.remote_cursor_textures.keys().next().copied() else {
                    break;
                };
                self.remote_cursor_textures.remove(&oldest_serial);
            }
        } else {
            self.remote_cursor_textures.clear();
            self.latest_remote_cursor_serial = None;
        }
    }

    fn compute_cursor_overlay_geometry(
        &self,
        ctx: &egui::Context,
        stream_size: egui::Vec2,
        input_snapshot: &input::SharedInputSnapshot,
    ) -> Option<CursorOverlayGeometry> {
        if !matches!(
            self.capture_mode,
            LocalCaptureMode::HoverAbsolute | LocalCaptureMode::CapturedRelative
        ) {
            return None;
        }
        if input_snapshot.controller_state != ControllerState::OwnedByYou
            || !input_snapshot.capabilities.separate_cursor
            || !input_snapshot.cursor_state.visible
        {
            return None;
        }

        let texture = self.remote_cursor_texture_for_serial(input_snapshot.cursor_state.serial)?;
        let video_rect = self.last_video_rect?;
        let scale_x = if stream_size.x > 0.0 {
            video_rect.width() / stream_size.x
        } else {
            1.0
        };
        let scale_y = if stream_size.y > 0.0 {
            video_rect.height() / stream_size.y
        } else {
            1.0
        };
        let display_scale = if stream_size.x > 0.0 && stream_size.y > 0.0 {
            (video_rect.width() / stream_size.x).min(video_rect.height() / stream_size.y)
        } else {
            1.0
        };
        let pixels_per_point = ctx.pixels_per_point().max(1.0);
        let snap = |value: f32| (value * pixels_per_point).round() / pixels_per_point;
        let size = egui::vec2(
            snap((texture.size.x * display_scale).max(1.0)),
            snap((texture.size.y * display_scale).max(1.0)),
        );
        let hotspot = egui::vec2(
            snap(texture.hotspot.x * display_scale),
            snap(texture.hotspot.y * display_scale),
        );

        let (cursor_pos, using_pointer_pos) = if self.capture_mode == LocalCaptureMode::HoverAbsolute
        {
            let pointer_pos = ctx
                .input(|i| i.pointer.latest_pos())
                .filter(|pos| video_rect.contains(*pos))?;
            (pointer_pos, true)
        } else {
            (
                egui::pos2(
                    video_rect.left() + input_snapshot.cursor_state.x as f32 * scale_x,
                    video_rect.top() + input_snapshot.cursor_state.y as f32 * scale_y,
                ),
                false,
            )
        };

        let top_left = egui::pos2(snap(cursor_pos.x - hotspot.x), snap(cursor_pos.y - hotspot.y));
        let rect = egui::Rect::from_min_size(top_left, size);
        if rect.max.x <= video_rect.left()
            || rect.max.y <= video_rect.top()
            || rect.min.x >= video_rect.right()
            || rect.min.y >= video_rect.bottom()
        {
            return None;
        }

        Some(CursorOverlayGeometry {
            texture_id: texture.texture.id(),
            serial: input_snapshot.cursor_state.serial,
            rect,
            source_size: texture.size,
            hotspot,
            cursor_pos,
            scale_x,
            scale_y,
            display_scale,
            using_pointer_pos,
        })
    }

    fn cursor_debug_lines(
        &self,
        ctx: &egui::Context,
        input_snapshot: &input::SharedInputSnapshot,
    ) -> Vec<String> {
        let stream_size = self.video_texture.size_vec2();
        let pointer_pos = ctx.input(|i| i.pointer.latest_pos());
        let over_video = pointer_pos
            .zip(self.last_video_rect)
            .map(|(pos, rect)| rect.contains(pos))
            .unwrap_or(false);
        let over_local_overlay = pointer_pos
            .map(|pos| self.pointer_over_local_overlay(pos))
            .unwrap_or(false);
        let active_texture = self.remote_cursor_texture_for_serial(input_snapshot.cursor_state.serial);
        let overlay_geometry =
            self.compute_cursor_overlay_geometry(ctx, stream_size, input_snapshot);
        let mut lines = vec![
            format!(
                "cursor: mode={} controller={:?} visible={} separate={} overlay_active={} native_fallback={}",
                self.capture_mode.label(),
                input_snapshot.controller_state,
                if input_snapshot.cursor_state.visible { "y" } else { "n" },
                if input_snapshot.capabilities.separate_cursor { "y" } else { "n" },
                if overlay_geometry.is_some() { "y" } else { "n" },
                if self.native_cursor_fallback_active() { "y" } else { "n" },
            ),
            format!(
                "cursor: serial={} shape_cached={} latest_shape={} shape_ver={} state_ver={}",
                input_snapshot.cursor_state.serial,
                if active_texture.is_some() { "y" } else { "n" },
                self.latest_remote_cursor_serial
                    .map(|serial| serial.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                input_snapshot.cursor_shape_version,
                input_snapshot.cursor_state_version,
            ),
            format!(
                "stream: size={}x{}  video_rect={}",
                stream_size.x.round() as i32,
                stream_size.y.round() as i32,
                format_rect_opt(self.last_video_rect),
            ),
            format!(
                "pointer: pos={} over_video={} over_local_ui={}",
                format_pos_opt(pointer_pos),
                if over_video { "y" } else { "n" },
                if over_local_overlay { "y" } else { "n" },
            ),
            format!(
                "server: pos=({}, {}) visible={} serial={}",
                input_snapshot.cursor_state.x,
                input_snapshot.cursor_state.y,
                if input_snapshot.cursor_state.visible { "y" } else { "n" },
                input_snapshot.cursor_state.serial,
            ),
        ];

        if let Some(texture) = active_texture {
            lines.push(format!(
                "shape: source={}x{} hotspot=({}, {})",
                texture.size.x.round() as i32,
                texture.size.y.round() as i32,
                texture.hotspot.x.round() as i32,
                texture.hotspot.y.round() as i32,
            ));
        } else {
            lines.push("shape: -".to_string());
        }

        if let Some(geometry) = overlay_geometry {
            lines.push(format!(
                "mapped: serial={} pos={} rect={} pointer_driven={}",
                geometry.serial,
                format_pos(geometry.cursor_pos),
                format_rect(geometry.rect),
                if geometry.using_pointer_pos { "y" } else { "n" },
            ));
            lines.push(format!(
                "mapped: source={}x{} hotspot={} scale=({:.4}, {:.4}) display_scale={:.4}",
                geometry.source_size.x.round() as i32,
                geometry.source_size.y.round() as i32,
                format_vec2(geometry.hotspot),
                geometry.scale_x,
                geometry.scale_y,
                geometry.display_scale,
            ));
        } else {
            lines.push("mapped: -".to_string());
        }

        lines
    }

    fn handle_connected_video_response(
        &mut self,
        ctx: &egui::Context,
        response: &egui::Response,
        stream_size: egui::Vec2,
    ) {
        self.last_video_rect = Some(response.rect);
        let previous_capture_mode = self.capture_mode;

        let snapshot = self.shared_input.snapshot();
        let hover_supported =
            snapshot.capabilities.mouse_absolute && snapshot.capabilities.separate_cursor;
        let pointer_over_video = ctx
            .input(|i| i.pointer.latest_pos())
            .map(|pos| response.rect.contains(pos))
            .unwrap_or(false);
        if snapshot.controller_state != ControllerState::OwnedByYou
            && matches!(
                self.capture_mode,
                LocalCaptureMode::HoverAbsolute | LocalCaptureMode::CapturedRelative
            )
        {
            self.capture_mode = LocalCaptureMode::Idle;
        }
        if (!pointer_over_video || !hover_supported)
            && self.capture_mode == LocalCaptureMode::HoverAbsolute
        {
            self.capture_mode = LocalCaptureMode::Idle;
        }

        if self.capture_mode == LocalCaptureMode::CapturedRelative && !ctx.input(|i| i.focused) {
            self.force_release_capture();
        }

        if hover_supported
            && self.capture_mode != LocalCaptureMode::CapturedRelative
            && self.capture_mode != LocalCaptureMode::ForceReleased
            && pointer_over_video
            && snapshot.controller_state == ControllerState::OwnedByYou
        {
            self.capture_mode = LocalCaptureMode::HoverAbsolute;
        }

        if response.clicked_by(egui::PointerButton::Primary) {
            if snapshot.controller_state == ControllerState::OwnedByYou {
                if snapshot.capabilities.mouse_relative {
                    self.capture_mode = LocalCaptureMode::CapturedRelative;
                } else if hover_supported {
                    self.capture_mode = LocalCaptureMode::HoverAbsolute;
                }
                self.pending_capture_click = false;
            } else {
                self.send_control_message(ControlMessage::AcquireControl);
                self.pending_capture_click = true;
            }
            ctx.request_repaint();
        }

        if self.pending_capture_click && snapshot.controller_state == ControllerState::OwnedByYou {
            if snapshot.capabilities.mouse_relative {
                self.capture_mode = LocalCaptureMode::CapturedRelative;
            } else if hover_supported {
                self.capture_mode = LocalCaptureMode::HoverAbsolute;
            }
            self.pending_capture_click = false;
        }

        if pointer_over_video
            && self.capture_mode == LocalCaptureMode::ForceReleased
            && response.clicked_by(egui::PointerButton::Primary)
            && snapshot.controller_state == ControllerState::OwnedByYou
            && snapshot.capabilities.mouse_relative
        {
            self.capture_mode = LocalCaptureMode::CapturedRelative;
        }

        if previous_capture_mode != self.capture_mode && self.capture_mode == LocalCaptureMode::Idle
        {
            self.clear_remote_keyboard();
        }

        self.draw_remote_cursor_overlay(ctx, stream_size);
    }

    fn draw_remote_cursor_overlay(&self, ctx: &egui::Context, stream_size: egui::Vec2) {
        let snapshot = self.shared_input.snapshot();
        let Some(geometry) = self.compute_cursor_overlay_geometry(ctx, stream_size, &snapshot) else {
            return;
        };
        egui::Area::new(egui::Id::new("remote_cursor_overlay"))
            .order(egui::Order::Tooltip)
            .fixed_pos(geometry.rect.min)
            .show(ctx, |ui| {
                let sized =
                    egui::load::SizedTexture::new(geometry.texture_id, geometry.rect.size());
                ui.image(sized);
            });
    }

    fn render_home_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let rect = ui.max_rect();
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(8, 11, 16));
        painter.circle_filled(
            egui::pos2(rect.right() - 90.0, rect.top() + 90.0),
            170.0,
            egui::Color32::from_rgba_unmultiplied(64, 174, 255, 26),
        );
        painter.circle_filled(
            egui::pos2(rect.left() + 90.0, rect.bottom() - 70.0),
            200.0,
            egui::Color32::from_rgba_unmultiplied(50, 210, 154, 20),
        );

        let available_width = ui.available_width();
        let outer_padding = if available_width < 520.0 { 10.0 } else { 16.0 };
        let content_width = (available_width - outer_padding * 2.0).clamp(0.0, 980.0);
        let caps = self.native_surfaces.snapshot();
        let compact_hero = content_width < 860.0;
        let split_cards = content_width > 920.0;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(34.0);
                ui.add_space(outer_padding);
                ui.vertical_centered(|ui| {
                    ui.set_max_width(content_width);
                    ui.set_width(content_width);

                    egui::Frame::popup(ui.style())
                        .fill(egui::Color32::from_rgba_unmultiplied(15, 20, 28, 232))
                        .show(ui, |ui| {
                            if compact_hero {
                                ui.vertical(|ui| {
                                    ui.label(
                                        egui::RichText::new("st viewer")
                                            .size(30.0)
                                            .strong()
                                            .color(egui::Color32::from_rgb(240, 244, 248)),
                                    );
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(
                                            "Low-latency streaming client with high-refresh negotiation and native presentation paths.",
                                        )
                                        .size(15.0)
                                        .color(egui::Color32::from_rgb(166, 178, 191)),
                                    );
                                    ui.add_space(10.0);
                                    ui.label(
                                        egui::RichText::new(
                                            "The menu below exposes the current viewer controls directly: last server, audio/debug defaults, display refresh hint, codec support, and the best zero-copy path this platform can use.",
                                        )
                                        .size(13.0)
                                        .color(egui::Color32::from_rgb(130, 143, 157)),
                                    );
                                    ui.add_space(12.0);
                                    render_client_summary_panel(ui, caps, self.display_refresh_millihz);
                                });
                            } else {
                                let row_width = ui.available_width();
                                let summary_width = (row_width * 0.28).clamp(210.0, 250.0);
                                let text_width = (row_width - summary_width - 14.0).max(280.0);
                                ui.horizontal_top(|ui| {
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(text_width, 0.0),
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new("st viewer")
                                                    .size(30.0)
                                                    .strong()
                                                    .color(egui::Color32::from_rgb(240, 244, 248)),
                                            );
                                            ui.add_space(4.0);
                                            ui.label(
                                                egui::RichText::new(
                                                    "Low-latency streaming client with high-refresh negotiation and native presentation paths.",
                                                )
                                                .size(15.0)
                                                .color(egui::Color32::from_rgb(166, 178, 191)),
                                            );
                                            ui.add_space(10.0);
                                            ui.label(
                                                egui::RichText::new(
                                                    "The menu below exposes the current viewer controls directly: last server, audio/debug defaults, display refresh hint, codec support, and the best zero-copy path this platform can use.",
                                                )
                                                .size(13.0)
                                                .color(egui::Color32::from_rgb(130, 143, 157)),
                                            );
                                        },
                                    );
                                    ui.add_space(14.0);
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(summary_width, 0.0),
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            render_client_summary_panel(
                                                ui,
                                                caps,
                                                self.display_refresh_millihz,
                                            );
                                        },
                                    );
                                });
                            }
                        });

                    ui.add_space(14.0);
                    if split_cards {
                        ui.columns(2, |columns| {
                            self.render_connection_card(&mut columns[0], ctx);
                            self.render_client_capabilities_card(&mut columns[1], caps);
                        });
                    } else {
                        self.render_connection_card(ui, ctx);
                        ui.add_space(12.0);
                        self.render_client_capabilities_card(ui, caps);
                    }

                    ui.add_space(12.0);
                    egui::Frame::popup(ui.style())
                        .fill(egui::Color32::from_rgba_unmultiplied(14, 18, 24, 214))
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new("Flow")
                                        .strong()
                                        .color(egui::Color32::from_rgb(228, 234, 240)),
                                );
                                ui.label(
                                    egui::RichText::new(
                                        "Enter on the server field connects immediately. Audio and Debug act as session defaults. The detected display refresh is sent as the FPS ceiling hint when the stream starts.",
                                    )
                                    .color(egui::Color32::from_rgb(154, 166, 179)),
                                );
                            });
                        });
                });
                ui.add_space(28.0);
            });
    }

    fn render_connection_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::Frame::popup(ui.style())
            .fill(egui::Color32::from_rgba_unmultiplied(16, 21, 28, 230))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Connection")
                        .size(19.0)
                        .strong()
                        .color(egui::Color32::from_rgb(240, 244, 248)),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Pick the server control address and launch the current viewer pipeline.",
                    )
                    .size(13.0)
                    .color(egui::Color32::from_rgb(156, 169, 181)),
                );
                ui.add_space(14.0);
                egui::Frame::popup(ui.style())
                    .fill(egui::Color32::from_rgba_unmultiplied(10, 31, 52, 206))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new("Quick Start")
                                    .strong()
                                    .color(egui::Color32::from_rgb(228, 241, 255)),
                            );
                            ui.label(
                                egui::RichText::new(
                                    "Set the server endpoint, then launch the viewer. Control stays on TCP, video and audio switch to UDP after setup.",
                                )
                                .size(12.0)
                                .color(egui::Color32::from_rgb(180, 205, 232)),
                            );
                        });
                    });
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("Server")
                        .strong()
                        .color(egui::Color32::from_rgb(224, 230, 236)),
                );
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.server_addr)
                        .hint_text("127.0.0.1:8080")
                        .desired_width(f32::INFINITY),
                );
                let can_connect = !self.server_addr.trim().is_empty();
                if response.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && can_connect
                {
                    self.video_texture.clear_frame();
                    self.connect(ctx.clone());
                }

                ui.add_space(10.0);
                let connect_width = ui.available_width().max(0.0);
                let narrow_button = connect_width < 260.0;
                let connect_label = if can_connect {
                    if narrow_button {
                        "Connect"
                    } else {
                        "Connect To Stream"
                    }
                } else {
                    if narrow_button {
                        "Enter Server"
                    } else {
                        "Enter Server To Connect"
                    }
                };
                if ui
                    .add_enabled(
                        can_connect,
                        egui::Button::new(
                            egui::RichText::new(connect_label)
                                .size(17.0)
                                .strong()
                                .color(egui::Color32::from_rgb(246, 250, 252)),
                        )
                        .min_size(egui::vec2(connect_width, 50.0))
                        .fill(egui::Color32::from_rgb(18, 122, 235)),
                    )
                    .clicked()
                {
                    self.video_texture.clear_frame();
                    self.connect(ctx.clone());
                }

                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new("Hint")
                            .strong()
                            .color(egui::Color32::from_rgb(219, 225, 233)),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Press Enter from the address field for immediate connect.",
                        )
                        .size(12.0)
                        .color(egui::Color32::from_rgb(144, 157, 170)),
                    );
                });
                ui.add_space(10.0);
                ui.label(egui::RichText::new(format!("last target: {}", self.server_addr)).monospace());
                ui.label(
                    egui::RichText::new(format!("server udp: {UDP_PORT}, client udp: auto"))
                        .monospace(),
                );
                ui.label(
                    egui::RichText::new(
                        "The client keeps the last server address, performs TCP control setup first, then switches video and audio to UDP media transport.",
                    )
                    .size(12.0)
                    .color(egui::Color32::from_rgb(132, 145, 158)),
                );
            });
    }

    fn render_client_capabilities_card(
        &mut self,
        ui: &mut egui::Ui,
        caps: NativeSurfaceCapabilities,
    ) {
        egui::Frame::popup(ui.style())
            .fill(egui::Color32::from_rgba_unmultiplied(16, 21, 28, 230))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Client Defaults")
                        .size(19.0)
                        .strong()
                        .color(egui::Color32::from_rgb(240, 244, 248)),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Tune the next session and verify which fast paths this client can use.",
                    )
                    .size(13.0)
                    .color(egui::Color32::from_rgb(156, 169, 181)),
                );
                ui.add_space(12.0);

                let audio_changed = render_setting_row(
                    ui,
                    "Audio",
                    "Start stereo playback when the stream format matches the current client pipeline.",
                    self.audio_enabled,
                );
                if audio_changed {
                    self.audio_enabled = !self.audio_enabled;
                    save_audio_enabled(self.audio_enabled);
                    self.audio_enabled_flag
                        .store(self.audio_enabled, Ordering::SeqCst);
                }
                ui.add_space(8.0);
                let debug_changed = render_setting_row(
                    ui,
                    "Debug Overlay",
                    "Expose transport, decoder, bitrate, FPS, and latency telemetry while connected.",
                    self.debug_enabled,
                );
                if debug_changed {
                    self.debug_enabled = !self.debug_enabled;
                    save_debug_enabled(self.debug_enabled);
                    self.debug_enabled_flag
                        .store(self.debug_enabled, Ordering::SeqCst);
                }

                ui.add_space(14.0);
                ui.separator();
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("Capabilities")
                        .strong()
                        .color(egui::Color32::from_rgb(224, 230, 236)),
                );
                ui.add_space(8.0);
                if ui.available_width() < 360.0 {
                    render_capability_item(ui, "Platform", platform_label());
                    render_capability_item(
                        ui,
                        "Display",
                        &format_refresh(self.display_refresh_millihz),
                    );
                    render_capability_item(ui, "Renderer", "Glow");
                    render_capability_item(ui, "Best Present", native_surface_summary(caps));
                    render_capability_item(ui, "Codecs", "h264 / hevc / av1");
                    render_capability_item(ui, "Audio", "opus stereo / 48 kHz");
                } else {
                    egui::Grid::new("client_capabilities_grid")
                        .num_columns(2)
                        .spacing([14.0, 8.0])
                        .show(ui, |ui| {
                            ui.label(capability_key("Platform"));
                            ui.label(egui::RichText::new(platform_label()).monospace());
                            ui.end_row();

                            ui.label(capability_key("Display"));
                            ui.label(
                                egui::RichText::new(format_refresh(self.display_refresh_millihz))
                                    .monospace(),
                            );
                            ui.end_row();

                            ui.label(capability_key("Renderer"));
                            ui.label(egui::RichText::new("Glow").monospace());
                            ui.end_row();

                            ui.label(capability_key("Best Present"));
                            ui.label(
                                egui::RichText::new(native_surface_summary(caps)).monospace(),
                            );
                            ui.end_row();

                            ui.label(capability_key("Codecs"));
                            ui.label(egui::RichText::new("h264 / hevc / av1").monospace());
                            ui.end_row();

                            ui.label(capability_key("Audio"));
                            ui.label(egui::RichText::new("opus stereo / 48 kHz").monospace());
                            ui.end_row();
                        });
                }
            });
    }

    fn render_floating_menu(&mut self, ctx: &egui::Context) -> f32 {
        self.local_overlay_hit_rects.clear();
        let recent_pointer_activity = self
            .last_pointer_move
            .map(|t| t.elapsed() < Duration::from_secs(3))
            .unwrap_or(false);
        let launcher_alpha = if self.menu_open {
            220
        } else if recent_pointer_activity {
            170
        } else {
            96
        };

        let button = egui::Area::new(egui::Id::new("floating_menu_button"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(12.0, 12.0))
            .show(ctx, |ui| {
                ui.add_sized(
                    [78.0, 30.0],
                    egui::Button::new(
                        egui::RichText::new("Menu")
                            .strong()
                            .color(egui::Color32::from_rgb(235, 238, 242)),
                    )
                    .fill(egui::Color32::from_rgba_unmultiplied(
                        18,
                        22,
                        27,
                        launcher_alpha,
                    ))
                    .stroke(egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 36),
                    )),
                )
            });
        let button_rect = button.inner.rect;
        self.local_overlay_hit_rects.push(button_rect);
        if button.inner.clicked() {
            self.menu_open = !self.menu_open;
            self.last_pointer_move = Some(Instant::now());
        }

        let mut overlay_top = button_rect.bottom() + 10.0;
        if self.menu_open {
            let mut request_disconnect = false;
            let mut audio_toggled = false;
            let mut debug_toggled = false;
            let menu = egui::Area::new(egui::Id::new("floating_menu_popup"))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(button_rect.left(), button_rect.bottom() + 8.0))
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(egui::Color32::from_rgba_unmultiplied(20, 24, 28, 232))
                        .show(ui, |ui| {
                            ui.set_min_width(190.0);
                            ui.vertical(|ui| {
                                if ui
                                    .add_sized([170.0, 30.0], egui::Button::new("Disconnect"))
                                    .clicked()
                                {
                                    request_disconnect = true;
                                }

                                let audio_label = if self.audio_enabled {
                                    "Audio: ON"
                                } else {
                                    "Audio: OFF"
                                };
                                if ui
                                    .add_sized([170.0, 30.0], egui::Button::new(audio_label))
                                    .clicked()
                                {
                                    audio_toggled = true;
                                }

                                let debug_label = if self.debug_enabled {
                                    "Debug: ON"
                                } else {
                                    "Debug: OFF"
                                };
                                if ui
                                    .add_sized([170.0, 30.0], egui::Button::new(debug_label))
                                    .clicked()
                                {
                                    debug_toggled = true;
                                }
                            });
                        });
                });
            let menu_rect = menu.response.rect;
            self.local_overlay_hit_rects.push(menu_rect);
            overlay_top = menu_rect.bottom() + 10.0;

            if audio_toggled {
                self.audio_enabled = !self.audio_enabled;
                save_audio_enabled(self.audio_enabled);
                self.audio_enabled_flag
                    .store(self.audio_enabled, Ordering::SeqCst);
            }
            if debug_toggled {
                self.debug_enabled = !self.debug_enabled;
                save_debug_enabled(self.debug_enabled);
                self.debug_enabled_flag
                    .store(self.debug_enabled, Ordering::SeqCst);
            }
            if request_disconnect {
                self.disconnect();
            }

            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.menu_open = false;
            }
            if ctx.input(|i| i.pointer.any_pressed()) {
                if let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) {
                    if !button_rect.contains(pos) && !menu_rect.contains(pos) {
                        self.menu_open = false;
                    }
                }
            }
        }

        overlay_top
    }
}

// ---------------------------------------------------------------------------
// Connection thread
// ---------------------------------------------------------------------------

struct MediaThreads {
    input_stop_tx: crossbeam_channel::Sender<()>,
    input_thread: std::thread::JoinHandle<()>,
    video_stop_tx: crossbeam_channel::Sender<()>,
    video_thread: std::thread::JoinHandle<()>,
    audio_stop_tx: crossbeam_channel::Sender<()>,
    audio_thread: std::thread::JoinHandle<()>,
    feedback_rx: crossbeam_channel::Receiver<TransportFeedback>,
}

fn start_media_threads(
    socket_addr: std::net::SocketAddr,
    frame_buf: Arc<Mutex<VideoFrameBuffer>>,
    debug_state: Arc<ConnectionDebugState>,
    debug_enabled: Arc<AtomicBool>,
    ctx: egui::Context,
    display_refresh_millihz: Option<u32>,
    audio_enabled: Arc<AtomicBool>,
    native_surfaces: Arc<NativeSurfaceControl>,
    control_tx: crossbeam_channel::Sender<ControlMessage>,
    input_rx: crossbeam_channel::Receiver<InputPacket>,
    stream_config: StreamConfig,
    udp_socket: UdpSocket,
) -> Result<MediaThreads, String> {
    let input_socket = udp_socket
        .try_clone()
        .map_err(|err| format!("Failed to clone UDP socket: {err}"))?;
    let input_target = std::net::SocketAddr::new(socket_addr.ip(), UDP_PORT);
    let (input_stop_tx, input_stop_rx) = crossbeam_channel::bounded::<()>(1);
    let input_thread = std::thread::spawn(move || {
        run_input_sender(input_socket, input_target, input_rx, input_stop_rx);
    });

    let (audio_data_tx, audio_data_rx) = crossbeam_channel::bounded::<AudioPacket>(60);
    let (feedback_tx, feedback_rx) = crossbeam_channel::bounded::<TransportFeedback>(8);
    let present_refresh_millihz =
        display::desired_present_refresh_millihz(display_refresh_millihz, stream_config.framerate);

    let (video_stop_tx, video_stop_rx) = crossbeam_channel::bounded(1);
    let pipeline_frame = Arc::clone(&frame_buf);
    let pipeline_debug_state = Arc::clone(&debug_state);
    let pipeline_ctx = ctx.clone();
    let pipeline_audio_flag = Arc::clone(&audio_enabled);
    let video_thread = std::thread::spawn(move || {
        pipeline::run_receive_pipeline(
            pipeline_frame,
            pipeline_debug_state,
            debug_enabled,
            pipeline_ctx,
            video_stop_rx,
            audio_data_tx,
            feedback_tx,
            pipeline_audio_flag,
            native_surfaces,
            control_tx,
            present_refresh_millihz,
            stream_config,
            udp_socket,
        );
    });

    let (audio_stop_tx, audio_stop_rx) = crossbeam_channel::bounded(1);
    let audio_thread = std::thread::spawn(move || {
        if let Err(e) = audio::run_audio_pipeline(audio_data_rx, audio_stop_rx) {
            eprintln!("[audio] {e}");
        }
    });

    Ok(MediaThreads {
        input_stop_tx,
        input_thread,
        video_stop_tx,
        video_thread,
        audio_stop_tx,
        audio_thread,
        feedback_rx,
    })
}

fn stop_media_threads(media_threads: MediaThreads) {
    let _ = media_threads.video_stop_tx.send(());
    let _ = media_threads.audio_stop_tx.send(());
    let _ = media_threads.input_stop_tx.send(());
    let _ = media_threads.video_thread.join();
    let _ = media_threads.audio_thread.join();
    let _ = media_threads.input_thread.join();
}

fn run_connection(
    addr: String,
    display_refresh_millihz: Option<u32>,
    state: Arc<Mutex<ConnectionState>>,
    frame_buf: Arc<Mutex<VideoFrameBuffer>>,
    debug_state: Arc<ConnectionDebugState>,
    disconnect: Arc<AtomicBool>,
    connection_epoch: Arc<AtomicU64>,
    session_epoch: u64,
    audio_enabled: Arc<AtomicBool>,
    debug_enabled: Arc<AtomicBool>,
    native_surfaces: Arc<NativeSurfaceControl>,
    shared_input: Arc<SharedInputState>,
    control_rx: crossbeam_channel::Receiver<ControlMessage>,
    input_rx: crossbeam_channel::Receiver<InputPacket>,
    ctx: egui::Context,
) {
    if session_cancelled(
        disconnect.as_ref(),
        connection_epoch.as_ref(),
        session_epoch,
    ) {
        return;
    }
    shared_input.reset();
    // Resolve address
    let socket_addr = match addr.to_socket_addrs().ok().and_then(|mut it| it.next()) {
        Some(a) => a,
        None => {
            set_error(
                &state,
                &ctx,
                &disconnect,
                &connection_epoch,
                session_epoch,
                format!("Cannot resolve: {addr}"),
            );
            return;
        }
    };

    if session_cancelled(
        disconnect.as_ref(),
        connection_epoch.as_ref(),
        session_epoch,
    ) {
        return;
    }

    // Connect TCP with timeout
    let mut tcp = match TcpStream::connect_timeout(&socket_addr, Duration::from_secs(5)) {
        Ok(s) => s,
        Err(e) => {
            set_error(
                &state,
                &ctx,
                &disconnect,
                &connection_epoch,
                session_epoch,
                format!("Connection failed: {e}"),
            );
            return;
        }
    };
    let _ = tcp.set_nodelay(true);

    if session_cancelled(
        disconnect.as_ref(),
        connection_epoch.as_ref(),
        session_epoch,
    ) {
        return;
    }

    tcp.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let udp_socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(socket) => {
            // Increase the receive buffer to handle bursts of video packets.
            // The default OS buffer (~208KB on Linux) can overflow during
            // initial connection when queued frames are sent rapidly.
            set_udp_recv_buffer(&socket, 4 * 1024 * 1024);
            socket
        }
        Err(err) => {
            set_error(
                &state,
                &ctx,
                &disconnect,
                &connection_epoch,
                session_epoch,
                format!("Failed to bind UDP receiver: {err}"),
            );
            return;
        }
    };
    let local_udp_port = udp_socket
        .local_addr()
        .ok()
        .map(|addr| addr.port())
        .unwrap_or(0);
    if trace_enabled() {
        eprintln!(
            "[trace][client] sending ClientDisplayInfo: refresh_millihz={} udp_port={local_udp_port}",
            display_refresh_millihz.unwrap_or(0)
        );
    }
    let _ = tcp.write_all(
        &ControlMessage::ClientDisplayInfo(ClientDisplayInfo {
            max_refresh_millihz: display_refresh_millihz.unwrap_or(0),
            udp_port: local_udp_port,
        })
        .serialize(),
    );

    // Wait for stream config, start the UDP media path, then wait for StreamStarted.
    let mut buf = [0u8; 1024];
    let mut control_buf = Vec::new();
    let mut stream_config: Option<StreamConfig> = None;
    let mut stream_started = false;
    let mut media_threads: Option<MediaThreads> = None;
    let mut udp_socket = Some(udp_socket);
    let mut input_rx = Some(input_rx);
    let (pipeline_control_tx, pipeline_control_rx) =
        crossbeam_channel::bounded::<ControlMessage>(8);
    loop {
        if session_cancelled(
            disconnect.as_ref(),
            connection_epoch.as_ref(),
            session_epoch,
        ) {
            if let Some(media_threads) = media_threads.take() {
                stop_media_threads(media_threads);
            }
            return;
        }
        match tcp.read(&mut buf) {
            Ok(0) => {
                if let Some(media_threads) = media_threads.take() {
                    stop_media_threads(media_threads);
                }
                set_error(
                    &state,
                    &ctx,
                    &disconnect,
                    &connection_epoch,
                    session_epoch,
                    "Server closed connection".into(),
                );
                return;
            }
            Ok(n) => {
                control_buf.extend_from_slice(&buf[..n]);
                for msg in drain_control_messages(&mut control_buf) {
                    match msg {
                        ControlMessage::StreamConfig(cfg) => {
                            if trace_enabled() {
                                eprintln!(
                                    "[trace][client] received StreamConfig: {:?} {}x{} {}fps audio={}ch/{}Hz hdr={}",
                                    cfg.codec,
                                    cfg.width,
                                    cfg.height,
                                    cfg.framerate,
                                    cfg.audio_channels,
                                    cfg.audio_sample_rate,
                                    cfg.hdr
                                );
                            }
                            debug_state.set_stream_config(cfg);
                            stream_config = Some(cfg);
                            if media_threads.is_none() {
                                let media = match start_media_threads(
                                    socket_addr,
                                    Arc::clone(&frame_buf),
                                    Arc::clone(&debug_state),
                                    Arc::clone(&debug_enabled),
                                    ctx.clone(),
                                    display_refresh_millihz,
                                    Arc::clone(&audio_enabled),
                                    Arc::clone(&native_surfaces),
                                    pipeline_control_tx.clone(),
                                    input_rx.take().expect("input receiver already taken"),
                                    cfg,
                                    udp_socket.take().expect("udp socket already taken"),
                                ) {
                                    Ok(media) => media,
                                    Err(err) => {
                                        set_error(
                                            &state,
                                            &ctx,
                                            &disconnect,
                                            &connection_epoch,
                                            session_epoch,
                                            err,
                                        );
                                        return;
                                    }
                                };
                                media_threads = Some(media);
                                if trace_enabled() {
                                    eprintln!(
                                        "[trace][client] media threads started on udp_port={local_udp_port}"
                                    );
                                }
                                if let Err(err) =
                                    tcp.write_all(&ControlMessage::ClientReadyForMedia.serialize())
                                {
                                    if let Some(media_threads) = media_threads.take() {
                                        stop_media_threads(media_threads);
                                    }
                                    set_error(
                                        &state,
                                        &ctx,
                                        &disconnect,
                                        &connection_epoch,
                                        session_epoch,
                                        format!("Failed to acknowledge media readiness: {err}"),
                                    );
                                    return;
                                }
                                if trace_enabled() {
                                    eprintln!("[trace][client] sent ClientReadyForMedia");
                                }
                            }
                        }
                        ControlMessage::SessionDebugInfo(info) => {
                            debug_state.set_session_info(info);
                        }
                        ControlMessage::ClockSyncPong(pong) => {
                            if debug_enabled.load(Ordering::Relaxed) {
                                debug_state.update_clock_sync(pong, unix_time_micros());
                            }
                        }
                        ControlMessage::InputSession(session) => {
                            shared_input.set_client_id(session.client_id);
                        }
                        ControlMessage::ControllerState(controller_state) => {
                            shared_input.set_controller_state(controller_state);
                        }
                        ControlMessage::InputCapabilities(capabilities) => {
                            eprintln!(
                                "[cursor] capabilities: abs={} rel={} kbd={} separate_cursor={}",
                                capabilities.mouse_absolute,
                                capabilities.mouse_relative,
                                capabilities.keyboard,
                                capabilities.separate_cursor
                            );
                            shared_input.set_capabilities(capabilities);
                        }
                        ControlMessage::CursorShape(shape) => {
                            eprintln!(
                                "[cursor] shape: serial={} {}x{} hotspot=({},{}) rgba_len={}",
                                shape.serial,
                                shape.width,
                                shape.height,
                                shape.hotspot_x,
                                shape.hotspot_y,
                                shape.rgba.len()
                            );
                            shared_input.set_cursor_shape(shape);
                            ctx.request_repaint();
                        }
                        ControlMessage::CursorState(cursor_state) => {
                            shared_input.set_cursor_state(cursor_state);
                            ctx.request_repaint();
                        }
                        ControlMessage::StreamStarted => {
                            if trace_enabled() {
                                eprintln!("[trace][client] received StreamStarted");
                            }
                            if stream_config.is_none() {
                                set_error(
                                    &state,
                                    &ctx,
                                    &disconnect,
                                    &connection_epoch,
                                    session_epoch,
                                    "Server started stream without configuration".into(),
                                );
                                return;
                            }
                            stream_started = true;
                        }
                        ControlMessage::Error(msg) => {
                            if let Some(media_threads) = media_threads.take() {
                                stop_media_threads(media_threads);
                            }
                            set_error(
                                &state,
                                &ctx,
                                &disconnect,
                                &connection_epoch,
                                session_epoch,
                                format!("Server error: {msg}"),
                            );
                            return;
                        }
                        ControlMessage::Shutdown => {
                            if let Some(media_threads) = media_threads.take() {
                                stop_media_threads(media_threads);
                            }
                            set_error(
                                &state,
                                &ctx,
                                &disconnect,
                                &connection_epoch,
                                session_epoch,
                                "Server shutting down".into(),
                            );
                            return;
                        }
                        ControlMessage::SetAudio(_)
                        | ControlMessage::ClientDisplayInfo(_)
                        | ControlMessage::ClientReadyForMedia
                        | ControlMessage::ClockSyncPing(_)
                        | ControlMessage::TransportFeedback(_)
                        | ControlMessage::AcquireControl
                        | ControlMessage::ReleaseControl
                        | ControlMessage::RequestKeyframe => {}
                    }
                }
                if stream_started && media_threads.is_some() {
                    break;
                }
            }
            Err(ref e) if is_timeout(e) => continue,
            Err(e) => {
                if let Some(media_threads) = media_threads.take() {
                    stop_media_threads(media_threads);
                }
                set_error(
                    &state,
                    &ctx,
                    &disconnect,
                    &connection_epoch,
                    session_epoch,
                    format!("Read error: {e}"),
                );
                return;
            }
        }
    }
    let stream_config = stream_config.unwrap();
    let media_threads = media_threads.expect("media threads not started");
    let feedback_rx = media_threads.feedback_rx.clone();

    // Connected!
    {
        let mut s = state.lock().unwrap();
        if session_cancelled(
            disconnect.as_ref(),
            connection_epoch.as_ref(),
            session_epoch,
        ) {
            stop_media_threads(media_threads);
            return;
        }
        *s = ConnectionState::Connected;
    }
    if trace_enabled() {
        eprintln!("[trace][client] connection state -> Connected");
    }
    ctx.request_repaint();

    // Tell server our audio preference
    let initial_audio =
        audio_enabled.load(Ordering::SeqCst) && stream_supports_client_audio(&stream_config);
    audio_enabled.store(initial_audio, Ordering::SeqCst);
    let _ = tcp.write_all(&ControlMessage::SetAudio(initial_audio).serialize());

    // TCP control loop — check for server messages, disconnect flag, and audio toggle
    control_buf.clear();
    let mut last_audio_state = initial_audio;
    let mut last_debug_enabled = debug_enabled.load(Ordering::SeqCst);
    let mut next_clock_ping = Instant::now();
    loop {
        if session_cancelled(
            disconnect.as_ref(),
            connection_epoch.as_ref(),
            session_epoch,
        ) {
            break;
        }

        // Detect audio toggle from UI and notify server
        let current_audio = audio_enabled.load(Ordering::SeqCst);
        if current_audio != last_audio_state {
            last_audio_state = current_audio;
            let _ = tcp.write_all(&ControlMessage::SetAudio(current_audio).serialize());
        }
        let current_debug_enabled = debug_enabled.load(Ordering::SeqCst);
        if current_debug_enabled && !last_debug_enabled {
            next_clock_ping = Instant::now();
        }
        last_debug_enabled = current_debug_enabled;
        while let Ok(msg) = control_rx.try_recv() {
            let _ = tcp.write_all(&msg.serialize());
        }
        while let Ok(msg) = pipeline_control_rx.try_recv() {
            let _ = tcp.write_all(&msg.serialize());
        }
        while let Ok(feedback) = feedback_rx.try_recv() {
            if tcp
                .write_all(&ControlMessage::TransportFeedback(feedback).serialize())
                .is_err()
            {
                break;
            }
        }
        if current_debug_enabled && Instant::now() >= next_clock_ping {
            let ping = ControlMessage::ClockSyncPing(ClockSyncPing {
                client_send_micros: unix_time_micros(),
            });
            let _ = tcp.write_all(&ping.serialize());
            next_clock_ping = Instant::now() + Duration::from_secs(2);
        }

        match tcp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut should_break = false;
                control_buf.extend_from_slice(&buf[..n]);
                for msg in drain_control_messages(&mut control_buf) {
                    match msg {
                        ControlMessage::StreamConfig(cfg) => {
                            debug_state.set_stream_config(cfg);
                        }
                        ControlMessage::SessionDebugInfo(info) => {
                            debug_state.set_session_info(info);
                        }
                        ControlMessage::ClockSyncPong(pong) => {
                            if current_debug_enabled {
                                debug_state.update_clock_sync(pong, unix_time_micros());
                            }
                        }
                        ControlMessage::InputSession(session) => {
                            shared_input.set_client_id(session.client_id);
                        }
                        ControlMessage::ControllerState(controller_state) => {
                            shared_input.set_controller_state(controller_state);
                        }
                        ControlMessage::InputCapabilities(capabilities) => {
                            eprintln!(
                                "[cursor] capabilities: abs={} rel={} kbd={} separate_cursor={}",
                                capabilities.mouse_absolute,
                                capabilities.mouse_relative,
                                capabilities.keyboard,
                                capabilities.separate_cursor
                            );
                            shared_input.set_capabilities(capabilities);
                        }
                        ControlMessage::CursorShape(shape) => {
                            eprintln!(
                                "[cursor] shape: serial={} {}x{} hotspot=({},{}) rgba_len={}",
                                shape.serial,
                                shape.width,
                                shape.height,
                                shape.hotspot_x,
                                shape.hotspot_y,
                                shape.rgba.len()
                            );
                            shared_input.set_cursor_shape(shape);
                            ctx.request_repaint();
                        }
                        ControlMessage::CursorState(cursor_state) => {
                            shared_input.set_cursor_state(cursor_state);
                            ctx.request_repaint();
                        }
                        ControlMessage::Error(err) => {
                            set_error(
                                &state,
                                &ctx,
                                &disconnect,
                                &connection_epoch,
                                session_epoch,
                                format!("Server error: {err}"),
                            );
                            should_break = true;
                        }
                        ControlMessage::Shutdown => {
                            set_error(
                                &state,
                                &ctx,
                                &disconnect,
                                &connection_epoch,
                                session_epoch,
                                "Server shut down".into(),
                            );
                            should_break = true;
                        }
                        ControlMessage::SetAudio(_)
                        | ControlMessage::ClientDisplayInfo(_)
                        | ControlMessage::ClientReadyForMedia
                        | ControlMessage::ClockSyncPing(_)
                        | ControlMessage::TransportFeedback(_)
                        | ControlMessage::AcquireControl
                        | ControlMessage::ReleaseControl
                        | ControlMessage::RequestKeyframe => {}
                        ControlMessage::StreamStarted => {}
                    }
                }
                if should_break {
                    break;
                }
            }
            Err(ref e) if is_timeout(e) => continue,
            Err(_) => break,
        }
    }

    // Cleanup
    stop_media_threads(media_threads);

    if !session_is_current(connection_epoch.as_ref(), session_epoch) {
        return;
    }

    {
        let mut fb = frame_buf.lock().unwrap();
        fb.clear();
    }
    shared_input.reset();

    {
        let mut s = state.lock().unwrap();
        if !matches!(
            *s,
            ConnectionState::Error(_) | ConnectionState::Disconnected
        ) {
            *s = ConnectionState::Disconnected;
        }
    }
    ctx.request_repaint();
}

fn set_error(
    state: &Arc<Mutex<ConnectionState>>,
    ctx: &egui::Context,
    disconnect: &Arc<AtomicBool>,
    connection_epoch: &Arc<AtomicU64>,
    session_epoch: u64,
    msg: String,
) {
    if !session_cancelled(
        disconnect.as_ref(),
        connection_epoch.as_ref(),
        session_epoch,
    ) {
        *state.lock().unwrap() = ConnectionState::Error(msg);
        ctx.request_repaint();
    }
}

fn session_is_current(connection_epoch: &AtomicU64, session_epoch: u64) -> bool {
    connection_epoch.load(Ordering::SeqCst) == session_epoch
}

fn session_cancelled(
    disconnect: &AtomicBool,
    connection_epoch: &AtomicU64,
    session_epoch: u64,
) -> bool {
    disconnect.load(Ordering::SeqCst) || !session_is_current(connection_epoch, session_epoch)
}

fn is_timeout(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut
}

fn drain_control_messages(buf: &mut Vec<u8>) -> Vec<ControlMessage> {
    let mut messages = Vec::new();
    let mut consumed = 0usize;

    while consumed < buf.len() {
        match ControlMessage::deserialize(&buf[consumed..]) {
            Some((msg, used)) => {
                messages.push(msg);
                consumed += used;
            }
            None => break,
        }
    }

    if consumed > 0 {
        buf.drain(..consumed);
    }

    messages
}

fn run_input_sender(
    socket: UdpSocket,
    target: std::net::SocketAddr,
    input_rx: crossbeam_channel::Receiver<InputPacket>,
    shutdown_rx: crossbeam_channel::Receiver<()>,
) {
    let mut seq = 0u16;
    let _ = socket.set_write_timeout(Some(Duration::from_millis(50)));
    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        match input_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(packet) => {
                let raw = packet.serialize(seq);
                seq = seq.wrapping_add(1);
                let _ = socket.send_to(&raw, target);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(unix)]
fn set_udp_recv_buffer(socket: &UdpSocket, size: i32) {
    use std::os::unix::io::AsRawFd;
    let fd = socket.as_raw_fd();
    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &size as *const i32 as *const _,
            std::mem::size_of::<i32>() as libc::socklen_t,
        )
    };
    if ret != 0 {
        eprintln!(
            "[udp] setsockopt SO_RCVBUF failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
fn set_udp_recv_buffer(_socket: &UdpSocket, _size: i32) {}

fn stream_supports_client_audio(stream_config: &StreamConfig) -> bool {
    stream_config.audio_sample_rate == 48_000 && stream_config.audio_channels == 2
}

fn format_refresh(refresh_millihz: Option<u32>) -> String {
    refresh_millihz
        .map(|value| format!("{:.2} Hz", value as f32 / 1000.0))
        .unwrap_or_else(|| "-".to_string())
}

fn format_opt_ms(value: Option<f32>) -> String {
    value
        .map(|v| format!("{v:.1} ms"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_opt_kbps(value: Option<u32>) -> String {
    value
        .map(|v| format!("{v} kbps"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_pos(pos: egui::Pos2) -> String {
    format!("({:.1}, {:.1})", pos.x, pos.y)
}

fn format_pos_opt(pos: Option<egui::Pos2>) -> String {
    pos.map(format_pos).unwrap_or_else(|| "-".to_string())
}

fn format_vec2(value: egui::Vec2) -> String {
    format!("({:.1}, {:.1})", value.x, value.y)
}

fn format_rect(rect: egui::Rect) -> String {
    format!(
        "min={} size=({:.1}, {:.1})",
        format_pos(rect.min),
        rect.width(),
        rect.height()
    )
}

fn format_rect_opt(rect: Option<egui::Rect>) -> String {
    rect.map(format_rect).unwrap_or_else(|| "-".to_string())
}

fn codec_label(stream_config: &StreamConfig) -> &'static str {
    match stream_config.codec {
        st_protocol::VideoCodec::H264 => "h264",
        st_protocol::VideoCodec::Hevc => "hevc",
        st_protocol::VideoCodec::Av1 => "av1",
    }
}

fn normalized_coord(value: f32, min: f32, max: f32) -> u16 {
    let span = (max - min).max(1.0);
    let normalized = ((value - min) / span).clamp(0.0, 1.0);
    (normalized * 65535.0).round() as u16
}

fn pointer_button_mask(button: egui::PointerButton) -> u8 {
    match button {
        egui::PointerButton::Primary => MOUSE_BUTTON_PRIMARY,
        egui::PointerButton::Secondary => MOUSE_BUTTON_SECONDARY,
        egui::PointerButton::Middle => MOUSE_BUTTON_MIDDLE,
        egui::PointerButton::Extra1 => MOUSE_BUTTON_EXTRA1,
        egui::PointerButton::Extra2 => MOUSE_BUTTON_EXTRA2,
    }
}

fn quantize_wheel(delta: egui::Vec2, unit: egui::MouseWheelUnit) -> (i16, i16) {
    let scale = match unit {
        egui::MouseWheelUnit::Point => 1.0 / 40.0,
        egui::MouseWheelUnit::Line => 1.0,
        egui::MouseWheelUnit::Page => 6.0,
    };
    let dx = (delta.x * scale)
        .round()
        .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    let dy = (delta.y * scale)
        .round()
        .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    (dx, dy)
}

fn egui_key_to_remote_key(key: egui::Key) -> Option<KeyboardKey> {
    use egui::Key;
    use KeyboardKey as Remote;

    Some(match key {
        Key::Escape => Remote::Escape,
        Key::Tab => Remote::Tab,
        Key::Backspace => Remote::Backspace,
        Key::Enter => Remote::Enter,
        Key::Space => Remote::Space,
        Key::Insert => Remote::Insert,
        Key::Delete => Remote::Delete,
        Key::Home => Remote::Home,
        Key::End => Remote::End,
        Key::PageUp => Remote::PageUp,
        Key::PageDown => Remote::PageDown,
        Key::ArrowUp => Remote::ArrowUp,
        Key::ArrowDown => Remote::ArrowDown,
        Key::ArrowLeft => Remote::ArrowLeft,
        Key::ArrowRight => Remote::ArrowRight,
        Key::Minus => Remote::Minus,
        Key::Equals | Key::Plus => Remote::Equals,
        Key::OpenBracket | Key::OpenCurlyBracket => Remote::OpenBracket,
        Key::CloseBracket | Key::CloseCurlyBracket => Remote::CloseBracket,
        Key::Backslash | Key::Pipe => Remote::Backslash,
        Key::Semicolon | Key::Colon => Remote::Semicolon,
        Key::Quote => Remote::Quote,
        Key::Backtick | Key::Exclamationmark => Remote::Backtick,
        Key::Comma => Remote::Comma,
        Key::Period => Remote::Period,
        Key::Slash | Key::Questionmark => Remote::Slash,
        Key::Num0 => Remote::Num0,
        Key::Num1 => Remote::Num1,
        Key::Num2 => Remote::Num2,
        Key::Num3 => Remote::Num3,
        Key::Num4 => Remote::Num4,
        Key::Num5 => Remote::Num5,
        Key::Num6 => Remote::Num6,
        Key::Num7 => Remote::Num7,
        Key::Num8 => Remote::Num8,
        Key::Num9 => Remote::Num9,
        Key::A => Remote::A,
        Key::B => Remote::B,
        Key::C => Remote::C,
        Key::D => Remote::D,
        Key::E => Remote::E,
        Key::F => Remote::F,
        Key::G => Remote::G,
        Key::H => Remote::H,
        Key::I => Remote::I,
        Key::J => Remote::J,
        Key::K => Remote::K,
        Key::L => Remote::L,
        Key::M => Remote::M,
        Key::N => Remote::N,
        Key::O => Remote::O,
        Key::P => Remote::P,
        Key::Q => Remote::Q,
        Key::R => Remote::R,
        Key::S => Remote::S,
        Key::T => Remote::T,
        Key::U => Remote::U,
        Key::V => Remote::V,
        Key::W => Remote::W,
        Key::X => Remote::X,
        Key::Y => Remote::Y,
        Key::Z => Remote::Z,
        Key::F1 => Remote::F1,
        Key::F2 => Remote::F2,
        Key::F3 => Remote::F3,
        Key::F4 => Remote::F4,
        Key::F5 => Remote::F5,
        Key::F6 => Remote::F6,
        Key::F7 => Remote::F7,
        Key::F8 => Remote::F8,
        Key::F9 => Remote::F9,
        Key::F10 => Remote::F10,
        Key::F11 => Remote::F11,
        Key::F12 => Remote::F12,
        _ => return None,
    })
}

fn render_setting_row(ui: &mut egui::Ui, title: &str, description: &str, enabled: bool) -> bool {
    let mut clicked = false;
    if ui.available_width() < 360.0 {
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(title)
                    .strong()
                    .color(egui::Color32::from_rgb(235, 239, 244)),
            );
            ui.label(
                egui::RichText::new(description)
                    .size(12.0)
                    .color(egui::Color32::from_rgb(134, 147, 160)),
            );
            ui.add_space(6.0);
            if render_setting_toggle(ui, enabled) {
                clicked = true;
            }
        });
    } else {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .strong()
                        .color(egui::Color32::from_rgb(235, 239, 244)),
                );
                ui.label(
                    egui::RichText::new(description)
                        .size(12.0)
                        .color(egui::Color32::from_rgb(134, 147, 160)),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if render_setting_toggle(ui, enabled) {
                    clicked = true;
                }
            });
        });
    }
    clicked
}

fn render_setting_toggle(ui: &mut egui::Ui, enabled: bool) -> bool {
    let (label, fill) = if enabled {
        (
            "Enabled",
            egui::Color32::from_rgba_unmultiplied(38, 146, 108, 220),
        )
    } else {
        (
            "Disabled",
            egui::Color32::from_rgba_unmultiplied(69, 81, 94, 220),
        )
    };
    ui.add_sized(
        [94.0, 28.0],
        egui::Button::new(
            egui::RichText::new(label)
                .strong()
                .color(egui::Color32::from_rgb(243, 247, 250)),
        )
        .fill(fill),
    )
    .clicked()
}

fn render_capability_item(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(capability_key(label));
    ui.label(
        egui::RichText::new(value)
            .monospace()
            .color(egui::Color32::from_rgb(228, 234, 240)),
    );
    ui.add_space(6.0);
}

fn capability_key(label: &str) -> egui::RichText {
    egui::RichText::new(label).color(egui::Color32::from_rgb(150, 163, 176))
}

fn render_client_summary_panel(
    ui: &mut egui::Ui,
    caps: NativeSurfaceCapabilities,
    display_refresh_millihz: Option<u32>,
) {
    egui::Frame::popup(ui.style())
        .fill(egui::Color32::from_rgba_unmultiplied(11, 15, 20, 220))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new("Client")
                    .strong()
                    .color(egui::Color32::from_rgb(226, 232, 240)),
            );
            ui.add_space(6.0);
            ui.label(egui::RichText::new(format!("platform: {}", platform_label())).monospace());
            ui.label(
                egui::RichText::new(format!(
                    "display: {}",
                    format_refresh(display_refresh_millihz)
                ))
                .monospace(),
            );
            ui.label(
                egui::RichText::new(format!("present: {}", native_surface_summary(caps)))
                    .monospace(),
            );
        });
}

fn platform_label() -> &'static str {
    if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Unknown"
    }
}

fn native_surface_summary(caps: NativeSurfaceCapabilities) -> &'static str {
    if caps.linux_dmabuf {
        "dmabuf / egl"
    } else if caps.macos_videotoolbox {
        "videotoolbox / iosurface"
    } else if caps.windows_d3d11 {
        "d3d11 interop"
    } else {
        "cpu upload fallback"
    }
}

fn render_debug_overlay(
    ctx: &egui::Context,
    snapshot: &ConnectionDebugSnapshot,
    input_snapshot: &input::SharedInputSnapshot,
    capture_mode: LocalCaptureMode,
    audio_enabled: bool,
    pointer_buttons: u8,
    pressed_keys: usize,
    top_offset: f32,
    debug_tab: &mut DebugOverlayTab,
    cursor_lines: &[String],
) {
    let stream_line = snapshot
        .stream_config
        .as_ref()
        .map(|cfg| {
            format!(
                "{} {}x{} {} fps hdr={} audio={}ch/{}Hz",
                codec_label(cfg),
                cfg.width,
                cfg.height,
                cfg.framerate,
                cfg.hdr,
                cfg.audio_channels,
                cfg.audio_sample_rate
            )
        })
        .unwrap_or_else(|| "-".to_string());

    let packet_total = snapshot
        .received_packets
        .saturating_add(snapshot.lost_packets);
    let loss_pct = if packet_total > 0 {
        snapshot.lost_packets as f32 * 100.0 / packet_total as f32
    } else {
        0.0
    };

    let general_lines = vec![
        format!("server: {}", snapshot.server_addr),
        format!("stream: {stream_line}"),
        format!(
            "display: {}  audio={}  decoder={}  encoder={}  capture={}  input={}",
            format_refresh(snapshot.display_refresh_millihz),
            if audio_enabled { "on" } else { "off" },
            if snapshot.decoder_name.is_empty() {
                "-"
            } else {
                snapshot.decoder_name.as_str()
            },
            if snapshot.encoder_name.is_empty() {
                "-"
            } else {
                snapshot.encoder_name.as_str()
            },
            if snapshot.capture_backend.is_empty() {
                "-"
            } else {
                snapshot.capture_backend.as_str()
            },
            if snapshot.input_backend.is_empty() {
                "-"
            } else {
                snapshot.input_backend.as_str()
            }
        ),
        format!(
            "input: controller={:?} mode={} client_id={} keys={} buttons=0x{pointer_buttons:02x} caps=abs:{} rel:{} kb:{} cursor:{}",
            input_snapshot.controller_state,
            capture_mode.label(),
            input_snapshot
                .client_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string()),
            pressed_keys,
            if input_snapshot.capabilities.mouse_absolute {
                "y"
            } else {
                "n"
            },
            if input_snapshot.capabilities.mouse_relative {
                "y"
            } else {
                "n"
            },
            if input_snapshot.capabilities.keyboard {
                "y"
            } else {
                "n"
            },
            if input_snapshot.capabilities.separate_cursor {
                "y"
            } else {
                "n"
            }
        ),
        format!(
            "bitrate: target={}  rx={:.0} kbps (video {:.0} / audio {:.0})",
            format_opt_kbps(snapshot.target_bitrate_kbps),
            snapshot.received_total_kbps,
            snapshot.received_video_kbps,
            snapshot.received_audio_kbps
        ),
        format!(
            "fps: stream={}  rx={:.1}  decode={:.1}  present={:.1}",
            snapshot
                .stream_config
                .as_ref()
                .map(|cfg| cfg.framerate.to_string())
                .unwrap_or_else(|| "-".to_string()),
            snapshot.receive_fps,
            snapshot.decode_fps,
            snapshot.present_fps
        ),
        format!(
            "udp: packets={} lost={} ({loss_pct:.1}%) late={} dropped_frames={} window={} ms",
            snapshot.received_packets,
            snapshot.lost_packets,
            snapshot.late_packets,
            snapshot.dropped_frames,
            snapshot.transport_interval_ms
        ),
        format!(
            "latency: total={}  capture->send={}  send->assemble={}",
            format_opt_ms(snapshot.total_latency_ms),
            format_opt_ms(snapshot.capture_to_send_ms),
            format_opt_ms(snapshot.send_to_assemble_ms)
        ),
        format!(
            "latency: assemble->decode={}  decode->present={}  clock_rtt={}  clock_offset={}",
            format_opt_ms(snapshot.assemble_to_decode_ms),
            format_opt_ms(snapshot.decode_to_present_ms),
            format_opt_ms(snapshot.clock_rtt_ms),
            format_opt_ms(snapshot.server_clock_ahead_ms)
        ),
        format!(
            "frame: id={} format={} present_path={} decode_work={}",
            snapshot
                .last_frame_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string()),
            if snapshot.last_video_format.is_empty() {
                "-"
            } else {
                snapshot.last_video_format.as_str()
            },
            if snapshot.last_present_path.is_empty() {
                "-"
            } else {
                snapshot.last_present_path.as_str()
            },
            format_opt_ms(snapshot.decode_work_ms)
        ),
    ];

    let max_width = (ctx.content_rect().width() - 20.0).clamp(260.0, 560.0);

    egui::Area::new(egui::Id::new("debug_overlay"))
        .fixed_pos(egui::pos2(10.0, top_offset))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .fill(egui::Color32::from_rgba_unmultiplied(20, 20, 20, 220))
                .show(ui, |ui| {
                    ui.set_width(max_width);
                    ui.horizontal(|ui| {
                        ui.selectable_value(debug_tab, DebugOverlayTab::General, "General");
                        ui.selectable_value(debug_tab, DebugOverlayTab::Cursor, "Cursor");
                    });
                    ui.separator();
                    ui.vertical(|ui| {
                        let lines: &[String] = match *debug_tab {
                            DebugOverlayTab::General => &general_lines,
                            DebugOverlayTab::Cursor => cursor_lines,
                        };
                        for line in lines {
                            ui.monospace(line);
                        }
                    });
                });
        });
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

impl eframe::App for StreamApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let state = self.state.lock().unwrap().clone();
        if matches!(
            state,
            ConnectionState::Disconnected | ConnectionState::Error(_)
        ) {
            self.clear_local_session_interaction();
            self.video_texture.clear_frame();
        }
        self.apply_pointer_capture_mode(ctx);
        self.sync_remote_cursor_texture(ctx);
        let recent_pointer_activity = self
            .last_pointer_move
            .map(|t| t.elapsed() < Duration::from_secs(3))
            .unwrap_or(false);
        if state == ConnectionState::Connected
            && (self.debug_enabled || self.menu_open || recent_pointer_activity)
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        // Track pointer movement so the floating launcher can briefly become more visible.
        if ctx.input(|i| i.pointer.velocity().length() > 2.0) {
            self.last_pointer_move = Some(Instant::now());
        }

        // Upload the latest frame directly into a native GL texture.
        let upload_error = {
            let mut fb = self.frame.lock().unwrap();
            if fb.dirty && fb.width > 0 {
                fb.dirty = false;
                if self.video_texture.stage_direct_frame(&fb) {
                    if self.debug_enabled {
                        self.debug_state.record_present(&fb, unix_time_micros());
                    }
                    None
                } else {
                    match self
                        .video_texture
                        .upload(frame, &fb, self.native_surfaces.as_ref())
                    {
                        Ok(()) => {
                            if self.debug_enabled {
                                self.debug_state.record_present(&fb, unix_time_micros());
                            }
                            None
                        }
                        Err(err) => Some(err),
                    }
                }
            } else {
                None
            }
        };
        if let Some(err) = upload_error {
            self.video_texture.clear_frame();
            *self.state.lock().unwrap() =
                ConnectionState::Error(format!("GL upload failed: {err}"));
            ctx.request_repaint();
        }

        let central_panel = if state == ConnectionState::Connected {
            egui::CentralPanel::default().frame(egui::Frame::NONE)
        } else {
            egui::CentralPanel::default()
        };

        central_panel.show(ctx, |ui| match &state {
            ConnectionState::Disconnected => {
                self.render_home_screen(ui, ctx);
            }
            ConnectionState::Connecting => {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.spinner();
                    ui.label("Connecting...");
                    ui.add_space(10.0);
                    if ui.button("Cancel").clicked() {
                        self.disconnect();
                    }
                });
            }
            ConnectionState::Connected => {
                if self.video_texture.has_frame() {
                    let available = ui.available_size();
                    let tex_size = self.video_texture.size_vec2();
                    let scale = (available.x / tex_size.x).min(available.y / tex_size.y);
                    let sized = egui::Vec2::new(tex_size.x * scale, tex_size.y * scale);
                    ui.centered_and_justified(|ui| {
                        let (rect, response) = ui.allocate_exact_size(sized, egui::Sense::click());
                        let painted_direct = self
                            .video_texture
                            .paint_direct_if_available(frame, ui, rect);

                        if !painted_direct {
                            if let Some(texture_id) = self.video_texture.texture_id() {
                                ui.painter().image(
                                    texture_id,
                                    rect,
                                    egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    egui::Color32::WHITE,
                                );
                            }
                        }
                        self.handle_connected_video_response(ctx, &response, tex_size);
                    });
                } else {
                    self.last_video_rect = None;
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() / 3.0);
                        ui.spinner();
                        ui.label("Waiting for video...");
                    });
                }
            }
            ConnectionState::Error(msg) => {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.colored_label(egui::Color32::RED, msg);
                    ui.add_space(10.0);
                    if ui.button("Retry").clicked() {
                        self.video_texture.clear_frame();
                        *self.state.lock().unwrap() = ConnectionState::Disconnected;
                    }
                });
            }
        });

        if state == ConnectionState::Connected {
            let debug_top = self.render_floating_menu(ctx);

            if self.debug_enabled {
                let snapshot = self.debug_state.snapshot();
                let input_snapshot = self.shared_input.snapshot();
                let cursor_lines = self.cursor_debug_lines(ctx, &input_snapshot);
                render_debug_overlay(
                    ctx,
                    &snapshot,
                    &input_snapshot,
                    self.capture_mode,
                    self.audio_enabled,
                    self.pointer_buttons,
                    self.keyboard_state.pressed_count(),
                    debug_top,
                    &mut self.debug_overlay_tab,
                    &cursor_lines,
                );
            }
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if *self.state.lock().unwrap() == ConnectionState::Connected {
            [0.0, 0.0, 0.0, 0.0]
        } else {
            #[cfg(target_os = "macos")]
            {
                return egui::Color32::from_rgb(12, 12, 12).to_normalized_gamma_f32();
            }
            egui::Color32::from_rgba_unmultiplied(12, 12, 12, 180).to_normalized_gamma_f32()
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if *self.state.lock().unwrap() != ConnectionState::Connected {
            return;
        }

        let force_release = raw_input.events.iter().any(|event| match event {
            egui::Event::Key {
                key: egui::Key::Escape,
                pressed: true,
                ..
            } => {
                raw_input.modifiers.ctrl || cfg!(target_os = "macos") && raw_input.modifiers.mac_cmd
            }
            _ => false,
        });
        if force_release {
            if self.capture_mode != LocalCaptureMode::Idle {
                self.force_release_capture();
            }
            raw_input.events.retain(|event| {
                !matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::Escape,
                        pressed: true,
                        ..
                    }
                )
            });
        }

        let snapshot = self.shared_input.snapshot();
        let Some(client_id) = snapshot.client_id else {
            return;
        };
        let keyboard_forward_active = self.keyboard_forward_active(&snapshot);
        if !keyboard_forward_active {
            self.clear_remote_keyboard();
        }

        let video_rect = self.last_video_rect;
        let mut last_pointer_pos = raw_input.events.iter().rev().find_map(|event| match event {
            egui::Event::PointerMoved(pos) => Some(*pos),
            _ => None,
        });
        let mut keyboard_dirty = false;

        if keyboard_forward_active {
            keyboard_dirty |= self.keyboard_state.sync_modifiers(raw_input.modifiers);
        }

        for event in &raw_input.events {
            match *event {
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    ..
                } => {
                    if !keyboard_forward_active {
                        continue;
                    }
                    let source_key = physical_key.unwrap_or(key);
                    if let Some(remote_key) = egui_key_to_remote_key(source_key) {
                        keyboard_dirty |= self.keyboard_state.set_key(remote_key, pressed);
                    }
                }
                egui::Event::PointerMoved(pos) => {
                    last_pointer_pos = Some(pos);
                    if self.capture_mode == LocalCaptureMode::HoverAbsolute
                        && snapshot.controller_state == ControllerState::OwnedByYou
                        && self.capture_mode != LocalCaptureMode::ForceReleased
                    {
                        if let Some(rect) = video_rect {
                            if rect.contains(pos) && !self.pointer_over_local_overlay(pos) {
                                self.send_input_packet(InputPacket::MouseAbsolute(
                                    MouseAbsoluteInput {
                                        client_id,
                                        x: normalized_coord(pos.x, rect.left(), rect.right()),
                                        y: normalized_coord(pos.y, rect.top(), rect.bottom()),
                                        buttons: self.pointer_buttons,
                                    },
                                ));
                            }
                        }
                    }
                }
                egui::Event::MouseMoved(delta) => {
                    if self.capture_mode == LocalCaptureMode::CapturedRelative
                        && snapshot.controller_state == ControllerState::OwnedByYou
                    {
                        let dx = delta.x.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                        let dy = delta.y.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                        if dx != 0 || dy != 0 {
                            self.send_input_packet(InputPacket::MouseRelative(
                                MouseRelativeInput {
                                    client_id,
                                    dx,
                                    dy,
                                    buttons: self.pointer_buttons,
                                },
                            ));
                        }
                    }
                }
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    let mask = pointer_button_mask(button);
                    if mask == 0 {
                        continue;
                    }
                    if pressed {
                        self.pointer_buttons |= mask;
                    } else {
                        self.pointer_buttons &= !mask;
                    }
                    let over_video = video_rect
                        .map(|rect| rect.contains(pos) && !self.pointer_over_local_overlay(pos))
                        .unwrap_or(false);
                    if snapshot.controller_state == ControllerState::OwnedByYou
                        && (self.capture_mode == LocalCaptureMode::CapturedRelative
                            || self.capture_mode == LocalCaptureMode::HoverAbsolute && over_video)
                    {
                        self.send_input_packet(InputPacket::MouseButtons(MouseButtonsInput {
                            client_id,
                            buttons: self.pointer_buttons,
                        }));
                    }
                }
                egui::Event::MouseWheel { delta, unit, .. } => {
                    if snapshot.controller_state != ControllerState::OwnedByYou {
                        continue;
                    }
                    let over_video = last_pointer_pos
                        .and_then(|pos| {
                            video_rect.map(|rect| {
                                rect.contains(pos) && !self.pointer_over_local_overlay(pos)
                            })
                        })
                        .unwrap_or(false);
                    if self.capture_mode != LocalCaptureMode::CapturedRelative
                        && !(self.capture_mode == LocalCaptureMode::HoverAbsolute && over_video)
                    {
                        continue;
                    }
                    let (delta_x, delta_y) = quantize_wheel(delta, unit);
                    if delta_x != 0 || delta_y != 0 {
                        self.send_input_packet(InputPacket::MouseWheel(MouseWheelInput {
                            client_id,
                            delta_x,
                            delta_y,
                            buttons: self.pointer_buttons,
                        }));
                    }
                }
                _ => {}
            }
        }

        if keyboard_dirty {
            self.send_keyboard_snapshot(client_id);
        }

        if keyboard_forward_active {
            raw_input.events.retain(|event| {
                !matches!(
                    event,
                    egui::Event::Key { .. }
                        | egui::Event::Text(_)
                        | egui::Event::Copy
                        | egui::Event::Cut
                        | egui::Event::Paste(_)
                )
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    #[cfg(target_os = "macos")]
    let viewport = egui::ViewportBuilder::default()
        .with_title("Stream Client")
        .with_inner_size([1280.0, 720.0])
        .with_transparent(true);
    #[cfg(not(target_os = "macos"))]
    let viewport = egui::ViewportBuilder::default()
        .with_title("Stream Client")
        .with_inner_size([1280.0, 720.0]);

    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        vsync: display::vsync_enabled(),
        ..Default::default()
    };

    eframe::run_native(
        "Stream Client",
        options,
        Box::new(|cc| Ok(Box::new(StreamApp::new(cc)))),
    )
    .expect("Failed to run eframe");
}
