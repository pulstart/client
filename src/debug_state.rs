use crate::transport::TransportWindowStats;
use crate::video_frame::{FrameDebugTiming, VideoFormat, VideoFrameBuffer};
use st_protocol::{ClockSyncPong, SessionDebugInfo};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Default)]
pub struct ConnectionDebugSnapshot {
    pub server_addr: String,
    pub display_refresh_millihz: Option<u32>,
    pub decoder_name: String,
    pub encoder_name: String,
    pub capture_backend: String,
    pub input_backend: String,
    pub quality_preset: String,
    pub target_bitrate_kbps: Option<u32>,
    pub received_total_kbps: f32,
    pub received_video_kbps: f32,
    pub received_audio_kbps: f32,
    pub transport_interval_ms: u32,
    pub received_packets: u32,
    pub lost_packets: u32,
    pub late_packets: u32,
    pub completed_frames: u32,
    pub dropped_frames: u32,
    pub receive_fps: f32,
    pub decode_fps: f32,
    pub present_fps: f32,
    pub clock_rtt_ms: Option<f32>,
    pub server_clock_ahead_ms: Option<f32>,
    pub capture_to_send_ms: Option<f32>,
    pub send_to_assemble_ms: Option<f32>,
    pub assemble_to_decode_ms: Option<f32>,
    pub decode_work_ms: Option<f32>,
    pub decode_to_present_ms: Option<f32>,
    pub total_latency_ms: Option<f32>,
    pub last_frame_id: Option<u32>,
    pub last_video_format: String,
    pub last_present_path: String,
}

pub struct ConnectionDebugState {
    inner: Mutex<ConnectionDebugInner>,
}

struct ConnectionDebugInner {
    snapshot: ConnectionDebugSnapshot,
    decode_rate: EventRate,
    present_rate: EventRate,
    capture_to_send_ms: SmoothedValue,
    send_to_assemble_ms: SmoothedValue,
    assemble_to_decode_ms: SmoothedValue,
    decode_work_ms: SmoothedValue,
    decode_to_present_ms: SmoothedValue,
    total_latency_ms: SmoothedValue,
    server_clock_ahead_micros: Option<i64>,
}

#[derive(Default)]
struct SmoothedValue {
    value: Option<f32>,
}

struct EventRate {
    window_started: Instant,
    count: u32,
    fps: f32,
}

impl Default for EventRate {
    fn default() -> Self {
        Self {
            window_started: Instant::now(),
            count: 0,
            fps: 0.0,
        }
    }
}

