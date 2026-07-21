use crate::debug_state::{mono_micros, ConnectionDebugState};
use crate::decode::VideoDecoder;
use crate::transport::{AudioPacket, MediaReceiver, ReceivedData, TransportWindowStats};
use crate::video_frame::{FrameDebugTiming, NativeSurfaceControl, VideoFrameBuffer};
use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use st_protocol::{ControlMessage, StreamConfig, TransportFeedback};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::{Duration, Instant};

/// Keep shutdown and feedback/recovery checks responsive without waking an
/// otherwise idle media thread every 2 ms. The 20 ms cap matches the urgent
/// feedback debounce; queued video deadlines can still shorten it to sub-ms.
const MEDIA_MAINTENANCE_INTERVAL: Duration = Duration::from_millis(20);
const LIVE_DECODE_OUTPUT_TIMEOUT: Duration = Duration::from_secs(5);

struct RepaintPacer {
    min_interval: Option<Duration>,
    last_request: Option<Instant>,
    immediate_pending: bool,
}

impl RepaintPacer {
    fn new(refresh_millihz: Option<u32>) -> Self {
        Self {
            min_interval: refresh_millihz.and_then(|refresh| {
                if refresh == 0 {
                    None
                } else {
                    Some(Duration::from_secs_f64(1000.0 / refresh as f64))
                }
            }),
            last_request: None,
            immediate_pending: false,
        }
    }

    fn request_video(&mut self, ctx: &egui::Context, immediate: bool, repaint_pending: bool) {
        if !repaint_pending {
            self.immediate_pending = false;
        } else if !immediate || self.immediate_pending {
            return;
        }

        if immediate {
            self.last_request = Some(Instant::now());
            self.immediate_pending = true;
            ctx.request_repaint();
        } else {
            self.request(ctx);
        }
    }

    fn request(&mut self, ctx: &egui::Context) {
        let Some(min_interval) = self.min_interval else {
            self.last_request = Some(Instant::now());
            ctx.request_repaint();
            return;
        };

        let now = Instant::now();
        let Some(last_request) = self.last_request else {
            self.last_request = Some(now);
            ctx.request_repaint();
            return;
        };

        let elapsed = now.saturating_duration_since(last_request);
        if elapsed >= min_interval {
            self.last_request = Some(now);
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(min_interval - elapsed);
        }
    }

    fn set_refresh_millihz(&mut self, refresh_millihz: Option<u32>) {
        self.min_interval = refresh_millihz.and_then(|refresh| {
            (refresh > 0).then(|| Duration::from_secs_f64(1000.0 / refresh as f64))
        });
        self.last_request = None;
        self.immediate_pending = false;
    }
}

struct QueuedVideoFrame {
    present_at: Instant,
    frame_id: Option<u32>,
    frame: VideoFrameBuffer,
}

#[derive(Default)]
struct DueVideoFrames {
    present: Option<VideoFrameBuffer>,
    dropped: Vec<VideoFrameBuffer>,
}

struct VideoPlayoutBuffer {
    /// Current effective scheduling delay. With adaptation enabled this floats
    /// between `delay_floor` and `delay_ceiling` driven by measured interarrival
    /// jitter; with a user-forced delay (`ST_CLIENT_VIDEO_JITTER_MS`) it is fixed
    /// and `adaptive` is false.
    min_delay: Duration,
    /// Lower bound on the playout delay — the proven low-latency baseline
    /// (~one frame). Adaptation never drops below this, so a clean network sees
    /// exactly today's latency; it only adds headroom above it under jitter.
    delay_floor: Duration,
    /// Upper bound on how much headroom jitter can buy, so a bad path cannot
    /// inflate latency without limit.
    delay_ceiling: Duration,
    adaptive: bool,
    /// EWMA of |interarrival − frame_interval| in seconds (RFC 3550-style).
    jitter_secs: f64,
    last_arrival: Option<Instant>,
    frame_interval: Option<Duration>,
    max_queued_frames: usize,
    queued: VecDeque<QueuedVideoFrame>,
    last_scheduled_at: Option<Instant>,
    last_presented_frame_id: Option<u32>,
}

impl VideoPlayoutBuffer {
    // Grow the delay immediately on a jitter spike (avoid underrun/stutter) but
    // shrink it slowly when the path calms (avoid collapsing headroom and
    // fast-forwarding). Classic adaptive de-jitter asymmetry.
    //
    // Latency-first tuning: 2.5× headroom (was 3×) buys a little less buffer per
    // unit jitter, and a 1/12 shrink gain (was 1/16) returns to the floor a bit
    // faster once the path calms — so dec→present settles lower after a transient
    // (e.g. bufferbloat clearing). Still grow-fast/shrink-slow; the multiplier
    // stays high enough to cover real jitter without underrunning into stutter.
    const JITTER_GAIN: f64 = 1.0 / 16.0;
    const DELAY_SHRINK_GAIN: f64 = 1.0 / 12.0;
    const JITTER_MULTIPLIER: f64 = 2.5;
    // Cap a single interarrival sample's contribution so one large legitimate
    // gap (idle → motion, frame_id skip) can't blow up the estimate.
    const MAX_SAMPLE_DEVIATION: Duration = Duration::from_millis(100);

    fn new(stream_fps: u16) -> Self {
        let (floor, adaptive) = configured_video_jitter_delay(stream_fps);
        let frame_interval = if stream_fps > 0 {
            Some(Duration::from_secs_f64(1.0 / f64::from(stream_fps)))
        } else {
            None
        };
        let delay_ceiling = adaptive_delay_ceiling(floor, frame_interval);
        let (configured_max, max_is_explicit) = configured_video_jitter_max_frames();
        // The queue must be deep enough to actually hold the frames buffered at
        // the ceiling delay, or adaptation would drop them before they are due.
        // Respect an explicit user cap; otherwise bump the default to fit.
        let max_queued_frames = if adaptive && !max_is_explicit {
            let needed = frame_interval
                .map(|interval| {
                    (delay_ceiling.as_secs_f64() / interval.as_secs_f64()).ceil() as usize + 1
                })
                .unwrap_or(configured_max);
            configured_max.max(needed)
        } else {
            configured_max
        };
        Self {
            min_delay: floor,
            delay_floor: floor,
            delay_ceiling,
            adaptive,
            jitter_secs: 0.0,
            last_arrival: None,
            frame_interval,
            max_queued_frames,
            queued: VecDeque::new(),
            last_scheduled_at: None,
            last_presented_frame_id: None,
        }
    }

