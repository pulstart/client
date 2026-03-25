#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod audio;
mod debug_state;
mod decode;
mod graph_overlay;
mod display;
mod input;
mod keep_awake;
mod pipeline;
mod render_gl;
#[cfg(target_os = "macos")]
mod render_macos;
#[cfg(target_os = "macos")]
mod render_macos_metal;
#[cfg(target_os = "windows")]
mod render_windows;
mod transport;
mod updater;
mod video_frame;

use eframe::egui;
use input::{LocalCaptureMode, LocalKeyboardState, RemoteCursorTexture, SharedInputState};
use keep_awake::KeepAwakeController;
use render_gl::NativeVideoTexture;
use serde::{Deserialize, Serialize};
use st_protocol::{
    ClientDisplayInfo, ClockSyncPing, ControlMessage, ControllerState, InputPacket, KeyboardKey,
    KeyboardStateInput, MouseAbsoluteInput, MouseButtonsInput, MouseRelativeInput, MouseWheelInput,
    StreamConfig, TransportFeedback, VideoCodec, VideoCodecSupport, MOUSE_BUTTON_EXTRA1,
    MOUSE_BUTTON_EXTRA2, MOUSE_BUTTON_MIDDLE, MOUSE_BUTTON_PRIMARY, MOUSE_BUTTON_SECONDARY,
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv6Addr, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use transport::AudioPacket;
use video_frame::{NativeSurfaceControl, VideoFrameBuffer};

use crate::debug_state::{unix_time_micros, ConnectionDebugSnapshot, ConnectionDebugState};

const DEFAULT_APP_PORT: u16 = 28_480;
const MAX_REMOTE_CURSOR_TEXTURES: usize = 8;
const INPUT_SENDER_POLL_INTERVAL: Duration = Duration::from_millis(20);
const INPUT_STATE_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(50);
const INPUT_STATE_REPAIR_WINDOW: Duration = Duration::from_millis(200);

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
enum HomeTab {
    Servers,
    Settings,
    Update,
    About,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DebugOverlayTab {
    General,
    Cursor,
}

#[derive(Clone, Debug)]
enum UpdateUiState {
    Unsupported(String),
    Idle,
    Checking,
    UpToDate { version: String, html_url: String },
    UpdateAvailable(updater::ReleaseInfo),
    Downloading { version: String },
    ClosingForUpdate { version: String },
    Error(String),
}

#[derive(Clone, Debug)]
enum UpdateWorkerEvent {
    CheckFinished(Result<updater::CheckOutcome, String>),
    InstallPrepared {
        version: String,
        result: Result<(), String>,
    },
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
    server_list: Vec<ServerEntry>,
    add_server_addr: String,
    search_query: String,
    home_tab: HomeTab,
    audio_enabled: bool,
    debug_enabled: bool,
    display_refresh_millihz: Option<u32>,
    video_codec_support: decode::VideoCodecSupportReport,
    audio_enabled_flag: Arc<AtomicBool>,
    debug_enabled_flag: Arc<AtomicBool>,
    state: Arc<Mutex<ConnectionState>>,
    frame: Arc<Mutex<VideoFrameBuffer>>,
    debug_state: Arc<ConnectionDebugState>,
    video_texture: NativeVideoTexture,
    keep_awake: KeepAwakeController,
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
    last_sent_absolute_cursor: Option<(u16, u16)>,
    hover_cursor_pos: Option<egui::Pos2>,
    resume_hover_after_relative_drag: bool,
    hover_cursor_resync_pending: bool,
    hover_drag_edge_mismatch_frames: u8,
    remote_cursor_textures: BTreeMap<u64, RemoteCursorTexture>,
    latest_remote_cursor_serial: Option<u64>,
    seen_cursor_shape_version: u64,
    debug_overlay_tab: DebugOverlayTab,
    graph_overlay: graph_overlay::GraphOverlay,
    menu_open: bool,
    menu_button_pos: egui::Pos2,
    menu_button_drag_origin: Option<egui::Pos2>,
    local_overlay_hit_rects: Vec<egui::Rect>,
    last_pointer_move: Option<Instant>,
    await_pointer_exit_after_auto_release: bool,
    applied_cursor_visible: Option<bool>,
    applied_cursor_grab: Option<egui::CursorGrab>,
    suppress_mouse_delta: bool,
    suppress_pointer_pos_frames: u8,
    excluded_video_codecs: Arc<Mutex<st_protocol::VideoCodecSupport>>,
    update_ui_state: UpdateUiState,
    update_tx: crossbeam_channel::Sender<UpdateWorkerEvent>,
    update_rx: crossbeam_channel::Receiver<UpdateWorkerEvent>,
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

const FLOATING_MENU_BUTTON_SIZE: f32 = 40.0;
const FLOATING_MENU_BUTTON_MARGIN: f32 = 12.0;

fn default_menu_button_pos() -> egui::Pos2 {
    egui::pos2(FLOATING_MENU_BUTTON_MARGIN, FLOATING_MENU_BUTTON_MARGIN)
}

fn default_server_addr() -> String {
    format!("127.0.0.1:{DEFAULT_APP_PORT}")
}

fn normalize_server_addr(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Some(host) = trimmed.strip_prefix('[').and_then(|value| value.strip_suffix(']')) {
        return format!("[{host}]:{DEFAULT_APP_PORT}");
    }

    if trimmed.starts_with('[') && trimmed.contains("]:") {
        return trimmed.to_string();
    }

    if trimmed.parse::<Ipv6Addr>().is_ok() {
        return format!("[{trimmed}]:{DEFAULT_APP_PORT}");
    }

    if let Some((host, port)) = trimmed.rsplit_once(':') {
        if !host.is_empty() && !host.contains(':') && port.parse::<u16>().is_ok() {
            return trimmed.to_string();
        }
    }

    format!("{trimmed}:{DEFAULT_APP_PORT}")
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

fn load_menu_button_pos() -> Option<egui::Pos2> {
    let text = std::fs::read_to_string(state_dir().join("menu_button_pos")).ok()?;
    let mut parts = text.split_whitespace();
    let x = parts.next()?.parse::<f32>().ok()?;
    let y = parts.next()?.parse::<f32>().ok()?;
    if x.is_finite() && y.is_finite() {
        Some(egui::pos2(x, y))
    } else {
        None
    }
}

fn save_menu_button_pos(pos: egui::Pos2) {
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("menu_button_pos"), format!("{:.1} {:.1}", pos.x, pos.y));
}

// ---------------------------------------------------------------------------
// Server list persistence
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ServerEntry {
    address: String,
    #[serde(default)]
    nickname: String,
    /// Unix timestamp (seconds) of last successful connection, 0 if never.
    #[serde(default)]
    last_connected: u64,
}

fn load_server_list() -> Vec<ServerEntry> {
    let path = state_dir().join("servers.json");
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn save_server_list(list: &[ServerEntry]) {
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(data) = serde_json::to_string_pretty(list) {
        let _ = std::fs::write(dir.join("servers.json"), data);
    }
}

/// Ensure the last-connected server is in the list and migrate the legacy
/// `last_server` file if the list is empty.
fn ensure_server_in_list(list: &mut Vec<ServerEntry>, addr: &str) {
    let normalized = normalize_server_addr(addr);
    if normalized.is_empty() {
        return;
    }
    if !list.iter().any(|s| s.address == normalized) {
        list.push(ServerEntry {
            address: normalized,
            nickname: String::new(),
            last_connected: 0,
        });
    }
}

fn touch_server_connected(list: &mut Vec<ServerEntry>, addr: &str) {
    let normalized = normalize_server_addr(addr);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(entry) = list.iter_mut().find(|s| s.address == normalized) {
        entry.last_connected = now;
    } else {
        list.push(ServerEntry {
            address: normalized,
            nickname: String::new(),
            last_connected: now,
        });
    }
    save_server_list(list);
}

fn format_last_connected(ts: u64) -> String {
    if ts == 0 {
        return "Never connected".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ago = now.saturating_sub(ts);
    if ago < 60 {
        "Just now".to_string()
    } else if ago < 3600 {
        format!("{} min ago", ago / 60)
    } else if ago < 86400 {
        format!("{} hours ago", ago / 3600)
    } else if ago < 86400 * 30 {
        format!("{} days ago", ago / 86400)
    } else {
        "Long ago".to_string()
    }
}

fn clamp_menu_button_pos(pos: egui::Pos2, content_rect: egui::Rect) -> egui::Pos2 {
    let max_x = (content_rect.right() - FLOATING_MENU_BUTTON_SIZE).max(content_rect.left());
    let max_y = (content_rect.bottom() - FLOATING_MENU_BUTTON_SIZE).max(content_rect.top());
    egui::pos2(
        pos.x.clamp(content_rect.left(), max_x),
        pos.y.clamp(content_rect.top(), max_y),
    )
}

// ---------------------------------------------------------------------------
// StreamApp
// ---------------------------------------------------------------------------

impl StreamApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let saved = load_last_server().unwrap_or_else(default_server_addr);
        let mut server_list = load_server_list();
        // Migrate legacy last_server into the list
        if server_list.is_empty() && !saved.trim().is_empty() {
            ensure_server_in_list(&mut server_list, &saved);
            save_server_list(&server_list);
        }
        let audio = load_audio_enabled();
        let debug_enabled = load_debug_enabled();
        let menu_button_pos = load_menu_button_pos().unwrap_or_else(default_menu_button_pos);
        let display_refresh_millihz = display::detect_max_refresh_millihz();
        let video_codec_support = decode::VideoDecoder::detect_supported_codecs();
        let (update_tx, update_rx) = crossbeam_channel::unbounded();
        let update_ui_state = match updater::supported_target_label() {
            Ok(_) => UpdateUiState::Idle,
            Err(err) => UpdateUiState::Unsupported(err),
        };
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
            server_list,
            add_server_addr: String::new(),
            search_query: String::new(),
            home_tab: HomeTab::Servers,
            audio_enabled: audio,
            debug_enabled,
            display_refresh_millihz,
            video_codec_support,
            audio_enabled_flag: Arc::new(AtomicBool::new(audio)),
            debug_enabled_flag: Arc::new(AtomicBool::new(debug_enabled)),
            state: Arc::new(Mutex::new(ConnectionState::Disconnected)),
            frame: Arc::new(Mutex::new(VideoFrameBuffer::default())),
            debug_state: Arc::new(ConnectionDebugState::new()),
            video_texture,
            keep_awake: KeepAwakeController::new(),
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
            last_sent_absolute_cursor: None,
            hover_cursor_pos: None,
            resume_hover_after_relative_drag: false,
            hover_cursor_resync_pending: false,
            hover_drag_edge_mismatch_frames: 0,
            remote_cursor_textures: BTreeMap::new(),
            latest_remote_cursor_serial: None,
            seen_cursor_shape_version: 0,
            debug_overlay_tab: DebugOverlayTab::General,
            graph_overlay: graph_overlay::GraphOverlay::new(),
            menu_open: false,
            menu_button_pos,
            menu_button_drag_origin: None,
            local_overlay_hit_rects: Vec::new(),
            last_pointer_move: None,
            await_pointer_exit_after_auto_release: false,
            applied_cursor_visible: None,
            applied_cursor_grab: None,
            suppress_mouse_delta: false,
            suppress_pointer_pos_frames: 0,
            excluded_video_codecs: Arc::new(Mutex::new(st_protocol::VideoCodecSupport::empty())),
            update_ui_state,
            update_tx,
            update_rx,
        }
    }

    fn connect(&mut self, ctx: egui::Context) {
        let saved_addr = self.server_addr.trim().to_string();
        save_last_server(&saved_addr);
        touch_server_connected(&mut self.server_list, &saved_addr);

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
        self.last_sent_absolute_cursor = None;
        self.hover_cursor_pos = None;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.hover_drag_edge_mismatch_frames = 0;
        self.await_pointer_exit_after_auto_release = false;
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

        let addr = normalize_server_addr(&self.server_addr);
        let state = Arc::clone(&self.state);
        let frame_buf = Arc::clone(&self.frame);
        let disconnect = disconnect_flag;
        let connection_epoch = Arc::clone(&self.connection_epoch);
        let audio_flag = Arc::clone(&self.audio_enabled_flag);
        let debug_enabled_flag = Arc::clone(&self.debug_enabled_flag);
        let debug_state = Arc::clone(&self.debug_state);
        let native_surfaces = Arc::clone(&self.native_surfaces);
        let display_refresh_millihz = self.display_refresh_millihz;
        let video_codec_support = self.video_codec_support;
        let excluded_video_codecs = Arc::clone(&self.excluded_video_codecs);
        let shared_input = Arc::clone(&self.shared_input);
        debug_state.reset_for_connect(&addr, display_refresh_millihz);

        std::thread::spawn(move || {
            run_connection(
                addr,
                display_refresh_millihz,
                video_codec_support,
                excluded_video_codecs,
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
        self.last_sent_absolute_cursor = None;
        self.hover_cursor_pos = None;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.hover_drag_edge_mismatch_frames = 0;
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
        self.last_sent_absolute_cursor = None;
        self.hover_cursor_pos = None;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.hover_drag_edge_mismatch_frames = 0;
        self.await_pointer_exit_after_auto_release = false;
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
        self.last_sent_absolute_cursor = None;
        self.hover_cursor_pos = None;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.hover_drag_edge_mismatch_frames = 0;
        self.menu_open = false;
        self.await_pointer_exit_after_auto_release = false;
        self.local_overlay_hit_rects.clear();
    }

    fn force_release_capture(&mut self) {
        self.capture_mode = LocalCaptureMode::ForceReleased;
        self.pending_capture_click = false;
        self.pointer_buttons = 0;
        self.last_sent_absolute_cursor = None;
        self.hover_cursor_pos = None;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.hover_drag_edge_mismatch_frames = 0;
        self.await_pointer_exit_after_auto_release = false;
        self.clear_remote_keyboard();
        if let Some(client_id) = self.shared_input.snapshot().client_id {
            self.send_input_packet(InputPacket::MouseButtons(MouseButtonsInput {
                client_id,
                buttons: 0,
            }));
        }
    }

    fn auto_release_capture(&mut self, await_pointer_exit_before_recapture: bool) {
        self.release_pointer_capture();
        self.await_pointer_exit_after_auto_release = await_pointer_exit_before_recapture;
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

    fn cursor_space_size(&self) -> Option<egui::Vec2> {
        if let Some(stream_config) = self.shared_input.snapshot().stream_config {
            if stream_config.width > 0 && stream_config.height > 0 {
                return Some(egui::vec2(
                    stream_config.width as f32,
                    stream_config.height as f32,
                ));
            }
        }

        let size = self.video_texture.size_vec2();
        if size.x >= 1.0 && size.y >= 1.0 {
            Some(size)
        } else {
            None
        }
    }

    fn video_space_size(&self) -> Option<egui::Vec2> {
        if let Some(stream_config) = self.shared_input.snapshot().stream_config {
            if stream_config.width > 0 && stream_config.height > 0 {
                return Some(egui::vec2(
                    stream_config.width as f32,
                    stream_config.height as f32,
                ));
            }
        }

        let size = self.video_texture.size_vec2();
        if size.x >= 1.0 && size.y >= 1.0 {
            Some(size)
        } else {
            None
        }
    }

    fn video_rect_for_container(&self, content_rect: egui::Rect) -> Option<egui::Rect> {
        let video_size = self.video_space_size()?;
        if video_size.x <= 0.0
            || video_size.y <= 0.0
            || content_rect.width() <= 0.0
            || content_rect.height() <= 0.0
        {
            return None;
        }

        let scale = (content_rect.width() / video_size.x).min(content_rect.height() / video_size.y);
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }

        let sized = egui::vec2(video_size.x * scale, video_size.y * scale);
        Some(egui::Rect::from_center_size(content_rect.center(), sized))
    }

    fn current_video_rect(&self, ctx: &egui::Context) -> Option<egui::Rect> {
        #[cfg(target_os = "macos")]
        if let Some(rect) = self.video_texture.current_native_video_rect() {
            return Some(rect);
        }

        self.video_rect_for_container(ctx.content_rect())
    }

    fn server_cursor_stream_top_left(
        &self,
        snapshot: &input::SharedInputSnapshot,
    ) -> egui::Pos2 {
        egui::pos2(
            snapshot.cursor_state.x as f32,
            snapshot.cursor_state.y as f32,
        )
    }

    fn mapped_server_cursor_video_pos(
        &self,
        snapshot: &input::SharedInputSnapshot,
        video_rect: egui::Rect,
    ) -> Option<egui::Pos2> {
        let stream_size = self.cursor_space_size()?;
        if stream_size.x <= 0.0 || stream_size.y <= 0.0 {
            return None;
        }

        let scale_x = video_rect.width() / stream_size.x;
        let scale_y = video_rect.height() / stream_size.y;
        let top_left = egui::pos2(
            video_rect.left() + snapshot.cursor_state.x as f32 * scale_x,
            video_rect.top() + snapshot.cursor_state.y as f32 * scale_y,
        );

        if let Some(texture) = self.remote_cursor_texture_for_serial(snapshot.cursor_state.serial) {
            let display_scale =
                (video_rect.width() / stream_size.x).min(video_rect.height() / stream_size.y);
            Some(egui::pos2(
                top_left.x + texture.hotspot.x * display_scale,
                top_left.y + texture.hotspot.y * display_scale,
            ))
        } else {
            Some(top_left)
        }
    }

    fn uses_virtual_hover_cursor(&self, snapshot: &input::SharedInputSnapshot) -> bool {
        cfg!(target_os = "macos")
            && snapshot.controller_state == ControllerState::OwnedByYou
            && snapshot.capabilities.hover_capture
            && !snapshot.capabilities.separate_cursor
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
        if snapshot.controller_state != ControllerState::OwnedByYou {
            return false;
        }

        match self.capture_mode {
            LocalCaptureMode::HoverAbsolute => {
                let pointer_pos = self
                    .hover_cursor_pos
                    .or_else(|| {
                        if self.uses_virtual_hover_cursor(&snapshot) {
                            None
                        } else {
                            ctx.input(|i| i.pointer.latest_pos())
                        }
                    });
                pointer_pos
                    .zip(self.current_video_rect(ctx).or(self.last_video_rect))
                    .map(|(pointer_pos, rect)| {
                        rect.contains(pointer_pos)
                            && !self.pointer_over_local_overlay(pointer_pos)
                    })
                    .unwrap_or(false)
            }
            LocalCaptureMode::CapturedRelative => {
                snapshot.capabilities.separate_cursor
                    && (self
                        .remote_cursor_texture_for_serial(snapshot.cursor_state.serial)
                        .is_some()
                        || snapshot.cursor_state.visible
                        || snapshot.cursor_state.serial == 0)
            }
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
        let snapshot = self.shared_input.snapshot();
        let overlay_cursor_active = self.overlay_cursor_active(ctx);
        let hover_drag_active = self.capture_mode == LocalCaptureMode::HoverAbsolute
            && snapshot.controller_state == ControllerState::OwnedByYou
            && self.pointer_buttons != 0;
        let (cursor_visible, cursor_grab) =
            if self.capture_mode == LocalCaptureMode::CapturedRelative {
                if self.native_cursor_fallback_active() {
                    (true, egui::CursorGrab::Confined)
                } else {
                    (false, egui::CursorGrab::Locked)
                }
            } else if self.capture_mode == LocalCaptureMode::HoverAbsolute
                && self.uses_virtual_hover_cursor(&snapshot)
            {
                (false, egui::CursorGrab::Locked)
            } else if hover_drag_active {
                (false, egui::CursorGrab::Confined)
            } else if overlay_cursor_active {
                (false, egui::CursorGrab::None)
            } else {
                (true, egui::CursorGrab::None)
            };

        // Hide the cursor before changing the grab mode so that
        // CursorGrab::Locked (which centres the OS cursor on Windows) does
        // not flash the pointer at the centre of the window.  When making
        // the cursor visible again, do it after the grab change so the
        // cursor appears at the released position, not the locked one.
        if !cursor_visible && self.applied_cursor_visible != Some(false) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(false));
            self.applied_cursor_visible = Some(false);
        }
        if self.applied_cursor_grab != Some(cursor_grab) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(cursor_grab));
            self.suppress_mouse_delta = true;
            self.suppress_pointer_pos_frames = 2;
            self.applied_cursor_grab = Some(cursor_grab);
        }
        if cursor_visible && self.applied_cursor_visible != Some(true) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(true));
            self.applied_cursor_visible = Some(true);
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

    fn process_update_events(&mut self) {
        while let Ok(event) = self.update_rx.try_recv() {
            match event {
                UpdateWorkerEvent::CheckFinished(result) => match result {
                    Ok(updater::CheckOutcome::UpToDate {
                        latest_version: version,
                        html_url,
                    }) => {
                        self.update_ui_state = UpdateUiState::UpToDate {
                            version,
                            html_url,
                        };
                    }
                    Ok(updater::CheckOutcome::UpdateAvailable(release)) => {
                        self.update_ui_state = UpdateUiState::UpdateAvailable(release);
                    }
                    Err(err) => {
                        self.update_ui_state = UpdateUiState::Error(err);
                    }
                },
                UpdateWorkerEvent::InstallPrepared { version, result } => match result {
                    Ok(()) => {
                        self.update_ui_state = UpdateUiState::ClosingForUpdate { version };
                    }
                    Err(err) => {
                        self.update_ui_state = UpdateUiState::Error(err);
                    }
                },
            }
        }
    }

    fn begin_update_check(&mut self, ctx: egui::Context) {
        if matches!(
            self.update_ui_state,
            UpdateUiState::Checking | UpdateUiState::Downloading { .. }
        ) {
            return;
        }
        if matches!(self.update_ui_state, UpdateUiState::Unsupported(_)) {
            return;
        }

        self.update_ui_state = UpdateUiState::Checking;
        let tx = self.update_tx.clone();
        std::thread::spawn(move || {
            let result = updater::check_latest_release();
            let _ = tx.send(UpdateWorkerEvent::CheckFinished(result));
            ctx.request_repaint();
        });
    }

    fn begin_update_install(&mut self, ctx: egui::Context, release: updater::ReleaseInfo) {
        if matches!(self.update_ui_state, UpdateUiState::Downloading { .. }) {
            return;
        }

        let version = release.version.clone();
        self.update_ui_state = UpdateUiState::Downloading {
            version: version.clone(),
        };
        let tx = self.update_tx.clone();
        std::thread::spawn(move || {
            let result = updater::prepare_and_spawn_update(&release);
            let _ = tx.send(UpdateWorkerEvent::InstallPrepared { version, result });
            ctx.request_repaint();
        });
    }

    fn send_absolute_cursor(
        &mut self,
        client_id: u32,
        pos: egui::Pos2,
        video_rect: egui::Rect,
        force: bool,
    ) -> bool {
        if !video_rect.contains(pos) || self.pointer_over_local_overlay(pos) {
            self.last_sent_absolute_cursor = None;
            return false;
        }

        let absolute_x = normalized_coord(pos.x, video_rect.left(), video_rect.right());
        let absolute_y = normalized_coord(pos.y, video_rect.top(), video_rect.bottom());
        let next = (absolute_x, absolute_y);
        if force || self.last_sent_absolute_cursor != Some(next) {
            self.last_sent_absolute_cursor = Some(next);
            self.send_input_packet(InputPacket::MouseAbsolute(MouseAbsoluteInput {
                client_id,
                x: absolute_x,
                y: absolute_y,
                buttons: self.pointer_buttons,
            }));
            true
        } else {
            false
        }
    }

    fn send_absolute_cursor_if_needed(
        &mut self,
        client_id: u32,
        pos: egui::Pos2,
        video_rect: egui::Rect,
    ) {
        let _ = self.send_absolute_cursor(client_id, pos, video_rect, false);
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
        let video_rect = self.current_video_rect(ctx).or(self.last_video_rect)?;
        let stream_size = self.cursor_space_size()?;
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

        let (top_left, cursor_pos, using_pointer_pos) =
            if self.capture_mode == LocalCaptureMode::HoverAbsolute
                || self.resume_hover_after_relative_drag
            {
                let pointer_pos = self
                    .hover_cursor_pos
                    .filter(|pos| video_rect.contains(*pos))?;
                (
                    egui::pos2(pointer_pos.x - hotspot.x, pointer_pos.y - hotspot.y),
                    pointer_pos,
                    true,
                )
            } else {
                let server_top_left = self.server_cursor_stream_top_left(input_snapshot);
                let top_left = egui::pos2(
                    video_rect.left() + server_top_left.x * scale_x,
                    video_rect.top() + server_top_left.y * scale_y,
                );
                let cursor_pos = egui::pos2(top_left.x + hotspot.x, top_left.y + hotspot.y);
                (top_left, cursor_pos, false)
            };

        let top_left = egui::pos2(snap(top_left.x), snap(top_left.y));
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
        let decoded_size = self.video_space_size().unwrap_or_default();
        let cursor_space = self.cursor_space_size().unwrap_or_default();
        let video_rect = self.current_video_rect(ctx).or(self.last_video_rect);
        let pointer_pos = ctx.input(|i| i.pointer.latest_pos());
        let over_video = pointer_pos
            .zip(video_rect)
            .map(|(pos, rect)| rect.contains(pos))
            .unwrap_or(false);
        let over_local_overlay = pointer_pos
            .map(|pos| self.pointer_over_local_overlay(pos))
            .unwrap_or(false);
        let active_texture = self.remote_cursor_texture_for_serial(input_snapshot.cursor_state.serial);
        let overlay_geometry = self.compute_cursor_overlay_geometry(ctx, input_snapshot);
        let mut lines = vec![
            format!(
                "cursor: mode={} controller={:?} visible={} separate={} hover={} overlay_active={} native_fallback={}",
                self.capture_mode.label(),
                input_snapshot.controller_state,
                if input_snapshot.cursor_state.visible { "y" } else { "n" },
                if input_snapshot.capabilities.separate_cursor { "y" } else { "n" },
                if input_snapshot.capabilities.hover_capture { "y" } else { "n" },
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
                "video: decoded={}x{} cursor_space={}x{} video_rect={}",
                decoded_size.x.round() as i32,
                decoded_size.y.round() as i32,
                cursor_space.x.round() as i32,
                cursor_space.y.round() as i32,
                format_rect_opt(video_rect),
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
    ) {
        let previous_video_rect = self.last_video_rect;
        let video_rect = self.current_video_rect(ctx).unwrap_or(response.rect);
        self.last_video_rect = Some(video_rect);
        let previous_capture_mode = self.capture_mode;

        let snapshot = self.shared_input.snapshot();
        let hover_supported = snapshot.capabilities.hover_capture;
        let prefer_hover_absolute = snapshot.capabilities.hover_capture;
        let hover_drag_active = previous_capture_mode == LocalCaptureMode::HoverAbsolute
            && snapshot.controller_state == ControllerState::OwnedByYou
            && self.pointer_buttons != 0;
        let virtual_hover = self.uses_virtual_hover_cursor(&snapshot);
        let actual_pointer_pos = if self.suppress_pointer_pos_frames > 0 {
            self.hover_cursor_pos.or_else(|| ctx.input(|i| i.pointer.latest_pos()))
        } else {
            ctx.input(|i| i.pointer.latest_pos())
        };
        let mut pointer_pos = if virtual_hover && previous_capture_mode == LocalCaptureMode::HoverAbsolute
        {
            self.hover_cursor_pos.or(actual_pointer_pos)
        } else {
            actual_pointer_pos
        };
        if virtual_hover
            && previous_capture_mode == LocalCaptureMode::HoverAbsolute
            && snapshot.controller_state == ControllerState::OwnedByYou
            && hover_supported
        {
            let base_pos = if previous_video_rect != Some(video_rect) {
                self.hover_cursor_pos
                    .map(|pos| {
                        previous_video_rect
                            .map(|previous_rect| {
                                remap_pos_between_video_rects(pos, previous_rect, video_rect)
                            })
                            .unwrap_or(pos)
                    })
                    .or(if virtual_hover && self.hover_cursor_pos.is_some() {
                        None
                    } else {
                        actual_pointer_pos
                    })
            } else {
                self.hover_cursor_pos.or(actual_pointer_pos)
            };
            if let Some(base_pos) = base_pos {
                let clamped = clamp_pos_to_video_rect(base_pos, video_rect, ctx.pixels_per_point());
                self.hover_cursor_pos = Some(clamped);
                pointer_pos = Some(clamped);
            }
        }
        let pointer_over_local_overlay = pointer_pos
            .map(|pos| self.pointer_over_local_overlay(pos))
            .unwrap_or(false);
        let pointer_inside_video_rect = pointer_pos
            .map(|pos| video_rect.contains(pos))
            .unwrap_or(false);
        let pointer_over_video = pointer_inside_video_rect && !pointer_over_local_overlay;
        let clicked_video = response.clicked_by(egui::PointerButton::Primary) && pointer_over_video;
        if self.await_pointer_exit_after_auto_release && !pointer_inside_video_rect {
            self.await_pointer_exit_after_auto_release = false;
        }
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
            && !hover_drag_active
        {
            self.auto_release_capture(false);
        }

        if !ctx.input(|i| i.focused) {
            // The OS resets cursor grab and visibility when the window loses
            // focus.  Clear the applied state so apply_pointer_capture_mode
            // re-sends the commands when focus returns.
            self.applied_cursor_grab = None;
            self.applied_cursor_visible = None;
            if self.capture_mode == LocalCaptureMode::CapturedRelative {
                self.force_release_capture();
            }
        }

        if hover_supported
            && self.capture_mode != LocalCaptureMode::CapturedRelative
            && self.capture_mode != LocalCaptureMode::ForceReleased
            && !self.await_pointer_exit_after_auto_release
            && pointer_over_video
            && snapshot.controller_state == ControllerState::OwnedByYou
        {
            self.capture_mode = LocalCaptureMode::HoverAbsolute;
        }

        if clicked_video {
            if snapshot.controller_state == ControllerState::OwnedByYou {
                if hover_supported && prefer_hover_absolute {
                    self.capture_mode = LocalCaptureMode::HoverAbsolute;
                } else if snapshot.capabilities.mouse_relative {
                    self.capture_mode = LocalCaptureMode::CapturedRelative;
                } else if hover_supported {
                    self.capture_mode = LocalCaptureMode::HoverAbsolute;
                }
                self.await_pointer_exit_after_auto_release = false;
                self.pending_capture_click = false;
            } else {
                self.send_control_message(ControlMessage::AcquireControl);
                self.pending_capture_click = true;
            }
            ctx.request_repaint();
        }

        if self.pending_capture_click && snapshot.controller_state == ControllerState::OwnedByYou {
            if hover_supported && prefer_hover_absolute {
                self.capture_mode = LocalCaptureMode::HoverAbsolute;
            } else if snapshot.capabilities.mouse_relative {
                self.capture_mode = LocalCaptureMode::CapturedRelative;
            } else if hover_supported {
                self.capture_mode = LocalCaptureMode::HoverAbsolute;
            }
            self.await_pointer_exit_after_auto_release = false;
            self.pending_capture_click = false;
        }

        if pointer_over_video
            && self.capture_mode == LocalCaptureMode::ForceReleased
            && clicked_video
            && snapshot.controller_state == ControllerState::OwnedByYou
        {
            if hover_supported && prefer_hover_absolute {
                self.capture_mode = LocalCaptureMode::HoverAbsolute;
            } else if snapshot.capabilities.mouse_relative {
                self.capture_mode = LocalCaptureMode::CapturedRelative;
            }
        }

        let drag_buttons = MOUSE_BUTTON_PRIMARY | MOUSE_BUTTON_SECONDARY;
        let hidden_cursor_relative_drag = snapshot.capabilities.separate_cursor
            && snapshot.capabilities.mouse_relative
            && !snapshot.cursor_state.visible;
        let edge_mismatch_relative_drag = if self.capture_mode == LocalCaptureMode::HoverAbsolute
            && snapshot.controller_state == ControllerState::OwnedByYou
            && self.pointer_buttons & drag_buttons != 0
            && snapshot.capabilities.separate_cursor
            && snapshot.capabilities.mouse_relative
            && snapshot.cursor_state.visible
        {
            pointer_pos
                .filter(|pos| {
                    video_rect.contains(*pos) && !self.pointer_over_local_overlay(*pos)
                })
                .filter(|pos| pos_near_video_edge(*pos, video_rect, ctx.pixels_per_point()))
                .and_then(|pos| {
                    self.mapped_server_cursor_video_pos(&snapshot, video_rect)
                        .map(|remote_pos| pos.distance(remote_pos))
                })
                .map(|distance| {
                    distance
                        >= (video_rect.width().min(video_rect.height()) * 0.10).clamp(64.0, 160.0)
                })
                .unwrap_or(false)
        } else {
            false
        };
        if edge_mismatch_relative_drag {
            self.hover_drag_edge_mismatch_frames =
                self.hover_drag_edge_mismatch_frames.saturating_add(1);
        } else {
            self.hover_drag_edge_mismatch_frames = 0;
        }
        if self.capture_mode == LocalCaptureMode::HoverAbsolute
            && snapshot.controller_state == ControllerState::OwnedByYou
            && self.pointer_buttons & drag_buttons != 0
            && (hidden_cursor_relative_drag || self.hover_drag_edge_mismatch_frames >= 2)
        {
            if let Some(pos) = pointer_pos.filter(|pos| {
                video_rect.contains(*pos) && !self.pointer_over_local_overlay(*pos)
            }) {
                self.hover_cursor_pos =
                    Some(clamp_pos_to_video_rect(pos, video_rect, ctx.pixels_per_point()));
            } else if self.hover_cursor_pos.is_none() {
                // Don't fall back to center — skip the transition until we
                // have a real pointer position to anchor the drag.
                self.hover_drag_edge_mismatch_frames = 0;
            }
            if self.hover_cursor_pos.is_some() {
                self.capture_mode = LocalCaptureMode::CapturedRelative;
                self.resume_hover_after_relative_drag = true;
                self.hover_cursor_resync_pending = false;
                self.hover_drag_edge_mismatch_frames = 0;
                ctx.request_repaint();
            }
        }

        if previous_capture_mode != self.capture_mode && self.capture_mode == LocalCaptureMode::Idle
        {
            self.clear_remote_keyboard();
        }

        if self.capture_mode == LocalCaptureMode::HoverAbsolute
            && snapshot.controller_state == ControllerState::OwnedByYou
            && self.capture_mode != LocalCaptureMode::ForceReleased
        {
            if virtual_hover {
                let desired_hover_pos = if previous_capture_mode == LocalCaptureMode::HoverAbsolute {
                    self.hover_cursor_pos.or(pointer_pos).map(|pos| {
                        clamp_pos_to_video_rect(pos, video_rect, ctx.pixels_per_point())
                    })
                } else {
                    actual_pointer_pos
                        .or(pointer_pos)
                        .map(|pos| clamp_pos_to_video_rect(pos, video_rect, ctx.pixels_per_point()))
                };
                if let Some(pos) = desired_hover_pos {
                    self.hover_cursor_pos = Some(pos);
                }

                if let (Some(client_id), Some(pos)) = (snapshot.client_id, self.hover_cursor_pos) {
                    self.send_absolute_cursor_if_needed(client_id, pos, video_rect);
                }
            } else {
                let hover_pos = if hover_drag_active {
                    actual_pointer_pos
                        .or(pointer_pos)
                        .map(|pos| clamp_pos_to_video_rect(pos, video_rect, ctx.pixels_per_point()))
                        .filter(|pos| !self.pointer_over_local_overlay(*pos))
                } else if self.hover_cursor_resync_pending {
                    self.hover_cursor_pos
                        .filter(|pos| {
                            video_rect.contains(*pos) && !self.pointer_over_local_overlay(*pos)
                        })
                        .or_else(|| {
                            actual_pointer_pos.or(pointer_pos).filter(|pos| {
                                video_rect.contains(*pos) && !self.pointer_over_local_overlay(*pos)
                            })
                        })
                } else {
                    actual_pointer_pos.or(pointer_pos).filter(|pos| {
                        video_rect.contains(*pos) && !self.pointer_over_local_overlay(*pos)
                    })
                };
                self.hover_cursor_pos = hover_pos;
                if let (Some(client_id), Some(pos)) = (snapshot.client_id, hover_pos) {
                    self.send_absolute_cursor_if_needed(client_id, pos, video_rect);
                } else {
                    self.last_sent_absolute_cursor = None;
                }
            }
        } else if !self.resume_hover_after_relative_drag {
            self.hover_cursor_pos = None;
            self.last_sent_absolute_cursor = None;
        }

        self.draw_remote_cursor_overlay(ctx);
    }

    fn draw_remote_cursor_overlay(&self, ctx: &egui::Context) {
        let snapshot = self.shared_input.snapshot();
        if let Some(geometry) = self.compute_cursor_overlay_geometry(ctx, &snapshot) {
            egui::Area::new(egui::Id::new("remote_cursor_overlay"))
                .order(egui::Order::Tooltip)
                .fixed_pos(geometry.rect.min)
                .show(ctx, |ui| {
                    let sized =
                        egui::load::SizedTexture::new(geometry.texture_id, geometry.rect.size());
                    ui.image(sized);
                });
            return;
        }

        let Some(pos) = self.hover_cursor_pos else {
            return;
        };
        let Some(video_rect) = self.current_video_rect(ctx).or(self.last_video_rect) else {
            return;
        };
        if self.capture_mode != LocalCaptureMode::HoverAbsolute
            || snapshot.controller_state != ControllerState::OwnedByYou
            || !snapshot.capabilities.hover_capture
            || !video_rect.contains(pos)
            || !snapshot.cursor_state.visible
        {
            return;
        }

        egui::Area::new(egui::Id::new("remote_cursor_fallback_overlay"))
            .order(egui::Order::Tooltip)
            .fixed_pos(video_rect.min)
            .show(ctx, |ui| {
                let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, video_rect.size());
                let local_pos = pos - video_rect.min.to_vec2();
                let painter = ui.painter().with_clip_rect(rect);
                painter.circle_filled(local_pos, 5.0, egui::Color32::WHITE);
                painter.circle_stroke(
                    local_pos,
                    5.0,
                    egui::Stroke::new(1.5, egui::Color32::BLACK),
                );
            });
    }

    #[cfg(not(target_os = "macos"))]
    fn paint_connected_background(&self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(7, 10, 14));
        painter.circle_filled(
            egui::pos2(rect.left() + rect.width() * 0.18, rect.top() + rect.height() * 0.22),
            rect.width().min(rect.height()) * 0.16,
            egui::Color32::from_rgba_unmultiplied(54, 156, 255, 20),
        );
        painter.circle_filled(
            egui::pos2(
                rect.right() - rect.width() * 0.16,
                rect.bottom() - rect.height() * 0.18,
            ),
            rect.width().min(rect.height()) * 0.20,
            egui::Color32::from_rgba_unmultiplied(34, 198, 140, 16),
        );
    }

    #[cfg(target_os = "macos")]
    fn paint_connected_background(&self, _ui: &mut egui::Ui) {}

    fn render_home_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let full_rect = ui.max_rect();
        let painter = ui.painter().clone();

        // Parsec-style colors
        const BG_MAIN: egui::Color32 = egui::Color32::from_rgb(26, 30, 38);
        const BG_SIDEBAR: egui::Color32 = egui::Color32::from_rgb(21, 24, 31);
        const TEXT_WHITE: egui::Color32 = egui::Color32::from_rgb(230, 233, 240);
        const TEXT_GRAY: egui::Color32 = egui::Color32::from_rgb(138, 142, 150);
        const ACCENT_BLUE: egui::Color32 = egui::Color32::from_rgb(90, 200, 250);
        const BTN_DARK: egui::Color32 = egui::Color32::from_rgb(52, 56, 66);
        const SIDEBAR_W: f32 = 48.0;

        // Fill backgrounds
        painter.rect_filled(full_rect, 0.0, BG_MAIN);
        let sidebar_rect = egui::Rect::from_min_size(
            full_rect.min,
            egui::vec2(SIDEBAR_W, full_rect.height()),
        );
        painter.rect_filled(sidebar_rect, 0.0, BG_SIDEBAR);

        // --- Sidebar icons ---
        let sidebar_tabs = [
            ("S", "Servers", HomeTab::Servers),
            ("G", "Settings", HomeTab::Settings),
            ("U", "Update", HomeTab::Update),
            ("?", "About", HomeTab::About),
        ];
        for (i, (icon, tooltip, tab)) in sidebar_tabs.iter().enumerate() {
            let selected = self.home_tab == *tab;
            let icon_y = full_rect.top() + 16.0 + i as f32 * 48.0;
            let icon_rect = egui::Rect::from_min_size(
                egui::pos2(full_rect.left(), icon_y),
                egui::vec2(SIDEBAR_W, 40.0),
            );

            if selected {
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(full_rect.left(), icon_y),
                        egui::vec2(3.0, 40.0),
                    ),
                    0.0,
                    ACCENT_BLUE,
                );
                painter.rect_filled(
                    icon_rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(90, 200, 250, 20),
                );
            }

            let icon_response = ui.allocate_rect(icon_rect, egui::Sense::click());
            painter.text(
                icon_rect.center(),
                egui::Align2::CENTER_CENTER,
                icon,
                egui::FontId::proportional(16.0),
                if selected { ACCENT_BLUE } else { TEXT_GRAY },
            );
            if icon_response.hovered() && !selected {
                painter.rect_filled(
                    icon_rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 8),
                );
            }
            if icon_response.clicked() {
                self.home_tab = *tab;
            }
            icon_response.on_hover_text(*tooltip);
        }

        // --- Main content area ---
        let content_left = full_rect.left() + SIDEBAR_W;

        // Bottom bar (only on Servers tab)
        let bottom_bar_h = if self.home_tab == HomeTab::Servers {
            48.0
        } else {
            0.0
        };
        let main_bottom = full_rect.bottom() - bottom_bar_h;

        if self.home_tab == HomeTab::Servers && bottom_bar_h > 0.0 {
            let bottom_rect = egui::Rect::from_min_max(
                egui::pos2(content_left, main_bottom),
                full_rect.max,
            );
            painter.rect_filled(
                bottom_rect,
                0.0,
                egui::Color32::from_rgb(30, 34, 42),
            );
            painter.line_segment(
                [
                    egui::pos2(content_left, bottom_rect.top()),
                    egui::pos2(full_rect.right(), bottom_rect.top()),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(46, 50, 58)),
            );

            let bottom_inner = bottom_rect.shrink2(egui::vec2(20.0, 8.0));
            let mut bottom_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(bottom_inner)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            bottom_ui.label(
                egui::RichText::new("Add server by address.")
                    .size(12.0)
                    .color(TEXT_GRAY),
            );
            bottom_ui.add_space(8.0);
            let add_resp = bottom_ui.add(
                egui::TextEdit::singleline(&mut self.add_server_addr)
                    .hint_text("IP or host[:port]")
                    .desired_width(200.0),
            );
            let can_add = !self.add_server_addr.trim().is_empty();
            if add_resp.lost_focus()
                && bottom_ui.input(|i| i.key_pressed(egui::Key::Enter))
                && can_add
            {
                let addr = self.add_server_addr.trim().to_string();
                ensure_server_in_list(&mut self.server_list, &addr);
                save_server_list(&self.server_list);
                self.add_server_addr.clear();
            }
            bottom_ui.add_space(4.0);
            if bottom_ui
                .add_enabled(
                    can_add,
                    egui::Button::new(
                        egui::RichText::new("Add")
                            .size(12.0)
                            .color(TEXT_WHITE),
                    )
                    .fill(BTN_DARK)
                    .corner_radius(4)
                    .min_size(egui::vec2(50.0, 28.0)),
                )
                .clicked()
            {
                let addr = self.add_server_addr.trim().to_string();
                ensure_server_in_list(&mut self.server_list, &addr);
                save_server_list(&self.server_list);
                self.add_server_addr.clear();
            }
        }

        // Scrollable main content
        let main_rect = egui::Rect::from_min_max(
            egui::pos2(content_left, full_rect.top()),
            egui::pos2(full_rect.right(), main_bottom),
        );
        let mut main_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(main_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(&mut main_ui, |ui| {
                let margin = egui::Margin {
                    left: 32,
                    right: 32,
                    top: 28,
                    bottom: 28,
                };
                egui::Frame::NONE
                    .inner_margin(margin)
                    .show(ui, |ui| {
                        match self.home_tab {
                            HomeTab::Servers => self.render_servers_tab(ui, ctx, &painter),
                            HomeTab::Settings => self.render_settings_tab(ui),
                            HomeTab::Update => self.render_update_tab(ui, ctx),
                            HomeTab::About => self.render_about_tab(ui),
                        }
                    });
            });
    }

    fn render_servers_tab(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        painter: &egui::Painter,
    ) {
        const TEXT_WHITE: egui::Color32 = egui::Color32::from_rgb(230, 233, 240);
        const TEXT_GRAY: egui::Color32 = egui::Color32::from_rgb(138, 142, 150);
        const TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(90, 95, 108);
        const ACCENT_BLUE: egui::Color32 = egui::Color32::from_rgb(90, 200, 250);
        const BTN_DARK: egui::Color32 = egui::Color32::from_rgb(52, 56, 66);
        const BG_CARD: egui::Color32 = egui::Color32::from_rgb(42, 46, 56);
        const BG_CARD_HOVER: egui::Color32 = egui::Color32::from_rgb(50, 55, 66);
        const CARD_BORDER: egui::Color32 = egui::Color32::from_rgb(58, 62, 72);
        const CARD_W: f32 = 180.0;
        const CARD_H: f32 = 210.0;

        ui.label(
            egui::RichText::new("Computers")
                .size(32.0)
                .color(TEXT_WHITE),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Connect to your computer in low latency desktop mode.")
                .size(13.0)
                .color(TEXT_GRAY),
        );
        ui.add_space(16.0);

        // Search bar + Reload
        let search_width = ui.available_width().min(500.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("Search Hosts and Computers")
                    .desired_width(search_width),
            );
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Reload")
                                    .size(13.0)
                                    .color(ACCENT_BLUE),
                            )
                            .fill(egui::Color32::TRANSPARENT),
                        )
                        .clicked()
                    {
                        // Placeholder
                    }
                },
            );
        });
        ui.add_space(20.0);

        // Sort: most recently connected first
        let mut indices: Vec<usize> = (0..self.server_list.len()).collect();
        indices.sort_by(|&a, &b| {
            self.server_list[b]
                .last_connected
                .cmp(&self.server_list[a].last_connected)
        });

        // Filter by search
        let query = self.search_query.trim().to_lowercase();
        if !query.is_empty() {
            indices.retain(|&i| {
                let e = &self.server_list[i];
                e.address.to_lowercase().contains(&query)
                    || e.nickname.to_lowercase().contains(&query)
            });
        }

        let mut connect_idx: Option<usize> = None;
        let mut delete_idx: Option<usize> = None;

        if self.server_list.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("No computers")
                        .size(15.0)
                        .color(TEXT_DIM),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Add a server address using the bar below.")
                        .size(12.0)
                        .color(TEXT_DIM),
                );
            });
        } else if indices.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("No matches")
                        .size(15.0)
                        .color(TEXT_DIM),
                );
            });
        } else {
            let avail_w = ui.available_width();
            let cols = ((avail_w + 12.0) / (CARD_W + 12.0)).floor().max(1.0) as usize;

            let mut card_i = 0;
            while card_i < indices.len() {
                ui.horizontal(|ui| {
                    for _ in 0..cols {
                        if card_i >= indices.len() {
                            break;
                        }
                        let idx = indices[card_i];
                        card_i += 1;
                        let entry = &self.server_list[idx];

                        let (card_id, card_rect) =
                            ui.allocate_space(egui::vec2(CARD_W, CARD_H));
                        let hover = ui
                            .interact(card_rect, card_id, egui::Sense::hover())
                            .hovered();
                        let fill = if hover { BG_CARD_HOVER } else { BG_CARD };

                        painter.rect(
                            card_rect,
                            8.0,
                            fill,
                            egui::Stroke::new(1.0, CARD_BORDER),
                            egui::StrokeKind::Outside,
                        );

                        // Monitor icon
                        let icon_cx = card_rect.center().x;
                        let icon_top = card_rect.top() + 20.0;
                        paint_monitor_icon(
                            painter,
                            egui::pos2(icon_cx, icon_top),
                            entry.last_connected > 0,
                        );

                        // Server name
                        let display_name = if entry.nickname.is_empty() {
                            &entry.address
                        } else {
                            &entry.nickname
                        };
                        let name_y = icon_top + 76.0;
                        painter.text(
                            egui::pos2(icon_cx, name_y),
                            egui::Align2::CENTER_CENTER,
                            truncate_str(display_name, 20),
                            egui::FontId::proportional(12.0),
                            TEXT_WHITE,
                        );

                        // Subtitle
                        let sub_y = name_y + 16.0;
                        let subtitle = if !entry.nickname.is_empty() {
                            truncate_str(&entry.address, 24).to_string()
                        } else {
                            format_last_connected(entry.last_connected)
                        };
                        painter.text(
                            egui::pos2(icon_cx, sub_y),
                            egui::Align2::CENTER_CENTER,
                            &subtitle,
                            egui::FontId::proportional(10.0),
                            TEXT_DIM,
                        );

                        // Connect button
                        let btn_w = CARD_W - 24.0;
                        let btn_h = 28.0;
                        let btn_rect = egui::Rect::from_min_size(
                            egui::pos2(
                                card_rect.left() + 12.0,
                                card_rect.bottom() - 12.0 - btn_h,
                            ),
                            egui::vec2(btn_w, btn_h),
                        );
                        let btn_resp =
                            ui.allocate_rect(btn_rect, egui::Sense::click());
                        let btn_fill = if btn_resp.hovered() {
                            egui::Color32::from_rgb(62, 66, 76)
                        } else {
                            BTN_DARK
                        };
                        painter.rect(
                            btn_rect,
                            4.0,
                            btn_fill,
                            egui::Stroke::NONE,
                            egui::StrokeKind::Outside,
                        );
                        painter.text(
                            btn_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "Connect",
                            egui::FontId::proportional(12.0),
                            TEXT_WHITE,
                        );
                        if btn_resp.clicked() {
                            connect_idx = Some(idx);
                        }

                        // Delete X in top-right corner
                        let x_rect = egui::Rect::from_min_size(
                            egui::pos2(card_rect.right() - 22.0, card_rect.top() + 4.0),
                            egui::vec2(18.0, 18.0),
                        );
                        let x_resp =
                            ui.allocate_rect(x_rect, egui::Sense::click());
                        if hover {
                            let x_color = if x_resp.hovered() {
                                egui::Color32::from_rgb(220, 100, 100)
                            } else {
                                egui::Color32::from_rgb(140, 90, 90)
                            };
                            painter.text(
                                x_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "x",
                                egui::FontId::proportional(12.0),
                                x_color,
                            );
                        }
                        if x_resp.clicked() {
                            delete_idx = Some(idx);
                        }

                        ui.add_space(12.0);
                    }
                });
                ui.add_space(12.0);
            }
        }

        if let Some(idx) = connect_idx {
            self.server_addr = self.server_list[idx].address.clone();
            self.video_texture.clear_frame();
            self.connect(ctx.clone());
        }
        if let Some(idx) = delete_idx {
            self.server_list.remove(idx);
            save_server_list(&self.server_list);
        }
    }

    fn render_settings_tab(&mut self, ui: &mut egui::Ui) {
        const TEXT_WHITE: egui::Color32 = egui::Color32::from_rgb(230, 233, 240);
        const TEXT_GRAY: egui::Color32 = egui::Color32::from_rgb(138, 142, 150);
        const BG_ROW: egui::Color32 = egui::Color32::from_rgb(34, 38, 48);

        ui.label(
            egui::RichText::new("Settings")
                .size(32.0)
                .color(TEXT_WHITE),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Session defaults for the next connection.")
                .size(13.0)
                .color(TEXT_GRAY),
        );
        ui.add_space(24.0);

        // Audio toggle
        let audio_clicked = render_parsec_toggle(ui, "Audio", "Start stereo playback on connect.", self.audio_enabled, BG_ROW);
        if audio_clicked {
            self.audio_enabled = !self.audio_enabled;
            save_audio_enabled(self.audio_enabled);
            self.audio_enabled_flag.store(self.audio_enabled, Ordering::SeqCst);
        }
        ui.add_space(8.0);

        // Debug overlay toggle
        let debug_clicked = render_parsec_toggle(ui, "Debug Overlay", "Show transport, decoder, and latency telemetry.", self.debug_enabled, BG_ROW);
        if debug_clicked {
            self.debug_enabled = !self.debug_enabled;
            save_debug_enabled(self.debug_enabled);
            self.debug_enabled_flag.store(self.debug_enabled, Ordering::SeqCst);
        }
    }

    fn render_update_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        const TEXT_WHITE: egui::Color32 = egui::Color32::from_rgb(230, 233, 240);
        const TEXT_GRAY: egui::Color32 = egui::Color32::from_rgb(138, 142, 150);

        ui.label(
            egui::RichText::new("Update")
                .size(32.0)
                .color(TEXT_WHITE),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Check for new client releases and update in place.")
                .size(13.0)
                .color(TEXT_GRAY),
        );
        ui.add_space(24.0);
        self.render_update_panel(ui, ctx, false);
    }

    fn render_about_tab(&self, ui: &mut egui::Ui) {
        const TEXT_WHITE: egui::Color32 = egui::Color32::from_rgb(230, 233, 240);
        const TEXT_GRAY: egui::Color32 = egui::Color32::from_rgb(138, 142, 150);
        const TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(90, 95, 108);

        ui.label(
            egui::RichText::new("About")
                .size(32.0)
                .color(TEXT_WHITE),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Client capabilities and platform information.")
                .size(13.0)
                .color(TEXT_GRAY),
        );
        ui.add_space(24.0);

        let caps = self.native_surfaces.snapshot();
        let codec_support = self.video_codec_support;

        let rows: &[(&str, String)] = &[
            ("Version", format!("v{}", updater::current_version())),
            ("Platform", about_platform_label().to_string()),
            ("Display", about_format_refresh(self.display_refresh_millihz)),
            ("Present", about_native_surface(caps).to_string()),
            ("Codecs", about_codec_summary(codec_support)),
            ("Audio", "opus stereo / 48 kHz".to_string()),
        ];

        for (label, value) in rows {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(*label)
                        .size(13.0)
                        .color(TEXT_DIM),
                );
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(value.as_str())
                        .size(13.0)
                        .monospace()
                        .color(TEXT_WHITE),
                );
            });
            ui.add_space(6.0);
        }
    }

    fn render_update_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, compact: bool) {
        ui.label(
            egui::RichText::new("App Update")
                .strong()
                .color(egui::Color32::from_rgb(224, 230, 236)),
        );
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(format!("installed: v{}", updater::current_version()))
                .monospace()
                .color(egui::Color32::from_rgb(228, 234, 240)),
        );
        if let Ok(target) = updater::supported_target_label() {
            ui.label(
                egui::RichText::new(format!("asset: {target}"))
                    .monospace()
                    .color(egui::Color32::from_rgb(174, 186, 198)),
            );
        }

        ui.add_space(6.0);
        match &self.update_ui_state {
            UpdateUiState::Unsupported(message) => {
                ui.label(
                    egui::RichText::new(message)
                        .size(12.0)
                        .color(egui::Color32::from_rgb(198, 111, 111)),
                );
            }
            UpdateUiState::Idle => {
                let description = if compact {
                    "Check GitHub releases and replace the installed package in place."
                } else {
                    "Check the client GitHub releases and stage a full in-place package update for this platform."
                };
                ui.label(
                    egui::RichText::new(description)
                        .size(12.0)
                        .color(egui::Color32::from_rgb(134, 147, 160)),
                );
            }
            UpdateUiState::Checking => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new("Checking GitHub releases...")
                            .size(12.0)
                            .color(egui::Color32::from_rgb(180, 191, 203)),
                    );
                });
            }
            UpdateUiState::UpToDate { version, html_url } => {
                ui.label(
                    egui::RichText::new(format!("Up to date. Latest release: v{version}"))
                        .size(12.0)
                        .color(egui::Color32::from_rgb(114, 200, 153)),
                );
                ui.hyperlink_to("View releases", html_url);
            }
            UpdateUiState::UpdateAvailable(release) => {
                ui.label(
                    egui::RichText::new(format!("Update available: v{}", release.version))
                        .size(12.0)
                        .color(egui::Color32::from_rgb(136, 199, 255)),
                );
                if !compact {
                    ui.label(
                        egui::RichText::new(format!("package: {}", release.asset_name))
                            .size(12.0)
                            .color(egui::Color32::from_rgb(134, 147, 160)),
                    );
                }
                ui.hyperlink_to("Open release page", &release.html_url);
            }
            UpdateUiState::Downloading { version } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new(format!(
                            "Downloading and installing v{version}..."
                        ))
                        .size(12.0)
                        .color(egui::Color32::from_rgb(180, 191, 203)),
                    );
                });
            }
            UpdateUiState::ClosingForUpdate { version } => {
                ui.label(
                    egui::RichText::new(format!(
                        "v{version} is staged. Closing the client so the package updater can finish."
                    ))
                    .size(12.0)
                    .color(egui::Color32::from_rgb(114, 200, 153)),
                );
            }
            UpdateUiState::Error(message) => {
                ui.label(
                    egui::RichText::new(message)
                        .size(12.0)
                        .color(egui::Color32::from_rgb(198, 111, 111)),
                );
            }
        }

        ui.add_space(8.0);
        let busy = matches!(
            self.update_ui_state,
            UpdateUiState::Checking | UpdateUiState::Downloading { .. }
        );
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !busy && !matches!(self.update_ui_state, UpdateUiState::Unsupported(_)),
                    egui::Button::new("Check For Updates"),
                )
                .clicked()
            {
                self.begin_update_check(ctx.clone());
            }

            if let UpdateUiState::UpdateAvailable(release) = &self.update_ui_state {
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(format!("Update To v{}", release.version)),
                    )
                    .clicked()
                {
                    self.begin_update_install(ctx.clone(), release.clone());
                }
            }

            if compact {
                ui.hyperlink_to("Releases", updater::releases_page_url());
            }
        });
    }

    fn render_floating_menu(&mut self, ctx: &egui::Context) -> f32 {
        self.local_overlay_hit_rects.clear();
        let content_rect = ctx.content_rect();
        self.menu_button_pos = clamp_menu_button_pos(self.menu_button_pos, content_rect);
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
            .fixed_pos(self.menu_button_pos)
            .show(ctx, |ui| {
                ui.add_sized(
                    [FLOATING_MENU_BUTTON_SIZE, FLOATING_MENU_BUTTON_SIZE],
                    egui::Button::new(
                        egui::RichText::new("M")
                            .size(18.0)
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
                    ))
                    .sense(egui::Sense::click_and_drag()),
                )
            });
        let button_response = button.inner;
        if button_response.drag_started() {
            self.menu_button_drag_origin = Some(self.menu_button_pos);
        }
        if button_response.dragged() {
            if let (Some(origin), Some(delta)) =
                (self.menu_button_drag_origin, button_response.total_drag_delta())
            {
                self.menu_button_pos = clamp_menu_button_pos(origin + delta, content_rect);
                self.last_pointer_move = Some(Instant::now());
            }
        }
        if button_response.drag_stopped() {
            self.menu_button_drag_origin = None;
            save_menu_button_pos(self.menu_button_pos);
        }
        let button_rect =
            egui::Rect::from_min_size(self.menu_button_pos, egui::vec2(FLOATING_MENU_BUTTON_SIZE, FLOATING_MENU_BUTTON_SIZE));
        self.local_overlay_hit_rects.push(button_rect);
        if button_response.clicked() {
            self.menu_open = !self.menu_open;
            self.last_pointer_move = Some(Instant::now());
        }

        let mut overlay_top = button_rect.bottom() + 10.0;
        if self.menu_open {
            let mut request_disconnect = false;
            let mut audio_toggled = false;
            let mut debug_toggled = false;
            let menu_left =
                button_rect
                    .left()
                    .clamp(content_rect.left(), (content_rect.right() - 190.0).max(content_rect.left()));
            let menu = egui::Area::new(egui::Id::new("floating_menu_popup"))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(menu_left, button_rect.bottom() + 8.0))
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

                                ui.add_space(8.0);
                                ui.separator();
                                ui.add_space(8.0);
                                self.render_update_panel(ui, ctx, true);
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
    let input_target = std::net::SocketAddr::new(socket_addr.ip(), socket_addr.port());
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

