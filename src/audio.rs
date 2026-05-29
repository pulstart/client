/// Audio playback pipeline: Opus decode -> cpal output.
///
/// Receives raw Opus packets from the unified UDP pipeline (via channel),
/// decodes to float32 PCM, and plays back through the system audio device.
use crate::transport::AudioPacket;
use crossbeam_channel::Receiver;
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u32 = 2;
/// Maximum Opus frame size at 48kHz (120ms frame).
const MAX_OPUS_FRAME_SAMPLES: usize = 5760;
/// Conceal up to 60ms of consecutive missing packets before resyncing.
const MAX_CONCEALED_AUDIO_PACKETS: usize = 3;
const AUDIO_PACKET_DURATION_MS: usize = 20;
/// Steady-state playback buffer. Two packets of jitter cushion on top of the
/// 20ms producer cadence. Tuned for game-streaming latency, not studio audio.
const DEFAULT_TARGET_BUFFER_MS: usize = 60;
/// Upper bound on the playback buffer before draining excess samples.
const DEFAULT_MAX_BUFFER_MS: usize = 140;
/// Channel backlog before the decode thread drops stale packets. Loose enough
/// that normal scheduler bursts (3-4 packets arriving together) don't trigger
/// a drop; tight enough that a real stall doesn't accumulate audible latency.
const DEFAULT_MAX_QUEUED_PACKETS: usize = 4;
/// Per-sample decay applied during underruns and brief lock contention. Fades
/// the last real sample toward silence over ~30ms instead of slamming to zero,
/// which removes the audible click at the boundary.
const SILENCE_DECAY_PER_SAMPLE: f32 = 0.995;
/// Crossfade window applied across any known waveform discontinuity (underrun,
/// mid-buffer trim, large packet gap, concealment-to-primary boundary). ~2ms
/// at 48kHz interleaved stereo — short enough to be inaudible as delay, long
/// enough to mask a sample-level jump as a smooth ramp.
const CROSSFADE_SAMPLES: usize = 192;

struct PlaybackBuffer {
    samples: VecDeque<f32>,
    primed: bool,
    last_sample: f32,
    /// Set when the next chunk pushed into `samples` is known to start at a
    /// value that may not match what cpal last emitted (or what is currently
    /// at the back of the buffer). The next decode applies a crossfade ramp
    /// over [`CROSSFADE_SAMPLES`] to mask the jump.
    needs_crossfade: bool,
}

