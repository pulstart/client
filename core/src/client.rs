use crate::control_stream::{drain_control_messages, normalize_server_address};
use crate::media::{
    AudioPacket, AudioPacketQueue, AudioQueueStats, MediaDemux, QueuedAudioPacket, ReceivedData,
};
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use st_protocol::packet::frame_type;
use st_protocol::reliable_udp::PunchedMessage;
use st_protocol::tcp_tunnel::{TcpTunnel, TunnelLink, TCP_TUNNEL_PREAMBLE};
use st_protocol::{
    ClientDisplayInfo, ClockSyncPing, ControlMessage, ControllerState, CursorShape, CursorState,
    InputCapabilities, InputCredential, InputPacket, KeyboardKey, KeyboardStateInput,
    MouseAbsoluteInput, MouseButtonsInput, MouseRelativeInput, MouseWheelInput, StreamConfig,
    VideoChromaSampling, VideoCodec, VideoCodecSupport, KEYBOARD_STATE_BYTES, MAX_TEXT_INPUT_BYTES,
};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(35);
const MEDIA_STALL_TIMEOUT: Duration = Duration::from_secs(15);
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(250);
const INPUT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(50);
const INPUT_REPAIR_WINDOW: Duration = Duration::from_millis(200);
const VIDEO_QUEUE_CAPACITY: usize = 8;
const AUDIO_QUEUE_MAX_CAPACITY: usize = 8;
const AUDIO_QUEUE_TARGET_MS: usize = 20;
const COMMAND_QUEUE_CAPACITY: usize = 64;
const STREAM_ACCEPT_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_UDP_DATAGRAM_SIZE: usize = 65_535;
const MAX_TOKEN_LEN: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionStage {
    DirectConnect,
    DirectTcpTunnel,
    ApiPunch,
    ApiRelay,
    Authentication,
    Startup,
    Connected,
}

impl SessionStage {
    fn label(self) -> &'static str {
        match self {
            Self::DirectConnect => "direct connect",
            Self::DirectTcpTunnel => "direct TCP tunnel",
            Self::ApiPunch => "API punch",
            Self::ApiRelay => "API relay",
            Self::Authentication => "authentication",
            Self::Startup => "stream startup",
            Self::Connected => "connected session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionFailure {
    stage: SessionStage,
    terminal: bool,
    message: String,
}

impl SessionFailure {
    fn transport(stage: SessionStage, message: impl Into<String>) -> Self {
        Self {
            stage,
            terminal: false,
            message: message.into(),
        }
    }