const STARTUP_DECODE_TIMEOUT: Duration = Duration::from_secs(5);

fn run_connection(
    addr: String,
    display_refresh_millihz: Option<u32>,
    video_codec_support: decode::VideoCodecSupportReport,
    excluded_video_codecs: Arc<Mutex<st_protocol::VideoCodecSupport>>,
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
            "[trace][client] sending ClientDisplayInfo: refresh_millihz={} udp_port={local_udp_port} codecs={} hw_codecs={}",
            display_refresh_millihz.unwrap_or(0),
            codec_support_summary(video_codec_support.supported),
            codec_support_summary(video_codec_support.hardware)
        );
    }
    let excluded = *excluded_video_codecs.lock().unwrap();
    let effective_supported = video_codec_support.supported.subtract(excluded);
    let effective_hardware = video_codec_support.hardware.subtract(excluded);
    if !excluded.is_empty() {
        eprintln!(
            "[codec] excluding previously failed codecs: {} (effective: supported={} hw={})",
            codec_support_summary(excluded),
            codec_support_summary(effective_supported),
            codec_support_summary(effective_hardware),
        );
    }
    let _ = tcp.write_all(
        &ControlMessage::ClientDisplayInfo(ClientDisplayInfo {
            max_refresh_millihz: display_refresh_millihz.unwrap_or(0),
            udp_port: local_udp_port,
            supported_video_codecs: effective_supported,
            hardware_video_codecs: effective_hardware,
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
                            shared_input.set_stream_config(cfg);
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
                                "[cursor] capabilities: abs={} rel={} kbd={} separate_cursor={} hover_capture={}",
                                capabilities.mouse_absolute,
                                capabilities.mouse_relative,
                                capabilities.keyboard,
                                capabilities.separate_cursor,
                                capabilities.hover_capture
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
    let mut startup_decode_ok = false;
    let startup_deadline = Instant::now() + STARTUP_DECODE_TIMEOUT;
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
            if !startup_decode_ok && feedback.completed_frames > 0 {
                startup_decode_ok = true;
            }
            if tcp
                .write_all(&ControlMessage::TransportFeedback(feedback).serialize())
                .is_err()
            {
                break;
            }
        }
        if !startup_decode_ok && Instant::now() >= startup_deadline {
            let codec = stream_config.codec;
            eprintln!(
                "[codec] no frames decoded within {}s — excluding {:?} and reconnecting",
                STARTUP_DECODE_TIMEOUT.as_secs(),
                codec,
            );
            excluded_video_codecs.lock().unwrap().insert(codec);
            // Trigger reconnect by disconnecting cleanly
            *state.lock().unwrap() = ConnectionState::Disconnected;
            ctx.request_repaint();
            stop_media_threads(media_threads);
            return;
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
                            shared_input.set_stream_config(cfg);
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
                                "[cursor] capabilities: abs={} rel={} kbd={} separate_cursor={} hover_capture={}",
                                capabilities.mouse_absolute,
                                capabilities.mouse_relative,
                                capabilities.keyboard,
                                capabilities.separate_cursor,
                                capabilities.hover_capture
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

#[derive(Clone, Copy, Default)]
struct MouseInputHeartbeat {
    packet: Option<InputPacket>,
    buttons: u8,
    resend_until: Option<Instant>,
    last_sent_at: Option<Instant>,
}

impl MouseInputHeartbeat {
    fn observe(&mut self, packet: InputPacket, now: Instant) {
        let Some(heartbeat_packet) = mouse_heartbeat_packet(packet) else {
            return;
        };
        let next_buttons = mouse_heartbeat_buttons(heartbeat_packet);
        if next_buttons != self.buttons {
            self.resend_until = Some(now + INPUT_STATE_REPAIR_WINDOW);
        }
        self.packet = Some(heartbeat_packet);
        self.buttons = next_buttons;
    }

    fn mark_sent(&mut self, packet: InputPacket, now: Instant) {
        if matches!(
            packet,
            InputPacket::MouseAbsolute(_)
                | InputPacket::MouseRelative(_)
                | InputPacket::MouseButtons(_)
                | InputPacket::MouseWheel(_)
        ) {
            self.last_sent_at = Some(now);
        }
    }

    fn due_packet(&mut self, now: Instant) -> Option<InputPacket> {
        let packet = self.packet?;
        let resend_due_to_transition = self.repair_window_active(now);
        if self.buttons == 0 && !resend_due_to_transition {
            return None;
        }
        if let Some(last_sent_at) = self.last_sent_at {
            if now.saturating_duration_since(last_sent_at) < INPUT_STATE_HEARTBEAT_INTERVAL {
                return None;
            }
        }
        self.last_sent_at = Some(now);
        Some(packet)
    }

    fn repair_window_active(&mut self, now: Instant) -> bool {
        match self.resend_until {
            Some(until) if now < until => true,
            Some(_) => {
                self.resend_until = None;
                false
            }
            None => false,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct KeyboardInputHeartbeat {
    packet: Option<InputPacket>,
    any_pressed: bool,
    resend_until: Option<Instant>,
    last_sent_at: Option<Instant>,
}

impl KeyboardInputHeartbeat {
    fn observe(&mut self, packet: InputPacket, now: Instant) {
        let InputPacket::KeyboardState(state) = packet else {
            return;
        };

        let next_packet = InputPacket::KeyboardState(state);
        if self.packet != Some(next_packet) {
            self.resend_until = Some(now + INPUT_STATE_REPAIR_WINDOW);
        }
        self.packet = Some(next_packet);
        self.any_pressed = state.pressed.iter().any(|byte| *byte != 0);
    }

    fn mark_sent(&mut self, packet: InputPacket, now: Instant) {
        if matches!(packet, InputPacket::KeyboardState(_)) {
            self.last_sent_at = Some(now);
        }
    }

    fn due_packet(&mut self, now: Instant) -> Option<InputPacket> {
        let packet = self.packet?;
        let resend_due_to_transition = self.repair_window_active(now);
        if !self.any_pressed && !resend_due_to_transition {
            return None;
        }
        if let Some(last_sent_at) = self.last_sent_at {
            if now.saturating_duration_since(last_sent_at) < INPUT_STATE_HEARTBEAT_INTERVAL {
                return None;
            }
        }
        self.last_sent_at = Some(now);
        Some(packet)
    }

    fn repair_window_active(&mut self, now: Instant) -> bool {
        match self.resend_until {
            Some(until) if now < until => true,
            Some(_) => {
                self.resend_until = None;
                false
            }
            None => false,
        }
    }
}

fn mouse_heartbeat_packet(packet: InputPacket) -> Option<InputPacket> {
    match packet {
        InputPacket::MouseAbsolute(packet) => Some(InputPacket::MouseAbsolute(packet)),
        InputPacket::MouseRelative(packet) => Some(InputPacket::MouseButtons(MouseButtonsInput {
            client_id: packet.client_id,
            buttons: packet.buttons,
        })),
        InputPacket::MouseButtons(packet) => Some(InputPacket::MouseButtons(packet)),
        InputPacket::MouseWheel(packet) => Some(InputPacket::MouseButtons(MouseButtonsInput {
            client_id: packet.client_id,
            buttons: packet.buttons,
        })),
        InputPacket::KeyboardState(_) => None,
    }
}

fn mouse_heartbeat_buttons(packet: InputPacket) -> u8 {
    match packet {
        InputPacket::MouseAbsolute(packet) => packet.buttons,
        InputPacket::MouseButtons(packet) => packet.buttons,
        _ => 0,
    }
}

fn send_input_packet_raw(
    socket: &UdpSocket,
    target: std::net::SocketAddr,
    packet: InputPacket,
    seq: &mut u16,
) {
    let raw = packet.serialize(*seq);
    *seq = seq.wrapping_add(1);
    let _ = socket.send_to(&raw, target);
}

fn run_input_sender(
    socket: UdpSocket,
    target: std::net::SocketAddr,
    input_rx: crossbeam_channel::Receiver<InputPacket>,
    shutdown_rx: crossbeam_channel::Receiver<()>,
) {
    let mut seq = 0u16;
    let mut mouse_heartbeat = MouseInputHeartbeat::default();
    let mut keyboard_heartbeat = KeyboardInputHeartbeat::default();
    let _ = socket.set_write_timeout(Some(INPUT_STATE_HEARTBEAT_INTERVAL));
    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        match input_rx.recv_timeout(INPUT_SENDER_POLL_INTERVAL) {
            Ok(packet) => {
                let now = Instant::now();
                mouse_heartbeat.observe(packet, now);
                keyboard_heartbeat.observe(packet, now);
                send_input_packet_raw(&socket, target, packet, &mut seq);
                mouse_heartbeat.mark_sent(packet, now);
                keyboard_heartbeat.mark_sent(packet, now);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                if let Some(packet) = mouse_heartbeat.due_packet(now) {
                    send_input_packet_raw(&socket, target, packet, &mut seq);
                }
                if let Some(packet) = keyboard_heartbeat.due_packet(now) {
                    send_input_packet_raw(&socket, target, packet, &mut seq);
                }
            }
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

fn codec_support_summary(support: VideoCodecSupport) -> String {
    let mut entries = Vec::new();
    for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
        if support.supports(codec) {
            entries.push(codec_name(codec));
        }
    }
    if entries.is_empty() {
        "-".to_string()
    } else {
        entries.join(" / ")
    }
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
    codec_name(stream_config.codec)
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
        VideoCodec::Av1 => "av1",
    }
}

fn normalized_coord(value: f32, min: f32, max: f32) -> u16 {
    let span = (max - min).max(1.0);
    let normalized = ((value - min) / span).clamp(0.0, 1.0);
    (normalized * 65535.0).round() as u16
}

fn clamp_pos_to_video_rect(pos: egui::Pos2, rect: egui::Rect, pixels_per_point: f32) -> egui::Pos2 {
    let inset = 0.5 / pixels_per_point.max(1.0);
    let max_x = (rect.right() - inset).max(rect.left());
    let max_y = (rect.bottom() - inset).max(rect.top());
    egui::pos2(pos.x.clamp(rect.left(), max_x), pos.y.clamp(rect.top(), max_y))
}

fn pointer_escape_position(
    rect: egui::Rect,
    attempted_pos: egui::Pos2,
    pixels_per_point: f32,
) -> egui::Pos2 {
    let outward = 6.0 / pixels_per_point.max(1.0);
    let clamped_x = attempted_pos.x.clamp(rect.left(), rect.right());
    let clamped_y = attempted_pos.y.clamp(rect.top(), rect.bottom());
    let overflow_left = (rect.left() - attempted_pos.x).max(0.0);
    let overflow_right = (attempted_pos.x - rect.right()).max(0.0);
    let overflow_top = (rect.top() - attempted_pos.y).max(0.0);
    let overflow_bottom = (attempted_pos.y - rect.bottom()).max(0.0);

    let horizontal_overflow = overflow_left.max(overflow_right);
    let vertical_overflow = overflow_top.max(overflow_bottom);
    if horizontal_overflow >= vertical_overflow {
        if overflow_left > 0.0 {
            egui::pos2(rect.left() - outward, clamped_y)
        } else if overflow_right > 0.0 {
            egui::pos2(rect.right() + outward, clamped_y)
        } else if overflow_top > 0.0 {
            egui::pos2(clamped_x, rect.top() - outward)
        } else {
            egui::pos2(clamped_x, rect.bottom() + outward)
        }
    } else if overflow_top > 0.0 {
        egui::pos2(clamped_x, rect.top() - outward)
    } else if overflow_bottom > 0.0 {
        egui::pos2(clamped_x, rect.bottom() + outward)
    } else if overflow_left > 0.0 {
        egui::pos2(rect.left() - outward, clamped_y)
    } else {
        egui::pos2(rect.right() + outward, clamped_y)
    }
}

fn pos_near_video_edge(pos: egui::Pos2, rect: egui::Rect, pixels_per_point: f32) -> bool {
    let margin = (24.0 / pixels_per_point.max(1.0)).max(12.0);
    pos.x <= rect.left() + margin
        || pos.x >= rect.right() - margin
        || pos.y <= rect.top() + margin
        || pos.y >= rect.bottom() - margin
}

fn remap_pos_between_video_rects(
    pos: egui::Pos2,
    old_rect: egui::Rect,
    new_rect: egui::Rect,
) -> egui::Pos2 {
    let old_width = old_rect.width().max(1.0);
    let old_height = old_rect.height().max(1.0);
    let normalized_x = ((pos.x - old_rect.left()) / old_width).clamp(0.0, 1.0);
    let normalized_y = ((pos.y - old_rect.top()) / old_height).clamp(0.0, 1.0);
    egui::pos2(
        new_rect.left() + normalized_x * new_rect.width().max(1.0),
        new_rect.top() + normalized_y * new_rect.height().max(1.0),
    )
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

fn drag_capture_button_mask(button: egui::PointerButton) -> u8 {
    match button {
        egui::PointerButton::Primary => MOUSE_BUTTON_PRIMARY,
        egui::PointerButton::Secondary => MOUSE_BUTTON_SECONDARY,
        _ => 0,
    }
}

fn should_enter_relative_button_drag_capture(
    capture_mode: LocalCaptureMode,
    controller_state: ControllerState,
    hidden_cursor_relative_drag: bool,
    button: egui::PointerButton,
    pressed: bool,
    over_video: bool,
    over_local_overlay: bool,
) -> bool {
    capture_mode == LocalCaptureMode::HoverAbsolute
        && controller_state == ControllerState::OwnedByYou
        && hidden_cursor_relative_drag
        && drag_capture_button_mask(button) != 0
        && pressed
        && over_video
        && !over_local_overlay
}

fn should_return_to_hover_after_relative_button_drag(
    capture_mode: LocalCaptureMode,
    resume_hover_after_relative_drag: bool,
    button: egui::PointerButton,
    pressed: bool,
    pointer_buttons: u8,
) -> bool {
    let drag_buttons = MOUSE_BUTTON_PRIMARY | MOUSE_BUTTON_SECONDARY;
    capture_mode == LocalCaptureMode::CapturedRelative
        && resume_hover_after_relative_drag
        && drag_capture_button_mask(button) != 0
        && !pressed
        && pointer_buttons & drag_buttons == 0
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

/// Draw a simple monitor icon with signal waves. `center_top` is the top-center of the icon area.
fn paint_monitor_icon(painter: &egui::Painter, center_top: egui::Pos2, connected: bool) {
    let cx = center_top.x;
    let top = center_top.y;

    // Monitor body (rounded rect)
    let mon_w = 56.0;
    let mon_h = 40.0;
    let mon_rect = egui::Rect::from_min_size(
        egui::pos2(cx - mon_w / 2.0, top + 14.0),
        egui::vec2(mon_w, mon_h),
    );
    painter.rect(
        mon_rect,
        4.0,
        egui::Color32::from_rgb(60, 64, 74),
        egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 94, 104)),
        egui::StrokeKind::Outside,
    );

    // Screen inner area
    let screen = mon_rect.shrink(4.0);
    let screen_color = if connected {
        egui::Color32::from_rgb(40, 50, 70)
    } else {
        egui::Color32::from_rgb(30, 33, 40)
    };
    painter.rect_filled(screen, 2.0, screen_color);

    // Stand
    let stand_w = 16.0;
    let stand_h = 6.0;
    painter.rect_filled(
        egui::Rect::from_min_size(
            egui::pos2(cx - stand_w / 2.0, mon_rect.bottom()),
            egui::vec2(stand_w, stand_h),
        ),
        0.0,
        egui::Color32::from_rgb(70, 74, 84),
    );
    // Base
    let base_w = 28.0;
    painter.rect_filled(
        egui::Rect::from_min_size(
            egui::pos2(cx - base_w / 2.0, mon_rect.bottom() + stand_h),
            egui::vec2(base_w, 3.0),
        ),
        1.0,
        egui::Color32::from_rgb(70, 74, 84),
    );

    // Signal waves above monitor (if connected)
    if connected {
        let wave_color = egui::Color32::from_rgb(120, 200, 255);
        let wave_cx = cx;
        let wave_top = top;
        for i in 0..3 {
            let r = 6.0 + i as f32 * 5.0;
            let stroke = egui::Stroke::new(1.2, wave_color.gamma_multiply(1.0 - i as f32 * 0.25));
            // Draw arc segments (approximated with small line segments)
            let segments = 8;
            let start_angle = -std::f32::consts::FRAC_PI_4;
            let end_angle = std::f32::consts::FRAC_PI_4;
            let mut points = Vec::with_capacity(segments + 1);
            for s in 0..=segments {
                let t = s as f32 / segments as f32;
                let angle = start_angle + t * (end_angle - start_angle) - std::f32::consts::FRAC_PI_2;
                points.push(egui::pos2(
                    wave_cx + r * angle.cos(),
                    wave_top + 8.0 + r * angle.sin(),
                ));
            }
            painter.add(egui::Shape::line(points, stroke));
        }
    }
}

fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        let end = s
            .char_indices()
            .nth(max_len.saturating_sub(1))
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        &s[..end]
    }
}

fn render_parsec_toggle(
    ui: &mut egui::Ui,
    title: &str,
    description: &str,
    enabled: bool,
    bg: egui::Color32,
) -> bool {
    let mut clicked = false;
    egui::Frame::NONE
        .fill(bg)
        .corner_radius(6)
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(title)
                            .size(14.0)
                            .strong()
                            .color(egui::Color32::from_rgb(230, 233, 240)),
                    );
                    ui.label(
                        egui::RichText::new(description)
                            .size(12.0)
                            .color(egui::Color32::from_rgb(138, 142, 150)),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (label, fill) = if enabled {
                        ("ON", egui::Color32::from_rgb(38, 146, 108))
                    } else {
                        ("OFF", egui::Color32::from_rgb(60, 64, 74))
                    };
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(label)
                                    .size(12.0)
                                    .strong()
                                    .color(egui::Color32::from_rgb(230, 233, 240)),
                            )
                            .fill(fill)
                            .corner_radius(4)
                            .min_size(egui::vec2(54.0, 28.0)),
                        )
                        .clicked()
                    {
                        clicked = true;
                    }
                });
            });
        });
    clicked
}

