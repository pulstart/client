#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod api_client;
mod audio;
mod clipboard;
mod debug_state;
mod decode;
mod display;
mod file_transfer;
mod graph_overlay;
mod input;
mod keep_awake;
#[cfg(target_os = "linux")]
mod linux_uring;
mod pipeline;
mod render;
mod render_gl;
#[cfg(target_os = "macos")]
mod render_macos;
#[cfg(target_os = "macos")]
mod render_macos_metal;
mod render_wgpu;
#[cfg(target_os = "linux")]
mod render_wgpu_linux_dmabuf;
#[cfg(target_os = "windows")]
mod render_windows;
#[cfg(target_os = "windows")]
mod render_windows_native;
mod transport;
mod updater;
mod video_frame;

use eframe::egui;
use input::{LocalCaptureMode, LocalKeyboardState, RemoteCursorTexture, SharedInputState};
use keep_awake::KeepAwakeController;
use render::VideoTexture;
use serde::{Deserialize, Serialize};
use st_protocol::{
    ClientDisplayInfo, ClockSyncPing, ControlMessage, ControllerState, InputPacket, KeyboardKey,
    KeyboardStateInput, MouseAbsoluteInput, MouseButtonsInput, MouseRelativeInput, MouseWheelInput,
    StreamConfig, TransportFeedback, VideoChromaSampling, VideoCodec, VideoCodecSupport,
    MOUSE_BUTTON_EXTRA1, MOUSE_BUTTON_EXTRA2, MOUSE_BUTTON_MIDDLE, MOUSE_BUTTON_PRIMARY,
    MOUSE_BUTTON_SECONDARY, MOUSE_WHEEL_STEP_UNITS,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{Ipv6Addr, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use transport::{AudioPacket, MediaReceiver};
use video_frame::{NativeSurfaceControl, VideoFrameBuffer};

use crate::debug_state::{unix_time_micros, ConnectionDebugSnapshot, ConnectionDebugState};

const DEFAULT_APP_PORT: u16 = 28_480;
const DISCOVERY_PORT: u16 = 28_481;
const DISCOVERY_EXPIRY: Duration = Duration::from_secs(10);
const MAX_REMOTE_CURSOR_TEXTURES: usize = 8;
const INPUT_SENDER_POLL_INTERVAL: Duration = Duration::from_millis(20);
const INPUT_STATE_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(50);
const INPUT_STATE_REPAIR_WINDOW: Duration = Duration::from_millis(200);
const HOVER_EDGE_MISMATCH_UPDATES_THRESHOLD: u8 = 6;
const LOCAL_CURSOR_PREDICTION_HOLD: Duration = Duration::from_millis(80);

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
struct DiscoveredServer {
    hostname: String,
    address: String,
    token: String,
    peer_id: Option<String>,
    last_seen: Instant,
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
    using_local_pos: bool,
}

struct StreamApp {
    server_addr: String,
    server_list: Vec<ServerEntry>,
    add_server_addr: String,
    search_query: String,
    token: String,
    api_discovery: Arc<api_client::ApiDiscoveryShared>,
    discovered_servers: Arc<Mutex<Vec<DiscoveredServer>>>,
    home_tab: HomeTab,
    audio_enabled: bool,
    debug_enabled: bool,
    yuv444_enabled: bool,
    display_refresh_millihz: Option<u32>,
    video_codec_support: decode::VideoCodecSupportReport,
    audio_enabled_flag: Arc<AtomicBool>,
    debug_enabled_flag: Arc<AtomicBool>,
    state: Arc<Mutex<ConnectionState>>,
    frame: Arc<Mutex<VideoFrameBuffer>>,
    upload_frame: VideoFrameBuffer,
    debug_state: Arc<ConnectionDebugState>,
    video_texture: VideoTexture,
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
    last_local_cursor_prediction_at: Option<Instant>,
    resume_hover_after_relative_drag: bool,
    hover_cursor_resync_pending: bool,
    hover_drag_edge_mismatch_updates: u8,
    hover_drag_edge_mismatch_cursor_state_version: u64,
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
    pending_wheel_units: egui::Vec2,
    suppress_mouse_delta: bool,
    suppress_pointer_pos_frames: u8,
    excluded_video_codecs: Arc<Mutex<st_protocol::VideoCodecSupport>>,
    file_transfer_state: file_transfer::SharedTransferState,
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

fn normalize_server_addr(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Some(host) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
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

fn addr_host(addr: &str) -> &str {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some((host, _)) = rest.split_once(']') {
            return host;
        }
    }
    addr.rsplit_once(':').map(|(host, _)| host).unwrap_or(addr)
}

fn addr_ip(addr: &str) -> Option<std::net::IpAddr> {
    addr_host(addr).parse().ok()
}

fn is_privateish_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            if v4.is_private() || v4.is_loopback() || v4.is_link_local() {
                return true;
            }
            // CGNAT range (100.64.0.0/10), which includes Tailscale's typical
            // address space. stdlib's Ipv4Addr::is_private() doesn't cover it,
            // and we don't want hole-punch fallback firing on a Tailscale IP
            // since punching to a CGNAT address has no chance of working.
            let o = v4.octets();
            o[0] == 100 && (64..=127).contains(&o[1])
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
        }
    }
}

fn is_public_addr(addr: &str) -> bool {
    addr_ip(addr)
        .map(|ip| !is_privateish_ip(ip))
        .unwrap_or(false)
}

fn preferred_api_display_addr(host: &api_client::ApiDiscoveredHost) -> Option<String> {
    host.candidates
        .iter()
        .find(|candidate| is_public_addr(candidate))
        .cloned()
        .or_else(|| host.candidates.first().cloned())
}

fn allow_hole_punch_fallback(socket_addr: std::net::SocketAddr) -> bool {
    !is_privateish_ip(socket_addr.ip())
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

fn load_yuv444_enabled() -> bool {
    std::fs::read_to_string(state_dir().join("yuv444_enabled"))
        .ok()
        .map(|s| s.trim() != "0")
        .unwrap_or(true)
}

fn save_yuv444_enabled(enabled: bool) {
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("yuv444_enabled"), if enabled { "1" } else { "0" });
}

fn load_token() -> String {
    std::fs::read_to_string(state_dir().join("token"))
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn save_token(token: &str) {
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("token"), token);
}

fn load_or_create_peer_id() -> String {
    let path = state_dir().join("peer_id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }
    // Generate and persist a new one.
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let s = RandomState::new();
    let mut h = s.build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    let a = h.finish();
    let mut h2 = s.build_hasher();
    h2.write_u64(a ^ 0xdeadbeef);
    let b = h2.finish();
    let id = format!("{a:016x}{b:016x}");
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, &id);
    id
}

/// Hardcoded API server URL. Override at build time or via ST_API_URL env var.
const API_SERVER_URL: &str = "https://st-api.kubemaxx.io";

fn resolve_api_url() -> String {
    std::env::var("ST_API_URL")
        .unwrap_or_else(|_| API_SERVER_URL.to_string())
        .trim_end_matches('/')
        .to_string()
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
    let _ = std::fs::write(
        dir.join("menu_button_pos"),
        format!("{:.1} {:.1}", pos.x, pos.y),
    );
}

// ---------------------------------------------------------------------------
// Server list persistence
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ServerEntry {
    address: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    peer_id: Option<String>,
    /// Unix timestamp (seconds) of last successful connection, 0 if never.
    #[serde(default)]
    last_connected: u64,
    /// True only for entries the user explicitly added.
    #[serde(default)]
    manually_added: bool,
}

