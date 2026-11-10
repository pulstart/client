//! Aux streams: extra video-only connections, each pinned to another server
//! output and shown in its own window. Input stays on the main session: while
//! the pointer is over an aux window, the main session sends absolute mouse
//! positions that name that window's output.

use crate::debug_state::{mono_micros, unix_time_micros, ConnectionDebugState};
use crate::input::{RemoteCursorTexture, SharedInputSnapshot, SharedInputState};
use crate::layout::{self, AuxPlan, AuxTarget, WindowPlacement};
use crate::render::VideoTexture;
use crate::render_gl::GlowSlotPool;
use crate::repaint::Repaint;
use crate::video_frame::{NativeSurfaceCapabilities, NativeSurfaceControl, VideoFrameBuffer};
use crate::{
    clamp_pos_to_video_rect, connection_error_is_terminal, controller_state_allows_input, decode,
    display, egui_key_to_remote_key, file_transfer, normalized_coord, pointer_button_mask,
    reconnect_backoff, run_connection, take_wheel_units, wheel_unit_scale, ConnectionState,
    SessionDisplay, StreamApp,
};
use eframe::egui;
use st_protocol::{
    ControlMessage, InputPacket, KeyboardStateInput, MouseAbsoluteInput, MouseButtonsInput,
    MouseWheelInput, VideoCodecSupport, KEYBOARD_STATE_BYTES,
};
use std::collections::{BTreeMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const MAX_CURSOR_TEXTURES: usize = 8;

/// Aux windows are on wherever they can work; `ST_AUX_WINDOWS=0` (`false`,
/// `no`, `off`) is the escape hatch that keeps a session to its main window.
fn windows_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("ST_AUX_WINDOWS").ok().as_deref(),
            Some("0" | "false" | "no" | "off")
        )
    })
}
/// Frames a new window may take to appear on its monitor before it goes
/// fullscreen anyway.
const FULLSCREEN_SETTLE_FRAMES: u32 = 8;

/// What every aux session needs to start or restart.
pub struct AuxEnv {
    pub ctx: egui::Context,
    pub token: Mutex<String>,
    /// Direct address of the server the main session is connected to.
    pub addr: Mutex<String>,
    pub video_codec_support: Mutex<decode::VideoCodecSupportReport>,
    pub api_discovery: Arc<crate::api_client::ApiDiscoveryShared>,
    pub debug_enabled: Arc<AtomicBool>,
    /// Output textures of closed glow windows, reused by the next ones.
    pub glow_slots: GlowSlotPool,
    /// Present with the wgpu renderer; lets a window upload its own frames.
    pub render_state: Option<eframe::egui_wgpu::RenderState>,
}

/// What a restart policy should do right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryStep {
    Idle,
    /// Start a new session now.
    Restart,
    /// Check again after this long.
    Wait(Duration),
}

/// Reconnect policy of one aux session. A non-terminal error backs off like the
/// main session; a codec exclusion restarts at once; a terminal error (bad
/// token, the server refusing the output) stays on screen.
pub fn retry_step(
    state: &ConnectionState,
    codec_excluded: bool,
    attempts: &mut u32,
    next_retry: &mut Option<Instant>,
    now: Instant,
) -> RetryStep {
    match state {
        ConnectionState::Connected => {
            *attempts = 0;
            *next_retry = None;
            RetryStep::Idle
        }
        ConnectionState::Error(msg) if !connection_error_is_terminal(msg) => match *next_retry {
            None => {
                let delay = reconnect_backoff(*attempts);
                *next_retry = Some(now + delay);
                RetryStep::Wait(delay)
            }
            Some(at) if now >= at => {
                *attempts = attempts.saturating_add(1);
                *next_retry = None;
                RetryStep::Restart
            }
            Some(at) => RetryStep::Wait(at - now),
        },
        ConnectionState::Disconnected if codec_excluded => RetryStep::Restart,
        _ => RetryStep::Idle,
    }
}

/// Adds to `owned` the keys that went down between `before` and `after`, and
/// drops those that came up. Keys already held before stay someone else's.
fn track_owned_keys(
    owned: &mut [u8; KEYBOARD_STATE_BYTES],
    before: &[u8; KEYBOARD_STATE_BYTES],
    after: &[u8; KEYBOARD_STATE_BYTES],
) {
    for ((owned, before), after) in owned.iter_mut().zip(before).zip(after) {
        *owned = (*owned | (after & !before)) & after;
    }
}

/// Whether the pointer at hand drives the remote cursor of an aux window.
pub fn should_forward(hover_ok: bool, inside: bool, dragging: bool, over_hud: bool) -> bool {
    hover_ok && !over_hud && (inside || dragging)
}

/// Where the remote cursor image goes inside an aux window. `local_tip` (the
/// local pointer) wins over the server's reported position so the cursor
/// tracks the hand without a round trip.
pub fn cursor_overlay_rect(
    video_rect: egui::Rect,
    picture: egui::Vec2,
    server_top_left: egui::Pos2,
    local_tip: Option<egui::Pos2>,
    image_size: egui::Vec2,
    hotspot: egui::Vec2,
) -> egui::Rect {
    let scale = if picture.x > 0.0 {
        video_rect.width() / picture.x
    } else {
        1.0
    };
    let top_left = match local_tip {
        Some(tip) => tip - hotspot * scale,
        None => video_rect.min + server_top_left.to_vec2() * scale,
    };
    egui::Rect::from_min_size(top_left, image_size * scale)
}

struct SessionControl {
    disconnect: Arc<AtomicBool>,
    attempts: u32,
    next_retry: Option<Instant>,
}

/// A value that never leaves the UI thread, held where the type system wants
/// `Send`: egui requires `Send + Sync` of a deferred viewport's callback but
/// runs it on the UI thread. The platform presenters inside `VideoTexture`
/// (D3D11/WGL interop, Metal layers) hold thread-bound handles and are not
/// `Send` on Windows and macOS.
#[derive(Default)]
struct UiThreadOnly<T>(T);

// SAFETY: the only value wrapped is `WindowUi::video`, and `AuxStream::ui` is
// locked from the UI thread alone — the viewport callback, the root update
// (`upload_pending`, `aux_raw_input`, window removal) and `connect`. The
// connection thread is handed clones of other fields, never the stream itself,
// so the last `Arc<AuxStream>` is dropped on the UI thread too.
unsafe impl<T> Send for UiThreadOnly<T> {}