fn about_platform_label() -> &'static str {
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

fn about_format_refresh(refresh_millihz: Option<u32>) -> String {
    match refresh_millihz {
        Some(mhz) => format!("{:.1} Hz", mhz as f64 / 1000.0),
        None => "not detected".to_string(),
    }
}

fn about_native_surface(caps: video_frame::NativeSurfaceCapabilities) -> &'static str {
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

fn about_codec_summary(report: decode::VideoCodecSupportReport) -> String {
    use st_protocol::VideoCodec;
    let mut entries = Vec::new();
    for (codec, name) in [
        (VideoCodec::H264, "h264"),
        (VideoCodec::Hevc, "hevc"),
        (VideoCodec::Av1, "av1"),
    ] {
        if report.supported.supports(codec) {
            let suffix = if report.hardware.supports(codec) {
                "hw"
            } else {
                "sw"
            };
            entries.push(format!("{name}({suffix})"));
        }
    }
    if entries.is_empty() {
        "-".to_string()
    } else {
        entries.join(" / ")
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
    let stream_line = input_snapshot
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

    let general_lines = vec![
        format!("server: {}", snapshot.server_addr),
        format!("stream: {stream_line}"),
        format!(
            "display: {}  audio={}  decoder={}  encoder={}  capture={}  input={}  quality={}",
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
            },
            if snapshot.quality_preset.is_empty() {
                "-"
            } else {
                snapshot.quality_preset.as_str()
            }
        ),
        format!(
            "input: controller={:?} mode={} client_id={} keys={} buttons=0x{pointer_buttons:02x} caps=abs:{} rel:{} kb:{} cursor:{} hover:{}",
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
            },
            if input_snapshot.capabilities.hover_capture {
                "y"
            } else {
                "n"
            }
        ),
        format!(
            "frame: id={} format={} present_path={}",
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
            }
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
        self.process_update_events();
        let state = self.state.lock().unwrap().clone();
        if matches!(
            state,
            ConnectionState::Disconnected | ConnectionState::Error(_)
        ) {
            self.clear_local_session_interaction();
            self.video_texture.clear_frame();
        }
        // Auto-reconnect after codec fallback: the connection thread set state to
        // Disconnected and added the failed codec to the exclusion list.
        if state == ConnectionState::Disconnected
            && !self.excluded_video_codecs.lock().unwrap().is_empty()
            && !self.server_addr.trim().is_empty()
        {
            self.connect(ctx.clone());
            return;
        }
        self.keep_awake
            .set_active(matches!(state, ConnectionState::Connected));
        self.suppress_pointer_pos_frames = self.suppress_pointer_pos_frames.saturating_sub(1);
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
        if matches!(
            self.update_ui_state,
            UpdateUiState::Checking | UpdateUiState::Downloading { .. }
        ) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if matches!(self.update_ui_state, UpdateUiState::ClosingForUpdate { .. }) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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

        let debug_top = if state == ConnectionState::Connected {
            self.render_floating_menu(ctx)
        } else {
            0.0
        };

        let central_panel = if state == ConnectionState::Connected {
            #[cfg(target_os = "macos")]
            let frame = egui::Frame::NONE;
            #[cfg(not(target_os = "macos"))]
            let frame = egui::Frame::NONE.fill(egui::Color32::from_rgb(7, 10, 14));

            egui::CentralPanel::default().frame(frame)
        } else {
            egui::CentralPanel::default()
        };

        central_panel.show(ctx, |ui| match &state {
            ConnectionState::Disconnected => {
                self.render_home_screen(ui, ctx);
            }
            ConnectionState::Connecting => {
                let rect = ui.max_rect();
                ui.painter()
                    .rect_filled(rect, 0.0, egui::Color32::from_rgb(26, 30, 38));
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.spinner();
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("Connecting...")
                            .size(16.0)
                            .color(egui::Color32::from_rgb(230, 233, 240)),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(&self.server_addr)
                            .size(12.0)
                            .monospace()
                            .color(egui::Color32::from_rgb(138, 142, 150)),
                    );
                    ui.add_space(20.0);
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Cancel")
                                    .size(13.0)
                                    .color(egui::Color32::from_rgb(230, 233, 240)),
                            )
                            .fill(egui::Color32::from_rgb(52, 56, 66))
                            .corner_radius(4)
                            .min_size(egui::vec2(100.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.disconnect();
                    }
                });
            }
            ConnectionState::Connected => {
                self.paint_connected_background(ui);
                if self.video_texture.has_frame() {
                    let available = ui.available_size();
                    let tex_size = self.video_space_size().unwrap_or_else(|| self.video_texture.size_vec2());
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
                        self.handle_connected_video_response(ctx, &response);
                    });
                } else {
                    self.last_video_rect = None;
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() / 3.0);
                        ui.spinner();
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new("Waiting for video...")
                                .size(14.0)
                                .color(egui::Color32::from_rgb(138, 142, 150)),
                        );
                    });
                }
            }
            ConnectionState::Error(msg) => {
                let rect = ui.max_rect();
                ui.painter()
                    .rect_filled(rect, 0.0, egui::Color32::from_rgb(26, 30, 38));
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.label(
                        egui::RichText::new("Connection Failed")
                            .size(20.0)
                            .strong()
                            .color(egui::Color32::from_rgb(230, 233, 240)),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(msg)
                            .size(13.0)
                            .color(egui::Color32::from_rgb(220, 100, 100)),
                    );
                    ui.add_space(20.0);
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Back")
                                    .size(13.0)
                                    .color(egui::Color32::from_rgb(230, 233, 240)),
                            )
                            .fill(egui::Color32::from_rgb(52, 56, 66))
                            .corner_radius(4)
                            .min_size(egui::vec2(100.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.video_texture.clear_frame();
                        *self.state.lock().unwrap() = ConnectionState::Disconnected;
                    }
                });
            }
        });

        if state == ConnectionState::Connected {
            let snapshot = self.debug_state.snapshot();
            if self.debug_enabled {
                self.graph_overlay.push(&snapshot);
                self.graph_overlay.render(ctx);
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

        self.apply_pointer_capture_mode(ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if *self.state.lock().unwrap() == ConnectionState::Connected {
            return [0.0, 0.0, 0.0, 0.0];
        }

        #[cfg(target_os = "macos")]
        {
            egui::Color32::from_rgb(12, 12, 12).to_normalized_gamma_f32()
        }

        #[cfg(not(target_os = "macos"))]
        {
            egui::Color32::from_rgba_unmultiplied(12, 12, 12, 180).to_normalized_gamma_f32()
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if *self.state.lock().unwrap() != ConnectionState::Connected {
            return;
        }

        let force_release = raw_input.events.iter().any(|event| match event {
            egui::Event::Key {
                key: egui::Key::Z,
                pressed: true,
                modifiers,
                ..
            } => is_force_release_shortcut(modifiers),
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
                        key: egui::Key::Z,
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

        let video_rect = raw_input
            .screen_rect
            .and_then(|rect| self.video_rect_for_container(rect))
            .or_else(|| self.current_video_rect(ctx))
            .or(self.last_video_rect);
        let virtual_hover =
            self.capture_mode == LocalCaptureMode::HoverAbsolute
                && self.uses_virtual_hover_cursor(&snapshot);
        let mut last_pointer_pos = raw_input
            .events
            .iter()
            .rev()
            .find_map(|event| match event {
                egui::Event::PointerMoved(pos) => Some(*pos),
                _ => None,
            })
            .or(if virtual_hover { self.hover_cursor_pos } else { None });
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
                    if !virtual_hover {
                        last_pointer_pos = Some(pos);
                    }
                    if self.capture_mode == LocalCaptureMode::HoverAbsolute
                        && self.hover_cursor_resync_pending
                        && self.suppress_pointer_pos_frames == 0
                    {
                        if let Some(rect) = video_rect {
                            self.hover_cursor_pos = Some(clamp_pos_to_video_rect(
                                pos,
                                rect,
                                ctx.pixels_per_point(),
                            ));
                        } else {
                            self.hover_cursor_pos = Some(pos);
                        }
                        self.hover_cursor_resync_pending = false;
                    }
                }
                egui::Event::MouseMoved(delta) => {
                    if self.suppress_mouse_delta {
                        continue;
                    }
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
                            if self.resume_hover_after_relative_drag {
                                if let Some(rect) = video_rect {
                                    let Some(base_pos) = self
                                        .hover_cursor_pos
                                        .or(last_pointer_pos) else { continue };
                                    let next_pos = clamp_pos_to_video_rect(
                                        egui::pos2(base_pos.x + delta.x, base_pos.y + delta.y),
                                        rect,
                                        ctx.pixels_per_point(),
                                    );
                                    self.hover_cursor_pos = Some(next_pos);
                                    last_pointer_pos = Some(next_pos);
                                }
                            }
                            ctx.request_repaint();
                        }
                    } else if virtual_hover
                        && snapshot.controller_state == ControllerState::OwnedByYou
                    {
                        if let Some(rect) = video_rect {
                            let hover_drag_active = self.capture_mode == LocalCaptureMode::HoverAbsolute
                                && self.pointer_buttons != 0;
                            let base_pos = self
                                .hover_cursor_pos
                                .or(last_pointer_pos)
                                .unwrap_or_else(|| rect.center());
                            let unclamped =
                                egui::pos2(base_pos.x + delta.x, base_pos.y + delta.y);
                            if self.pointer_over_local_overlay(unclamped) {
                                if hover_drag_active {
                                    let next_pos = clamp_pos_to_video_rect(
                                        unclamped,
                                        rect,
                                        ctx.pixels_per_point(),
                                    );
                                    self.hover_cursor_pos = Some(next_pos);
                                    last_pointer_pos = Some(next_pos);
                                    self.send_absolute_cursor_if_needed(client_id, next_pos, rect);
                                    ctx.request_repaint();
                                    continue;
                                }
                                self.auto_release_capture(false);
                                last_pointer_pos = Some(unclamped);
                                ctx.send_viewport_cmd(egui::ViewportCommand::CursorPosition(unclamped));
                                ctx.request_repaint();
                                continue;
                            }
                            if unclamped.x < rect.left()
                                || unclamped.x > rect.right()
                                || unclamped.y < rect.top()
                                || unclamped.y > rect.bottom()
                            {
                                if hover_drag_active {
                                    let next_pos = clamp_pos_to_video_rect(
                                        unclamped,
                                        rect,
                                        ctx.pixels_per_point(),
                                    );
                                    self.hover_cursor_pos = Some(next_pos);
                                    last_pointer_pos = Some(next_pos);
                                    self.send_absolute_cursor_if_needed(client_id, next_pos, rect);
                                    ctx.request_repaint();
                                    continue;
                                }
                                let escape_pos = pointer_escape_position(rect, unclamped, ctx.pixels_per_point());
                                self.auto_release_capture(true);
                                last_pointer_pos = Some(escape_pos);
                                ctx.send_viewport_cmd(egui::ViewportCommand::CursorPosition(escape_pos));
                                ctx.request_repaint();
                                continue;
                            }
                            let next_pos = clamp_pos_to_video_rect(
                                unclamped,
                                rect,
                                ctx.pixels_per_point(),
                            );
                            self.hover_cursor_pos = Some(next_pos);
                            last_pointer_pos = Some(next_pos);
                            self.send_absolute_cursor_if_needed(client_id, next_pos, rect);
                            ctx.request_repaint();
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
                    let route_pos = if self.capture_mode == LocalCaptureMode::HoverAbsolute {
                        self.hover_cursor_pos.or(Some(pos))
                    } else if virtual_hover {
                        self.hover_cursor_pos
                    } else {
                        Some(pos)
                    };
                    let over_video = route_pos
                        .and_then(|pos| {
                            video_rect.map(|rect| {
                                rect.contains(pos) && !self.pointer_over_local_overlay(pos)
                            })
                        })
                        .unwrap_or(false);
                    let over_local_overlay = route_pos
                        .map(|pos| self.pointer_over_local_overlay(pos))
                        .unwrap_or(false);
                    let hidden_cursor_relative_drag = snapshot.capabilities.separate_cursor
                        && snapshot.capabilities.mouse_relative
                        && !snapshot.cursor_state.visible;
                    let enter_relative_drag = should_enter_relative_button_drag_capture(
                        self.capture_mode,
                        snapshot.controller_state,
                        hidden_cursor_relative_drag,
                        button,
                        pressed,
                        over_video,
                        over_local_overlay,
                    );
                    let restore_hover_after_drag = should_return_to_hover_after_relative_button_drag(
                        self.capture_mode,
                        self.resume_hover_after_relative_drag,
                        button,
                        pressed,
                        self.pointer_buttons,
                    );
                    let mut sent_button_state = false;
                    if enter_relative_drag {
                        if let (Some(route_pos), Some(rect)) = (route_pos, video_rect) {
                            self.hover_cursor_pos = Some(clamp_pos_to_video_rect(
                                route_pos,
                                rect,
                                ctx.pixels_per_point(),
                            ));
                            sent_button_state =
                                self.send_absolute_cursor(client_id, route_pos, rect, true);
                        }
                        self.capture_mode = LocalCaptureMode::CapturedRelative;
                        self.resume_hover_after_relative_drag = true;
                        self.hover_cursor_resync_pending = false;
                        self.await_pointer_exit_after_auto_release = false;
                        ctx.request_repaint();
                    }
                    if snapshot.controller_state == ControllerState::OwnedByYou
                        && !sent_button_state
                        && !over_local_overlay
                        && (self.capture_mode == LocalCaptureMode::CapturedRelative
                            || self.capture_mode == LocalCaptureMode::HoverAbsolute && over_video)
                    {
                        if self.capture_mode == LocalCaptureMode::HoverAbsolute {
                            if let (Some(route_pos), Some(rect)) = (route_pos, video_rect) {
                                let _ =
                                    self.send_absolute_cursor(client_id, route_pos, rect, true);
                            } else {
                                self.send_input_packet(InputPacket::MouseButtons(
                                    MouseButtonsInput {
                                        client_id,
                                        buttons: self.pointer_buttons,
                                    },
                                ));
                            }
                        } else {
                            self.send_input_packet(InputPacket::MouseButtons(MouseButtonsInput {
                                client_id,
                                buttons: self.pointer_buttons,
                            }));
                        }
                    }
                    if restore_hover_after_drag {
                        self.capture_mode = LocalCaptureMode::HoverAbsolute;
                        self.resume_hover_after_relative_drag = false;
                        if self.hover_cursor_pos.is_none() {
                            self.hover_cursor_pos = route_pos.filter(|pos| {
                                video_rect
                                    .map(|rect| {
                                        rect.contains(*pos) && !self.pointer_over_local_overlay(*pos)
                                    })
                                    .unwrap_or(false)
                            });
                        }
                        self.hover_cursor_resync_pending = self.hover_cursor_pos.is_some();
                        self.last_sent_absolute_cursor = None;
                        if let Some(pos) = self.hover_cursor_pos {
                            ctx.send_viewport_cmd(egui::ViewportCommand::CursorPosition(pos));
                        }
                        ctx.request_repaint();
                    }
                }
                egui::Event::MouseWheel { delta, unit, .. } => {
                    if snapshot.controller_state != ControllerState::OwnedByYou {
                        continue;
                    }
                    let wheel_pos = if virtual_hover {
                        self.hover_cursor_pos.or(last_pointer_pos)
                    } else {
                        last_pointer_pos
                    };
                    if wheel_pos
                        .map(|pos| self.pointer_over_local_overlay(pos))
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    let over_video = wheel_pos
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

        self.suppress_mouse_delta = false;

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

fn is_force_release_shortcut(modifiers: &egui::Modifiers) -> bool {
    #[cfg(target_os = "macos")]
    {
        modifiers.ctrl && modifiers.command && !modifiers.alt
    }

    #[cfg(not(target_os = "macos"))]
    {
        modifiers.ctrl && modifiers.alt && !modifiers.command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_heartbeat_uses_safe_packets() {
        let absolute = InputPacket::MouseAbsolute(MouseAbsoluteInput {
            client_id: 7,
            x: 100,
            y: 200,
            buttons: MOUSE_BUTTON_PRIMARY,
        });
        assert_eq!(mouse_heartbeat_packet(absolute), Some(absolute));

        let relative = InputPacket::MouseRelative(MouseRelativeInput {
            client_id: 7,
            dx: 3,
            dy: -4,
            buttons: MOUSE_BUTTON_PRIMARY,
        });
        assert_eq!(
            mouse_heartbeat_packet(relative),
            Some(InputPacket::MouseButtons(MouseButtonsInput {
                client_id: 7,
                buttons: MOUSE_BUTTON_PRIMARY,
            }))
        );
    }

    #[test]
    fn mouse_heartbeat_repeats_release_for_repair_window() {
        let client_id = 9;
        let start = Instant::now();
        let mut heartbeat = MouseInputHeartbeat::default();

        let pressed = InputPacket::MouseButtons(MouseButtonsInput {
            client_id,
            buttons: MOUSE_BUTTON_PRIMARY,
        });
        heartbeat.observe(pressed, start);
        heartbeat.mark_sent(pressed, start);

        let release_at = start + Duration::from_millis(10);
        let released = InputPacket::MouseButtons(MouseButtonsInput {
            client_id,
            buttons: 0,
        });
        heartbeat.observe(released, release_at);
        heartbeat.mark_sent(released, release_at);

        assert_eq!(
            heartbeat.due_packet(release_at + INPUT_STATE_HEARTBEAT_INTERVAL),
            Some(released)
        );
        assert_eq!(
            heartbeat.due_packet(release_at + INPUT_STATE_REPAIR_WINDOW + Duration::from_millis(1)),
            None
        );
    }

    #[test]
    fn keyboard_heartbeat_repeats_release_for_repair_window() {
        let client_id = 11;
        let start = Instant::now();
        let mut heartbeat = KeyboardInputHeartbeat::default();
        let mut pressed = [0u8; st_protocol::KEYBOARD_STATE_BYTES];
        let (byte, bit) = KeyboardKey::W.bit();
        pressed[byte] |= bit;

        let down = InputPacket::KeyboardState(KeyboardStateInput { client_id, pressed });
        heartbeat.observe(down, start);
        heartbeat.mark_sent(down, start);

        let release_at = start + Duration::from_millis(10);
        let up = InputPacket::KeyboardState(KeyboardStateInput {
            client_id,
            pressed: [0u8; st_protocol::KEYBOARD_STATE_BYTES],
        });
        heartbeat.observe(up, release_at);
        heartbeat.mark_sent(up, release_at);

        assert_eq!(
            heartbeat.due_packet(release_at + INPUT_STATE_HEARTBEAT_INTERVAL),
            Some(up)
        );
        assert_eq!(
            heartbeat.due_packet(release_at + INPUT_STATE_REPAIR_WINDOW + Duration::from_millis(1)),
            None
        );
    }

    #[test]
    fn hidden_cursor_drag_buttons_enter_relative_drag() {
        assert!(should_enter_relative_button_drag_capture(
            LocalCaptureMode::HoverAbsolute,
            ControllerState::OwnedByYou,
            true,
            egui::PointerButton::Primary,
            true,
            true,
            false,
        ));
        assert!(should_enter_relative_button_drag_capture(
            LocalCaptureMode::HoverAbsolute,
            ControllerState::OwnedByYou,
            true,
            egui::PointerButton::Secondary,
            true,
            true,
            false,
        ));
        assert!(!should_enter_relative_button_drag_capture(
            LocalCaptureMode::HoverAbsolute,
            ControllerState::OwnedByYou,
            false,
            egui::PointerButton::Primary,
            true,
            true,
            false,
        ));
    }

    #[test]
    fn temporary_relative_drag_returns_to_hover_after_drag_buttons_release() {
        assert!(should_return_to_hover_after_relative_button_drag(
            LocalCaptureMode::CapturedRelative,
            true,
            egui::PointerButton::Primary,
            false,
            0,
        ));
        assert!(!should_return_to_hover_after_relative_button_drag(
            LocalCaptureMode::CapturedRelative,
            true,
            egui::PointerButton::Primary,
            false,
            MOUSE_BUTTON_PRIMARY,
        ));
        assert!(!should_return_to_hover_after_relative_button_drag(
            LocalCaptureMode::CapturedRelative,
            true,
            egui::PointerButton::Secondary,
            false,
            MOUSE_BUTTON_SECONDARY,
        ));
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    match updater::maybe_run_apply_update_from_args() {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => {
            eprintln!("[updater] {err}");
            std::process::exit(1);
        }
    }
    updater::cleanup_old_update_files();

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