    fn classified(stage: SessionStage, message: impl Into<String>) -> Self {
        let message = message.into();
        let terminal = message.starts_with("authentication failed")
            || message.starts_with("server error")
            || message.starts_with("server is shutting down")
            || message.contains("unsupported codec")
            || message.contains("Android MVP supports")
            || message.contains("invalid stream size");
        Self {
            stage,
            terminal,
            message,
        }
    }
}

impl std::fmt::Display for SessionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.stage.label(), self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiConnectionConfig {
    pub api_url: String,
    pub client_peer_id: String,
    pub host_peer_id: String,
    pub request_nonce: u64,
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub server: Option<String>,
    pub api: Option<ApiConnectionConfig>,
    pub token: String,
    pub refresh_millihz: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamEvent {
    pub transport_generation: u32,
    pub generation: u32,
    pub video_epoch: u64,
    pub width: u32,
    pub height: u32,
    pub cursor_width: u32,
    pub cursor_height: u32,
    pub framerate: u16,
    pub audio_sample_rate: u32,
    pub audio_channels: u8,
    pub packet_duration_ms: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackpadRoute {
    Relative,
    Absolute { x: u16, y: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revised<T> {
    pub revision: u64,
    pub value: T,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlSnapshot {
    pub transport_generation: u32,
    pub input_capabilities: Option<Revised<InputCapabilities>>,
    pub controller_state: Option<Revised<ControllerState>>,
    pub cursor_shape: Option<Revised<CursorShape>>,
    pub cursor_state: Option<Revised<CursorState>>,
}

impl ControlSnapshot {
    fn is_empty(&self) -> bool {
        self.input_capabilities.is_none()
            && self.controller_state.is_none()
            && self.cursor_shape.is_none()
            && self.cursor_state.is_none()
    }
}

struct RevisionedValue<T> {
    value: Option<T>,
    revision: u64,
    taken_revision: u64,
}

impl<T> Default for RevisionedValue<T> {
    fn default() -> Self {
        Self {
            value: None,
            revision: 0,
            taken_revision: 0,
        }
    }
}

impl<T: PartialEq> RevisionedValue<T> {
    fn set(&mut self, value: T) {
        if self.value.as_ref() == Some(&value) {
            return;
        }
        self.value = Some(value);
        self.revision = self.revision.wrapping_add(1).max(1);
    }
}

impl<T: Clone> RevisionedValue<T> {
    fn take_changed(&mut self) -> Option<Revised<T>> {
        if self.revision == self.taken_revision {
            return None;
        }
        self.taken_revision = self.revision;
        Some(Revised {
            revision: self.revision,
            value: self.value.as_ref()?.clone(),
        })
    }
}

#[derive(Default)]
struct ControlState {
    taken_transport_generation: u32,
    input_capabilities: RevisionedValue<InputCapabilities>,
    controller_state: RevisionedValue<ControllerState>,
    cursor_shape: RevisionedValue<CursorShape>,
    cursor_state: RevisionedValue<CursorState>,
}

#[derive(Clone, Copy)]
struct InputEligibility {
    capabilities: InputCapabilities,
    controller_state: ControllerState,
}

impl Default for InputEligibility {
    fn default() -> Self {
        Self {
            capabilities: InputCapabilities::default(),
            controller_state: ControllerState::Unavailable,
        }
    }
}

impl InputEligibility {
    fn has_control(self) -> bool {
        self.controller_state != ControllerState::Unavailable
    }

    fn allows_absolute(self) -> bool {
        self.has_control() && self.capabilities.mouse_absolute
    }

    fn allows_trackpad_delta(self) -> bool {
        self.has_control() && (self.capabilities.mouse_relative || self.capabilities.mouse_absolute)
    }

    fn allows_buttons(self) -> bool {
        self.allows_trackpad_delta()
    }

    fn allows_keyboard(self) -> bool {
        self.has_control() && self.capabilities.keyboard
    }

    fn allows_text_input(self) -> bool {
        self.allows_keyboard() && self.capabilities.text_input
    }
}

impl ControlState {
    fn take_snapshot(&mut self, transport_generation: u32) -> Option<ControlSnapshot> {
        let transport_changed = self.taken_transport_generation != transport_generation;
        self.taken_transport_generation = transport_generation;
        let snapshot = ControlSnapshot {
            transport_generation,
            input_capabilities: self.input_capabilities.take_changed(),
            controller_state: self.controller_state.take_changed(),
            cursor_shape: self.cursor_shape.take_changed(),
            cursor_state: self.cursor_state.take_changed(),
        };
        (transport_changed || !snapshot.is_empty()).then_some(snapshot)
    }
}

#[derive(Debug)]
pub struct AccessUnit {
    pub frame_id: u32,
    pub frame_type: u8,
    pub data: Vec<u8>,
}

enum Command {
    AcceptStream {
        generation: u32,
        video_epoch: u64,
        acknowledgement: Sender<bool>,
    },
    RequestKeyframe,
    SetAudio(bool),
    TextInput(String),
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseState {
    Absolute {
        x: u16,
        y: u16,
        buttons: u8,
    },
    Relative {
        dx: i16,
        dy: i16,
        buttons: u8,
    },
    Wheel {
        delta_x: i16,
        delta_y: i16,
        buttons: u8,
    },
    Keyboard([u8; KEYBOARD_STATE_BYTES]),
    Buttons(u8),
}

struct Shared {
    stop: AtomicBool,
    status: Mutex<String>,
    stream_event: Mutex<Option<StreamEvent>>,
    control_state: Mutex<ControlState>,
    trackpad_router: Mutex<TrackpadRouter>,
    video_tx: Sender<AccessUnit>,
    video_rx: Receiver<AccessUnit>,
    audio_queue: AudioPacketQueue,
    command_rx: Receiver<Command>,
    mouse_rx: Receiver<MouseState>,
    accepted_generation: AtomicU32,
    accepted_video_epoch: AtomicU64,
    last_stream_generation: AtomicU32,
    transport_generation: AtomicU32,
    audio_enabled_requested: AtomicBool,
}

pub struct Client {
    shared: Arc<Shared>,
    command_tx: Sender<Command>,
    mouse_tx: Sender<MouseState>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Client {
    pub fn start(config: ClientConfig) -> Result<Arc<Self>, String> {
        if let Some(server) = config.server.as_deref() {
            normalize_server_address(server)?;
        } else if config.api.is_none() {
            return Err("no direct server or API tunnel target was supplied".into());
        }
        if config.token.trim().is_empty() {
            return Err("token is empty".into());
        }
        if config.token.len() > MAX_TOKEN_LEN {
            return Err(format!("token exceeds {MAX_TOKEN_LEN} bytes"));
        }

        let (video_tx, video_rx) = crossbeam_channel::bounded(VIDEO_QUEUE_CAPACITY);
        let (command_tx, command_rx) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let (mouse_tx, mouse_rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            status: Mutex::new("connecting".into()),
            stream_event: Mutex::new(None),
            control_state: Mutex::new(ControlState::default()),
            trackpad_router: Mutex::new(TrackpadRouter::default()),
            video_tx,
            video_rx,
            audio_queue: AudioPacketQueue::new(AUDIO_QUEUE_MAX_CAPACITY, 1),
            command_rx,
            mouse_rx,
            accepted_generation: AtomicU32::new(0),
            accepted_video_epoch: AtomicU64::new(0),
            last_stream_generation: AtomicU32::new(0),
            transport_generation: AtomicU32::new(1),
            audio_enabled_requested: AtomicBool::new(false),
        });
        let client = Arc::new(Self {
            shared: Arc::clone(&shared),
            command_tx,
            mouse_tx,
            worker: Mutex::new(None),
        });
        let worker = thread::Builder::new()
            .name("st-client-core".into())
            .spawn(move || run_worker(shared, config))
            .map_err(|error| format!("failed to start client thread: {error}"))?;
        *client.worker.lock().unwrap() = Some(worker);
        Ok(client)
    }

    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared
            .audio_enabled_requested
            .store(false, Ordering::Release);
        clear_audio_queue(&self.shared);
        let _ = self.command_tx.try_send(Command::Stop);
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
        set_status(&self.shared, "disconnected");
        *self.shared.control_state.lock().unwrap() = ControlState::default();
        *self.shared.trackpad_router.lock().unwrap() = TrackpadRouter::default();
    }

    pub fn status(&self) -> String {
        self.shared.status.lock().unwrap().clone()
    }

    pub fn take_stream_event(&self) -> Option<StreamEvent> {
        self.shared.stream_event.lock().unwrap().take()
    }

    pub fn take_control_snapshot(&self) -> Option<ControlSnapshot> {
        let mut control_state = self.shared.control_state.lock().unwrap();
        let transport_generation = self.shared.transport_generation.load(Ordering::Acquire);
        control_state.take_snapshot(transport_generation)
    }

    pub fn recv_access_unit(&self, timeout: Duration) -> Option<AccessUnit> {
        self.shared.video_rx.recv_timeout(timeout).ok()
    }

    pub fn recv_audio_packet(&self, timeout: Duration) -> Option<QueuedAudioPacket> {
        self.shared.audio_queue.recv_timeout(timeout)
    }

    pub fn try_recv_audio_packet(&self) -> Option<QueuedAudioPacket> {
        self.shared.audio_queue.try_recv()
    }

    pub fn audio_backlog(&self) -> usize {
        self.shared.audio_queue.stats().occupancy
    }

    pub fn audio_queue_stats(&self) -> AudioQueueStats {
        self.shared.audio_queue.stats()
    }

    pub fn clear_audio_queue(&self) {
        clear_audio_queue(&self.shared);
    }

    pub fn set_audio_enabled(&self, enabled: bool) -> Result<(), String> {
        if enabled && self.status() != "connected" {
            return Err("audio is unavailable while disconnected".into());
        }
        let previous = self
            .shared
            .audio_enabled_requested
            .swap(enabled, Ordering::AcqRel);
        if !enabled {
            clear_audio_queue(&self.shared);
        }
        if previous == enabled {
            return Ok(());
        }
        if let Err(error) = self.command_tx.try_send(Command::SetAudio(enabled)) {
            self.shared
                .audio_enabled_requested
                .store(previous, Ordering::Release);
            return Err(format!("audio command queue unavailable: {error}"));
        }
        Ok(())
    }

    pub fn send_mouse_absolute(&self, x: u16, y: u16, buttons: u8) -> Result<(), String> {
        if self.status() != "connected" {
            return Err("input is unavailable while disconnected".into());
        }
        let mut router = self.shared.trackpad_router.lock().unwrap();
        if !router.eligibility().allows_absolute() {
            return Err("absolute mouse input is unavailable".into());
        }
        router.note_absolute_input(x, y);
        self.mouse_tx
            .send(MouseState::Absolute { x, y, buttons })
            .map_err(|error| format!("input queue unavailable: {error}"))
    }

    pub fn send_trackpad_delta(
        &self,
        dx: i16,
        dy: i16,
        buttons: u8,
    ) -> Result<TrackpadRoute, String> {
        if self.status() != "connected" {
            return Err("input is unavailable while disconnected".into());
        }
        let mut router = self.shared.trackpad_router.lock().unwrap();
        let (route, mouse) = router
            .route_trackpad_delta(dx, dy, buttons)
            .ok_or_else(|| "trackpad input is unavailable".to_string())?;
        self.mouse_tx
            .send(mouse)
            .map_err(|error| format!("input queue unavailable: {error}"))?;
        Ok(route)
    }

    pub fn send_mouse_buttons(&self, buttons: u8) -> Result<(), String> {
        if self.status() != "connected" {
            return Err("input is unavailable while disconnected".into());
        }
        if !self
            .shared
            .trackpad_router
            .lock()
            .unwrap()
            .eligibility()
            .allows_buttons()
        {
            return Err("mouse buttons are unavailable".into());
        }
        self.mouse_tx
            .send(MouseState::Buttons(buttons))
            .map_err(|error| format!("input queue unavailable: {error}"))
    }

    pub fn send_mouse_wheel(&self, delta_x: i16, delta_y: i16, buttons: u8) -> Result<(), String> {
        if self.status() != "connected" {
            return Err("input is unavailable while disconnected".into());
        }
        if !self
            .shared
            .trackpad_router
            .lock()
            .unwrap()
            .eligibility()
            .allows_buttons()
        {
            return Err("mouse wheel input is unavailable".into());
        }
        self.mouse_tx
            .send(MouseState::Wheel {
                delta_x,
                delta_y,
                buttons,
            })
            .map_err(|error| format!("input queue unavailable: {error}"))
    }

    pub fn send_keyboard_state(&self, pressed: [u8; KEYBOARD_STATE_BYTES]) -> Result<(), String> {
        if self.status() != "connected" {
            return Err("input is unavailable while disconnected".into());
        }
        if !keyboard_state_is_valid(&pressed) {
            return Err("keyboard state contains unsupported keys".into());
        }
        let any_pressed = pressed.iter().any(|byte| *byte != 0);
        if any_pressed
            && !self
                .shared
                .trackpad_router
                .lock()
                .unwrap()
                .eligibility()
                .allows_keyboard()
        {
            return Err("keyboard input is unavailable".into());
        }
        self.mouse_tx
            .send(MouseState::Keyboard(pressed))
            .map_err(|error| format!("input queue unavailable: {error}"))
    }

    pub fn send_text_input(&self, text: String) -> Result<(), String> {
        if self.status() != "connected" {
            return Err("text input is unavailable while disconnected".into());
        }
        validate_text_input(&text)?;
        if !self
            .shared
            .trackpad_router
            .lock()
            .unwrap()
            .eligibility()
            .allows_text_input()
        {
            return Err("committed Unicode text input is unavailable".into());
        }
        self.command_tx
            .try_send(Command::TextInput(text))
            .map_err(|error| format!("text input queue unavailable: {error}"))
    }

    pub fn accept_stream_generation(&self, generation: u32, video_epoch: u64) -> bool {
        let (acknowledgement, accepted) = crossbeam_channel::bounded(1);
        if self
            .command_tx
            .send_timeout(
                Command::AcceptStream {
                    generation,
                    video_epoch,
                    acknowledgement,
                },
                STREAM_ACCEPT_TIMEOUT,
            )
            .is_err()
        {
            return false;
        }
        accepted
            .recv_timeout(STREAM_ACCEPT_TIMEOUT)
            .unwrap_or(false)
    }

    pub fn request_keyframe(&self) {
        let _ = self.command_tx.try_send(Command::RequestKeyframe);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared
            .audio_enabled_requested
            .store(false, Ordering::Release);
        clear_audio_queue(&self.shared);
        let _ = self.command_tx.try_send(Command::Stop);
        if let Some(worker) = self.worker.get_mut().unwrap().take() {
            let _ = worker.join();
        }
    }
}

fn run_worker(shared: Arc<Shared>, config: ClientConfig) {
    let result = run_session(&shared, &config);
    shared
        .audio_enabled_requested
        .store(false, Ordering::Release);
    clear_audio_queue(&shared);
    if shared.stop.load(Ordering::Acquire) {
        set_status(&shared, "disconnected");
    } else if let Err(error) = result {
        set_status(&shared, &format!("error: {error}"));
    } else {
        set_status(&shared, "disconnected");
    }
}

fn run_session(shared: &Arc<Shared>, config: &ClientConfig) -> Result<(), SessionFailure> {
    let mut previous_failure = None;
    if let Some(server) = config.server.as_deref() {
        let direct_result = SessionTransport::connect_direct(server)
            .map_err(|error| SessionFailure::transport(SessionStage::DirectConnect, error))
            .and_then(|mut transport| run_transport_session(shared, config, &mut transport));
        let direct_failure = match direct_result {
            Ok(()) => return Ok(()),
            Err(_error) if should_stop(shared) => return Ok(()),
            Err(error) if error.terminal => return Err(error),
            Err(error) => error,
        };

        if should_try_direct_tcp_tunnel(&direct_failure) {
            reset_for_transport_fallback(shared, "connecting through direct TCP tunnel");
            let tunnel_result = SessionTransport::connect_direct_tunnel(server)
                .map_err(|error| SessionFailure::transport(SessionStage::DirectTcpTunnel, error))
                .and_then(|mut transport| run_transport_session(shared, config, &mut transport));
            match tunnel_result {
                Ok(()) => return Ok(()),
                Err(_error) if should_stop(shared) => return Ok(()),
                Err(error) if error.terminal || config.api.is_none() => return Err(error),
                Err(error) => previous_failure = Some(error),
            }
        } else if config.api.is_none() {
            return Err(direct_failure);
        } else {
            previous_failure = Some(direct_failure);
        }
    }

    let api = config.api.as_ref().ok_or_else(|| {
        previous_failure.unwrap_or_else(|| {
            SessionFailure::transport(
                SessionStage::DirectConnect,
                "API tunnel configuration is missing",
            )
        })
    })?;
    reset_for_transport_fallback(shared, "connecting through API punch");
    let punch_result = crate::api_tunnel::connect_punch(api, &config.token, &shared.stop)
        .map_err(|error| SessionFailure::transport(SessionStage::ApiPunch, error));
    let punch_failure = match punch_result {
        Ok(connection) => {
            let result = SessionTransport::from_tunnel(connection.link())
                .map_err(|error| SessionFailure::transport(SessionStage::ApiPunch, error))
                .and_then(|mut transport| run_transport_session(shared, config, &mut transport));
            connection.close(api, &config.token);
            match result {
                Ok(()) => return Ok(()),
                Err(_error) if should_stop(shared) => return Ok(()),
                Err(error) if !should_fallback_from_punch(&error) => return Err(error),
                Err(error) => error,
            }
        }
        Err(error) => error,
    };

    reset_for_transport_fallback(shared, "connecting through API relay");
    let connection =
        crate::api_tunnel::connect_relay(api, &config.token, &shared.stop).map_err(|error| {
            SessionFailure::transport(
                SessionStage::ApiRelay,
                format!("after {punch_failure}; {error}"),
            )
        })?;
    let result = SessionTransport::from_tunnel(connection.link())
        .map_err(|error| SessionFailure::transport(SessionStage::ApiRelay, error))
        .and_then(|mut transport| run_transport_session(shared, config, &mut transport));
    connection.close(api, &config.token);
    result
}

fn should_try_direct_tcp_tunnel(error: &SessionFailure) -> bool {
    error.stage == SessionStage::Connected && error.message == "media path timed out"
}

fn should_fallback_from_punch(error: &SessionFailure) -> bool {
    !error.terminal
        && matches!(
            error.stage,
            SessionStage::ApiPunch | SessionStage::Authentication | SessionStage::Startup
        )
}

fn reset_for_transport_fallback(shared: &Shared, status: &str) {
    set_status(shared, status);
    *shared.stream_event.lock().unwrap() = None;
    {
        let mut control_state = shared.control_state.lock().unwrap();
        next_transport_generation(shared);
        *control_state = ControlState::default();
    }
    *shared.trackpad_router.lock().unwrap() = TrackpadRouter::default();
    shared.accepted_generation.store(0, Ordering::Release);
    shared.accepted_video_epoch.store(0, Ordering::Release);
    clear_video_queue(shared);
    clear_audio_queue(shared);
}

fn run_transport_session(
    shared: &Arc<Shared>,
    config: &ClientConfig,
    transport: &mut SessionTransport,
) -> Result<(), SessionFailure> {
    let result = run_transport_session_inner(shared, config, transport);
    graceful_release(transport);
    result
}

fn run_transport_session_inner(
    shared: &Arc<Shared>,
    config: &ClientConfig,
    transport: &mut SessionTransport,
) -> Result<(), SessionFailure> {
    transport
        .send_control(ControlMessage::Authenticate(config.token.clone()))
        .map_err(|error| SessionFailure::transport(SessionStage::Authentication, error))?;
    let mut deferred = VecDeque::new();
    authenticate(transport, &mut deferred, shared)?;

    let codecs = VideoCodecSupport::h264_only();
    transport
        .send_control(ControlMessage::ClientDisplayInfo(ClientDisplayInfo {
            max_refresh_millihz: config.refresh_millihz,
            udp_port: transport.udp_port(),
            supported_video_codecs: codecs,
            hardware_video_codecs: codecs,
            supported_yuv444_video_codecs: VideoCodecSupport::empty(),
            hardware_yuv444_video_codecs: VideoCodecSupport::empty(),
            hdr_display: false,
        }))
        .map_err(|error| SessionFailure::transport(SessionStage::Startup, error))?;

    let mut session = SessionState::new(transport.peer().ip());
    let mut input_seq = 0u16;
    let startup_deadline = Instant::now() + STARTUP_TIMEOUT;
    while !session.stream_started {
        if should_stop(shared) {
            return Ok(());
        }
        if Instant::now() >= startup_deadline {
            return Err(SessionFailure::transport(
                SessionStage::Startup,
                "timed out waiting for the stream to start",
            ));
        }

        let messages = if deferred.is_empty() {
            transport
                .poll_control()
                .map_err(|error| SessionFailure::transport(SessionStage::Startup, error))?
        } else {
            deferred.drain(..).collect()
        };
        for message in messages {
            handle_control_message(message, &mut session, shared)
                .map_err(|error| SessionFailure::classified(SessionStage::Startup, error))?;
        }
        if session.stream_config.is_some()
            && session.client_id.is_some()
            && session.input_credential.is_some()
            && !session.media_ready_sent
        {
            send_media_bootstrap(transport, &session, &mut input_seq);
            transport
                .send_control(ControlMessage::ClientReadyForMedia)
                .map_err(|error| SessionFailure::transport(SessionStage::Startup, error))?;
            session.media_ready_sent = true;
        }
        drain_media(transport, &mut session, shared)
            .map_err(|error| SessionFailure::classified(SessionStage::Startup, error))?;
        transport.pace();
    }

    transport
        .send_control(ControlMessage::SetAudio(
            shared.audio_enabled_requested.load(Ordering::Acquire),
        ))
        .map_err(|error| SessionFailure::transport(SessionStage::Startup, error))?;
    send_media_bootstrap(transport, &session, &mut input_seq);
    set_status(shared, "connected");
    let mut next_ping = Instant::now();

    loop {
        if should_stop(shared) {
            return Ok(());
        }

        for message in transport
            .poll_control()
            .map_err(|error| SessionFailure::transport(SessionStage::Connected, error))?
        {
            handle_control_message(message, &mut session, shared)
                .map_err(|error| SessionFailure::classified(SessionStage::Connected, error))?;
        }
        drain_commands(shared, transport, &mut session, &mut input_seq)
            .map_err(|error| SessionFailure::classified(SessionStage::Connected, error))?;
        send_mouse_heartbeat(transport, &mut session, &mut input_seq);
        send_keyboard_heartbeat(transport, &mut session, &mut input_seq);
        drain_media(transport, &mut session, shared)
            .map_err(|error| SessionFailure::classified(SessionStage::Connected, error))?;

        if session.last_media_at.elapsed() > MEDIA_STALL_TIMEOUT {
            return Err(SessionFailure::transport(
                SessionStage::Connected,
                "media path timed out",
            ));
        }
        if Instant::now() >= next_ping {
            transport
                .send_control(ControlMessage::ClockSyncPing(ClockSyncPing {
                    client_send_micros: unix_time_micros(),
                }))
                .map_err(|error| SessionFailure::transport(SessionStage::Connected, error))?;
            next_ping = Instant::now() + Duration::from_secs(2);
        }
        transport.pace();
    }
}

fn authenticate(
    transport: &mut SessionTransport,
    deferred: &mut VecDeque<ControlMessage>,
    shared: &Shared,
) -> Result<(), SessionFailure> {
    let deadline = Instant::now() + AUTH_TIMEOUT;
    while Instant::now() < deadline {
        if should_stop(shared) {
            return Ok(());
        }
        let messages = transport
            .poll_control()
            .map_err(|error| SessionFailure::transport(SessionStage::Authentication, error))?;
        let mut authenticated = false;
        for message in messages {
            match message {
                ControlMessage::AuthResult(true) => authenticated = true,
                ControlMessage::AuthResult(false) => {
                    return Err(SessionFailure::classified(
                        SessionStage::Authentication,
                        "authentication failed; check the token",
                    ))
                }
                ControlMessage::Error(error) => {
                    return Err(SessionFailure::classified(
                        SessionStage::Authentication,
                        format!("server error: {error}"),
                    ))
                }
                other => deferred.push_back(other),
            }
        }
        if authenticated {
            return Ok(());
        }
        transport.pace();
    }
    Err(SessionFailure::transport(
        SessionStage::Authentication,
        "authentication timed out",
    ))
}

enum SessionTransport {
    Direct {
        tcp: TcpStream,
        udp: UdpSocket,
        server_addr: SocketAddr,
        control_buf: Vec<u8>,
    },
    Tunnel {
        link: Arc<dyn TunnelLink>,
        control_buf: Vec<u8>,
        pending_media: VecDeque<Vec<u8>>,
    },
}

impl SessionTransport {
    fn connect_direct(server: &str) -> Result<Self, String> {
        let (tcp, server_addr) = Self::connect_tcp(server)?;
        tcp.set_nodelay(true)
            .map_err(|error| format!("failed to enable TCP_NODELAY: {error}"))?;
        tcp.set_nonblocking(true)
            .map_err(|error| format!("failed to configure control socket: {error}"))?;

        let udp_bind = if server_addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let udp = UdpSocket::bind(udp_bind)
            .map_err(|error| format!("failed to bind UDP media socket: {error}"))?;
        tune_udp_recv_buffer(&udp, 4 * 1024 * 1024);
        udp.set_nonblocking(true)
            .map_err(|error| format!("failed to configure UDP media socket: {error}"))?;
        Ok(Self::Direct {
            tcp,
            udp,
            server_addr,
            control_buf: Vec::new(),
        })
    }

    fn connect_direct_tunnel(server: &str) -> Result<Self, String> {
        let (mut tcp, _) = Self::connect_tcp(server)?;
        tcp.set_nodelay(true)
            .map_err(|error| format!("failed to enable TCP_NODELAY: {error}"))?;
        tcp.write_all(TCP_TUNNEL_PREAMBLE)
            .map_err(|error| format!("failed to write TCP tunnel preamble: {error}"))?;
        let tunnel = TcpTunnel::new(tcp, None, Vec::new())?;
        Self::from_tunnel(Arc::new(tunnel))
    }

    fn connect_tcp(server: &str) -> Result<(TcpStream, SocketAddr), String> {
        let endpoint = normalize_server_address(server)?;
        let addresses: Vec<_> = endpoint
            .to_socket_addrs()
            .map_err(|error| format!("cannot resolve {endpoint}: {error}"))?
            .collect();
        if addresses.is_empty() {
            return Err(format!("{endpoint} did not resolve to an address"));
        }
        let mut last_error = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
                Ok(stream) => return Ok((stream, address)),
                Err(error) => last_error = Some(format!("{address}: {error}")),
            }
        }
        Err(format!(
            "connection failed: {}",
            last_error.unwrap_or_else(|| "no compatible address".into())
        ))
    }

    fn from_tunnel(link: Arc<dyn TunnelLink>) -> Result<Self, String> {
        link.set_read_timeout(None)?;
        link.set_nonblocking(true)?;
        Ok(Self::Tunnel {
            link,
            control_buf: Vec::new(),
            pending_media: VecDeque::new(),
        })
    }

    fn peer(&self) -> SocketAddr {
        match self {
            Self::Direct { server_addr, .. } => *server_addr,
            Self::Tunnel { link, .. } => link.peer(),
        }
    }

    fn udp_port(&self) -> u16 {
        match self {
            Self::Direct { udp, .. } => udp.local_addr().map_or(0, |address| address.port()),
            Self::Tunnel { .. } => 0,
        }
    }

    fn send_control(&mut self, message: ControlMessage) -> Result<(), String> {
        let bytes = message.serialize();
        match self {
            Self::Direct { tcp, .. } => write_control_nonblocking(tcp, &bytes),
            Self::Tunnel { link, .. } => link.send_control(&bytes),
        }
    }

    fn send_input(&self, bytes: &[u8]) {
        match self {
            Self::Direct {
                udp, server_addr, ..
            } => {
                let _ = udp.send_to(bytes, server_addr);
            }
            Self::Tunnel { link, .. } => {
                let _ = link.send_media(bytes);
            }
        }
    }

    fn poll_control(&mut self) -> Result<Vec<ControlMessage>, String> {
        match self {
            Self::Direct {
                tcp, control_buf, ..
            } => read_control_messages(tcp, control_buf),
            Self::Tunnel {
                link,
                control_buf,
                pending_media,
            } => {
                link.tick();
                let mut controls = Vec::new();
                for _ in 0..128 {
                    let messages = link.try_recv_all();
                    if messages.is_empty() {
                        break;
                    }
                    for message in messages {
                        match message {
                            PunchedMessage::Media(packet) => pending_media.push_back(packet),
                            PunchedMessage::Control(bytes) => {
                                control_buf.extend_from_slice(&bytes);
                                controls.extend(drain_control_messages(control_buf));
                            }
                        }
                    }
                }
                if link.is_closed() {
                    return Err("tunnel closed".into());
                }
                Ok(controls)
            }
        }
    }

    fn pace(&self) {
        thread::sleep(Duration::from_millis(1));
    }

    fn flush_control(&self, timeout: Duration) {
        if let Self::Tunnel { link, .. } = self {
            let _ = link.flush_control(timeout);
        }
    }
}

struct TrackpadRouter {
    stream_extent: Option<(u32, u32)>,
    capabilities: InputCapabilities,
    controller_state: ControllerState,
    latest_cursor_shape: Option<CursorShape>,
    latest_cursor_state: Option<CursorState>,
    latest_cursor_state_extent: Option<(u32, u32)>,
    absolute_target: Option<(f64, f64)>,
    local_absolute_control: bool,
}

impl Default for TrackpadRouter {
    fn default() -> Self {
        Self {
            stream_extent: None,
            capabilities: InputCapabilities::default(),
            controller_state: ControllerState::Unavailable,
            latest_cursor_shape: None,
            latest_cursor_state: None,
            latest_cursor_state_extent: None,
            absolute_target: None,
            local_absolute_control: false,
        }
    }
}

impl TrackpadRouter {
    fn eligibility(&self) -> InputEligibility {
        InputEligibility {
            capabilities: self.capabilities,
            controller_state: self.controller_state,
        }
    }

    fn set_stream_config(&mut self, config: StreamConfig) {
        let extent = (config.width, config.height);
        if let Some(previous) = self.stream_extent {
            if previous != extent {
                self.absolute_target = self.absolute_target.and_then(|(x, y)| {
                    remap_cursor_axis(x, previous.0, extent.0)
                        .zip(remap_cursor_axis(y, previous.1, extent.1))
                });
            }
        }
        self.stream_extent = Some(extent);
        self.seed_absolute_target();
    }

    fn set_capabilities(&mut self, capabilities: InputCapabilities) {
        let was_eligible = self.eligibility().allows_trackpad_delta();
        let was_absolute_mode = self.absolute_mode();
        self.capabilities = capabilities;
        self.finish_mode_update(was_eligible, was_absolute_mode);
    }

    fn set_controller_state(&mut self, controller_state: ControllerState) {
        let was_eligible = self.eligibility().allows_trackpad_delta();
        let was_absolute_mode = self.absolute_mode();
        let ownership_moved_away = self.controller_state != ControllerState::OwnedByOther
            && controller_state == ControllerState::OwnedByOther;
        self.controller_state = controller_state;
        self.finish_mode_update(was_eligible, was_absolute_mode);
        if ownership_moved_away && self.capabilities.cursor_position_reliable {
            self.reset_absolute_target();
            self.seed_absolute_target();
        }
    }

    fn set_cursor_shape(&mut self, shape: CursorShape) {
        self.latest_cursor_shape = Some(shape);
        self.seed_absolute_target();
    }

    fn set_cursor_state(&mut self, state: CursorState) {
        let was_drawable = self.cursor_is_drawable();
        self.latest_cursor_state = Some(state);
        self.latest_cursor_state_extent = self.stream_extent;
        let drawable = self.cursor_is_drawable();
        if was_drawable != drawable {
            self.reset_absolute_target();
        }
        self.seed_absolute_target();
    }

    fn note_absolute_input(&mut self, x: u16, y: u16) {
        let Some((width, height)) = self.stream_extent else {
            return;
        };
        self.absolute_target = Some((
            denormalize_cursor_coord(x, width),
            denormalize_cursor_coord(y, height),
        ));
        self.local_absolute_control = true;
    }

    fn route_trackpad_delta(
        &mut self,
        dx: i16,
        dy: i16,
        buttons: u8,
    ) -> Option<(TrackpadRoute, MouseState)> {
        if !self.eligibility().allows_trackpad_delta() {
            return None;
        }
        if self.absolute_mode() {
            let extent = self.stream_extent?;
            self.seed_absolute_target();
            let (x, y) = self.absolute_target.get_or_insert_with(|| {
                (
                    cursor_extent_center(extent.0),
                    cursor_extent_center(extent.1),
                )
            });
            *x = (*x + dx as f64).clamp(0.0, cursor_extent_max(extent.0));
            *y = (*y + dy as f64).clamp(0.0, cursor_extent_max(extent.1));
            let x = normalize_cursor_coord(*x, extent.0);
            let y = normalize_cursor_coord(*y, extent.1);
            self.local_absolute_control = true;
            return Some((
                TrackpadRoute::Absolute { x, y },
                MouseState::Absolute { x, y, buttons },
            ));
        }
        self.capabilities.mouse_relative.then_some((
            TrackpadRoute::Relative,
            MouseState::Relative { dx, dy, buttons },
        ))
    }

    fn finish_mode_update(&mut self, was_eligible: bool, was_absolute_mode: bool) {
        let eligible = self.eligibility().allows_trackpad_delta();
        let absolute_mode = self.absolute_mode();
        if !eligible || !was_eligible || was_absolute_mode != absolute_mode {
            self.reset_absolute_target();
        }
        self.seed_absolute_target();
    }

    fn reset_absolute_target(&mut self) {
        self.absolute_target = None;
        self.local_absolute_control = false;
    }

    fn seed_absolute_target(&mut self) {
        if self.local_absolute_control || !self.absolute_mode() {
            return;
        }
        let extent = self.stream_extent.unwrap();
        self.absolute_target = self.reliable_cursor_tip(extent).or_else(|| {
            Some((
                cursor_extent_center(extent.0),
                cursor_extent_center(extent.1),
            ))
        });
    }

    fn reliable_cursor_tip(&self, target_extent: (u32, u32)) -> Option<(f64, f64)> {
        if !self.capabilities.cursor_position_reliable {
            return None;
        }
        let state = self.latest_cursor_state?;
        let shape = self
            .latest_cursor_shape
            .as_ref()
            .filter(|shape| shape.serial == state.serial)?;
        let source_extent = self.latest_cursor_state_extent.unwrap_or(target_extent);
        let x = state.x as f64 + shape.hotspot_x as f64;
        let y = state.y as f64 + shape.hotspot_y as f64;
        Some((
            remap_cursor_axis_or_clamp(x, source_extent.0, target_extent.0),
            remap_cursor_axis_or_clamp(y, source_extent.1, target_extent.1),
        ))
    }

    fn cursor_is_drawable(&self) -> bool {
        self.latest_cursor_state
            .is_some_and(|state| state.visible && !state.app_grab)
    }

    fn absolute_mode(&self) -> bool {
        self.eligibility().allows_absolute()
            && self.capabilities.separate_cursor
            && self.cursor_is_drawable()
            && self.stream_extent.is_some()
    }
}

struct SessionState {
    server_ip: IpAddr,
    demux: MediaDemux,
    stream_config: Option<StreamConfig>,
    stream_started: bool,
    media_ready_sent: bool,
    generation: u32,
    client_id: Option<u32>,
    input_credential: Option<InputCredential>,
    input_capabilities: InputCapabilities,
    controller_state: ControllerState,
    last_media_at: Instant,
    last_frame_id: Option<u32>,
    waiting_for_recovery: bool,
    last_keyframe_request: Instant,
    rtt_ms: u32,
    mouse_heartbeat: Option<MouseHeartbeat>,
    keyboard_heartbeat: KeyboardHeartbeat,
}

impl SessionState {
    fn new(server_ip: IpAddr) -> Self {
        Self {
            server_ip,
            demux: MediaDemux::default(),
            stream_config: None,
            stream_started: false,
            media_ready_sent: false,
            generation: 0,
            client_id: None,
            input_credential: None,
            input_capabilities: InputCapabilities::default(),
            controller_state: ControllerState::Unavailable,
            last_media_at: Instant::now(),
            last_frame_id: None,
            waiting_for_recovery: true,
            last_keyframe_request: Instant::now() - KEYFRAME_REQUEST_INTERVAL,
            rtt_ms: 0,
            mouse_heartbeat: None,
            keyboard_heartbeat: KeyboardHeartbeat::default(),
        }
    }

    fn set_stream_config(&mut self, config: StreamConfig) {
        self.stream_config = Some(config);
    }

    fn input_eligibility(&self) -> InputEligibility {
        InputEligibility {
            capabilities: self.input_capabilities,
            controller_state: self.controller_state,
        }
    }

    fn clear_unsupported_input_heartbeats(&mut self) {
        let eligibility = self.input_eligibility();
        if self
            .mouse_heartbeat
            .is_some_and(|heartbeat| !heartbeat.is_supported(eligibility))
        {
            self.mouse_heartbeat = None;
        }
        if !eligibility.allows_keyboard() {
            self.keyboard_heartbeat = KeyboardHeartbeat::default();
        }
    }
}

fn handle_control_message(
    message: ControlMessage,
    session: &mut SessionState,
    shared: &Shared,
) -> Result<(), String> {
    match message {
        ControlMessage::StreamConfig(config) => {
            validate_stream_config(config)?;
            let video_epoch_changed = session
                .stream_config
                .is_none_or(|previous| previous.video_epoch != config.video_epoch);
            let decoder_changed = session
                .stream_config
                .is_none_or(|previous| !decoder_configs_compatible(previous, config));
            let audio_changed = session
                .stream_config
                .is_none_or(|previous| !audio_configs_compatible(previous, config));
            session.set_stream_config(config);
            shared
                .trackpad_router
                .lock()
                .unwrap()
                .set_stream_config(config);
            if audio_changed {
                clear_audio_queue(shared);
            }
            shared
                .audio_queue
                .set_limit(audio_queue_limit(config.packet_duration_ms));
            if decoder_changed {
                session.generation = next_stream_generation(shared);
            }
            if decoder_changed || video_epoch_changed {
                session.last_frame_id = None;
                session.waiting_for_recovery = true;
                session.demux.reset_video();
                clear_video_queue(shared);
                shared.accepted_video_epoch.store(0, Ordering::Release);
            }
            *shared.stream_event.lock().unwrap() = Some(StreamEvent {
                transport_generation: shared.transport_generation.load(Ordering::Acquire),
                generation: session.generation,
                video_epoch: config.video_epoch,
                width: config.width,
                height: config.height,
                // CursorState uses these same stream-space dimensions today,
                // but presentation needs the two coordinate spaces explicit.
                cursor_width: config.width,
                cursor_height: config.height,
                framerate: config.framerate,
                audio_sample_rate: config.audio_sample_rate,
                audio_channels: config.audio_channels,
                packet_duration_ms: config.packet_duration_ms,
            });
        }
        ControlMessage::StreamStarted => {
            if session.stream_config.is_none() || !session.media_ready_sent {
                return Err("server started media before sending configuration".into());
            }
            session.stream_started = true;
            session.last_media_at = Instant::now();
        }
        ControlMessage::InputSession(input) => {
            session.client_id = Some(input.client_id);
            session.input_credential = Some(input.credential);
        }
        ControlMessage::InputCapabilities(capabilities) => {
            session.input_capabilities = capabilities;
            shared
                .trackpad_router
                .lock()
                .unwrap()
                .set_capabilities(capabilities);
            session.clear_unsupported_input_heartbeats();
            shared
                .control_state
                .lock()
                .unwrap()
                .input_capabilities
                .set(capabilities);
        }
        ControlMessage::ControllerState(state) => {
            session.controller_state = state;
            shared
                .trackpad_router
                .lock()
                .unwrap()
                .set_controller_state(state);
            session.clear_unsupported_input_heartbeats();
            shared
                .control_state
                .lock()
                .unwrap()
                .controller_state
                .set(state);
        }
        ControlMessage::CursorShape(shape) => {
            shared
                .trackpad_router
                .lock()
                .unwrap()
                .set_cursor_shape(shape.clone());
            shared.control_state.lock().unwrap().cursor_shape.set(shape);
        }
        ControlMessage::CursorState(cursor) => {
            shared
                .trackpad_router
                .lock()
                .unwrap()
                .set_cursor_state(cursor);
            shared
                .control_state
                .lock()
                .unwrap()
                .cursor_state
                .set(cursor);
        }
        ControlMessage::ClockSyncPong(pong) => {
            let now = unix_time_micros();
            let round_trip = now.saturating_sub(pong.client_send_micros);
            let server_dwell = pong
                .server_send_micros
                .saturating_sub(pong.server_recv_micros);
            session.rtt_ms = round_trip
                .saturating_sub(server_dwell)
                .saturating_div(1_000)
                .min(u32::MAX as u64) as u32;
        }
        ControlMessage::Error(error) => return Err(format!("server error: {error}")),
        ControlMessage::Shutdown => return Err("server is shutting down".into()),
        _ => {}
    }
    Ok(())
}

fn validate_stream_config(config: StreamConfig) -> Result<(), String> {
    if config.video_epoch == 0 {
        return Err("server supplied an invalid video epoch".into());
    }
    if config.codec != VideoCodec::H264 {
        return Err(format!(
            "server selected unsupported codec {:?}; Android MVP supports H.264 only",
            config.codec
        ));
    }
    if config.chroma != VideoChromaSampling::Yuv420 || config.hdr {
        return Err("Android MVP supports SDR YUV420 streams only".into());
    }
    if config.width == 0 || config.height == 0 {
        return Err("server supplied an invalid stream size".into());
    }
    Ok(())
}

fn decoder_configs_compatible(previous: StreamConfig, next: StreamConfig) -> bool {
    previous.codec == next.codec
        && previous.width == next.width
        && previous.height == next.height
        && previous.hdr == next.hdr
        && previous.chroma == next.chroma
}

fn audio_configs_compatible(previous: StreamConfig, next: StreamConfig) -> bool {
    previous.audio_sample_rate == next.audio_sample_rate
        && previous.audio_channels == next.audio_channels
        && previous.packet_duration_ms == next.packet_duration_ms
}

fn drain_media(
    transport: &mut SessionTransport,
    session: &mut SessionState,
    shared: &Shared,
) -> Result<(), String> {
    let mut request_recovery = false;
    match transport {
        SessionTransport::Direct { udp, .. } => {
            let mut buffer = [0u8; MAX_UDP_DATAGRAM_SIZE];
            for _ in 0..128 {
                match udp.recv_from(&mut buffer) {
                    Ok((size, source)) => {
                        if source.ip() != session.server_ip {
                            continue;
                        }
                        request_recovery |=
                            process_media_packet(&buffer[..size], Some(source), session, shared)?;
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => return Err(format!("UDP receive failed: {error}")),
                }
            }
        }
        SessionTransport::Tunnel { pending_media, .. } => {
            while let Some(packet) = pending_media.pop_front() {
                request_recovery |= process_media_packet(&packet, None, session, shared)?;
            }
        }
    }

    if let Some(stats) = session.demux.take_stats() {
        if stats.needs_recovery_keyframe() {
            session.waiting_for_recovery = true;
            request_recovery = true;
        }
        let mut feedback = stats.feedback();
        feedback.rtt_ms = session.rtt_ms;
        transport.send_control(ControlMessage::TransportFeedback(feedback))?;
    }
    if request_recovery {
        request_keyframe(transport, session)?;
    }
    Ok(())
}

fn process_media_packet(
    packet: &[u8],
    source: Option<SocketAddr>,
    session: &mut SessionState,
    shared: &Shared,
) -> Result<bool, String> {
    let Some(data) = session.demux.process_packet(packet, source) else {
        return Ok(false);
    };
    session.last_media_at = Instant::now();
    match data {
        ReceivedData::Video(frame, _, _) => queue_video_frame(frame, session, shared),
        ReceivedData::Audio(packet) if shared.audio_enabled_requested.load(Ordering::Acquire) => {
            queue_audio_packet(packet, shared);
            Ok(false)
        }
        ReceivedData::Audio(_) | ReceivedData::Keepalive => Ok(false),
    }
}

fn queue_video_frame(
    frame: st_protocol::CompletedFrame,
    session: &mut SessionState,
    shared: &Shared,
) -> Result<bool, String> {
    if shared.accepted_generation.load(Ordering::Acquire) != session.generation
        || shared.accepted_video_epoch.load(Ordering::Acquire) != frame.video_epoch
        || session
            .stream_config
            .is_none_or(|config| config.video_epoch != frame.video_epoch)
    {
        return Ok(false);
    }
    let recovery = matches!(frame.frame_type, frame_type::IDR | frame_type::RECOVERY);
    if let Some(last) = session.last_frame_id {
        let distance = frame.frame_id.wrapping_sub(last);
        if distance == 0 || distance >= 0x8000_0000 {
            return Ok(false);
        }
        if distance > 1 && distance < 0x8000_0000 {
            session.waiting_for_recovery = true;
        }
    }
    session.last_frame_id = Some(frame.frame_id);

    if session.waiting_for_recovery && !recovery {
        return Ok(true);
    }
    if recovery {
        session.waiting_for_recovery = false;
    }

    let unit = AccessUnit {
        frame_id: frame.frame_id,
        frame_type: frame.frame_type,
        data: frame.data,
    };
    match shared.video_tx.try_send(unit) {
        Ok(()) => Ok(false),
        Err(TrySendError::Full(_)) => {
            clear_video_queue(shared);
            session.demux.record_consumer_queue_overflow();
            session.waiting_for_recovery = true;
            session.last_frame_id = None;
            Ok(true)
        }
        Err(TrySendError::Disconnected(_)) => Err("video consumer disconnected".into()),
    }
}

fn drain_commands(
    shared: &Shared,
    transport: &mut SessionTransport,
    session: &mut SessionState,
    input_seq: &mut u16,
) -> Result<(), String> {
    while let Ok(mouse) = shared.mouse_rx.try_recv() {
        let Some((client_id, credential)) = session.client_id.zip(session.input_credential) else {
            continue;
        };
        let (packet, buttons) = match mouse {
            MouseState::Absolute { x, y, buttons } => (
                InputPacket::MouseAbsolute(MouseAbsoluteInput {
                    client_id,
                    x,
                    y,
                    buttons,
                }),
                buttons,
            ),
            MouseState::Relative { dx, dy, buttons } => (
                InputPacket::MouseRelative(MouseRelativeInput {
                    client_id,
                    dx,
                    dy,
                    buttons,
                }),
                buttons,
            ),
            MouseState::Wheel {
                delta_x,
                delta_y,
                buttons,
            } => (
                InputPacket::MouseWheel(MouseWheelInput {
                    client_id,
                    delta_x,
                    delta_y,
                    buttons,
                }),
                buttons,
            ),
            MouseState::Keyboard(pressed) => (
                InputPacket::KeyboardState(KeyboardStateInput { client_id, pressed }),
                0,
            ),
            MouseState::Buttons(buttons) => (
                InputPacket::MouseButtons(MouseButtonsInput { client_id, buttons }),
                buttons,
            ),
        };
        send_input(transport, packet, credential, input_seq);
        if matches!(packet, InputPacket::KeyboardState(_)) {
            session.keyboard_heartbeat.observe(packet);
            continue;
        }
        let heartbeat = MouseHeartbeat::new(packet, buttons);
        session.mouse_heartbeat = heartbeat
            .is_supported(session.input_eligibility())
            .then_some(heartbeat);
    }

    loop {
        match shared.command_rx.try_recv() {
            Ok(Command::AcceptStream {
                generation,
                video_epoch,
                acknowledgement,
            }) => {
                if generation != session.generation
                    || session
                        .stream_config
                        .is_none_or(|config| config.video_epoch != video_epoch)
                {
                    let _ = acknowledgement.try_send(false);
                    continue;
                }
                shared
                    .accepted_generation
                    .store(generation, Ordering::Release);
                shared
                    .accepted_video_epoch
                    .store(video_epoch, Ordering::Release);
                clear_video_queue(shared);
                session.waiting_for_recovery = true;
                session.last_frame_id = None;
                session.last_keyframe_request = Instant::now() - KEYFRAME_REQUEST_INTERVAL;
                let accepted = request_keyframe(transport, session).is_ok();
                let _ = acknowledgement.try_send(accepted);
                if !accepted {
                    return Err("failed to request the accepted stream keyframe".into());
                }
            }
            Ok(Command::RequestKeyframe) => {
                session.waiting_for_recovery = true;
                clear_video_queue(shared);
                request_keyframe(transport, session)?;
            }
            Ok(Command::SetAudio(enabled)) => {
                if !enabled {
                    clear_audio_queue(shared);
                }
                transport.send_control(ControlMessage::SetAudio(enabled))?;
            }
            Ok(Command::TextInput(text)) => {
                transport.send_control(ControlMessage::TextInput(text))?;
            }
            Ok(Command::Stop) | Err(TryRecvError::Disconnected) => return Ok(()),
            Err(TryRecvError::Empty) => return Ok(()),
        }
    }
}

#[derive(Clone, Copy)]
struct MouseHeartbeat {
    packet: InputPacket,
    buttons: u8,
    last_sent: Instant,
    resend_until: Option<Instant>,
}

impl MouseHeartbeat {
    fn new(packet: InputPacket, buttons: u8) -> Self {
        let packet = match packet {
            InputPacket::MouseAbsolute(absolute) => InputPacket::MouseButtons(MouseButtonsInput {
                client_id: absolute.client_id,
                buttons,
            }),
            InputPacket::MouseRelative(relative) => InputPacket::MouseButtons(MouseButtonsInput {
                client_id: relative.client_id,
                buttons,
            }),
            InputPacket::MouseWheel(wheel) => InputPacket::MouseButtons(MouseButtonsInput {
                client_id: wheel.client_id,
                buttons,
            }),
            packet => packet,
        };
        Self {
            packet,
            buttons,
            last_sent: Instant::now(),
            resend_until: (buttons == 0).then(|| Instant::now() + INPUT_REPAIR_WINDOW),
        }
    }

    fn is_supported(self, eligibility: InputEligibility) -> bool {
        match self.packet {
            InputPacket::MouseAbsolute(_) => eligibility.allows_absolute(),
            InputPacket::MouseButtons(_) => eligibility.allows_buttons(),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct KeyboardHeartbeat {
    packet: Option<InputPacket>,
    any_pressed: bool,
    last_sent: Option<Instant>,
    resend_until: Option<Instant>,
}

impl KeyboardHeartbeat {
    fn observe(&mut self, packet: InputPacket) {
        let InputPacket::KeyboardState(state) = packet else {
            return;
        };
        let now = Instant::now();
        if self.packet != Some(packet) {
            self.resend_until = Some(now + INPUT_REPAIR_WINDOW);
        }
        self.packet = Some(packet);
        self.any_pressed = state.pressed.iter().any(|byte| *byte != 0);
        self.last_sent = Some(now);
    }

    fn due_packet(&mut self, now: Instant, eligibility: InputEligibility) -> Option<InputPacket> {
        if !eligibility.allows_keyboard() {
            *self = Self::default();
            return None;
        }
        let packet = self.packet?;
        let repair_active = self.resend_until.is_some_and(|deadline| now < deadline);
        if !self.any_pressed && !repair_active {
            self.resend_until = None;
            return None;
        }
        if self
            .last_sent
            .is_some_and(|last| now.duration_since(last) < INPUT_HEARTBEAT_INTERVAL)
        {
            return None;
        }
        self.last_sent = Some(now);
        Some(packet)
    }
}

fn send_mouse_heartbeat(
    transport: &SessionTransport,
    session: &mut SessionState,
    input_seq: &mut u16,
) {
    let eligibility = session.input_eligibility();
    let Some(heartbeat) = session.mouse_heartbeat.as_mut() else {
        return;
    };
    let Some(credential) = session.input_credential else {
        return;
    };
    if !heartbeat.is_supported(eligibility) {
        session.mouse_heartbeat = None;
        return;
    }
    let now = Instant::now();
    let active = heartbeat.buttons != 0
        || heartbeat
            .resend_until
            .is_some_and(|deadline| now < deadline);
    if !active || now.duration_since(heartbeat.last_sent) < INPUT_HEARTBEAT_INTERVAL {
        return;
    }
    send_input(transport, heartbeat.packet, credential, input_seq);
    heartbeat.last_sent = now;
}

fn send_keyboard_heartbeat(
    transport: &SessionTransport,
    session: &mut SessionState,
    input_seq: &mut u16,
) {
    let eligibility = session.input_eligibility();
    if let Some(packet) = session
        .keyboard_heartbeat
        .due_packet(Instant::now(), eligibility)
    {
        let Some(credential) = session.input_credential else {
            return;
        };
        send_input(transport, packet, credential, input_seq);
    }
}

fn send_input(
    transport: &SessionTransport,
    packet: InputPacket,
    credential: InputCredential,
    input_seq: &mut u16,
) {
    let bytes = packet.serialize(*input_seq, credential);
    *input_seq = input_seq.wrapping_add(1);
    transport.send_input(&bytes);
}

fn send_media_bootstrap(transport: &SessionTransport, session: &SessionState, input_seq: &mut u16) {
    if let Some((client_id, credential)) = session.client_id.zip(session.input_credential) {
        send_input(
            transport,
            InputPacket::MouseButtons(MouseButtonsInput {
                client_id,
                buttons: 0,
            }),
            credential,
            input_seq,
        );
        send_input(
            transport,
            InputPacket::KeyboardState(KeyboardStateInput {
                client_id,
                pressed: [0; KEYBOARD_STATE_BYTES],
            }),
            credential,
            input_seq,
        );
    }
}

fn keyboard_state_is_valid(pressed: &[u8; KEYBOARD_STATE_BYTES]) -> bool {
    (KeyboardKey::COUNT..KEYBOARD_STATE_BYTES * 8).all(|index| {
        let byte = index / 8;
        let bit = 1 << (index % 8);
        pressed[byte] & bit == 0
    })
}

fn validate_text_input(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("committed text is empty".into());
    }
    if text.len() > MAX_TEXT_INPUT_BYTES {
        return Err(format!(
            "committed text exceeds {MAX_TEXT_INPUT_BYTES} UTF-8 bytes"
        ));
    }
    if text.contains('\0') {
        return Err("committed text contains NUL".into());
    }
    Ok(())
}

fn graceful_release(transport: &mut SessionTransport) {
    let _ = transport.send_control(ControlMessage::ReleaseControl);
    transport.flush_control(Duration::from_millis(250));
}

fn request_keyframe(
    transport: &mut SessionTransport,
    session: &mut SessionState,
) -> Result<(), String> {
    if session.last_keyframe_request.elapsed() < KEYFRAME_REQUEST_INTERVAL {
        return Ok(());
    }
    transport.send_control(ControlMessage::RequestKeyframe)?;
    session.last_keyframe_request = Instant::now();
    Ok(())
}

fn read_control_messages(
    tcp: &mut TcpStream,
    pending: &mut Vec<u8>,
) -> Result<Vec<ControlMessage>, String> {
    let mut buffer = [0u8; 8 * 1024];
    let mut read_any = false;
    for _ in 0..128 {
        match tcp.read(&mut buffer) {
            Ok(0) if read_any => break,
            Ok(0) => return Err("server closed the control connection".into()),
            Ok(size) => {
                read_any = true;
                pending.extend_from_slice(&buffer[..size]);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if is_retryable_read(&error) => break,
            Err(error) => return Err(format!("control read failed: {error}")),
        }
    }
    Ok(drain_control_messages(pending))
}

fn write_control_nonblocking(tcp: &mut TcpStream, bytes: &[u8]) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut written = 0;
    while written < bytes.len() {
        match tcp.write(&bytes[written..]) {
            Ok(0) => return Err("server closed the control connection".into()),
            Ok(size) => written += size,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(format!("control write failed: {error}")),
        }
    }
    Ok(())
}

#[cfg(test)]
fn write_control(tcp: &mut TcpStream, message: ControlMessage) -> Result<(), String> {
    tcp.write_all(&message.serialize())
        .map_err(|error| format!("control write failed: {error}"))
}

fn is_retryable_read(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    )
}

fn clear_video_queue(shared: &Shared) {
    while shared.video_rx.try_recv().is_ok() {}
}

fn queue_audio_packet(packet: AudioPacket, shared: &Shared) {
    shared.audio_queue.push_latest(packet);
}

fn audio_queue_limit(packet_duration_ms: u8) -> usize {
    AUDIO_QUEUE_TARGET_MS
        .div_ceil(packet_duration_ms.max(1) as usize)
        .clamp(1, AUDIO_QUEUE_MAX_CAPACITY)
}

fn clear_audio_queue(shared: &Shared) {
    shared.audio_queue.clear();
}

fn should_stop(shared: &Shared) -> bool {
    shared.stop.load(Ordering::Acquire)
}

fn set_status(shared: &Shared, status: &str) {
    *shared.status.lock().unwrap() = status.to_string();
}

fn next_stream_generation(shared: &Shared) -> u32 {
    let previous = shared
        .last_stream_generation
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
            generation.checked_add(1)
        })
        .expect("stream generation exhausted");
    previous + 1
}

fn next_transport_generation(shared: &Shared) -> u32 {
    let previous = shared
        .transport_generation
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
            generation.checked_add(1)
        })
        .expect("transport generation exhausted");
    previous + 1
}

fn unix_time_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u64::MAX as u128) as u64
}

