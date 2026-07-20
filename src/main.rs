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
use st_client_core::drain_control_messages;
use st_protocol::{
    ClientDisplayInfo, ClockSyncPing, ControlMessage, ControllerState, InputCredential,
    InputPacket, KeyboardKey, KeyboardStateInput, MouseAbsoluteInput, MouseButtonsInput,
    MouseRelativeInput, MouseWheelInput, StreamConfig, TransportFeedback, VideoChromaSampling,
    VideoCodec, VideoCodecSupport, MOUSE_BUTTON_EXTRA1, MOUSE_BUTTON_EXTRA2, MOUSE_BUTTON_MIDDLE,
    MOUSE_BUTTON_PRIMARY, MOUSE_BUTTON_SECONDARY, MOUSE_WHEEL_STEP_UNITS,
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

use crate::debug_state::{
    clock_sync_rtt_micros, loss_percent, mono_micros, unix_time_micros, ConnectionDebugSnapshot,
    ConnectionDebugState,
};

const DEFAULT_APP_PORT: u16 = 28_480;
const DISCOVERY_PORT: u16 = 28_481;
const DISCOVERY_EXPIRY: Duration = Duration::from_secs(10);
/// While connected over a public/VPN path, how long a LAN beacon for the same
/// peer must persist before we tear the WAN session down and re-connect over
/// LAN. Short enough to switch promptly on returning to the server's subnet,
/// long enough that a single stray beacon can't thrash a healthy session.
const LAN_UPGRADE_HYSTERESIS: Duration = Duration::from_secs(2);
const MAX_REMOTE_CURSOR_TEXTURES: usize = 8;
const INPUT_SENDER_POLL_INTERVAL: Duration = Duration::from_millis(20);
const INPUT_STATE_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(50);
const INPUT_STATE_REPAIR_WINDOW: Duration = Duration::from_millis(200);
/// Consecutive frames the server must report the cursor hidden before the
/// client switches into relative game capture (mouselook). Debounced so a
/// single `visible=false` pulse from the compositor does not lock the pointer
/// during ordinary desktop use.
const CURSOR_HIDDEN_CAPTURE_FRAMES: u8 = 4;
/// Wall-clock backstop for committing the OS-cursor hide before the remote
/// overlay is painted. The 1-frame `os_cursor_hide_settle` counter slips when a
/// frame hitches, and a compositor can take more than one frame to actually hide
/// the OS cursor after `CursorVisible(false)` (notably Wayland/KDE, under load,
/// or at high refresh where one frame is only a few ms). Painting the overlay
/// before the hide commits is the "both cursors at once" bug. Mirrors the
/// `suppress_mouse_delta_until` wall-clock backstop.
///
/// Two-sided tradeoff: too short re-admits the double cursor on a slow hide; too
/// long shows *no* cursor for `this - actual_hide_latency` after entering the
/// video on a fast compositor (native arrow gone, overlay not yet drawn). 50ms
/// (~3 frames @60Hz) covers a hitched frame and typical compositor latency while
/// keeping any fast-path no-cursor gap imperceptible. Tune live per compositor.
const OS_CURSOR_HIDE_SETTLE: Duration = Duration::from_millis(50);
/// Exit hysteresis (symmetric to the entry guard above): the server cursor must
/// be continuously *shown* for this many frames before we drop game (relative)
/// capture. Without it, a single stale/blipped `CursorState{visible=true}` — a
/// server-game hitch, or a stutter delivering one old frame — would instantly
/// flip the grab `Locked→None`, the OS warps the pointer to re-center, and that
/// warp delta jumps the remote camera. A genuine release (game shows the desktop
/// cursor) persists well past this, so the ~150-200ms debounce is imperceptible.
const CURSOR_SHOWN_RELEASE_FRAMES: u8 = 10;
/// How long a locally-predicted cursor position is trusted for the relative
/// (game) overlay before it re-anchors to the server-reported position. Long
/// enough to bridge the gap between consecutive mouse-move events at low poll
/// rates, short enough that the overlay snaps back to where clicks actually
/// land shortly after the user stops moving.
const LOCAL_CURSOR_PREDICTION_TTL: Duration = Duration::from_millis(120);

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
    peer_id: String,
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
    // Auto-reconnect on unexpected connection loss (network drop, server
    // restart, transient unreachable). Terminal errors (auth rejected, explicit
    // server rejection) are excluded so we never hammer a server that said no.
    // Connection-level only — adds no latency to the live media path.
    auto_reconnect_attempts: u32,
    next_reconnect_at: Option<Instant>,
    // Stable identity of the machine the live/last session targets, captured at
    // connect time from the discovered/saved card. Auto-reconnect re-resolves
    // this peer's best *current* path each attempt instead of replaying the
    // address the session opened with (which a network switch can leave dead).
    // None when connecting to a bare address with no known peer_id.
    connect_peer_id: Option<String>,
    // First instant a stable LAN beacon for `connect_peer_id` was seen while the
    // live session runs over a non-LAN path. Drives the proactive LAN upgrade
    // after `LAN_UPGRADE_HYSTERESIS`. Reset whenever LAN is absent or we're
    // already on LAN.
    lan_upgrade_since: Option<Instant>,
    shared_input: Arc<SharedInputState>,
    control_tx: Option<crossbeam_channel::Sender<ControlMessage>>,
    input_tx: Option<crossbeam_channel::Sender<InputPacket>>,
    capture_mode: LocalCaptureMode,
    pointer_buttons: u8,
    keyboard_state: LocalKeyboardState,
    pending_capture_click: bool,
    last_video_rect: Option<egui::Rect>,
    last_sent_absolute_cursor: Option<(u16, u16)>,
    // Local pointer position used to draw the remote cursor overlay in Desktop
    // (HoverAbsolute) mode. In Desktop mode the drawn cursor is *always* the
    // local pointer position — the server's reported cursor position is never
    // read back to place it, which is what eliminates the position feedback
    // loop and the cursor "jumping".
    hover_cursor_pos: Option<egui::Pos2>,
    // Consecutive frames the server has reported the cursor hidden. Drives the
    // automatic Desktop -> Game (relative mouselook) capture transition.
    cursor_hidden_frames: u8,
    // Exit-side counterpart: consecutive frames the server cursor has been shown.
    // Drives the release hysteresis so a single blipped CursorState can't drop
    // game capture and warp/jump the camera (see CURSOR_SHOWN_RELEASE_FRAMES).
    cursor_shown_frames: u8,
    // Game (CapturedRelative) sub-state: true while a hidden-cursor button drag
    // temporarily promoted Desktop hover into relative capture. On button
    // release we drop straight back to HoverAbsolute instead of staying locked.
    resume_hover_after_relative_drag: bool,
    // Latched screen->stream delta-scale decision for the duration of a
    // button-held relative drag. Frozen so a stale CursorState from a dropped
    // frame (cursor_state.visible flipping mid-drag) can't suddenly amplify or
    // shrink the relative delta and jolt the remote camera. None when not in a
    // held drag.
    relative_drag_scaling: Option<bool>,
    // Set when we re-enter Desktop hover and must force a one-shot absolute
    // resync so the server cursor snaps to the local overlay even if the pointer
    // is stationary (no PointerMoved event to trigger the usual send).
    hover_cursor_resync_pending: bool,
    // When the relative-mode overlay was last advanced from a locally predicted
    // delta. While this is recent the overlay is drawn at the predicted local
    // position (1:1, no network latency); once it goes stale the overlay
    // re-anchors to the server-reported cursor so clicks land where it is drawn.
    last_local_cursor_prediction_at: Option<Instant>,
    remote_cursor_textures: BTreeMap<u64, RemoteCursorTexture>,
    latest_remote_cursor_serial: Option<u64>,
    seen_cursor_shape_version: u64,
    debug_overlay_tab: DebugOverlayTab,
    graph_overlay: graph_overlay::GraphOverlay,
    /// Throttled cache for the text overlay's General-tab lines. Rebuilding the
    /// ~12 `format!`ed lines (and cloning the string-heavy snapshot) every frame
    /// is wasteful; we refresh them at ~6Hz instead.
    debug_lines_cache: Vec<String>,
    debug_lines_built: Instant,
    menu_open: bool,
    menu_button_pos: egui::Pos2,
    menu_button_drag_origin: Option<egui::Pos2>,
    /// Active set of client-HUD rectangles the pointer can be "over" instead of
    /// the video. Read by `pointer_over_local_overlay`. Holds the rects
    /// collected during the *previous* frame (see `pending_overlay_hit_rects`),
    /// so it is complete regardless of the order overlays render in.
    local_overlay_hit_rects: Vec<egui::Rect>,
    /// Rects registered by overlays *this* frame via `register_overlay_rect`.
    /// Swapped into `local_overlay_hit_rects` at the start of the next frame.
    pending_overlay_hit_rects: Vec<egui::Rect>,
    last_pointer_move: Option<Instant>,
    await_pointer_exit_after_auto_release: bool,
    applied_cursor_visible: Option<bool>,
    applied_cursor_grab: Option<egui::CursorGrab>,
    /// Frames to wait after requesting the OS cursor be hidden before painting
    /// the remote-cursor overlay. The compositor applies `CursorVisible(false)`
    /// asynchronously (~1 frame late), so drawing the overlay the same frame we
    /// request the hide briefly shows both the OS cursor and the overlay at a
    /// HUD/edge→video transition. Deferring one frame keeps exactly one cursor.
    os_cursor_hide_settle: u8,
    /// Wall-clock backstop paired with `os_cursor_hide_settle`: the frame counter
    /// alone slips when a frame hitches and a compositor can take more than one
    /// frame to apply `CursorVisible(false)`, so the overlay must also wait until
    /// this instant before painting. `None` once the cursor is shown again.
    os_cursor_hide_settle_until: Option<Instant>,
    /// Whether the pointer was over our surface last frame (`hover_pos().is_some()`).
    /// The compositor only (re)applies our hidden-cursor state when the pointer
    /// *enters* the surface, so a rising edge here — even with no change to our own
    /// `applied_cursor_visible` belief — must re-arm the hide-settle backstop. This
    /// is the "cross in from a window stacked on top" double-cursor case.
    prev_pointer_present: bool,
    pending_wheel_units: egui::Vec2,
    /// Number of upcoming frames whose `MouseMoved` deltas must be dropped.
    /// Set on every `CursorGrab` change because locking/warping the pointer
    /// makes the OS emit a spurious recentering delta that can land one or two
    /// frames late; a single-frame skip is not enough to catch it.
    suppress_mouse_delta: u8,
    // Wall-clock backstop for the warp suppression above. The frame counter
    // alone slips when a frame hitches: the OS recenter delta can land a few ms
    // after the 2-frame window expired, sail through, and jump the camera. A
    // short time window survives hitches regardless of frame cadence.
    suppress_mouse_delta_until: Option<Instant>,
    suppress_pointer_pos_frames: u8,
    excluded_video_codecs: Arc<Mutex<st_protocol::VideoCodecSupport>>,
    /// Per-server sticky fallback: holds the normalized addresses of servers
    /// for which a session had a live TCP control link but zero UDP media
    /// (UDP blocked by firewall/proxy/NAT). The next connect to *that* server
    /// tunnels all media over TCP; other servers are unaffected. An address is
    /// removed when its TCP-tunnel session fails to establish, so the UDP path
    /// gets retried.
    force_tcp_media: Arc<Mutex<std::collections::HashSet<String>>>,
    file_transfer_state: file_transfer::SharedTransferState,
    update_ui_state: UpdateUiState,
    update_tx: crossbeam_channel::Sender<UpdateWorkerEvent>,
    update_rx: crossbeam_channel::Receiver<UpdateWorkerEvent>,
}

// ---------------------------------------------------------------------------
// Server address persistence
// ---------------------------------------------------------------------------

/// Whether the remote-cursor overlay uses strict presence gating: paint it only
/// once the OS cursor is committed-hidden past the wall-clock settle AND the
/// window is focused with the pointer genuinely over the video. Closes the
/// "both cursors" cases at window edges / on focus changes / when the pointer
/// left the surface (egui's `latest_pos` lingers there). Default-on; set
/// `ST_CURSOR_STRICT_PRESENCE=0` (`false`/`no`/`off`) to revert to the prior
/// 1-frame-settle-only behavior for debugging.
fn cursor_strict_presence() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        !matches!(
            std::env::var("ST_CURSOR_STRICT_PRESENCE").ok().as_deref(),
            Some("0") | Some("false") | Some("no") | Some("off")
        )
    })
}

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

/// Reachability class of a server address, used for the connection-path badge.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PathClass {
    /// Same L2 / private LAN — direct, lowest latency.
    Lan,
    /// VPN or CGNAT overlay (10.x, 100.64-127, Tailscale) — direct but routed.
    Vpn,
    /// Public internet — reached via NAT hole punch.
    Public,
}

fn classify_path(addr: &str) -> PathClass {
    match addr_ip(addr) {
        Some(std::net::IpAddr::V4(v4)) => {
            let o = v4.octets();
            if (o[0] == 192 && o[1] == 168)
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || v4.is_link_local()
                || v4.is_loopback()
            {
                PathClass::Lan
            } else if o[0] == 10 || (o[0] == 100 && (64..=127).contains(&o[1])) {
                PathClass::Vpn
            } else {
                PathClass::Public
            }
        }
        Some(std::net::IpAddr::V6(v6)) => {
            if v6.is_loopback() || v6.is_unicast_link_local() {
                PathClass::Lan
            } else if v6.is_unique_local() {
                PathClass::Vpn
            } else {
                PathClass::Public
            }
        }
        None => PathClass::Public,
    }
}

fn allow_hole_punch_fallback(socket_addr: std::net::SocketAddr) -> bool {
    !is_privateish_ip(socket_addr.ip())
}