pub fn run_audio_pipeline(
    opus_rx: Receiver<AudioPacket>,
    shutdown_rx: Receiver<()>,
) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let target_buffer_samples =
        configured_audio_buffer_samples("ST_CLIENT_AUDIO_BUFFER_MS", DEFAULT_TARGET_BUFFER_MS);
    let max_buffer_samples =
        configured_audio_buffer_samples("ST_CLIENT_AUDIO_MAX_BUFFER_MS", DEFAULT_MAX_BUFFER_MS)
            .max(target_buffer_samples * 2);
    let max_queued_packets = std::env::var("ST_CLIENT_AUDIO_MAX_BACKLOG_PACKETS")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .map(|value| value.clamp(1, 8))
        .unwrap_or(DEFAULT_MAX_QUEUED_PACKETS);

    // Create Opus decoder
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
        .map_err(|e| format!("Opus decoder: {e}"))?;

    // Create cpal audio output
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("No audio output device")?;

    let config = cpal::StreamConfig {
        channels: CHANNELS as u16,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    // Shared ring buffer between decode thread and cpal callback
    let ring: Arc<Mutex<PlaybackBuffer>> = Arc::new(Mutex::new(PlaybackBuffer {
        samples: VecDeque::with_capacity(max_buffer_samples),
        primed: false,
        last_sample: 0.0,
        needs_crossfade: false,
    }));
    let ring_cb = Arc::clone(&ring);
    let last_sample_bits = Arc::new(AtomicU32::new(0.0f32.to_bits()));
    let last_sample_bits_cb = Arc::clone(&last_sample_bits);

    let stream = device
        .build_output_stream(
            &config,
            move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // Use try_lock to avoid blocking the real-time audio thread.
                // If the decode thread holds the lock, repeat the last sample.
                let mut buf = match ring_cb.try_lock() {
                    Ok(b) => b,
                    Err(_) => {
                        // Real-time callback can't block. Fade from the last
                        // real sample toward silence so brief contention sounds
                        // like a dip, not a DC step or a click.
                        let mut held = f32::from_bits(last_sample_bits_cb.load(Ordering::Relaxed));
                        for sample in output.iter_mut() {
                            *sample = held;
                            held *= SILENCE_DECAY_PER_SAMPLE;
                        }
                        last_sample_bits_cb.store(held.to_bits(), Ordering::Relaxed);
                        return;
                    }
                };
                if !buf.primed && buf.samples.len() >= target_buffer_samples {
                    buf.primed = true;
                }
                for sample in output.iter_mut() {
                    if buf.primed {
                        if let Some(next) = buf.samples.pop_front() {
                            buf.last_sample = next;
                            last_sample_bits_cb.store(next.to_bits(), Ordering::Relaxed);
                            *sample = next;
                        } else {
                            buf.primed = false;
                            buf.needs_crossfade = true;
                            *sample = buf.last_sample;
                            buf.last_sample *= SILENCE_DECAY_PER_SAMPLE;
                        }
                    } else {
                        *sample = buf.last_sample;
                        buf.last_sample *= SILENCE_DECAY_PER_SAMPLE;
                    }
                }
            },
            |err| eprintln!("[audio] output error: {err}"),
            None,
        )
        .map_err(|e| format!("audio stream: {e}"))?;
    stream.play().map_err(|e| format!("audio play: {e}"))?;

    eprintln!("[audio] Playback started (48kHz stereo)");

    // Decode loop: receive Opus packets from pipeline, decode, push to ring
    let mut pcm_buf = vec![0.0f32; MAX_OPUS_FRAME_SAMPLES * CHANNELS as usize];
    let mut expected_seq = None::<u16>;
    let packet_samples = audio_packet_samples();
    let trace = std::env::var_os("ST_TRACE").is_some();
    let mut concealment_logs = 0usize;
    let mut backlog_logs = 0usize;

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        let mut packet = match opus_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(d) => d,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        if trim_packet_backlog(
            &opus_rx,
            &mut packet,
            &ring,
            max_queued_packets,
            target_buffer_samples,
            packet_samples,
            trace,
            &mut backlog_logs,
        ) {
            expected_seq = None;
        }

        if let Some(expected) = expected_seq {
            let delta = packet.seq.wrapping_sub(expected);
            if delta != 0 {
                if delta < 0x8000 {
                    let missing_packets = delta as usize;
                    if missing_packets <= MAX_CONCEALED_AUDIO_PACKETS {
                        let redundancy_count = packet.redundant_prev.len();
                        let mut via_redundancy = 0usize;
                        let mut via_fec = 0usize;
                        let mut via_plc = 0usize;
                        for i in 0..missing_packets {
                            // distance_from_primary in 1..=missing_packets;
                            // 1 = the slot immediately before the primary packet.
                            let distance_from_primary = (missing_packets - i) as u16;
                            let mut decoded = false;
                            if (distance_from_primary as usize) <= redundancy_count {
                                let idx = redundancy_count - distance_from_primary as usize;
                                if decode_and_enqueue(
                                    &mut decoder,
                                    &packet.redundant_prev[idx],
                                    false,
                                    &mut pcm_buf,
                                    &ring,
                                    target_buffer_samples,
                                    max_buffer_samples,
                                )
                                .is_ok()
                                {
                                    via_redundancy += 1;
                                    decoded = true;
                                }
                            }
                            // Slot immediately before the primary is also
                            // recoverable from the primary's in-band LBRR FEC.
                            if !decoded
                                && distance_from_primary == 1
                                && decode_and_enqueue(
                                    &mut decoder,
                                    &packet.data,
                                    true,
                                    &mut pcm_buf,
                                    &ring,
                                    target_buffer_samples,
                                    max_buffer_samples,
                                )
                                .is_ok()
                            {
                                via_fec += 1;
                                decoded = true;
                            }
                            if !decoded
                                && decode_and_enqueue(
                                    &mut decoder,
                                    &[],
                                    false,
                                    &mut pcm_buf,
                                    &ring,
                                    target_buffer_samples,
                                    max_buffer_samples,
                                )
                                .is_ok()
                            {
                                via_plc += 1;
                            }
                        }

                        // PLC / FEC / redundancy tails may not perfectly match
                        // the next real packet's first sample. Crossfade the
                        // boundary on the upcoming primary push.
                        mark_needs_crossfade(&ring);

                        if trace && concealment_logs < 12 {
                            eprintln!(
                                "[trace][audio] recovered {} missing packet(s) before seq={} via redundancy={} fec={} plc={}",
                                via_redundancy + via_fec + via_plc,
                                packet.seq,
                                via_redundancy,
                                via_fec,
                                via_plc
                            );
                            concealment_logs += 1;
                        }
                    } else {
                        expected_seq = None;
                        // Hard resync — buffer (if any) ends at one waveform,
                        // primary starts at another. Crossfade the join.
                        mark_needs_crossfade(&ring);
                        if trace && concealment_logs < 12 {
                            eprintln!(
                                "[trace][audio] large audio gap ({} packets), resyncing at seq={}",
                                missing_packets, packet.seq
                            );
                            concealment_logs += 1;
                        }
                    }
                } else {
                    continue;
                }
            }
        }

        match decode_and_enqueue(
            &mut decoder,
            &packet.data,
            false,
            &mut pcm_buf,
            &ring,
            target_buffer_samples,
            max_buffer_samples,
        ) {
            Ok(()) => {
                expected_seq = Some(packet.seq.wrapping_add(1));
            }
            Err(e) => {
                eprintln!("[audio] decode error: {e}");
            }
        }
    }

    drop(stream);
    eprintln!("[audio] Pipeline stopped");
    Ok(())
}