fn cursor_extent_max(extent: u32) -> f64 {
    extent.saturating_sub(1) as f64
}

fn cursor_extent_center(extent: u32) -> f64 {
    (extent as f64 / 2.0).min(cursor_extent_max(extent))
}

fn normalize_cursor_coord(value: f64, extent: u32) -> u16 {
    if extent <= 1 {
        return 0;
    }
    let normalized =
        value.clamp(0.0, cursor_extent_max(extent)) * u16::MAX as f64 / cursor_extent_max(extent);
    normalized.round().clamp(0.0, u16::MAX as f64) as u16
}

fn denormalize_cursor_coord(value: u16, extent: u32) -> f64 {
    value as f64 * cursor_extent_max(extent) / u16::MAX as f64
}

fn remap_cursor_axis(value: f64, old_extent: u32, new_extent: u32) -> Option<f64> {
    if old_extent <= 1 || new_extent <= 1 {
        return None;
    }
    Some(
        value.clamp(0.0, cursor_extent_max(old_extent)) * cursor_extent_max(new_extent)
            / cursor_extent_max(old_extent),
    )
}

fn remap_cursor_axis_or_clamp(value: f64, old_extent: u32, new_extent: u32) -> f64 {
    remap_cursor_axis(value, old_extent, new_extent)
        .unwrap_or_else(|| value.clamp(0.0, cursor_extent_max(new_extent)))
}

