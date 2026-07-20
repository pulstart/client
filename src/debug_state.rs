use crate::transport::TransportWindowStats;
use crate::video_frame::{FrameDebugTiming, VideoFormat, VideoFrameBuffer};
use st_protocol::{ClockSyncPong, SessionDebugInfo};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How far back the latency window keeps raw samples for p95/max readouts.
const LATENCY_WINDOW: Duration = Duration::from_secs(3);
/// Window over which the graph latency lane reports its peak (anti-aliasing
/// short hitches that fall between graph sample ticks).
const LATENCY_RECENT_WINDOW: Duration = Duration::from_millis(250);

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
    pub latency_p95_ms: Option<f32>,
    pub latency_max_ms: Option<f32>,
    pub playout_drops: u32,
    pub jitter_delay_ms: f32,
    pub last_frame_id: Option<u32>,
    pub last_video_format: String,
    pub last_present_path: String,
}

/// Lightweight, `Copy` view of just the numeric fields the live graph needs.
///
/// The graph samples every UI frame; cloning the full `ConnectionDebugSnapshot`
/// (with its ~8 `String` fields) that often is pure waste. `metrics()` returns
/// this instead, so the only per-frame allocation-heavy `snapshot()` call is the
/// throttled text-overlay rebuild.
#[derive(Clone, Copy, Default)]
pub struct MetricsSnapshot {
    pub received_video_kbps: f32,
    pub present_fps: f32,
    pub clock_rtt_ms: Option<f32>,
    pub decode_work_ms: Option<f32>,
    pub total_latency_ms: Option<f32>,
    /// Peak end-to-end latency over the last few hundred ms — what the graph's
    /// latency lane plots, so a brief hitch isn't averaged into invisibility.
    pub latency_recent_max_ms: Option<f32>,
    pub received_packets: u32,
    pub lost_packets: u32,
    pub dropped_frames: u32,
    pub completed_frames: u32,
    pub playout_drops: u32,
}

pub struct ConnectionDebugState {
    inner: Mutex<ConnectionDebugInner>,
}

#[derive(Default)]
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
    received_total_kbps: SmoothedValue,
    received_video_kbps: SmoothedValue,
    received_audio_kbps: SmoothedValue,
    receive_fps: SmoothedValue,
    server_clock_ahead_micros: Option<i64>,
    clock_offset_filter: ClockOffsetFilter,
    latency_window: VecDeque<(Instant, f32)>,
    playout_drops: u32,
    jitter_delay_ms: f32,
}

#[derive(Default)]
struct SmoothedValue {
    value: Option<f32>,
}

/// Min-RTT clock-offset filter.
///
/// A single ping/pong yields a noisy offset estimate — its error scales with
/// that round's RTT. Feeding the raw value straight into `total_latency` made it
/// jitter and sometimes flip negative (→ dashed-out). NTP's trick: over a short
/// history, the sample with the *lowest* RTT had the least queuing noise, so its
/// offset is the most trustworthy. We keep a small ring and report that one.
#[derive(Default)]
struct ClockOffsetFilter {
    samples: VecDeque<(i64, i64)>,
}

impl ClockOffsetFilter {
    fn push(&mut self, rtt_micros: i64, offset_micros: i64) -> i64 {
        if self.samples.len() >= 16 {
            self.samples.pop_front();
        }
        self.samples.push_back((rtt_micros.max(0), offset_micros));
        self.samples
            .iter()
            .min_by_key(|(rtt, _)| *rtt)
            .map(|(_, offset)| *offset)
            .unwrap_or(offset_micros)
    }
}

struct EventRate {
    window_started: Instant,
    last_event: Instant,
    count: u32,
    fps: f32,
}

impl Default for EventRate {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            window_started: now,
            last_event: now,
            count: 0,
            fps: 0.0,
        }
    }
}