impl<T> std::ops::Deref for UiThreadOnly<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for UiThreadOnly<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

#[derive(Default)]
struct WindowUi {
    video: UiThreadOnly<Option<VideoTexture>>,
    upload_frame: VideoFrameBuffer,
    cursor_textures: BTreeMap<u64, RemoteCursorTexture>,
    latest_cursor_serial: Option<u64>,
    seen_cursor_shape_version: u64,
    /// Where the picture was last painted, in this window's points.
    video_rect: Option<egui::Rect>,
    /// Rects of the window's own controls; the pointer over them is not
    /// forwarded.
    hud_rects: Vec<egui::Rect>,
    /// The main session is forwarding this window's pointer.
    forwarding: bool,
    pointer_pos: Option<egui::Pos2>,
    os_cursor_hidden: bool,
    frames: u32,
    fullscreen_sent: bool,
    debug_lines: Vec<String>,
    debug_lines_built: Option<Instant>,
}

pub struct AuxStream {
    pub id: u64,
    pub viewport: egui::ViewportId,
    pub target: AuxTarget,
    pub title: String,
    /// Index into the local monitor list this window is meant for.
    pub monitor_index: Option<usize>,
    placement: WindowPlacement,
    env: Arc<AuxEnv>,
    display: SessionDisplay,
    state: Arc<Mutex<ConnectionState>>,
    shared_input: Arc<SharedInputState>,
    debug_state: Arc<ConnectionDebugState>,
    frame: Arc<Mutex<VideoFrameBuffer>>,
    native_surfaces: Arc<NativeSurfaceControl>,
    excluded_video_codecs: Arc<Mutex<VideoCodecSupport>>,
    connection_epoch: Arc<AtomicU64>,
    session: Mutex<SessionControl>,
    ui: Mutex<WindowUi>,
    pub close_requested: AtomicBool,
}