    /// Update the jitter estimate from a frame's arrival time and retarget the
    /// effective delay. No-op when adaptation is off or the frame interval is
    /// unknown (e.g. fps=0 streams), leaving `min_delay` at its fixed value.
    fn observe_arrival(&mut self, now: Instant) {
        if !self.adaptive {
            return;
        }
        let Some(interval) = self.frame_interval else {
            return;
        };
        if let Some(last) = self.last_arrival {
            let gap = now.saturating_duration_since(last).as_secs_f64();
            let deviation = (gap - interval.as_secs_f64())
                .abs()
                .min(Self::MAX_SAMPLE_DEVIATION.as_secs_f64());
            self.jitter_secs += (deviation - self.jitter_secs) * Self::JITTER_GAIN;
            self.retarget_delay();
        }
        self.last_arrival = Some(now);
    }

    fn retarget_delay(&mut self) {
        let floor = self.delay_floor.as_secs_f64();
        let ceil = self.delay_ceiling.as_secs_f64();
        let target = (floor + Self::JITTER_MULTIPLIER * self.jitter_secs).clamp(floor, ceil);
        let current = self.min_delay.as_secs_f64();
        let next = if target >= current {
            target
        } else {
            current + (target - current) * Self::DELAY_SHRINK_GAIN
        };
        self.min_delay = Duration::from_secs_f64(next.clamp(floor, ceil));
    }

    fn current_delay_ms(&self) -> f32 {
        self.min_delay.as_secs_f32() * 1000.0
    }

    /// Time until the earliest queued-but-not-yet-due frame becomes due, or
    /// `None` when the queue is empty. The idle loop caps its socket wait at
    /// this so a frame scheduled <2ms out isn't held back by a flat poll
    /// timeout (saturates to zero for an already-due frame).
    fn next_present_delay(&self, now: Instant) -> Option<Duration> {
        self.queued
            .front()
            .map(|queued| queued.present_at.saturating_duration_since(now))
    }

    fn enqueue(&mut self, frame: VideoFrameBuffer) -> Option<VideoFrameBuffer> {
        let frame_id = frame.debug_timing.as_ref().map(|timing| timing.frame_id);
        if let Some(frame_id) = frame_id {
            if self
                .last_presented_frame_id
                .map(|last| !frame_id_is_newer(frame_id, last))
                .unwrap_or(false)
            {
                return Some(frame);
            }

            if self
                .queued
                .iter()
                .rev()
                .find_map(|queued| queued.frame_id)
                .map(|last| !frame_id_is_newer(frame_id, last))
                .unwrap_or(false)
            {
                return Some(frame);
            }
        }

        let now = Instant::now();
        self.observe_arrival(now);
        let candidate = now + self.min_delay;
        let present_at = self
            .last_scheduled_at
            .zip(self.frame_interval)
            .map(|(last, interval)| candidate.max(last + interval))
            .unwrap_or(candidate);
        self.last_scheduled_at = Some(present_at);
        self.queued.push_back(QueuedVideoFrame {
            present_at,
            frame_id,
            frame,
        });
        if self.queued.len() > self.max_queued_frames.max(1) {
            return self.queued.pop_front().map(|queued| queued.frame);
        }
        None
    }

    fn take_due_frames(&mut self) -> DueVideoFrames {
        let now = Instant::now();
        let Some(front) = self.queued.front() else {
            return DueVideoFrames::default();
        };
        if front.present_at > now {
            return DueVideoFrames::default();
        }

        let mut due = DueVideoFrames::default();
        while self
            .queued
            .front()
            .map(|queued| queued.present_at <= now)
            .unwrap_or(false)
        {
            let queued = self.queued.pop_front().expect("front checked");
            if queued
                .frame_id
                .zip(self.last_presented_frame_id)
                .map(|(frame_id, last)| !frame_id_is_newer(frame_id, last))
                .unwrap_or(false)
            {
                due.dropped.push(queued.frame);
                continue;
            }

            if let Some(frame_id) = queued.frame_id {
                self.last_presented_frame_id = Some(frame_id);
            }
            let frame = queued.frame;
            if let Some(previous) = due.present.replace(frame) {
                due.dropped.push(previous);
            }
        }
        due
    }

    fn update_framerate(&mut self, stream_fps: u16) {
        let updated = Self::new(stream_fps);
        self.min_delay = updated.min_delay;
        self.delay_floor = updated.delay_floor;
        self.delay_ceiling = updated.delay_ceiling;
        self.adaptive = updated.adaptive;
        self.frame_interval = updated.frame_interval;
        self.max_queued_frames = updated.max_queued_frames;
        self.jitter_secs = 0.0;
        self.last_arrival = None;
    }