impl ConnectionDebugState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ConnectionDebugInner {
                snapshot: ConnectionDebugSnapshot::default(),
                decode_rate: EventRate::default(),
                present_rate: EventRate::default(),
                capture_to_send_ms: SmoothedValue::default(),
                send_to_assemble_ms: SmoothedValue::default(),
                assemble_to_decode_ms: SmoothedValue::default(),
                decode_work_ms: SmoothedValue::default(),
                decode_to_present_ms: SmoothedValue::default(),
                total_latency_ms: SmoothedValue::default(),
                server_clock_ahead_micros: None,
            }),
        }
    }

    pub fn reset_for_connect(&self, server_addr: &str, display_refresh_millihz: Option<u32>) {
        let mut inner = self.inner.lock().unwrap();
        inner.snapshot = ConnectionDebugSnapshot {
            server_addr: server_addr.to_string(),
            display_refresh_millihz,
            ..ConnectionDebugSnapshot::default()
        };
        inner.decode_rate = EventRate::default();
        inner.present_rate = EventRate::default();
        inner.capture_to_send_ms = SmoothedValue::default();
        inner.send_to_assemble_ms = SmoothedValue::default();
        inner.assemble_to_decode_ms = SmoothedValue::default();
        inner.decode_work_ms = SmoothedValue::default();
        inner.decode_to_present_ms = SmoothedValue::default();
        inner.total_latency_ms = SmoothedValue::default();
        inner.server_clock_ahead_micros = None;
    }

    pub fn snapshot(&self) -> ConnectionDebugSnapshot {
        self.inner.lock().unwrap().snapshot.clone()
    }

    pub fn set_decoder_name(&self, decoder_name: &str) {
        self.inner.lock().unwrap().snapshot.decoder_name = decoder_name.to_string();
    }

    pub fn set_session_info(&self, info: SessionDebugInfo) {
        let mut inner = self.inner.lock().unwrap();
        inner.snapshot.encoder_name = info.encoder_name;
        inner.snapshot.capture_backend = info.capture_backend;
        inner.snapshot.input_backend = info.input_backend;
        inner.snapshot.quality_preset = info.quality_preset;
        inner.snapshot.target_bitrate_kbps = Some(info.target_bitrate_kbps);
    }

    pub fn update_clock_sync(&self, pong: ClockSyncPong, client_recv_micros: u64) {
        let mut inner = self.inner.lock().unwrap();
        let t1 = pong.client_send_micros as i128;
        let t2 = pong.server_recv_micros as i128;
        let t3 = pong.server_send_micros as i128;
        let t4 = client_recv_micros as i128;

        let server_clock_ahead = ((t2 - t1) + (t3 - t4)) / 2;
        let rtt = (t4 - t1) - (t3 - t2);

        inner.server_clock_ahead_micros = Some(server_clock_ahead as i64);
        inner.snapshot.server_clock_ahead_ms = Some(server_clock_ahead as f32 / 1000.0);
        inner.snapshot.clock_rtt_ms = Some((rtt.max(0) as f32) / 1000.0);
        inner.snapshot.target_bitrate_kbps = Some(pong.bitrate_kbps);
    }

    pub fn update_transport_window(&self, stats: &TransportWindowStats) {
        let mut inner = self.inner.lock().unwrap();
        let interval_ms = stats.interval_ms.max(1);
        inner.snapshot.transport_interval_ms = stats.interval_ms;
        inner.snapshot.received_packets = stats.received_packets;
        inner.snapshot.lost_packets = stats.lost_packets;
        inner.snapshot.late_packets = stats.late_packets;
        inner.snapshot.completed_frames = stats.completed_frames;
        inner.snapshot.dropped_frames = stats.dropped_frames;
        inner.snapshot.receive_fps = stats.completed_frames as f32 * 1000.0 / interval_ms as f32;
        inner.snapshot.received_total_kbps = bytes_to_kbps(stats.received_bytes, interval_ms);
        inner.snapshot.received_video_kbps = bytes_to_kbps(stats.received_video_bytes, interval_ms);
        inner.snapshot.received_audio_kbps = bytes_to_kbps(stats.received_audio_bytes, interval_ms);
    }

    pub fn record_decoded(&self, timing: &FrameDebugTiming) {
        let mut inner = self.inner.lock().unwrap();
        inner.decode_rate.note_event();
        inner.snapshot.decode_fps = inner.decode_rate.fps;
        inner.snapshot.last_frame_id = Some(timing.frame_id);

        if let Some(sample) = micros_diff_ms(
            timing.client_assembled_micros,
            timing.client_decode_done_micros,
        ) {
            let value = inner.assemble_to_decode_ms.update(sample);
            inner.snapshot.assemble_to_decode_ms = Some(value);
        }
        if let Some(sample) =
            micros_diff_ms(timing.server_capture_micros, timing.server_send_micros)
        {
            let value = inner.capture_to_send_ms.update(sample);
            inner.snapshot.capture_to_send_ms = Some(value);
        }
        if let Some(sample) = micros_diff_ms(
            timing.client_decode_start_micros,
            timing.client_decode_done_micros,
        ) {
            let value = inner.decode_work_ms.update(sample);
            inner.snapshot.decode_work_ms = Some(value);
        }
        if let Some(server_clock_ahead_micros) = inner.server_clock_ahead_micros {
            let adjusted_send_micros =
                adjust_server_time_to_client(timing.server_send_micros, server_clock_ahead_micros);
            if let Some(sample) =
                micros_diff_ms(adjusted_send_micros, timing.client_assembled_micros)
            {
                let value = inner.send_to_assemble_ms.update(sample);
                inner.snapshot.send_to_assemble_ms = Some(value);
            }
        }
    }

    pub fn record_present(&self, frame: &VideoFrameBuffer, client_present_micros: u64) {
        let Some(timing) = frame.debug_timing.as_ref() else {
            return;
        };

        let mut inner = self.inner.lock().unwrap();
        inner.present_rate.note_event();
        inner.snapshot.present_fps = inner.present_rate.fps;
        inner.snapshot.last_video_format = format_label(frame.format).to_string();
        inner.snapshot.last_present_path = present_path_label(frame).to_string();

        if let Some(sample) =
            micros_diff_ms(timing.client_decode_done_micros, client_present_micros)
        {
            let value = inner.decode_to_present_ms.update(sample);
            inner.snapshot.decode_to_present_ms = Some(value);
        }

        if let Some(server_clock_ahead_micros) = inner.server_clock_ahead_micros {
            let adjusted_capture_micros = adjust_server_time_to_client(
                timing.server_capture_micros,
                server_clock_ahead_micros,
            );
            if let Some(sample) = micros_diff_ms(adjusted_capture_micros, client_present_micros) {
                let value = inner.total_latency_ms.update(sample);
                inner.snapshot.total_latency_ms = Some(value);
            }
        }
    }
}

impl SmoothedValue {
    fn update(&mut self, sample: f32) -> f32 {
        let next = match self.value {
            Some(current) => current * 0.75 + sample * 0.25,
            None => sample,
        };
        self.value = Some(next);
        next
    }
}

impl EventRate {
    fn note_event(&mut self) {
        self.count = self.count.saturating_add(1);
        let elapsed = self.window_started.elapsed();
        if elapsed.as_millis() >= 500 {
            self.fps = self.count as f32 / elapsed.as_secs_f32();
            self.count = 0;
            self.window_started = Instant::now();
        }
    }
}

fn adjust_server_time_to_client(server_micros: u64, server_clock_ahead_micros: i64) -> u64 {
    if server_clock_ahead_micros >= 0 {
        server_micros.saturating_sub(server_clock_ahead_micros as u64)
    } else {
        server_micros.saturating_add((-server_clock_ahead_micros) as u64)
    }
}

fn micros_diff_ms(start_micros: u64, end_micros: u64) -> Option<f32> {
    if end_micros < start_micros {
        return None;
    }
    Some((end_micros - start_micros) as f32 / 1000.0)
}

fn bytes_to_kbps(bytes: u64, interval_ms: u32) -> f32 {
    if interval_ms == 0 {
        return 0.0;
    }
    (bytes as f32 * 8.0) / interval_ms as f32
}

fn format_label(format: VideoFormat) -> &'static str {
    match format {
        VideoFormat::Rgba8 => "rgba8",
        VideoFormat::Yuv420p8 => "yuv420p",
        VideoFormat::Nv12 => "nv12",
    }
}

fn present_path_label(frame: &VideoFrameBuffer) -> &'static str {
    #[cfg(target_os = "linux")]
    if frame.dmabuf.is_some() {
        return "dmabuf";
    }
    #[cfg(target_os = "macos")]
    if frame.videotoolbox.is_some() {
        return "videotoolbox";
    }
    #[cfg(target_os = "windows")]
    if frame.d3d11.is_some() {
        return "d3d11";
    }

    format_label(frame.format)
}

pub fn unix_time_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u64::MAX as u128) as u64
}