impl AuxStream {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        id: u64,
        target: AuxTarget,
        title: String,
        monitor_index: Option<usize>,
        monitor: Option<&display::LocalMonitor>,
        placement: WindowPlacement,
        env: Arc<AuxEnv>,
        use_wgpu: bool,
    ) -> Arc<Self> {
        let display = SessionDisplay {
            refresh_millihz: monitor.and_then(|m| m.refresh_millihz),
            width: monitor.map_or(0, |m| m.width),
            height: monitor.map_or(0, |m| m.height),
            output_id: target.requested_output(),
            aux: true,
        };
        let ui = WindowUi {
            video: UiThreadOnly(Some(VideoTexture::new_secondary(
                use_wgpu,
                Arc::clone(&env.glow_slots),
            ))),
            ..Default::default()
        };
        let stream = Arc::new(Self {
            id,
            viewport: egui::ViewportId::from_hash_of(("st-aux-window", id)),
            target,
            title,
            monitor_index,
            placement,
            env,
            display,
            state: Arc::new(Mutex::new(ConnectionState::Connecting)),
            shared_input: Arc::new(SharedInputState::new()),
            debug_state: Arc::new(ConnectionDebugState::new()),
            frame: Arc::new(Mutex::new(VideoFrameBuffer::default())),
            native_surfaces: Arc::new(NativeSurfaceControl::new(
                NativeSurfaceCapabilities::default(),
            )),
            excluded_video_codecs: Arc::new(Mutex::new(VideoCodecSupport::empty())),
            connection_epoch: Arc::new(AtomicU64::new(0)),
            session: Mutex::new(SessionControl {
                disconnect: Arc::new(AtomicBool::new(true)),
                attempts: 0,
                next_retry: None,
            }),
            ui: Mutex::new(ui),
            close_requested: AtomicBool::new(false),
        });
        stream.start();
        stream
    }

    /// The output this window actually shows, once the server said so.
    pub fn output_id(&self) -> Option<u32> {
        self.shared_input
            .snapshot()
            .stream_config
            .map(|cfg| cfg.output_id)
    }

    /// Whether this window shows (or is about to show) `output_id`.
    pub fn shows_output(&self, output_id: u32) -> bool {
        match self.output_id() {
            Some(shown) => shown == output_id,
            None => self.target == AuxTarget::Output(output_id),
        }
    }

    pub fn state(&self) -> ConnectionState {
        self.state.lock().unwrap().clone()
    }

    /// (Re)start the session on a fresh epoch; the previous one winds down.
    fn start(self: &Arc<Self>) {
        let mut session = self.session.lock().unwrap();
        session.disconnect.store(true, Ordering::SeqCst);
        let disconnect = Arc::new(AtomicBool::new(false));
        session.disconnect = Arc::clone(&disconnect);
        let session_epoch = self.connection_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        drop(session);

        *self.state.lock().unwrap() = ConnectionState::Connecting;
        self.frame.lock().unwrap().clear();
        // Glow windows only show an output texture, never a platform
        // presenter, so they take CPU planes; wgpu imports DMA-BUFs itself.
        self.native_surfaces.reset(
            self.ui
                .lock()
                .unwrap()
                .video
                .as_ref()
                .filter(|video| !video.needs_root_frame())
                .map_or_else(NativeSurfaceCapabilities::default, |video| {
                    video.native_surface_capabilities()
                }),
        );
        let addr = self.env.addr.lock().unwrap().clone();
        self.debug_state
            .reset_for_connect(&addr, self.display.refresh_millihz);

        let (_control_tx, control_rx) = crossbeam_channel::bounded::<ControlMessage>(1);
        let (_input_tx, input_rx) = crossbeam_channel::bounded::<InputPacket>(1);
        let uploads_need_root = self
            .ui
            .lock()
            .unwrap()
            .video
            .as_ref()
            .is_some_and(|v| v.needs_root_frame());
        let repaint = Repaint::window(self.env.ctx.clone(), self.viewport, uploads_need_root);

        let token = self.env.token.lock().unwrap().clone();
        let display = self.display;
        let video_codec_support = *self.env.video_codec_support.lock().unwrap();
        let excluded = Arc::clone(&self.excluded_video_codecs);
        let state = Arc::clone(&self.state);
        let frame = Arc::clone(&self.frame);
        let debug_state = Arc::clone(&self.debug_state);
        let connection_epoch = Arc::clone(&self.connection_epoch);
        let debug_enabled = Arc::clone(&self.env.debug_enabled);
        let native_surfaces = Arc::clone(&self.native_surfaces);
        let shared_input = Arc::clone(&self.shared_input);
        let api_discovery = Arc::clone(&self.env.api_discovery);
        std::thread::spawn(move || {
            run_connection(
                addr,
                token,
                display,
                video_codec_support,
                excluded,
                Arc::new(Mutex::new(HashSet::new())),
                state,
                frame,
                debug_state,
                disconnect,
                connection_epoch,
                session_epoch,
                Arc::new(AtomicBool::new(false)),
                debug_enabled,
                native_surfaces,
                shared_input,
                control_rx,
                input_rx,
                repaint,
                api_discovery,
                file_transfer::new_shared_state(),
                Arc::new(AtomicBool::new(false)),
            );
        });
    }

    /// A window for one specific output must not quietly show a different one:
    /// when the server falls back (the output is gone), say so and stop.
    fn reject_substituted_output(&self) {
        let AuxTarget::Output(wanted) = self.target else {
            return;
        };
        let mut state = self.state.lock().unwrap();
        if *state == ConnectionState::Connected && self.output_id().is_some_and(|got| got != wanted)
        {
            *state =
                ConnectionState::Error("Server error: this display is no longer available".into());
            drop(state);
            self.stop();
        }
    }

    /// Runs the reconnect policy. Returns when to look again.
    pub fn tick(self: &Arc<Self>) -> Option<Duration> {
        self.reject_substituted_output();
        let state = self.state();
        let codec_excluded = !self.excluded_video_codecs.lock().unwrap().is_empty();
        let step = {
            let mut session = self.session.lock().unwrap();
            let session = &mut *session;
            retry_step(
                &state,
                codec_excluded,
                &mut session.attempts,
                &mut session.next_retry,
                Instant::now(),
            )
        };
        match step {
            RetryStep::Idle => None,
            RetryStep::Wait(delay) => Some(delay),
            RetryStep::Restart => {
                self.start();
                None
            }
        }
    }

    pub fn stop(&self) {
        self.session
            .lock()
            .unwrap()
            .disconnect
            .store(true, Ordering::SeqCst);
        self.connection_epoch.fetch_add(1, Ordering::SeqCst);
    }

    pub fn builder(&self) -> egui::ViewportBuilder {
        let (width, height) = self.placement.size;
        let mut builder = egui::ViewportBuilder::default()
            .with_title(self.title.clone())
            .with_inner_size([width, height])
            .with_min_inner_size([320.0, 200.0]);
        if let Some((x, y)) = self.placement.position {
            builder = builder.with_position([x, y]);
        }
        builder
    }

    /// The window's texture, for the main window to release (it needs the GL
    /// context or the renderer, not the window's own thread of work).
    fn take_video(&self) -> Option<VideoTexture> {
        self.ui.lock().unwrap().video.take()
    }

    pub fn needs_root_frame(&self) -> bool {
        self.ui
            .lock()
            .unwrap()
            .video
            .as_ref()
            .is_some_and(|v| v.needs_root_frame())
    }

    /// Moves the newest decoded frame into the window's texture. A glow window
    /// needs the root `eframe::Frame`; a wgpu one uploads on its own. Returns
    /// true when a new picture is ready.
    pub fn upload_pending(&self, root_frame: Option<&mut eframe::Frame>) -> bool {
        let mut ui = self.ui.lock().unwrap();
        self.upload_locked(&mut ui, root_frame)
    }

    fn upload_locked(&self, ui: &mut WindowUi, root_frame: Option<&mut eframe::Frame>) -> bool {
        {
            let mut fb = self.frame.lock().unwrap();
            if !(fb.dirty && fb.width > 0) {
                return false;
            }
            std::mem::swap(&mut ui.upload_frame, &mut *fb);
            fb.dirty = false;
            ui.upload_frame.dirty = false;
        }
        let Some(video) = ui.video.as_mut() else {
            return false;
        };
        let result = match (root_frame, self.env.render_state.as_ref()) {
            (Some(frame), _) => video.upload(frame, &ui.upload_frame, &self.native_surfaces),
            (None, Some(render_state)) if !video.needs_root_frame() => {
                video.upload_with_render_state(render_state, &ui.upload_frame)
            }
            _ => return false,
        };
        match result {
            Ok(()) => {
                if self.env.debug_enabled.load(Ordering::Relaxed) {
                    self.debug_state.record_present(
                        &ui.upload_frame,
                        unix_time_micros(),
                        mono_micros(),
                    );
                }
                true
            }
            Err(err) => {
                video.clear_frame();
                *self.state.lock().unwrap() =
                    ConnectionState::Error(format!("GL upload failed: {err}"));
                false
            }
        }
    }

    fn sync_cursor_texture(
        &self,
        ui: &mut WindowUi,
        ctx: &egui::Context,
        snap: &SharedInputSnapshot,
    ) {
        if snap.cursor_shape_version == ui.seen_cursor_shape_version {
            return;
        }
        ui.seen_cursor_shape_version = snap.cursor_shape_version;
        let Some(shape) = snap.cursor_shape.as_ref() else {
            ui.cursor_textures.clear();
            ui.latest_cursor_serial = None;
            return;
        };
        ui.latest_cursor_serial = Some(shape.serial);
        ui.cursor_textures.insert(
            shape.serial,
            RemoteCursorTexture {
                hotspot: egui::vec2(shape.hotspot_x as f32, shape.hotspot_y as f32),
                size: egui::vec2(shape.width as f32, shape.height as f32),
                texture: ctx.load_texture(
                    format!("aux_cursor_{}_{}", self.id, shape.serial),
                    egui::ColorImage::from_rgba_premultiplied(
                        [shape.width as usize, shape.height as usize],
                        &shape.rgba,
                    ),
                    egui::TextureOptions::NEAREST,
                ),
            },
        );
        while ui.cursor_textures.len() > MAX_CURSOR_TEXTURES {
            let Some(oldest) = ui.cursor_textures.keys().next().copied() else {
                break;
            };
            ui.cursor_textures.remove(&oldest);
        }
    }

    /// Windows that can be placed open windowed on their monitor, then go
    /// fullscreen there once the window system has put them in place.
    fn drive_fullscreen(&self, ui: &mut WindowUi, ctx: &egui::Context) {
        ui.frames = ui.frames.saturating_add(1);
        if ui.fullscreen_sent || !self.placement.fullscreen {
            return;
        }
        let on_target = match (
            self.placement.position,
            ctx.input(|i| i.viewport().outer_rect),
        ) {
            (Some((x, y)), Some(rect)) => rect.min.distance(egui::pos2(x, y)) < 100.0,
            _ => false,
        };
        if on_target || ui.frames >= FULLSCREEN_SETTLE_FRAMES {
            ui.fullscreen_sent = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
        } else {
            ctx.request_repaint();
        }
    }

    /// One pass of this window's UI.
    pub fn show_window(self: &Arc<Self>, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) {
            self.close_requested.store(true, Ordering::SeqCst);
            ctx.request_repaint_of(egui::ViewportId::ROOT);
        }
        if let Some(delay) = self.tick() {
            ctx.request_repaint_after(delay);
        }
        let state = self.state();
        let snapshot = self.shared_input.snapshot();

        let mut ui_state = self.ui.lock().unwrap();
        let ui_state = &mut *ui_state;
        if !ui_state
            .video
            .as_ref()
            .is_some_and(|v| v.needs_root_frame())
        {
            self.upload_locked(ui_state, None);
        }
        self.sync_cursor_texture(ui_state, ctx, &snapshot);
        self.drive_fullscreen(ui_state, ctx);
        ui_state.hud_rects.clear();

        let has_frame = ui_state.video.as_ref().is_some_and(|v| v.has_frame());
        let picture = snapshot
            .stream_config
            .map(|cfg| cfg.display_size())
            .filter(|(w, h)| *w > 0 && *h > 0)
            .map(|(w, h)| egui::vec2(w as f32, h as f32))
            .or_else(|| {
                ui_state
                    .video
                    .as_ref()
                    .map(|v| v.size_vec2())
                    .filter(|s| s.x >= 1.0 && s.y >= 1.0)
            });

        let mut close_clicked = false;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
            .show(ctx, |ui| match (&state, picture) {
                (ConnectionState::Connected, Some(picture)) if has_frame => {
                    let available = ui.available_size();
                    let scale = (available.x / picture.x).min(available.y / picture.y);
                    let sized = picture * scale;
                    ui.centered_and_justified(|ui| {
                        let (rect, _) = ui.allocate_exact_size(sized, egui::Sense::hover());
                        if let Some(texture) = ui_state.video.as_ref().and_then(|v| v.texture_id())
                        {
                            ui.painter().image(
                                texture,
                                rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                        ui_state.video_rect = Some(rect);
                    });
                }
                _ => {
                    ui_state.video_rect = None;
                    status_view(ui, &self.title, &state, &mut close_clicked);
                }
            });
        if close_clicked {
            self.close_requested.store(true, Ordering::SeqCst);
            ctx.request_repaint_of(egui::ViewportId::ROOT);
        }

        self.paint_cursor(ui_state, ctx, &snapshot, picture);
        self.paint_hud(ui_state, ctx);
        if self.env.debug_enabled.load(Ordering::Relaxed) {
            self.paint_debug(ui_state, ctx, &snapshot);
        }
    }

    fn paint_cursor(
        &self,
        ui: &mut WindowUi,
        ctx: &egui::Context,
        snap: &SharedInputSnapshot,
        picture: Option<egui::Vec2>,
    ) {
        let drawable = snap.cursor_state.visible && !snap.cursor_state.app_grab;
        let texture = ui
            .cursor_textures
            .get(&snap.cursor_state.serial)
            .or_else(|| {
                ui.latest_cursor_serial
                    .and_then(|serial| ui.cursor_textures.get(&serial))
            });
        let rect = ui.video_rect;
        let overlay = match (drawable, texture, rect, picture) {
            (true, Some(texture), Some(rect), Some(picture)) => {
                let local_tip = if ui.forwarding { ui.pointer_pos } else { None };
                Some((
                    texture.texture.id(),
                    cursor_overlay_rect(
                        rect,
                        picture,
                        egui::pos2(snap.cursor_state.x as f32, snap.cursor_state.y as f32),
                        local_tip,
                        texture.size,
                        texture.hotspot,
                    ),
                    rect,
                ))
            }
            _ => None,
        };

        let hide_os = ui.forwarding && overlay.is_some();
        if hide_os != ui.os_cursor_hidden {
            ui.os_cursor_hidden = hide_os;
            ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(!hide_os));
        }
        if let Some((texture_id, image_rect, video_rect)) = overlay {
            // With the local pointer over the window but not forwarded, the OS
            // cursor shows; a second one at the server position would double it.
            let local_pointer_free = ctx.input(|i| i.pointer.hover_pos()).is_none();
            if image_rect.intersects(video_rect) && (ui.forwarding || local_pointer_free) {
                egui::Area::new(egui::Id::new(("aux_cursor", self.id)))
                    .order(egui::Order::Tooltip)
                    .interactable(false)
                    .fixed_pos(image_rect.min)
                    .show(ctx, |ui| {
                        ui.image(egui::load::SizedTexture::new(texture_id, image_rect.size()));
                    });
            }
        }
    }

    /// Small controls that appear while the pointer is near the top right.
    fn paint_hud(&self, ui: &mut WindowUi, ctx: &egui::Context) {
        let screen = ctx.content_rect();
        let zone = egui::Rect::from_min_max(
            egui::pos2((screen.right() - 220.0).max(screen.left()), screen.top()),
            egui::pos2(screen.right(), screen.top() + 56.0),
        );
        let near = ctx
            .input(|i| i.pointer.hover_pos())
            .is_some_and(|pos| zone.contains(pos));
        if !near {
            return;
        }
        let fullscreen = ctx.input(|i| i.viewport().fullscreen).unwrap_or(false);
        let area = egui::Area::new(egui::Id::new(("aux_hud", self.id)))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_TOP, [-8.0, 8.0])
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(egui::Color32::from_rgba_unmultiplied(20, 24, 28, 220))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let label = if fullscreen { "Window" } else { "Fullscreen" };
                            if ui.button(label).clicked() {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(
                                    !fullscreen,
                                ));
                            }
                            if ui.button("Close").clicked() {
                                self.close_requested.store(true, Ordering::SeqCst);
                                ctx.request_repaint_of(egui::ViewportId::ROOT);
                            }
                        });
                    });
            });
        ui.hud_rects.push(area.response.rect);
    }

    fn paint_debug(&self, ui: &mut WindowUi, ctx: &egui::Context, snap: &SharedInputSnapshot) {
        if ui
            .debug_lines_built
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(150))
        {
            let snapshot = self.debug_state.snapshot();
            let mut lines = vec![
                format!(
                    "window: {} (output {})",
                    self.title,
                    self.output_id()
                        .map_or_else(|| "-".to_string(), |id| id.to_string())
                ),
                format!("server: {}", snapshot.server_addr),
                format!("stream: {}", crate::debug_stream_line(snap)),
                format!(
                    "display: {}  decoder={}",
                    crate::format_refresh(snapshot.display_refresh_millihz),
                    if snapshot.decoder_name.is_empty() {
                        "-"
                    } else {
                        snapshot.decoder_name.as_str()
                    }
                ),
            ];
            lines.extend(crate::debug_link_lines(&snapshot));
            ui.debug_lines = lines;
            ui.debug_lines_built = Some(Instant::now());
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let area = egui::Area::new(egui::Id::new(("aux_debug", self.id)))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::LEFT_TOP, [8.0, 8.0])
            .interactable(false)
            .show(ctx, |ui_| {
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 170))
                    .inner_margin(6.0)
                    .show(ui_, |ui_| {
                        for line in &ui.debug_lines {
                            ui_.label(
                                egui::RichText::new(line)
                                    .monospace()
                                    .size(11.0)
                                    .color(egui::Color32::from_rgb(210, 220, 230)),
                            );
                        }
                    });
            });
        ui.hud_rects.push(area.response.rect);
    }
}