    fn reset_for_discontinuity(&mut self, stream_fps: u16) -> Vec<VideoFrameBuffer> {
        let dropped = self.queued.drain(..).map(|queued| queued.frame).collect();
        *self = Self::new(stream_fps);
        dropped
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoProfileUpdate {
    Unchanged,
    FrameRateOnly,
    EpochReset,
    ReplaceDecoder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VideoDecoderProfile {
    codec: st_protocol::VideoCodec,
    chroma: st_protocol::VideoChromaSampling,
    hdr: bool,
    width: u32,
    height: u32,
}

impl VideoDecoderProfile {
    fn from_config(config: StreamConfig) -> Self {
        Self {
            codec: config.codec,
            chroma: config.chroma,
            hdr: config.hdr,
            width: config.width,
            height: config.height,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DecoderBuildToken {
    generation: u64,
    video_epoch: u64,
    profile: VideoDecoderProfile,
}

impl DecoderBuildToken {
    fn matches_config(self, config: StreamConfig) -> bool {
        self.video_epoch == config.video_epoch
            && self.profile == VideoDecoderProfile::from_config(config)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VideoProfileTransition {
    update: VideoProfileUpdate,
    build: Option<DecoderBuildToken>,
}

struct VideoProfileState {
    config: StreamConfig,
    generation: u64,
    installed_profile: Option<VideoDecoderProfile>,
    pending_build: Option<DecoderBuildToken>,
    output_deadline: Option<Instant>,
    output_seen: bool,
}

impl VideoProfileState {
    fn new(config: StreamConfig) -> Self {
        Self {
            config,
            generation: 0,
            installed_profile: Some(VideoDecoderProfile::from_config(config)),
            pending_build: None,
            // Initial startup remains covered by the connection-level watchdog,
            // which can distinguish a blocked UDP path from a bad codec.
            output_deadline: None,
            output_seen: false,
        }
    }

    fn classify(&self, next: StreamConfig) -> VideoProfileUpdate {
        if VideoDecoderProfile::from_config(self.config) != VideoDecoderProfile::from_config(next) {
            VideoProfileUpdate::ReplaceDecoder
        } else if self.config.video_epoch != next.video_epoch {
            VideoProfileUpdate::EpochReset
        } else if self.config.framerate != next.framerate {
            VideoProfileUpdate::FrameRateOnly
        } else {
            VideoProfileUpdate::Unchanged
        }
    }

    fn apply(&mut self, next: StreamConfig, now: Instant) -> VideoProfileTransition {
        let update = self.classify(next);
        self.config = next;
        let build = match update {
            VideoProfileUpdate::ReplaceDecoder => Some(self.begin_build(now)),
            VideoProfileUpdate::EpochReset => {
                self.reset_output_progress(now);
                if self.pending_build.is_some()
                    || self.installed_profile != Some(VideoDecoderProfile::from_config(next))
                {
                    Some(self.begin_build(now))
                } else {
                    None
                }
            }
            VideoProfileUpdate::Unchanged | VideoProfileUpdate::FrameRateOnly => None,
        };
        VideoProfileTransition { update, build }
    }

    fn begin_rebuild(&mut self, now: Instant) -> DecoderBuildToken {
        self.begin_build(now)
    }

    fn begin_build(&mut self, now: Instant) -> DecoderBuildToken {
        self.generation = self.generation.wrapping_add(1);
        let token = DecoderBuildToken {
            generation: self.generation,
            video_epoch: self.config.video_epoch,
            profile: VideoDecoderProfile::from_config(self.config),
        };
        self.installed_profile = None;
        self.pending_build = Some(token);
        self.reset_output_progress(now);
        token
    }

    fn accept_build(&mut self, token: DecoderBuildToken, now: Instant) -> bool {
        if self.pending_build != Some(token) || !token.matches_config(self.config) {
            return false;
        }
        self.pending_build = None;
        self.installed_profile = Some(token.profile);
        self.reset_output_progress(now);
        true
    }

    fn reset_output_progress(&mut self, now: Instant) {
        self.output_seen = false;
        self.output_deadline = Some(now + LIVE_DECODE_OUTPUT_TIMEOUT);
    }

    fn note_output(&mut self) -> bool {
        let first_output = !self.output_seen;
        self.output_seen = true;
        self.output_deadline = None;
        first_output
    }

    fn output_deadline_expired(&self, now: Instant) -> bool {
        !self.output_seen && self.output_deadline.is_some_and(|deadline| now >= deadline)
    }

    fn progress(&self) -> VideoDecodeProgress {
        VideoDecodeProgress {
            video_epoch: self.config.video_epoch,
            profile: VideoDecoderProfile::from_config(self.config),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoDecodeProgress {
    video_epoch: u64,
    profile: VideoDecoderProfile,
}

impl VideoDecodeProgress {
    pub fn matches_config(self, config: StreamConfig) -> bool {
        self.video_epoch == config.video_epoch
            && self.profile == VideoDecoderProfile::from_config(config)
    }
}

pub fn decode_progress_resets(current: StreamConfig, next: StreamConfig) -> bool {
    current.video_epoch != next.video_epoch
        || VideoDecoderProfile::from_config(current) != VideoDecoderProfile::from_config(next)
}

fn frame_matches_profile(frame: &st_protocol::CompletedFrame, profile: &VideoProfileState) -> bool {
    frame.video_epoch == profile.config.video_epoch
}

#[derive(Debug)]
pub struct VideoProfileError {
    pub codec: st_protocol::VideoCodec,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecoderBuildMode {
    Automatic,
    Software,
}

#[derive(Clone, Copy, Debug)]
struct DecoderBuildRequest {
    token: DecoderBuildToken,
    mode: DecoderBuildMode,
}

struct DecoderBuildCompleted {
    token: DecoderBuildToken,
    mode: DecoderBuildMode,
    result: Result<BuiltVideoDecoder, String>,
}

struct BuiltVideoDecoder(VideoDecoder);

// SAFETY: a builder result is transferred exactly once before live decode
// begins. The FFmpeg scaler that makes VideoDecoder !Send is still None at
// construction, and no decoder method runs concurrently with the transfer.
unsafe impl Send for BuiltVideoDecoder {}

#[derive(Default)]
struct DecoderBuilderState {
    latest: Option<DecoderBuildToken>,
    pending: Option<DecoderBuildRequest>,
    completed: Option<DecoderBuildCompleted>,
    shutdown: bool,
}

struct DecoderBuilderShared {
    state: Mutex<DecoderBuilderState>,
    wake: Condvar,
}

struct DecoderBuilder {
    shared: Arc<DecoderBuilderShared>,
}

impl DecoderBuilder {
    fn new() -> Self {
        let shared = Arc::new(DecoderBuilderShared {
            state: Mutex::new(DecoderBuilderState::default()),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("decoder-builder".into())
            .spawn(move || run_decoder_builder(worker_shared))
            .expect("failed to spawn decoder builder");
        Self { shared }
    }

    fn request(&self, request: DecoderBuildRequest) {
        let mut state = self.shared.state.lock().unwrap();
        state.latest = Some(request.token);
        state.pending = Some(request);
        state.completed = None;
        self.shared.wake.notify_one();
    }

    fn take_completed(&self) -> Option<DecoderBuildCompleted> {
        self.shared.state.lock().unwrap().completed.take()
    }
}

impl Drop for DecoderBuilder {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        state.shutdown = true;
        state.pending = None;
        state.completed = None;
        self.shared.wake.notify_one();
    }
}

fn run_decoder_builder(shared: Arc<DecoderBuilderShared>) {
    loop {
        let request = {
            let mut state = shared.state.lock().unwrap();
            while state.pending.is_none() && !state.shutdown {
                state = shared.wake.wait(state).unwrap();
            }
            if state.shutdown {
                return;
            }
            state.pending.take().expect("pending request checked")
        };

        let result = match request.mode {
            DecoderBuildMode::Automatic => {
                VideoDecoder::new(request.token.profile.codec, request.token.profile.chroma)
            }
            DecoderBuildMode::Software => VideoDecoder::new_software(request.token.profile.codec),
        }
        .map(BuiltVideoDecoder);

        let mut state = shared.state.lock().unwrap();
        if state.shutdown {
            return;
        }
        if state.latest == Some(request.token) {
            state.completed = Some(DecoderBuildCompleted {
                token: request.token,
                mode: request.mode,
                result,
            });
        }
    }
}

fn frame_id_is_newer(candidate: u32, previous: u32) -> bool {
    let delta = candidate.wrapping_sub(previous);
    delta > 0 && delta < 0x8000_0000
}

/// Returns the playout delay floor and whether adaptation is enabled.
///
/// `ST_CLIENT_VIDEO_JITTER_MS` forces a fixed delay (adaptation off) as the
/// escape hatch. Otherwise the returned value is the *floor* — the proven
/// low-latency baseline (~one frame) below which the adaptive buffer never
/// drops — and adaptation is on.
fn configured_video_jitter_delay(stream_fps: u16) -> (Duration, bool) {
    if let Ok(raw) = std::env::var("ST_CLIENT_VIDEO_JITTER_MS") {
        if let Ok(parsed) = raw.parse::<u64>() {
            return (Duration::from_millis(parsed.min(250)), false);
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Keep the Windows client biased toward low-delay
        // presentation instead of buffering a full extra frame by default.
        if stream_fps == 0 {
            return (Duration::from_millis(6), true);
        }

        return (
            Duration::from_secs_f64((0.5 / f64::from(stream_fps)).clamp(0.003, 0.008)),
            true,
        );
    }

    #[cfg(not(target_os = "windows"))]
    {
        if stream_fps == 0 {
            return (Duration::from_millis(10), true);
        }

        // Latency-first: hold roughly a half-frame baseline rather than a full
        // frame, letting the adaptive buffer grow from here only when real jitter
        // appears. Lever-1 (server adaptive fps) keeps the cadence regular so the
        // buffer stays near this floor; the Stutter graph surfaces any playout
        // drops if the floor is too tight on a given path.
        (
            Duration::from_secs_f64((0.6 / f64::from(stream_fps)).clamp(0.006, 0.020)),
            true,
        )
    }
}

/// Upper bound for the adaptive playout delay: ~3 frame intervals of headroom,
/// hard-capped at 80ms so a pathological path cannot inflate latency without
/// limit. Falls back to floor+45ms when the frame interval is unknown.
fn adaptive_delay_ceiling(floor: Duration, frame_interval: Option<Duration>) -> Duration {
    const ABS_MAX: Duration = Duration::from_millis(80);
    let by_interval = frame_interval
        .map(|interval| interval * 3)
        .unwrap_or(floor + Duration::from_millis(45));
    by_interval.clamp(floor, ABS_MAX)
}

/// Returns the configured max queued frames and whether the user set it
/// explicitly (so adaptation can grow the default but never override an
/// explicit user cap).
fn configured_video_jitter_max_frames() -> (usize, bool) {
    match std::env::var("ST_CLIENT_VIDEO_JITTER_MAX_FRAMES")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
    {
        Some(value) => (value.clamp(1, 8), true),
        None => (3, false),
    }
}

fn recycle_video_frame(recycled_frames: &mut Vec<VideoFrameBuffer>, mut frame: VideoFrameBuffer) {
    if recycled_frames.len() >= 3 {
        return;
    }
    frame.dirty = false;
    #[cfg(target_os = "macos")]
    frame.clear_native_surfaces();
    recycled_frames.push(frame);
}

/// Presents the newest due frame and recycles any it superseded. Returns the
/// number of decoded frames the playout buffer dropped (skipped before display)
/// so the caller can surface playout/jitter churn in the debug HUD.
fn present_due_video_frames(
    playout: &mut VideoPlayoutBuffer,
    recycled_frames: &mut Vec<VideoFrameBuffer>,
    frame_buf: &Arc<Mutex<VideoFrameBuffer>>,
    ctx: &egui::Context,
    repaint_pacer: &mut RepaintPacer,
    // When this present follows a fresh decode this iteration, repaint
    // immediately: the frame is brand-new content and the playout buffer +
    // egui mailbox (`desired_maximum_frame_latency=1`) already bound the real
    // present rate, so routing it through the refresh-rate pacer only defers a
    // ready frame by up to one display interval (~16.6ms @60Hz). The pacer is
    // still used for the idle/no-new-data re-present path.
    immediate: bool,
) -> usize {
    let due = playout.take_due_frames();
    let dropped_count = due.dropped.len();
    for dropped in due.dropped {
        recycle_video_frame(recycled_frames, dropped);
    }

    let Some(mut frame) = due.present else {
        return dropped_count;
    };

    let repaint_pending = {
        let mut fb = frame_buf.lock().unwrap();
        let repaint_pending = fb.dirty;
        std::mem::swap(&mut *fb, &mut frame);
        fb.dirty = true;
        repaint_pending
    };
    recycle_video_frame(recycled_frames, frame);
    repaint_pacer.request_video(ctx, immediate, repaint_pending);
    dropped_count
}

#[allow(clippy::too_many_arguments)]
pub fn run_receive_pipeline(
    frame_buf: Arc<Mutex<VideoFrameBuffer>>,
    debug_state: Arc<ConnectionDebugState>,
    debug_enabled: Arc<AtomicBool>,
    ctx: egui::Context,
    shutdown_rx: Receiver<()>,
    audio_tx: Sender<AudioPacket>,
    audio_drop_rx: Receiver<AudioPacket>,
    feedback_tx: Sender<TransportFeedback>,
    decode_started_tx: Sender<VideoDecodeProgress>,
    // B1: bumped on every received video unit so the connection loop can detect
    // a mid-session media stall (TCP alive but UDP video dead — wifi switch /
    // NAT rebind) and trigger reconnect instead of freezing on the last frame.
    video_arrival: Arc<AtomicU64>,
    audio_enabled: Arc<AtomicBool>,
    native_surfaces: Arc<NativeSurfaceControl>,
    control_tx: Sender<ControlMessage>,
    stream_config_rx: Receiver<StreamConfig>,
    profile_error_tx: Sender<VideoProfileError>,
    display_refresh_millihz: Option<u32>,
    stream_config: StreamConfig,
    mut receiver: MediaReceiver,
) {
    let trace = std::env::var_os("ST_TRACE").is_some();
    let mut trace_completed_logged = 0usize;
    let mut last_recovery_keyframe_request = Instant::now() - Duration::from_secs(2);
    let mut attempted_software_fallback = false;

    let mut profile = VideoProfileState::new(stream_config);
    let decoder = match VideoDecoder::new(profile.config.codec, profile.config.chroma) {
        Ok(d) => {
            eprintln!("[pipeline] decoder ready: {}", d.name());
            debug_state.set_decoder_name(d.name());
            d
        }
        Err(e) => {
            eprintln!("[pipeline] failed to create decoder: {e}");
            let _ = profile_error_tx.try_send(VideoProfileError {
                codec: profile.config.codec,
                message: e,
            });
            return;
        }
    };
    let mut decoder = Some(decoder);
    let decoder_builder = DecoderBuilder::new();
    decoder
        .as_mut()
        .expect("initial decoder present")
        .set_native_surface_control(Arc::clone(&native_surfaces));
    let mut decoded_frame = VideoFrameBuffer::default();
    let mut playout = VideoPlayoutBuffer::new(profile.config.framerate);
    let mut recycled_frames = Vec::new();
    let mut repaint_pacer = RepaintPacer::new(crate::display::desired_present_refresh_millihz(
        display_refresh_millihz,
        profile.config.framerate,
    ));

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        let mut next_config = None;
        while let Ok(config) = stream_config_rx.try_recv() {
            next_config = Some(config);
        }
        if let Some(next_config) = next_config {
            let transition = profile.apply(next_config, Instant::now());
            let discontinuity = matches!(
                transition.update,
                VideoProfileUpdate::EpochReset | VideoProfileUpdate::ReplaceDecoder
            );
            if discontinuity {
                receiver.reset_video();
                for frame in playout.reset_for_discontinuity(next_config.framerate) {
                    recycle_video_frame(&mut recycled_frames, frame);
                }
                decoded_frame = VideoFrameBuffer::default();
            }
            if let Some(token) = transition.build {
                decoder = None;
                attempted_software_fallback = false;
                decoder_builder.request(DecoderBuildRequest {
                    token,
                    mode: DecoderBuildMode::Automatic,
                });
            } else if transition.update == VideoProfileUpdate::EpochReset {
                if let Some(decoder) = decoder.as_mut() {
                    decoder.enter_recovery_mode("stream epoch changed");
                }
                request_recovery_keyframe(
                    &control_tx,
                    &mut last_recovery_keyframe_request,
                    trace,
                    "stream epoch changed",
                );
            }
            if transition.update == VideoProfileUpdate::FrameRateOnly {
                playout.update_framerate(next_config.framerate);
            }
            repaint_pacer.set_refresh_millihz(crate::display::desired_present_refresh_millihz(
                display_refresh_millihz,
                profile.config.framerate,
            ));
        }

        if let Some(completed) = decoder_builder.take_completed() {
            if profile.pending_build != Some(completed.token)
                || !completed.token.matches_config(profile.config)
            {
                continue;
            }
            let mut next_decoder = match completed.result {
                Ok(BuiltVideoDecoder(decoder)) => decoder,
                Err(error) => {
                    eprintln!("[pipeline] live decoder replacement failed: {error}");
                    let _ = profile_error_tx.try_send(VideoProfileError {
                        codec: profile.config.codec,
                        message: error,
                    });
                    return;
                }
            };
            if !profile.accept_build(completed.token, Instant::now()) {
                continue;
            }
            next_decoder.set_native_surface_control(Arc::clone(&native_surfaces));
            next_decoder.enter_recovery_mode(match completed.mode {
                DecoderBuildMode::Automatic => "stream profile changed",
                DecoderBuildMode::Software => "hardware decoder failure",
            });
            debug_state.set_decoder_name(next_decoder.name());
            attempted_software_fallback = completed.mode == DecoderBuildMode::Software;
            eprintln!(
                "[pipeline] decoder replacement ready: {}",
                next_decoder.name()
            );
            decoder = Some(next_decoder);
            receiver.reset_video();
            request_recovery_keyframe(
                &control_tx,
                &mut last_recovery_keyframe_request,
                trace,
                "decoder replacement installed",
            );
        }

        if profile.output_deadline_expired(Instant::now()) {
            let message = format!(
                "decoder produced no frame for epoch {} within {}s",
                profile.config.video_epoch,
                LIVE_DECODE_OUTPUT_TIMEOUT.as_secs()
            );
            eprintln!("[pipeline] {message}");
            let _ = profile_error_tx.try_send(VideoProfileError {
                codec: profile.config.codec,
                message,
            });
            return;
        }

        let data = receiver.try_receive();
        if let Some(stats) = receiver.take_stats() {
            if debug_enabled.load(Ordering::Relaxed) {
                debug_state.update_transport_window(&stats);
            }
            maybe_request_transport_recovery_keyframe(
                stats,
                &control_tx,
                &mut last_recovery_keyframe_request,
                trace,
            );
            let _ = feedback_tx.try_send(stats.feedback());
        }

        // Recovery is driven by the decoder's frame-id continuity check (see
        // VideoDecoder::decode_into), not by a transport-layer flag that could
        // be consumed before the drain loop below ingests the unit that exposes
        // the gap. The transport window still nudges the server early via
        // `maybe_request_transport_recovery_keyframe` above, and every skipped
        // unit re-requests a keyframe through the `waiting_for_recovery` path.

        match data {
            None => {
                let drops = present_due_video_frames(
                    &mut playout,
                    &mut recycled_frames,
                    &frame_buf,
                    &ctx,
                    &mut repaint_pacer,
                    false,
                );
                if drops > 0 && debug_enabled.load(Ordering::Relaxed) {
                    debug_state.record_playout_drop(drops as u32);
                }
                // Block until data, the next playout deadline, or the bounded
                // maintenance cadence for shutdown and transport feedback.
                let wait = playout
                    .next_present_delay(Instant::now())
                    .map(|until| until.min(MEDIA_MAINTENANCE_INTERVAL))
                    .unwrap_or(MEDIA_MAINTENANCE_INTERVAL);
                receiver.wait_for_data(wait);
                continue;
            }
            Some(ReceivedData::Audio(opus)) => {
                // B1: audio is liveness too (flows continuously while enabled).
                video_arrival.fetch_add(1, Ordering::Relaxed);
                if audio_enabled.load(Ordering::Relaxed) {
                    queue_latest_audio(&audio_tx, &audio_drop_rx, opus);
                }
            }
            Some(ReceivedData::Keepalive) => {
                // B1: server liveness keepalive — no media, just reset the
                // connection loop's media-stall watchdog (path is alive but idle).
                video_arrival.fetch_add(1, Ordering::Relaxed);
            }
            Some(ReceivedData::Video(completed, assembled_micros, assembled_mono)) => {
                // B1: signal liveness to the connection loop's media watchdog.
                video_arrival.fetch_add(1, Ordering::Relaxed);
                if !frame_matches_profile(&completed, &profile) {
                    continue;
                }
                if decoder.is_none() {
                    continue;
                }
                if trace && trace_completed_logged < 12 {
                    eprintln!(
                        "[trace][client] assembled video unit #{}: frame_id={} bytes={} capture_ts={} send_ts={}",
                        trace_completed_logged,
                        completed.frame_id,
                        completed.data.len(),
                        completed.timing.capture_ts_micros,
                        completed.timing.send_ts_micros
                    );
                }
                // Keep decoder input in-order. We can present only the newest decoded
                // frame, but we must not drop inter-frame video packets before decode.
                // Carry each unit's assembly timestamp so latency stages stay accurate.
                let mut pending_video = vec![(completed, assembled_micros, assembled_mono)];
                let drain_deadline = Instant::now() + Duration::from_millis(2);
                let mut drained = 0usize;
                loop {
                    if drained >= 64 || Instant::now() >= drain_deadline {
                        break;
                    }
                    match receiver.try_receive() {
                        Some(ReceivedData::Video(newer, newer_wall, newer_mono)) => {
                            video_arrival.fetch_add(1, Ordering::Relaxed);
                            if frame_matches_profile(&newer, &profile) {
                                pending_video.push((newer, newer_wall, newer_mono));
                            }
                        }
                        Some(ReceivedData::Audio(opus)) => {
                            video_arrival.fetch_add(1, Ordering::Relaxed);
                            if audio_enabled.load(Ordering::Relaxed) {
                                queue_latest_audio(&audio_tx, &audio_drop_rx, opus);
                            }
                        }
                        Some(ReceivedData::Keepalive) => {
                            video_arrival.fetch_add(1, Ordering::Relaxed);
                        }
                        None => break,
                    }
                    drained += 1;
                }

                let mut latest_timing = None;
                let mut produced_frame = false;
                let mut fatal_decode_error = None;
                for (completed, assembled_micros, assembled_mono) in pending_video {
                    let decode_start_mono = mono_micros();
                    let active_decoder = decoder.as_mut().expect("decoder checked before drain");
                    let decoded = match active_decoder.decode_into(
                        &completed.data,
                        completed.frame_id,
                        completed.frame_type,
                        &mut decoded_frame,
                    ) {
                        Ok(frame) => frame,
                        Err(e) => {
                            eprintln!("decode error: {e}");
                            fatal_decode_error =
                                Some((active_decoder.is_hardware_accelerated(), e));
                            break;
                        }
                    };
                    if active_decoder.waiting_for_recovery() {
                        request_recovery_keyframe(
                            &control_tx,
                            &mut last_recovery_keyframe_request,
                            trace,
                            "decoder recovery wait",
                        );
                    }
                    if decoded.dropped_stale_output {
                        request_recovery_keyframe(
                            &control_tx,
                            &mut last_recovery_keyframe_request,
                            trace,
                            "stale decoder output",
                        );
                    }
                    let decode_done_mono = mono_micros();

                    if trace && trace_completed_logged < 12 {
                        eprintln!(
                            "[trace][client] decode input frame_id={} produced_frame={} output_frame_id={:?}",
                            completed.frame_id,
                            decoded.produced_frame,
                            decoded.frame_id
                        );
                        trace_completed_logged += 1;
                    }
                    if decoded.produced_frame {
                        let frame_id = decoded.frame_id.unwrap_or(completed.frame_id);
                        produced_frame = true;
                        latest_timing = Some(FrameDebugTiming {
                            frame_id,
                            server_capture_micros: completed.timing.capture_ts_micros,
                            server_send_micros: completed.timing.send_ts_micros,
                            client_assembled_micros: assembled_micros,
                            client_assembled_mono: assembled_mono,
                            client_decode_start_mono: decode_start_mono,
                            client_decode_done_mono: decode_done_mono,
                        });
                    }
                }

                if let Some((hardware_accelerated, error)) = fatal_decode_error {
                    if hardware_accelerated && !attempted_software_fallback {
                        attempted_software_fallback = true;
                        decoder = None;
                        receiver.reset_video();
                        for frame in playout.reset_for_discontinuity(profile.config.framerate) {
                            recycle_video_frame(&mut recycled_frames, frame);
                        }
                        decoded_frame = VideoFrameBuffer::default();
                        let token = profile.begin_rebuild(Instant::now());
                        decoder_builder.request(DecoderBuildRequest {
                            token,
                            mode: DecoderBuildMode::Software,
                        });
                        request_recovery_keyframe(
                            &control_tx,
                            &mut last_recovery_keyframe_request,
                            trace,
                            "hardware decoder fallback",
                        );
                        continue;
                    } else {
                        let message = format!("fatal runtime decode failure: {error}");
                        let _ = profile_error_tx.try_send(VideoProfileError {
                            codec: profile.config.codec,
                            message,
                        });
                        return;
                    }
                }

                if produced_frame {
                    if profile.note_output() {
                        let _ = decode_started_tx.send(profile.progress());
                    }
                    decoded_frame.debug_timing = latest_timing;
                    if debug_enabled.load(Ordering::Relaxed) {
                        if let Some(timing) = decoded_frame.debug_timing.as_ref() {
                            debug_state.record_decoded(timing);
                        }
                    }
                    let mut queued_frame = recycled_frames.pop().unwrap_or_default();
                    std::mem::swap(&mut queued_frame, &mut decoded_frame);
                    let debug_on = debug_enabled.load(Ordering::Relaxed);
                    if let Some(dropped) = playout.enqueue(queued_frame) {
                        if debug_on {
                            debug_state.record_playout_drop(1);
                        }
                        recycle_video_frame(&mut recycled_frames, dropped);
                    }
                    if debug_on {
                        debug_state.set_jitter_delay(playout.current_delay_ms());
                    }
                    let drops = present_due_video_frames(
                        &mut playout,
                        &mut recycled_frames,
                        &frame_buf,
                        &ctx,
                        &mut repaint_pacer,
                        true,
                    );
                    if drops > 0 && debug_on {
                        debug_state.record_playout_drop(drops as u32);
                    }
                }
            }
        }
    }
}

fn queue_latest_audio(
    audio_tx: &Sender<AudioPacket>,
    audio_drop_rx: &Receiver<AudioPacket>,
    mut packet: AudioPacket,
) {
    loop {
        match audio_tx.try_send(packet) {
            Ok(()) | Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
            Err(crossbeam_channel::TrySendError::Full(returned)) => {
                packet = returned;
                let _ = audio_drop_rx.try_recv();
            }
        }
    }
}

fn maybe_request_transport_recovery_keyframe(
    stats: TransportWindowStats,
    control_tx: &Sender<ControlMessage>,
    last_recovery_keyframe_request: &mut Instant,
    trace: bool,
) {
    if !stats.needs_recovery_keyframe() {
        return;
    }

    request_recovery_keyframe(
        control_tx,
        last_recovery_keyframe_request,
        trace,
        &format!(
            "transport loss: lost_packets={} dropped_frames={}",
            stats.lost_packets, stats.dropped_frames
        ),
    );
}

fn request_recovery_keyframe(
    control_tx: &Sender<ControlMessage>,
    last_recovery_keyframe_request: &mut Instant,
    trace: bool,
    reason: &str,
) {
    if last_recovery_keyframe_request.elapsed() < Duration::from_millis(250) {
        return;
    }

    let _ = control_tx.try_send(ControlMessage::RequestKeyframe);
    *last_recovery_keyframe_request = Instant::now();
    if trace {
        eprintln!("[trace][client] requested recovery keyframe after {reason}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        adaptive_delay_ceiling, frame_id_is_newer, frame_matches_profile, queue_latest_audio,
        VideoPlayoutBuffer, VideoProfileState, VideoProfileUpdate, LIVE_DECODE_OUTPUT_TIMEOUT,
    };
    use crate::transport::AudioPacket;
    use crate::video_frame::{FrameDebugTiming, VideoFrameBuffer};
    use st_protocol::{StreamConfig, VideoChromaSampling, VideoCodec};
    use std::time::{Duration, Instant};

    fn stream_config(codec: VideoCodec, framerate: u16) -> StreamConfig {
        StreamConfig {
            video_epoch: 1,
            codec,
            width: 1920,
            height: 1080,
            framerate,
            audio_sample_rate: 48_000,
            audio_channels: 2,
            hdr: false,
            chroma: VideoChromaSampling::Yuv420,
            packet_duration_ms: 5,
        }
    }

    #[test]
    fn live_profile_change_replaces_decoder() {
        let mut initial = stream_config(VideoCodec::Hevc, 120);
        initial.chroma = VideoChromaSampling::Yuv444;
        initial.hdr = true;
        let state = VideoProfileState::new(initial);

        assert_eq!(
            state.classify(stream_config(VideoCodec::H264, 60)),
            VideoProfileUpdate::ReplaceDecoder
        );
    }

    #[test]
    fn fps_only_profile_change_keeps_decoder() {
        let mut state = VideoProfileState::new(stream_config(VideoCodec::Hevc, 120));
        state.note_output();
        let generation = state.generation;
        let transition = state.apply(stream_config(VideoCodec::Hevc, 60), Instant::now());

        assert_eq!(transition.update, VideoProfileUpdate::FrameRateOnly);
        assert!(transition.build.is_none());
        assert_eq!(state.generation, generation);
        assert!(state.output_seen);
        assert!(state.output_deadline.is_none());
        assert_eq!(state.config.framerate, 60);

        let mut playout = VideoPlayoutBuffer::new(120);
        assert!(playout.enqueue(frame_with_id(1)).is_none());
        let scheduled = playout.last_scheduled_at;
        playout.update_framerate(60);
        assert_eq!(playout.queued.len(), 1);
        assert_eq!(playout.last_scheduled_at, scheduled);
        assert_eq!(
            playout.frame_interval,
            Some(Duration::from_secs_f64(1.0 / 60.0))
        );
    }

    #[test]
    fn stale_decoder_build_result_is_rejected() {
        let now = Instant::now();
        let mut state = VideoProfileState::new(stream_config(VideoCodec::H264, 60));
        let mut hevc = stream_config(VideoCodec::Hevc, 60);
        hevc.video_epoch = 2;
        let stale = state.apply(hevc, now).build.expect("HEVC build");
        let mut av1 = stream_config(VideoCodec::Av1, 60);
        av1.video_epoch = 3;
        let latest = state
            .apply(av1, now + Duration::from_millis(1))
            .build
            .expect("AV1 build");

        assert!(!state.accept_build(stale, now + Duration::from_millis(2)));
        assert!(state.accept_build(latest, now + Duration::from_millis(3)));
        assert_eq!(state.config, av1);
    }

    #[test]
    fn replacement_runtime_no_output_deadline_is_per_epoch() {
        let now = Instant::now();
        let mut state = VideoProfileState::new(stream_config(VideoCodec::H264, 60));
        let mut replacement = stream_config(VideoCodec::Hevc, 60);
        replacement.video_epoch = 2;
        let token = state
            .apply(replacement, now)
            .build
            .expect("replacement build");
        let opened_at = now + Duration::from_millis(25);
        assert!(state.accept_build(token, opened_at));

        assert!(!state.output_deadline_expired(
            opened_at + LIVE_DECODE_OUTPUT_TIMEOUT - Duration::from_nanos(1)
        ));
        assert!(state.output_deadline_expired(opened_at + LIVE_DECODE_OUTPUT_TIMEOUT));
        state.note_output();
        assert!(!state.output_deadline_expired(opened_at + LIVE_DECODE_OUTPUT_TIMEOUT));
    }

    #[test]
    fn epoch_discontinuity_resets_decode_and_all_playout_time_state() {
        let now = Instant::now();
        let mut state = VideoProfileState::new(stream_config(VideoCodec::H264, 60));
        state.note_output();
        let mut next = state.config;
        next.video_epoch += 1;
        let transition = state.apply(next, now);

        assert_eq!(transition.update, VideoProfileUpdate::EpochReset);
        assert!(transition.build.is_none());
        assert!(!state.output_seen);
        assert!(state.output_deadline.is_some());

        let mut playout = VideoPlayoutBuffer::new(60);
        assert!(playout.enqueue(frame_with_id(9)).is_none());
        playout.jitter_secs = 0.012;
        playout.last_arrival = Some(now);
        playout.last_presented_frame_id = Some(8);
        let dropped = playout.reset_for_discontinuity(60);

        assert_eq!(dropped.len(), 1);
        assert!(playout.queued.is_empty());
        assert_eq!(playout.jitter_secs, 0.0);
        assert!(playout.last_arrival.is_none());
        assert!(playout.last_scheduled_at.is_none());
        assert!(playout.last_presented_frame_id.is_none());
    }

    #[test]
    fn frame_epoch_must_match_accepted_profile() {
        let profile = VideoProfileState::new(stream_config(VideoCodec::H264, 60));
        let frame = st_protocol::CompletedFrame {
            frame_id: 1,
            video_epoch: profile.config.video_epoch + 1,
            data: Vec::new(),
            timing: Default::default(),
            frame_type: st_protocol::packet::frame_type::IDR,
        };

        assert!(!frame_matches_profile(&frame, &profile));
        assert!(frame_matches_profile(
            &st_protocol::CompletedFrame {
                video_epoch: profile.config.video_epoch,
                ..frame
            },
            &profile
        ));
    }

    fn frame_with_id(frame_id: u32) -> VideoFrameBuffer {
        VideoFrameBuffer {
            debug_timing: Some(FrameDebugTiming {
                frame_id,
                ..FrameDebugTiming::default()
            }),
            ..VideoFrameBuffer::default()
        }
    }

    #[test]
    fn bounded_audio_handoff_evicts_oldest_packet() {
        let (tx, rx) = crossbeam_channel::bounded(3);
        let drop_rx = rx.clone();
        for seq in 1..=5 {
            queue_latest_audio(
                &tx,
                &drop_rx,
                AudioPacket {
                    seq,
                    data: vec![seq as u8],
                    redundant_prev: Vec::new(),
                },
            );
        }

        assert_eq!(
            rx.try_iter().map(|packet| packet.seq).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
    }

    #[test]
    fn frame_ids_use_wrap_aware_ordering() {
        assert!(frame_id_is_newer(11, 10));
        assert!(!frame_id_is_newer(10, 10));
        assert!(!frame_id_is_newer(9, 10));
        assert!(frame_id_is_newer(0, u32::MAX));
        assert!(frame_id_is_newer(1, u32::MAX));
        assert!(!frame_id_is_newer(u32::MAX, 0));
    }

    #[test]
    fn enqueue_drops_frame_older_than_last_queued() {
        let mut playout = VideoPlayoutBuffer::new(60);
        playout.min_delay = Duration::ZERO;
        playout.frame_interval = None;
        playout.max_queued_frames = 8;

        assert!(playout.enqueue(frame_with_id(11)).is_none());
        let dropped = playout
            .enqueue(frame_with_id(10))
            .expect("stale frame dropped");
        assert_eq!(
            dropped.debug_timing.as_ref().expect("frame id").frame_id,
            10
        );
        assert_eq!(playout.queued.len(), 1);
    }

    #[test]
    fn enqueue_drops_frame_older_than_last_presented() {
        let mut playout = VideoPlayoutBuffer::new(60);
        playout.min_delay = Duration::ZERO;
        playout.frame_interval = None;
        playout.max_queued_frames = 8;

        assert!(playout.enqueue(frame_with_id(10)).is_none());
        let due = playout.take_due_frames();
        let presented = due.present.expect("presented frame");
        assert_eq!(
            presented.debug_timing.as_ref().expect("frame id").frame_id,
            10
        );

        let dropped = playout
            .enqueue(frame_with_id(9))
            .expect("stale frame dropped");
        assert_eq!(dropped.debug_timing.as_ref().expect("frame id").frame_id, 9);
        assert!(playout.queued.is_empty());
    }

    // --- Adaptive jitter buffer -------------------------------------------

    /// Build a buffer with deterministic adaptive parameters, independent of
    /// the host environment / platform defaults.
    fn adaptive_playout(floor_ms: u64, ceiling_ms: u64, interval_ms: u64) -> VideoPlayoutBuffer {
        let mut playout = VideoPlayoutBuffer::new(60);
        playout.adaptive = true;
        playout.delay_floor = Duration::from_millis(floor_ms);
        playout.delay_ceiling = Duration::from_millis(ceiling_ms);
        playout.min_delay = playout.delay_floor;
        playout.frame_interval = Some(Duration::from_millis(interval_ms));
        playout.jitter_secs = 0.0;
        playout.last_arrival = None;
        playout
    }

    fn assert_close_ms(actual: Duration, expected_ms: f64, tol_ms: f64) {
        let actual_ms = actual.as_secs_f64() * 1000.0;
        assert!(
            (actual_ms - expected_ms).abs() <= tol_ms,
            "expected ~{expected_ms}ms (±{tol_ms}), got {actual_ms}ms"
        );
    }

    #[test]
    fn adaptive_delay_stays_at_floor_when_steady() {
        let mut p = adaptive_playout(16, 50, 16);
        let mut t = Instant::now();
        for _ in 0..60 {
            t += Duration::from_millis(16);
            p.observe_arrival(t);
        }
        assert_close_ms(p.min_delay, 16.0, 0.5);
    }

    #[test]
    fn adaptive_delay_grows_under_jitter_then_decays() {
        let mut p = adaptive_playout(16, 50, 16);
        let mut t = Instant::now();
        // Alternating 6ms / 26ms gaps → ~10ms mean abs deviation.
        for _ in 0..60 {
            t += Duration::from_millis(6);
            p.observe_arrival(t);
            t += Duration::from_millis(26);
            p.observe_arrival(t);
        }
        let jittered = p.min_delay;
        assert!(
            jittered > Duration::from_millis(16),
            "delay should grow above floor under jitter, got {jittered:?}"
        );
        assert!(jittered <= p.delay_ceiling);

        // Network calms: delay decays back toward the floor.
        for _ in 0..400 {
            t += Duration::from_millis(16);
            p.observe_arrival(t);
        }
        assert!(
            p.min_delay < jittered,
            "delay should shrink once steady ({:?} !< {jittered:?})",
            p.min_delay
        );
        assert_close_ms(p.min_delay, 16.0, 2.0);
    }

    #[test]
    fn adaptive_delay_capped_at_ceiling() {
        let mut p = adaptive_playout(16, 50, 16);
        let mut t = Instant::now();
        // Severe jitter (huge gaps) must never push past the ceiling.
        for _ in 0..200 {
            t += Duration::from_millis(1);
            p.observe_arrival(t);
            t += Duration::from_millis(220);
            p.observe_arrival(t);
        }
        assert!(p.min_delay <= p.delay_ceiling);
        assert_close_ms(p.min_delay, 50.0, 0.5);
    }

    #[test]
    fn forced_delay_disables_adaptation() {
        let mut p = adaptive_playout(16, 50, 16);
        p.adaptive = false;
        p.min_delay = Duration::ZERO; // simulate ST_CLIENT_VIDEO_JITTER_MS=0
        let mut t = Instant::now();
        for _ in 0..60 {
            t += Duration::from_millis(6);
            p.observe_arrival(t);
            t += Duration::from_millis(40);
            p.observe_arrival(t);
        }
        assert_eq!(p.min_delay, Duration::ZERO);
    }

    #[test]
    fn ceiling_is_three_intervals_capped_at_80ms() {
        assert_eq!(
            adaptive_delay_ceiling(Duration::from_millis(16), Some(Duration::from_millis(16))),
            Duration::from_millis(48)
        );
        // 3 * 30ms = 90ms → hard-capped at 80ms.
        assert_eq!(
            adaptive_delay_ceiling(Duration::from_millis(30), Some(Duration::from_millis(30))),
            Duration::from_millis(80)
        );
        // Unknown interval → floor + 45ms.
        assert_eq!(
            adaptive_delay_ceiling(Duration::from_millis(18), None),
            Duration::from_millis(63)
        );
    }
}