fn load_server_list() -> Vec<ServerEntry> {
    let path = state_dir().join("servers.json");
    match std::fs::read_to_string(&path) {
        Ok(data) => {
            let mut list: Vec<ServerEntry> = serde_json::from_str(&data).unwrap_or_default();
            let original_len = list.len();
            list.retain(|entry| entry.manually_added);
            if list.len() != original_len {
                save_server_list(&list);
            }
            list
        }
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

/// Ensure the given address is in the manually added server list.
fn ensure_server_in_list(list: &mut Vec<ServerEntry>, addr: &str) -> bool {
    let normalized = normalize_server_addr(addr);
    if normalized.is_empty() {
        return false;
    }
    if let Some(existing) = list.iter_mut().find(|s| s.address == normalized) {
        if !existing.manually_added {
            existing.manually_added = true;
            return true;
        }
        return false;
    }
    list.push(ServerEntry {
        address: normalized,
        nickname: String::new(),
        peer_id: None,
        last_connected: 0,
        manually_added: true,
    });
    true
}

fn touch_server_connected(list: &mut Vec<ServerEntry>, addr: &str) {
    let normalized = normalize_server_addr(addr);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(entry) = list.iter_mut().find(|s| s.address == normalized) {
        entry.last_connected = now;
        save_server_list(list);
    }
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
// Discovery listener
// ---------------------------------------------------------------------------

fn start_discovery_listener(discovered: Arc<Mutex<Vec<DiscoveredServer>>>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let sock = match std::net::UdpSocket::bind(format!("0.0.0.0:{DISCOVERY_PORT}")) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("[discovery] Failed to bind listener on port {DISCOVERY_PORT}: {err}");
                return;
            }
        };
        // Short timeout so we can prune expired servers promptly
        let _ = sock.set_read_timeout(Some(Duration::from_secs(3)));

        let mut buf = [0u8; 512];
        loop {
            let changed;
            match sock.recv_from(&mut buf) {
                Ok((n, src_addr)) => {
                    let data = String::from_utf8_lossy(&buf[..n]);
                    let lines: Vec<&str> = data.lines().collect();
                    if lines.len() >= 4 && lines[0] == "ST_DISCOVER" {
                        let hostname = lines[1].to_string();
                        let port: u16 = lines[2].parse().unwrap_or(DEFAULT_APP_PORT);
                        let token = lines[3].to_string();
                        let peer_id = lines
                            .get(4)
                            .map(|value| value.trim())
                            .filter(|value| !value.is_empty())
                            .map(str::to_string);
                        let address = format!("{}:{port}", src_addr.ip());

                        let mut servers = discovered.lock().unwrap();
                        let before = servers.len();
                        servers.retain(|s| s.last_seen.elapsed() < DISCOVERY_EXPIRY);
                        if let Some(existing) = servers.iter_mut().find(|s| s.address == address) {
                            existing.hostname = hostname;
                            existing.token = token;
                            existing.peer_id = peer_id;
                            existing.last_seen = Instant::now();
                            changed = servers.len() != before;
                        } else {
                            servers.push(DiscoveredServer {
                                hostname,
                                address,
                                token,
                                peer_id,
                                last_seen: Instant::now(),
                            });
                            changed = true;
                        }
                    } else {
                        changed = false;
                    }
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    let mut servers = discovered.lock().unwrap();
                    let before = servers.len();
                    servers.retain(|s| s.last_seen.elapsed() < DISCOVERY_EXPIRY);
                    changed = servers.len() != before;
                }
                Err(_) => {
                    changed = false;
                }
            }
            if changed {
                ctx.request_repaint();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// StreamApp
// ---------------------------------------------------------------------------

impl StreamApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let server_list = load_server_list();
        let token = load_token();
        let audio = load_audio_enabled();
        let debug_enabled = load_debug_enabled();
        let yuv444_enabled = load_yuv444_enabled();
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
        let video_texture = VideoTexture::new(cc);
        let native_surfaces = Arc::new(NativeSurfaceControl::new(
            video_texture.native_surface_capabilities(),
        ));
        let discovered_servers = Arc::new(Mutex::new(Vec::<DiscoveredServer>::new()));
        start_discovery_listener(Arc::clone(&discovered_servers), cc.egui_ctx.clone());
        let peer_id = load_or_create_peer_id();
        let api_discovery = Arc::new(api_client::ApiDiscoveryShared::new(
            resolve_api_url(),
            token.clone(),
            peer_id,
        ));
        api_client::start_api_discovery(Arc::clone(&api_discovery), cc.egui_ctx.clone());
        // Ask the client's router for an explicit external port forward
        // (PCP / NAT-PMP). When successful, the resulting candidate works
        // even on symmetric NATs and survives idle periods. Quiet no-op
        // if the gateway speaks neither protocol.
        api_client::start_port_mapping(Arc::clone(&api_discovery));
        Self {
            server_addr: String::new(),
            server_list,
            add_server_addr: String::new(),
            search_query: String::new(),
            token,
            api_discovery,
            discovered_servers,
            home_tab: HomeTab::Servers,
            audio_enabled: audio,
            debug_enabled,
            yuv444_enabled,
            display_refresh_millihz,
            video_codec_support,
            audio_enabled_flag: Arc::new(AtomicBool::new(audio)),
            debug_enabled_flag: Arc::new(AtomicBool::new(debug_enabled)),
            state: Arc::new(Mutex::new(ConnectionState::Disconnected)),
            frame: Arc::new(Mutex::new(VideoFrameBuffer::default())),
            upload_frame: VideoFrameBuffer::default(),
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
            last_local_cursor_prediction_at: None,
            resume_hover_after_relative_drag: false,
            hover_cursor_resync_pending: false,
            hover_drag_edge_mismatch_updates: 0,
            hover_drag_edge_mismatch_cursor_state_version: 0,
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
            pending_wheel_units: egui::Vec2::ZERO,
            suppress_mouse_delta: false,
            suppress_pointer_pos_frames: 0,
            excluded_video_codecs: Arc::new(Mutex::new(st_protocol::VideoCodecSupport::empty())),
            file_transfer_state: file_transfer::new_shared_state(),
            update_ui_state,
            update_tx,
            update_rx,
        }
    }

    fn connect(&mut self, ctx: egui::Context) {
        let saved_addr = self.server_addr.trim().to_string();
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
        self.last_local_cursor_prediction_at = None;
        self.pending_wheel_units = egui::Vec2::ZERO;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.hover_drag_edge_mismatch_updates = 0;
        self.hover_drag_edge_mismatch_cursor_state_version = 0;
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
        let video_codec_support =
            advertised_video_codec_support(self.video_codec_support, self.yuv444_enabled);
        let excluded_video_codecs = Arc::clone(&self.excluded_video_codecs);
        let shared_input = Arc::clone(&self.shared_input);
        let token = self.token.clone();
        let api_disc = Arc::clone(&self.api_discovery);
        let ft_state = file_transfer::new_shared_state();
        self.file_transfer_state = Arc::clone(&ft_state);
        debug_state.reset_for_connect(&addr, display_refresh_millihz);

        std::thread::spawn(move || {
            run_connection(
                addr,
                token,
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
                api_disc,
                ft_state,
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
        self.last_local_cursor_prediction_at = None;
        self.pending_wheel_units = egui::Vec2::ZERO;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.hover_drag_edge_mismatch_updates = 0;
        self.hover_drag_edge_mismatch_cursor_state_version = 0;
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
        self.last_local_cursor_prediction_at = None;
        self.pending_wheel_units = egui::Vec2::ZERO;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.hover_drag_edge_mismatch_updates = 0;
        self.hover_drag_edge_mismatch_cursor_state_version = 0;
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
        self.last_local_cursor_prediction_at = None;
        self.pending_wheel_units = egui::Vec2::ZERO;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.hover_drag_edge_mismatch_updates = 0;
        self.hover_drag_edge_mismatch_cursor_state_version = 0;
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
        self.last_local_cursor_prediction_at = None;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.hover_drag_edge_mismatch_updates = 0;
        self.hover_drag_edge_mismatch_cursor_state_version = 0;
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
        let snapshot = self.shared_input.snapshot();
        let preserved_cursor_pos = if self.capture_mode == LocalCaptureMode::CapturedRelative
            && snapshot.capabilities.separate_cursor
            && snapshot.cursor_state.visible
        {
            self.hover_cursor_pos.or_else(|| {
                self.last_video_rect
                    .and_then(|rect| self.mapped_server_cursor_video_pos(&snapshot, rect))
            })
        } else {
            None
        };
        self.release_pointer_capture();
        if let Some(pos) = preserved_cursor_pos {
            self.hover_cursor_pos = Some(pos);
        }
        self.await_pointer_exit_after_auto_release = await_pointer_exit_before_recapture;
        self.clear_remote_keyboard();
        if let Some(client_id) = snapshot.client_id {
            self.send_input_packet(InputPacket::MouseButtons(MouseButtonsInput {
                client_id,
                buttons: 0,
            }));
        }
    }

    fn keyboard_forward_active(&self, snapshot: &input::SharedInputSnapshot) -> bool {
        controller_state_allows_input(snapshot.controller_state)
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

    fn send_remote_wheel(&mut self, client_id: u32, delta: egui::Vec2, unit: egui::MouseWheelUnit) {
        self.pending_wheel_units += delta * wheel_unit_scale(unit);
        let delta_x = take_wheel_units(&mut self.pending_wheel_units.x);
        let delta_y = take_wheel_units(&mut self.pending_wheel_units.y);
        if delta_x != 0 || delta_y != 0 {
            self.send_input_packet(InputPacket::MouseWheel(MouseWheelInput {
                client_id,
                delta_x,
                delta_y,
                buttons: self.pointer_buttons,
            }));
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
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(rect) = self.video_texture.current_native_video_rect() {
            return Some(rect);
        }

        self.video_rect_for_container(ctx.content_rect())
    }

    fn prefers_windows_overlayless_present(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            self.capture_mode == LocalCaptureMode::CapturedRelative
                && !self.menu_open
                && !self.debug_enabled
        }

        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }

    fn server_cursor_stream_top_left(&self, snapshot: &input::SharedInputSnapshot) -> egui::Pos2 {
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
            && controller_state_allows_input(snapshot.controller_state)
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
        if !controller_state_has_separate_cursor(snapshot.controller_state) {
            return false;
        }

        match self.capture_mode {
            LocalCaptureMode::HoverAbsolute => {
                let pointer_pos = self.hover_cursor_pos.or_else(|| {
                    if self.uses_virtual_hover_cursor(&snapshot) {
                        None
                    } else {
                        ctx.input(|i| i.pointer.latest_pos())
                    }
                });
                pointer_pos
                    .zip(self.current_video_rect(ctx).or(self.last_video_rect))
                    .map(|(pointer_pos, rect)| {
                        rect.contains(pointer_pos) && !self.pointer_over_local_overlay(pointer_pos)
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
            && controller_state_has_separate_cursor(snapshot.controller_state)
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
            && controller_state_allows_input(snapshot.controller_state)
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
                        self.update_ui_state = UpdateUiState::UpToDate { version, html_url };
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
        if !controller_state_has_separate_cursor(input_snapshot.controller_state)
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

        let use_local_cursor_pos = self.capture_mode == LocalCaptureMode::HoverAbsolute
            || self.resume_hover_after_relative_drag
            || (self.capture_mode == LocalCaptureMode::CapturedRelative
                && input_snapshot.capabilities.separate_cursor
                && input_snapshot.cursor_state.visible
                && !matches!(
                    input_snapshot.controller_state,
                    ControllerState::Unavailable | ControllerState::OwnedByOther
                )
                && self.hover_cursor_pos.is_some());
        let (top_left, cursor_pos, using_local_pos) = if use_local_cursor_pos {
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
            using_local_pos,
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
        let active_texture =
            self.remote_cursor_texture_for_serial(input_snapshot.cursor_state.serial);
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
                "mapped: serial={} pos={} rect={} local_driven={}",
                geometry.serial,
                format_pos(geometry.cursor_pos),
                format_rect(geometry.rect),
                if geometry.using_local_pos { "y" } else { "n" },
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

    fn handle_connected_video_response(&mut self, ctx: &egui::Context, response: &egui::Response) {
        let previous_video_rect = self.last_video_rect;
        let video_rect = self.current_video_rect(ctx).unwrap_or(response.rect);
        self.last_video_rect = Some(video_rect);
        let previous_capture_mode = self.capture_mode;

        let snapshot = self.shared_input.snapshot();
        let hover_supported = snapshot.capabilities.hover_capture;
        let prefer_hover_absolute = snapshot.capabilities.hover_capture;
        let hover_drag_active = previous_capture_mode == LocalCaptureMode::HoverAbsolute
            && controller_state_allows_input(snapshot.controller_state)
            && self.pointer_buttons != 0;
        let virtual_hover = self.uses_virtual_hover_cursor(&snapshot);
        let actual_pointer_pos = if self.suppress_pointer_pos_frames > 0 {
            self.hover_cursor_pos
                .or_else(|| ctx.input(|i| i.pointer.latest_pos()))
        } else {
            ctx.input(|i| i.pointer.latest_pos())
        };
        let mut pointer_pos =
            if virtual_hover && previous_capture_mode == LocalCaptureMode::HoverAbsolute {
                self.hover_cursor_pos.or(actual_pointer_pos)
            } else {
                actual_pointer_pos
            };
        if virtual_hover
            && previous_capture_mode == LocalCaptureMode::HoverAbsolute
            && controller_state_allows_input(snapshot.controller_state)
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
        let hover_drag_active = hover_drag_active && pointer_over_video;
        if self.await_pointer_exit_after_auto_release && !pointer_inside_video_rect {
            self.await_pointer_exit_after_auto_release = false;
        }
        if !controller_state_allows_input(snapshot.controller_state)
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
            && controller_state_allows_input(snapshot.controller_state)
        {
            self.capture_mode = LocalCaptureMode::HoverAbsolute;
        }
        let relative_hover_supported = !hover_supported
            && snapshot.capabilities.mouse_relative
            && snapshot.capabilities.separate_cursor
            && snapshot.cursor_state.visible;
        if relative_hover_supported
            && self.capture_mode != LocalCaptureMode::CapturedRelative
            && self.capture_mode != LocalCaptureMode::ForceReleased
            && !self.await_pointer_exit_after_auto_release
            && pointer_over_video
            && controller_state_allows_input(snapshot.controller_state)
        {
            self.capture_mode = LocalCaptureMode::CapturedRelative;
            self.resume_hover_after_relative_drag = false;
            self.hover_cursor_resync_pending = false;
            if let Some(pos) = self.mapped_server_cursor_video_pos(&snapshot, video_rect) {
                self.hover_cursor_pos = Some(clamp_pos_to_video_rect(
                    pos,
                    video_rect,
                    ctx.pixels_per_point(),
                ));
            }
            ctx.request_repaint();
        }

        if clicked_video {
            if controller_state_allows_input(snapshot.controller_state) {
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
                // ControllerState / InputCapabilities haven't landed yet
                // (TCP control messages arrive after StreamStarted).  Remember
                // the click so we re-evaluate capture once the controller
                // transitions out of Unavailable.
                self.pending_capture_click = true;
            }
            ctx.request_repaint();
        }

        if self.pending_capture_click && controller_state_allows_input(snapshot.controller_state) {
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
            && controller_state_allows_input(snapshot.controller_state)
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
            && controller_state_allows_input(snapshot.controller_state)
            && self.pointer_buttons & drag_buttons != 0
            && snapshot.capabilities.separate_cursor
            && snapshot.capabilities.mouse_relative
            && snapshot.cursor_state.visible
        {
            pointer_pos
                .filter(|pos| video_rect.contains(*pos) && !self.pointer_over_local_overlay(*pos))
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
            if snapshot.cursor_state_version != self.hover_drag_edge_mismatch_cursor_state_version {
                self.hover_drag_edge_mismatch_updates =
                    self.hover_drag_edge_mismatch_updates.saturating_add(1);
                self.hover_drag_edge_mismatch_cursor_state_version = snapshot.cursor_state_version;
            }
        } else {
            self.hover_drag_edge_mismatch_updates = 0;
            self.hover_drag_edge_mismatch_cursor_state_version = snapshot.cursor_state_version;
        }
        if self.capture_mode == LocalCaptureMode::HoverAbsolute
            && controller_state_allows_input(snapshot.controller_state)
            && self.pointer_buttons & drag_buttons != 0
            && (hidden_cursor_relative_drag
                || self.hover_drag_edge_mismatch_updates >= HOVER_EDGE_MISMATCH_UPDATES_THRESHOLD)
        {
            if let Some(pos) = pointer_pos
                .filter(|pos| video_rect.contains(*pos) && !self.pointer_over_local_overlay(*pos))
            {
                self.hover_cursor_pos = Some(clamp_pos_to_video_rect(
                    pos,
                    video_rect,
                    ctx.pixels_per_point(),
                ));
            } else if self.hover_cursor_pos.is_none() {
                // Don't fall back to center — skip the transition until we
                // have a real pointer position to anchor the drag.
                self.hover_drag_edge_mismatch_updates = 0;
            }
            if self.hover_cursor_pos.is_some() {
                self.capture_mode = LocalCaptureMode::CapturedRelative;
                self.resume_hover_after_relative_drag = true;
                self.hover_cursor_resync_pending = false;
                self.hover_drag_edge_mismatch_updates = 0;
                self.hover_drag_edge_mismatch_cursor_state_version = snapshot.cursor_state_version;
                ctx.request_repaint();
            }
        }

        if previous_capture_mode != self.capture_mode && self.capture_mode == LocalCaptureMode::Idle
        {
            self.clear_remote_keyboard();
        }

        if previous_capture_mode != LocalCaptureMode::CapturedRelative
            && self.capture_mode == LocalCaptureMode::CapturedRelative
            && !self.resume_hover_after_relative_drag
            && snapshot.capabilities.separate_cursor
            && snapshot.cursor_state.visible
        {
            // In relative mode the server cursor metadata is the source of truth;
            // the local pointer only decides when capture starts or escapes.
            let remote_pos = self.mapped_server_cursor_video_pos(&snapshot, video_rect);
            let local_pos = pointer_pos
                .filter(|pos| video_rect.contains(*pos) && !self.pointer_over_local_overlay(*pos));
            if let Some(anchor_pos) = relative_capture_entry_anchor(remote_pos, local_pos) {
                self.hover_cursor_pos = Some(clamp_pos_to_video_rect(
                    anchor_pos,
                    video_rect,
                    ctx.pixels_per_point(),
                ));
                ctx.request_repaint();
            }
        }

        if self.capture_mode == LocalCaptureMode::HoverAbsolute
            && controller_state_allows_input(snapshot.controller_state)
            && self.capture_mode != LocalCaptureMode::ForceReleased
        {
            if virtual_hover {
                let desired_hover_pos = if previous_capture_mode == LocalCaptureMode::HoverAbsolute
                {
                    self.hover_cursor_pos
                        .or(pointer_pos)
                        .map(|pos| clamp_pos_to_video_rect(pos, video_rect, ctx.pixels_per_point()))
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
        } else if self.capture_mode == LocalCaptureMode::CapturedRelative
            && snapshot.capabilities.separate_cursor
            && snapshot.cursor_state.visible
        {
            let local_entry_pos = if previous_capture_mode != LocalCaptureMode::CapturedRelative
                && !self.resume_hover_after_relative_drag
            {
                pointer_pos.filter(|pos| {
                    video_rect.contains(*pos) && !self.pointer_over_local_overlay(*pos)
                })
            } else {
                None
            };
            let remote_pos = self.mapped_server_cursor_video_pos(&snapshot, video_rect);
            let local_prediction = self.hover_cursor_pos.map(|pos| {
                previous_video_rect
                    .filter(|previous_rect| *previous_rect != video_rect)
                    .map(|previous_rect| {
                        remap_pos_between_video_rects(pos, previous_rect, video_rect)
                    })
                    .unwrap_or(pos)
            });
            let local_prediction_recent = self
                .last_local_cursor_prediction_at
                .map(|at| at.elapsed() <= LOCAL_CURSOR_PREDICTION_HOLD)
                .unwrap_or(false);
            let entering_relative = previous_capture_mode != LocalCaptureMode::CapturedRelative
                && !self.resume_hover_after_relative_drag;
            let predicted_pos = if entering_relative {
                relative_capture_entry_anchor(remote_pos, local_entry_pos)
            } else {
                relative_capture_tracking_anchor(
                    remote_pos,
                    local_prediction,
                    local_prediction_recent,
                )
            };
            self.hover_cursor_pos = predicted_pos
                .map(|pos| clamp_pos_to_video_rect(pos, video_rect, ctx.pixels_per_point()));
        } else if !self.resume_hover_after_relative_drag {
            self.hover_cursor_pos = None;
            self.last_sent_absolute_cursor = None;
        }

        self.draw_remote_cursor_overlay(ctx);
    }

    fn draw_remote_cursor_overlay(&self, ctx: &egui::Context) {
        if self.video_texture.occludes_egui_overlay() {
            return;
        }

        let snapshot = self.shared_input.snapshot();
        if let Some(geometry) = self.compute_cursor_overlay_geometry(ctx, &snapshot) {
            egui::Area::new(egui::Id::new("remote_cursor_overlay"))
                .order(egui::Order::Tooltip)
                .interactable(false)
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
            || !controller_state_has_separate_cursor(snapshot.controller_state)
            || !snapshot.capabilities.hover_capture
            || !video_rect.contains(pos)
            || !snapshot.cursor_state.visible
        {
            return;
        }

        egui::Area::new(egui::Id::new("remote_cursor_fallback_overlay"))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .fixed_pos(video_rect.min)
            .show(ctx, |ui| {
                let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, video_rect.size());
                let local_pos = pos - video_rect.min.to_vec2();
                let painter = ui.painter().with_clip_rect(rect);
                painter.circle_filled(local_pos, 5.0, egui::Color32::WHITE);
                painter.circle_stroke(local_pos, 5.0, egui::Stroke::new(1.5, egui::Color32::BLACK));
            });
    }

    #[cfg(not(target_os = "macos"))]
    fn paint_connected_background(&self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let painter = ui.painter().clone();
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(7, 10, 14));
        painter.circle_filled(
            egui::pos2(
                rect.left() + rect.width() * 0.18,
                rect.top() + rect.height() * 0.22,
            ),
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
        let sidebar_rect =
            egui::Rect::from_min_size(full_rect.min, egui::vec2(SIDEBAR_W, full_rect.height()));
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
            let bottom_rect =
                egui::Rect::from_min_max(egui::pos2(content_left, main_bottom), full_rect.max);
            painter.rect_filled(bottom_rect, 0.0, egui::Color32::from_rgb(30, 34, 42));
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
                if ensure_server_in_list(&mut self.server_list, &addr) {
                    save_server_list(&self.server_list);
                }
                self.add_server_addr.clear();
            }
            bottom_ui.add_space(4.0);
            if bottom_ui
                .add_enabled(
                    can_add,
                    egui::Button::new(egui::RichText::new("Add").size(12.0).color(TEXT_WHITE))
                        .fill(BTN_DARK)
                        .corner_radius(4)
                        .min_size(egui::vec2(50.0, 28.0)),
                )
                .clicked()
            {
                let addr = self.add_server_addr.trim().to_string();
                if ensure_server_in_list(&mut self.server_list, &addr) {
                    save_server_list(&self.server_list);
                }
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
            .wheel_scroll_multiplier(egui::vec2(1.0, 1.35))
            .show(&mut main_ui, |ui| {
                let margin = egui::Margin {
                    left: 32,
                    right: 32,
                    top: 28,
                    bottom: 28,
                };
                egui::Frame::NONE
                    .inner_margin(margin)
                    .show(ui, |ui| match self.home_tab {
                        HomeTab::Servers => self.render_servers_tab(ui, ctx),
                        HomeTab::Settings => self.render_settings_tab(ui),
                        HomeTab::Update => self.render_update_tab(ui, ctx),
                        HomeTab::About => self.render_about_tab(ui),
                    });
            });
    }

    fn render_servers_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
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
        const CARD_ROW_SPACING: f32 = 12.0;

        let painter = ui.painter().clone();

        ui.label(
            egui::RichText::new("Computers")
                .size(32.0)
                .color(TEXT_WHITE),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Connect to your computer in low latency desktop mode.")
                    .size(13.0)
                    .color(TEXT_GRAY),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let api_url = self.api_discovery.api_url.lock().unwrap().clone();
                if !api_url.is_empty() {
                    let connected = self.api_discovery.is_connected();
                    let (dot, label) = if connected {
                        (egui::Color32::from_rgb(80, 200, 120), "Online")
                    } else {
                        (egui::Color32::from_rgb(180, 80, 80), "Offline")
                    };
                    ui.label(egui::RichText::new(label).size(11.0).color(dot));
                    let dot_rect = ui.allocate_space(egui::vec2(8.0, 8.0)).1;
                    ui.painter().circle_filled(dot_rect.center(), 4.0, dot);
                }
            });
        });
        ui.add_space(16.0);

        // Search bar + Reload
        let search_width = ui.available_width().min(500.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("Search Hosts and Computers")
                    .desired_width(search_width),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Reload").size(13.0).color(ACCENT_BLUE),
                        )
                        .fill(egui::Color32::TRANSPARENT),
                    )
                    .clicked()
                {
                    self.server_list = load_server_list();
                    // Prune stale discovered servers so the list is fresh
                    self.discovered_servers
                        .lock()
                        .unwrap()
                        .retain(|s| s.last_seen.elapsed() < DISCOVERY_EXPIRY);
                }
            });
        });
        ui.add_space(20.0);

        // ---- Build unified server list from all sources ----
        const ACCENT_GREEN: egui::Color32 = egui::Color32::from_rgb(80, 200, 120);

        // Collect LAN-discovered servers (token-matched, non-expired).
        let discovered = self.discovered_servers.lock().unwrap().clone();
        let lan_servers: Vec<&DiscoveredServer> = discovered
            .iter()
            .filter(|d| d.token == self.token && !self.token.is_empty())
            .filter(|d| d.last_seen.elapsed() < DISCOVERY_EXPIRY)
            .collect();
        let lan_addrs: BTreeSet<String> = lan_servers.iter().map(|d| d.address.clone()).collect();
        let lan_peer_ids: BTreeSet<String> = lan_servers
            .iter()
            .filter_map(|d| d.peer_id.clone())
            .collect();

        // Collect API-discovered host: prefer a public address for fallback UI,
        // but suppress the API card entirely when the same peer is already on LAN.
        let api_host = self.api_discovery.host.lock().unwrap().clone();
        let api_best: Option<(String, Option<String>, Option<String>)> = api_host
            .as_ref()
            .filter(|h| !h.candidates.is_empty() && h.last_seen.elapsed() < Duration::from_secs(15))
            .and_then(|h| {
                if h.peer_id
                    .as_ref()
                    .map(|peer_id| lan_peer_ids.contains(peer_id))
                    .unwrap_or(false)
                {
                    None
                } else {
                    preferred_api_display_addr(h)
                        .map(|addr| (addr, h.hostname.clone(), h.peer_id.clone()))
                }
            });
        // For the empty-check below.
        let api_candidates: Vec<String> = api_best.iter().map(|(a, _, _)| a.clone()).collect();

        // Merged entry for rendering.
        struct MergedServer {
            address: String,
            display_name: String,
            subtitle: String,
            peer_id: Option<String>,
            is_lan: bool,
            is_dynamic: bool, // LAN or API discovered — don't allow delete
            saved_idx: Option<usize>,
            icon_active: bool,
        }

        let mut merged: Vec<MergedServer> = Vec::new();
        let mut seen_addrs: BTreeSet<String> = BTreeSet::new();
        let mut seen_peer_ids: BTreeSet<String> = BTreeSet::new();

        // 1) Saved servers (preserve order: most recently connected first).
        let mut indices: Vec<usize> = (0..self.server_list.len()).collect();
        indices.sort_by(|&a, &b| {
            self.server_list[b]
                .last_connected
                .cmp(&self.server_list[a].last_connected)
        });
        let api_addr = api_best.as_ref().map(|(a, _, _)| a.clone());
        for idx in &indices {
            let entry = &self.server_list[*idx];
            if let Some(peer_id) = entry.peer_id.as_ref() {
                if lan_peer_ids.contains(peer_id) || seen_peer_ids.contains(peer_id) {
                    continue;
                }
            }
            let is_lan = lan_addrs.contains(&entry.address);
            let is_api = api_addr.as_deref() == Some(entry.address.as_str());
            let display_name = if entry.nickname.is_empty() {
                entry.address.clone()
            } else {
                entry.nickname.clone()
            };
            let subtitle = if !entry.nickname.is_empty() {
                truncate_str(&entry.address, 24).to_string()
            } else {
                format_last_connected(entry.last_connected)
            };
            seen_addrs.insert(entry.address.clone());
            if let Some(peer_id) = entry.peer_id.as_ref() {
                seen_peer_ids.insert(peer_id.clone());
            }
            merged.push(MergedServer {
                address: entry.address.clone(),
                display_name,
                subtitle,
                peer_id: entry.peer_id.clone(),
                is_lan,
                is_dynamic: is_lan || is_api,
                saved_idx: Some(*idx),
                icon_active: entry.last_connected > 0 || is_lan || is_api,
            });
        }

        // 2) LAN-discovered servers not already in the saved list.
        for srv in &lan_servers {
            if srv
                .peer_id
                .as_ref()
                .map(|peer_id| seen_peer_ids.contains(peer_id))
                .unwrap_or(false)
            {
                continue;
            }
            if !seen_addrs.contains(&srv.address) {
                seen_addrs.insert(srv.address.clone());
                if let Some(peer_id) = srv.peer_id.as_ref() {
                    seen_peer_ids.insert(peer_id.clone());
                }
                merged.push(MergedServer {
                    address: srv.address.clone(),
                    display_name: srv.hostname.clone(),
                    subtitle: truncate_str(&srv.address, 24).to_string(),
                    peer_id: srv.peer_id.clone(),
                    is_lan: true,
                    is_dynamic: true,
                    saved_idx: None,
                    icon_active: true,
                });
            }
        }

        // 3) API-discovered best candidate (not already shown).
        if let Some((ref addr, ref hostname, ref peer_id)) = api_best {
            let peer_seen = peer_id
                .as_ref()
                .map(|peer_id| seen_peer_ids.contains(peer_id))
                .unwrap_or(false);
            if !peer_seen && !seen_addrs.contains(addr) {
                seen_addrs.insert(addr.clone());
                if let Some(peer_id) = peer_id.as_ref() {
                    seen_peer_ids.insert(peer_id.clone());
                }
                let name = hostname.as_deref().unwrap_or(addr.as_str());
                merged.push(MergedServer {
                    address: addr.clone(),
                    display_name: name.to_string(),
                    subtitle: if hostname.is_some() {
                        addr.clone()
                    } else {
                        String::new()
                    },
                    peer_id: peer_id.clone(),
                    is_lan: false,
                    is_dynamic: true,
                    saved_idx: None,
                    icon_active: true,
                });
            }
        }

        // Filter by search query.
        let query = self.search_query.trim().to_lowercase();
        if !query.is_empty() {
            merged.retain(|m| {
                m.address.to_lowercase().contains(&query)
                    || m.display_name.to_lowercase().contains(&query)
            });
        }

        let mut connect_addr: Option<String> = None;
        let mut delete_idx: Option<usize> = None;

        if merged.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                let msg = if self.server_list.is_empty()
                    && lan_servers.is_empty()
                    && api_candidates.is_empty()
                {
                    "No computers\nAdd a server address using the bar below."
                } else {
                    "No matches"
                };
                ui.label(egui::RichText::new(msg).size(14.0).color(TEXT_DIM));
            });
        } else {
            let avail_w = ui.available_width();
            let cols = ((avail_w + 12.0) / (CARD_W + 12.0)).floor().max(1.0) as usize;
            let total_rows = (merged.len() + cols - 1) / cols;
            let row_height = CARD_H + CARD_ROW_SPACING;
            let content_top = ui.max_rect().top();
            let viewport_min_y = (ui.clip_rect().top() - content_top).max(0.0);
            let viewport_max_y = (ui.clip_rect().bottom() - content_top).max(viewport_min_y);
            let list_start_y = ui.next_widget_position().y - content_top;
            let start_row = (((viewport_min_y - list_start_y) / row_height)
                .floor()
                .max(0.0) as usize)
                .min(total_rows);
            let end_row = (((viewport_max_y - list_start_y) / row_height)
                .ceil()
                .max(0.0) as usize
                + 1)
            .min(total_rows);

            if start_row > 0 {
                ui.add_space(start_row as f32 * row_height);
            }

            for row in start_row..end_row {
                ui.horizontal(|ui| {
                    for col in 0..cols {
                        let card_i = row * cols + col;
                        if card_i >= merged.len() {
                            break;
                        }
                        let srv = &merged[card_i];
                        let (_, card_rect) = ui.allocate_space(egui::vec2(CARD_W, CARD_H));
                        let card_id = ui.make_persistent_id(("server-card", srv.address.as_str()));
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
                            &painter,
                            egui::pos2(icon_cx, icon_top),
                            srv.icon_active,
                        );

                        // Server name
                        let name_y = icon_top + 76.0;
                        painter.text(
                            egui::pos2(icon_cx, name_y),
                            egui::Align2::CENTER_CENTER,
                            truncate_str(&srv.display_name, 20),
                            egui::FontId::proportional(12.0),
                            TEXT_WHITE,
                        );

                        // Subtitle
                        let sub_y = name_y + 16.0;
                        if !srv.subtitle.is_empty() {
                            painter.text(
                                egui::pos2(icon_cx, sub_y),
                                egui::Align2::CENTER_CENTER,
                                &srv.subtitle,
                                egui::FontId::proportional(10.0),
                                TEXT_DIM,
                            );
                        }

                        // LAN badge
                        if srv.is_lan {
                            let badge_y = if srv.subtitle.is_empty() {
                                sub_y
                            } else {
                                sub_y + 14.0
                            };
                            painter.text(
                                egui::pos2(icon_cx, badge_y),
                                egui::Align2::CENTER_CENTER,
                                "LAN",
                                egui::FontId::proportional(9.0),
                                ACCENT_GREEN,
                            );
                        }

                        // Connect button
                        let btn_w = CARD_W - 24.0;
                        let btn_h = 28.0;
                        let btn_rect = egui::Rect::from_min_size(
                            egui::pos2(card_rect.left() + 12.0, card_rect.bottom() - 12.0 - btn_h),
                            egui::vec2(btn_w, btn_h),
                        );
                        let btn_resp = ui.interact(
                            btn_rect,
                            ui.make_persistent_id(("server-connect", srv.address.as_str())),
                            egui::Sense::click(),
                        );
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
                            connect_addr = Some(srv.address.clone());
                            let mut changed = false;
                            if let Some(entry) = self
                                .server_list
                                .iter_mut()
                                .find(|e| e.address == srv.address)
                            {
                                // Copy hostname as nickname for LAN-discovered entries.
                                if srv.saved_idx.is_none() {
                                    if entry.nickname.is_empty() && srv.display_name != srv.address
                                    {
                                        entry.nickname = srv.display_name.clone();
                                        changed = true;
                                    }
                                }
                                if entry.peer_id.is_none() {
                                    entry.peer_id = srv.peer_id.clone();
                                    changed = true;
                                }
                            }
                            if changed {
                                save_server_list(&self.server_list);
                            }
                        }

                        // Delete X in top-right corner (only for saved, non-dynamic servers)
                        if let Some(saved) = srv.saved_idx.filter(|_| !srv.is_dynamic) {
                            let x_rect = egui::Rect::from_min_size(
                                egui::pos2(card_rect.right() - 22.0, card_rect.top() + 4.0),
                                egui::vec2(18.0, 18.0),
                            );
                            let x_resp = ui.interact(
                                x_rect,
                                ui.make_persistent_id(("server-delete", srv.address.as_str())),
                                egui::Sense::click(),
                            );
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
                                delete_idx = Some(saved);
                            }
                        }

                        ui.add_space(CARD_ROW_SPACING);
                    }
                });
                ui.add_space(CARD_ROW_SPACING);
            }

            if end_row < total_rows {
                ui.add_space((total_rows - end_row) as f32 * row_height);
            }
        }

        if let Some(addr) = connect_addr {
            self.server_addr = addr;
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

        ui.label(egui::RichText::new("Settings").size(32.0).color(TEXT_WHITE));
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Session defaults for the next connection.")
                .size(13.0)
                .color(TEXT_GRAY),
        );
        ui.add_space(24.0);

        // Audio toggle
        let audio_clicked = render_parsec_toggle(
            ui,
            "Audio",
            "Start stereo playback on connect.",
            self.audio_enabled,
            BG_ROW,
        );
        if audio_clicked {
            self.audio_enabled = !self.audio_enabled;
            save_audio_enabled(self.audio_enabled);
            self.audio_enabled_flag
                .store(self.audio_enabled, Ordering::SeqCst);
        }
        ui.add_space(8.0);

        // Debug overlay toggle
        let debug_clicked = render_parsec_toggle(
            ui,
            "Debug Overlay",
            "Show transport, decoder, and latency telemetry.",
            self.debug_enabled,
            BG_ROW,
        );
        if debug_clicked {
            self.debug_enabled = !self.debug_enabled;
            save_debug_enabled(self.debug_enabled);
            self.debug_enabled_flag
                .store(self.debug_enabled, Ordering::SeqCst);
        }
        ui.add_space(8.0);

        let yuv444_description = if cfg!(target_os = "macos") {
            "Advertise 4:4:4 decode support on the next connection, but keep macOS out of the hardware/low-latency 4:4:4 preference path until a fast present path exists."
        } else {
            "Advertise 4:4:4 decode support and prefer 4:4:4 streams when both sides support it. Applies on the next connection."
        };
        let yuv444_clicked = render_parsec_toggle(
            ui,
            "YUV 4:4:4",
            yuv444_description,
            self.yuv444_enabled,
            BG_ROW,
        );
        if yuv444_clicked {
            self.yuv444_enabled = !self.yuv444_enabled;
            save_yuv444_enabled(self.yuv444_enabled);
        }

        ui.add_space(16.0);
        ui.label(
            egui::RichText::new("Authentication")
                .size(16.0)
                .color(TEXT_WHITE),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "Token to authenticate with servers. Must match the server's token.",
            )
            .size(12.0)
            .color(TEXT_GRAY),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Token").size(13.0).color(TEXT_WHITE));
            ui.add_space(8.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.token)
                    .desired_width(300.0)
                    .hint_text("Paste server token here"),
            );
            if resp.changed() {
                save_token(&self.token);
                *self.api_discovery.token.lock().unwrap() = self.token.clone();
            }
        });
    }

    fn render_update_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        const TEXT_WHITE: egui::Color32 = egui::Color32::from_rgb(230, 233, 240);
        const TEXT_GRAY: egui::Color32 = egui::Color32::from_rgb(138, 142, 150);

        ui.label(egui::RichText::new("Update").size(32.0).color(TEXT_WHITE));
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

        ui.label(egui::RichText::new("About").size(32.0).color(TEXT_WHITE));
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Client capabilities and platform information.")
                .size(13.0)
                .color(TEXT_GRAY),
        );
        ui.add_space(24.0);

        let caps = self.native_surfaces.snapshot();
        let codec_support = self.video_codec_support;
        let yuv444_pref = if self.yuv444_enabled {
            "enabled"
        } else {
            "disabled"
        };

        let rows: &[(&str, String)] = &[
            ("Version", format!("v{}", updater::current_version())),
            ("Platform", about_platform_label().to_string()),
            (
                "Display",
                about_format_refresh(self.display_refresh_millihz),
            ),
            ("Present", about_native_surface(caps).to_string()),
            ("Codecs", about_codec_summary(codec_support)),
            ("YUV 4:4:4", yuv444_pref.to_string()),
            ("Audio", "opus stereo / 48 kHz".to_string()),
        ];

        for (label, value) in rows {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(*label).size(13.0).color(TEXT_DIM));
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
                        egui::RichText::new(format!("Downloading and installing v{version}..."))
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
            if let (Some(origin), Some(delta)) = (
                self.menu_button_drag_origin,
                button_response.total_drag_delta(),
            ) {
                self.menu_button_pos = clamp_menu_button_pos(origin + delta, content_rect);
                self.last_pointer_move = Some(Instant::now());
            }
        }
        if button_response.drag_stopped() {
            self.menu_button_drag_origin = None;
            save_menu_button_pos(self.menu_button_pos);
        }
        let button_rect = egui::Rect::from_min_size(
            self.menu_button_pos,
            egui::vec2(FLOATING_MENU_BUTTON_SIZE, FLOATING_MENU_BUTTON_SIZE),
        );
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
            let menu_left = button_rect.left().clamp(
                content_rect.left(),
                (content_rect.right() - 190.0).max(content_rect.left()),
            );
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

    fn render_file_transfer_overlay(&mut self, ctx: &egui::Context, menu_bottom: f32) {
        use st_protocol::file_transfer::{format_bytes, TransferDirection, TransferStatus};

        let (pending, entries) = {
            let guard = self.file_transfer_state.lock().unwrap();
            if guard.pending_offers.is_empty() && guard.entries.is_empty() {
                return;
            }
            (guard.pending_offers.clone(), guard.entries.clone())
        };

        let panel_width = 280.0;
        let x = self.menu_button_pos.x;
        let y = menu_bottom + 4.0;
        let text_color = egui::Color32::from_rgb(220, 220, 220);
        let dim_color = egui::Color32::from_rgb(140, 140, 140);
        let accent_color = egui::Color32::from_rgb(80, 160, 255);

        let area = egui::Area::new(egui::Id::new("file_transfer_overlay"))
            .fixed_pos(egui::pos2(x, y))
            .order(egui::Order::Foreground);

        let resp = area.show(ctx, |ui| {
            let frame = egui::Frame::NONE
                .fill(egui::Color32::from_rgba_unmultiplied(12, 12, 12, 220))
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(egui::Margin::same(8));

            frame.show(ui, |ui| {
                ui.set_width(panel_width);

                // --- Pending offers: "N files ready to paste" ---
                if !pending.is_empty() {
                    let total_size: u64 = pending.iter().map(|o| o.file_size).sum();
                    let label = if pending.len() == 1 {
                        format!("{} ({})", pending[0].file_name, format_bytes(total_size))
                    } else {
                        format!("{} files ({})", pending.len(), format_bytes(total_size))
                    };

                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(label).color(text_color).size(12.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button(
                                    egui::RichText::new("Transfer")
                                        .color(accent_color)
                                        .size(12.0),
                                )
                                .clicked()
                            {
                                let mut guard = self.file_transfer_state.lock().unwrap();
                                for offer in &pending {
                                    guard.accept_queue.push(offer.transfer_id);
                                }
                            }
                        });
                    });
                    if pending.len() > 1 {
                        for offer in &pending {
                            let name = if offer.file_name.len() > 30 {
                                format!("  {}...", &offer.file_name[..27])
                            } else {
                                format!("  {}", offer.file_name)
                            };
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} ({})",
                                    name,
                                    format_bytes(offer.file_size)
                                ))
                                .color(dim_color)
                                .size(10.0),
                            );
                        }
                    }
                    if !entries.is_empty() {
                        ui.add_space(4.0);
                        ui.separator();
                    }
                }

                // --- Active / completed transfers ---
                for entry in &entries {
                    let dir_icon = match entry.direction {
                        TransferDirection::Sending => "^ ",
                        TransferDirection::Receiving => "v ",
                    };
                    let display_name = if entry.file_name.len() > 24 {
                        format!("{}...", &entry.file_name[..21])
                    } else {
                        entry.file_name.clone()
                    };

                    ui.label(
                        egui::RichText::new(format!("{dir_icon}{display_name}"))
                            .color(text_color)
                            .size(12.0),
                    );

                    if matches!(
                        entry.status,
                        TransferStatus::Active | TransferStatus::Verifying
                    ) {
                        let progress = if entry.total_bytes > 0 {
                            entry.transferred_bytes as f32 / entry.total_bytes as f32
                        } else {
                            1.0
                        };
                        ui.add(egui::ProgressBar::new(progress).desired_height(10.0));

                        let elapsed = entry.started_at.elapsed().as_secs_f64();
                        let speed = if elapsed > 0.1 {
                            entry.transferred_bytes as f64 / elapsed
                        } else {
                            0.0
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "{} / {}  {}/s",
                                format_bytes(entry.transferred_bytes),
                                format_bytes(entry.total_bytes),
                                format_bytes(speed as u64),
                            ))
                            .color(dim_color)
                            .size(10.0),
                        );
                    } else {
                        let status = match entry.status {
                            TransferStatus::AwaitingAccept => "waiting...",
                            TransferStatus::Completed => "completed",
                            TransferStatus::Cancelled => "cancelled",
                            TransferStatus::Failed => "failed",
                            _ => "",
                        };
                        ui.label(egui::RichText::new(status).color(dim_color).size(10.0));
                    }

                    ui.add_space(4.0);
                }
            });
        });

        self.local_overlay_hit_rects.push(resp.response.rect);
        ctx.request_repaint();
    }
}