impl Drop for AuxStream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn status_view(ui: &mut egui::Ui, title: &str, state: &ConnectionState, close_clicked: &mut bool) {
    const TEXT: egui::Color32 = egui::Color32::from_rgb(230, 233, 240);
    const DIM: egui::Color32 = egui::Color32::from_rgb(138, 142, 150);
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 3.0);
        match state {
            ConnectionState::Error(msg) if connection_error_is_terminal(msg) => {
                ui.label(egui::RichText::new(title).size(16.0).color(TEXT));
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(msg)
                        .size(13.0)
                        .color(egui::Color32::from_rgb(220, 100, 100)),
                );
                ui.add_space(14.0);
                if ui.button("Close").clicked() {
                    *close_clicked = true;
                }
            }
            ConnectionState::Error(msg) => {
                ui.spinner();
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Reconnecting…").size(15.0).color(TEXT));
                ui.add_space(4.0);
                ui.label(egui::RichText::new(msg).size(12.0).color(DIM));
            }
            _ => {
                ui.spinner();
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(format!("Waiting for {title}…"))
                        .size(14.0)
                        .color(DIM),
                );
            }
        }
    });
}

/// Pointer state the main session keeps while an aux window drives the cursor.
pub struct AuxPointer {
    viewport: egui::ViewportId,
    buttons: u8,
    /// Keys this window pressed in the shared keyboard state. Releasing only
    /// these leaves keys held in the main window alone.
    keys: [u8; KEYBOARD_STATE_BYTES],
    last_sent: Option<(u16, u16)>,
}