/// Choose the host candidate a *remote* client can actually reach.
///
/// Server candidates arrive sorted LAN-first (the server's own best path), but
/// an API-discovered host with no LAN beacon means the client is almost
/// certainly not on the server's LAN. The server's 192.168/172.16 address is
/// then both unreachable AND — being private-ish — would suppress the
/// hole-punch fallback in `connect()` (`allow_hole_punch_fallback`), so a
/// connect attempt just hard-fails. Prefer a shared VPN address
/// (Tailscale/CGNAT, directly routable across networks), else a public address
/// (which routes via hole punch), and fall back to a LAN address only when the
/// host advertised nothing better. When the client *is* co-LAN, `best_path`
/// already overrides this with the real beacon address.
fn pick_remote_reachable(candidates: &[String]) -> Option<String> {
    candidates
        .iter()
        .min_by_key(|a| match classify_path(a) {
            PathClass::Vpn => 0u8,
            PathClass::Public => 1,
            PathClass::Lan => 2,
        })
        .cloned()
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

fn touch_server_connected(list: &mut [ServerEntry], addr: &str) {
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
                    // peer_id (line 5) is mandatory: it is the identity used to merge
                    // a machine's LAN and public variants into one card. A beacon
                    // without it cannot be deduplicated, so ignore it.
                    let peer_id = lines
                        .get(4)
                        .map(|value| value.trim())
                        .filter(|value| !value.is_empty());
                    let discover_valid = lines.len() >= 4 && lines[0] == "ST_DISCOVER";
                    if let Some(peer_id) = peer_id.filter(|_| discover_valid) {
                        let hostname = lines[1].to_string();
                        let port: u16 = lines[2].parse().unwrap_or(DEFAULT_APP_PORT);
                        let token = lines[3].to_string();
                        let peer_id = peer_id.to_string();
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
            auto_reconnect_attempts: 0,
            next_reconnect_at: None,
            connect_peer_id: None,
            lan_upgrade_since: None,
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
            cursor_hidden_frames: 0,
            cursor_shown_frames: 0,
            resume_hover_after_relative_drag: false,
            relative_drag_scaling: None,
            hover_cursor_resync_pending: false,
            last_local_cursor_prediction_at: None,
            remote_cursor_textures: BTreeMap::new(),
            latest_remote_cursor_serial: None,
            seen_cursor_shape_version: 0,
            debug_overlay_tab: DebugOverlayTab::General,
            graph_overlay: graph_overlay::GraphOverlay::new(),
            debug_lines_cache: Vec::new(),
            debug_lines_built: Instant::now() - Duration::from_secs(1),
            menu_open: false,
            menu_button_pos,
            menu_button_drag_origin: None,
            local_overlay_hit_rects: Vec::new(),
            pending_overlay_hit_rects: Vec::new(),
            last_pointer_move: None,
            await_pointer_exit_after_auto_release: false,
            applied_cursor_visible: None,
            applied_cursor_grab: None,
            os_cursor_hide_settle: 0,
            os_cursor_hide_settle_until: None,
            prev_pointer_present: false,
            pending_wheel_units: egui::Vec2::ZERO,
            suppress_mouse_delta: 0,
            suppress_mouse_delta_until: None,
            suppress_pointer_pos_frames: 0,
            excluded_video_codecs: Arc::new(Mutex::new(st_protocol::VideoCodecSupport::empty())),
            force_tcp_media: Arc::new(Mutex::new(std::collections::HashSet::new())),
            file_transfer_state: file_transfer::new_shared_state(),
            update_ui_state,
            update_tx,
            update_rx,
        }
    }

    /// Re-resolve the best currently-reachable address for a peer we are
    /// (re)connecting to. Mirrors the server-list merge: a live, token-matched
    /// LAN beacon wins (a packet we actually received), else the public/VPN
    /// path advertised over the API for this peer. Returns `None` when the peer
    /// has no live variant right now — the caller then keeps the last-known
    /// address and lets backoff retry until a path reappears.
    fn best_addr_for_peer(&self, peer_id: &str) -> Option<String> {
        let lan = self
            .discovered_servers
            .lock()
            .unwrap()
            .iter()
            .find(|d| {
                d.peer_id == peer_id
                    && d.token == self.token
                    && !self.token.is_empty()
                    && d.last_seen.elapsed() < DISCOVERY_EXPIRY
            })
            .map(|d| d.address.clone());
        if lan.is_some() {
            return lan;
        }
        let host = self.api_discovery.host.lock().unwrap();
        host.as_ref()
            .filter(|h| {
                h.peer_id.as_deref() == Some(peer_id)
                    && !h.candidates.is_empty()
                    && h.last_seen.elapsed() < Duration::from_secs(30)
            })
            .and_then(|h| pick_remote_reachable(&h.candidates))
    }

    /// A fresh, token-matched LAN beacon address for the currently-targeted peer
    /// — only when we are *not* already on a LAN path and it differs from the
    /// current address. This is the low-latency path worth switching to, whether
    /// the session is live (proactive upgrade) or mid-backoff (collapse the wait).
    fn pending_lan_upgrade(&self) -> Option<String> {
        let pid = self.connect_peer_id.as_deref()?;
        if classify_path(&self.server_addr) == PathClass::Lan {
            return None;
        }
        self.discovered_servers
            .lock()
            .unwrap()
            .iter()
            .find(|d| {
                d.peer_id == pid
                    && d.token == self.token
                    && !self.token.is_empty()
                    && d.last_seen.elapsed() < DISCOVERY_EXPIRY
            })
            .map(|d| d.address.clone())
            .filter(|a| a != &self.server_addr)
    }

    /// While connected over a public/VPN path, switch to the server's LAN address
    /// as soon as a LAN beacon for that peer has been stable for
    /// `LAN_UPGRADE_HYSTERESIS`. LAN is the low-latency path this app exists to
    /// use; staying on WAN after returning to the server's subnet is a large,
    /// needless latency cost, and waiting for the WAN session to drop on its own
    /// is what made the switch feel slow. Resets the timer when no upgrade is
    /// pending so a single stray beacon can't tear a healthy session.
    fn maybe_upgrade_to_lan(&mut self, ctx: &egui::Context) {
        let Some(lan_addr) = self.pending_lan_upgrade() else {
            self.lan_upgrade_since = None;
            return;
        };
        let stable_since = *self.lan_upgrade_since.get_or_insert_with(Instant::now);
        if stable_since.elapsed() >= LAN_UPGRADE_HYSTERESIS {
            self.lan_upgrade_since = None;
            eprintln!(
                "[connect] LAN path for peer is up — upgrading from {} to {lan_addr}",
                self.server_addr
            );
            self.server_addr = lan_addr;
            self.video_texture.clear_frame();
            self.connect(ctx.clone());
        } else {
            // Keep re-checking until hysteresis elapses even if video is static.
            ctx.request_repaint_after(LAN_UPGRADE_HYSTERESIS);
        }
    }

    fn connect(&mut self, ctx: egui::Context) {
        let saved_addr = self.server_addr.trim().to_string();
        touch_server_connected(&mut self.server_list, &saved_addr);
        // A reconnect attempt is now in flight; clear the pending schedule so the
        // update loop doesn't double-fire. `auto_reconnect_attempts` is reset on
        // a successful connection (state reaches Connected), not here, so backoff
        // keeps growing across a run of failures.
        self.next_reconnect_at = None;
        self.lan_upgrade_since = None;

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
        self.pending_wheel_units = egui::Vec2::ZERO;
        self.cursor_hidden_frames = 0;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.last_local_cursor_prediction_at = None;
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
        let force_tcp_media = Arc::clone(&self.force_tcp_media);
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
                force_tcp_media,
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
        // User-initiated teardown cancels any pending auto-reconnect.
        self.auto_reconnect_attempts = 0;
        self.next_reconnect_at = None;
        self.lan_upgrade_since = None;
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
        self.pending_wheel_units = egui::Vec2::ZERO;
        self.cursor_hidden_frames = 0;
        self.remote_cursor_textures.clear();
        self.latest_remote_cursor_serial = None;
        self.seen_cursor_shape_version = 0;
        self.menu_open = false;
        self.local_overlay_hit_rects.clear();
        self.pending_overlay_hit_rects.clear();
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
        self.pending_wheel_units = egui::Vec2::ZERO;
        self.cursor_hidden_frames = 0;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.last_local_cursor_prediction_at = None;
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
        self.pending_wheel_units = egui::Vec2::ZERO;
        self.cursor_hidden_frames = 0;
        self.menu_open = false;
        self.await_pointer_exit_after_auto_release = false;
        self.local_overlay_hit_rects.clear();
        self.pending_overlay_hit_rects.clear();
    }

    /// Entering relative (game) capture by click on a relative-only backend
    /// (no absolute injection): anchor the rendered overlay at the local pointer
    /// and warp the server cursor to match, so the cursor does not teleport to
    /// the server-reported position on entry. Subsequent relative deltas keep
    /// both aligned.
    fn anchor_relative_capture_to_local(
        &mut self,
        ctx: &egui::Context,
        video_rect: egui::Rect,
        pointer_pos: Option<egui::Pos2>,
        snapshot: &input::SharedInputSnapshot,
    ) {
        let Some(client_id) = snapshot.client_id else {
            return;
        };
        let local =
            pointer_pos.map(|pos| clamp_pos_to_video_rect(pos, video_rect, ctx.pixels_per_point()));
        let remote = self.mapped_server_cursor_video_pos(snapshot, video_rect);
        if let Some(anchor) = relative_capture_entry_anchor(remote, local) {
            self.hover_cursor_pos = Some(anchor);
            self.last_local_cursor_prediction_at = Some(Instant::now());
        }
        if let Some(local) = local {
            self.send_relative_warp_to_local(client_id, local, video_rect, snapshot);
        }
    }

    fn force_release_capture(&mut self) {
        self.capture_mode = LocalCaptureMode::ForceReleased;
        self.pending_capture_click = false;
        self.pointer_buttons = 0;
        self.last_sent_absolute_cursor = None;
        self.hover_cursor_pos = None;
        self.cursor_hidden_frames = 0;
        self.resume_hover_after_relative_drag = false;
        self.hover_cursor_resync_pending = false;
        self.last_local_cursor_prediction_at = None;
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
            && server_cursor_drawable(&snapshot)
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
        // Send wheel deltas exactly as the OS reports them. On macOS the sign
        // already reflects the system "natural scrolling" setting, so that
        // toggle is the single source of truth for remote scroll direction —
        // no app-level inversion.
        let scaled = delta * wheel_unit_scale(unit);
        self.pending_wheel_units += scaled;
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

    /// The single rule for "this is HUD, not video": every client overlay
    /// (menu button, menu popup, file-transfer card, debug overlay, graph
    /// overlay, and any future one) calls this as it renders. The pointer being
    /// over any registered rect makes the cursor logic keep the OS cursor shown
    /// instead of the remote overlay. Rects are collected per frame and promoted
    /// to the active set next frame, so registration order is irrelevant.
    fn register_overlay_rect(&mut self, rect: egui::Rect) {
        self.pending_overlay_hit_rects.push(rect);
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

    /// Local pointer is over the video and not over the local HUD overlay, in
    /// HoverAbsolute desktop mode. Shared by the overlay-active and
    /// remote-cursor-hidden checks so both use one definition of "over video".
    fn hover_pointer_over_video(
        &self,
        ctx: &egui::Context,
        snapshot: &input::SharedInputSnapshot,
    ) -> bool {
        let pointer_pos = self.hover_cursor_pos.or_else(|| {
            if self.uses_virtual_hover_cursor(snapshot) {
                None
            } else {
                ctx.input(|i| i.pointer.latest_pos())
            }
        });
        // 1 logical-pixel tolerance avoids the OS cursor flickering back
        // on sub-pixel/fractional-DPI boundary frames.
        pointer_pos
            .zip(self.current_video_rect(ctx).or(self.last_video_rect))
            .map(|(pointer_pos, rect)| {
                rect.expand(1.0).contains(pointer_pos)
                    && !self.pointer_over_local_overlay(pointer_pos)
            })
            .unwrap_or(false)
    }

    /// True when the client is drawing its own remote-cursor overlay this
    /// frame and the OS cursor should therefore be hidden.
    fn overlay_cursor_active(&self, ctx: &egui::Context) -> bool {
        let snapshot = self.shared_input.snapshot();
        if !controller_state_has_separate_cursor(snapshot.controller_state)
            || !snapshot.capabilities.separate_cursor
        {
            return false;
        }
        // We only draw a custom cursor when we actually have its shape; without
        // one we leave the native OS cursor visible so there is always exactly
        // one cursor on screen.
        if self
            .remote_cursor_texture_for_serial(snapshot.cursor_state.serial)
            .is_none()
        {
            return false;
        }

        match self.capture_mode {
            // Desktop: drawn at the local pointer position. Active whenever
            // that pointer is over the video and not over the local HUD.
            LocalCaptureMode::HoverAbsolute => self.hover_pointer_over_video(ctx, &snapshot),
            // Game: drawn at the server-reported position, only while the cursor
            // is drawable (hidden or app_grab == mouselook, nothing to draw).
            LocalCaptureMode::CapturedRelative => server_cursor_drawable(&snapshot),
            _ => false,
        }
    }

    /// Relative (game) capture where the server reports a visible cursor but we
    /// have no shape bitmap to draw: fall back to showing the native OS cursor
    /// confined to the window rather than hiding the pointer with nothing drawn.
    fn native_cursor_fallback_active(&self) -> bool {
        let snapshot = self.shared_input.snapshot();
        self.capture_mode == LocalCaptureMode::CapturedRelative
            && controller_state_has_separate_cursor(snapshot.controller_state)
            && snapshot.capabilities.separate_cursor
            && server_cursor_drawable(&snapshot)
            && self
                .remote_cursor_texture_for_serial(snapshot.cursor_state.serial)
                .is_none()
    }

    /// HoverAbsolute desktop mode where the server owns the cursor and reports
    /// it hidden while the local pointer is over the video. The OS pointer must
    /// then be hidden too (with nothing drawn) so the local view matches the
    /// remote — e.g. a fullscreen video player that auto-hides the cursor.
    fn hover_remote_cursor_hidden(
        &self,
        ctx: &egui::Context,
        snapshot: &input::SharedInputSnapshot,
    ) -> bool {
        self.capture_mode == LocalCaptureMode::HoverAbsolute
            && controller_state_has_separate_cursor(snapshot.controller_state)
            && snapshot.capabilities.separate_cursor
            && server_wants_relative(snapshot)
            && self.hover_pointer_over_video(ctx, snapshot)
    }

    fn apply_pointer_capture_mode(&mut self, ctx: &egui::Context) {
        let snapshot = self.shared_input.snapshot();
        let overlay_cursor_active = self.overlay_cursor_active(ctx);
        let (cursor_visible, cursor_grab) = match self.capture_mode {
            // Game / relative capture: the OS cursor is locked + hidden so we
            // can read raw relative deltas (mouselook), unless there is no shape
            // to draw, in which case we show the native cursor confined.
            LocalCaptureMode::CapturedRelative => {
                if self.native_cursor_fallback_active() {
                    (true, egui::CursorGrab::Confined)
                } else {
                    (false, egui::CursorGrab::Locked)
                }
            }
            LocalCaptureMode::HoverAbsolute => {
                if self.uses_virtual_hover_cursor(&snapshot) {
                    // macOS without a separate cursor: lock + hide, draw virtual.
                    (false, egui::CursorGrab::Locked)
                } else {
                    // Desktop: keep the pointer free (grab None) so the user can
                    // leave the video to control their own machine at any time.
                    // While a button is held we confine it so a drag (text
                    // selection, window drag) cannot escape the video mid-drag.
                    let dragging = self.pointer_buttons != 0
                        && controller_state_allows_input(snapshot.controller_state);
                    let grab = if dragging {
                        egui::CursorGrab::Confined
                    } else {
                        egui::CursorGrab::None
                    };
                    // Hide the OS cursor while drawing the overlay, and also when
                    // the server reports its cursor hidden over the video (e.g.
                    // fullscreen-video auto-hide): the remote shows no cursor, so
                    // neither should we. The pointer reappears at the video edges
                    // / local HUD where hover_pointer_over_video stops matching.
                    let hide_os =
                        overlay_cursor_active || self.hover_remote_cursor_hidden(ctx, &snapshot);
                    (!hide_os, grab)
                }
            }
            // Idle / ForceReleased: the local OS cursor is the real cursor.
            _ => (true, egui::CursorGrab::None),
        };

        // Hide the cursor before changing the grab mode so that
        // CursorGrab::Locked (which centres the OS cursor on Windows) does
        // not flash the pointer at the centre of the window.  When making
        // the cursor visible again, do it after the grab change so the
        // cursor appears at the released position, not the locked one.
        // Pointer surface entry, independent of our own visibility belief. The
        // compositor (re)applies our cursor state only when the pointer enters our
        // surface, so crossing in from a window stacked on top of ours — where our
        // belief was already `hidden` (held by a stale `latest_pos`) and so no
        // visible→hidden transition fires below — would otherwise flash the OS
        // arrow under the overlay with no settle armed. Re-arm on the rising edge.
        let pointer_present = ctx.input(|i| i.pointer.hover_pos()).is_some();
        let pointer_just_entered = pointer_present && !self.prev_pointer_present;
        self.prev_pointer_present = pointer_present;

        if !cursor_visible
            && (self.applied_cursor_visible != Some(false)
                || (pointer_just_entered && self.applied_cursor_visible == Some(false)))
        {
            if self.applied_cursor_visible != Some(false) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(false));
                self.applied_cursor_visible = Some(false);
            }
            // Defer the overlay: the hide applies asynchronously. The frame
            // counter handles the common case; the wall-clock backstop covers a
            // compositor that takes longer than one (possibly hitched) frame to
            // actually hide the cursor — that gap is the "both cursors" bug.
            self.os_cursor_hide_settle = 1;
            self.os_cursor_hide_settle_until = Some(Instant::now() + OS_CURSOR_HIDE_SETTLE);
        }
        if self.applied_cursor_grab != Some(cursor_grab) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(cursor_grab));
            self.suppress_mouse_delta = 2;
            // Wall-clock backstop: the recenter delta can land after the 2-frame
            // window when a frame hitches. 80ms covers the OS warp latency on any
            // cadence; the cost is a tiny input deadzone right at the transition.
            self.suppress_mouse_delta_until = Some(Instant::now() + Duration::from_millis(80));
            self.suppress_pointer_pos_frames = 2;
            self.applied_cursor_grab = Some(cursor_grab);
        }
        if cursor_visible && self.applied_cursor_visible != Some(true) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(true));
            self.applied_cursor_visible = Some(true);
            self.os_cursor_hide_settle_until = None;
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

    /// One-shot MouseRelative packet that warps the server cursor to land at
    /// `local_pos` (a screen-coords position inside the video rect).  Used on
    /// Idle→CapturedRelative entry on relative-only backends: the overlay
    /// renders at local pos for the user, and this delta tells the server to
    /// move its cursor there so subsequent deltas keep both aligned and clicks
    /// land where the user sees the cursor.
    fn send_relative_warp_to_local(
        &self,
        client_id: u32,
        local_pos: egui::Pos2,
        video_rect: egui::Rect,
        snapshot: &input::SharedInputSnapshot,
    ) {
        if !snapshot.capabilities.mouse_relative {
            return;
        }
        let Some(stream_size) = self.cursor_space_size() else {
            return;
        };
        if stream_size.x <= 0.0
            || stream_size.y <= 0.0
            || video_rect.width() <= 0.0
            || video_rect.height() <= 0.0
        {
            return;
        }
        let scale_x = stream_size.x / video_rect.width();
        let scale_y = stream_size.y / video_rect.height();
        let local_tip_stream_x = (local_pos.x - video_rect.left()) * scale_x;
        let local_tip_stream_y = (local_pos.y - video_rect.top()) * scale_y;
        // cursor_state.x/y is the top-left of the cursor bitmap in stream
        // coords; the tip ("hotspot") is at cursor_state + hotspot.
        let hotspot = self
            .remote_cursor_texture_for_serial(snapshot.cursor_state.serial)
            .map(|tex| (tex.hotspot.x, tex.hotspot.y))
            .unwrap_or((0.0, 0.0));
        let server_tip_stream_x = snapshot.cursor_state.x as f32 + hotspot.0;
        let server_tip_stream_y = snapshot.cursor_state.y as f32 + hotspot.1;
        let dx = (local_tip_stream_x - server_tip_stream_x)
            .round()
            .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        let dy = (local_tip_stream_y - server_tip_stream_y)
            .round()
            .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        if dx == 0 && dy == 0 {
            return;
        }
        self.send_input_packet(InputPacket::MouseRelative(MouseRelativeInput {
            client_id,
            dx,
            dy,
            buttons: self.pointer_buttons,
        }));
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
        {
            return None;
        }
        // The server owns cursor visibility in both modes: when it reports the
        // cursor hidden (mouselook in Game, fullscreen-video auto-hide on the
        // desktop) or app_grab (warp/park mouselook) we draw nothing, so the
        // local view matches the remote.
        if !server_cursor_drawable(input_snapshot) {
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

        // The two modes have exactly one source of position each — no merging,
        // no fallback, no timer arbitration. That is what stops the jumping.
        //   * Desktop (HoverAbsolute): the local pointer, 1:1. The server cursor
        //     follows via absolute injection but is never read back here.
        //   * Game (CapturedRelative): the server-reported position, scaled into
        //     the video rect.
        let (cursor_pos, using_local_pos) = match self.capture_mode {
            LocalCaptureMode::HoverAbsolute => {
                let pos = self
                    .hover_cursor_pos
                    .filter(|pos| video_rect.expand(1.0).contains(*pos))?;
                (pos, true)
            }
            LocalCaptureMode::CapturedRelative => {
                // While the user is actively moving, draw at the locally
                // predicted position so the cursor tracks the hand 1:1 with no
                // network latency. Once movement goes idle, fall back to the
                // server-reported position so any accumulated relative-delta
                // rounding drift re-anchors to where clicks actually land.
                let recent_local_prediction = self
                    .last_local_cursor_prediction_at
                    .map(|at| at.elapsed() < LOCAL_CURSOR_PREDICTION_TTL)
                    .unwrap_or(false);
                let local_prediction = self
                    .hover_cursor_pos
                    .filter(|pos| video_rect.expand(1.0).contains(*pos));
                let server_top_left = self.server_cursor_stream_top_left(input_snapshot);
                let server_pos = egui::pos2(
                    video_rect.left() + server_top_left.x * scale_x + hotspot.x,
                    video_rect.top() + server_top_left.y * scale_y + hotspot.y,
                );
                // When the server can't report a true cursor position (KMS: the
                // legacy plane reports (0,0)), never re-anchor to server_pos on
                // idle — it would snap an in-game menu cursor to the top-left
                // corner. Hold the local prediction instead.
                let prefer_local = recent_local_prediction
                    || !input_snapshot.capabilities.cursor_position_reliable;
                let anchor = relative_capture_tracking_anchor(
                    Some(server_pos),
                    local_prediction,
                    prefer_local,
                )
                .unwrap_or(server_pos);
                let using_local = prefer_local && local_prediction.is_some();
                (anchor, using_local)
            }
            _ => return None,
        };
        let top_left = egui::pos2(cursor_pos.x - hotspot.x, cursor_pos.y - hotspot.y);

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
                "cursor: mode={} controller={:?} visible={} app_grab={} separate={} hover={} overlay_active={} native_fallback={}",
                self.capture_mode.label(),
                input_snapshot.controller_state,
                if input_snapshot.cursor_state.visible { "y" } else { "n" },
                if input_snapshot.cursor_state.app_grab { "y" } else { "n" },
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
        let controller_ok = controller_state_allows_input(snapshot.controller_state);
        let absolute_ok = snapshot.capabilities.mouse_absolute;
        let relative_ok = snapshot.capabilities.mouse_relative;
        let separate_cursor = snapshot.capabilities.separate_cursor;
        // The server advertises hover_capture when it can position an absolute
        // cursor for Parsec-style desktop control.
        let hover_supported = snapshot.capabilities.hover_capture && absolute_ok;
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
        // macOS virtual-hover keeps the OS cursor locked and tracks a synthetic
        // pointer; remap it when the video rect changes so it stays put.
        if virtual_hover
            && previous_capture_mode == LocalCaptureMode::HoverAbsolute
            && controller_ok
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
                    .or(if self.hover_cursor_pos.is_some() {
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
        // 1 logical-pixel tolerance so fractional-DPI sub-pixel boundary noise
        // does not flap the over-video test every frame.
        let pointer_inside_video_rect = pointer_pos
            .map(|pos| video_rect.expand(1.0).contains(pos))
            .unwrap_or(false);
        let pointer_over_video = pointer_inside_video_rect && !pointer_over_local_overlay;
        let clicked_video = response.clicked_by(egui::PointerButton::Primary) && pointer_over_video;
        let dragging = self.pointer_buttons != 0;
        if self.await_pointer_exit_after_auto_release && !pointer_inside_video_rect {
            self.await_pointer_exit_after_auto_release = false;
        }

        // Lost control entirely -> hands off.
        if !controller_ok
            && matches!(
                self.capture_mode,
                LocalCaptureMode::HoverAbsolute | LocalCaptureMode::CapturedRelative
            )
        {
            self.capture_mode = LocalCaptureMode::Idle;
        }

        // Window focus lost: the OS resets grab/visibility. Re-apply next frame,
        // and drop any game capture so the pointer is never stuck locked.
        if !ctx.input(|i| i.focused) {
            self.applied_cursor_grab = None;
            self.applied_cursor_visible = None;
            if self.capture_mode == LocalCaptureMode::CapturedRelative {
                self.force_release_capture();
            }
        }

        // Track how long the server has reported the cursor hidden. A game that
        // grabs the pointer for mouselook hides it; that is our signal to switch
        // into relative capture (and back out when it reappears).
        // Only honor a hidden cursor once the server has actually sent at least
        // one real CursorState (version > 0). The client's initial cursor_state
        // defaults to visible=false, and the server suppresses the all-zero
        // default, so without this guard the client misreads its own
        // uninitialized state as "a game hid the cursor" and locks itself into
        // relative capture on connect — pointer grabbed and no cursor drawn,
        // with no way to leave the window short of force-release.
        // "Server wants relative": cursor hidden, OR the warp detector flagged a
        // mouselook-without-hide app (app_grab). Folding app_grab in here means
        // the same hidden-frame counters drive entry into relative capture AND
        // gate the exit — while app_grab holds, cursor_shown_frames never climbs,
        // so the client can't bounce back to HoverAbsolute on the still-visible
        // cursor.
        let server_cursor_hidden = separate_cursor
            && controller_state_has_separate_cursor(snapshot.controller_state)
            && snapshot.cursor_state_version > 0
            && server_wants_relative(&snapshot);
        if server_cursor_hidden {
            self.cursor_hidden_frames = self.cursor_hidden_frames.saturating_add(1);
            self.cursor_shown_frames = 0;
        } else {
            self.cursor_hidden_frames = 0;
            self.cursor_shown_frames = self.cursor_shown_frames.saturating_add(1);
        }
        let want_game_capture =
            relative_ok && self.cursor_hidden_frames >= CURSOR_HIDDEN_CAPTURE_FRAMES;

        // ---- Mode resolution -------------------------------------------------
        // ForceReleased stays hands-off until the user clicks back into the
        // video (handled in the click block below).
        if controller_ok && self.capture_mode != LocalCaptureMode::ForceReleased {
            if self.capture_mode == LocalCaptureMode::CapturedRelative {
                // Currently in Game/relative capture. On an absolute-capable
                // backend, leave it once the server has shown the cursor again
                // for a *sustained* run (the game genuinely released the pointer).
                // Relative-only backends stay captured until force-release.
                //
                // The sustained requirement (cursor_shown_frames) is the fix for
                // the camera jump: a single stale/blipped CursorState{visible} —
                // from a server-game hitch or a stutter delivering one old frame —
                // must NOT flip the grab, warp the OS pointer, and snap the camera.
                // Entry already debounces (cursor_hidden_frames); this makes exit
                // symmetric. Still also deferred while a button is held (active
                // mouselook drag) so a release mid-drag can't switch to absolute.
                if absolute_ok
                    && self.cursor_shown_frames >= CURSOR_SHOWN_RELEASE_FRAMES
                    && self.pointer_buttons == 0
                {
                    // Warp the local pointer to where the cursor now is so Desktop
                    // control resumes from the same spot. This is the one place we
                    // read the server position back into a local pos — a single
                    // discrete event, not a per-frame feedback loop. But only when
                    // that position is real: on KMS the server reports (0,0), so
                    // reading it here teleports the cursor to the top-left corner
                    // the instant an in-game menu releases the pointer. There we
                    // resume from the position we have been predicting instead.
                    let resume_pos = if snapshot.capabilities.cursor_position_reliable {
                        self.mapped_server_cursor_video_pos(&snapshot, video_rect)
                    } else {
                        self.hover_cursor_pos
                    };
                    if let Some(pos) = resume_pos {
                        let clamped =
                            clamp_pos_to_video_rect(pos, video_rect, ctx.pixels_per_point());
                        self.hover_cursor_pos = Some(clamped);
                        ctx.send_viewport_cmd(egui::ViewportCommand::CursorPosition(clamped));
                    }
                    self.capture_mode = if hover_supported && pointer_over_video {
                        LocalCaptureMode::HoverAbsolute
                    } else {
                        LocalCaptureMode::Idle
                    };
                    self.last_sent_absolute_cursor = None;
                    ctx.request_repaint();
                }
            } else if want_game_capture
                && (self.capture_mode == LocalCaptureMode::HoverAbsolute || pointer_over_video)
            {
                // A game grabbed the pointer while we were engaged with the
                // video: enter relative mouselook capture.
                self.capture_mode = LocalCaptureMode::CapturedRelative;
                ctx.request_repaint();
            } else if hover_supported
                && pointer_over_video
                && !self.await_pointer_exit_after_auto_release
            {
                // Desktop passthrough: automatic free-cursor control while the
                // pointer is over the video.
                self.capture_mode = LocalCaptureMode::HoverAbsolute;
            } else if self.capture_mode == LocalCaptureMode::HoverAbsolute
                && !pointer_over_video
                && !dragging
            {
                // Pointer left the video (or moved onto the HUD) and no button is
                // held: hand control straight back to the local machine.
                self.auto_release_capture(false);
            }
        }

        // Click re-arms forwarding from a hands-off state. Clicking while
        // already forwarding just acts (handled in the raw input hook), so it
        // must not override an active Game capture here.
        if response.clicked_by(egui::PointerButton::Primary) && pointer_over_video {
            if controller_ok {
                if matches!(
                    self.capture_mode,
                    LocalCaptureMode::Idle | LocalCaptureMode::ForceReleased
                ) {
                    if hover_supported {
                        self.capture_mode = LocalCaptureMode::HoverAbsolute;
                    } else if relative_ok {
                        self.capture_mode = LocalCaptureMode::CapturedRelative;
                        self.anchor_relative_capture_to_local(
                            ctx,
                            video_rect,
                            pointer_pos,
                            &snapshot,
                        );
                    }
                }
                self.await_pointer_exit_after_auto_release = false;
                self.pending_capture_click = false;
            } else {
                // Capabilities/controller state arrive over TCP after
                // StreamStarted; remember the click and re-evaluate once they do.
                self.pending_capture_click = true;
            }
            ctx.request_repaint();
        }
        if self.pending_capture_click && controller_ok {
            if matches!(
                self.capture_mode,
                LocalCaptureMode::Idle | LocalCaptureMode::ForceReleased
            ) {
                if hover_supported {
                    self.capture_mode = LocalCaptureMode::HoverAbsolute;
                } else if relative_ok {
                    self.capture_mode = LocalCaptureMode::CapturedRelative;
                    self.anchor_relative_capture_to_local(ctx, video_rect, pointer_pos, &snapshot);
                }
            }
            self.await_pointer_exit_after_auto_release = false;
            self.pending_capture_click = false;
        }

        let _ = clicked_video;

        if previous_capture_mode != self.capture_mode && self.capture_mode == LocalCaptureMode::Idle
        {
            self.clear_remote_keyboard();
        }

        // ---- Desktop absolute send + local cursor tracking -------------------
        // In Desktop mode the local pointer is the single source of truth: we
        // record it (to draw the overlay at exactly that spot) and forward it as
        // an absolute position. The server's reported cursor position is never
        // read back here, which is what removes the position feedback loop.
        if self.capture_mode == LocalCaptureMode::HoverAbsolute && controller_ok {
            // One-shot resync after returning to Desktop hover: force the next
            // absolute send so the server cursor snaps to the local overlay even
            // if the pointer is stationary (no PointerMoved to trigger a send).
            let force_resync = std::mem::take(&mut self.hover_cursor_resync_pending);
            if virtual_hover {
                let desired_hover_pos = if previous_capture_mode == LocalCaptureMode::HoverAbsolute
                {
                    self.hover_cursor_pos.or(pointer_pos)
                } else {
                    actual_pointer_pos.or(pointer_pos)
                }
                .map(|pos| clamp_pos_to_video_rect(pos, video_rect, ctx.pixels_per_point()));
                if let Some(pos) = desired_hover_pos {
                    self.hover_cursor_pos = Some(pos);
                }
                if let (Some(client_id), Some(pos)) = (snapshot.client_id, self.hover_cursor_pos) {
                    let _ = self.send_absolute_cursor(client_id, pos, video_rect, force_resync);
                }
            } else {
                let hover_pos = actual_pointer_pos
                    .or(pointer_pos)
                    .filter(|pos| !self.pointer_over_local_overlay(*pos))
                    .map(|pos| clamp_pos_to_video_rect(pos, video_rect, ctx.pixels_per_point()));
                self.hover_cursor_pos = hover_pos;
                if let (Some(client_id), Some(pos)) = (snapshot.client_id, hover_pos) {
                    let _ = self.send_absolute_cursor(client_id, pos, video_rect, force_resync);
                } else {
                    self.last_sent_absolute_cursor = None;
                }
            }
        } else if self.capture_mode != LocalCaptureMode::CapturedRelative {
            // Idle / ForceReleased: nothing local-driven. (Game mode draws from
            // the server position and does not use hover_cursor_pos.)
            self.hover_cursor_pos = None;
            self.last_sent_absolute_cursor = None;
        }

        self.draw_remote_cursor_overlay(ctx);
    }

    fn draw_remote_cursor_overlay(&self, ctx: &egui::Context) {
        if self.video_texture.occludes_egui_overlay() {
            return;
        }
        // Invariant: the overlay is painted only while the OS cursor is
        // committed-hidden (and the hide has settled a frame). This guarantees
        // exactly one cursor across HUD/edge transitions — overlay drawn iff OS
        // cursor hidden — instead of briefly showing both.
        if self.applied_cursor_visible != Some(false) || self.os_cursor_hide_settle > 0 {
            return;
        }
        // Strict presence (default-on, ST_CURSOR_STRICT_PRESENCE=0 to disable):
        // the OS-cursor hide is asynchronous and the 1-frame counter above slips
        // when a frame hitches, so also wait on the wall-clock backstop; and only
        // paint while the window is focused with the pointer genuinely over it.
        // These close the "both cursors" cases: a compositor slow to hide the
        // cursor, the pointer at a flush window edge / over a foreign window, or
        // a stale `latest_pos` lingering after the pointer left the surface.
        if cursor_strict_presence() {
            if self
                .os_cursor_hide_settle_until
                .is_some_and(|t| Instant::now() < t)
            {
                return;
            }
            if !ctx.input(|i| i.focused) {
                return;
            }
            // The desktop (HoverAbsolute) overlay tracks the local pointer; if the
            // pointer is not currently over our window the OS is drawing its own
            // cursor elsewhere, so a second one here would be the stale-position
            // double. (Relative/game capture draws at the server position with the
            // pointer locked, where hover_pos() is legitimately None.)
            if self.capture_mode == LocalCaptureMode::HoverAbsolute
                && ctx.input(|i| i.pointer.hover_pos()).is_none()
            {
                return;
            }
        }

        let snapshot = self.shared_input.snapshot();
        // A single draw path: if we have a remote cursor shape and a position
        // for it (local pointer in Desktop, server position in Game), draw it.
        // Otherwise nothing is drawn and the native OS cursor stays visible
        // (apply_pointer_capture_mode keeps them in sync), so there is always
        // exactly one cursor on screen.
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
        }
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

        // Collect LAN-discovered servers (token-matched, non-expired). The beacon
        // parser now requires a peer_id, so every LAN entry can be merged by identity.
        let discovered = self.discovered_servers.lock().unwrap().clone();
        let lan_servers: Vec<&DiscoveredServer> = discovered
            .iter()
            .filter(|d| d.token == self.token && !self.token.is_empty())
            .filter(|d| d.last_seen.elapsed() < DISCOVERY_EXPIRY)
            .collect();

        // API-discovered host. peer_id is mandatory to merge it against a LAN/saved
        // variant of the same machine; without it we cannot tell variants apart, so
        // an identity-less API host is dropped (the server always advertises one).
        // Window must stay larger than the API discovery poll cadence
        // (start_api_discovery in api_client.rs, 10s when keyed) or the public
        // host blinks out between polls.
        let api_host = self.api_discovery.host.lock().unwrap().clone().filter(|h| {
            !h.candidates.is_empty()
                && h.last_seen.elapsed() < Duration::from_secs(30)
                && h.peer_id.as_ref().map(|p| !p.is_empty()).unwrap_or(false)
        });
        let any_api = api_host.is_some();

        // Aggregate every reachable variant of a logical server, keyed by peer_id.
        // The same machine can be seen at once over LAN (beacon) and the public
        // internet (API candidates); collapse them into one card and pick the best
        // path, so a public-only card upgrades to LAN in place the moment a beacon
        // arrives for the same peer_id.
        #[derive(Default)]
        struct Variant {
            lan_addr: Option<String>,
            api_addr: Option<String>,
            hostname: Option<String>,
        }
        let mut by_peer: BTreeMap<String, Variant> = BTreeMap::new();
        for d in &lan_servers {
            let v = by_peer.entry(d.peer_id.clone()).or_default();
            v.lan_addr = Some(d.address.clone());
            if v.hostname.is_none() && !d.hostname.is_empty() {
                v.hostname = Some(d.hostname.clone());
            }
        }
        if let Some(h) = &api_host {
            let pid = h
                .peer_id
                .clone()
                .expect("filtered for non-empty peer_id above");
            let v = by_peer.entry(pid).or_default();
            // Candidates arrive sorted LAN-first (the server's best *local* path),
            // but a remote client needs the candidate reachable from where it sits
            // — and a private LAN addr here would also block the punch fallback.
            if v.api_addr.is_none() {
                v.api_addr = pick_remote_reachable(&h.candidates);
            }
            if v.hostname.is_none() {
                v.hostname = h.hostname.clone();
            }
        }

        // Best address + path for a discovered variant: prefer a confirmed LAN beacon
        // (we literally received a packet from it) over advertised API candidates.
        let best_path = |v: &Variant| -> Option<(String, PathClass)> {
            if let Some(a) = &v.lan_addr {
                return Some((a.clone(), classify_path(a)));
            }
            v.api_addr.as_ref().map(|a| (a.clone(), classify_path(a)))
        };

        // Merged entry for rendering.
        struct MergedServer {
            card_key: String, // stable egui id: peer_id when known, else address
            connect_addr: String,
            display_name: String,
            subtitle: String,
            peer_id: Option<String>,
            path: Option<PathClass>, // Some only when a live variant exists
            is_dynamic: bool,        // currently discovered — don't allow delete
            saved_idx: Option<usize>,
            icon_active: bool,
        }

        let mut merged: Vec<MergedServer> = Vec::new();
        let mut used_peers: BTreeSet<String> = BTreeSet::new();
        // card_key must be unique — egui persistent ids collide otherwise.
        let mut seen_keys: BTreeSet<String> = BTreeSet::new();

        // 1) Saved servers (most recently connected first), upgraded to their live
        //    variant when one is currently discovered.
        let mut indices: Vec<usize> = (0..self.server_list.len()).collect();
        indices.sort_by(|&a, &b| {
            self.server_list[b]
                .last_connected
                .cmp(&self.server_list[a].last_connected)
        });
        for idx in indices {
            let entry = &self.server_list[idx];
            let card_key = entry
                .peer_id
                .clone()
                .unwrap_or_else(|| entry.address.clone());
            // Skip a second saved entry that resolves to the same machine.
            if !seen_keys.insert(card_key.clone()) {
                continue;
            }
            let live = entry
                .peer_id
                .as_ref()
                .and_then(|pid| by_peer.get(pid).map(|v| (pid.clone(), v)));
            let (connect_addr, path, host, online) = match live {
                Some((pid, v)) => match best_path(v) {
                    Some((addr, p)) => {
                        used_peers.insert(pid);
                        (addr, Some(p), v.hostname.clone(), true)
                    }
                    None => (entry.address.clone(), None, None, false),
                },
                None => (entry.address.clone(), None, None, false),
            };
            let display_name = if !entry.nickname.is_empty() {
                entry.nickname.clone()
            } else if let Some(h) = host.as_ref().filter(|h| !h.is_empty()) {
                h.clone()
            } else {
                entry.address.clone()
            };
            let subtitle = if online {
                truncate_str(&connect_addr, 24).to_string()
            } else if !entry.nickname.is_empty() {
                truncate_str(&entry.address, 24).to_string()
            } else {
                format_last_connected(entry.last_connected)
            };
            merged.push(MergedServer {
                card_key,
                connect_addr,
                display_name,
                subtitle,
                peer_id: entry.peer_id.clone(),
                path,
                is_dynamic: online,
                saved_idx: Some(idx),
                icon_active: entry.last_connected > 0 || online,
            });
        }

        // 2) Discovered servers (LAN and/or API) with no saved entry yet.
        for (pid, v) in &by_peer {
            if used_peers.contains(pid) || !seen_keys.insert(pid.clone()) {
                continue;
            }
            let Some((addr, path)) = best_path(v) else {
                continue;
            };
            let display_name = v
                .hostname
                .clone()
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| addr.clone());
            merged.push(MergedServer {
                card_key: pid.clone(),
                connect_addr: addr.clone(),
                display_name,
                subtitle: truncate_str(&addr, 24).to_string(),
                peer_id: Some(pid.clone()),
                path: Some(path),
                is_dynamic: true,
                saved_idx: None,
                icon_active: true,
            });
        }

        // Filter by search query.
        let query = self.search_query.trim().to_lowercase();
        if !query.is_empty() {
            merged.retain(|m| {
                m.connect_addr.to_lowercase().contains(&query)
                    || m.display_name.to_lowercase().contains(&query)
            });
        }

        let mut connect_addr: Option<String> = None;
        let mut connect_peer_id: Option<String> = None;
        let mut delete_idx: Option<usize> = None;

        if merged.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                let msg = if self.server_list.is_empty() && lan_servers.is_empty() && !any_api {
                    "No computers\nAdd a server address using the bar below."
                } else {
                    "No matches"
                };
                ui.label(egui::RichText::new(msg).size(14.0).color(TEXT_DIM));
            });
        } else {
            let avail_w = ui.available_width();
            let cols = ((avail_w + 12.0) / (CARD_W + 12.0)).floor().max(1.0) as usize;
            let total_rows = merged.len().div_ceil(cols);
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
                        let card_id = ui.make_persistent_id(("server-card", srv.card_key.as_str()));
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

                        // Connection-path badge (LAN / VPN / WAN), shown only for a
                        // live discovered variant. Flips in place as the path upgrades.
                        if let Some(path) = srv.path {
                            const ACCENT_VPN: egui::Color32 = egui::Color32::from_rgb(90, 160, 230);
                            const ACCENT_WAN: egui::Color32 = egui::Color32::from_rgb(220, 170, 90);
                            let (label, color) = match path {
                                PathClass::Lan => ("LAN", ACCENT_GREEN),
                                PathClass::Vpn => ("VPN", ACCENT_VPN),
                                PathClass::Public => ("WAN", ACCENT_WAN),
                            };
                            let badge_y = if srv.subtitle.is_empty() {
                                sub_y
                            } else {
                                sub_y + 14.0
                            };
                            painter.text(
                                egui::pos2(icon_cx, badge_y),
                                egui::Align2::CENTER_CENTER,
                                label,
                                egui::FontId::proportional(9.0),
                                color,
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
                            ui.make_persistent_id(("server-connect", srv.card_key.as_str())),
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
                            connect_addr = Some(srv.connect_addr.clone());
                            connect_peer_id = srv.peer_id.clone();
                            // Backfill the saved entry: match by stable peer_id when
                            // known (the connect address may be a freshly-discovered
                            // LAN addr that differs from what was saved), else address.
                            let sl = &mut self.server_list;
                            let found = srv
                                .peer_id
                                .as_ref()
                                .and_then(|pid| {
                                    sl.iter().position(|e| e.peer_id.as_deref() == Some(pid))
                                })
                                .or_else(|| sl.iter().position(|e| e.address == srv.connect_addr));
                            let mut changed = false;
                            if let Some(i) = found {
                                let entry = &mut sl[i];
                                // Copy hostname as nickname for discovered entries.
                                if srv.saved_idx.is_none()
                                    && entry.nickname.is_empty()
                                    && srv.display_name != srv.connect_addr
                                {
                                    entry.nickname = srv.display_name.clone();
                                    changed = true;
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
                                ui.make_persistent_id(("server-delete", srv.card_key.as_str())),
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
            self.connect_peer_id = connect_peer_id;
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
        self.register_overlay_rect(button_rect);
        if button_response.clicked() {
            self.menu_open = !self.menu_open;
            self.last_pointer_move = Some(Instant::now());
        }

        let mut overlay_top = button_rect.bottom() + 10.0;
        if self.menu_open {
            let mut request_disconnect = false;
            let mut audio_toggled = false;
            let mut debug_toggled = false;
            let mut selected_output_request: Option<u32> = None;
            let menu_snapshot = self.shared_input.snapshot();
            let available_outputs = menu_snapshot.available_outputs.clone();
            let selected_output = menu_snapshot.selected_output;
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

                                // Monitor picker — only when the server reports
                                // more than one capturable output (KMS path).
                                if available_outputs.len() > 1 {
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.add_space(4.0);
                                    ui.label(egui::RichText::new("Monitor").size(12.0).weak());
                                    for out in &available_outputs {
                                        let is_selected = selected_output == Some(out.id);
                                        let label = format!(
                                            "{}{} ({}×{})",
                                            if is_selected { "● " } else { "   " },
                                            out.name,
                                            out.width,
                                            out.height
                                        );
                                        let mut button = egui::Button::new(label);
                                        if is_selected {
                                            button =
                                                button.fill(egui::Color32::from_rgb(40, 70, 110));
                                        }
                                        if ui.add_sized([170.0, 26.0], button).clicked()
                                            && !is_selected
                                        {
                                            selected_output_request = Some(out.id);
                                        }
                                    }
                                }

                                ui.add_space(8.0);
                                ui.separator();
                                ui.add_space(8.0);
                                self.render_update_panel(ui, ctx, true);
                            });
                        });
                });
            let menu_rect = menu.response.rect;
            self.register_overlay_rect(menu_rect);
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
            if let Some(id) = selected_output_request {
                if let Some(tx) = &self.control_tx {
                    let _ = tx.send(ControlMessage::SelectOutput(id));
                }
                // Optimistically reflect the choice so the picker highlights it
                // immediately; the server confirms with a SelectOutput echo.
                self.shared_input.set_selected_output(id);
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

        self.register_overlay_rect(resp.response.rect);
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

#[allow(clippy::too_many_arguments)]
fn start_media_threads(
    socket_addr: std::net::SocketAddr,
    frame_buf: Arc<Mutex<VideoFrameBuffer>>,
    debug_state: Arc<ConnectionDebugState>,
    debug_enabled: Arc<AtomicBool>,
    video_arrival: Arc<AtomicU64>,
    ctx: egui::Context,
    display_refresh_millihz: Option<u32>,
    audio_enabled: Arc<AtomicBool>,
    native_surfaces: Arc<NativeSurfaceControl>,
    control_tx: crossbeam_channel::Sender<ControlMessage>,
    input_rx: crossbeam_channel::Receiver<InputPacket>,
    shared_input: Arc<SharedInputState>,
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
            shared_input,
        );
    });

    let (audio_data_tx, audio_data_rx) = crossbeam_channel::bounded::<AudioPacket>(16);
    let audio_drop_rx = audio_data_rx.clone();
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
            audio_drop_rx,
            feedback_tx,
            decode_started_tx,
            video_arrival,
            pipeline_audio_flag,
            native_surfaces,
            control_tx,
            present_refresh_millihz,
            stream_config,
            receiver,
        );
    });

    let (audio_stop_tx, audio_stop_rx) = crossbeam_channel::bounded(1);
    let audio_packet_duration_ms = stream_config.packet_duration_ms as u32;
    let audio_thread = std::thread::spawn(move || {
        if let Err(e) = audio::run_audio_pipeline(
            audio_data_rx,
            audio_stop_rx,
            audio_packet_duration_ms,
            audio_enabled,
        ) {
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

#[allow(clippy::too_many_arguments)]
fn start_punched_media_threads(
    frame_buf: Arc<Mutex<VideoFrameBuffer>>,
    debug_state: Arc<ConnectionDebugState>,
    debug_enabled: Arc<AtomicBool>,
    video_arrival: Arc<AtomicU64>,
    ctx: egui::Context,
    display_refresh_millihz: Option<u32>,
    audio_enabled: Arc<AtomicBool>,
    native_surfaces: Arc<NativeSurfaceControl>,
    control_tx: crossbeam_channel::Sender<ControlMessage>,
    stream_config: StreamConfig,
    media_packet_rx: crossbeam_channel::Receiver<Vec<u8>>,
) -> MediaThreads {
    let receiver = MediaReceiver::from_packet_channel(media_packet_rx);
    let (audio_data_tx, audio_data_rx) = crossbeam_channel::bounded::<AudioPacket>(16);
    let audio_drop_rx = audio_data_rx.clone();
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
            audio_drop_rx,
            feedback_tx,
            decode_started_tx,
            video_arrival,
            pipeline_audio_flag,
            native_surfaces,
            control_tx,
            present_refresh_millihz,
            stream_config,
            receiver,
        );
    });

    let (audio_stop_tx, audio_stop_rx) = crossbeam_channel::bounded(1);
    let audio_packet_duration_ms = stream_config.packet_duration_ms as u32;
    let audio_thread = std::thread::spawn(move || {
        if let Err(e) = audio::run_audio_pipeline(
            audio_data_rx,
            audio_stop_rx,
            audio_packet_duration_ms,
            audio_enabled,
        ) {
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

/// B1: once decoding has started, this long without a new video unit (while the
/// TCP control link is still alive) means the UDP media path died silently — a
/// wifi switch / NAT rebind that broke the server's return route — so we drop
/// into the auto-reconnect path instead of freezing on the last frame. Longer
/// than a normal encoder rebuild / keyframe-recovery gap, short enough to feel
/// responsive (the server streams continuously while a subscriber is attached).
const MEDIA_STALL_TIMEOUT: Duration = Duration::from_secs(4);

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

/// `ST_FORCE_TCP=1` skips UDP media entirely and tunnels everything over TCP
/// from the first connect (useful behind proxies / UDP-blocking firewalls).
fn force_tcp_env() -> bool {
    matches!(
        std::env::var("ST_FORCE_TCP").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Fallback connection chain when the direct TCP path is unusable: UDP hole
/// punch first, then the API server's TCP relay. Reached both when `connect()`
/// itself fails AND when it "succeeded" but the auth handshake got no reply —
/// locked-down networks put transparent proxies in the path that fake-accept
/// every TCP SYN and then black-hole the bytes, so a successful `connect()`
/// alone proves nothing about reachability. Returns `Ok(())` once a fallback
/// session ran (the session manages its own state transitions), or
/// `Err(description)` when neither path could establish.
#[allow(clippy::too_many_arguments)]
fn run_fallback_chain(
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
) -> Result<(), String> {
    let mut punch_failure: String;
    match api_client::prepare_punch_attempt(api_discovery.as_ref()) {
        Ok((partner_cands, punched_crypto)) => {
            eprintln!("[connect] attempting UDP hole punch...");
            let punched = run_punched_session(
                partner_cands,
                punched_crypto,
                token.clone(),
                display_refresh_millihz,
                video_codec_support,
                Arc::clone(&excluded_video_codecs),
                Arc::clone(&state),
                Arc::clone(&frame_buf),
                Arc::clone(&debug_state),
                Arc::clone(&disconnect),
                Arc::clone(&connection_epoch),
                session_epoch,
                Arc::clone(&audio_enabled),
                Arc::clone(&debug_enabled),
                Arc::clone(&native_surfaces),
                Arc::clone(&shared_input),
                control_rx.clone(),
                input_rx.clone(),
                ctx.clone(),
                Arc::clone(&api_discovery),
                ft_shared_state.clone(),
            );
            if punched {
                return Ok(());
            }
            let terminal = match &*state.lock().unwrap() {
                ConnectionState::Error(message) => connection_error_is_terminal(message),
                _ => false,
            };
            if terminal {
                // A real server explicitly rejected authentication/startup.
                // Changing transport cannot make that request valid.
                return Ok(());
            }
            punch_failure = "UDP hole punch failed".into();
        }
        Err(punch_err) => {
            punch_failure = format!("Hole punch setup failed: {punch_err}");
        }
    }
    // Last resort: end-to-end encrypted TCP tunnel through the API server's
    // relay (works when UDP is blocked entirely).
    eprintln!("[connect] {punch_failure}; attempting TCP relay fallback...");
    if run_relay_tunnel_session(
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
    ) {
        return Ok(());
    }
    punch_failure.push_str(". TCP relay fallback failed");
    Err(punch_failure)
}

#[allow(clippy::too_many_arguments)]
fn run_connection(
    addr: String,
    token: String,
    display_refresh_millihz: Option<u32>,
    video_codec_support: decode::VideoCodecSupportReport,
    excluded_video_codecs: Arc<Mutex<st_protocol::VideoCodecSupport>>,
    force_tcp_media: Arc<Mutex<std::collections::HashSet<String>>>,
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

    // Try direct TCP first. If it fails and we have tunnel state, fall back to
    // hole punch, then to the API server's TCP relay.
    let force_tcp = force_tcp_media.lock().unwrap().contains(&addr) || force_tcp_env();
    let tcp_timeout = if punch_fallback_available { 3 } else { 5 };
    let tcp_result = TcpStream::connect_timeout(&socket_addr, Duration::from_secs(tcp_timeout));

    let mut tcp = match tcp_result {
        Ok(s) => {
            if force_tcp {
                // A previous session saw a live control link but zero UDP
                // media (or ST_FORCE_TCP is set): run the whole session over
                // this TCP connection using tunnel framing.
                eprintln!(
                    "[connect] TCP media transport active for {socket_addr} (UDP blocked or forced)"
                );
                let established = run_direct_tcp_tunnel_session(
                    s,
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
                    ft_shared_state,
                );
                if !established && !force_tcp_env() {
                    // The tunnel never reached Connected — drop this server's
                    // sticky flag so the next attempt retries the UDP path.
                    eprintln!("[connect] TCP tunnel did not establish; re-enabling UDP path");
                    force_tcp_media.lock().unwrap().remove(&addr);
                }
                return;
            }
            s
        }
        Err(tcp_err) => {
            if allow_hole_punch_fallback(socket_addr) && punch_fallback_available {
                eprintln!(
                    "[connect] Direct TCP to {socket_addr} failed ({tcp_err}); trying punch/relay fallback..."
                );
                if let Err(failure) = run_fallback_chain(
                    token,
                    display_refresh_millihz,
                    video_codec_support,
                    excluded_video_codecs,
                    Arc::clone(&state),
                    frame_buf,
                    debug_state,
                    Arc::clone(&disconnect),
                    Arc::clone(&connection_epoch),
                    session_epoch,
                    audio_enabled,
                    debug_enabled,
                    native_surfaces,
                    shared_input,
                    control_rx,
                    input_rx,
                    ctx.clone(),
                    api_discovery,
                    ft_shared_state,
                ) {
                    set_error(
                        &state,
                        &ctx,
                        &disconnect,
                        &connection_epoch,
                        session_epoch,
                        format!("Connection failed: {tcp_err}. {failure}."),
                    );
                }
                return;
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
    // Detect a silently-dead server (machine/network vanished without a TCP
    // RST) in ~11s instead of the OS default of ~2h, so the control read errors
    // out and the session tears down into the auto-reconnect path. Without this,
    // a frozen frame could sit on screen indefinitely.
    configure_tcp_keepalive(&tcp);

    // B1: media-stall watchdog counter, bumped by the receive pipeline on every
    // video unit. The control loop reconnects if it stops advancing while TCP
    // is still up (UDP media path silently died — wifi switch / NAT rebind).
    let video_arrival = Arc::new(AtomicU64::new(0));

    // --- Authentication handshake ---
    let _ = tcp.write_all(&ControlMessage::Authenticate(token.clone()).serialize());
    tcp.set_read_timeout(Some(Duration::from_secs(5))).ok();
    // `None` = authenticated. `Some(reason)` = the socket connected but no
    // genuine st-server ever spoke back (EOF, read error, or silence). That is
    // indistinguishable from a transparent proxy fake-accepting the SYN on a
    // locked-down network, so it is treated like a failed connect: fall through
    // to the punch → relay chain instead of erroring out. An explicit
    // AuthResult(false)/Error from a real server still returns terminally.
    let auth_dead: Option<String> = {
        let mut auth_buf = vec![0u8; 64];
        let mut pending = Vec::new();
        let auth_deadline = Instant::now() + Duration::from_secs(5);
        let mut dead = Some("Authentication timed out.".to_string());
        'auth: while Instant::now() < auth_deadline {
            match tcp.read(&mut auth_buf) {
                Ok(0) => {
                    dead = Some("Server closed the connection during authentication.".to_string());
                    break 'auth;
                }
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
                                dead = None;
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
                    dead = Some(format!("Auth read error: {e}"));
                    break 'auth;
                }
            }
        }
        dead
    };
    if let Some(reason) = auth_dead {
        if allow_hole_punch_fallback(socket_addr) && punch_fallback_available {
            eprintln!(
                "[connect] TCP to {socket_addr} accepted but handshake dead ({reason}); \
                 trying punch/relay fallback..."
            );
            drop(tcp);
            if let Err(failure) = run_fallback_chain(
                token,
                display_refresh_millihz,
                video_codec_support,
                excluded_video_codecs,
                Arc::clone(&state),
                frame_buf,
                debug_state,
                Arc::clone(&disconnect),
                Arc::clone(&connection_epoch),
                session_epoch,
                audio_enabled,
                debug_enabled,
                native_surfaces,
                shared_input,
                control_rx,
                input_rx,
                ctx.clone(),
                api_discovery,
                ft_shared_state,
            ) {
                set_error(
                    &state,
                    &ctx,
                    &disconnect,
                    &connection_epoch,
                    session_epoch,
                    format!("Connection failed: {reason} {failure}."),
                );
            }
            return;
        }
        set_error(
            &state,
            &ctx,
            &disconnect,
            &connection_epoch,
            session_epoch,
            reason,
        );
        return;
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
            hdr_display: client_hdr_display_supported(),
        })
        .serialize(),
    );
    if let Some(max_kbps) = client_bitrate_preference_kbps() {
        let _ = tcp.write_all(&ControlMessage::ClientBitratePreference(max_kbps).serialize());
    }

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
                                    Arc::clone(&video_arrival),
                                    ctx.clone(),
                                    display_refresh_millihz,
                                    Arc::clone(&audio_enabled),
                                    Arc::clone(&native_surfaces),
                                    pipeline_control_tx.clone(),
                                    input_rx.take().expect("input receiver already taken"),
                                    Arc::clone(&shared_input),
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
                        ControlMessage::ClockSyncPong(pong)
                            if debug_enabled.load(Ordering::Relaxed) =>
                        {
                            debug_state.update_clock_sync(pong, unix_time_micros());
                        }
                        ControlMessage::ClockSyncPong(_) => {}
                        ControlMessage::InputSession(session) => {
                            shared_input.set_input_session(session);
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
                        ControlMessage::AvailableOutputs(outputs) => {
                            shared_input.set_available_outputs(outputs);
                            ctx.request_repaint();
                        }
                        ControlMessage::SelectOutput(id) => {
                            shared_input.set_selected_output(id);
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
    // B1: last clock-sync RTT (ms) stamped onto outgoing TransportFeedback.
    let mut last_rtt_ms: u32 = 0;
    let mut startup_decode_ok = false;
    let startup_deadline = Instant::now() + STARTUP_DECODE_TIMEOUT;
    // B1: media-stall watchdog state. `video_arrival` counts every media
    // datagram (video, audio, AND keepalive — see run_receive_pipeline), so it
    // doubles as the UDP-liveness signal for the UDP-blocked detector below.
    let mut last_video_count = video_arrival.load(Ordering::Relaxed);
    let mut last_video_at = Instant::now();
    loop {
        if session_cancelled(
            disconnect.as_ref(),
            connection_epoch.as_ref(),
            session_epoch,
        ) {
            break;
        }

        // B1: reconnect if video stopped arriving while TCP is still alive.
        // Breaking here drops through to the cleanup below, which marks the
        // session Error("Connection lost") → auto-reconnect (re-resolves the
        // peer, preferring LAN), binding a fresh UDP socket on the new path.
        let video_count = video_arrival.load(Ordering::Relaxed);
        if video_count != last_video_count {
            last_video_count = video_count;
            last_video_at = Instant::now();
        } else if startup_decode_ok && last_video_at.elapsed() > MEDIA_STALL_TIMEOUT {
            eprintln!(
                "[media] no video for {}s while TCP alive — media path dead, reconnecting",
                MEDIA_STALL_TIMEOUT.as_secs()
            );
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
        while let Ok(mut feedback) = feedback_rx.try_recv() {
            // B1: stamp the latest clock-sync RTT so the wire field is live
            // (telemetry; the server-side CC consumer is a separate step).
            feedback.rtt_ms = last_rtt_ms;
            if tcp
                .write_all(&ControlMessage::TransportFeedback(feedback).serialize())
                .is_err()
            {
                break;
            }
        }
        if !startup_decode_ok && Instant::now() >= startup_deadline {
            if video_arrival.load(Ordering::Relaxed) == 0 {
                // TCP control is alive but not a single UDP datagram (video,
                // audio, or keepalive) arrived: the media path is blocked, not
                // the codec. Flip THIS server to TCP media transport and
                // reconnect (non-terminal error → auto-reconnect picks it up
                // with the per-server force-TCP flag set).
                eprintln!(
                    "[transport] no UDP media within {}s while TCP control is alive — \
                     switching to TCP media transport and reconnecting",
                    STARTUP_DECODE_TIMEOUT.as_secs(),
                );
                force_tcp_media.lock().unwrap().insert(addr.clone());
                clipboard_sync.stop();
                ft_manager.stop();
                stop_media_threads(media_threads);
                set_error(
                    &state,
                    &ctx,
                    &disconnect,
                    &connection_epoch,
                    session_epoch,
                    "UDP media blocked — retrying over TCP".into(),
                );
                return;
            }
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
        // Clock-sync ping runs always (not only with the debug overlay) so the
        // RTT stamped onto TransportFeedback (B1) stays fresh — 16 B every 2 s.
        if Instant::now() >= next_clock_ping {
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
                            // B1: always derive RTT for the feedback field; the
                            // overlay update stays gated on the debug toggle.
                            let now = unix_time_micros();
                            last_rtt_ms = (clock_sync_rtt_micros(
                                pong.client_send_micros as i128,
                                pong.server_recv_micros as i128,
                                pong.server_send_micros as i128,
                                now as i128,
                            ) / 1000)
                                .min(u32::MAX as i64)
                                as u32;
                            if current_debug_enabled {
                                debug_state.update_clock_sync(pong, now);
                            }
                        }
                        ControlMessage::InputSession(session) => {
                            shared_input.set_input_session(session);
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
                        ControlMessage::AvailableOutputs(outputs) => {
                            shared_input.set_available_outputs(outputs);
                            ctx.request_repaint();
                        }
                        ControlMessage::SelectOutput(id) => {
                            shared_input.set_selected_output(id);
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
            // Reaching here with a live (Connected/Connecting) state and a still-
            // current session means the connection dropped unexpectedly (the
            // user-initiated path bumps the epoch and returns above). Mark it as
            // a retriable error so the UI auto-reconnects instead of silently
            // dropping to the home screen with a frozen frame.
            *s = ConnectionState::Error("Connection lost".into());
        }
    }
    ctx.request_repaint();
}

/// Run a full streaming session over a hole-punched UDP socket.
/// Called when direct TCP connection fails but tunnel crypto + partner candidates are available.
///
/// Returns `true` when the hole punch succeeded and a session ran (however it
/// ended); `false` when punching itself failed, so the caller can fall back to
/// the TCP relay.
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
) -> bool {
    use st_protocol::reliable_udp::PunchedSocket;

    // Clone the process-lifetime punch socket from API discovery.
    let socket = match api_discovery.clone_punch_socket() {
        Ok(socket) => socket,
        Err(e) => {
            eprintln!("[punch] Failed to prepare punch socket: {e}");
            return false;
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
            eprintln!("[punch] Hole punch failed: {e}");
            return false;
        }
    };
    eprintln!("[punch] Success! Server confirmed at {peer}");

    let punched: Arc<dyn st_protocol::tcp_tunnel::TunnelLink> =
        Arc::new(PunchedSocket::new(socket, peer, crypto));
    // The punch-session guard stays alive for the whole session (the session
    // shares the process-lifetime punch socket with the STUN refresher).
    //
    // Return whether the session actually reached Connected. The punch
    // handshake passing does NOT mean the session works — a NAT can pass the
    // tiny STPUNCH probe yet block/throttle sustained UDP (PMTU blackhole), in
    // which case the session auth-times-out. Propagating that lets the caller
    // fall through to the TCP relay instead of treating the dead punched path
    // as a successful connection.
    run_tunnel_session(
        punched,
        "punch",
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
        ft_shared_state,
    )
}

/// Run a full streaming session over any tunnel link: hole-punched UDP,
/// direct TCP tunnel, or relayed TCP tunnel. All control and media traffic
/// flows through the single link; media packets are bridged into the normal
/// receive pipeline through a channel.
///
/// Returns `true` once the session reached the Connected state.
#[allow(clippy::too_many_arguments)]
fn run_tunnel_session(
    punched: Arc<dyn st_protocol::tcp_tunnel::TunnelLink>,
    via: &'static str,
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
    ft_shared_state: file_transfer::SharedTransferState,
) -> bool {
    use st_protocol::reliable_udp::PunchedMessage;
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
        return false;
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut authenticated = false;
    while Instant::now() < deadline {
        punched.tick();
        if let Some(PunchedMessage::Control(data)) = punched.try_recv() {
            let mut offset = 0;
            while let Some((message, used)) = ControlMessage::deserialize(&data[offset..]) {
                offset += used;
                match message {
                    ControlMessage::AuthResult(true) => authenticated = true,
                    ControlMessage::AuthResult(false) => {
                        set_error(
                            &state,
                            &ctx,
                            &disconnect,
                            &connection_epoch,
                            session_epoch,
                            "Authentication failed. Check your token.".into(),
                        );
                        return false;
                    }
                    ControlMessage::Error(error) => {
                        set_error(
                            &state,
                            &ctx,
                            &disconnect,
                            &connection_epoch,
                            session_epoch,
                            format!("Server error: {error}"),
                        );
                        return false;
                    }
                    _ => {}
                }
            }
            if authenticated {
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
            "Authentication timeout over tunnel connection".into(),
        );
        return false;
    }
    eprintln!("[{via}] Authenticated");

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
        hdr_display: client_hdr_display_supported(),
    };
    let _ = punched.send_control(&ControlMessage::ClientDisplayInfo(display_info).serialize());
    if let Some(max_kbps) = client_bitrate_preference_kbps() {
        let _ =
            punched.send_control(&ControlMessage::ClientBitratePreference(max_kbps).serialize());
    }

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
                        shared_input.set_input_session(session);
                    }
                    ControlMessage::InputCapabilities(caps) => {
                        shared_input.set_capabilities(caps);
                    }
                    ControlMessage::ControllerState(cs) => {
                        shared_input.set_controller_state(cs);
                    }
                    ControlMessage::AvailableOutputs(outputs) => {
                        shared_input.set_available_outputs(outputs);
                    }
                    ControlMessage::SelectOutput(id) => {
                        shared_input.set_selected_output(id);
                    }
                    ControlMessage::Error(error) => {
                        set_error(
                            &state,
                            &ctx,
                            &disconnect,
                            &connection_epoch,
                            session_epoch,
                            format!("Server error: {error}"),
                        );
                        return false;
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
                        return false;
                    }
                    _ => {}
                }
            }
        }
        if stream_config.is_some() && shared_input.snapshot().input_credential.is_some() {
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
            return false;
        }
    };

    // --- Send ClientReadyForMedia ---
    let _ = punched.send_control(&ControlMessage::ClientReadyForMedia.serialize());

    // Wait for StreamStarted.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream_started = false;
    while Instant::now() < deadline {
        punched.tick();
        if let Some(PunchedMessage::Control(data)) = punched.try_recv() {
            let mut offset = 0;
            while let Some((message, used)) = ControlMessage::deserialize(&data[offset..]) {
                offset += used;
                match message {
                    ControlMessage::StreamStarted => stream_started = true,
                    ControlMessage::Error(error) => {
                        set_error(
                            &state,
                            &ctx,
                            &disconnect,
                            &connection_epoch,
                            session_epoch,
                            format!("Server error: {error}"),
                        );
                        return false;
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
                        return false;
                    }
                    _ => {}
                }
            }
            if stream_started {
                break;
            }
        }
    }
    if !stream_started {
        set_error(
            &state,
            &ctx,
            &disconnect,
            &connection_epoch,
            session_epoch,
            "Stream startup timed out over tunnel connection".into(),
        );
        return false;
    }

    eprintln!("[{via}] Stream started, entering unified session loop");
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
    // Punched sessions detect a dead media path via last_peer_activity below
    // (PUNCHED_INACTIVITY_TIMEOUT), so this video_arrival counter isn't watched
    // here — it's only required by start_punched_media_threads' signature.
    let video_arrival = Arc::new(AtomicU64::new(0));
    let media = start_punched_media_threads(
        Arc::clone(&frame_buf),
        Arc::clone(&debug_state),
        Arc::clone(&debug_enabled),
        video_arrival,
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
    // B1: last clock-sync RTT (ms) stamped onto outgoing TransportFeedback.
    let mut last_rtt_ms: u32 = 0;
    let mut input_seq: u16 = 0;
    let mut mouse_heartbeat = MouseInputHeartbeat::default();
    let mut keyboard_heartbeat = KeyboardInputHeartbeat::default();
    let (clipboard_control_tx, clipboard_control_rx) =
        crossbeam_channel::bounded::<ControlMessage>(8);
    let (file_detect_tx, file_detect_rx) = crossbeam_channel::bounded::<std::path::PathBuf>(8);
    let suppressed_paths = clipboard::new_suppressed_paths();
    let mut clipboard_sync = clipboard::ClipboardSync::start_with_file_detection(
        "client-punched",
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
        if punched.is_closed() {
            eprintln!("[{via}] tunnel closed by peer");
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
            if let Some(credential) = shared_input.snapshot().input_credential {
                let serialized = input_pkt.serialize(input_seq, credential);
                input_seq = input_seq.wrapping_add(1);
                let _ = punched.send_media(&serialized);
            }
            mouse_heartbeat.mark_sent(input_pkt, now);
            keyboard_heartbeat.mark_sent(input_pkt, now);
        }
        // Heartbeat retransmission for button/key state repair over lossy UDP.
        if let Some(pkt) = mouse_heartbeat.due_packet(now) {
            did_work = true;
            if let Some(credential) = shared_input.snapshot().input_credential {
                let serialized = pkt.serialize(input_seq, credential);
                input_seq = input_seq.wrapping_add(1);
                let _ = punched.send_media(&serialized);
            }
        }
        if let Some(pkt) = keyboard_heartbeat.due_packet(now) {
            did_work = true;
            if let Some(credential) = shared_input.snapshot().input_credential {
                let serialized = pkt.serialize(input_seq, credential);
                input_seq = input_seq.wrapping_add(1);
                let _ = punched.send_media(&serialized);
            }
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
        while let Ok(mut fb) = media.feedback_rx.try_recv() {
            did_work = true;
            // B1: stamp the latest clock-sync RTT onto the wire field.
            fb.rtt_ms = last_rtt_ms;
            let _ = punched.send_control(&ControlMessage::TransportFeedback(fb).serialize());
        }

        // Forward pipeline control messages (keyframe requests, etc.)
        while let Ok(ctrl) = pipeline_control_rx.try_recv() {
            did_work = true;
            let _ = punched.send_control(&ctrl.serialize());
        }
        // Clock-sync ping runs always (B1) so RTT on feedback stays fresh.
        if Instant::now() >= next_clock_ping {
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
                                    // B1: always derive RTT for the feedback
                                    // field; overlay update stays debug-gated.
                                    let now = unix_time_micros();
                                    last_rtt_ms = (clock_sync_rtt_micros(
                                        pong.client_send_micros as i128,
                                        pong.server_recv_micros as i128,
                                        pong.server_send_micros as i128,
                                        now as i128,
                                    ) / 1000)
                                        .min(u32::MAX as i64)
                                        as u32;
                                    if current_debug_enabled {
                                        debug_state.update_clock_sync(pong, now);
                                    }
                                }
                                ControlMessage::InputSession(session) => {
                                    shared_input.set_input_session(session);
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
                                ControlMessage::AvailableOutputs(outputs) => {
                                    shared_input.set_available_outputs(outputs);
                                    ctx.request_repaint();
                                }
                                ControlMessage::SelectOutput(id) => {
                                    shared_input.set_selected_output(id);
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
                "[codec] no frames decoded within {}s over tunnel transport — excluding {:?} and reconnecting",
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
            // Unexpected loss (not a user-initiated teardown, which bumps the
            // epoch / sets the disconnect flag) → retriable error so the UI
            // auto-reconnects.
            if session_is_current(connection_epoch.as_ref(), session_epoch)
                && !disconnect.load(Ordering::SeqCst)
            {
                *s = ConnectionState::Error("Connection lost".into());
            } else {
                *s = ConnectionState::Disconnected;
            }
        }
    }
    ctx.request_repaint();
    true
}

/// Run a whole session (control + media) over one direct TCP connection using
/// tunnel framing. Used when UDP media is blocked (firewall/proxy) but the
/// server's control port is reachable, or when `ST_FORCE_TCP` is set. The
/// preamble tells the server to switch this connection into tunnel mode.
///
/// Returns `true` once the session reached the Connected state.
#[allow(clippy::too_many_arguments)]
fn run_direct_tcp_tunnel_session(
    mut stream: TcpStream,
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
    ft_shared_state: file_transfer::SharedTransferState,
) -> bool {
    let _ = stream.set_nodelay(true);
    configure_tcp_keepalive(&stream);
    if let Err(e) = stream.write_all(st_protocol::tcp_tunnel::TCP_TUNNEL_PREAMBLE) {
        set_error(
            &state,
            &ctx,
            &disconnect,
            &connection_epoch,
            session_epoch,
            format!("TCP tunnel handshake failed: {e}"),
        );
        return false;
    }
    let tunnel = match st_protocol::tcp_tunnel::TcpTunnel::new(stream, None, Vec::new()) {
        Ok(t) => t,
        Err(e) => {
            set_error(
                &state,
                &ctx,
                &disconnect,
                &connection_epoch,
                session_epoch,
                format!("TCP tunnel setup failed: {e}"),
            );
            return false;
        }
    };
    run_tunnel_session(
        Arc::new(tunnel),
        "tcp",
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
        ft_shared_state,
    )
}

/// Last-resort fallback: run the session through the API server's TCP relay,
/// end-to-end encrypted with the X25519-derived key (the relay only ever sees
/// ciphertext). Used when direct TCP and UDP hole punching both failed —
/// typically when UDP is blocked entirely.
///
/// Returns `true` when a session ran (pairing + tunnel setup succeeded);
/// `false` when the relay attempt failed and the caller should report the
/// connection as failed.
#[allow(clippy::too_many_arguments)]
fn run_relay_tunnel_session(
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
) -> bool {
    {
        let mut s = state.lock().unwrap();
        *s = ConnectionState::Connecting;
    }
    ctx.request_repaint();

    let (crypto, relay_addr, relay_ticket) =
        match api_client::prepare_relay_attempt(api_discovery.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[relay] Relay setup failed: {e}");
                return false;
            }
        };
    eprintln!("[relay] Dialing TCP relay {relay_addr}...");
    let stream = match api_client::connect_relay(&relay_addr, &relay_ticket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[relay] {e}");
            return false;
        }
    };
    eprintln!("[relay] Paired with host via relay");
    let tunnel = match st_protocol::tcp_tunnel::TcpTunnel::new(stream, Some(crypto), Vec::new()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[relay] Tunnel setup failed: {e}");
            return false;
        }
    };
    run_tunnel_session(
        Arc::new(tunnel),
        "relay",
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
        ft_shared_state,
    );
    true
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

/// A terminal error is one where reconnecting would just fail again the same
/// way, so auto-reconnect must NOT retry it: the server explicitly rejected us
/// (bad token / server-side error). Everything else — network drops, timeouts,
/// "server unreachable", "server shut down" (restart/auto-update) — is transient
/// and worth retrying. Matched on the stable message prefixes produced by the
/// `set_error` call sites.
fn connection_error_is_terminal(msg: &str) -> bool {
    msg.starts_with("Authentication failed")
        || msg.starts_with("Server error:")
        // B8b: a GL upload failure is a local rendering/driver fault — a network
        // reconnect can't fix it, so don't spin the auto-reconnect loop forever;
        // surface it to the user instead.
        || msg.starts_with("GL upload failed")
}

/// Exponential backoff for auto-reconnect: 0.5s, 1s, 2s, 4s, capped at 8s. The
/// first retry is near-immediate so a brief blip recovers fast; the cap keeps a
/// genuinely-down server from being hammered while still recovering promptly
/// once it returns.
fn reconnect_backoff(attempts: u32) -> Duration {
    let secs = match attempts {
        0 => return Duration::from_millis(500),
        1 => 1,
        2 => 2,
        3 => 4,
        _ => 8,
    };
    Duration::from_secs(secs)
}

fn session_cancelled(
    disconnect: &AtomicBool,
    connection_epoch: &AtomicU64,
    session_epoch: u64,
) -> bool {
    disconnect.load(Ordering::SeqCst) || !session_is_current(connection_epoch, session_epoch)
}

/// Enable aggressive TCP keepalive on the control socket so a dead peer is
/// detected in ~11s (idle 5s, then 3 probes 2s apart) rather than the OS
/// default (~2h on Linux). When the peer is truly gone the probes fail, the
/// blocking/timeout read returns an error, and the session tears down into the
/// auto-reconnect path. Best-effort: failures to set the options are ignored.
#[cfg(unix)]
fn configure_tcp_keepalive(tcp: &TcpStream) {
    use std::os::fd::AsRawFd;
    let fd = tcp.as_raw_fd();
    unsafe {
        let on: libc::c_int = 1;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        // TCP_KEEPIDLE/INTVL/CNT exist on Linux; macOS uses TCP_KEEPALIVE for
        // idle time. Set what each platform supports; ignore the rest.
        #[cfg(target_os = "linux")]
        {
            let idle: libc::c_int = 5;
            let intvl: libc::c_int = 2;
            let cnt: libc::c_int = 3;
            for (opt, val) in [
                (libc::TCP_KEEPIDLE, idle),
                (libc::TCP_KEEPINTVL, intvl),
                (libc::TCP_KEEPCNT, cnt),
            ] {
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_TCP,
                    opt,
                    &val as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
        #[cfg(target_os = "macos")]
        {
            // TCP_KEEPALIVE (idle seconds) — constant value 0x10 on Darwin.
            const TCP_KEEPALIVE: libc::c_int = 0x10;
            let idle: libc::c_int = 5;
            libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                TCP_KEEPALIVE,
                &idle as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

#[cfg(not(unix))]
fn configure_tcp_keepalive(_tcp: &TcpStream) {}

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
    credential: InputCredential,
    seq: &mut u16,
    crypto: Option<&st_protocol::tunnel::CryptoContext>,
) {
    let raw = packet.serialize(*seq, credential);
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
    shared_input: Arc<SharedInputState>,
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
                if let Some(credential) = shared_input.snapshot().input_credential {
                    send_input_packet_raw(&socket, target, packet, credential, &mut seq, cref);
                    mouse_heartbeat.mark_sent(packet, now);
                    keyboard_heartbeat.mark_sent(packet, now);
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                if let Some(credential) = shared_input.snapshot().input_credential {
                    if let Some(packet) = mouse_heartbeat.due_packet(now) {
                        send_input_packet_raw(&socket, target, packet, credential, &mut seq, cref);
                    }
                    if let Some(packet) = keyboard_heartbeat.due_packet(now) {
                        send_input_packet_raw(&socket, target, packet, credential, &mut seq, cref);
                    }
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

/// Client-declared max bitrate ceiling in kbps (B4), from `ST_CLIENT_MAX_BITRATE`.
/// Sent to the server right after the display info so the ABR prober is seeded
/// with the client's known link ceiling instead of probing up blindly. Unset =
/// no client-side cap (server uses its own max).
fn client_bitrate_preference_kbps() -> Option<u32> {
    std::env::var("ST_CLIENT_MAX_BITRATE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|kbps| *kbps > 0)
}

/// Whether this client can correctly present an HDR (BT.2020 + PQ) stream (D2).
/// The server AND-gates HDR on this flag, so we must only advertise it once the
/// client genuinely has a 10-bit decode + tone-map/HDR render path. That path
/// (D3) is not implemented yet, so this is `false` for now — `ST_HDR_DISPLAY=1`
/// is an opt-in override for testing. Advertising `true` prematurely would get a
/// washed-out image with no SDR fallback.
fn client_hdr_display_supported() -> bool {
    matches!(
        std::env::var("ST_HDR_DISPLAY").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
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
    // Prefer the local OS pointer position so the rendered overlay does not
    // teleport to the server cursor on Idle→CapturedRelative entry.  The
    // server is realigned to local via a one-shot warp MouseRelative packet
    // sent from the entry transition (see send_relative_warp_to_local).
    local_pos.or(remote_pos)
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

/// The server is signalling it wants relative (mouselook) input: either it hid
/// the cursor, or the server-side warp detector flagged the remote app grabbing
/// the pointer *without* hiding the cursor (`app_grab` — many XWayland/Proton
/// FPS titles warp/park the pointer instead of hiding it). Both feed the
/// hidden-frame counters, so this also gates the relative→desktop *exit*: while
/// the server wants relative, `cursor_shown_frames` never climbs.
fn server_wants_relative(snapshot: &input::SharedInputSnapshot) -> bool {
    !snapshot.cursor_state.visible || snapshot.cursor_state.app_grab
}

/// A real, positionable remote cursor the client can draw. False during
/// mouselook (cursor hidden, or `app_grab` where the server position is parked
/// and meaningless), so the overlay is suppressed and raw deltas flow.
fn server_cursor_drawable(snapshot: &input::SharedInputSnapshot) -> bool {
    snapshot.cursor_state.visible && !snapshot.cursor_state.app_grab
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

/// Format an optional millisecond latency stage, dashing out missing samples
/// (e.g. before clock sync lands, or when a clock skew produced a negative diff).
fn fmt_ms(v: Option<f32>) -> String {
    v.map(|x| format!("{x:.1}"))
        .unwrap_or_else(|| "-".to_string())
}

fn build_debug_general_lines(
    snapshot: &ConnectionDebugSnapshot,
    input_snapshot: &input::SharedInputSnapshot,
    capture_mode: LocalCaptureMode,
    audio_enabled: bool,
    pointer_buttons: u8,
    pressed_keys: usize,
) -> Vec<String> {
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

    vec![
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
        format!(
            "net: {:.0} kbps (video {:.0} / audio {:.0})  target={}",
            snapshot.received_total_kbps,
            snapshot.received_video_kbps,
            snapshot.received_audio_kbps,
            snapshot
                .target_bitrate_kbps
                .map(|b| format!("{b} kbps"))
                .unwrap_or_else(|| "-".to_string()),
        ),
        format!(
            "fps: recv {:.1}  decode {:.1}  present {:.1}",
            snapshot.receive_fps, snapshot.decode_fps, snapshot.present_fps,
        ),
        format!(
            "packets: rx={} lost={} late={} dropped_frames={} playout_drops={} jitter_buf={:.0}ms  loss={:.2}%",
            snapshot.received_packets,
            snapshot.lost_packets,
            snapshot.late_packets,
            snapshot.dropped_frames,
            snapshot.playout_drops,
            snapshot.jitter_delay_ms,
            loss_percent(
                snapshot.received_packets,
                snapshot.lost_packets,
                snapshot.dropped_frames,
                snapshot.completed_frames,
            ),
        ),
        format!(
            "latency ms: cap\u{2192}send {} | send\u{2192}asm {} | asm\u{2192}dec {} | decode {} | dec\u{2192}present {} | total {}",
            fmt_ms(snapshot.capture_to_send_ms),
            fmt_ms(snapshot.send_to_assemble_ms),
            fmt_ms(snapshot.assemble_to_decode_ms),
            fmt_ms(snapshot.decode_work_ms),
            fmt_ms(snapshot.decode_to_present_ms),
            fmt_ms(snapshot.total_latency_ms),
        ),
        format!(
            "latency total ms: avg {} | p95 {} | max {}  (last 3s)",
            fmt_ms(snapshot.total_latency_ms),
            fmt_ms(snapshot.latency_p95_ms),
            fmt_ms(snapshot.latency_max_ms),
        ),
        format!(
            "clock: rtt={} ms  server_ahead={} ms  fb_window={} ms",
            fmt_ms(snapshot.clock_rtt_ms),
            fmt_ms(snapshot.server_clock_ahead_ms),
            snapshot.transport_interval_ms,
        ),
    ]
}

fn render_debug_overlay(
    ctx: &egui::Context,
    general_lines: &[String],
    cursor_lines: &[String],
    top_offset: f32,
    debug_tab: &mut DebugOverlayTab,
) -> egui::Rect {
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
                            DebugOverlayTab::General => general_lines,
                            DebugOverlayTab::Cursor => cursor_lines,
                        };
                        for line in lines {
                            ui.monospace(line);
                        }
                    });
                });
        })
        .response
        .rect
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
        // Auto-reconnect on unexpected connection loss (network drop, transient
        // unreachable, server restart/auto-update). Terminal rejections (bad
        // token / explicit server error) are excluded so we never hammer a
        // server that refused us. Backoff grows across consecutive failures and
        // resets once a live session is re-established.
        match &state {
            ConnectionState::Connected => {
                self.auto_reconnect_attempts = 0;
                self.next_reconnect_at = None;
                // Returned to the server's subnet? Switch off WAN onto LAN
                // without waiting for the WAN session to drop.
                self.maybe_upgrade_to_lan(ctx);
            }
            ConnectionState::Error(msg)
                if !connection_error_is_terminal(msg) && !self.server_addr.trim().is_empty() =>
            {
                let now = Instant::now();
                match self.next_reconnect_at {
                    None => {
                        let delay = reconnect_backoff(self.auto_reconnect_attempts);
                        self.next_reconnect_at = Some(now + delay);
                        ctx.request_repaint_after(delay);
                    }
                    Some(at) if now >= at => {
                        self.auto_reconnect_attempts =
                            self.auto_reconnect_attempts.saturating_add(1);
                        // Re-resolve the peer's live best path before retrying so
                        // a network switch promotes us onto whatever is reachable
                        // now (LAN beacon ↔ public/VPN API candidate) instead of
                        // hammering the address the session opened with — which a
                        // switch can leave permanently dead. Keep the last address
                        // when no live variant is known yet; backoff retries until
                        // one reappears.
                        if let Some(pid) = self.connect_peer_id.clone() {
                            if let Some(addr) = self.best_addr_for_peer(&pid) {
                                self.server_addr = addr;
                            }
                        }
                        self.connect(ctx.clone());
                        return;
                    }
                    Some(at) => {
                        // LAN reappeared mid-backoff — don't sit on the timer;
                        // retry now so we land on the low-latency LAN path
                        // immediately instead of after the full delay.
                        if self.pending_lan_upgrade().is_some() {
                            self.next_reconnect_at = Some(now);
                            ctx.request_repaint();
                        } else {
                            ctx.request_repaint_after(at.saturating_duration_since(now));
                        }
                    }
                }
            }
            _ => {}
        }
        self.keep_awake
            .set_active(matches!(state, ConnectionState::Connected));
        // Promote the overlay rects collected last frame into the active set and
        // begin a fresh pending set for this frame. This is what lets the HUD
        // hit-test cover every overlay regardless of the order they render in
        // (e.g. the debug/graph overlays draw after the cursor logic).
        self.local_overlay_hit_rects = std::mem::take(&mut self.pending_overlay_hit_rects);
        self.suppress_pointer_pos_frames = self.suppress_pointer_pos_frames.saturating_sub(1);
        self.suppress_mouse_delta = self.suppress_mouse_delta.saturating_sub(1);
        self.os_cursor_hide_settle = self.os_cursor_hide_settle.saturating_sub(1);
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
                    self.debug_state.record_present(
                        &self.upload_frame,
                        unix_time_micros(),
                        mono_micros(),
                    );
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
                            self.debug_state.record_present(
                                &self.upload_frame,
                                unix_time_micros(),
                                mono_micros(),
                            );
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
                // Retriable errors are being auto-reconnected: show a recovering
                // screen instead of a dead "Connection Failed" so a network blip
                // self-heals without the user touching anything. Terminal
                // rejections keep the explicit failure screen.
                let reconnecting =
                    !connection_error_is_terminal(msg) && !self.server_addr.trim().is_empty();
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    if reconnecting {
                        ui.spinner();
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new("Connection lost — reconnecting…")
                                .size(16.0)
                                .color(egui::Color32::from_rgb(230, 233, 240)),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(msg)
                                .size(12.0)
                                .color(egui::Color32::from_rgb(138, 142, 150)),
                        );
                        if self.auto_reconnect_attempts > 0 {
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "attempt {}",
                                    self.auto_reconnect_attempts + 1
                                ))
                                .size(11.0)
                                .color(egui::Color32::from_rgb(110, 114, 122)),
                            );
                        }
                    } else {
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
                    }
                    ui.add_space(20.0);
                    let button_label = if reconnecting { "Cancel" } else { "Back" };
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(button_label)
                                    .size(13.0)
                                    .color(egui::Color32::from_rgb(230, 233, 240)),
                            )
                            .fill(egui::Color32::from_rgb(52, 56, 66))
                            .corner_radius(4)
                            .min_size(egui::vec2(100.0, 34.0)),
                        )
                        .clicked()
                    {
                        // Cancel auto-reconnect and return home.
                        self.disconnect();
                        self.video_texture.clear_frame();
                        *self.state.lock().unwrap() = ConnectionState::Disconnected;
                    }
                });
            }
        });

        if state == ConnectionState::Connected && self.debug_enabled {
            // Graph: feed it the cheap Copy metrics view every frame (no String
            // clones). It self-throttles its own sampling to ~6Hz internally.
            let metrics = self.debug_state.metrics();
            self.graph_overlay.push(&metrics);
            // Register both debug overlays as HUD regions (same rule as the
            // menu) so the pointer over them keeps the OS cursor, not the
            // remote overlay. They render after the cursor logic, so the
            // double-buffer makes their rects available next frame.
            let graph_rect = self.graph_overlay.render(ctx);
            self.register_overlay_rect(graph_rect);

            let input_snapshot = self.shared_input.snapshot();
            let cursor_lines = self.cursor_debug_lines(ctx, &input_snapshot);

            // Text overlay: rebuilding the lines (and cloning the string-heavy
            // full snapshot) every frame is wasteful, so refresh at ~6Hz.
            if self.debug_lines_cache.is_empty()
                || self.debug_lines_built.elapsed() >= Duration::from_millis(150)
            {
                let snapshot = self.debug_state.snapshot();
                self.debug_lines_cache = build_debug_general_lines(
                    &snapshot,
                    &input_snapshot,
                    self.capture_mode,
                    self.audio_enabled,
                    self.pointer_buttons,
                    self.keyboard_state.pressed_count(),
                );
                self.debug_lines_built = Instant::now();
            }

            let debug_rect = render_debug_overlay(
                ctx,
                &self.debug_lines_cache,
                &cursor_lines,
                debug_top,
                &mut self.debug_overlay_tab,
            );
            self.register_overlay_rect(debug_rect);
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

        // Match handle_connected_video_response / compute_cursor_overlay_geometry:
        // they both source video_rect from current_video_rect first.  Using
        // raw_input.screen_rect here would diverge when egui has any top/bottom
        // panel margin, causing hover_cursor_pos to toggle Some↔None on
        // sub-pixel boundary positions and the overlay to flip between local
        // and server positions.
        let video_rect = self
            .current_video_rect(ctx)
            .or(self.last_video_rect)
            .or_else(|| {
                raw_input
                    .screen_rect
                    .and_then(|rect| self.video_rect_for_container(rect))
            });
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
                    if self.await_pointer_exit_after_auto_release
                        && video_rect.map(|rect| !rect.contains(pos)).unwrap_or(true)
                    {
                        self.await_pointer_exit_after_auto_release = false;
                    }
                    // Desktop (HoverAbsolute): the local pointer drives the
                    // cursor 1:1 and is forwarded as an absolute position. If it
                    // moves onto the HUD or out of the video, hand control back
                    // to the local machine immediately.
                    if !virtual_hover
                        && self.capture_mode == LocalCaptureMode::HoverAbsolute
                        && controller_state_allows_input(snapshot.controller_state)
                    {
                        if let Some(rect) = video_rect {
                            // Same 1-pixel tolerance as the auto_release check
                            // in handle_connected_video_response: a strict
                            // contains miss on a sub-pixel boundary used to
                            // fire auto_release here every other frame.
                            if self.pointer_over_local_overlay(pos) {
                                self.auto_release_capture(false);
                                ctx.request_repaint();
                            } else if rect.expand(1.0).contains(pos) {
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
                    if self.suppress_mouse_delta > 0
                        || self
                            .suppress_mouse_delta_until
                            .is_some_and(|until| Instant::now() < until)
                    {
                        continue;
                    }
                    if self.capture_mode == LocalCaptureMode::CapturedRelative
                        && controller_state_allows_input(snapshot.controller_state)
                    {
                        let mut input_delta = delta;
                        // Convert screen-pixel deltas into stream space (keeps a
                        // visible Desktop cursor 1:1 when the window is smaller
                        // than the stream). Latch the decision while a button is
                        // held so a stale CursorState from a dropped frame can't
                        // toggle scaling mid-drag and jolt the remote camera.
                        let live_scale = snapshot.capabilities.separate_cursor
                            && server_cursor_drawable(&snapshot)
                            && !self.resume_hover_after_relative_drag;
                        let scale_enabled = if self.pointer_buttons != 0 {
                            *self.relative_drag_scaling.get_or_insert(live_scale)
                        } else {
                            self.relative_drag_scaling = None;
                            live_scale
                        };
                        if scale_enabled {
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
                        if snapshot.capabilities.separate_cursor
                            && server_cursor_drawable(&snapshot)
                        {
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
                        && server_cursor_drawable(&snapshot);
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
                        && server_wants_relative(&snapshot);
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
                        && server_cursor_drawable(&snapshot);
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
        let wgpu_cfg = eframe::egui_wgpu::WgpuConfiguration {
            desired_maximum_frame_latency: Some(1),
            present_mode: wgpu_present_mode,
            ..Default::default()
        };
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
    fn relative_capture_entry_prefers_local_cursor_position() {
        let remote = egui::pos2(100.0, 200.0);
        let local = egui::pos2(800.0, 600.0);

        // Local wins so the overlay doesn't teleport to wherever the server
        // cursor happens to be when the user enters/clicks the video.  The
        // server cursor is realigned via a warp delta.
        assert_eq!(
            relative_capture_entry_anchor(Some(remote), Some(local)),
            Some(local)
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