// ---------------------------------------------------------------------------
// Connection thread
// ---------------------------------------------------------------------------

struct MediaThreads {
    input_stop_tx: Option<crossbeam_channel::Sender<()>>,
    input_thread: Option<std::thread::JoinHandle<()>>,
    video_stop_tx: crossbeam_channel::Sender<()>,
    video_thread: std::thread::JoinHandle<()>,
    audio_stop_tx: crossbeam_channel::Sender<()>,
    audio_thread: std::thread::JoinHandle<()>,
    feedback_rx: crossbeam_channel::Receiver<TransportFeedback>,
    decode_started_rx: crossbeam_channel::Receiver<()>,
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
    crypto: Option<Arc<st_protocol::tunnel::CryptoContext>>,
) -> Result<MediaThreads, String> {
    let input_socket = udp_socket
        .try_clone()
        .map_err(|err| format!("Failed to clone UDP socket: {err}"))?;
    let input_target = std::net::SocketAddr::new(socket_addr.ip(), socket_addr.port());
    let input_crypto = crypto.clone();
    let receiver = MediaReceiver::from_udp_socket(udp_socket, crypto)?;
    let (input_stop_tx, input_stop_rx) = crossbeam_channel::bounded::<()>(1);
    let input_thread = std::thread::spawn(move || {
        run_input_sender(
            input_socket,
            input_target,
            input_rx,
            input_stop_rx,
            input_crypto,
        );
    });

    let (audio_data_tx, audio_data_rx) = crossbeam_channel::unbounded::<AudioPacket>();
    let (feedback_tx, feedback_rx) = crossbeam_channel::bounded::<TransportFeedback>(8);
    let (decode_started_tx, decode_started_rx) = crossbeam_channel::bounded::<()>(1);
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
            decode_started_tx,
            pipeline_audio_flag,
            native_surfaces,
            control_tx,
            present_refresh_millihz,
            stream_config,
            receiver,
        );
    });

    let (audio_stop_tx, audio_stop_rx) = crossbeam_channel::bounded(1);
    let audio_thread = std::thread::spawn(move || {
        if let Err(e) = audio::run_audio_pipeline(audio_data_rx, audio_stop_rx) {
            eprintln!("[audio] {e}");
        }
    });

    Ok(MediaThreads {
        input_stop_tx: Some(input_stop_tx),
        input_thread: Some(input_thread),
        video_stop_tx,
        video_thread,
        audio_stop_tx,
        audio_thread,
        feedback_rx,
        decode_started_rx,
    })
}

