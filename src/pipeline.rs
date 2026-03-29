use crate::debug_state::{unix_time_micros, ConnectionDebugState};
use crate::decode::VideoDecoder;
use crate::transport::{AudioPacket, ReceivedData, TransportWindowStats, UdpReceiver};
use crate::video_frame::{FrameDebugTiming, NativeSurfaceControl, VideoFrameBuffer};
use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use st_protocol::{ControlMessage, StreamConfig, TransportFeedback};
use std::net::UdpSocket;
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
    udp_socket: UdpSocket,
    crypto: Option<std::sync::Arc<st_protocol::tunnel::CryptoContext>>,
) {
    let mut receiver = match UdpReceiver::from_socket(udp_socket, crypto) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to create UDP receiver: {e}");
            return;
        }
    };
    let trace = std::env::var_os("ST_TRACE").is_some();
    let mut trace_completed_logged = 0usize;
    let mut last_recovery_keyframe_request = Instant::now() - Duration::from_secs(2);

    let mut decoder = match VideoDecoder::new(stream_config.codec) {
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
    decoder.set_native_surface_control(native_surfaces);
    let mut decoded_frame = VideoFrameBuffer::default();
    let mut repaint_pacer = RepaintPacer::new(present_refresh_millihz);

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        let data = match receiver.try_receive() {
            Some(d) => d,
            None => {
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
                std::thread::sleep(Duration::from_micros(500));
                continue;
            }
        };

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
            ReceivedData::Audio(opus) => {
                if audio_enabled.load(Ordering::Relaxed) {
                    let _ = audio_tx.try_send(opus);
                }
            }
            ReceivedData::Video(completed) => {
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
                    let decoded = match decoder.decode_into(&completed.data, &mut decoded_frame) {
                        Ok(frame) => frame,
                        Err(e) => {
                            eprintln!("decode error: {e}");
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
                    let decode_done_micros = unix_time_micros();

                    if trace && trace_completed_logged < 12 {
                        eprintln!(
                            "[trace][client] decode input frame_id={} produced_frame={decoded}",
                            completed.frame_id
                        );
                        trace_completed_logged += 1;
                    }
                    if decoded {
                        produced_frame = true;
                        latest_timing = Some(FrameDebugTiming {
                            frame_id: completed.frame_id,
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
                    let mut fb = frame_buf.lock().unwrap();
                    std::mem::swap(&mut *fb, &mut decoded_frame);
                    fb.dirty = true;
                    decoded_frame.dirty = false;
                    repaint_pacer.request(&ctx);
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
