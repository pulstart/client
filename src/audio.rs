/// Audio playback pipeline: Opus decode -> cpal output.
///
/// Receives raw Opus packets from the unified UDP pipeline (via channel),
/// decodes to float32 PCM, and plays back through the system audio device.
use crossbeam_channel::Receiver;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u32 = 2;
/// Maximum Opus frame size at 48kHz (120ms frame).
const MAX_OPUS_FRAME_SAMPLES: usize = 5760;
/// Maximum audio buffer in samples (~100ms at 48kHz stereo).
const MAX_BUFFER_SAMPLES: usize = (SAMPLE_RATE as usize * CHANNELS as usize) / 10;

pub fn run_audio_pipeline(
    opus_rx: Receiver<Vec<u8>>,
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
    let ring: Arc<Mutex<VecDeque<f32>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(MAX_BUFFER_SAMPLES)));
    let ring_cb = Arc::clone(&ring);

    let stream = device
        .build_output_stream(
            &config,
            move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut buf = ring_cb.lock().unwrap();
                for sample in output.iter_mut() {
                    *sample = buf.pop_front().unwrap_or(0.0);
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

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        let opus_data = match opus_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(d) => d,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };

        match decoder.decode_float(&opus_data, &mut pcm_buf, false) {
            Ok(samples_per_channel) => {
                let total = samples_per_channel * CHANNELS as usize;
                let mut buf = ring.lock().unwrap();
                // Keep latency low: drop old samples if buffer exceeds limit
                if buf.len() + total > MAX_BUFFER_SAMPLES {
                    let buf_len = buf.len();
                    let excess = buf_len + total - MAX_BUFFER_SAMPLES;
                    buf.drain(..excess.min(buf_len));
                }
                buf.extend(&pcm_buf[..total]);
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
