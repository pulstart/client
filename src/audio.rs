/// Audio playback pipeline: Opus decode -> cpal output.
///
/// Receives raw Opus packets from the unified UDP pipeline (via channel),
/// decodes to float32 PCM, and plays back through the system audio device.
use crate::transport::AudioPacket;
use crossbeam_channel::Receiver;
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc, Mutex,
};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u32 = 2;
/// Maximum Opus frame size at 48kHz (120ms frame).
const MAX_OPUS_FRAME_SAMPLES: usize = 5760;
/// Conceal up to this many ms of consecutive missing packets before resyncing.
/// Expressed in ms (not packets) so it stays a fixed time budget regardless of
/// the negotiated Opus frame duration (E1: 5 ms frames → a higher packet cap).
const MAX_CONCEALED_AUDIO_MS: usize = 60;
/// Fallback Opus frame duration when the server doesn't declare one.
const DEFAULT_AUDIO_PACKET_DURATION_MS: usize = 20;
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

struct AudioOutput {
    ring: Arc<Mutex<PlaybackBuffer>>,
    stream: cpal::Stream,
}

fn fill_playback_output(
    playback: &mut PlaybackBuffer,
    output: &mut [f32],
    target_buffer_samples: usize,
) {
    if !playback.primed && playback.samples.len() >= target_buffer_samples {
        playback.primed = true;
    }

    let copied = if playback.primed {
        let count = output.len().min(playback.samples.len());
        let (front, back) = playback.samples.as_slices();
        let front_count = count.min(front.len());
        output[..front_count].copy_from_slice(&front[..front_count]);
        let back_count = count - front_count;
        if back_count > 0 {
            output[front_count..count].copy_from_slice(&back[..back_count]);
        }
        playback.samples.drain(..count);
        if count > 0 {
            playback.last_sample = output[count - 1];
        }
        count
    } else {
        0
    };

    if copied < output.len() {
        if playback.primed {
            playback.primed = false;
            playback.needs_crossfade = true;
        }
        for sample in &mut output[copied..] {
            *sample = playback.last_sample;
            playback.last_sample *= SILENCE_DECAY_PER_SAMPLE;
        }
    }
}

fn create_audio_output(
    target_buffer_samples: usize,
    max_buffer_samples: usize,
) -> Result<AudioOutput, String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let device = cpal::default_host()
        .default_output_device()
        .ok_or("No audio output device")?;
    let config = cpal::StreamConfig {
        channels: CHANNELS as u16,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };
    let ring = Arc::new(Mutex::new(PlaybackBuffer {
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
                // Never block the real-time thread. A callback-wide atomic
                // handoff is enough; doing one atomic and one VecDeque pop per
                // sample wastes substantial CPU at 48 kHz.
                let mut playback = match ring_cb.try_lock() {
                    Ok(playback) => playback,
                    Err(_) => {
                        let mut held = f32::from_bits(last_sample_bits_cb.load(Ordering::Relaxed));
                        for sample in output {
                            *sample = held;
                            held *= SILENCE_DECAY_PER_SAMPLE;
                        }
                        last_sample_bits_cb.store(held.to_bits(), Ordering::Relaxed);
                        return;
                    }
                };
                playback.last_sample = f32::from_bits(last_sample_bits_cb.load(Ordering::Relaxed));
                fill_playback_output(&mut playback, output, target_buffer_samples);
                last_sample_bits_cb.store(playback.last_sample.to_bits(), Ordering::Relaxed);
            },
            |err| eprintln!("[audio] output error: {err}"),
            None,
        )
        .map_err(|e| format!("audio stream: {e}"))?;
    stream.play().map_err(|e| format!("audio play: {e}"))?;
    eprintln!("[audio] Playback started (48kHz stereo)");
    Ok(AudioOutput { ring, stream })
}

fn drop_audio_output(output: &mut Option<AudioOutput>) {
    use cpal::traits::StreamTrait;

    if let Some(output) = output.take() {
        let _ = output.stream.pause();
        eprintln!("[audio] Playback paused");
    }
}

/// E1: derive per-packet audio timing from the negotiated Opus frame duration.
/// `packet_duration_ms == 0` (server declared none) falls back to the default.
/// Returns `(effective_ms, max_concealed_packets, packet_samples)`. Kept pure so
/// the 5 ms-frame sequence-gap / concealment math is unit-tested without cpal.
fn audio_timing(packet_duration_ms: u32) -> (usize, usize, usize) {
    let ms = if packet_duration_ms == 0 {
        DEFAULT_AUDIO_PACKET_DURATION_MS
    } else {
        packet_duration_ms as usize
    };
    let max_concealed_packets = (MAX_CONCEALED_AUDIO_MS / ms.max(1)).max(1);
    let packet_samples = (SAMPLE_RATE as usize * CHANNELS as usize * ms) / 1000;
    (ms, max_concealed_packets, packet_samples)
}