impl StreamApp {
    fn aux_env(&mut self, ctx: &egui::Context) -> Arc<AuxEnv> {
        let codecs =
            crate::advertised_video_codec_support(self.video_codec_support, self.yuv444_enabled);
        let env = self.aux_env.get_or_insert_with(|| {
            Arc::new(AuxEnv {
                ctx: ctx.clone(),
                token: Mutex::new(String::new()),
                addr: Mutex::new(String::new()),
                video_codec_support: Mutex::new(codecs),
                api_discovery: Arc::clone(&self.api_discovery),
                debug_enabled: Arc::clone(&self.debug_enabled_flag),
                glow_slots: Arc::clone(&self.glow_slots),
                render_state: self.render_state.clone(),
            })
        });
        *env.token.lock().unwrap() = self.token.clone();
        *env.addr.lock().unwrap() = crate::normalize_server_addr(&self.server_addr);
        *env.video_codec_support.lock().unwrap() = codecs;
        Arc::clone(env)
    }

    /// Opens a window for `target`. `monitor` is the local monitor it is meant
    /// for; `None` opens an ordinary window.
    pub fn open_aux(&mut self, ctx: &egui::Context, target: AuxTarget, monitor: Option<usize>) {
        if !self.direct_path.load(Ordering::SeqCst) || !windows_enabled() {
            return;
        }
        if matches!(target, AuxTarget::Output(id) if self.aux_shows_output(id)) {
            return;
        }
        let snapshot = self.shared_input.snapshot();
        let output = match target {
            AuxTarget::Output(id) => snapshot.available_outputs.iter().find(|o| o.id == id),
            AuxTarget::Virtual => None,
        };
        let title = match (target, output) {
            (AuxTarget::Virtual, _) => "Virtual display".to_string(),
            (_, Some(output)) => format!("{} ({}×{})", output.name, output.width, output.height),
            (AuxTarget::Output(id), None) => format!("Display {id}"),
        };
        let local = monitor.and_then(|i| self.local_monitors.get(i)).cloned();
        let picture = match (output, local.as_ref()) {
            (Some(output), _) => (output.width, output.height),
            (None, Some(local)) => (local.width, local.height),
            (None, None) => (0, 0),
        };
        let placement = match local.as_ref() {
            Some(local) => layout::window_placement(local, picture, display::can_place_windows()),
            None => layout::floating_placement(picture),
        };
        self.aux_seq += 1;
        let env = self.aux_env(ctx);
        let stream = AuxStream::open(
            self.aux_seq,
            target,
            title,
            monitor,
            local.as_ref(),
            placement,
            env,
            self.render_state.is_some(),
        );
        self.aux.push(stream);
        if monitor.is_some() && !placement.fullscreen {
            self.note_manual_placement();
        }
    }

