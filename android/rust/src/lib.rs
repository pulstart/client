use jni::objects::{JByteArray, JByteBuffer, JClass, JString};
use jni::sys::{jboolean, jbyteArray, jint, jintArray, jlong, jstring};
use jni::JNIEnv;
use st_client_core::{
    ApiConnectionConfig, Client, ClientConfig, ControlSnapshot, ControllerState, InputCapabilities,
    LanDiscovery, QueuedAudioPacket as AudioPacket, TrackpadRoute, KEYBOARD_STATE_BYTES,
};
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_CHANNELS: usize = 2;
const AUDIO_SMOOTHING_MS: usize = 2;
const MAX_RECOVERY_MS: usize = 60;
const MAX_AUDIO_POLL_MS: u64 = 50;
const AUDIO_FILL_BUFFER_ERROR: jint = -1;
const AUDIO_FILL_DECODE_ERROR: jint = -2;
const AUDIO_FILL_RESYNC_FLAG: jint = 1 << 30;
const CURSOR_SNAPSHOT_VERSION: u8 = 2;
const SNAPSHOT_CAPABILITIES: u8 = 1 << 0;
const SNAPSHOT_CONTROLLER_STATE: u8 = 1 << 1;
const SNAPSHOT_CURSOR_SHAPE: u8 = 1 << 2;
const SNAPSHOT_CURSOR_STATE: u8 = 1 << 3;

// Trackpad JNI result: mode in bits 0..7, absolute x in 8..23, y in 24..39.
// Coordinates are normalized u16 values. Relative/rejected results leave them zero.
const TRACKPAD_ROUTE_REJECTED: jlong = 0;
const TRACKPAD_ROUTE_RELATIVE: jlong = 1;
const TRACKPAD_ROUTE_ABSOLUTE: jlong = 2;
const TRACKPAD_ROUTE_X_SHIFT: u32 = 8;
const TRACKPAD_ROUTE_Y_SHIFT: u32 = 24;