impl ConnectionDebugState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ConnectionDebugInner::default()),
        }
    }

    pub fn reset_for_connect(&self, server_addr: &str, display_refresh_millihz: Option<u32>) {
        let mut inner = self.inner.lock().unwrap();
        *inner = ConnectionDebugInner::default();
        inner.snapshot.server_addr = server_addr.to_string();
        inner.snapshot.display_refresh_millihz = display_refresh_millihz;
    }

    pub fn snapshot(&self) -> ConnectionDebugSnapshot {
        let inner = self.inner.lock().unwrap();
        let mut snap = inner.snapshot.clone();
        // Recompute the event-driven rates against wall time so a frozen stream
        // decays toward 0 instead of holding the last healthy reading.
        snap.decode_fps = inner.decode_rate.fps_decayed();
        snap.present_fps = inner.present_rate.fps_decayed();
        let (p95, max) = inner.latency_percentile_max();
        snap.latency_p95_ms = p95;
        snap.latency_max_ms = max;
        snap
    }

    /// Cheap, `Copy` numeric view for the live graph (no `String` clones).
    pub fn metrics(&self) -> MetricsSnapshot {
        let inner = self.inner.lock().unwrap();
        let s = &inner.snapshot;
        MetricsSnapshot {
            received_video_kbps: s.received_video_kbps,
            present_fps: inner.present_rate.fps_decayed(),
            clock_rtt_ms: s.clock_rtt_ms,
            decode_work_ms: s.decode_work_ms,
            total_latency_ms: s.total_latency_ms,
            latency_recent_max_ms: inner.latency_recent_max().or(s.total_latency_ms),
            received_packets: s.received_packets,
            lost_packets: s.lost_packets,
            dropped_frames: s.dropped_frames,
            completed_frames: s.completed_frames,
            playout_drops: inner.playout_drops,
        }
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
        let rtt = clock_sync_rtt_micros(t1, t2, t3, t4) as i128;

        // Filter the raw per-pong offset through a min-RTT window so latency
        // stages that depend on it stay stable instead of jittering with RTT.
        let filtered_offset = inner
            .clock_offset_filter
            .push(rtt.max(0) as i64, server_clock_ahead as i64);
        inner.server_clock_ahead_micros = Some(filtered_offset);
        inner.snapshot.server_clock_ahead_ms = Some(filtered_offset as f32 / 1000.0);
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
        // Rates are computed over a variable-length window (the feedback window
        // shortens to ~100ms on urgent loss reports), so the raw per-window value
        // is spiky. EMA-smooth the displayed rates so the graph/readouts don't
        // jump around exactly when loss makes the window shrink.
        let receive_fps_sample = stats.completed_frames as f32 * 1000.0 / interval_ms as f32;
        inner.snapshot.receive_fps = inner.receive_fps.update(receive_fps_sample);
        inner.snapshot.received_total_kbps = inner
            .received_total_kbps
            .update(bytes_to_kbps(stats.received_bytes, interval_ms));
        inner.snapshot.received_video_kbps = inner
            .received_video_kbps
            .update(bytes_to_kbps(stats.received_video_bytes, interval_ms));
        inner.snapshot.received_audio_kbps = inner
            .received_audio_kbps
            .update(bytes_to_kbps(stats.received_audio_bytes, interval_ms));

        // Playout drops accumulate between windows; surface and reset alongside
        // the other window-scoped packet counters.
        inner.snapshot.playout_drops = inner.playout_drops;
        inner.playout_drops = 0;
    }

    /// Current effective adaptive playout/jitter delay (latest-value gauge).
    pub fn set_jitter_delay(&self, ms: f32) {
        let mut inner = self.inner.lock().unwrap();
        inner.jitter_delay_ms = ms;
        inner.snapshot.jitter_delay_ms = ms;
    }

    /// Record video frames the playout/jitter buffer discarded (decoded but
    /// superseded before display) — smoothness churn distinct from network loss.
    pub fn record_playout_drop(&self, count: u32) {
        if count == 0 {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.playout_drops = inner.playout_drops.saturating_add(count);
    }

    pub fn record_decoded(&self, timing: &FrameDebugTiming) {
        let mut inner = self.inner.lock().unwrap();
        inner.decode_rate.note_event();
        inner.snapshot.decode_fps = inner.decode_rate.fps;
        inner.snapshot.last_frame_id = Some(timing.frame_id);

        // Time the assembled unit waited in the receive queue before decode
        // started. This must end at decode *start*, not decode *done* — ending
        // at decode_done would double-count the decode itself (already measured
        // by decode_work_ms) and make the two stages report the same number.
        // Both endpoints are monotonic-clock micros (client-only delta), so an
        // NTP step on the wall clock can't corrupt them.
        if let Some(sample) = micros_diff_ms(
            timing.client_assembled_mono,
            timing.client_decode_start_mono,
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
            timing.client_decode_start_mono,
            timing.client_decode_done_mono,
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

    pub fn record_present(
        &self,
        frame: &VideoFrameBuffer,
        present_wall_micros: u64,
        present_mono_micros: u64,
    ) {
        let Some(timing) = frame.debug_timing.as_ref() else {
            return;
        };

        let mut inner = self.inner.lock().unwrap();
        inner.present_rate.note_event();
        inner.snapshot.present_fps = inner.present_rate.fps;
        inner.snapshot.last_video_format = format_label(frame.format).to_string();
        inner.snapshot.last_present_path = present_path_label(frame).to_string();

        // Client-only stage → monotonic clock.
        if let Some(sample) = micros_diff_ms(timing.client_decode_done_mono, present_mono_micros) {
            let value = inner.decode_to_present_ms.update(sample);
            inner.snapshot.decode_to_present_ms = Some(value);
        }

        // Cross-machine stage → wall clock corrected by the filtered offset.
        if let Some(server_clock_ahead_micros) = inner.server_clock_ahead_micros {
            let adjusted_capture_micros = adjust_server_time_to_client(
                timing.server_capture_micros,
                server_clock_ahead_micros,
            );
            if let Some(sample) = micros_diff_ms(adjusted_capture_micros, present_wall_micros) {
                let value = inner.total_latency_ms.update(sample);
                inner.snapshot.total_latency_ms = Some(value);
                inner.record_latency_sample(sample);
            }
        }
    }
}

impl ConnectionDebugInner {
    fn record_latency_sample(&mut self, sample_ms: f32) {
        let now = Instant::now();
        self.latency_window.push_back((now, sample_ms));
        while let Some(&(t, _)) = self.latency_window.front() {
            if now.duration_since(t) > LATENCY_WINDOW {
                self.latency_window.pop_front();
            } else {
                break;
            }
        }
    }

    /// Peak end-to-end latency over the most recent short window (graph lane).
    fn latency_recent_max(&self) -> Option<f32> {
        let now = Instant::now();
        self.latency_window
            .iter()
            .rev()
            .take_while(|(t, _)| now.duration_since(*t) <= LATENCY_RECENT_WINDOW)
            .map(|(_, v)| *v)
            .fold(None, |acc: Option<f32>, v| {
                Some(acc.map_or(v, |m| m.max(v)))
            })
    }

    /// (p95, max) end-to-end latency over the full ~3s window (text readout).
    fn latency_percentile_max(&self) -> (Option<f32>, Option<f32>) {
        if self.latency_window.is_empty() {
            return (None, None);
        }
        let mut values: Vec<f32> = self.latency_window.iter().map(|(_, v)| *v).collect();
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let max = *values.last().unwrap();
        // Nearest-rank p95.
        let rank = ((values.len() as f32) * 0.95).ceil() as usize;
        let idx = rank.saturating_sub(1).min(values.len() - 1);
        (Some(values[idx]), Some(max))
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
        self.last_event = Instant::now();
        let elapsed = self.window_started.elapsed();
        if elapsed.as_millis() >= 500 {
            self.fps = self.count as f32 / elapsed.as_secs_f32();
            self.count = 0;
            self.window_started = Instant::now();
        }
    }

    /// Frame rate that decays toward zero when events stop arriving.
    ///
    /// `note_event` only recomputes `fps` when an event lands, so a frozen or
    /// stalled stream would otherwise keep reporting the last healthy rate
    /// forever. Once the idle gap grows past ~1.5 frame intervals we treat the
    /// stream as slowing and fall back to `1 / idle_gap`, which converges to 0.
    fn fps_decayed(&self) -> f32 {
        if self.fps <= 0.0 {
            return 0.0;
        }
        let since = self.last_event.elapsed().as_secs_f32();
        let interval = 1.0 / self.fps;
        if since <= interval * 1.5 {
            self.fps
        } else {
            (1.0 / since).min(self.fps)
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
        VideoFormat::Yuv444p8 => "yuv444p",
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

/// Estimated media loss for the last feedback window, as a percentage.
///
/// Packet-gap loss (`lost_packets`) alone under-reports reality: when whole
/// frames never arrive, the assembler reports them as `dropped_frames` with
/// zero `lost_packets`, so a stream visibly dropping frames could read 0%.
/// We fold dropped frames back in by estimating their packet count from the
/// window's average packets-per-completed-frame.
pub fn loss_percent(
    received_packets: u32,
    lost_packets: u32,
    dropped_frames: u32,
    completed_frames: u32,
) -> f32 {
    let completed = completed_frames.max(1) as f32;
    let avg_pkts_per_frame = (received_packets as f32 / completed).max(1.0);
    let estimated_dropped_pkts = dropped_frames as f32 * avg_pkts_per_frame;
    let lost = lost_packets as f32 + estimated_dropped_pkts;
    let total = received_packets as f32 + lost;
    if total > 0.0 {
        lost * 100.0 / total
    } else {
        0.0
    }
}

/// Fraction of *intended* frames that did not display smoothly this window —
/// whole frames that never assembled (`dropped_frames`) plus decoded frames the
/// playout buffer discarded as superseded (`playout_drops`).
///
/// Distinct from [`loss_percent`] on purpose: a stream can stutter badly
/// (irregular server cadence → late frames coalesced/dropped at playout) with
/// **zero packet loss**. That is exactly why hitches were invisible in the loss
/// graph — `loss_percent` counts packets, this counts frames that should have
/// shown but didn't.
pub fn stutter_percent(completed_frames: u32, dropped_frames: u32, playout_drops: u32) -> f32 {
    let intended = completed_frames as f32 + dropped_frames as f32;
    if intended <= 0.0 {
        return 0.0;
    }
    // playout_drops are a subset of completed frames; dropped never completed.
    let missed = (dropped_frames as f32 + playout_drops as f32).min(intended);
    missed * 100.0 / intended
}

/// Monotonic-clock micros since process start. Use this for client-internal
/// durations (decode, queue, present); it can't jump or run backwards the way
/// `unix_time_micros` (wall clock) can when NTP steps the system clock.
pub fn mono_micros() -> u64 {
    st_client_core::mono_micros()
}

pub fn unix_time_micros() -> u64 {
    st_client_core::unix_time_micros()
}

/// Network round-trip time in microseconds from a clock-sync exchange, clamped
/// to ≥0. `t1`=client send, `t2`=server receive, `t3`=server send, `t4`=client
/// receive. Subtracting `(t3−t2)` removes the server's processing dwell, and the
/// client/server clock offset cancels, so the result is the pure network RTT
/// regardless of unsynchronized clocks. Single source for the debug overlay and
/// the B1 `TransportFeedback.rtt_ms` wire field.
pub fn clock_sync_rtt_micros(t1: i128, t2: i128, t3: i128, t4: i128) -> i64 {
    ((t4 - t1) - (t3 - t2)).max(0).min(i64::MAX as i128) as i64
}

#[cfg(test)]
mod stutter_tests {
    use super::{loss_percent, stutter_percent};

    #[test]
    fn stutter_surfaces_playout_drops_that_loss_misses() {
        // The reported case: 66 frames completed, 0 dropped, ~10 dropped at
        // playout, zero packet loss. Loss reads 0% but stutter must not.
        assert_eq!(loss_percent(1121, 0, 0, 66), 0.0);
        let s = stutter_percent(66, 0, 10);
        assert!(s > 0.0, "playout drops must register as stutter");
        assert!((s - (10.0 / 66.0 * 100.0)).abs() < 0.01);
    }

    #[test]
    fn stutter_zero_when_smooth() {
        assert_eq!(stutter_percent(120, 0, 0), 0.0);
        assert_eq!(stutter_percent(0, 0, 0), 0.0);
    }

    #[test]
    fn stutter_counts_dropped_frames_too() {
        // 50 completed + 10 whole frames never assembled → 10/60.
        let s = stutter_percent(50, 10, 0);
        assert!((s - (10.0 / 60.0 * 100.0)).abs() < 0.01);
    }
}

#[cfg(test)]
mod clock_sync_tests {
    use super::clock_sync_rtt_micros;

    #[test]
    fn rtt_excludes_server_dwell_and_clock_offset() {
        // Client clock is 1_000_000 µs behind the server, RTT is 20 ms split
        // 10 ms each way, server dwells 5 ms. t in µs.
        // t1 client-send=0; packet reaches server (server clock +1_000_000)
        // 10 ms later → t2=1_010_000; server dwells 5 ms → t3=1_015_000;
        // reply reaches client 10 ms later → t4 (client clock)=25_000.
        let rtt = clock_sync_rtt_micros(0, 1_010_000, 1_015_000, 25_000);
        // (25_000 − 0) − (1_015_000 − 1_010_000) = 25_000 − 5_000 = 20_000 µs.
        assert_eq!(rtt, 20_000);
    }

    #[test]
    fn negative_clamps_to_zero() {
        // Pathological/jittered samples must never produce a negative RTT.
        assert_eq!(clock_sync_rtt_micros(100, 0, 0, 0), 0);
    }
}