    /// The one way an aux window goes away: whatever the pointer holds through
    /// it is released first, and its texture is queued for the main window to
    /// free.
    fn remove_aux(&mut self, remove: impl Fn(&AuxStream) -> bool) {
        let holds_pointer = self.aux_pointer.as_ref().is_some_and(|p| {
            self.aux
                .iter()
                .any(|a| a.viewport == p.viewport && remove(a))
        });
        if holds_pointer {
            self.stop_aux_pointer();
        }
        let (removed, kept) = std::mem::take(&mut self.aux)
            .into_iter()
            .partition::<Vec<_>, _>(|a| remove(a));
        self.aux = kept;
        self.retired_video
            .extend(removed.iter().filter_map(|a| a.take_video()));
    }

    pub fn close_all_aux(&mut self) {
        self.remove_aux(|_| true);
        self.stop_aux_pointer();
    }

    pub fn close_aux_for_output(&mut self, output_id: u32) {
        self.remove_aux(|a| a.shows_output(output_id));
    }

    pub fn aux_shows_output(&self, output_id: u32) -> bool {
        self.aux.iter().any(|a| a.shows_output(output_id))
    }

    /// First local monitor that neither the main window nor an aux window uses.
    pub fn free_monitor(&self, ctx: &egui::Context) -> Option<usize> {
        let root = self.root_monitor(ctx);
        (0..self.local_monitors.len())
            .find(|i| Some(*i) != root && !self.aux.iter().any(|a| a.monitor_index == Some(*i)))
    }

    fn note_manual_placement(&mut self) {
        if !display::can_place_windows() && !self.placement_note_shown {
            self.placement_note_until = Some(Instant::now() + Duration::from_secs(12));
            self.placement_note_shown = true;
            crate::save_placement_note_shown();
        }
    }

    /// Per root pass: reconcile aux windows with the main session, upload
    /// frames glow windows cannot upload themselves, and (re)declare every
    /// window. Must run on every pass — a window not declared is closed.
    pub fn service_aux(
        &mut self,
        ctx: &egui::Context,
        frame: &mut eframe::Frame,
        state: &ConnectionState,
    ) {
        self.reconcile_aux(ctx, frame, state);
        for mut video in self.retired_video.drain(..) {
            video.release(frame);
        }
    }

    fn reconcile_aux(
        &mut self,
        ctx: &egui::Context,
        frame: &mut eframe::Frame,
        state: &ConnectionState,
    ) {
        self.remove_aux(|a| a.close_requested.load(Ordering::SeqCst));
        match state {
            ConnectionState::Disconnected => {
                self.close_all_aux();
                self.layout_applied = false;
                return;
            }
            ConnectionState::Error(msg) if crate::connection_error_is_terminal(msg) => {
                self.close_all_aux();
                self.layout_applied = false;
                return;
            }
            ConnectionState::Connected => {
                if let Some(output) = self
                    .shared_input
                    .snapshot()
                    .stream_config
                    .map(|cfg| cfg.output_id)
                {
                    // The main window switched to an output an aux window
                    // shows: the main window takes it over.
                    self.remove_aux(|a| a.output_id() == Some(output));
                }
                self.apply_auto_layout(ctx);
            }
            _ => {}
        }

        for aux in &self.aux {
            if aux.needs_root_frame() && aux.upload_pending(Some(frame)) {
                ctx.request_repaint_of(aux.viewport);
            }
            if let Some(delay) = aux.tick() {
                ctx.request_repaint_after_for(delay, aux.viewport);
            }
            let window = Arc::clone(aux);
            ctx.show_viewport_deferred(aux.viewport, aux.builder(), move |ctx, _class| {
                window.show_window(ctx);
            });
        }
    }