fn audio_packet_samples() -> usize {
    (SAMPLE_RATE as usize * CHANNELS as usize * AUDIO_PACKET_DURATION_MS) / 1000
}

fn configured_audio_buffer_samples(var: &str, default_ms: usize) -> usize {
    let buffer_ms = std::env::var(var)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .map(|value| value.clamp(10, 200))
        .unwrap_or(default_ms);
    (SAMPLE_RATE as usize * CHANNELS as usize * buffer_ms) / 1000
}

#[allow(clippy::too_many_arguments)]
fn trim_packet_backlog(
    opus_rx: &Receiver<AudioPacket>,
    latest_packet: &mut AudioPacket,
    ring: &Arc<Mutex<PlaybackBuffer>>,
    max_queued_packets: usize,
    target_buffer_samples: usize,
    packet_samples: usize,
    trace: bool,
    backlog_logs: &mut usize,
) -> bool {
    if opus_rx.len() <= max_queued_packets {
        return false;
    }

    let mut dropped_packets = 0usize;
    while opus_rx.len() > max_queued_packets {
        match opus_rx.try_recv() {
            Ok(newer) => {
                *latest_packet = newer;
                dropped_packets += 1;
            }
            Err(_) => break,
        }
    }

    if dropped_packets == 0 {
        return false;
    }

    trim_playback_buffer(
        ring,
        dropped_packets.saturating_mul(packet_samples),
        target_buffer_samples,
    );
    if trace && *backlog_logs < 12 {
        eprintln!(
            "[trace][audio] dropped {} stale queued packet(s) to cut playback latency",
            dropped_packets
        );
        *backlog_logs += 1;
    }
    true
}

fn trim_playback_buffer(
    ring: &Arc<Mutex<PlaybackBuffer>>,
    dropped_samples: usize,
    target_buffer_samples: usize,
) {
    let mut playback = ring.lock().unwrap();
    let to_drop = dropped_samples.min(playback.samples.len());
    let did_drop = to_drop > 0 || dropped_samples > to_drop;
    if to_drop > 0 {
        playback.samples.drain(..to_drop);
    }
    if dropped_samples > to_drop {
        playback.samples.clear();
    }
    if playback.samples.is_empty() {
        playback.primed = false;
        if did_drop {
            // Next push will need to ramp from buf.last_sample.
            playback.needs_crossfade = true;
        }
    } else if playback.samples.len() > target_buffer_samples.saturating_mul(2) {
        let trim_extra = playback.samples.len() - target_buffer_samples.saturating_mul(2);
        playback.samples.drain(..trim_extra);
        crossfade_buffer_front(&mut playback);
    } else if did_drop {
        crossfade_buffer_front(&mut playback);
    }
}

fn crossfade_buffer_front(playback: &mut PlaybackBuffer) {
    let len = playback.samples.len().min(CROSSFADE_SAMPLES);
    if len == 0 {
        return;
    }
    let anchor = playback.last_sample;
    for i in 0..len {
        let t = (i + 1) as f32 / len as f32;
        let v = playback.samples[i];
        playback.samples[i] = anchor * (1.0 - t) + v * t;
    }
}

fn mark_needs_crossfade(ring: &Arc<Mutex<PlaybackBuffer>>) {
    if let Ok(mut playback) = ring.lock() {
        playback.needs_crossfade = true;
    }
}

fn apply_crossfade_ramp(chunk: &mut [f32], anchor: f32) {
    let len = chunk.len().min(CROSSFADE_SAMPLES);
    if len == 0 {
        return;
    }
    for (i, sample) in chunk.iter_mut().take(len).enumerate() {
        let t = (i + 1) as f32 / len as f32;
        *sample = anchor * (1.0 - t) + *sample * t;
    }
}

fn decode_and_enqueue(
    decoder: &mut opus::Decoder,
    opus_data: &[u8],
    fec: bool,
    pcm_buf: &mut [f32],
    ring: &Arc<Mutex<PlaybackBuffer>>,
    target_buffer_samples: usize,
    max_buffer_samples: usize,
) -> Result<(), opus::Error> {
    let samples_per_channel = decoder.decode_float(opus_data, pcm_buf, fec)?;
    let total = samples_per_channel * CHANNELS as usize;
    let mut playback = ring.lock().unwrap();
    if total > 0 && playback.needs_crossfade {
        // Anchor on the actual sample cpal will emit just before this chunk:
        // the back of the buffer if non-empty, otherwise the cpal's last
        // emitted sample (which has been decaying during the underrun).
        let anchor = playback
            .samples
            .back()
            .copied()
            .unwrap_or(playback.last_sample);
        apply_crossfade_ramp(&mut pcm_buf[..total], anchor);
        playback.needs_crossfade = false;
    }
    playback.samples.extend(&pcm_buf[..total]);
    if playback.samples.len() > max_buffer_samples {
        let retain = target_buffer_samples
            .saturating_add(total)
            .min(max_buffer_samples);
        let excess = playback.samples.len().saturating_sub(retain);
        playback.samples.drain(..excess);
    }
    Ok(())
}