#[cfg(unix)]
fn tune_udp_recv_buffer(socket: &UdpSocket, target_bytes: libc::c_int) {
    use std::os::fd::AsRawFd;
    unsafe {
        let _ = libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &target_bytes as *const _ as *const libc::c_void,
            std::mem::size_of_val(&target_bytes) as libc::socklen_t,
        );
    }
}

#[cfg(not(unix))]
fn tune_udp_recv_buffer(_socket: &UdpSocket, _target_bytes: i32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use st_protocol::reliable_udp::PunchedSocket;
    use st_protocol::tunnel::CryptoContext;
    use st_protocol::{
        packet::frame_type, FrameSlicer, InputSession, KeyboardStateInput, MouseAbsoluteInput,
        MouseWheelInput,
    };
    use std::net::TcpListener;

    const TEST_INPUT_CREDENTIAL: InputCredential = InputCredential::from_bytes([0xC7; 16]);

    #[test]
    fn probe_success_then_control_failure_routes_to_relay() {
        let failure = SessionFailure::transport(
            SessionStage::Authentication,
            "tunnel closed before authentication",
        );
        assert!(should_fallback_from_punch(&failure));

        let rejection = SessionFailure::classified(
            SessionStage::Authentication,
            "authentication failed; check the token",
        );
        assert!(!should_fallback_from_punch(&rejection));
    }

    #[test]
    fn blocked_udp_routes_to_direct_tcp_tunnel_before_api() {
        let failure = SessionFailure::transport(SessionStage::Connected, "media path timed out");
        assert!(should_try_direct_tcp_tunnel(&failure));
        assert!(!should_fallback_from_punch(&failure));
    }

    #[test]
    fn direct_tcp_tunnel_sends_preamble_and_carries_control() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut preamble = [0u8; TCP_TUNNEL_PREAMBLE.len()];
            stream.read_exact(&mut preamble).unwrap();
            assert_eq!(preamble, *TCP_TUNNEL_PREAMBLE);
            let tunnel = TcpTunnel::new(stream, None, Vec::new()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                tunnel.tick();
                if let Some(PunchedMessage::Control(bytes)) = tunnel.try_recv() {
                    let (message, _) = ControlMessage::deserialize(&bytes).unwrap();
                    assert_eq!(message, ControlMessage::Authenticate("token".into()));
                    break;
                }
                assert!(Instant::now() < deadline, "timed out waiting for control");
                thread::sleep(Duration::from_millis(1));
            }
        });

        let mut transport = SessionTransport::connect_direct_tunnel(&address.to_string()).unwrap();
        transport
            .send_control(ControlMessage::Authenticate("token".into()))
            .unwrap();
        server.join().unwrap();
    }

    fn read_control(
        tcp: &mut TcpStream,
        pending: &mut Vec<u8>,
        deadline: Instant,
    ) -> ControlMessage {
        let mut buffer = [0u8; 1024];
        loop {
            if let Some((message, used)) = ControlMessage::deserialize(pending) {
                pending.drain(..used);
                return message;
            }
            assert!(
                Instant::now() < deadline,
                "timed out reading control message"
            );
            match tcp.read(&mut buffer) {
                Ok(0) => panic!("client closed control connection"),
                Ok(size) => pending.extend_from_slice(&buffer[..size]),
                Err(error) if is_retryable_read(&error) => {}
                Err(error) => panic!("control read failed: {error}"),
            }
        }
    }

    fn test_shared() -> (Arc<Shared>, Sender<Command>, Sender<MouseState>) {
        let (video_tx, video_rx) = crossbeam_channel::bounded(VIDEO_QUEUE_CAPACITY);
        let (command_tx, command_rx) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let (mouse_tx, mouse_rx) = crossbeam_channel::unbounded();
        (
            Arc::new(Shared {
                stop: AtomicBool::new(false),
                status: Mutex::new("connecting".into()),
                stream_event: Mutex::new(None),
                control_state: Mutex::new(ControlState::default()),
                trackpad_router: Mutex::new(TrackpadRouter::default()),
                video_tx,
                video_rx,
                audio_queue: AudioPacketQueue::new(AUDIO_QUEUE_MAX_CAPACITY, 1),
                command_rx,
                mouse_rx,
                accepted_generation: AtomicU32::new(0),
                accepted_video_epoch: AtomicU64::new(0),
                last_stream_generation: AtomicU32::new(0),
                transport_generation: AtomicU32::new(1),
                audio_enabled_requested: AtomicBool::new(false),
            }),
            command_tx,
            mouse_tx,
        )
    }

    fn test_stream_config(width: u32, height: u32) -> StreamConfig {
        StreamConfig {
            video_epoch: 1,
            codec: VideoCodec::H264,
            width,
            height,
            framerate: 60,
            audio_sample_rate: 48_000,
            audio_channels: 2,
            hdr: false,
            chroma: VideoChromaSampling::Yuv420,
            packet_duration_ms: 20,
        }
    }

    fn desktop_capabilities(position_reliable: bool) -> InputCapabilities {
        InputCapabilities {
            mouse_absolute: true,
            mouse_relative: true,
            separate_cursor: true,
            cursor_position_reliable: position_reliable,
            ..InputCapabilities::default()
        }
    }

    fn trackpad_router(
        width: u32,
        height: u32,
        position_reliable: bool,
        state: CursorState,
    ) -> TrackpadRouter {
        let mut router = TrackpadRouter::default();
        router.set_stream_config(test_stream_config(width, height));
        router.set_capabilities(desktop_capabilities(position_reliable));
        router.set_controller_state(ControllerState::Available);
        router.set_cursor_shape(CursorShape {
            serial: state.serial,
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: vec![0; 4],
        });
        router.set_cursor_state(state);
        router
    }

    fn absolute_tip(route: TrackpadRoute, width: u32, height: u32) -> (f64, f64) {
        let TrackpadRoute::Absolute { x, y } = route else {
            panic!("expected absolute route, got {route:?}");
        };
        (
            denormalize_cursor_coord(x, width),
            denormalize_cursor_coord(y, height),
        )
    }

    fn assert_tip_near(actual: (f64, f64), expected: (f64, f64)) {
        assert!(
            (actual.0 - expected.0).abs() <= 1.0,
            "x tip {} was not within one pixel of {}",
            actual.0,
            expected.0
        );
        assert!(
            (actual.1 - expected.1).abs() <= 1.0,
            "y tip {} was not within one pixel of {}",
            actual.1,
            expected.1
        );
    }

    #[test]
    fn synchronous_route_exactly_matches_queued_mouse_state() {
        let mut router = trackpad_router(
            1920,
            1080,
            true,
            CursorState {
                serial: 1,
                x: 1_000,
                y: 300,
                visible: true,
                app_grab: false,
            },
        );

        let (first, queued) = router.route_trackpad_delta(-30, 40, 5).unwrap();
        let TrackpadRoute::Absolute { x, y } = first else {
            panic!("expected absolute route");
        };
        assert_eq!(queued, MouseState::Absolute { x, y, buttons: 5 });
        assert_tip_near(absolute_tip(first, 1920, 1080), (970.0, 340.0));

        let (second, queued) = router.route_trackpad_delta(-70, 90, 0).unwrap();
        let TrackpadRoute::Absolute { x, y } = second else {
            panic!("expected absolute route");
        };
        assert_eq!(queued, MouseState::Absolute { x, y, buttons: 0 });
        assert_tip_near(absolute_tip(second, 1920, 1080), (900.0, 430.0));
    }

    #[test]
    fn reliable_trackpad_anchor_includes_matching_hotspot() {
        let mut router = trackpad_router(
            800,
            600,
            true,
            CursorState {
                serial: 9,
                x: 100,
                y: 200,
                visible: true,
                app_grab: false,
            },
        );
        router.set_cursor_shape(CursorShape {
            serial: 9,
            width: 16,
            height: 16,
            hotspot_x: 7,
            hotspot_y: 11,
            rgba: vec![0; 16 * 16 * 4],
        });

        let (route, _) = router.route_trackpad_delta(3, -2, 0).unwrap();
        assert_tip_near(absolute_tip(route, 800, 600), (110.0, 209.0));
    }

    #[test]
    fn cursor_state_after_local_absolute_control_does_not_rewind_target() {
        let state = CursorState {
            serial: 1,
            x: 100,
            y: 100,
            visible: true,
            app_grab: false,
        };
        let mut router = trackpad_router(800, 600, true, state);
        router.route_trackpad_delta(10, 0, 0).unwrap();
        router.set_cursor_state(CursorState { x: 300, ..state });

        let (route, _) = router.route_trackpad_delta(10, 0, 0).unwrap();
        assert_tip_near(absolute_tip(route, 800, 600), (120.0, 100.0));
    }

    #[test]
    fn ownership_moving_away_reseeds_trackpad_from_reliable_server_cursor() {
        let state = CursorState {
            serial: 1,
            x: 100,
            y: 200,
            visible: true,
            app_grab: false,
        };
        let mut router = trackpad_router(800, 600, true, state);
        router.set_controller_state(ControllerState::OwnedByYou);
        router.note_absolute_input(u16::MAX, u16::MAX);

        router.set_controller_state(ControllerState::OwnedByOther);

        assert!(!router.local_absolute_control);
        assert_eq!(router.absolute_target, Some((100.0, 200.0)));
        let (route, _) = router.route_trackpad_delta(5, -10, 0).unwrap();
        assert_tip_near(absolute_tip(route, 800, 600), (105.0, 190.0));
        assert!(router.local_absolute_control);
    }

    #[test]
    fn unreliable_cursor_ignores_bogus_state_and_continues_from_center() {
        let state = CursorState {
            serial: 1,
            x: 0,
            y: 0,
            visible: true,
            app_grab: false,
        };
        let mut router = trackpad_router(1000, 600, false, state);
        let (first, _) = router.route_trackpad_delta(10, -10, 0).unwrap();
        assert_tip_near(absolute_tip(first, 1000, 600), (510.0, 290.0));
        router.set_cursor_state(state);

        let (second, _) = router.route_trackpad_delta(5, 5, 0).unwrap();
        assert_tip_near(absolute_tip(second, 1000, 600), (515.0, 295.0));
    }

    #[test]
    fn hidden_cursor_routes_trackpad_delta_relative() {
        let visible = CursorState {
            serial: 1,
            x: 100,
            y: 100,
            visible: true,
            app_grab: false,
        };
        let mut router = trackpad_router(
            800,
            600,
            true,
            CursorState {
                visible: false,
                ..visible
            },
        );
        assert_eq!(
            router.route_trackpad_delta(-4, 6, 3),
            Some((
                TrackpadRoute::Relative,
                MouseState::Relative {
                    dx: -4,
                    dy: 6,
                    buttons: 3,
                },
            ))
        );
    }

    #[test]
    fn absolute_only_trackpad_route_rejects_when_cursor_requires_relative() {
        let mut router = trackpad_router(
            800,
            600,
            true,
            CursorState {
                serial: 1,
                x: 100,
                y: 100,
                visible: false,
                app_grab: false,
            },
        );
        router.set_capabilities(InputCapabilities {
            mouse_absolute: true,
            separate_cursor: true,
            cursor_position_reliable: true,
            ..InputCapabilities::default()
        });

        assert_eq!(router.route_trackpad_delta(1, 1, 0), None);
    }

    #[test]
    fn stream_resize_remaps_existing_trackpad_tip_by_normalized_position() {
        let mut router = trackpad_router(
            1000,
            500,
            false,
            CursorState {
                serial: 1,
                x: 0,
                y: 0,
                visible: true,
                app_grab: false,
            },
        );
        router.route_trackpad_delta(100, 50, 0).unwrap();
        router.set_stream_config(test_stream_config(2000, 1000));

        let (route, _) = router.route_trackpad_delta(0, 0, 0).unwrap();
        assert_tip_near(
            absolute_tip(route, 2000, 1000),
            (600.0 * 1999.0 / 999.0, 300.0 * 999.0 / 499.0),
        );
    }

    #[test]
    fn validates_android_stream_subset() {
        let valid = StreamConfig {
            video_epoch: 1,
            codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
            framerate: 60,
            audio_sample_rate: 48_000,
            audio_channels: 2,
            hdr: false,
            chroma: VideoChromaSampling::Yuv420,
            packet_duration_ms: 20,
        };
        assert!(validate_stream_config(valid).is_ok());
        assert!(validate_stream_config(StreamConfig {
            codec: VideoCodec::Hevc,
            ..valid
        })
        .is_err());
    }

    #[test]
    fn stream_updates_restart_only_the_incompatible_media_path() {
        let (shared, _, _) = test_shared();
        let mut session = SessionState::new("127.0.0.1".parse().unwrap());
        let initial = test_stream_config(1920, 1080);
        handle_control_message(ControlMessage::StreamConfig(initial), &mut session, &shared)
            .unwrap();
        let first = shared.stream_event.lock().unwrap().take().unwrap();
        shared
            .accepted_generation
            .store(first.generation, Ordering::Release);
        shared
            .accepted_video_epoch
            .store(first.video_epoch, Ordering::Release);
        session.waiting_for_recovery = false;
        shared
            .video_tx
            .send(AccessUnit {
                frame_id: 7,
                frame_type: frame_type::P,
                data: vec![7],
            })
            .unwrap();
        queue_audio_packet(
            AudioPacket {
                seq: 8,
                data: vec![8],
                redundant_prev: Vec::new(),
            },
            &shared,
        );

        let fps_only = StreamConfig {
            framerate: 90,
            ..initial
        };
        handle_control_message(
            ControlMessage::StreamConfig(fps_only),
            &mut session,
            &shared,
        )
        .unwrap();
        let fps_event = shared.stream_event.lock().unwrap().take().unwrap();
        assert_eq!(fps_event.generation, first.generation);
        assert_eq!(fps_event.framerate, 90);
        assert_eq!(shared.video_rx.len(), 1);
        assert_eq!(shared.audio_queue.stats().occupancy, 1);
        assert!(!session.waiting_for_recovery);

        let audio_only = StreamConfig {
            packet_duration_ms: 10,
            ..fps_only
        };
        handle_control_message(
            ControlMessage::StreamConfig(audio_only),
            &mut session,
            &shared,
        )
        .unwrap();
        let audio_event = shared.stream_event.lock().unwrap().take().unwrap();
        assert_eq!(audio_event.generation, first.generation);
        assert_eq!(shared.video_rx.len(), 1);
        assert_eq!(shared.audio_queue.stats().occupancy, 0);
        assert!(!session.waiting_for_recovery);

        queue_audio_packet(
            AudioPacket {
                seq: 9,
                data: vec![9],
                redundant_prev: Vec::new(),
            },
            &shared,
        );
        let resized = StreamConfig {
            width: 1280,
            height: 720,
            ..audio_only
        };
        handle_control_message(ControlMessage::StreamConfig(resized), &mut session, &shared)
            .unwrap();
        let resize_event = shared.stream_event.lock().unwrap().take().unwrap();
        assert_eq!(resize_event.generation, first.generation + 1);
        assert!(shared.video_rx.is_empty());
        assert_eq!(shared.audio_queue.stats().occupancy, 1);
        assert!(session.waiting_for_recovery);
    }

    #[test]
    fn incompatible_stream_config_discards_partial_old_video_epoch() {
        let (shared, _, _) = test_shared();
        let mut session = SessionState::new("127.0.0.1".parse().unwrap());
        let initial = test_stream_config(1920, 1080);
        handle_control_message(ControlMessage::StreamConfig(initial), &mut session, &shared)
            .unwrap();

        let mut slicer = st_protocol::FrameSlicer::new();
        slicer.set_parity_enabled(false);
        let packets = slicer.slice(&vec![0x65; 4_000], 77).to_vec();
        assert!(packets.len() > 1);
        assert!(session.demux.process_packet(&packets[0], None).is_none());

        handle_control_message(
            ControlMessage::StreamConfig(StreamConfig {
                width: 1280,
                height: 720,
                ..initial
            }),
            &mut session,
            &shared,
        )
        .unwrap();
        for packet in &packets[1..] {
            assert!(
                session.demux.process_packet(packet, None).is_none(),
                "old-epoch remainder must not complete after assembler reset"
            );
        }
    }

    #[test]
    fn media_waits_for_matching_stream_config_epoch_acceptance() {
        let (shared, _, _) = test_shared();
        let mut session = SessionState::new("127.0.0.1".parse().unwrap());
        let initial = test_stream_config(1920, 1080);
        handle_control_message(ControlMessage::StreamConfig(initial), &mut session, &shared)
            .unwrap();
        let initial_event = shared.stream_event.lock().unwrap().take().unwrap();
        shared
            .accepted_generation
            .store(initial_event.generation, Ordering::Release);
        shared
            .accepted_video_epoch
            .store(initial_event.video_epoch, Ordering::Release);
        session.waiting_for_recovery = false;

        let packet_for = |frame_id, video_epoch| {
            let mut slicer = FrameSlicer::new();
            slicer
                .slice_with_meta_parts(
                    &[0, 0, 0, 1, 0x65, frame_id as u8],
                    frame_id,
                    st_protocol::FrameTimingMeta {
                        video_epoch,
                        ..Default::default()
                    },
                    frame_type::IDR,
                )
                .0[0]
                .clone()
        };

        process_media_packet(&packet_for(1, 2), None, &mut session, &shared).unwrap();
        assert!(
            shared.video_rx.is_empty(),
            "future epoch arrived before config"
        );

        let next = StreamConfig {
            video_epoch: 2,
            ..initial
        };
        handle_control_message(ControlMessage::StreamConfig(next), &mut session, &shared).unwrap();
        let next_event = shared.stream_event.lock().unwrap().take().unwrap();
        assert_eq!(next_event.generation, initial_event.generation);
        assert_eq!(shared.accepted_video_epoch.load(Ordering::Acquire), 0);

        process_media_packet(&packet_for(2, 2), None, &mut session, &shared).unwrap();
        assert!(
            shared.video_rx.is_empty(),
            "epoch was not accepted by the UI"
        );

        shared
            .accepted_video_epoch
            .store(next_event.video_epoch, Ordering::Release);
        process_media_packet(&packet_for(3, 2), None, &mut session, &shared).unwrap();
        assert_eq!(shared.video_rx.try_recv().unwrap().frame_id, 3);
    }

    #[test]
    fn api_fallback_retries_transport_failures_but_not_terminal_protocol_errors() {
        let refused =
            SessionFailure::classified(SessionStage::DirectConnect, "connection failed: refused");
        let auth = SessionFailure::classified(
            SessionStage::Authentication,
            "authentication failed; check the token",
        );
        let codec = SessionFailure::classified(
            SessionStage::Startup,
            "server selected unsupported codec Hevc",
        );
        assert!(!refused.terminal);
        assert!(auth.terminal);
        assert!(codec.terminal);
    }

    #[test]
    fn transport_fallback_allocates_a_new_stream_generation() {
        let (shared, _, _) = test_shared();
        let mut direct = SessionState::new("127.0.0.1".parse().unwrap());
        handle_control_message(
            ControlMessage::StreamConfig(test_stream_config(1920, 1080)),
            &mut direct,
            &shared,
        )
        .unwrap();
        let direct_event = shared.stream_event.lock().unwrap().take().unwrap();
        handle_control_message(
            ControlMessage::InputCapabilities(desktop_capabilities(true)),
            &mut direct,
            &shared,
        )
        .unwrap();
        let direct_control = shared
            .control_state
            .lock()
            .unwrap()
            .take_snapshot(direct_event.transport_generation)
            .unwrap();
        shared
            .accepted_generation
            .store(direct_event.generation, Ordering::Release);
        shared
            .accepted_video_epoch
            .store(direct_event.video_epoch, Ordering::Release);

        reset_for_transport_fallback(&shared, "connecting through API punch");
        let reset_control = shared
            .control_state
            .lock()
            .unwrap()
            .take_snapshot(shared.transport_generation.load(Ordering::Acquire))
            .unwrap();
        let mut api = SessionState::new("127.0.0.1".parse().unwrap());
        handle_control_message(
            ControlMessage::StreamConfig(test_stream_config(1920, 1080)),
            &mut api,
            &shared,
        )
        .unwrap();
        let api_event = shared.stream_event.lock().unwrap().take().unwrap();

        assert_eq!(direct_event.generation, 1);
        assert_eq!(api_event.generation, 2);
        assert!(api_event.generation > direct_event.generation);
        assert_eq!(direct_event.transport_generation, 1);
        assert_eq!(api_event.transport_generation, 2);
        assert_eq!(direct_control.transport_generation, 1);
        assert_eq!(reset_control.transport_generation, 2);
        assert!(reset_control.is_empty());
        assert_eq!(shared.accepted_generation.load(Ordering::Acquire), 0);
        assert_eq!(shared.accepted_video_epoch.load(Ordering::Acquire), 0);
    }

    #[test]
    fn recovery_rejects_p_frames_even_when_they_carry_parameter_sets() {
        let (shared, _, _) = test_shared();
        let mut session = SessionState::new("127.0.0.1".parse().unwrap());
        handle_control_message(
            ControlMessage::StreamConfig(test_stream_config(1920, 1080)),
            &mut session,
            &shared,
        )
        .unwrap();
        shared
            .accepted_generation
            .store(session.generation, Ordering::Release);
        shared.accepted_video_epoch.store(1, Ordering::Release);

        let p_with_parameter_sets = st_protocol::CompletedFrame {
            frame_id: 1,
            video_epoch: 1,
            data: vec![
                0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 4, 0, 0, 0, 1, 0x41, 5, 6,
            ],
            timing: Default::default(),
            frame_type: frame_type::P,
        };
        assert!(queue_video_frame(p_with_parameter_sets, &mut session, &shared).unwrap());
        assert!(shared.video_rx.is_empty());
        assert!(session.waiting_for_recovery);

        let idr = st_protocol::CompletedFrame {
            frame_id: 2,
            video_epoch: 1,
            data: vec![0, 0, 0, 1, 0x65, 7, 8],
            timing: Default::default(),
            frame_type: frame_type::IDR,
        };
        assert!(!queue_video_frame(idr, &mut session, &shared).unwrap());
        assert!(!session.waiting_for_recovery);
        assert_eq!(shared.video_rx.try_recv().unwrap().frame_id, 2);
    }

    #[test]
    fn late_frame_after_recovery_does_not_rewind_decode_order() {
        let (shared, _, _) = test_shared();
        let mut session = SessionState::new("127.0.0.1".parse().unwrap());
        handle_control_message(
            ControlMessage::StreamConfig(test_stream_config(1920, 1080)),
            &mut session,
            &shared,
        )
        .unwrap();
        shared
            .accepted_generation
            .store(session.generation, Ordering::Release);
        shared.accepted_video_epoch.store(1, Ordering::Release);

        let frame = |frame_id, frame_type| st_protocol::CompletedFrame {
            frame_id,
            video_epoch: 1,
            data: vec![frame_id as u8],
            timing: Default::default(),
            frame_type,
        };
        assert!(!queue_video_frame(frame(10, frame_type::IDR), &mut session, &shared).unwrap());
        assert!(!queue_video_frame(frame(9, frame_type::P), &mut session, &shared).unwrap());
        assert!(!queue_video_frame(frame(11, frame_type::P), &mut session, &shared).unwrap());

        assert_eq!(shared.video_rx.try_recv().unwrap().frame_id, 10);
        assert_eq!(shared.video_rx.try_recv().unwrap().frame_id, 11);
        assert!(shared.video_rx.is_empty());
        assert_eq!(session.last_frame_id, Some(11));
        assert!(!session.waiting_for_recovery);
    }

    #[test]
    fn video_consumer_overflow_is_reported_as_an_urgent_dropped_frame() {
        let (shared, _, _) = test_shared();
        let mut session = SessionState::new("127.0.0.1".parse().unwrap());
        handle_control_message(
            ControlMessage::StreamConfig(test_stream_config(1920, 1080)),
            &mut session,
            &shared,
        )
        .unwrap();
        shared
            .accepted_generation
            .store(session.generation, Ordering::Release);
        shared.accepted_video_epoch.store(1, Ordering::Release);

        for frame_id in 0..=VIDEO_QUEUE_CAPACITY as u32 {
            let frame = st_protocol::CompletedFrame {
                frame_id,
                video_epoch: 1,
                data: vec![frame_id as u8],
                timing: Default::default(),
                frame_type: if frame_id == 0 {
                    frame_type::IDR
                } else {
                    frame_type::P
                },
            };
            let overflowed = queue_video_frame(frame, &mut session, &shared).unwrap();
            assert_eq!(overflowed, frame_id == VIDEO_QUEUE_CAPACITY as u32);
        }

        assert!(shared.video_rx.is_empty());
        let stats = session.demux.take_stats().unwrap();
        assert_eq!(stats.dropped_frames, 1);
        assert!(stats.needs_recovery_keyframe());
    }

    #[cfg(unix)]
    #[test]
    fn direct_control_socket_is_nonblocking() {
        use std::os::fd::AsRawFd;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let accept = thread::spawn(move || listener.accept().unwrap());
        let transport = SessionTransport::connect_direct(&address.to_string()).unwrap();
        let (_server, _) = accept.join().unwrap();
        let SessionTransport::Direct { tcp, .. } = transport else {
            unreachable!();
        };
        let flags = unsafe { libc::fcntl(tcp.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::O_NONBLOCK, 0);
    }

    #[test]
    fn control_messages_publish_revisioned_latest_cursor_snapshot() {
        let (video_tx, video_rx) = crossbeam_channel::bounded(1);
        let (_command_tx, command_rx) = crossbeam_channel::bounded(1);
        let (_mouse_tx, mouse_rx) = crossbeam_channel::unbounded();
        let shared = Shared {
            stop: AtomicBool::new(false),
            status: Mutex::new(String::new()),
            stream_event: Mutex::new(None),
            control_state: Mutex::new(ControlState::default()),
            trackpad_router: Mutex::new(TrackpadRouter::default()),
            video_tx,
            video_rx,
            audio_queue: AudioPacketQueue::new(AUDIO_QUEUE_MAX_CAPACITY, 1),
            command_rx,
            mouse_rx,
            accepted_generation: AtomicU32::new(0),
            accepted_video_epoch: AtomicU64::new(0),
            last_stream_generation: AtomicU32::new(0),
            transport_generation: AtomicU32::new(1),
            audio_enabled_requested: AtomicBool::new(false),
        };
        let mut session = SessionState::new("127.0.0.1".parse().unwrap());
        let capabilities = InputCapabilities {
            mouse_absolute: true,
            mouse_relative: true,
            keyboard: true,
            separate_cursor: true,
            hover_capture: true,
            cursor_position_reliable: true,
            text_input: true,
        };
        let first_shape = CursorShape {
            serial: 0x0102_0304_0506_0708,
            width: 2,
            height: 1,
            hotspot_x: 1,
            hotspot_y: 0,
            rgba: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let latest_shape = CursorShape {
            rgba: vec![8, 7, 6, 5, 4, 3, 2, 1],
            ..first_shape.clone()
        };
        let cursor = CursorState {
            serial: latest_shape.serial,
            x: -37,
            y: -91,
            visible: true,
            app_grab: false,
        };

        for message in [
            ControlMessage::InputCapabilities(capabilities),
            ControlMessage::ControllerState(ControllerState::OwnedByOther),
            ControlMessage::CursorShape(first_shape),
            ControlMessage::CursorShape(latest_shape.clone()),
            ControlMessage::CursorState(cursor),
        ] {
            handle_control_message(message, &mut session, &shared).unwrap();
        }

        let snapshot = shared
            .control_state
            .lock()
            .unwrap()
            .take_snapshot(1)
            .unwrap();
        assert_eq!(snapshot.input_capabilities.unwrap().value, capabilities);
        assert_eq!(
            snapshot.controller_state.unwrap().value,
            ControllerState::OwnedByOther
        );
        let shape = snapshot.cursor_shape.unwrap();
        assert_eq!(shape.revision, 2);
        assert_eq!(shape.value, latest_shape);
        assert_eq!(snapshot.cursor_state.unwrap().value, cursor);
        assert!(shared
            .control_state
            .lock()
            .unwrap()
            .take_snapshot(1)
            .is_none());

        let hidden = CursorState {
            serial: cursor.serial,
            x: i32::MIN,
            y: -1,
            visible: false,
            app_grab: true,
        };
        handle_control_message(
            ControlMessage::InputCapabilities(capabilities),
            &mut session,
            &shared,
        )
        .unwrap();
        handle_control_message(ControlMessage::CursorState(hidden), &mut session, &shared).unwrap();
        let changed = shared
            .control_state
            .lock()
            .unwrap()
            .take_snapshot(1)
            .unwrap();
        assert!(changed.input_capabilities.is_none());
        assert!(changed.controller_state.is_none());
        assert!(changed.cursor_shape.is_none());
        let state = changed.cursor_state.unwrap();
        assert_eq!(state.revision, 2);
        assert_eq!(state.value, hidden);
    }

    #[test]
    fn full_audio_queue_evicts_oldest_packets() {
        let (video_tx, video_rx) = crossbeam_channel::bounded(1);
        let (_command_tx, command_rx) = crossbeam_channel::bounded(1);
        let (_mouse_tx, mouse_rx) = crossbeam_channel::unbounded();
        let shared = Shared {
            stop: AtomicBool::new(false),
            status: Mutex::new(String::new()),
            stream_event: Mutex::new(None),
            control_state: Mutex::new(ControlState::default()),
            trackpad_router: Mutex::new(TrackpadRouter::default()),
            video_tx,
            video_rx,
            audio_queue: AudioPacketQueue::new(AUDIO_QUEUE_MAX_CAPACITY, AUDIO_QUEUE_MAX_CAPACITY),
            command_rx,
            mouse_rx,
            accepted_generation: AtomicU32::new(0),
            accepted_video_epoch: AtomicU64::new(0),
            last_stream_generation: AtomicU32::new(0),
            transport_generation: AtomicU32::new(1),
            audio_enabled_requested: AtomicBool::new(true),
        };
        for seq in 0..AUDIO_QUEUE_MAX_CAPACITY as u16 + 3 {
            queue_audio_packet(
                AudioPacket {
                    seq,
                    data: vec![seq as u8],
                    redundant_prev: Vec::new(),
                },
                &shared,
            );
        }

        let mut queued = Vec::new();
        while let Some(packet) = shared.audio_queue.try_recv() {
            queued.push((packet.seq, packet.local_discontinuity));
        }
        assert_eq!(queued.len(), AUDIO_QUEUE_MAX_CAPACITY);
        assert_eq!(queued[0], (3, true));
        assert_eq!(queued[AUDIO_QUEUE_MAX_CAPACITY - 1].0, 10);
        assert_eq!(shared.audio_queue.stats().local_drops, 3);
    }

    #[test]
    fn audio_queue_limit_tracks_about_twenty_ms() {
        assert_eq!(audio_queue_limit(5), 4);
        assert_eq!(audio_queue_limit(10), 2);
        assert_eq!(audio_queue_limit(20), 1);
        assert_eq!(audio_queue_limit(40), 1);
        assert_eq!(audio_queue_limit(60), 1);
        assert_eq!(audio_queue_limit(1), AUDIO_QUEUE_MAX_CAPACITY);
    }

    #[test]
    fn input_eligibility_allows_every_available_ownership_state() {
        let mut eligibility = InputEligibility {
            capabilities: InputCapabilities {
                mouse_absolute: true,
                ..InputCapabilities::default()
            },
            controller_state: ControllerState::OwnedByOther,
        };
        assert!(eligibility.allows_absolute());
        assert!(eligibility.allows_trackpad_delta());
        assert!(eligibility.allows_buttons());
        assert!(!eligibility.allows_keyboard());

        eligibility.controller_state = ControllerState::Available;
        assert!(eligibility.allows_absolute());
        assert!(eligibility.allows_trackpad_delta());
        assert!(eligibility.allows_buttons());
        assert!(!eligibility.allows_keyboard());

        eligibility.capabilities.keyboard = true;
        assert!(eligibility.allows_keyboard());
        assert!(!eligibility.allows_text_input());
        eligibility.capabilities.text_input = true;
        assert!(eligibility.allows_text_input());

        eligibility.controller_state = ControllerState::OwnedByOther;
        assert!(eligibility.allows_absolute());
        assert!(eligibility.allows_trackpad_delta());
        assert!(eligibility.allows_buttons());
        assert!(eligibility.allows_keyboard());
        assert!(eligibility.allows_text_input());

        eligibility.controller_state = ControllerState::Unavailable;
        assert!(!eligibility.allows_absolute());
        assert!(!eligibility.allows_trackpad_delta());
        assert!(!eligibility.allows_buttons());
        assert!(!eligibility.allows_keyboard());
        assert!(!eligibility.allows_text_input());

        eligibility.controller_state = ControllerState::Available;
        eligibility.capabilities = InputCapabilities::default();
        assert!(!eligibility.allows_absolute());
        assert!(!eligibility.allows_trackpad_delta());
        assert!(!eligibility.allows_buttons());
        assert!(!eligibility.allows_keyboard());
        assert!(!eligibility.allows_text_input());
    }

    #[test]
    fn keyboard_state_rejects_reserved_bits() {
        let mut pressed = [0u8; KEYBOARD_STATE_BYTES];
        let (byte, bit) = KeyboardKey::IntlBackslash.bit();
        pressed[byte] |= bit;
        assert!(keyboard_state_is_valid(&pressed));
        pressed[15] |= 1 << 6;
        assert!(!keyboard_state_is_valid(&pressed));
    }

    #[test]
    fn committed_text_validation_preserves_exact_unicode() {
        let text = "e\u{301} 中文 مرحبا 😀";
        assert!(validate_text_input(text).is_ok());
        assert!(validate_text_input("").is_err());
        assert!(validate_text_input("a\0b").is_err());
        assert!(validate_text_input(&"x".repeat(MAX_TEXT_INPUT_BYTES)).is_ok());
        assert!(validate_text_input(&"x".repeat(MAX_TEXT_INPUT_BYTES + 1)).is_err());
    }

    #[test]
    fn keyboard_heartbeat_repairs_pressed_and_release_states() {
        let eligibility = InputEligibility {
            capabilities: InputCapabilities {
                keyboard: true,
                ..InputCapabilities::default()
            },
            controller_state: ControllerState::Available,
        };
        let mut pressed = [0u8; KEYBOARD_STATE_BYTES];
        let (byte, bit) = KeyboardKey::A.bit();
        pressed[byte] = bit;
        let down = InputPacket::KeyboardState(KeyboardStateInput {
            client_id: 7,
            pressed,
        });
        let mut heartbeat = KeyboardHeartbeat::default();
        heartbeat.observe(down);
        let observed_at = heartbeat.last_sent.unwrap();
        assert!(heartbeat
            .due_packet(observed_at + INPUT_HEARTBEAT_INTERVAL / 2, eligibility)
            .is_none());
        assert_eq!(
            heartbeat.due_packet(observed_at + INPUT_HEARTBEAT_INTERVAL, eligibility),
            Some(down)
        );

        let up = InputPacket::KeyboardState(KeyboardStateInput {
            client_id: 7,
            pressed: [0; KEYBOARD_STATE_BYTES],
        });
        heartbeat.observe(up);
        let released_at = heartbeat.last_sent.unwrap();
        assert_eq!(
            heartbeat.due_packet(released_at + INPUT_HEARTBEAT_INTERVAL, eligibility),
            Some(up)
        );
        assert!(heartbeat
            .due_packet(released_at + INPUT_REPAIR_WINDOW, eligibility)
            .is_none());
    }

    #[test]
    fn wheel_heartbeat_preserves_buttons_without_replaying_scroll() {
        let heartbeat = MouseHeartbeat::new(
            InputPacket::MouseWheel(MouseWheelInput {
                client_id: 7,
                delta_x: -33,
                delta_y: 120,
                buttons: 2,
            }),
            2,
        );
        assert_eq!(
            heartbeat.packet,
            InputPacket::MouseButtons(MouseButtonsInput {
                client_id: 7,
                buttons: 2,
            })
        );
    }

    #[test]
    fn absolute_heartbeat_preserves_only_buttons_without_replaying_position() {
        let heartbeat = MouseHeartbeat::new(
            InputPacket::MouseAbsolute(MouseAbsoluteInput {
                client_id: 7,
                x: 10,
                y: 20,
                buttons: 1,
            }),
            1,
        );
        assert_eq!(
            heartbeat.packet,
            InputPacket::MouseButtons(MouseButtonsInput {
                client_id: 7,
                buttons: 1,
            })
        );
    }

    #[test]
    fn ownership_handoff_does_not_clear_held_input_heartbeats() {
        let mut session = SessionState::new("127.0.0.1".parse().unwrap());
        session.input_capabilities = InputCapabilities {
            mouse_absolute: true,
            keyboard: true,
            ..InputCapabilities::default()
        };
        session.controller_state = ControllerState::OwnedByYou;
        session.mouse_heartbeat = Some(MouseHeartbeat::new(
            InputPacket::MouseAbsolute(MouseAbsoluteInput {
                client_id: 7,
                x: 10,
                y: 20,
                buttons: 1,
            }),
            1,
        ));
        let mut pressed = [0u8; KEYBOARD_STATE_BYTES];
        let (byte, bit) = KeyboardKey::A.bit();
        pressed[byte] = bit;
        session
            .keyboard_heartbeat
            .observe(InputPacket::KeyboardState(KeyboardStateInput {
                client_id: 7,
                pressed,
            }));

        session.controller_state = ControllerState::OwnedByOther;
        session.clear_unsupported_input_heartbeats();

        assert!(session.mouse_heartbeat.is_some());
        assert_eq!(
            session.keyboard_heartbeat.packet,
            Some(InputPacket::KeyboardState(KeyboardStateInput {
                client_id: 7,
                pressed,
            }))
        );
    }

    #[test]
    fn punched_session_routes_control_media_and_input() {
        let client_udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client_addr = client_udp.local_addr().unwrap();
        let server_addr = server_udp.local_addr().unwrap();
        let key = [0x4du8; 32];
        let client_link = Arc::new(PunchedSocket::new(
            client_udp,
            server_addr,
            Arc::new(CryptoContext::new(key, false)),
        ));
        let server_link = Arc::new(PunchedSocket::new(
            server_udp,
            client_addr,
            Arc::new(CryptoContext::new(key, true)),
        ));
        client_link.set_nonblocking(true).unwrap();
        server_link.set_nonblocking(true).unwrap();

        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut pending = Vec::new();
            let mut authenticated = false;
            let mut configured = false;
            let mut started = false;
            let mut sent_video = false;
            let mut received_text = false;
            let mut received_mouse = false;
            loop {
                assert!(Instant::now() < deadline, "punched server timed out");
                server_link.tick();
                for message in server_link.try_recv_all() {
                    match message {
                        PunchedMessage::Control(bytes) => {
                            pending.extend_from_slice(&bytes);
                            for message in drain_control_messages(&mut pending) {
                                match message {
                                    ControlMessage::Authenticate(token) => {
                                        assert_eq!(token, "test-token");
                                        server_link
                                            .send_control(
                                                &ControlMessage::AuthResult(true).serialize(),
                                            )
                                            .unwrap();
                                        authenticated = true;
                                    }
                                    ControlMessage::ClientDisplayInfo(display) => {
                                        assert!(authenticated);
                                        assert_eq!(display.udp_port, 0);
                                        for message in [
                                            ControlMessage::StreamConfig(test_stream_config(
                                                1280, 720,
                                            )),
                                            ControlMessage::InputSession(InputSession {
                                                client_id: 42,
                                                credential: TEST_INPUT_CREDENTIAL,
                                            }),
                                            ControlMessage::InputCapabilities(InputCapabilities {
                                                mouse_absolute: true,
                                                keyboard: true,
                                                text_input: true,
                                                ..InputCapabilities::default()
                                            }),
                                            ControlMessage::ControllerState(
                                                ControllerState::Available,
                                            ),
                                        ] {
                                            server_link.send_control(&message.serialize()).unwrap();
                                        }
                                        configured = true;
                                    }
                                    ControlMessage::ClientReadyForMedia => {
                                        assert!(configured);
                                        server_link
                                            .send_control(
                                                &ControlMessage::StreamStarted.serialize(),
                                            )
                                            .unwrap();
                                        started = true;
                                    }
                                    ControlMessage::RequestKeyframe if started && !sent_video => {
                                        let access_unit = vec![0, 0, 0, 1, 0x65, 1, 2, 3];
                                        let mut slicer = FrameSlicer::new();
                                        let (packets, _) = slicer.slice_with_meta_parts(
                                            &access_unit,
                                            1,
                                            st_protocol::FrameTimingMeta {
                                                video_epoch: 1,
                                                ..Default::default()
                                            },
                                            frame_type::IDR,
                                        );
                                        for packet in packets {
                                            server_link.send_media(packet).unwrap();
                                        }
                                        sent_video = true;
                                    }
                                    ControlMessage::TextInput(text) => {
                                        assert_eq!(text, "e\u{301} 中文 😀");
                                        received_text = true;
                                        if received_mouse {
                                            return;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        PunchedMessage::Media(bytes) => {
                            if let Some((_, credential, InputPacket::MouseAbsolute(input))) =
                                InputPacket::deserialize(&bytes)
                            {
                                assert_eq!(credential, TEST_INPUT_CREDENTIAL);
                                assert_eq!(input.client_id, 42);
                                assert_eq!((input.x, input.y, input.buttons), (100, 200, 1));
                                received_mouse = true;
                                if received_text {
                                    return;
                                }
                            }
                        }
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
        });

        let (shared, command_tx, mouse_tx) = test_shared();
        let client_shared = Arc::clone(&shared);
        let client_link: Arc<dyn TunnelLink> = client_link;
        let client = thread::spawn(move || {
            let mut transport = SessionTransport::from_tunnel(client_link).unwrap();
            run_transport_session(
                &client_shared,
                &ClientConfig {
                    server: None,
                    api: None,
                    token: "test-token".into(),
                    refresh_millihz: 60_000,
                },
                &mut transport,
            )
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            if let Some(stream) = shared.stream_event.lock().unwrap().take() {
                break stream;
            }
            assert!(Instant::now() < deadline, "stream config did not arrive");
            thread::sleep(Duration::from_millis(1));
        };
        let (acknowledgement, accepted) = crossbeam_channel::bounded(1);
        command_tx
            .send(Command::AcceptStream {
                generation: stream.generation,
                video_epoch: stream.video_epoch,
                acknowledgement,
            })
            .unwrap();
        assert!(accepted.recv_timeout(Duration::from_secs(1)).unwrap());
        let unit = shared
            .video_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("video access unit did not arrive");
        assert_eq!(unit.data, vec![0, 0, 0, 1, 0x65, 1, 2, 3]);
        command_tx
            .send(Command::TextInput("e\u{301} 中文 😀".into()))
            .unwrap();
        mouse_tx
            .send(MouseState::Absolute {
                x: 100,
                y: 200,
                buttons: 1,
            })
            .unwrap();
        server.join().unwrap();
        shared.stop.store(true, Ordering::Release);
        assert!(client.join().unwrap().is_ok());
    }

    #[test]
    fn direct_session_reuses_udp_socket_for_media_and_input() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_addr = listener.local_addr().unwrap();
        let media = UdpSocket::bind(server_addr).unwrap();
        media
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let server = thread::spawn(move || {
            let (mut tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            let mut pending = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(3);
            assert_eq!(
                read_control(&mut tcp, &mut pending, deadline),
                ControlMessage::Authenticate("test-token".into())
            );
            write_control(&mut tcp, ControlMessage::AuthResult(true)).unwrap();

            let display = loop {
                if let ControlMessage::ClientDisplayInfo(display) =
                    read_control(&mut tcp, &mut pending, deadline)
                {
                    break display;
                }
            };
            let config = StreamConfig {
                video_epoch: 1,
                codec: VideoCodec::H264,
                width: 1280,
                height: 720,
                framerate: 60,
                audio_sample_rate: 48_000,
                audio_channels: 2,
                hdr: false,
                chroma: VideoChromaSampling::Yuv420,
                packet_duration_ms: 20,
            };
            for message in [
                ControlMessage::StreamConfig(config),
                ControlMessage::InputSession(InputSession {
                    client_id: 42,
                    credential: TEST_INPUT_CREDENTIAL,
                }),
                ControlMessage::InputCapabilities(InputCapabilities {
                    mouse_absolute: true,
                    keyboard: true,
                    text_input: true,
                    ..InputCapabilities::default()
                }),
                ControlMessage::ControllerState(ControllerState::Available),
            ] {
                write_control(&mut tcp, message).unwrap();
            }
            loop {
                if read_control(&mut tcp, &mut pending, deadline)
                    == ControlMessage::ClientReadyForMedia
                {
                    break;
                }
            }
            write_control(&mut tcp, ControlMessage::StreamStarted).unwrap();

            let mut packet = [0u8; 128];
            let (_, bootstrap_source) = media.recv_from(&mut packet).unwrap();
            assert_eq!(bootstrap_source.port(), display.udp_port);

            loop {
                if read_control(&mut tcp, &mut pending, deadline) == ControlMessage::RequestKeyframe
                {
                    break;
                }
            }
            let access_unit = vec![0, 0, 0, 1, 0x65, 1, 2, 3];
            let mut slicer = FrameSlicer::new();
            let (packets, _) = slicer.slice_with_meta_parts(
                &access_unit,
                1,
                st_protocol::FrameTimingMeta {
                    video_epoch: 1,
                    ..Default::default()
                },
                frame_type::IDR,
            );
            for packet in packets {
                media
                    .send_to(packet, SocketAddr::new(server_addr.ip(), display.udp_port))
                    .unwrap();
            }

            loop {
                let (size, source) = media.recv_from(&mut packet).unwrap();
                if let Some((_, credential, InputPacket::MouseAbsolute(input))) =
                    InputPacket::deserialize(&packet[..size])
                {
                    assert_eq!(credential, TEST_INPUT_CREDENTIAL);
                    assert_eq!(source.port(), display.udp_port);
                    assert_eq!(
                        input,
                        MouseAbsoluteInput {
                            client_id: 42,
                            x: 100,
                            y: 200,
                            buttons: 1,
                        }
                    );
                    break;
                }
            }
            loop {
                let (size, source) = media.recv_from(&mut packet).unwrap();
                if let Some((_, credential, InputPacket::MouseWheel(input))) =
                    InputPacket::deserialize(&packet[..size])
                {
                    assert_eq!(credential, TEST_INPUT_CREDENTIAL);
                    assert_eq!(source.port(), display.udp_port);
                    assert_eq!(
                        input,
                        MouseWheelInput {
                            client_id: 42,
                            delta_x: -30,
                            delta_y: 120,
                            buttons: 0,
                        }
                    );
                    break;
                }
            }
            loop {
                let (size, source) = media.recv_from(&mut packet).unwrap();
                if let Some((_, credential, InputPacket::KeyboardState(input))) =
                    InputPacket::deserialize(&packet[..size])
                {
                    assert_eq!(credential, TEST_INPUT_CREDENTIAL);
                    if input.pressed.iter().all(|byte| *byte == 0) {
                        continue;
                    }
                    assert_eq!(source.port(), display.udp_port);
                    assert_eq!(input.client_id, 42);
                    let (byte, bit) = KeyboardKey::A.bit();
                    assert_eq!(input.pressed[byte], bit);
                    break;
                }
            }
            loop {
                if let ControlMessage::TextInput(text) =
                    read_control(&mut tcp, &mut pending, deadline)
                {
                    assert_eq!(text, "e\u{301} 中文 😀");
                    break;
                }
            }
        });

        let client = Client::start(ClientConfig {
            server: Some(server_addr.to_string()),
            api: None,
            token: "test-token".into(),
            refresh_millihz: 60_000,
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let stream = loop {
            if let Some(stream) = client.take_stream_event() {
                break stream;
            }
            assert!(Instant::now() < deadline, "stream config did not arrive");
            thread::sleep(Duration::from_millis(5));
        };
        assert!(client.accept_stream_generation(stream.generation, stream.video_epoch));
        let unit = client
            .recv_access_unit(Duration::from_secs(3))
            .expect("video access unit did not arrive");
        assert_eq!(unit.data, vec![0, 0, 0, 1, 0x65, 1, 2, 3]);
        client.send_mouse_absolute(100, 200, 1).unwrap();
        client.send_mouse_wheel(-30, 120, 0).unwrap();
        let mut pressed = [0u8; KEYBOARD_STATE_BYTES];
        let (byte, bit) = KeyboardKey::A.bit();
        pressed[byte] = bit;
        client.send_keyboard_state(pressed).unwrap();
        client.send_text_input("e\u{301} 中文 😀".into()).unwrap();
        server.join().unwrap();
        client.stop();
    }
}