    /// Opens a window for every local monitor beyond the main one, once per
    /// session, when the setting is on and the main session is direct.
    fn apply_auto_layout(&mut self, ctx: &egui::Context) {
        if self.layout_applied || !self.auto_layout || !self.direct_path.load(Ordering::SeqCst) {
            return;
        }
        let snapshot = self.shared_input.snapshot();
        let Some(stream) = snapshot.stream_config else {
            return;
        };
        if !snapshot.outputs_received {
            return;
        }
        // The listing comes from a worker; planning waits for it.
        if !self.monitors_fresh() {
            self.request_monitor_probe(ctx);
            return;
        }
        let window_monitor = self.window_monitor(ctx);
        let waited = self
            .layout_wait_since
            .get_or_insert_with(Instant::now)
            .elapsed();
        if !layout::position_settled(
            window_monitor.is_some(),
            display::can_place_windows(),
            waited,
        ) {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        self.layout_wait_since = None;
        self.layout_applied = true;
        let Some(root) = window_monitor.or_else(|| display::primary_monitor(&self.local_monitors))
        else {
            return;
        };
        let plan = layout::plan_auto_layout(
            &self.local_monitors,
            &snapshot.available_outputs,
            root,
            stream.output_id,
            snapshot.virtual_supported,
        );
        for AuxPlan { monitor, target } in plan {
            let taken = self.aux.iter().any(|a| a.monitor_index == Some(monitor))
                || matches!(target, AuxTarget::Output(id) if self.aux_shows_output(id));
            if !taken {
                self.open_aux(ctx, target, Some(monitor));
            }
        }
    }

    // ---- input -----------------------------------------------------------

    pub fn aux_window(&self, viewport: egui::ViewportId) -> Option<Arc<AuxStream>> {
        self.aux.iter().find(|a| a.viewport == viewport).cloned()
    }

    /// Releases the keys an aux window pressed (and only those): they would
    /// stay down on the server once its input stops.
    fn release_aux_keys(&mut self, client_id: u32) {
        let Some(pointer) = self.aux_pointer.as_mut() else {
            return;
        };
        let mask = std::mem::take(&mut pointer.keys);
        self.release_key_mask(client_id, &mask);
    }

    fn release_key_mask(&mut self, client_id: u32, mask: &[u8; KEYBOARD_STATE_BYTES]) {
        if self.keyboard_state.release_mask(mask) {
            self.send_input_packet(InputPacket::KeyboardState(KeyboardStateInput {
                client_id,
                pressed: self.keyboard_state.pressed(),
            }));
        }
    }

    fn stop_aux_pointer(&mut self) {
        let Some(pointer) = self.aux_pointer.take() else {
            return;
        };
        if let Some(aux) = self.aux_window(pointer.viewport) {
            let mut ui = aux.ui.lock().unwrap();
            ui.forwarding = false;
            ui.pointer_pos = None;
        }
        if let Some(client_id) = self.shared_input.client_id() {
            if pointer.buttons != 0 {
                self.send_input_packet(InputPacket::MouseButtons(MouseButtonsInput {
                    client_id,
                    buttons: 0,
                }));
            }
            self.release_key_mask(client_id, &pointer.keys);
        }
    }

    fn send_aux_absolute(
        &mut self,
        client_id: u32,
        output_id: u32,
        pos: egui::Pos2,
        rect: egui::Rect,
        force: bool,
    ) {
        let x = normalized_coord(pos.x, rect.left(), rect.right());
        let y = normalized_coord(pos.y, rect.top(), rect.bottom());
        let Some(pointer) = self.aux_pointer.as_mut() else {
            return;
        };
        if force || pointer.last_sent != Some((x, y)) {
            pointer.last_sent = Some((x, y));
            let buttons = pointer.buttons;
            self.send_input_packet(InputPacket::MouseAbsolute(MouseAbsoluteInput {
                client_id,
                x,
                y,
                buttons,
                output_id,
            }));
        }
    }

    /// Input of an aux window, forwarded through the main session.
    pub fn aux_raw_input(
        &mut self,
        ctx: &egui::Context,
        aux: &Arc<AuxStream>,
        raw_input: &mut egui::RawInput,
    ) {
        let snapshot = self.shared_input.snapshot();
        let Some(client_id) = snapshot.client_id else {
            return;
        };
        let hover_ok = snapshot.capabilities.mouse_absolute
            && snapshot.capabilities.hover_capture
            && controller_state_allows_input(snapshot.controller_state);
        let (rect, hud) = {
            let ui = aux.ui.lock().unwrap();
            (ui.video_rect, ui.hud_rects.clone())
        };
        let (Some(output_id), Some(rect)) = (aux.output_id(), rect) else {
            // The picture went away (reconnect): let go of whatever was held.
            if self
                .aux_pointer
                .as_ref()
                .is_some_and(|p| p.viewport == aux.viewport)
            {
                self.stop_aux_pointer();
            }
            return;
        };
        let over_hud = |pos: egui::Pos2| hud.iter().any(|r| r.contains(pos));
        let ppp = ctx.pixels_per_point();
        let mut forwarding_here = self
            .aux_pointer
            .as_ref()
            .is_some_and(|p| p.viewport == aux.viewport);
        let start_forwarding = |app: &mut StreamApp| {
            if app
                .aux_pointer
                .as_ref()
                .is_some_and(|p| p.viewport != aux.viewport)
            {
                app.stop_aux_pointer();
            }
            if app.aux_pointer.is_none() {
                // The main window cannot keep driving the cursor meanwhile, and
                // its first position on return must not be skipped as a repeat.
                // Its held keys are its own to release.
                if app.capture_mode == crate::input::LocalCaptureMode::HoverAbsolute {
                    app.auto_release_capture_with(false, false);
                }
                app.last_sent_absolute_cursor = None;
                app.aux_pointer = Some(AuxPointer {
                    viewport: aux.viewport,
                    buttons: 0,
                    keys: [0; KEYBOARD_STATE_BYTES],
                    last_sent: None,
                });
            }
        };

        for event in raw_input.events.clone() {
            match event {
                egui::Event::PointerMoved(pos) => {
                    let dragging = self
                        .aux_pointer
                        .as_ref()
                        .is_some_and(|p| p.viewport == aux.viewport && p.buttons != 0);
                    let inside = rect.expand(1.0).contains(pos);
                    if should_forward(hover_ok, inside, dragging, over_hud(pos)) {
                        if !forwarding_here {
                            start_forwarding(self);
                            forwarding_here = true;
                        }
                        let clamped = clamp_pos_to_video_rect(pos, rect, ppp);
                        {
                            let mut ui = aux.ui.lock().unwrap();
                            ui.forwarding = true;
                            ui.pointer_pos = Some(clamped);
                        }
                        self.send_aux_absolute(client_id, output_id, clamped, rect, false);
                        ctx.request_repaint_of(aux.viewport);
                    } else if forwarding_here && !dragging {
                        self.stop_aux_pointer();
                        forwarding_here = false;
                        ctx.request_repaint_of(aux.viewport);
                    }
                }
                egui::Event::WindowFocused(false) if forwarding_here => {
                    self.release_aux_keys(client_id);
                }
                egui::Event::PointerGone
                    if forwarding_here
                        && self.aux_pointer.as_ref().is_none_or(|p| p.buttons == 0) =>
                {
                    self.stop_aux_pointer();
                    forwarding_here = false;
                    ctx.request_repaint_of(aux.viewport);
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
                    let inside = rect.expand(1.0).contains(pos);
                    if !forwarding_here {
                        if !(pressed && should_forward(hover_ok, inside, false, over_hud(pos))) {
                            continue;
                        }
                        start_forwarding(self);
                        forwarding_here = true;
                    }
                    if let Some(pointer) = self.aux_pointer.as_mut() {
                        if pressed {
                            pointer.buttons |= mask;
                        } else {
                            pointer.buttons &= !mask;
                        }
                    }
                    let clamped = clamp_pos_to_video_rect(pos, rect, ppp);
                    {
                        let mut ui = aux.ui.lock().unwrap();
                        ui.forwarding = true;
                        ui.pointer_pos = Some(clamped);
                    }
                    self.send_aux_absolute(client_id, output_id, clamped, rect, true);
                    let released_outside = !pressed
                        && !inside
                        && self.aux_pointer.as_ref().is_some_and(|p| p.buttons == 0);
                    if released_outside {
                        self.stop_aux_pointer();
                        forwarding_here = false;
                    }
                    ctx.request_repaint_of(aux.viewport);
                }
                egui::Event::MouseWheel { delta, unit, .. } if forwarding_here => {
                    let buttons = self.aux_pointer.as_ref().map_or(0, |p| p.buttons);
                    let scaled = delta * wheel_unit_scale(unit) * self.scroll_sensitivity;
                    self.pending_wheel_units += scaled;
                    let delta_x = take_wheel_units(&mut self.pending_wheel_units.x);
                    let delta_y = take_wheel_units(&mut self.pending_wheel_units.y);
                    if delta_x != 0 || delta_y != 0 {
                        self.send_input_packet(InputPacket::MouseWheel(MouseWheelInput {
                            client_id,
                            delta_x,
                            delta_y,
                            buttons,
                        }));
                    }
                }
                _ => {}
            }
        }

        // Only the focused window receives keys; an unfocused one must not
        // overwrite modifiers held elsewhere.
        if forwarding_here && snapshot.capabilities.keyboard && raw_input.focused {
            let before = self.keyboard_state.pressed();
            let mut changed = self.keyboard_state.sync_modifiers(raw_input.modifiers);
            for event in &raw_input.events {
                if let egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    ..
                } = *event
                {
                    if let Some(remote) = egui_key_to_remote_key(physical_key.unwrap_or(key)) {
                        changed |= self.keyboard_state.set_key(remote, pressed);
                    }
                }
            }
            if changed {
                let after = self.keyboard_state.pressed();
                if let Some(pointer) = self.aux_pointer.as_mut() {
                    track_owned_keys(&mut pointer.keys, &before, &after);
                }
                self.send_input_packet(InputPacket::KeyboardState(KeyboardStateInput {
                    client_id,
                    pressed: after,
                }));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1))
    }