macro_rules! jni_boundary {
    ($fallback:expr, $body:block) => {{
        match catch_unwind(AssertUnwindSafe(|| $body)) {
            Ok(value) => value,
            Err(_) => $fallback,
        }
    }};
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Session {
    state: Mutex<SessionState>,
    audio: Mutex<AudioState>,
}

struct SessionState {
    epoch: i64,
    client: Option<EpochValue<Arc<Client>>>,
}

struct EpochValue<T> {
    epoch: i64,
    value: T,
}

impl<T> EpochValue<T> {
    fn get(&self, epoch: i64) -> Option<&T> {
        (self.epoch == epoch).then_some(&self.value)
    }
}

impl Session {
    fn new() -> Self {
        Self {
            state: Mutex::new(SessionState {
                epoch: 0,
                client: None,
            }),
            audio: Mutex::new(AudioState::default()),
        }
    }

    fn invalidate(&self) -> i64 {
        let (epoch, client) = {
            let mut state = lock_recover(&self.state);
            state.epoch = state
                .epoch
                .checked_add(1)
                .expect("Android session epoch exhausted");
            let client = state.client.take().map(|client| client.value);
            (state.epoch, client)
        };
        *lock_recover(&self.audio) = AudioState::default();
        if let Some(client) = client {
            client.clear_audio_queue();
            stop_client_async(client);
        }
        epoch
    }

    fn install_client(&self, epoch: i64, client: Arc<Client>) -> Result<(), Arc<Client>> {
        let mut state = lock_recover(&self.state);
        if state.epoch != epoch || state.client.is_some() {
            return Err(client);
        }
        state.client = Some(EpochValue {
            epoch,
            value: client,
        });
        Ok(())
    }

    fn client(&self) -> Option<Arc<Client>> {
        lock_recover(&self.state)
            .client
            .as_ref()
            .map(|client| Arc::clone(&client.value))
    }

    fn client_for_epoch(&self, epoch: i64) -> Option<Arc<Client>> {
        let state = lock_recover(&self.state);
        state.client.as_ref()?.get(epoch).map(Arc::clone)
    }

    fn epoch_is_current(&self, epoch: i64) -> bool {
        lock_recover(&self.state)
            .client
            .as_ref()
            .is_some_and(|client| client.epoch == epoch)
    }

    fn current_epoch(&self) -> i64 {
        lock_recover(&self.state)
            .client
            .as_ref()
            .map_or(0, |client| client.epoch)
    }

    fn with_client<T>(&self, epoch: i64, action: impl FnOnce(&Arc<Client>) -> T) -> Option<T> {
        let client = self.client_for_epoch(epoch)?;
        Some(action(&client))
    }

    fn with_client_audio<T>(
        &self,
        epoch: i64,
        action: impl FnOnce(&Arc<Client>, &mut AudioState) -> T,
    ) -> Option<T> {
        let client = self.client_for_epoch(epoch)?;
        let mut audio = lock_recover(&self.audio);
        if audio.epoch != epoch || !self.epoch_is_current(epoch) {
            return None;
        }
        Some(action(&client, &mut audio))
    }

    fn configure_client_audio<T>(
        &self,
        epoch: i64,
        action: impl FnOnce(&Arc<Client>, &mut AudioState) -> T,
    ) -> Option<T> {
        let client = self.client_for_epoch(epoch)?;
        let mut audio = lock_recover(&self.audio);
        if !self.epoch_is_current(epoch) {
            return None;
        }
        *audio = AudioState::for_epoch(epoch);
        Some(action(&client, &mut audio))
    }
}

fn stop_client_async(client: Arc<Client>) {
    let _ = std::thread::Builder::new()
        .name("st-client-stop".into())
        .spawn(move || client.stop());
}

#[derive(Default)]
struct AudioState {
    epoch: i64,
    decoder: Option<AudioDecoder>,
    enabled: bool,
}

impl AudioState {
    fn for_epoch(epoch: i64) -> Self {
        Self {
            epoch,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioConfig {
    packet_duration_ms: usize,
    frame_samples_per_channel: usize,
    max_missing_packets: usize,
}

impl AudioConfig {
    fn new(sample_rate: u32, channels: u8, packet_duration_ms: u8) -> Result<Self, String> {
        if sample_rate != AUDIO_SAMPLE_RATE || channels as usize != AUDIO_CHANNELS {
            return Err(format!(
                "audio unavailable: requires 48000 Hz stereo, server offered {sample_rate} Hz/{channels} channel(s)"
            ));
        }
        if !matches!(packet_duration_ms, 5 | 10 | 20 | 40 | 60) {
            return Err(format!(
                "audio unavailable: unsupported Opus packet duration {packet_duration_ms} ms"
            ));
        }
        let packet_duration_ms = packet_duration_ms as usize;
        Ok(Self {
            packet_duration_ms,
            frame_samples_per_channel: AUDIO_SAMPLE_RATE as usize * packet_duration_ms / 1_000,
            max_missing_packets: MAX_RECOVERY_MS / packet_duration_ms,
        })
    }

    fn frame_samples(self) -> usize {
        self.frame_samples_per_channel * AUDIO_CHANNELS
    }

    fn required_output_bytes(self) -> usize {
        self.frame_samples()
            .saturating_mul(self.max_missing_packets + 1)
            .saturating_mul(std::mem::size_of::<i16>())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecoveryPlan {
    InOrder,
    Recover(Vec<RecoveryStep>),
    HardResync,
    DropOld,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryStep {
    Redundant {
        index: usize,
        immediate_before_primary: bool,
    },
    Fec,
    Plc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoverySource {
    Redundant,
    Fec,
    Plc,
}

impl RecoverySource {
    fn is_synthetic(self) -> bool {
        matches!(self, Self::Fec | Self::Plc)
    }
}

fn plan_recovery(
    expected_seq: Option<u16>,
    primary_seq: u16,
    redundant_count: usize,
    config: AudioConfig,
) -> RecoveryPlan {
    let Some(expected_seq) = expected_seq else {
        return RecoveryPlan::HardResync;
    };
    let delta = primary_seq.wrapping_sub(expected_seq);
    if delta == 0 {
        return RecoveryPlan::InOrder;
    }
    if delta >= 0x8000 {
        return RecoveryPlan::DropOld;
    }
    let missing_packets = delta as usize;
    if missing_packets.saturating_mul(config.packet_duration_ms) > MAX_RECOVERY_MS {
        return RecoveryPlan::HardResync;
    }

    let mut steps = Vec::with_capacity(missing_packets);
    for chronological_index in 0..missing_packets {
        let distance_from_primary = missing_packets - chronological_index;
        if distance_from_primary <= redundant_count {
            steps.push(RecoveryStep::Redundant {
                index: redundant_count - distance_from_primary,
                immediate_before_primary: distance_from_primary == 1,
            });
        } else if distance_from_primary == 1 {
            steps.push(RecoveryStep::Fec);
        } else {
            steps.push(RecoveryStep::Plc);
        }
    }
    RecoveryPlan::Recover(steps)
}

struct AudioDecoder {
    decoder: opus::Decoder,
    config: AudioConfig,
    expected_seq: Option<u16>,
    scratch: Vec<i16>,
    output: Vec<i16>,
}

struct DecodedAudio<'a> {
    pcm: &'a [i16],
    hard_resync: bool,
}

impl AudioDecoder {
    fn new(config: AudioConfig) -> Result<Self, String> {
        let decoder = opus::Decoder::new(AUDIO_SAMPLE_RATE, opus::Channels::Stereo)
            .map_err(|error| format!("failed to create Opus decoder: {error}"))?;
        Ok(Self {
            decoder,
            config,
            expected_seq: None,
            scratch: vec![0; config.frame_samples()],
            output: Vec::with_capacity(config.frame_samples() * (config.max_missing_packets + 1)),
        })
    }

    fn reset(&mut self) -> Result<(), String> {
        self.decoder
            .reset_state()
            .map_err(|error| format!("failed to reset Opus decoder: {error}"))?;
        self.expected_seq = None;
        self.output.clear();
        Ok(())
    }

    fn decode_packet(&mut self, packet: &AudioPacket) -> Result<DecodedAudio<'_>, String> {
        self.output.clear();
        if packet.local_discontinuity {
            self.reset()?;
            if let Err(error) = self.decode_frame(&packet.data, false) {
                let _ = self.reset();
                return Err(format!("failed to decode Opus primary packet: {error}"));
            }
            fade_in_audio(&mut self.output, AUDIO_CHANNELS, audio_smoothing_frames());
            self.expected_seq = Some(packet.seq.wrapping_add(1));
            return Ok(DecodedAudio {
                pcm: &self.output,
                hard_resync: true,
            });
        }

        let mut previous_recovery = None;
        let hard_resync = match plan_recovery(
            self.expected_seq,
            packet.seq,
            packet.redundant_prev.len(),
            self.config,
        ) {
            RecoveryPlan::InOrder => false,
            RecoveryPlan::Recover(steps) => {
                for step in steps {
                    let start = self.output.len();
                    let source = self.recover_frame(step, packet)?;
                    let end = self.output.len();
                    if let Some((previous_start, previous_end, previous_source)) = previous_recovery
                    {
                        smooth_synthetic_to_next(
                            &mut self.output,
                            previous_start,
                            previous_end,
                            start,
                            end,
                            previous_source,
                        );
                    }
                    previous_recovery = Some((start, end, source));
                }
                false
            }
            RecoveryPlan::HardResync => {
                self.reset()?;
                true
            }
            RecoveryPlan::DropOld => {
                return Ok(DecodedAudio {
                    pcm: &[],
                    hard_resync: false,
                });
            }
        };

        let primary_start = self.output.len();
        if let Err(error) = self.decode_frame(&packet.data, false) {
            self.output.clear();
            let _ = self.reset();
            return Err(format!("failed to decode Opus primary packet: {error}"));
        }
        if let Some((previous_start, previous_end, previous_source)) = previous_recovery {
            let primary_end = self.output.len();
            smooth_synthetic_to_next(
                &mut self.output,
                previous_start,
                previous_end,
                primary_start,
                primary_end,
                previous_source,
            );
        }
        if hard_resync {
            fade_in_audio(&mut self.output, AUDIO_CHANNELS, audio_smoothing_frames());
        }
        self.expected_seq = Some(packet.seq.wrapping_add(1));
        Ok(DecodedAudio {
            pcm: &self.output,
            hard_resync,
        })
    }

    fn recover_frame(
        &mut self,
        step: RecoveryStep,
        packet: &AudioPacket,
    ) -> Result<RecoverySource, String> {
        match step {
            RecoveryStep::Redundant {
                index,
                immediate_before_primary,
            } => {
                let redundant = packet.redundant_prev.get(index).map(Vec::as_slice);
                if redundant.is_some_and(|data| self.decode_frame(data, false).is_ok()) {
                    Ok(RecoverySource::Redundant)
                } else if immediate_before_primary && self.decode_frame(&packet.data, true).is_ok()
                {
                    Ok(RecoverySource::Fec)
                } else {
                    self.decode_plc()
                }
            }
            RecoveryStep::Fec if self.decode_frame(&packet.data, true).is_ok() => {
                Ok(RecoverySource::Fec)
            }
            RecoveryStep::Fec | RecoveryStep::Plc => self.decode_plc(),
        }
    }

    fn decode_plc(&mut self) -> Result<RecoverySource, String> {
        self.decode_frame(&[], false)
            .map(|()| RecoverySource::Plc)
            .map_err(|error| format!("Opus packet-loss concealment failed: {error}"))
    }

    fn decode_frame(&mut self, data: &[u8], fec: bool) -> Result<(), String> {
        if !data.is_empty() && !fec {
            let packet_samples = self
                .decoder
                .get_nb_samples(data)
                .map_err(|error| error.to_string())?;
            if packet_samples != self.config.frame_samples_per_channel {
                return Err(format!(
                    "packet contains {packet_samples} samples/channel, expected {}",
                    self.config.frame_samples_per_channel
                ));
            }
        }
        let decoded = self
            .decoder
            .decode(data, &mut self.scratch, fec)
            .map_err(|error| error.to_string())?;
        if decoded != self.config.frame_samples_per_channel {
            return Err(format!(
                "decoder produced {decoded} samples/channel, expected {}",
                self.config.frame_samples_per_channel
            ));
        }
        self.output.extend_from_slice(&self.scratch);
        Ok(())
    }
}

fn audio_smoothing_frames() -> usize {
    AUDIO_SAMPLE_RATE as usize * AUDIO_SMOOTHING_MS / 1_000
}

fn smooth_audio_transition(
    previous: &mut [i16],
    current: &[i16],
    channels: usize,
    max_frames: usize,
) {
    if channels == 0 {
        return;
    }
    let frames = (previous.len() / channels)
        .min(current.len() / channels)
        .min(max_frames);
    if frames == 0 {
        return;
    }
    let previous_start = previous.len() - frames * channels;
    let denominator = (frames + 1) as i32;
    for frame in 0..frames {
        let current_weight = (frame + 1) as i32;
        let previous_weight = denominator - current_weight;
        for (channel, current_sample) in current.iter().take(channels).enumerate() {
            let previous_index = previous_start + frame * channels + channel;
            let previous_sample = previous[previous_index] as i32;
            previous[previous_index] = ((previous_sample * previous_weight
                + i32::from(*current_sample) * current_weight)
                / denominator) as i16;
        }
    }
}

fn smooth_synthetic_to_next(
    output: &mut [i16],
    previous_start: usize,
    previous_end: usize,
    next_start: usize,
    next_end: usize,
    previous_source: RecoverySource,
) {
    if !previous_source.is_synthetic()
        || previous_start > previous_end
        || previous_end > next_start
        || next_start > next_end
        || next_end > output.len()
    {
        return;
    }
    let (previous, next) = output.split_at_mut(next_start);
    smooth_audio_transition(
        &mut previous[previous_start..previous_end],
        &next[..next_end - next_start],
        AUDIO_CHANNELS,
        audio_smoothing_frames(),
    );
}

fn fade_in_audio(current: &mut [i16], channels: usize, max_frames: usize) {
    if channels == 0 {
        return;
    }
    let frames = (current.len() / channels).min(max_frames);
    let denominator = (frames + 1) as i32;
    for frame in 0..frames {
        let weight = (frame + 1) as i32;
        for channel in 0..channels {
            let index = frame * channels + channel;
            current[index] = (current[index] as i32 * weight / denominator) as i16;
        }
    }
}

static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);
static SESSIONS: OnceLock<Mutex<HashMap<i64, Arc<Session>>>> = OnceLock::new();
static DISCOVERY: OnceLock<Mutex<DiscoveryState>> = OnceLock::new();

#[derive(Default)]
struct DiscoveryState {
    active: Option<LanDiscovery>,
    handles: HashSet<i64>,
}

fn sessions() -> &'static Mutex<HashMap<i64, Arc<Session>>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn discovery() -> &'static Mutex<DiscoveryState> {
    DISCOVERY.get_or_init(Default::default)
}

fn update_discovery_handles(handles: &mut HashSet<i64>, handle: i64, enabled: bool) -> bool {
    if enabled {
        handles.insert(handle);
    } else {
        handles.remove(&handle);
    }
    handles.is_empty()
}

fn set_discovery_enabled(handle: i64, enabled: bool) {
    let mut state = lock_recover(discovery());
    let should_stop = update_discovery_handles(&mut state.handles, handle, enabled);
    if enabled && state.active.is_none() {
        state.active = LanDiscovery::start().ok();
    } else if should_stop {
        state.active.take();
    }
}

fn session(handle: jlong) -> Option<Arc<Session>> {
    lock_recover(sessions()).get(&handle).cloned()
}

fn java_string(env: &JNIEnv<'_>, value: &str) -> jstring {
    env.new_string(value)
        .map(|string| string.into_raw())
        .unwrap_or(ptr::null_mut())
}

fn input_capability_flags(capabilities: InputCapabilities) -> u8 {
    u8::from(capabilities.mouse_absolute)
        | (u8::from(capabilities.mouse_relative) << 1)
        | (u8::from(capabilities.keyboard) << 2)
        | (u8::from(capabilities.separate_cursor) << 3)
        | (u8::from(capabilities.hover_capture) << 4)
        | (u8::from(capabilities.cursor_position_reliable) << 5)
        | (u8::from(capabilities.text_input) << 6)
}

fn controller_state_code(state: ControllerState) -> u8 {
    match state {
        ControllerState::Unavailable => 0,
        ControllerState::Available => 1,
        ControllerState::OwnedByYou => 2,
        ControllerState::OwnedByOther => 3,
    }
}

/// JNI cursor snapshot v2. All integers are little-endian. The header is
/// `[version:u8][present_flags:u8][transport_generation:u32]`; present sections follow in bit order:
/// capabilities `[revision:u64][flags:u8]`, controller
/// `[revision:u64][state:u8]`, shape
/// `[revision:u64][serial:u64][width:u16][height:u16][hotspot_x:u16]
/// [hotspot_y:u16][rgba_len:u32][premultiplied_rgba8...]`, and state
/// `[revision:u64][serial:u64][x:i32][y:i32][flags:u8]` where state flag bit 0
/// is visible and bit 1 is app_grab. Capability flag bit 6 is reliable committed
/// Unicode text input.
fn serialize_control_snapshot(snapshot: ControlSnapshot) -> Vec<u8> {
    let mut present = 0u8;
    if snapshot.input_capabilities.is_some() {
        present |= SNAPSHOT_CAPABILITIES;
    }
    if snapshot.controller_state.is_some() {
        present |= SNAPSHOT_CONTROLLER_STATE;
    }
    if snapshot.cursor_shape.is_some() {
        present |= SNAPSHOT_CURSOR_SHAPE;
    }
    if snapshot.cursor_state.is_some() {
        present |= SNAPSHOT_CURSOR_STATE;
    }

    let shape_len = snapshot
        .cursor_shape
        .as_ref()
        .map_or(0, |shape| shape.value.rgba.len());
    let mut bytes = Vec::with_capacity(64 + shape_len);
    bytes.extend_from_slice(&[CURSOR_SNAPSHOT_VERSION, present]);
    bytes.extend_from_slice(&snapshot.transport_generation.to_le_bytes());
    if let Some(capabilities) = snapshot.input_capabilities {
        bytes.extend_from_slice(&capabilities.revision.to_le_bytes());
        bytes.push(input_capability_flags(capabilities.value));
    }
    if let Some(controller) = snapshot.controller_state {
        bytes.extend_from_slice(&controller.revision.to_le_bytes());
        bytes.push(controller_state_code(controller.value));
    }
    if let Some(shape) = snapshot.cursor_shape {
        bytes.extend_from_slice(&shape.revision.to_le_bytes());
        bytes.extend_from_slice(&shape.value.serial.to_le_bytes());
        bytes.extend_from_slice(&shape.value.width.to_le_bytes());
        bytes.extend_from_slice(&shape.value.height.to_le_bytes());
        bytes.extend_from_slice(&shape.value.hotspot_x.to_le_bytes());
        bytes.extend_from_slice(&shape.value.hotspot_y.to_le_bytes());
        bytes.extend_from_slice(&(shape.value.rgba.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&shape.value.rgba);
    }
    if let Some(state) = snapshot.cursor_state {
        bytes.extend_from_slice(&state.revision.to_le_bytes());
        bytes.extend_from_slice(&state.value.serial.to_le_bytes());
        bytes.extend_from_slice(&state.value.x.to_le_bytes());
        bytes.extend_from_slice(&state.value.y.to_le_bytes());
        bytes.push(u8::from(state.value.visible) | (u8::from(state.value.app_grab) << 1));
    }
    bytes
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeCreate(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jlong {
    jni_boundary!(0, {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        lock_recover(sessions()).insert(handle, Arc::new(Session::new()));
        handle
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeStart(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    server: JString<'_>,
    token: JString<'_>,
    refresh_millihz: jint,
    api_url: JString<'_>,
    client_peer_id: JString<'_>,
    host_peer_id: JString<'_>,
    request_nonce: jlong,
) -> jstring {
    jni_boundary!(java_string(&env, "native session failure"), {
        let Some(session) = session(handle) else {
            return java_string(&env, "invalid native handle");
        };
        let server: String = match env.get_string(&server) {
            Ok(value) => value.into(),
            Err(error) => return java_string(&env, &error.to_string()),
        };
        let token: String = match env.get_string(&token) {
            Ok(value) => value.into(),
            Err(error) => return java_string(&env, &error.to_string()),
        };
        let api_url: String = match env.get_string(&api_url) {
            Ok(value) => value.into(),
            Err(error) => return java_string(&env, &error.to_string()),
        };
        let client_peer_id: String = match env.get_string(&client_peer_id) {
            Ok(value) => value.into(),
            Err(error) => return java_string(&env, &error.to_string()),
        };
        let host_peer_id: String = match env.get_string(&host_peer_id) {
            Ok(value) => value.into(),
            Err(error) => return java_string(&env, &error.to_string()),
        };
        let api = if api_url.is_empty() && client_peer_id.is_empty() && host_peer_id.is_empty() {
            None
        } else {
            let Ok(request_nonce) = u64::try_from(request_nonce) else {
                return java_string(&env, "invalid API tunnel request nonce");
            };
            Some(ApiConnectionConfig {
                api_url,
                client_peer_id,
                host_peer_id,
                request_nonce,
            })
        };

        let epoch = session.invalidate();
        match Client::start(ClientConfig {
            server: (!server.is_empty()).then_some(server),
            api,
            token,
            refresh_millihz: refresh_millihz.max(0) as u32,
        }) {
            Ok(client) => match session.install_client(epoch, client) {
                Ok(()) => ptr::null_mut(),
                Err(client) => {
                    stop_client_async(client);
                    java_string(&env, "session start was superseded")
                }
            },
            Err(error) => java_string(&env, &error),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeGetSessionEpoch(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jlong {
    jni_boundary!(0, {
        session(handle).map_or(0, |session| session.current_epoch())
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeSetDiscoveryEnabled(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    enabled: jboolean,
) {
    jni_boundary!((), {
        if session(handle).is_some() {
            set_discovery_enabled(handle, enabled != 0);
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeStop(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    jni_boundary!((), {
        if let Some(session) = session(handle) {
            session.invalidate();
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeDestroy(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    jni_boundary!((), {
        set_discovery_enabled(handle, false);
        let (removed, last_session) = {
            let mut sessions = lock_recover(sessions());
            let removed = sessions.remove(&handle);
            (removed, sessions.is_empty())
        };
        if let Some(session) = removed {
            session.invalidate();
        }
        if last_session {
            let mut discovery = lock_recover(discovery());
            discovery.handles.clear();
            discovery.active.take();
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeGetStatus(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jstring {
    jni_boundary!(java_string(&env, "error: native status failure"), {
        let status = session(handle)
            .and_then(|session| session.client())
            .map(|client| client.status())
            .unwrap_or_else(|| "disconnected".into());
        java_string(&env, &status)
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeGetAudioQueueStats(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
) -> jlong {
    jni_boundary!(0, {
        let Some(stats) = session(handle)
            .and_then(|session| session.with_client(epoch, |client| client.audio_queue_stats()))
        else {
            return 0;
        };
        let occupancy = stats.occupancy.min(u32::MAX as usize) as u64;
        let local_drops = stats.local_drops.min(u32::MAX as u64);
        ((local_drops << 32) | occupancy) as jlong
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeTakeStreamConfig(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
) -> jintArray {
    jni_boundary!(ptr::null_mut(), {
        let Some(config) = session(handle)
            .and_then(|session| session.with_client(epoch, |client| client.take_stream_event()))
            .flatten()
        else {
            return ptr::null_mut();
        };
        let Ok(array) = env.new_int_array(12) else {
            return ptr::null_mut();
        };
        let values = [
            config.transport_generation as jint,
            config.generation as jint,
            (config.video_epoch >> 32) as u32 as jint,
            config.video_epoch as u32 as jint,
            config.width as jint,
            config.height as jint,
            config.cursor_width as jint,
            config.cursor_height as jint,
            config.framerate as jint,
            config.audio_sample_rate as jint,
            config.audio_channels as jint,
            config.packet_duration_ms as jint,
        ];
        if env.set_int_array_region(&array, 0, &values).is_err() {
            return ptr::null_mut();
        }
        array.into_raw()
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativePollCursorSnapshot(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
) -> jbyteArray {
    jni_boundary!(ptr::null_mut(), {
        let Some(snapshot) = session(handle)
            .and_then(|session| session.with_client(epoch, |client| client.take_control_snapshot()))
            .flatten()
        else {
            return ptr::null_mut();
        };
        env.byte_array_from_slice(&serialize_control_snapshot(snapshot))
            .map(|array| array.into_raw())
            .unwrap_or(ptr::null_mut())
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeConfigureAudio(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
    sample_rate: jint,
    channels: jint,
    packet_duration_ms: jint,
) -> jstring {
    jni_boundary!(java_string(&env, "native audio configuration failure"), {
        let Some(session) = session(handle) else {
            return java_string(&env, "invalid native handle");
        };
        let Some(result) = session.configure_client_audio(epoch, |client, audio| {
            let _ = client.set_audio_enabled(false);
            client.clear_audio_queue();

            let config = AudioConfig::new(
                sample_rate.max(0) as u32,
                u8::try_from(channels).unwrap_or(0),
                u8::try_from(packet_duration_ms).unwrap_or(0),
            )?;
            audio.decoder = Some(AudioDecoder::new(config)?);
            Ok::<(), String>(())
        }) else {
            return java_string(&env, "stale session epoch");
        };
        match result {
            Ok(()) => ptr::null_mut(),
            Err(error) => java_string(&env, &error),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeResetAudio(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
) {
    jni_boundary!((), {
        let Some(session) = session(handle) else {
            return;
        };
        let _ = session.with_client_audio(epoch, |client, audio| {
            client.clear_audio_queue();
            if let Some(decoder) = audio.decoder.as_mut() {
                let _ = decoder.reset();
            }
        });
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeSetAudioEnabled(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
    enabled: jboolean,
) -> jstring {
    jni_boundary!(java_string(&env, "native audio state failure"), {
        let Some(session) = session(handle) else {
            return java_string(&env, "invalid native handle");
        };
        let enabled = enabled != 0;
        let Some(result) = session.with_client_audio(epoch, |client, audio| {
            if enabled {
                let Some(decoder) = audio.decoder.as_mut() else {
                    return Err("audio decoder is not configured".into());
                };
                decoder.reset()?;
                audio.enabled = true;
                if let Err(error) = client.set_audio_enabled(true) {
                    audio.enabled = false;
                    return Err(error);
                }
            } else {
                let result = client.set_audio_enabled(false);
                audio.enabled = false;
                if let Some(decoder) = audio.decoder.as_mut() {
                    let _ = decoder.reset();
                }
                result?;
            }
            Ok::<(), String>(())
        }) else {
            return java_string(&env, "stale session epoch");
        };
        match result {
            Ok(()) => ptr::null_mut(),
            Err(error) => java_string(&env, &error),
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeFillAudio(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
    buffer: JByteBuffer<'_>,
    timeout_ms: jint,
) -> jint {
    jni_boundary!(AUDIO_FILL_DECODE_ERROR, {
        let Ok(capacity) = env.get_direct_buffer_capacity(&buffer) else {
            return AUDIO_FILL_BUFFER_ERROR;
        };
        let Ok(address) = env.get_direct_buffer_address(&buffer) else {
            return AUDIO_FILL_BUFFER_ERROR;
        };
        let Some(session) = session(handle) else {
            return 0;
        };
        session
            .with_client_audio(epoch, |client, audio| {
                if !audio.enabled {
                    return 0;
                }
                let Some(decoder) = audio.decoder.as_mut() else {
                    return AUDIO_FILL_BUFFER_ERROR;
                };
                if capacity < decoder.config.required_output_bytes() {
                    return AUDIO_FILL_BUFFER_ERROR;
                }
                let timeout =
                    Duration::from_millis((timeout_ms.max(0) as u64).min(MAX_AUDIO_POLL_MS));
                let Some(packet) = client.recv_audio_packet(timeout) else {
                    return 0;
                };
                let decoded = match decoder.decode_packet(&packet) {
                    Ok(decoded) => decoded,
                    Err(_) => return AUDIO_FILL_DECODE_ERROR,
                };
                let byte_len = std::mem::size_of_val(decoded.pcm);
                if byte_len == 0 {
                    return 0;
                }
                if byte_len >= AUDIO_FILL_RESYNC_FLAG as usize {
                    return AUDIO_FILL_BUFFER_ERROR;
                }
                // DirectByteBuffer storage remains valid for this JNI call and capacity was checked above.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        decoded.pcm.as_ptr().cast::<u8>(),
                        address,
                        byte_len,
                    );
                }
                byte_len as jint
                    | if decoded.hard_resync {
                        AUDIO_FILL_RESYNC_FLAG
                    } else {
                        0
                    }
            })
            .unwrap_or(0)
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativePollAccessUnit(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
    timeout_ms: jint,
) -> jbyteArray {
    jni_boundary!(ptr::null_mut(), {
        let Some(unit) = session(handle)
            .and_then(|session| {
                session.with_client(epoch, |client| {
                    client.recv_access_unit(Duration::from_millis(timeout_ms.max(0) as u64))
                })
            })
            .flatten()
        else {
            return ptr::null_mut();
        };
        env.byte_array_from_slice(&unit.data)
            .map(|array| array.into_raw())
            .unwrap_or(ptr::null_mut())
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeSendAbsolute(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
    x: jint,
    y: jint,
    buttons: jint,
) -> jboolean {
    jni_boundary!(0, {
        let Some(sent) = session(handle).and_then(|session| {
            session.with_client(epoch, |client| {
                client
                    .send_mouse_absolute(
                        x.clamp(0, u16::MAX as jint) as u16,
                        y.clamp(0, u16::MAX as jint) as u16,
                        buttons.clamp(0, u8::MAX as jint) as u8,
                    )
                    .is_ok()
            })
        }) else {
            return 0;
        };
        u8::from(sent)
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeSendTrackpadDelta(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
    dx: jint,
    dy: jint,
    buttons: jint,
) -> jlong {
    jni_boundary!(TRACKPAD_ROUTE_REJECTED, {
        let Some(route) = session(handle).and_then(|session| {
            session.with_client(epoch, |client| {
                client
                    .send_trackpad_delta(
                        dx.clamp(i16::MIN as jint, i16::MAX as jint) as i16,
                        dy.clamp(i16::MIN as jint, i16::MAX as jint) as i16,
                        buttons.clamp(0, u8::MAX as jint) as u8,
                    )
                    .ok()
            })
        }) else {
            return TRACKPAD_ROUTE_REJECTED;
        };
        match route {
            Some(TrackpadRoute::Relative) => TRACKPAD_ROUTE_RELATIVE,
            Some(TrackpadRoute::Absolute { x, y }) => {
                TRACKPAD_ROUTE_ABSOLUTE
                    | ((x as jlong) << TRACKPAD_ROUTE_X_SHIFT)
                    | ((y as jlong) << TRACKPAD_ROUTE_Y_SHIFT)
            }
            None => TRACKPAD_ROUTE_REJECTED,
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeSendMouseButtons(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
    buttons: jint,
) -> jboolean {
    jni_boundary!(0, {
        let Some(sent) = session(handle).and_then(|session| {
            session.with_client(epoch, |client| {
                client
                    .send_mouse_buttons(buttons.clamp(0, u8::MAX as jint) as u8)
                    .is_ok()
            })
        }) else {
            return 0;
        };
        u8::from(sent)
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeSendMouseWheel(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
    delta_x: jint,
    delta_y: jint,
    buttons: jint,
) -> jboolean {
    jni_boundary!(0, {
        let Some(sent) = session(handle).and_then(|session| {
            session.with_client(epoch, |client| {
                client
                    .send_mouse_wheel(
                        delta_x.clamp(i16::MIN as jint, i16::MAX as jint) as i16,
                        delta_y.clamp(i16::MIN as jint, i16::MAX as jint) as i16,
                        buttons.clamp(0, u8::MAX as jint) as u8,
                    )
                    .is_ok()
            })
        }) else {
            return 0;
        };
        u8::from(sent)
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeSendKeyboardState(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
    pressed: JByteArray<'_>,
) -> jboolean {
    jni_boundary!(0, {
        let Ok(bytes) = env.convert_byte_array(pressed) else {
            return 0;
        };
        let Ok(pressed): Result<[u8; KEYBOARD_STATE_BYTES], _> = bytes.try_into() else {
            return 0;
        };
        let Some(sent) = session(handle).and_then(|session| {
            session.with_client(epoch, |client| client.send_keyboard_state(pressed).is_ok())
        }) else {
            return 0;
        };
        u8::from(sent)
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeSendTextInput(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
    text: JString<'_>,
) -> jboolean {
    jni_boundary!(0, {
        let Ok(text) = env.get_string(&text) else {
            return 0;
        };
        let text: String = text.into();
        let Some(sent) = session(handle).and_then(|session| {
            session.with_client(epoch, |client| client.send_text_input(text).is_ok())
        }) else {
            return 0;
        };
        u8::from(sent)
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeRequestKeyframe(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
) {
    jni_boundary!((), {
        if let Some(session) = session(handle) {
            let _ = session.with_client(epoch, |client| client.request_keyframe());
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeAcceptStreamGeneration(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    epoch: jlong,
    generation: jint,
    video_epoch: jlong,
) -> jboolean {
    jni_boundary!(0, {
        if let Some(session) = session(handle) {
            return session
                .with_client(epoch, |client| {
                    client.accept_stream_generation(
                        generation.max(0) as u32,
                        video_epoch.max(0) as u64,
                    )
                })
                .map_or(0, u8::from);
        }
        0
    })
}

#[no_mangle]
pub extern "system" fn Java_io_kubemaxx_st_NativeBridge_nativeGetDiscoveredServers(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    token: JString<'_>,
) -> jstring {
    jni_boundary!(java_string(&env, "[]"), {
        if session(handle).is_none() {
            return java_string(&env, "[]");
        }
        let token: String = match env.get_string(&token) {
            Ok(value) => value.into(),
            Err(_) => return java_string(&env, "[]"),
        };
        let servers = lock_recover(discovery())
            .active
            .as_ref()
            .map(|discovery| discovery.snapshot(Some(token.trim())))
            .unwrap_or_default();
        let value: Vec<_> = servers
            .into_iter()
            .map(|server| {
                serde_json::json!({
                    "hostname": server.hostname,
                    "address": server.address,
                    "peer_id": server.peer_id,
                })
            })
            .collect();
        java_string(
            &env,
            &serde_json::to_string(&value).unwrap_or_else(|_| "[]".into()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn config(packet_duration_ms: u8) -> AudioConfig {
        AudioConfig::new(AUDIO_SAMPLE_RATE, AUDIO_CHANNELS as u8, packet_duration_ms).unwrap()
    }

    fn encoded_silence(config: AudioConfig) -> Vec<u8> {
        let pcm = vec![0i16; config.frame_samples()];
        let mut encoded = vec![0u8; 4_000];
        let mut encoder = opus::Encoder::new(
            AUDIO_SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::LowDelay,
        )
        .unwrap();
        let encoded_len = encoder.encode(&pcm, &mut encoded).unwrap();
        encoded.truncate(encoded_len);
        encoded
    }

    #[test]
    fn epoch_value_rejects_stale_client_lookup() {
        let client = EpochValue {
            epoch: 17,
            value: "current-client",
        };
        assert_eq!(client.get(17), Some(&"current-client"));
        assert_eq!(client.get(16), None);
        assert_eq!(client.get(18), None);
    }

    #[test]
    fn session_invalidation_advances_epoch_without_a_client() {
        let session = Session::new();
        assert_eq!(session.current_epoch(), 0);
        assert_eq!(session.invalidate(), 1);
        assert_eq!(session.invalidate(), 2);
        assert_eq!(session.current_epoch(), 0);
    }

    #[test]
    fn capability_snapshot_exposes_reliable_text_input_bit() {
        assert_eq!(
            input_capability_flags(InputCapabilities {
                mouse_absolute: true,
                mouse_relative: true,
                keyboard: true,
                separate_cursor: true,
                hover_capture: true,
                cursor_position_reliable: true,
                text_input: true,
            }),
            0x7f
        );
        assert_eq!(
            input_capability_flags(InputCapabilities {
                keyboard: true,
                ..InputCapabilities::default()
            }),
            1 << 2
        );
    }

    #[test]
    fn control_snapshot_carries_transport_generation_even_when_empty() {
        let bytes = serialize_control_snapshot(ControlSnapshot {
            transport_generation: 9,
            ..ControlSnapshot::default()
        });
        assert_eq!(bytes, vec![CURSOR_SNAPSHOT_VERSION, 0, 9, 0, 0, 0]);
    }

    #[test]
    fn blocking_media_receives_do_not_hold_session_state() {
        let session = Arc::new(Session::new());
        let epoch = session.invalidate();
        let client = Client::start(ClientConfig {
            server: Some("127.0.0.1:1".into()),
            api: None,
            token: "test-token".into(),
            refresh_millihz: 60_000,
        })
        .unwrap();
        assert!(session.install_client(epoch, client).is_ok());

        let (video_entered_tx, video_entered_rx) = mpsc::channel();
        let video_session = Arc::clone(&session);
        let video_waiter = std::thread::spawn(move || {
            video_session.with_client(epoch, |client| {
                video_entered_tx.send(()).unwrap();
                client.recv_access_unit(Duration::from_millis(500))
            })
        });
        video_entered_rx.recv().unwrap();
        let (video_probe_tx, video_probe_rx) = mpsc::channel();
        let video_probe_session = Arc::clone(&session);
        std::thread::spawn(move || {
            video_probe_tx
                .send(video_probe_session.current_epoch())
                .unwrap();
        });
        assert_eq!(
            video_probe_rx.recv_timeout(Duration::from_millis(250)),
            Ok(epoch)
        );
        video_waiter.join().unwrap();

        *lock_recover(&session.audio) = AudioState::for_epoch(epoch);
        let (audio_entered_tx, audio_entered_rx) = mpsc::channel();
        let audio_session = Arc::clone(&session);
        let audio_waiter = std::thread::spawn(move || {
            audio_session.with_client_audio(epoch, |client, _| {
                audio_entered_tx.send(()).unwrap();
                client.recv_audio_packet(Duration::from_millis(500))
            })
        });
        audio_entered_rx.recv().unwrap();
        let (audio_probe_tx, audio_probe_rx) = mpsc::channel();
        let audio_probe_session = Arc::clone(&session);
        std::thread::spawn(move || {
            audio_probe_tx
                .send(audio_probe_session.current_epoch())
                .unwrap();
        });
        assert_eq!(
            audio_probe_rx.recv_timeout(Duration::from_millis(250)),
            Ok(epoch)
        );
        audio_waiter.join().unwrap();

        assert_eq!(session.invalidate(), epoch + 1);
        assert!(session
            .with_client_audio(epoch, |_, _| unreachable!())
            .is_none());
    }

    #[test]
    fn audio_duration_controls_pcm_and_recovery_budget() {
        let five_ms = config(5);
        assert_eq!(five_ms.frame_samples_per_channel, 240);
        assert_eq!(five_ms.frame_samples(), 480);
        assert_eq!(five_ms.max_missing_packets, 12);
        assert_eq!(five_ms.required_output_bytes(), 12_480);

        let twenty_ms = config(20);
        assert_eq!(twenty_ms.frame_samples_per_channel, 960);
        assert_eq!(twenty_ms.max_missing_packets, 3);
        assert_eq!(twenty_ms.required_output_bytes(), 15_360);
    }

    #[test]
    fn recovery_uses_oldest_first_redundancy_then_fec_or_plc() {
        assert_eq!(
            plan_recovery(Some(10), 13, 2, config(5)),
            RecoveryPlan::Recover(vec![
                RecoveryStep::Plc,
                RecoveryStep::Redundant {
                    index: 0,
                    immediate_before_primary: false,
                },
                RecoveryStep::Redundant {
                    index: 1,
                    immediate_before_primary: true,
                },
            ])
        );
        assert_eq!(
            plan_recovery(Some(20), 22, 0, config(20)),
            RecoveryPlan::Recover(vec![RecoveryStep::Plc, RecoveryStep::Fec])
        );
        assert!(!RecoverySource::Redundant.is_synthetic());
        assert!(RecoverySource::Fec.is_synthetic());
        assert!(RecoverySource::Plc.is_synthetic());
    }

    #[test]
    fn recovery_sequence_order_wraps_and_drops_old_packets() {
        assert_eq!(
            plan_recovery(Some(u16::MAX), 1, 2, config(5)),
            RecoveryPlan::Recover(vec![
                RecoveryStep::Redundant {
                    index: 0,
                    immediate_before_primary: false,
                },
                RecoveryStep::Redundant {
                    index: 1,
                    immediate_before_primary: true,
                },
            ])
        );
        assert_eq!(
            plan_recovery(Some(1), u16::MAX, 0, config(5)),
            RecoveryPlan::DropOld
        );
    }

    #[test]
    fn recovery_hard_resyncs_only_beyond_sixty_ms() {
        assert!(matches!(
            plan_recovery(Some(100), 112, 0, config(5)),
            RecoveryPlan::Recover(steps) if steps.len() == 12
        ));
        assert_eq!(
            plan_recovery(Some(100), 113, 4, config(5)),
            RecoveryPlan::HardResync
        );
        assert!(matches!(
            plan_recovery(Some(100), 103, 0, config(20)),
            RecoveryPlan::Recover(steps) if steps.len() == 3
        ));
        assert_eq!(
            plan_recovery(Some(100), 104, 4, config(20)),
            RecoveryPlan::HardResync
        );
    }

    #[test]
    fn local_discontinuity_decodes_only_primary_and_resets_sequence() {
        let config = config(5);
        let encoded = encoded_silence(config);
        let mut decoder = AudioDecoder::new(config).unwrap();
        decoder
            .decode_packet(&AudioPacket {
                seq: 7,
                data: encoded.clone(),
                redundant_prev: Vec::new(),
                local_discontinuity: false,
            })
            .unwrap();

        let output = decoder
            .decode_packet(&AudioPacket {
                seq: 20,
                data: encoded.clone(),
                redundant_prev: vec![encoded.clone(), encoded],
                local_discontinuity: true,
            })
            .unwrap();
        assert!(output.hard_resync);
        assert_eq!(output.pcm.len(), config.frame_samples());
        assert_eq!(decoder.expected_seq, Some(21));
    }

    #[test]
    fn smoothing_keeps_interleaved_channels_isolated_and_next_frame_exact() {
        let mut previous = [1_000, -10_000, 3_000, -12_000];
        let current = [4_000, 2_000, 6_000, 4_000];

        smooth_audio_transition(&mut previous, &current, 2, 2);

        assert_eq!(previous, [2_000, -6_000, 3_666, -2_666]);
        assert_eq!(current, [4_000, 2_000, 6_000, 4_000]);
    }

    #[test]
    fn hard_resync_fades_in_without_a_discarded_previous_tail() {
        let mut current = [3_000, -12_000, 6_000, -15_000];

        fade_in_audio(&mut current, 2, 2);

        assert_eq!(current, [1_000, -4_000, 4_000, -10_000]);
    }

    #[test]
    fn every_synthetic_boundary_is_smoothed_without_changing_redundancy() {
        let mut output = [
            1_000, -10_000, 3_000, -12_000, // PLC
            4_000, 2_000, 6_000, 4_000, // FEC
            7_000, -7_000, 8_000, -8_000, // verbatim redundancy
        ];
        let redundancy = output[8..].to_vec();

        smooth_synthetic_to_next(&mut output, 0, 4, 4, 8, RecoverySource::Plc);
        smooth_synthetic_to_next(&mut output, 4, 8, 8, 12, RecoverySource::Fec);

        assert_ne!(&output[0..4], &[1_000, -10_000, 3_000, -12_000]);
        assert_ne!(&output[4..8], &[4_000, 2_000, 6_000, 4_000]);
        assert_eq!(&output[8..], redundancy);
    }

    #[test]
    fn jni_boundary_returns_its_safe_sentinel_after_panic() {
        let value = jni_boundary!(17, { panic!("test JNI panic") });
        assert_eq!(value, 17);
    }

    #[test]
    fn poisoned_android_mutex_is_recovered_deliberately() {
        let mutex = Arc::new(Mutex::new(3));
        let poisoned = Arc::clone(&mutex);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison test mutex");
        })
        .join();

        *lock_recover(&mutex) = 4;
        assert_eq!(*lock_recover(&mutex), 4);
    }

    #[test]
    fn opus_decode_produces_exact_negotiated_slice() {
        let config = config(5);
        let encoded = encoded_silence(config);

        let mut decoder = AudioDecoder::new(config).unwrap();
        let output = decoder
            .decode_packet(&AudioPacket {
                seq: u16::MAX,
                data: encoded.clone(),
                redundant_prev: Vec::new(),
                local_discontinuity: false,
            })
            .unwrap();
        assert_eq!(output.pcm.len(), config.frame_samples());
        assert!(output.hard_resync);

        let output = decoder
            .decode_packet(&AudioPacket {
                seq: 0,
                data: encoded.clone(),
                redundant_prev: Vec::new(),
                local_discontinuity: false,
            })
            .unwrap();
        assert_eq!(output.pcm.len(), config.frame_samples());
        assert!(!output.hard_resync);

        decoder.reset().unwrap();
        assert_eq!(decoder.expected_seq, None);
        let output = decoder
            .decode_packet(&AudioPacket {
                seq: 400,
                data: encoded,
                redundant_prev: Vec::new(),
                local_discontinuity: false,
            })
            .unwrap();
        assert!(output.hard_resync);
    }
}
