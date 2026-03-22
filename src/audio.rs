/// Audio playback pipeline: Opus decode -> cpal output.
///
/// Receives raw Opus packets from the unified UDP pipeline (via channel),
/// decodes to float32 PCM, and plays back through the system audio device.
use crate::transport::AudioPacket;
use crossbeam_channel::Receiver;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u32 = 2;
/// Maximum Opus frame size at 48kHz (120ms frame).
const MAX_OPUS_FRAME_SAMPLES: usize = 5760;
/// Target steady-state client audio buffer (~40ms at 48kHz stereo).
const TARGET_BUFFER_SAMPLES: usize = (SAMPLE_RATE as usize * CHANNELS as usize) / 25;
/// Maximum audio buffer in samples (~120ms at 48kHz stereo).
const MAX_BUFFER_SAMPLES: usize = (SAMPLE_RATE as usize * CHANNELS as usize * 12) / 100;
/// Conceal up to 60ms of consecutive missing packets before resyncing.
const MAX_CONCEALED_AUDIO_PACKETS: usize = 3;

struct PlaybackBuffer {
    samples: VecDeque<f32>,
    primed: bool,
    last_sample: f32,
}

pub fn run_audio_pipeline(
    opus_rx: Receiver<AudioPacket>,
    shutdown_rx: Receiver<()>,
) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

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
        samples: VecDeque::with_capacity(MAX_BUFFER_SAMPLES),
        primed: false,
        last_sample: 0.0,
    }));
    let ring_cb = Arc::clone(&ring);

    let stream = device
        .build_output_stream(
            &config,
            move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut buf = ring_cb.lock().unwrap();
                if !buf.primed && buf.samples.len() >= TARGET_BUFFER_SAMPLES {
                    buf.primed = true;
                }
                for sample in output.iter_mut() {
                    if buf.primed {
                        if let Some(next) = buf.samples.pop_front() {
                            buf.last_sample = next;
                            *sample = next;
                        } else {
                            buf.primed = false;
                            *sample = buf.last_sample;
                        }
                    } else {
                        *sample = 0.0;
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
    let trace = std::env::var_os("ST_TRACE").is_some();
    let mut concealment_logs = 0usize;

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        let packet = match opus_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(d) => d,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };

        if let Some(expected) = expected_seq {
            let delta = packet.seq.wrapping_sub(expected);
            if delta != 0 {
                if delta < 0x8000 {
                    let missing_packets = delta as usize;
                    if missing_packets <= MAX_CONCEALED_AUDIO_PACKETS {
                        let mut concealed_packets = 0usize;
                        if missing_packets > 1 {
                            for _ in 0..(missing_packets - 1) {
                                if decode_and_enqueue(&mut decoder, &[], false, &mut pcm_buf, &ring)
                                    .is_ok()
                                {
                                    concealed_packets += 1;
                                }
                            }
                        }

                        let recovered_with_fec = decode_and_enqueue(
                            &mut decoder,
                            &packet.data,
                            true,
                            &mut pcm_buf,
                            &ring,
                        )
                        .is_ok();
                        if recovered_with_fec {
                            concealed_packets += 1;
                        } else if decode_and_enqueue(&mut decoder, &[], false, &mut pcm_buf, &ring)
                            .is_ok()
                        {
                            concealed_packets += 1;
                        }

                        if trace && concealment_logs < 12 {
                            eprintln!(
                                "[trace][audio] concealed {} missing packet(s) before seq={}",
                                concealed_packets, packet.seq
                            );
                            concealment_logs += 1;
                        }
                    } else {
                        expected_seq = None;
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

        match decode_and_enqueue(&mut decoder, &packet.data, false, &mut pcm_buf, &ring) {
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

fn decode_and_enqueue(
    decoder: &mut opus::Decoder,
    opus_data: &[u8],
    fec: bool,
    pcm_buf: &mut [f32],
    ring: &Arc<Mutex<PlaybackBuffer>>,
) -> Result<(), opus::Error> {
    let samples_per_channel = decoder.decode_float(opus_data, pcm_buf, fec)?;
    let total = samples_per_channel * CHANNELS as usize;
    let mut playback = ring.lock().unwrap();
    if playback.samples.len() + total > MAX_BUFFER_SAMPLES {
        let buf_len = playback.samples.len();
        let excess = buf_len + total - MAX_BUFFER_SAMPLES;
        playback.samples.drain(..excess.min(buf_len));
    }
    playback.samples.extend(&pcm_buf[..total]);
    Ok(())
}
