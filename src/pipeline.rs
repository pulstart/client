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
    Arc, Mutex,
};
use std::time::{Duration, Instant};

struct RepaintPacer {
    min_interval: Option<Duration>,
    last_request: Option<Instant>,
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

    let mut fb = frame_buf.lock().unwrap();
    std::mem::swap(&mut *fb, &mut frame);
    fb.dirty = true;
    recycle_video_frame(recycled_frames, frame);
    if immediate {
        ctx.request_repaint();
    } else {
        repaint_pacer.request(ctx);
    }
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
    feedback_tx: Sender<TransportFeedback>,
    decode_started_tx: Sender<()>,
    // B1: bumped on every received video unit so the connection loop can detect
    // a mid-session media stall (TCP alive but UDP video dead — wifi switch /
    // NAT rebind) and trigger reconnect instead of freezing on the last frame.
    video_arrival: Arc<AtomicU64>,
    audio_enabled: Arc<AtomicBool>,
    native_surfaces: Arc<NativeSurfaceControl>,
    control_tx: Sender<ControlMessage>,
    present_refresh_millihz: Option<u32>,
    stream_config: StreamConfig,
    mut receiver: MediaReceiver,
) {
    let trace = std::env::var_os("ST_TRACE").is_some();
    let mut trace_completed_logged = 0usize;
    let mut last_recovery_keyframe_request = Instant::now() - Duration::from_secs(2);
    let mut attempted_software_fallback = false;

    let mut decoder = match VideoDecoder::new(stream_config.codec, stream_config.chroma) {
        Ok(d) => {
            eprintln!("[pipeline] decoder ready: {}", d.name());
            debug_state.set_decoder_name(d.name());
            d
        }
        Err(e) => {
            eprintln!("[pipeline] failed to create decoder: {e}");
            return;
        }
    };
    decoder.set_native_surface_control(Arc::clone(&native_surfaces));
    let mut decoded_frame = VideoFrameBuffer::default();
    let mut playout = VideoPlayoutBuffer::new(stream_config.framerate);
    let mut recycled_frames = Vec::new();
    let mut repaint_pacer = RepaintPacer::new(present_refresh_millihz);

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
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
                // Block briefly on the socket instead of spinning: wakes as soon
                // as a datagram arrives (Linux poll) or after a timeout so
                // stats/recovery/shutdown checks still get a turn. Cap the wait at
                // the next queued frame's due time (never more than 2ms) so a
                // frame scheduled <2ms out is presented on time instead of being
                // held back by a flat poll timeout.
                let wait = playout
                    .next_present_delay(Instant::now())
                    .map(|until| until.min(Duration::from_millis(2)))
                    .unwrap_or(Duration::from_millis(2));
                receiver.wait_for_data(wait);
                continue;
            }
            Some(ReceivedData::Audio(opus)) => {
                if audio_enabled.load(Ordering::Relaxed) {
                    let _ = audio_tx.try_send(opus);
                }
            }
            Some(ReceivedData::Video(completed, assembled_micros, assembled_mono)) => {
                // B1: signal liveness to the connection loop's media watchdog.
                video_arrival.fetch_add(1, Ordering::Relaxed);
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
                            pending_video.push((newer, newer_wall, newer_mono))
                        }
                        Some(ReceivedData::Audio(opus)) => {
                            if audio_enabled.load(Ordering::Relaxed) {
                                let _ = audio_tx.try_send(opus);
                            }
                        }
                        None => break,
                    }
                    drained += 1;
                }

                let mut latest_timing = None;
                let mut produced_frame = false;
                for (completed, assembled_micros, assembled_mono) in pending_video {
                    let decode_start_mono = mono_micros();
                    let decoded = match decoder.decode_into(
                        &completed.data,
                        completed.frame_id,
                        completed.frame_type,
                        &mut decoded_frame,
                    ) {
                        Ok(frame) => frame,
                        Err(e) => {
                            eprintln!("decode error: {e}");
                            if !attempted_software_fallback && decoder.is_hardware_accelerated() {
                                match VideoDecoder::new_software(stream_config.codec) {
                                    Ok(mut software_decoder) => {
                                        attempted_software_fallback = true;
                                        software_decoder.set_native_surface_control(Arc::clone(
                                            &native_surfaces,
                                        ));
                                        software_decoder
                                            .enter_recovery_mode("hardware decoder failure");
                                        decoder = software_decoder;
                                        debug_state.set_decoder_name(decoder.name());
                                        eprintln!(
                                            "[pipeline] switched to software decoder after hardware decode failure: {}",
                                            decoder.name()
                                        );
                                        request_recovery_keyframe(
                                            &control_tx,
                                            &mut last_recovery_keyframe_request,
                                            trace,
                                            "hardware decoder fallback",
                                        );
                                    }
                                    Err(fallback_err) => {
                                        eprintln!(
                                            "[pipeline] software decoder fallback failed: {fallback_err}"
                                        );
                                    }
                                }
                            }
                            continue;
                        }
                    };
                    if decoder.waiting_for_recovery() {
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

                if produced_frame {
                    let _ = decode_started_tx.try_send(());
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
    use super::{adaptive_delay_ceiling, frame_id_is_newer, VideoPlayoutBuffer};
    use crate::video_frame::{FrameDebugTiming, VideoFrameBuffer};
    use std::time::{Duration, Instant};

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
