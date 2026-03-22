use crate::debug_state::{unix_time_micros, ConnectionDebugState};
use crate::decode::VideoDecoder;
use crate::transport::{ReceivedData, UdpReceiver};
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

pub fn run_receive_pipeline(
    frame_buf: Arc<Mutex<VideoFrameBuffer>>,
    debug_state: Arc<ConnectionDebugState>,
    ctx: egui::Context,
    shutdown_rx: Receiver<()>,
    audio_tx: Sender<Vec<u8>>,
    feedback_tx: Sender<TransportFeedback>,
    audio_enabled: Arc<AtomicBool>,
    native_surfaces: Arc<NativeSurfaceControl>,
    control_tx: Sender<ControlMessage>,
    stream_config: StreamConfig,
    udp_socket: UdpSocket,
) {
    let mut receiver = match UdpReceiver::from_socket(udp_socket) {
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

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        let data = match receiver.try_receive() {
            Some(d) => d,
            None => {
                if let Some(stats) = receiver.take_stats() {
                    debug_state.update_transport_window(&stats);
                    let _ = feedback_tx.try_send(stats.feedback());
                }
                std::thread::sleep(Duration::from_micros(500));
                continue;
            }
        };

        if let Some(stats) = receiver.take_stats() {
            debug_state.update_transport_window(&stats);
            let _ = feedback_tx.try_send(stats.feedback());
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
                        && last_recovery_keyframe_request.elapsed() >= Duration::from_millis(250)
                    {
                        let _ = control_tx.try_send(ControlMessage::RequestKeyframe);
                        last_recovery_keyframe_request = Instant::now();
                        if trace {
                            eprintln!("[trace][client] requested recovery keyframe");
                        }
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
                    decoded_frame.debug_timing = latest_timing;
                    if let Some(timing) = decoded_frame.debug_timing.as_ref() {
                        debug_state.record_decoded(timing);
                    }
                    let mut fb = frame_buf.lock().unwrap();
                    std::mem::swap(&mut *fb, &mut decoded_frame);
                    fb.dirty = true;
                    decoded_frame.dirty = false;
                    ctx.request_repaint();
                }
            }
        }
    }
}