fn start_punched_media_threads(
    frame_buf: Arc<Mutex<VideoFrameBuffer>>,
    debug_state: Arc<ConnectionDebugState>,
    debug_enabled: Arc<AtomicBool>,
    ctx: egui::Context,
    display_refresh_millihz: Option<u32>,
    audio_enabled: Arc<AtomicBool>,
    native_surfaces: Arc<NativeSurfaceControl>,
    control_tx: crossbeam_channel::Sender<ControlMessage>,
    stream_config: StreamConfig,
    media_packet_rx: crossbeam_channel::Receiver<Vec<u8>>,
) -> MediaThreads {
    let receiver = MediaReceiver::from_packet_channel(media_packet_rx);
    let (audio_data_tx, audio_data_rx) = crossbeam_channel::unbounded::<AudioPacket>();
    let (feedback_tx, feedback_rx) = crossbeam_channel::bounded::<TransportFeedback>(8);
    let (decode_started_tx, decode_started_rx) = crossbeam_channel::bounded::<()>(1);
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
            decode_started_tx,
            pipeline_audio_flag,
            native_surfaces,
            control_tx,
            present_refresh_millihz,
            stream_config,
            receiver,
        );
    });

    let (audio_stop_tx, audio_stop_rx) = crossbeam_channel::bounded(1);
    let audio_thread = std::thread::spawn(move || {
        if let Err(e) = audio::run_audio_pipeline(audio_data_rx, audio_stop_rx) {
            eprintln!("[audio] {e}");
        }
    });

    MediaThreads {
        input_stop_tx: None,
        input_thread: None,
        video_stop_tx,
        video_thread,
        audio_stop_tx,
        audio_thread,
        feedback_rx,
        decode_started_rx,
    }
}