    #[test]
    fn transient_errors_back_off_then_restart() {
        let mut attempts = 0;
        let mut next = None;
        let now = Instant::now();
        let error = ConnectionState::Error("Connection lost".into());

        let first = retry_step(&error, false, &mut attempts, &mut next, now);
        assert_eq!(first, RetryStep::Wait(reconnect_backoff(0)));
        let waiting = retry_step(&error, false, &mut attempts, &mut next, now);
        assert!(matches!(waiting, RetryStep::Wait(d) if d <= reconnect_backoff(0)));

        let later = now + reconnect_backoff(0);
        assert_eq!(
            retry_step(&error, false, &mut attempts, &mut next, later),
            RetryStep::Restart
        );
        assert_eq!(attempts, 1);
        assert_eq!(next, None);

        // The next failure waits longer.
        assert_eq!(
            retry_step(&error, false, &mut attempts, &mut next, later),
            RetryStep::Wait(reconnect_backoff(1))
        );
        assert!(reconnect_backoff(1) > reconnect_backoff(0));
    }

    #[test]
    fn a_connected_session_clears_the_backoff() {
        let mut attempts = 3;
        let mut next = Some(Instant::now());
        let step = retry_step(
            &ConnectionState::Connected,
            false,
            &mut attempts,
            &mut next,
            Instant::now(),
        );
        assert_eq!(step, RetryStep::Idle);
        assert_eq!((attempts, next), (0, None));
    }

    #[test]
    fn terminal_errors_are_not_retried_but_codec_fallback_is() {
        let mut attempts = 0;
        let mut next = None;
        let now = Instant::now();
        for msg in [
            "Server error: no virtual outputs",
            "Authentication failed. Check your token.",
        ] {
            assert_eq!(
                retry_step(
                    &ConnectionState::Error(msg.into()),
                    false,
                    &mut attempts,
                    &mut next,
                    now
                ),
                RetryStep::Idle,
                "{msg}"
            );
        }
        assert_eq!(
            retry_step(
                &ConnectionState::Disconnected,
                true,
                &mut attempts,
                &mut next,
                now
            ),
            RetryStep::Restart
        );
        assert_eq!(
            retry_step(
                &ConnectionState::Disconnected,
                false,
                &mut attempts,
                &mut next,
                now
            ),
            RetryStep::Idle
        );
    }

    #[test]
    fn only_keys_pressed_through_the_aux_window_are_owned() {
        let mut keys = crate::input::LocalKeyboardState::default();
        keys.set_key(st_protocol::KeyboardKey::LeftShift, true);
        let before = keys.pressed();
        keys.set_key(st_protocol::KeyboardKey::Enter, true);
        let mut owned = [0; KEYBOARD_STATE_BYTES];
        track_owned_keys(&mut owned, &before, &keys.pressed());

        let mut expected = crate::input::LocalKeyboardState::default();
        expected.set_key(st_protocol::KeyboardKey::Enter, true);
        assert_eq!(owned, expected.pressed(), "shift stays the main window's");

        let released = keys.pressed();
        keys.set_key(st_protocol::KeyboardKey::Enter, false);
        track_owned_keys(&mut owned, &released, &keys.pressed());
        assert_eq!(owned, [0; KEYBOARD_STATE_BYTES]);
    }

    #[test]
    fn pointer_is_forwarded_over_the_picture_or_while_dragging() {
        assert!(should_forward(true, true, false, false));
        assert!(
            !should_forward(true, true, false, true),
            "over the window's controls"
        );
        assert!(
            !should_forward(false, true, false, false),
            "server cannot hover"
        );
        assert!(
            !should_forward(true, false, false, false),
            "outside the picture"
        );
        assert!(
            should_forward(true, false, true, false),
            "a drag may leave it"
        );
    }

    #[test]
    fn cursor_follows_the_local_pointer_or_the_server_position() {
        // A 400x300 picture shown at half size in a window offset by (100, 50).
        let video = rect(100.0, 50.0, 300.0, 200.0);
        let picture = egui::vec2(400.0, 300.0);
        let image = egui::vec2(32.0, 32.0);
        let hotspot = egui::vec2(4.0, 2.0);

        let from_server =
            cursor_overlay_rect(video, picture, egui::pos2(40.0, 20.0), None, image, hotspot);
        assert_eq!(from_server.min, egui::pos2(120.0, 60.0));
        assert_eq!(from_server.size(), egui::vec2(16.0, 16.0));

        let tip = egui::pos2(150.0, 90.0);
        let from_pointer = cursor_overlay_rect(
            video,
            picture,
            egui::pos2(40.0, 20.0),
            Some(tip),
            image,
            hotspot,
        );
        assert_eq!(
            from_pointer.min,
            egui::pos2(148.0, 89.0),
            "tip minus scaled hotspot"
        );
    }
}