pub fn run_audio_pipeline(
    opus_rx: Receiver<AudioPacket>,
    shutdown_rx: Receiver<()>,
    packet_duration_ms: u32,
    audio_enabled: Arc<AtomicBool>,
) -> Result<(), String> {
    // E1: derive the per-packet timing from the negotiated Opus frame duration
    // rather than a hardcoded 20 ms, so 5 ms frames keep correct sequence-gap /
    // concealment math (and a proportionally higher concealment packet cap).
    let (_packet_duration_ms, max_concealed_packets, packet_samples) =
        audio_timing(packet_duration_ms);

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

    // Decode loop: receive Opus packets from pipeline, decode, push to ring
    let mut pcm_buf = vec![0.0f32; MAX_OPUS_FRAME_SAMPLES * CHANNELS as usize];
    let mut expected_seq = None::<u16>;
    let mut output = None;
    let mut was_enabled = audio_enabled.load(Ordering::Relaxed);
    // `packet_samples` and `max_concealed_packets` are derived above from the
    // negotiated frame duration (E1).
    let trace = std::env::var_os("ST_TRACE").is_some();
    let mut concealment_logs = 0usize;
    let mut backlog_logs = 0usize;

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        let enabled = audio_enabled.load(Ordering::Relaxed);
        if !enabled {
            if was_enabled {
                drop_audio_output(&mut output);
                expected_seq = None;
                let _ = decoder.reset_state();
                while opus_rx.try_recv().is_ok() {}
            }
            was_enabled = false;
        } else {
            was_enabled = true;
        }

        let mut packet = match opus_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(d) => d,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        if !audio_enabled.load(Ordering::Relaxed) {
            continue;
        }
        if output.is_none() {
            output = Some(create_audio_output(
                target_buffer_samples,
                max_buffer_samples,
            )?);
        }
        let ring = &output.as_ref().expect("audio output initialized").ring;
        if trim_packet_backlog(
            &opus_rx,
            &mut packet,
            ring,
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
                    if missing_packets <= max_concealed_packets {
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
                                    ring,
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
                                    ring,
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
                                    ring,
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
                        mark_needs_crossfade(ring);

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
                        mark_needs_crossfade(ring);
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
            ring,
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

    drop_audio_output(&mut output);
    eprintln!("[audio] Pipeline stopped");
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_timing_derives_from_wire_frame_duration() {
        // 20 ms (legacy / restore path): 3 concealed packets, 1920 interleaved
        // samples (960/ch).
        assert_eq!(audio_timing(20), (20, 3, 1920));
        // 5 ms (E1 default): proportionally higher packet cap (12), 240 samples/ch
        // → 480 interleaved stereo samples. The sequence-gap / concealment math
        // scales correctly instead of silently breaking.
        assert_eq!(audio_timing(5), (5, 12, 480));
        // 10 ms: 6 packets, 960 interleaved.
        assert_eq!(audio_timing(10), (10, 6, 960));
        // 0 ⇒ server declared none ⇒ fall back to the default duration.
        assert_eq!(
            audio_timing(0),
            (
                DEFAULT_AUDIO_PACKET_DURATION_MS,
                MAX_CONCEALED_AUDIO_MS / DEFAULT_AUDIO_PACKET_DURATION_MS,
                SAMPLE_RATE as usize * CHANNELS as usize * DEFAULT_AUDIO_PACKET_DURATION_MS / 1000
            )
        );
        // Concealment budget stays a fixed 60 ms regardless of frame size.
        for ms in [5u32, 10, 20] {
            let (eff, max_concealed, _) = audio_timing(ms);
            assert_eq!(max_concealed * eff, MAX_CONCEALED_AUDIO_MS);
        }
    }

    #[test]
    fn playback_callback_drains_samples_in_bulk_and_fades_underrun() {
        let mut playback = PlaybackBuffer {
            samples: VecDeque::from([1.0, 2.0, 3.0, 4.0]),
            primed: false,
            last_sample: 0.0,
            needs_crossfade: false,
        };
        let mut output = [0.0; 6];

        fill_playback_output(&mut playback, &mut output, 4);

        assert_eq!(&output[..5], &[1.0, 2.0, 3.0, 4.0, 4.0]);
        assert!(output[5] < output[4]);
        assert!(playback.samples.is_empty());
        assert!(!playback.primed);
        assert!(playback.needs_crossfade);
    }

    #[test]
    fn playback_callback_waits_for_prebuffer_without_discarding_samples() {
        let mut playback = PlaybackBuffer {
            samples: VecDeque::from([1.0, 2.0]),
            primed: false,
            last_sample: 0.0,
            needs_crossfade: false,
        };
        let mut output = [1.0; 4];

        fill_playback_output(&mut playback, &mut output, 4);

        assert_eq!(output, [0.0; 4]);
        assert_eq!(playback.samples, VecDeque::from([1.0, 2.0]));
    }
}