fn stop_media_threads(media_threads: MediaThreads) {
    let _ = media_threads.video_stop_tx.send(());
    let _ = media_threads.audio_stop_tx.send(());
    if let Some(input_stop_tx) = media_threads.input_stop_tx {
        let _ = input_stop_tx.send(());
    }
    let _ = media_threads.video_thread.join();
    let _ = media_threads.audio_thread.join();
    if let Some(input_thread) = media_threads.input_thread {
        let _ = input_thread.join();
    }
}

const STARTUP_DECODE_TIMEOUT: Duration = Duration::from_secs(5);

fn filter_advertised_video_codec_support(
    report: decode::VideoCodecSupportReport,
    yuv444_enabled: bool,
    allow_yuv444_advertising: bool,
    allow_hardware_yuv444_advertising: bool,
) -> decode::VideoCodecSupportReport {
    let mut filtered = report;
    if !(yuv444_enabled && allow_yuv444_advertising) {
        filtered.yuv444 = VideoCodecSupport::empty();
    }
    if !(yuv444_enabled && allow_hardware_yuv444_advertising) {
        filtered.hardware_yuv444 = VideoCodecSupport::empty();
    }
    filtered
}

fn advertised_video_codec_support(
    report: decode::VideoCodecSupportReport,
    yuv444_enabled: bool,
) -> decode::VideoCodecSupportReport {
    // macOS can probe some YUV444 decode paths, but it does not yet have a
    // low-latency native presentation path for them. Keep advertising decode
    // support when the user enables it, but suppress the hardware YUV444 flag
    // so Linux servers do not auto-prefer 4:4:4 for low-latency sessions.
    filter_advertised_video_codec_support(report, yuv444_enabled, true, !cfg!(target_os = "macos"))
}

