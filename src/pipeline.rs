use crate::debug_state::{unix_time_micros, ConnectionDebugState};
use crate::decode::VideoDecoder;
use crate::transport::{AudioPacket, MediaReceiver, ReceivedData, TransportWindowStats};
use crate::video_frame::{FrameDebugTiming, NativeSurfaceControl, VideoFrameBuffer};
use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use st_protocol::{ControlMessage, StreamConfig, TransportFeedback};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
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
    min_delay: Duration,
    frame_interval: Option<Duration>,
    max_queued_frames: usize,
    queued: VecDeque<QueuedVideoFrame>,
    last_scheduled_at: Option<Instant>,
    last_presented_frame_id: Option<u32>,
}

impl VideoPlayoutBuffer {
    fn new(stream_fps: u16) -> Self {
        Self {
            min_delay: configured_video_jitter_delay(stream_fps),
            frame_interval: if stream_fps > 0 {
                Some(Duration::from_secs_f64(1.0 / f64::from(stream_fps)))
            } else {
                None
            },
            max_queued_frames: configured_video_jitter_max_frames(),
            queued: VecDeque::new(),
            last_scheduled_at: None,
            last_presented_frame_id: None,
        }
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

fn configured_video_jitter_delay(stream_fps: u16) -> Duration {
    if let Ok(raw) = std::env::var("ST_CLIENT_VIDEO_JITTER_MS") {
        if let Ok(parsed) = raw.parse::<u64>() {
            return Duration::from_millis(parsed.min(250));
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Keep the Windows client biased toward low-delay
        // presentation instead of buffering a full extra frame by default.
        if stream_fps == 0 {
            return Duration::from_millis(6);
        }

        return Duration::from_secs_f64((0.5 / f64::from(stream_fps)).clamp(0.003, 0.008));
    }

    #[cfg(not(target_os = "windows"))]
    {
        if stream_fps == 0 {
            return Duration::from_millis(18);
        }

        Duration::from_secs_f64((1.0 / f64::from(stream_fps)).clamp(0.012, 0.030))
    }
}

fn configured_video_jitter_max_frames() -> usize {
    std::env::var("ST_CLIENT_VIDEO_JITTER_MAX_FRAMES")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .map(|value| value.clamp(1, 8))
        .unwrap_or(3)
}

fn recycle_video_frame(recycled_frames: &mut Vec<VideoFrameBuffer>, mut frame: VideoFrameBuffer) {
    if recycled_frames.len() >= 3 {
        return;
    }
    frame.dirty = false;
    recycled_frames.push(frame);
}

fn present_due_video_frames(
    playout: &mut VideoPlayoutBuffer,
    recycled_frames: &mut Vec<VideoFrameBuffer>,
    frame_buf: &Arc<Mutex<VideoFrameBuffer>>,
    ctx: &egui::Context,
    repaint_pacer: &mut RepaintPacer,
) {
    let due = playout.take_due_frames();
    for dropped in due.dropped {
        recycle_video_frame(recycled_frames, dropped);
    }

    let Some(mut frame) = due.present else {
        return;
    };

    let mut fb = frame_buf.lock().unwrap();
    std::mem::swap(&mut *fb, &mut frame);
    fb.dirty = true;
    recycle_video_frame(recycled_frames, frame);
    repaint_pacer.request(ctx);
}

pub fn run_receive_pipeline(
    frame_buf: Arc<Mutex<VideoFrameBuffer>>,
    debug_state: Arc<ConnectionDebugState>,
    debug_enabled: Arc<AtomicBool>,
    ctx: egui::Context,
    shutdown_rx: Receiver<()>,
    audio_tx: Sender<AudioPacket>,
    feedback_tx: Sender<TransportFeedback>,
    decode_started_tx: Sender<()>,
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

        if receiver.take_pending_recovery() {
            decoder.enter_recovery_mode("transport loss");
            request_recovery_keyframe(
                &control_tx,
                &mut last_recovery_keyframe_request,
                trace,
                "immediate transport gap",
            );
        }

        match data {
            None => {
                present_due_video_frames(
                    &mut playout,
                    &mut recycled_frames,
                    &frame_buf,
                    &ctx,
                    &mut repaint_pacer,
                );
                std::thread::sleep(Duration::from_micros(500));
                continue;
            }
            Some(ReceivedData::Audio(opus)) => {
                if audio_enabled.load(Ordering::Relaxed) {
                    let _ = audio_tx.try_send(opus);
                }
            }
            Some(ReceivedData::Video(completed)) => {
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
                let mut pending_video = vec![completed];
                let drain_deadline = Instant::now() + Duration::from_millis(2);
                let mut drained = 0usize;
                loop {
                    if drained >= 64 || Instant::now() >= drain_deadline {
                        break;
                    }
                    match receiver.try_receive() {
                        Some(ReceivedData::Video(newer)) => pending_video.push(newer),
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
                for completed in pending_video {
                    let assembled_micros = unix_time_micros();
                    let decode_start_micros = unix_time_micros();
                    let decoded = match decoder.decode_into(
                        &completed.data,
                        completed.frame_id,
                        &mut decoded_frame,
                    ) {
                        Ok(frame) => frame,
                        Err(e) => {
                            eprintln!("decode error: {e}");
                            if !attempted_software_fallback && decoder.is_hardware_accelerated() {
                                match VideoDecoder::new_software(stream_config.codec) {
                                    Ok(mut software_decoder) => {
                                        attempted_software_fallback = true;
                                        software_decoder
                                            .set_native_surface_control(Arc::clone(
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
                    if decoder.waiting_for_recovery()
                    {
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
                    let decode_done_micros = unix_time_micros();

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
                            client_decode_start_micros: decode_start_micros,
                            client_decode_done_micros: decode_done_micros,
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
                    if let Some(dropped) = playout.enqueue(queued_frame) {
                        recycle_video_frame(&mut recycled_frames, dropped);
                    }
                    present_due_video_frames(
                        &mut playout,
                        &mut recycled_frames,
                        &frame_buf,
                        &ctx,
                        &mut repaint_pacer,
                    );
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
    if !stats.needs_recovery_keyframe()
    {
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
    use super::{frame_id_is_newer, VideoPlayoutBuffer};
    use crate::video_frame::{FrameDebugTiming, VideoFrameBuffer};
    use std::time::Duration;

    fn frame_with_id(frame_id: u32) -> VideoFrameBuffer {
        let mut frame = VideoFrameBuffer::default();
        frame.debug_timing = Some(FrameDebugTiming {
            frame_id,
            ..FrameDebugTiming::default()
        });
        frame
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
        let dropped = playout.enqueue(frame_with_id(10)).expect("stale frame dropped");
        assert_eq!(
            dropped
                .debug_timing
                .as_ref()
                .expect("frame id")
                .frame_id,
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
            presented
                .debug_timing
                .as_ref()
                .expect("frame id")
                .frame_id,
            10
        );

        let dropped = playout.enqueue(frame_with_id(9)).expect("stale frame dropped");
        assert_eq!(
            dropped
                .debug_timing
                .as_ref()
                .expect("frame id")
                .frame_id,
            9
        );
        assert!(playout.queued.is_empty());
    }
}