fn run_connection(
    addr: String,
    token: String,
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
    api_discovery: Arc<api_client::ApiDiscoveryShared>,
    ft_shared_state: file_transfer::SharedTransferState,
) {
    let punch_fallback_available = api_discovery.is_connected();

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

    // Try direct TCP first. If it fails and we have tunnel state, fall back to hole punch.
    let tcp_timeout = if punch_fallback_available { 3 } else { 5 };
    let tcp_result = TcpStream::connect_timeout(&socket_addr, Duration::from_secs(tcp_timeout));

    let mut tcp = match tcp_result {
        Ok(s) => s,
        Err(tcp_err) => {
            if allow_hole_punch_fallback(socket_addr) && punch_fallback_available {
                match api_client::prepare_punch_attempt(api_discovery.as_ref()) {
                    Ok((partner_cands, punched_crypto)) => {
                        eprintln!(
                            "[connect] Direct TCP to {socket_addr} failed ({tcp_err}), attempting hole punch..."
                        );
                        run_punched_session(
                            partner_cands,
                            punched_crypto,
                            token,
                            display_refresh_millihz,
                            video_codec_support,
                            excluded_video_codecs,
                            state,
                            frame_buf,
                            debug_state,
                            disconnect,
                            connection_epoch,
                            session_epoch,
                            audio_enabled,
                            debug_enabled,
                            native_surfaces,
                            shared_input,
                            control_rx,
                            input_rx,
                            ctx,
                            api_discovery,
                            ft_shared_state,
                        );
                        return;
                    }
                    Err(punch_err) => {
                        set_error(
                            &state,
                            &ctx,
                            &disconnect,
                            &connection_epoch,
                            session_epoch,
                            format!(
                                "Connection failed: {tcp_err}. Hole punch setup failed: {punch_err}"
                            ),
                        );
                        return;
                    }
                }
            }
            set_error(
                &state,
                &ctx,
                &disconnect,
                &connection_epoch,
                session_epoch,
                format!("Connection failed: {tcp_err}"),
            );
            return;
        }
    };
    let _ = tcp.set_nodelay(true);

    // --- Authentication handshake ---
    let _ = tcp.write_all(&ControlMessage::Authenticate(token).serialize());
    tcp.set_read_timeout(Some(Duration::from_secs(5))).ok();
    {
        let mut auth_buf = vec![0u8; 64];
        let mut pending = Vec::new();
        let auth_deadline = Instant::now() + Duration::from_secs(5);
        let mut authenticated = false;
        'auth: while Instant::now() < auth_deadline {
            match tcp.read(&mut auth_buf) {
                Ok(0) => break,
                Ok(n) => {
                    pending.extend_from_slice(&auth_buf[..n]);
                    let mut consumed = 0;
                    while let Some((msg, used)) = ControlMessage::deserialize(&pending[consumed..])
                    {
                        consumed += used;
                        match msg {
                            ControlMessage::AuthResult(ok) => {
                                if !ok {
                                    set_error(
                                        &state,
                                        &ctx,
                                        &disconnect,
                                        &connection_epoch,
                                        session_epoch,
                                        "Authentication failed. Check your token.".into(),
                                    );
                                    return;
                                }
                                authenticated = true;
                                break 'auth;
                            }
                            ControlMessage::Error(msg) => {
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
                            _ => {}
                        }
                    }
                    if consumed > 0 {
                        pending.drain(..consumed);
                    }
                }
                Err(ref e) if is_timeout(e) => continue,
                Err(e) => {
                    set_error(
                        &state,
                        &ctx,
                        &disconnect,
                        &connection_epoch,
                        session_epoch,
                        format!("Auth read error: {e}"),
                    );
                    return;
                }
            }
        }
        if !authenticated {
            set_error(
                &state,
                &ctx,
                &disconnect,
                &connection_epoch,
                session_epoch,
                "Authentication timed out.".into(),
            );
            return;
        }
    }

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
            // Give the unified media socket more headroom for bursty video and
            // low-latency input packets on the same fd.
            configure_media_udp_socket(&socket, socket_addr);
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
            "[trace][client] sending ClientDisplayInfo: refresh_millihz={} udp_port={local_udp_port} codecs={} hw_codecs={} yuv444={} yuv444_hw={}",
            display_refresh_millihz.unwrap_or(0),
            codec_support_summary(video_codec_support.supported),
            codec_support_summary(video_codec_support.hardware),
            codec_support_summary(video_codec_support.yuv444),
            codec_support_summary(video_codec_support.hardware_yuv444),
        );
    }
    let mut excluded = *excluded_video_codecs.lock().unwrap();
    let mut effective_supported = video_codec_support.supported.subtract(excluded);
    // If all supported codecs are excluded, reset the blacklist so we can retry.
    if effective_supported.is_empty() && !excluded.is_empty() {
        eprintln!("[codec] all codecs excluded — resetting blacklist to retry");
        *excluded_video_codecs.lock().unwrap() = st_protocol::VideoCodecSupport::empty();
        excluded = st_protocol::VideoCodecSupport::empty();
        effective_supported = video_codec_support.supported;
    }
    let effective_hardware = video_codec_support.hardware.subtract(excluded);
    let effective_yuv444 = video_codec_support.yuv444.subtract(excluded);
    let effective_hardware_yuv444 = video_codec_support.hardware_yuv444.subtract(excluded);
    if !excluded.is_empty() {
        eprintln!(
            "[codec] excluding previously failed codecs: {} (effective: supported={} hw={} yuv444={} yuv444_hw={})",
            codec_support_summary(excluded),
            codec_support_summary(effective_supported),
            codec_support_summary(effective_hardware),
            codec_support_summary(effective_yuv444),
            codec_support_summary(effective_hardware_yuv444),
        );
    }
    let _ = tcp.write_all(
        &ControlMessage::ClientDisplayInfo(ClientDisplayInfo {
            max_refresh_millihz: display_refresh_millihz.unwrap_or(0),
            udp_port: local_udp_port,
            supported_video_codecs: effective_supported,
            hardware_video_codecs: effective_hardware,
            supported_yuv444_video_codecs: effective_yuv444,
            hardware_yuv444_video_codecs: effective_hardware_yuv444,
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
                                    "[trace][client] received StreamConfig: {:?} {} {}x{} {}fps audio={}ch/{}Hz hdr={}",
                                    cfg.codec,
                                    stream_chroma_label(cfg.chroma),
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
                                    None,
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
                            if trace_enabled() {
                                eprintln!(
                                    "[cursor] shape: serial={} {}x{} hotspot=({},{}) rgba_len={}",
                                    shape.serial,
                                    shape.width,
                                    shape.height,
                                    shape.hotspot_x,
                                    shape.hotspot_y,
                                    shape.rgba.len()
                                );
                            }
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
                        | ControlMessage::RequestKeyframe
                        | ControlMessage::ClipboardText(_)
                        | ControlMessage::Authenticate(_)
                        | ControlMessage::AuthResult(_) => {}
                        _ => {}
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
    let decode_started_rx = media_threads.decode_started_rx.clone();
    let (clipboard_control_tx, clipboard_control_rx) =
        crossbeam_channel::bounded::<ControlMessage>(8);
    let (file_detect_tx, file_detect_rx) = crossbeam_channel::bounded::<std::path::PathBuf>(8);
    let suppressed_paths = clipboard::new_suppressed_paths();
    let mut clipboard_sync = clipboard::ClipboardSync::start_with_file_detection(
        "client",
        true,
        {
            let shared_input = Arc::clone(&shared_input);
            move || shared_input.snapshot().controller_state == ControllerState::OwnedByYou
        },
        clipboard_control_tx,
        file_detect_tx,
        Arc::clone(&suppressed_paths),
    );
    let mut ft_manager = file_transfer::FileTransferManager::start_full(
        st_protocol::file_transfer::TransportMode::Direct,
        Arc::clone(&ft_shared_state),
        suppressed_paths,
    );

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
        while let Ok(msg) = clipboard_control_rx.try_recv() {
            let _ = tcp.write_all(&msg.serialize());
        }
        while let Ok(path) = file_detect_rx.try_recv() {
            let _ = ft_manager
                .inbound_tx
                .try_send(file_transfer::FtInbound::SendFile { path });
        }
        while let Ok(msg) = ft_manager.outbound_rx.try_recv() {
            let _ = tcp.write_all(&msg.serialize());
        }
        while let Ok(msg) = pipeline_control_rx.try_recv() {
            let _ = tcp.write_all(&msg.serialize());
        }
        while decode_started_rx.try_recv().is_ok() {
            startup_decode_ok = true;
        }
        while let Ok(feedback) = feedback_rx.try_recv() {
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
            clipboard_sync.stop();
            ft_manager.stop();
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
                            if trace_enabled() {
                                eprintln!(
                                    "[cursor] shape: serial={} {}x{} hotspot=({},{}) rgba_len={}",
                                    shape.serial,
                                    shape.width,
                                    shape.height,
                                    shape.hotspot_x,
                                    shape.hotspot_y,
                                    shape.rgba.len()
                                );
                            }
                            shared_input.set_cursor_shape(shape);
                            ctx.request_repaint();
                        }
                        ControlMessage::CursorState(cursor_state) => {
                            shared_input.set_cursor_state(cursor_state);
                            ctx.request_repaint();
                        }
                        ControlMessage::ClipboardText(text) => {
                            clipboard_sync.set_remote_text(text);
                        }
                        ControlMessage::FileOffer {
                            transfer_id,
                            file_size,
                            file_name,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::OfferReceived {
                                    transfer_id,
                                    file_size,
                                    file_name,
                                },
                            );
                        }
                        ControlMessage::FileAccept {
                            transfer_id,
                            accepted,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::AcceptReceived {
                                    transfer_id,
                                    accepted,
                                },
                            );
                        }
                        ControlMessage::FileChunk {
                            transfer_id,
                            chunk_index,
                            data,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::ChunkReceived {
                                    transfer_id,
                                    chunk_index,
                                    data,
                                },
                            );
                        }
                        ControlMessage::FileComplete {
                            transfer_id,
                            total_chunks,
                            sha256,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::CompleteReceived {
                                    transfer_id,
                                    total_chunks,
                                    sha256,
                                },
                            );
                        }
                        ControlMessage::FileCancel { transfer_id } => {
                            let _ = ft_manager
                                .inbound_tx
                                .try_send(file_transfer::FtInbound::CancelReceived { transfer_id });
                        }
                        ControlMessage::FileProgress {
                            transfer_id,
                            chunks_received,
                        } => {
                            let _ = ft_manager.inbound_tx.try_send(
                                file_transfer::FtInbound::ProgressReceived {
                                    transfer_id,
                                    chunks_received,
                                },
                            );
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
                        _ => {}
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
    clipboard_sync.stop();
    ft_manager.stop();
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

/// Run a full streaming session over a hole-punched UDP socket.
/// Called when direct TCP connection fails but tunnel crypto + partner candidates are available.
///
/// Uses a packet channel bridge: a background thread reads from the PunchedSocket and
/// forwards decrypted media packets directly into the existing receive pipeline.
#[allow(clippy::too_many_arguments)]
fn run_punched_session(
    partner_candidates: Vec<std::net::SocketAddr>,
    crypto: Arc<st_protocol::tunnel::CryptoContext>,
    token: String,
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
    api_discovery: Arc<api_client::ApiDiscoveryShared>,
    ft_shared_state: file_transfer::SharedTransferState,
) {
    use st_protocol::reliable_udp::{PunchedMessage, PunchedSocket};

    // Clone the process-lifetime punch socket from API discovery.
    let socket = match api_discovery.clone_punch_socket() {
        Ok(socket) => socket,
        Err(e) => {
            set_error(
                &state,
                &ctx,
                &disconnect,
                &connection_epoch,
                session_epoch,
                format!("Failed to prepare punch socket: {e}"),
            );
            return;
        }
    };

    {
        let mut s = state.lock().unwrap();
        *s = ConnectionState::Connecting;
    }
    ctx.request_repaint();

    eprintln!(
        "[punch] Attempting hole punch to {} candidates...",
        partner_candidates.len()
    );
    // Mark the punch socket as in-use for the duration of this session so
    // the background API thread doesn't try to re-STUN on it (its recv would
    // steal session packets).
    struct PunchSessionGuard(Arc<api_client::ApiDiscoveryShared>);
    impl Drop for PunchSessionGuard {
        fn drop(&mut self) {
            self.0.set_punch_session_active(false);
        }
    }
    api_discovery.set_punch_session_active(true);
    let _session_guard = PunchSessionGuard(Arc::clone(&api_discovery));

    let peer = match st_protocol::tunnel::hole_punch(
        &socket,
        &partner_candidates,
        &crypto,
        Duration::from_secs(10),
    ) {
        Ok(p) => p,
        Err(e) => {
            set_error(
                &state,
                &ctx,
                &disconnect,
                &connection_epoch,
                session_epoch,
                format!("Hole punch failed: {e}"),
            );
            return;
        }
    };
    eprintln!("[punch] Success! Server confirmed at {peer}");

    let punched = Arc::new(PunchedSocket::new(socket, peer, crypto));
    let _ = punched.set_read_timeout(Some(Duration::from_millis(100)));

    // --- Authentication ---
    let auth_data = ControlMessage::Authenticate(token).serialize();
    if let Err(e) = punched.send_control(&auth_data) {
        set_error(
            &state,
            &ctx,
            &disconnect,
            &connection_epoch,
            session_epoch,
            format!("Failed to send auth: {e}"),
        );
        return;
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut authenticated = false;
    while Instant::now() < deadline {
        punched.tick();
        if let Some(PunchedMessage::Control(data)) = punched.try_recv() {
            if let Some((ControlMessage::AuthResult(ok), _)) = ControlMessage::deserialize(&data) {
                if ok {
                    authenticated = true;
                } else {
                    set_error(
                        &state,
                        &ctx,
                        &disconnect,
                        &connection_epoch,
                        session_epoch,
                        "Authentication failed. Check your token.".into(),
                    );
                    return;
                }
                break;
            }
        }
    }
    if !authenticated {
        set_error(
            &state,
            &ctx,
            &disconnect,
            &connection_epoch,
            session_epoch,
            "Authentication timeout over punched connection".into(),
        );
        return;
    }
    eprintln!("[punch] Authenticated");

    // --- Send ClientDisplayInfo ---
    let mut excluded = *excluded_video_codecs.lock().unwrap();
    let mut effective_supported = video_codec_support.supported.subtract(excluded);
    if effective_supported.is_empty() && !excluded.is_empty() {
        eprintln!("[codec] all codecs excluded — resetting blacklist to retry");
        *excluded_video_codecs.lock().unwrap() = st_protocol::VideoCodecSupport::empty();
        excluded = st_protocol::VideoCodecSupport::empty();
        effective_supported = video_codec_support.supported;
    }
    let effective_hardware = video_codec_support.hardware.subtract(excluded);
    let effective_yuv444 = video_codec_support.yuv444.subtract(excluded);
    let effective_hardware_yuv444 = video_codec_support.hardware_yuv444.subtract(excluded);
    let display_info = st_protocol::ClientDisplayInfo {
        max_refresh_millihz: display_refresh_millihz.unwrap_or(0),
        udp_port: 0, // Not used for punched connections.
        supported_video_codecs: effective_supported,
        hardware_video_codecs: effective_hardware,
        supported_yuv444_video_codecs: effective_yuv444,
        hardware_yuv444_video_codecs: effective_hardware_yuv444,
    };
    let _ = punched.send_control(&ControlMessage::ClientDisplayInfo(display_info).serialize());

    // --- Wait for StreamConfig and startup bundle ---
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream_config: Option<st_protocol::StreamConfig> = None;
    while Instant::now() < deadline {
        punched.tick();
        if let Some(PunchedMessage::Control(data)) = punched.try_recv() {
            let mut offset = 0;
            while let Some((msg, used)) = ControlMessage::deserialize(&data[offset..]) {
                offset += used;
                match msg {
                    ControlMessage::StreamConfig(cfg) => {
                        shared_input.set_stream_config(cfg);
                        stream_config = Some(cfg);
                    }
                    ControlMessage::SessionDebugInfo(info) => {
                        debug_state.set_session_info(info);
                    }
                    ControlMessage::InputSession(session) => {
                        shared_input.set_client_id(session.client_id);
                    }
                    ControlMessage::InputCapabilities(caps) => {
                        shared_input.set_capabilities(caps);
                    }
                    ControlMessage::ControllerState(cs) => {
                        shared_input.set_controller_state(cs);
                    }
                    _ => {}
                }
            }
        }
        if stream_config.is_some() {
            break;
        }
    }
    let stream_config = match stream_config {
        Some(cfg) => cfg,
        None => {
            set_error(
                &state,
                &ctx,
                &disconnect,
                &connection_epoch,
                session_epoch,
                "Timeout waiting for StreamConfig from server".into(),
            );
            return;
        }
    };

    // --- Send ClientReadyForMedia ---
    let _ = punched.send_control(&ControlMessage::ClientReadyForMedia.serialize());

    // Wait for StreamStarted.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        punched.tick();
        if let Some(PunchedMessage::Control(data)) = punched.try_recv() {
            if let Some((ControlMessage::StreamStarted, _)) = ControlMessage::deserialize(&data) {
                break;
            }
        }
    }

    eprintln!("[punch] Stream started, entering unified punched loop");
    let (media_packet_tx, media_packet_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let _ = punched.set_nonblocking(true);
    let _ = punched.set_read_timeout(None);

    // --- Set Connected state ---
    {
        let mut s = state.lock().unwrap();
        *s = ConnectionState::Connected;
    }
    ctx.request_repaint();

    // --- Start media threads using direct bridged packets ---
    let (pipeline_control_tx, pipeline_control_rx) =
        crossbeam_channel::bounded::<ControlMessage>(8);
    let media = start_punched_media_threads(
        Arc::clone(&frame_buf),
        Arc::clone(&debug_state),
        Arc::clone(&debug_enabled),
        ctx.clone(),
        display_refresh_millihz,
        Arc::clone(&audio_enabled),
        Arc::clone(&native_surfaces),
        pipeline_control_tx,
        stream_config,
        media_packet_rx,
    );

    // --- Control + input forwarding loop ---
    let decode_started_rx = media.decode_started_rx.clone();
    let mut startup_decode_ok = false;
    let startup_deadline = Instant::now() + STARTUP_DECODE_TIMEOUT;
    let initial_audio =
        audio_enabled.load(Ordering::SeqCst) && stream_supports_client_audio(&stream_config);
    audio_enabled.store(initial_audio, Ordering::SeqCst);
    let _ = punched.send_control(&ControlMessage::SetAudio(initial_audio).serialize());
    let mut last_audio_state = initial_audio;
    let mut last_debug_enabled = debug_enabled.load(Ordering::SeqCst);
    let mut next_clock_ping = Instant::now();
    let mut input_seq: u16 = 0;
    let mut mouse_heartbeat = MouseInputHeartbeat::default();
    let mut keyboard_heartbeat = KeyboardInputHeartbeat::default();
    let (clipboard_control_tx, clipboard_control_rx) =
        crossbeam_channel::bounded::<ControlMessage>(8);
    let (file_detect_tx, file_detect_rx) = crossbeam_channel::bounded::<std::path::PathBuf>(8);
    let suppressed_paths = clipboard::new_suppressed_paths();
    let mut clipboard_sync = clipboard::ClipboardSync::start_with_file_detection(
        "client-punched",
        true,
        {
            let shared_input = Arc::clone(&shared_input);
            move || shared_input.snapshot().controller_state == ControllerState::OwnedByYou
        },
        clipboard_control_tx,
        file_detect_tx,
        Arc::clone(&suppressed_paths),
    );
    let mut ft_manager = file_transfer::FileTransferManager::start_full(
        st_protocol::file_transfer::TransportMode::Punched,
        Arc::clone(&ft_shared_state),
        suppressed_paths,
    );

    // UDP gives no FIN; if the server vanishes (crash, network loss) nothing tells
    // us. The server pushes video every frame plus periodic control traffic, so a
    // few seconds of silence means it's gone — break out instead of sitting on a
    // dead session forever. Symmetric to the inactivity timeout in the host's
    // handle_punched_client loop.
    const PUNCHED_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(5);
    let mut last_peer_activity = Instant::now();

    loop {
        if session_cancelled(
            disconnect.as_ref(),
            connection_epoch.as_ref(),
            session_epoch,
        ) {
            break;
        }
        if last_peer_activity.elapsed() > PUNCHED_INACTIVITY_TIMEOUT {
            set_error(
                &state,
                &ctx,
                &disconnect,
                &connection_epoch,
                session_epoch,
                format!(
                    "Server unreachable: no traffic for {}s",
                    PUNCHED_INACTIVITY_TIMEOUT.as_secs()
                ),
            );
            break;
        }

        // Forward input from UI thread to server via punched socket media channel.
        let now = Instant::now();
        let mut did_work = false;
        let current_audio = audio_enabled.load(Ordering::SeqCst);
        if current_audio != last_audio_state {
            last_audio_state = current_audio;
            let _ = punched.send_control(&ControlMessage::SetAudio(current_audio).serialize());
            did_work = true;
        }
        let current_debug_enabled = debug_enabled.load(Ordering::SeqCst);
        if current_debug_enabled && !last_debug_enabled {
            next_clock_ping = Instant::now();
        }
        last_debug_enabled = current_debug_enabled;
        while let Ok(input_pkt) = input_rx.try_recv() {
            did_work = true;
            mouse_heartbeat.observe(input_pkt, now);
            keyboard_heartbeat.observe(input_pkt, now);
            let serialized = input_pkt.serialize(input_seq);
            input_seq = input_seq.wrapping_add(1);
            let _ = punched.send_media(&serialized);
            mouse_heartbeat.mark_sent(input_pkt, now);
            keyboard_heartbeat.mark_sent(input_pkt, now);
        }
        // Heartbeat retransmission for button/key state repair over lossy UDP.
        if let Some(pkt) = mouse_heartbeat.due_packet(now) {
            did_work = true;
            let serialized = pkt.serialize(input_seq);
            input_seq = input_seq.wrapping_add(1);
            let _ = punched.send_media(&serialized);
        }
        if let Some(pkt) = keyboard_heartbeat.due_packet(now) {
            did_work = true;
            let serialized = pkt.serialize(input_seq);
            input_seq = input_seq.wrapping_add(1);
            let _ = punched.send_media(&serialized);
        }

        // Forward outgoing control messages to punched socket.
        while let Ok(ctrl) = control_rx.try_recv() {
            did_work = true;
            let _ = punched.send_control(&ctrl.serialize());
        }
        while let Ok(ctrl) = clipboard_control_rx.try_recv() {
            did_work = true;
            let _ = punched.send_control(&ctrl.serialize());
        }
        while let Ok(path) = file_detect_rx.try_recv() {
            did_work = true;
            let _ = ft_manager
                .inbound_tx
                .try_send(file_transfer::FtInbound::SendFile { path });
        }
        while let Ok(ctrl) = ft_manager.outbound_rx.try_recv() {
            did_work = true;
            let _ = punched.send_control(&ctrl.serialize());
        }

        // Forward transport feedback.
        while let Ok(fb) = media.feedback_rx.try_recv() {
            did_work = true;
            let _ = punched.send_control(&ControlMessage::TransportFeedback(fb).serialize());
        }

        // Forward pipeline control messages (keyframe requests, etc.)
        while let Ok(ctrl) = pipeline_control_rx.try_recv() {
            did_work = true;
            let _ = punched.send_control(&ctrl.serialize());
        }
        if current_debug_enabled && Instant::now() >= next_clock_ping {
            let ping = ControlMessage::ClockSyncPing(ClockSyncPing {
                client_send_micros: unix_time_micros(),
            });
            let _ = punched.send_control(&ping.serialize());
            next_clock_ping = Instant::now() + Duration::from_secs(2);
            did_work = true;
        }

        punched.tick();
        let mut stop_session = false;
        loop {
            let incoming = punched.try_recv_all();
            if incoming.is_empty() {
                break;
            }
            did_work = true;
            last_peer_activity = Instant::now();
            for msg in incoming {
                match msg {
                    PunchedMessage::Media(data) => {
                        if media_packet_tx.send(data).is_err() {
                            stop_session = true;
                            break;
                        }
                    }
                    PunchedMessage::Control(data) => {
                        let mut offset = 0;
                        while let Some((msg, used)) = ControlMessage::deserialize(&data[offset..]) {
                            offset += used;
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
                                ControlMessage::ControllerState(cs) => {
                                    shared_input.set_controller_state(cs);
                                    ctx.request_repaint();
                                }
                                ControlMessage::InputCapabilities(caps) => {
                                    shared_input.set_capabilities(caps);
                                }
                                ControlMessage::CursorShape(shape) => {
                                    shared_input.set_cursor_shape(shape);
                                    ctx.request_repaint();
                                }
                                ControlMessage::CursorState(cs) => {
                                    shared_input.set_cursor_state(cs);
                                    ctx.request_repaint();
                                }
                                ControlMessage::ClipboardText(text) => {
                                    clipboard_sync.set_remote_text(text);
                                }
                                ControlMessage::FileOffer {
                                    transfer_id,
                                    file_size,
                                    file_name,
                                } => {
                                    let _ = ft_manager.inbound_tx.try_send(
                                        file_transfer::FtInbound::OfferReceived {
                                            transfer_id,
                                            file_size,
                                            file_name,
                                        },
                                    );
                                }
                                ControlMessage::FileAccept {
                                    transfer_id,
                                    accepted,
                                } => {
                                    let _ = ft_manager.inbound_tx.try_send(
                                        file_transfer::FtInbound::AcceptReceived {
                                            transfer_id,
                                            accepted,
                                        },
                                    );
                                }
                                ControlMessage::FileChunk {
                                    transfer_id,
                                    chunk_index,
                                    data,
                                } => {
                                    let _ = ft_manager.inbound_tx.try_send(
                                        file_transfer::FtInbound::ChunkReceived {
                                            transfer_id,
                                            chunk_index,
                                            data,
                                        },
                                    );
                                }
                                ControlMessage::FileComplete {
                                    transfer_id,
                                    total_chunks,
                                    sha256,
                                } => {
                                    let _ = ft_manager.inbound_tx.try_send(
                                        file_transfer::FtInbound::CompleteReceived {
                                            transfer_id,
                                            total_chunks,
                                            sha256,
                                        },
                                    );
                                }
                                ControlMessage::FileCancel { transfer_id } => {
                                    let _ = ft_manager.inbound_tx.try_send(
                                        file_transfer::FtInbound::CancelReceived { transfer_id },
                                    );
                                }
                                ControlMessage::FileProgress {
                                    transfer_id,
                                    chunks_received,
                                } => {
                                    let _ = ft_manager.inbound_tx.try_send(
                                        file_transfer::FtInbound::ProgressReceived {
                                            transfer_id,
                                            chunks_received,
                                        },
                                    );
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
                                    stop_session = true;
                                    break;
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
                                    stop_session = true;
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                if stop_session {
                    break;
                }
            }
            if stop_session {
                break;
            }
        }
        if stop_session {
            break;
        }

        while decode_started_rx.try_recv().is_ok() {
            startup_decode_ok = true;
        }
        if !startup_decode_ok && Instant::now() >= startup_deadline {
            let codec = stream_config.codec;
            eprintln!(
                "[codec] no frames decoded within {}s over punched transport — excluding {:?} and reconnecting",
                STARTUP_DECODE_TIMEOUT.as_secs(),
                codec,
            );
            excluded_video_codecs.lock().unwrap().insert(codec);
            *state.lock().unwrap() = ConnectionState::Disconnected;
            ctx.request_repaint();
            break;
        }

        if !did_work {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // Cleanup.
    clipboard_sync.stop();
    ft_manager.stop();
    stop_media_threads(media);

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
    // `Interrupted` (EINTR) is not a timeout but must be retried the same way.
    // io_uring's kernel-side worker threads deliver signals that interrupt
    // blocking syscalls on the same process; if we let EINTR tear down the
    // TCP control loop, the whole session collapses ~12 frames in. Before
    // this fix, ST_IO_URING=1 reproducibly disconnected after one decoded
    // frame with `Err kind=Interrupted` from `tcp.read`, even though the
    // decoder was producing frames correctly.
    e.kind() == std::io::ErrorKind::WouldBlock
        || e.kind() == std::io::ErrorKind::TimedOut
        || e.kind() == std::io::ErrorKind::Interrupted
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
    crypto: Option<&st_protocol::tunnel::CryptoContext>,
) {
    let raw = packet.serialize(*seq);
    *seq = seq.wrapping_add(1);
    if let Some(crypto) = crypto {
        let encrypted = crypto.encrypt(&raw);
        let _ = socket.send_to(&encrypted, target);
    } else {
        let _ = socket.send_to(&raw, target);
    }
}

fn run_input_sender(
    socket: UdpSocket,
    target: std::net::SocketAddr,
    input_rx: crossbeam_channel::Receiver<InputPacket>,
    shutdown_rx: crossbeam_channel::Receiver<()>,
    crypto: Option<Arc<st_protocol::tunnel::CryptoContext>>,
) {
    let mut seq = 0u16;
    let mut mouse_heartbeat = MouseInputHeartbeat::default();
    let mut keyboard_heartbeat = KeyboardInputHeartbeat::default();
    let _ = socket.set_write_timeout(Some(INPUT_STATE_HEARTBEAT_INTERVAL));
    let cref = crypto.as_deref();
    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        match input_rx.recv_timeout(INPUT_SENDER_POLL_INTERVAL) {
            Ok(packet) => {
                let now = Instant::now();
                mouse_heartbeat.observe(packet, now);
                keyboard_heartbeat.observe(packet, now);
                send_input_packet_raw(&socket, target, packet, &mut seq, cref);
                mouse_heartbeat.mark_sent(packet, now);
                keyboard_heartbeat.mark_sent(packet, now);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                if let Some(packet) = mouse_heartbeat.due_packet(now) {
                    send_input_packet_raw(&socket, target, packet, &mut seq, cref);
                }
                if let Some(packet) = keyboard_heartbeat.due_packet(now) {
                    send_input_packet_raw(&socket, target, packet, &mut seq, cref);
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

#[cfg(unix)]
fn set_udp_send_buffer(socket: &UdpSocket, size: i32) {
    use std::os::unix::io::AsRawFd;
    let fd = socket.as_raw_fd();
    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &size as *const i32 as *const _,
            std::mem::size_of::<i32>() as libc::socklen_t,
        )
    };
    if ret != 0 && trace_enabled() {
        eprintln!(
            "[udp] setsockopt SO_SNDBUF failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
fn set_udp_send_buffer(_socket: &UdpSocket, _size: i32) {}

#[cfg(unix)]
fn set_udp_dscp(socket: &UdpSocket, peer: std::net::SocketAddr, dscp: u8) {
    use std::os::unix::io::AsRawFd;

    let tos = i32::from(dscp) << 2;
    let (level, optname) = match peer.ip() {
        std::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
            (libc::IPPROTO_IPV6, libc::IPV6_TCLASS)
        }
        _ => (libc::IPPROTO_IP, libc::IP_TOS),
    };
    let fd = socket.as_raw_fd();
    let ret = unsafe {
        libc::setsockopt(
            fd,
            level,
            optname,
            &tos as *const i32 as *const _,
            std::mem::size_of::<i32>() as libc::socklen_t,
        )
    };
    if ret != 0 && trace_enabled() {
        eprintln!(
            "[udp] setsockopt DSCP failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
fn set_udp_dscp(_socket: &UdpSocket, _peer: std::net::SocketAddr, _dscp: u8) {}

#[cfg(target_os = "linux")]
fn set_udp_priority(socket: &UdpSocket, priority: i32) {
    use std::os::unix::io::AsRawFd;

    let fd = socket.as_raw_fd();
    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PRIORITY,
            &priority as *const i32 as *const _,
            std::mem::size_of::<i32>() as libc::socklen_t,
        )
    };
    if ret != 0 && trace_enabled() {
        eprintln!(
            "[udp] setsockopt SO_PRIORITY failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn set_udp_priority(_socket: &UdpSocket, _priority: i32) {}

fn configure_media_udp_socket(socket: &UdpSocket, peer: std::net::SocketAddr) {
    let recv_buf = std::env::var("ST_UDP_RCVBUF")
        .ok()
        .and_then(|raw| raw.parse::<i32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(4 * 1024 * 1024);
    let send_buf = std::env::var("ST_UDP_SNDBUF")
        .ok()
        .and_then(|raw| raw.parse::<i32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1024 * 1024);

    set_udp_recv_buffer(socket, recv_buf);
    set_udp_send_buffer(socket, send_buf);

    if let Some(dscp) = std::env::var("ST_UDP_DSCP")
        .ok()
        .and_then(|raw| raw.parse::<u8>().ok())
        .filter(|value| *value <= 63)
    {
        set_udp_dscp(socket, peer, dscp);
    }

    let priority = std::env::var("ST_UDP_SO_PRIORITY")
        .ok()
        .and_then(|raw| raw.parse::<i32>().ok())
        .filter(|value| *value >= 0)
        .unwrap_or(5);
    set_udp_priority(socket, priority);
}

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
    egui::pos2(
        pos.x.clamp(rect.left(), max_x),
        pos.y.clamp(rect.top(), max_y),
    )
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

fn relative_capture_entry_anchor(
    remote_pos: Option<egui::Pos2>,
    local_pos: Option<egui::Pos2>,
) -> Option<egui::Pos2> {
    remote_pos.or(local_pos)
}

fn relative_capture_tracking_anchor(
    remote_pos: Option<egui::Pos2>,
    local_prediction: Option<egui::Pos2>,
    local_prediction_recent: bool,
) -> Option<egui::Pos2> {
    if local_prediction_recent {
        local_prediction.or(remote_pos)
    } else {
        remote_pos.or(local_prediction)
    }
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

fn controller_state_allows_input(controller_state: ControllerState) -> bool {
    controller_state != ControllerState::Unavailable
}

fn controller_state_has_separate_cursor(controller_state: ControllerState) -> bool {
    matches!(
        controller_state,
        ControllerState::OwnedByYou | ControllerState::OwnedByOther
    )
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
        && controller_state_allows_input(controller_state)
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

fn wheel_unit_scale(unit: egui::MouseWheelUnit) -> f32 {
    match unit {
        // Trackpads often deliver small point deltas; keep them and convert
        // to high-resolution wheel units so remote scrolling does not feel sticky.
        egui::MouseWheelUnit::Point => f32::from(MOUSE_WHEEL_STEP_UNITS) / 24.0,
        egui::MouseWheelUnit::Line => f32::from(MOUSE_WHEEL_STEP_UNITS),
        egui::MouseWheelUnit::Page => f32::from(MOUSE_WHEEL_STEP_UNITS) * 6.0,
    }
}

fn take_wheel_units(accum: &mut f32) -> i16 {
    let whole = accum.trunc().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    *accum -= whole as f32;
    whole
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
                let angle =
                    start_angle + t * (end_angle - start_angle) - std::f32::consts::FRAC_PI_2;
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
            let mut tags = vec![if report.hardware.supports(codec) {
                "hw"
            } else {
                "sw"
            }];
            if report.yuv444.supports(codec) {
                tags.push(if report.hardware_yuv444.supports(codec) {
                    "444hw"
                } else {
                    "444sw"
                });
            }
            entries.push(format!("{name}({})", tags.join(",")));
        }
    }
    if entries.is_empty() {
        "-".to_string()
    } else {
        entries.join(" / ")
    }
}

fn stream_chroma_label(chroma: VideoChromaSampling) -> &'static str {
    match chroma {
        VideoChromaSampling::Yuv420 => "yuv420",
        VideoChromaSampling::Yuv444 => "yuv444",
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
                "{} {} {}x{} {} fps hdr={} audio={}ch/{}Hz",
                codec_label(cfg),
                stream_chroma_label(cfg.chroma),
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
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        api_client::unregister(&self.api_discovery);
    }

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
        self.video_texture.set_windows_overlayless_preferred(
            state == ConnectionState::Connected && self.prefers_windows_overlayless_present(),
        );
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
            let mut has_pending_upload = false;
            {
                let mut fb = self.frame.lock().unwrap();
                if fb.dirty && fb.width > 0 {
                    std::mem::swap(&mut self.upload_frame, &mut *fb);
                    fb.dirty = false;
                    self.upload_frame.dirty = false;
                    has_pending_upload = true;
                }
            }

            if !has_pending_upload {
                None
            } else if self.video_texture.stage_direct_frame(&self.upload_frame) {
                if self.debug_enabled {
                    self.debug_state
                        .record_present(&self.upload_frame, unix_time_micros());
                }
                None
            } else {
                match self.video_texture.upload(
                    frame,
                    &self.upload_frame,
                    self.native_surfaces.as_ref(),
                ) {
                    Ok(()) => {
                        if self.debug_enabled {
                            self.debug_state
                                .record_present(&self.upload_frame, unix_time_micros());
                        }
                        None
                    }
                    Err(err) => Some(err),
                }
            }
        };
        if let Some(err) = upload_error {
            self.video_texture.clear_frame();
            *self.state.lock().unwrap() =
                ConnectionState::Error(format!("GL upload failed: {err}"));
            ctx.request_repaint();
        }

        let debug_top =
            if state == ConnectionState::Connected && !self.video_texture.occludes_egui_overlay() {
                let menu_bottom = self.render_floating_menu(ctx);
                self.render_file_transfer_overlay(ctx, menu_bottom);
                menu_bottom
            } else {
                self.local_overlay_hit_rects.clear();
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
                    let tex_size = self
                        .video_space_size()
                        .unwrap_or_else(|| self.video_texture.size_vec2());
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
        let virtual_hover = self.capture_mode == LocalCaptureMode::HoverAbsolute
            && self.uses_virtual_hover_cursor(&snapshot);
        let mut last_pointer_pos = raw_input
            .events
            .iter()
            .rev()
            .find_map(|event| match event {
                egui::Event::PointerMoved(pos) => Some(*pos),
                _ => None,
            })
            .or(if virtual_hover {
                self.hover_cursor_pos
            } else {
                None
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
                    if !virtual_hover {
                        last_pointer_pos = Some(pos);
                    }
                    if self.await_pointer_exit_after_auto_release {
                        if video_rect.map(|rect| !rect.contains(pos)).unwrap_or(true) {
                            self.await_pointer_exit_after_auto_release = false;
                        }
                    }
                    if !snapshot.capabilities.hover_capture
                        && snapshot.capabilities.mouse_relative
                        && snapshot.capabilities.separate_cursor
                        && snapshot.cursor_state.visible
                        && controller_state_allows_input(snapshot.controller_state)
                        && self.capture_mode != LocalCaptureMode::CapturedRelative
                        && self.capture_mode != LocalCaptureMode::ForceReleased
                        && !self.await_pointer_exit_after_auto_release
                    {
                        if let Some(rect) = video_rect {
                            if rect.contains(pos) && !self.pointer_over_local_overlay(pos) {
                                let local_pos =
                                    clamp_pos_to_video_rect(pos, rect, ctx.pixels_per_point());
                                // Relative-only input cannot teleport to the local pointer.
                                // Keep the remote cursor position authoritative on entry.
                                let anchor_pos =
                                    self.mapped_server_cursor_video_pos(&snapshot, rect);
                                self.capture_mode = LocalCaptureMode::CapturedRelative;
                                self.resume_hover_after_relative_drag = false;
                                self.hover_cursor_resync_pending = false;
                                self.hover_cursor_pos = Some(clamp_pos_to_video_rect(
                                    relative_capture_entry_anchor(anchor_pos, Some(local_pos))
                                        .unwrap_or(local_pos),
                                    rect,
                                    ctx.pixels_per_point(),
                                ));
                                self.suppress_mouse_delta = true;
                                ctx.request_repaint();
                            }
                        }
                    }
                    if self.capture_mode == LocalCaptureMode::HoverAbsolute
                        && self.hover_cursor_resync_pending
                        && self.suppress_pointer_pos_frames == 0
                    {
                        if let Some(rect) = video_rect {
                            self.hover_cursor_pos =
                                Some(clamp_pos_to_video_rect(pos, rect, ctx.pixels_per_point()));
                        } else {
                            self.hover_cursor_pos = Some(pos);
                        }
                        self.hover_cursor_resync_pending = false;
                    }
                    if !virtual_hover
                        && self.capture_mode == LocalCaptureMode::HoverAbsolute
                        && controller_state_allows_input(snapshot.controller_state)
                    {
                        if let Some(rect) = video_rect {
                            if self.pointer_over_local_overlay(pos) {
                                self.auto_release_capture(false);
                                ctx.request_repaint();
                            } else if rect.contains(pos) {
                                let next_pos =
                                    clamp_pos_to_video_rect(pos, rect, ctx.pixels_per_point());
                                self.hover_cursor_pos = Some(next_pos);
                                self.send_absolute_cursor_if_needed(client_id, next_pos, rect);
                                ctx.request_repaint();
                            } else {
                                self.auto_release_capture(false);
                                ctx.request_repaint();
                            }
                        }
                    }
                }
                egui::Event::MouseMoved(delta) => {
                    if self.suppress_mouse_delta {
                        continue;
                    }
                    if self.capture_mode == LocalCaptureMode::CapturedRelative
                        && controller_state_allows_input(snapshot.controller_state)
                    {
                        let mut input_delta = delta;
                        if snapshot.capabilities.separate_cursor
                            && snapshot.cursor_state.visible
                            && !self.resume_hover_after_relative_drag
                        {
                            if let (Some(rect), Some(stream_size)) =
                                (video_rect, self.cursor_space_size())
                            {
                                if rect.width() > 0.0 && rect.height() > 0.0 {
                                    input_delta.x *= stream_size.x / rect.width();
                                    input_delta.y *= stream_size.y / rect.height();
                                }
                            }
                        }
                        let dx = input_delta
                            .x
                            .round()
                            .clamp(i16::MIN as f32, i16::MAX as f32)
                            as i16;
                        let dy = input_delta
                            .y
                            .round()
                            .clamp(i16::MIN as f32, i16::MAX as f32)
                            as i16;
                        let mut predicted_cursor_pos = None;
                        if snapshot.capabilities.separate_cursor && snapshot.cursor_state.visible {
                            if let Some(rect) = video_rect {
                                let base_pos = self
                                    .hover_cursor_pos
                                    .or_else(|| {
                                        self.mapped_server_cursor_video_pos(&snapshot, rect)
                                    })
                                    .or(last_pointer_pos)
                                    .unwrap_or_else(|| rect.center());
                                let attempted_pos =
                                    egui::pos2(base_pos.x + delta.x, base_pos.y + delta.y);
                                let allow_local_escape = self.pointer_buttons == 0
                                    && !self.resume_hover_after_relative_drag;
                                if allow_local_escape
                                    && self.pointer_over_local_overlay(attempted_pos)
                                {
                                    self.auto_release_capture(false);
                                    last_pointer_pos = Some(attempted_pos);
                                    ctx.send_viewport_cmd(egui::ViewportCommand::CursorPosition(
                                        attempted_pos,
                                    ));
                                    ctx.request_repaint();
                                    continue;
                                }
                                if allow_local_escape
                                    && (attempted_pos.x < rect.left()
                                        || attempted_pos.x > rect.right()
                                        || attempted_pos.y < rect.top()
                                        || attempted_pos.y > rect.bottom())
                                {
                                    let escape_pos = pointer_escape_position(
                                        rect,
                                        attempted_pos,
                                        ctx.pixels_per_point(),
                                    );
                                    self.auto_release_capture(true);
                                    last_pointer_pos = Some(escape_pos);
                                    ctx.send_viewport_cmd(egui::ViewportCommand::CursorPosition(
                                        escape_pos,
                                    ));
                                    ctx.request_repaint();
                                    continue;
                                }
                                predicted_cursor_pos = Some(clamp_pos_to_video_rect(
                                    attempted_pos,
                                    rect,
                                    ctx.pixels_per_point(),
                                ));
                            }
                        }
                        if dx != 0 || dy != 0 || predicted_cursor_pos.is_some() {
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
                            if let Some(next_pos) = predicted_cursor_pos {
                                self.hover_cursor_pos = Some(next_pos);
                                self.last_local_cursor_prediction_at = Some(Instant::now());
                                last_pointer_pos = Some(next_pos);
                            } else if self.resume_hover_after_relative_drag {
                                if let Some(rect) = video_rect {
                                    let Some(base_pos) = self.hover_cursor_pos.or(last_pointer_pos)
                                    else {
                                        continue;
                                    };
                                    let next_pos = clamp_pos_to_video_rect(
                                        egui::pos2(base_pos.x + delta.x, base_pos.y + delta.y),
                                        rect,
                                        ctx.pixels_per_point(),
                                    );
                                    self.hover_cursor_pos = Some(next_pos);
                                    self.last_local_cursor_prediction_at = Some(Instant::now());
                                    last_pointer_pos = Some(next_pos);
                                }
                            }
                            ctx.request_repaint();
                        }
                    } else if virtual_hover
                        && controller_state_allows_input(snapshot.controller_state)
                    {
                        if let Some(rect) = video_rect {
                            let hover_drag_active = self.capture_mode
                                == LocalCaptureMode::HoverAbsolute
                                && self.pointer_buttons != 0;
                            let base_pos = self
                                .hover_cursor_pos
                                .or(last_pointer_pos)
                                .unwrap_or_else(|| rect.center());
                            let unclamped = egui::pos2(base_pos.x + delta.x, base_pos.y + delta.y);
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
                                ctx.send_viewport_cmd(egui::ViewportCommand::CursorPosition(
                                    unclamped,
                                ));
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
                                let escape_pos = pointer_escape_position(
                                    rect,
                                    unclamped,
                                    ctx.pixels_per_point(),
                                );
                                self.auto_release_capture(true);
                                last_pointer_pos = Some(escape_pos);
                                ctx.send_viewport_cmd(egui::ViewportCommand::CursorPosition(
                                    escape_pos,
                                ));
                                ctx.request_repaint();
                                continue;
                            }
                            let next_pos =
                                clamp_pos_to_video_rect(unclamped, rect, ctx.pixels_per_point());
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
                    let relative_overlay_cursor = self.capture_mode
                        == LocalCaptureMode::CapturedRelative
                        && snapshot.capabilities.separate_cursor
                        && snapshot.cursor_state.visible;
                    let route_pos = if virtual_hover {
                        self.hover_cursor_pos.or(Some(pos))
                    } else if relative_overlay_cursor {
                        video_rect
                            .and_then(|rect| self.mapped_server_cursor_video_pos(&snapshot, rect))
                            .or(self.hover_cursor_pos)
                            .or(Some(pos))
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
                    if relative_overlay_cursor && over_local_overlay && pressed {
                        if let Some(route_pos) = route_pos {
                            ctx.send_viewport_cmd(egui::ViewportCommand::CursorPosition(route_pos));
                        }
                        self.auto_release_capture(false);
                        ctx.request_repaint();
                        continue;
                    }
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
                    let restore_hover_after_drag =
                        should_return_to_hover_after_relative_button_drag(
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
                    if controller_state_allows_input(snapshot.controller_state)
                        && !sent_button_state
                        && !over_local_overlay
                        && (self.capture_mode == LocalCaptureMode::CapturedRelative
                            || self.capture_mode == LocalCaptureMode::HoverAbsolute && over_video)
                    {
                        if self.capture_mode == LocalCaptureMode::HoverAbsolute {
                            if let (Some(route_pos), Some(rect)) = (route_pos, video_rect) {
                                let _ = self.send_absolute_cursor(client_id, route_pos, rect, true);
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
                                        rect.contains(*pos)
                                            && !self.pointer_over_local_overlay(*pos)
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
                    if !controller_state_allows_input(snapshot.controller_state) {
                        continue;
                    }
                    let relative_overlay_cursor = self.capture_mode
                        == LocalCaptureMode::CapturedRelative
                        && snapshot.capabilities.separate_cursor
                        && snapshot.cursor_state.visible;
                    let wheel_pos = if virtual_hover {
                        self.hover_cursor_pos.or(last_pointer_pos)
                    } else if relative_overlay_cursor {
                        video_rect
                            .and_then(|rect| self.mapped_server_cursor_video_pos(&snapshot, rect))
                            .or(self.hover_cursor_pos)
                            .or(last_pointer_pos)
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
                    self.send_remote_wheel(client_id, delta, unit);
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
    fn advertised_video_codec_support_disables_yuv444_when_toggle_is_off() {
        let report = decode::VideoCodecSupportReport {
            supported: VideoCodecSupport::all(),
            hardware: VideoCodecSupport::all(),
            yuv444: VideoCodecSupport::h264_only(),
            hardware_yuv444: VideoCodecSupport::h264_only(),
        };

        let filtered = filter_advertised_video_codec_support(report, false, true, true);

        assert_eq!(filtered.supported, report.supported);
        assert_eq!(filtered.hardware, report.hardware);
        assert!(filtered.yuv444.is_empty());
        assert!(filtered.hardware_yuv444.is_empty());
    }

    #[test]
    fn advertised_video_codec_support_disables_yuv444_when_platform_policy_blocks_it() {
        let report = decode::VideoCodecSupportReport {
            supported: VideoCodecSupport::all(),
            hardware: VideoCodecSupport::all(),
            yuv444: VideoCodecSupport::h264_only(),
            hardware_yuv444: VideoCodecSupport::h264_only(),
        };

        let filtered = filter_advertised_video_codec_support(report, true, true, false);

        assert_eq!(filtered.supported, report.supported);
        assert_eq!(filtered.hardware, report.hardware);
        assert_eq!(filtered.yuv444, report.yuv444);
        assert!(filtered.hardware_yuv444.is_empty());
    }

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

    #[test]
    fn relative_capture_entry_prefers_remote_cursor_position() {
        let remote = egui::pos2(100.0, 200.0);
        let local = egui::pos2(800.0, 600.0);

        assert_eq!(
            relative_capture_entry_anchor(Some(remote), Some(local)),
            Some(remote)
        );
    }

    #[test]
    fn relative_capture_entry_falls_back_without_remote_cursor_position() {
        let local = egui::pos2(800.0, 600.0);

        assert_eq!(
            relative_capture_entry_anchor(None, Some(local)),
            Some(local)
        );
    }

    #[test]
    fn relative_capture_tracking_prefers_recent_local_prediction() {
        let remote = egui::pos2(100.0, 200.0);
        let predicted = egui::pos2(120.0, 205.0);

        assert_eq!(
            relative_capture_tracking_anchor(Some(remote), Some(predicted), true),
            Some(predicted)
        );
    }

    #[test]
    fn relative_capture_tracking_returns_to_remote_after_prediction_hold() {
        let remote = egui::pos2(100.0, 200.0);
        let predicted = egui::pos2(120.0, 205.0);

        assert_eq!(
            relative_capture_tracking_anchor(Some(remote), Some(predicted), false),
            Some(remote)
        );
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Choose the best present mode we can *safely* request at surface config
/// time. `Mailbox` is a wgpu validation error on surfaces that only support
/// `Fifo`, which in practice covers software fallbacks (Mesa llvmpipe,
/// Microsoft Basic Render Driver). We probe the adapter list cheaply and
/// downgrade to `Fifo` if we only see software adapters.
fn pick_wgpu_present_mode() -> eframe::wgpu::PresentMode {
    use eframe::wgpu;
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });
    let adapters: Vec<_> = instance.enumerate_adapters(wgpu::Backends::all());
    if adapters.is_empty() {
        return wgpu::PresentMode::Fifo;
    }
    let has_hw = adapters.iter().any(|a| {
        let info = a.get_info();
        !matches!(info.device_type, wgpu::DeviceType::Cpu)
    });
    if has_hw {
        wgpu::PresentMode::Mailbox
    } else {
        eprintln!(
            "[main] No hardware wgpu adapter available (Mesa software fallback?); \
             using PresentMode::Fifo. Likely fix: add your user to the 'render' group \
             so /dev/dri/renderD* is accessible: sudo usermod -aG render $USER"
        );
        wgpu::PresentMode::Fifo
    }
}

fn main() {
    match updater::maybe_run_apply_update_from_args() {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => {
            eprintln!("[updater] {err}");
            // Exit cleanly to avoid triggering Windows Error Reporting dialogs.
            std::process::exit(0);
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

    // Renderer default: on Linux we default to wgpu because DMA-BUF zero-copy
    // import is validated end-to-end and wgpu additionally gets us
    // PresentMode::Mailbox + desired_maximum_frame_latency=1 for lower
    // compositor latency. On macOS and Windows we stay on Glow because the
    // VideoToolbox/IOSurface and D3D11 interop zero-copy paths are still
    // Glow-only — flipping to wgpu there would be a silent perf regression
    // until those platform-specific `wgpu_hal` external-memory imports land.
    // `ST_RENDERER=glow` forces the old default on any platform.
    let renderer_pref = std::env::var("ST_RENDERER")
        .unwrap_or_default()
        .to_lowercase();
    let default_to_wgpu = cfg!(target_os = "linux");
    let use_wgpu = match renderer_pref.as_str() {
        "wgpu" | "wgpu-min-latency" => true,
        "glow" => false,
        "" => default_to_wgpu,
        other => {
            eprintln!("[main] unknown ST_RENDERER={other:?}; falling back to default");
            default_to_wgpu
        }
    };
    // Mailbox gives us lower-latency compositor handoff than Fifo, but it's
    // not guaranteed to be supported — llvmpipe / software Vulkan surfaces
    // only expose Fifo, which makes `Surface::configure(Mailbox)` panic.
    // Probe the adapter type before committing and fall back to Fifo when
    // we're clearly on software rendering. Common failure mode: the user
    // isn't in the `render` group, so mesa can't open /dev/dri/renderD*
    // and silently falls back to llvmpipe.
    let wgpu_present_mode = if use_wgpu {
        pick_wgpu_present_mode()
    } else {
        eframe::wgpu::PresentMode::Fifo
    };

    if use_wgpu {
        eprintln!(
            "[main] wgpu renderer active ({:?} + max_frame_latency=1). \
             On Linux, DMA-BUF zero-copy import auto-enables when the Vulkan adapter \
             advertises VK_KHR_external_memory_fd + VK_EXT_external_memory_dma_buf. \
             Force Glow with ST_RENDERER=glow.",
            wgpu_present_mode,
        );
    }

    let options = if use_wgpu {
        let mut wgpu_cfg = eframe::egui_wgpu::WgpuConfiguration::default();
        wgpu_cfg.desired_maximum_frame_latency = Some(1);
        wgpu_cfg.present_mode = wgpu_present_mode;
        eframe::NativeOptions {
            viewport,
            renderer: eframe::Renderer::Wgpu,
            wgpu_options: wgpu_cfg,
            vsync: display::vsync_enabled(),
            ..Default::default()
        }
    } else {
        eframe::NativeOptions {
            viewport,
            renderer: eframe::Renderer::Glow,
            vsync: display::vsync_enabled(),
            ..Default::default()
        }
    };

    eframe::run_native(
        "Stream Client",
        options,
        Box::new(|cc| Ok(Box::new(StreamApp::new(cc)))),
    )
    .expect("Failed to run eframe");
}
