/// Audio playback: Opus decode -> self-tuning playout ring -> cpal output.
use crate::transport::AudioPacket;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::Receiver;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, MutexGuard, TryLockError,
};
use std::time::{Duration, Instant};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: usize = 2;
/// Maximum Opus frame size at 48kHz (120ms frame).
const MAX_OPUS_FRAME_SAMPLES: usize = 5760;
/// Conceal up to this many ms of consecutive missing packets before resyncing.
const MAX_CONCEALED_AUDIO_MS: usize = 60;
/// Fallback Opus frame duration when the server doesn't declare one.
const DEFAULT_AUDIO_PACKET_DURATION_MS: usize = 20;

const fn ms_frames(ms: usize) -> usize {
    SAMPLE_RATE as usize * ms / 1000
}

// Playout steering. Each push measures how late its audio arrived against the
// best transit seen recently; the target playout delay is the worst lateness
// of the last minute plus one packet. The delay itself (arrival baseline to
// what the device just took) is immune to jitter, so it only moves with clock
// skew, device stalls and gaps, and is steered back to the target.
const WINDOW_FRAMES: usize = ms_frames(250);
/// Windows of transit history forming the arrival baseline (2 s).
const TRANSIT_WINDOWS: usize = 8;
/// Windows a lateness peak is held (60 s) before decaying.
const LATE_WINDOWS: usize = 240;
const LATE_DECAY_PER_WINDOW: f64 = ms_frames(1) as f64;
/// Windows of measured delay the controller acts on.
const DELAY_WINDOWS: usize = 2;
const SAFETY_FRAMES: usize = ms_frames(2);
/// Covers ordinary Wi-Fi jitter before the first spike has taught the buffer.
const MIN_TARGET_FRAMES: usize = ms_frames(20);
const MAX_TARGET_FRAMES: usize = ms_frames(60);
const DEADBAND_FRAMES: f64 = ms_frames(1) as f64 / 2.0;
/// A delay error is corrected over this many frames, at most `MAX_RATE`.
const CORRECTION_FRAMES: f64 = 2.0 * SAMPLE_RATE as f64;
const MAX_RATE: f64 = 0.005;
/// Excess this large is spliced out at once instead of steered.
const SPLICE_EXCESS_FRAMES: f64 = ms_frames(40) as f64;
/// Audio later than any target could cover means an idle source or an outage:
/// restart the baseline instead of holding a minute of extra latency.
const REBASELINE_FRAMES: f64 = MAX_TARGET_FRAMES as f64;
/// Safety cap for a ring nothing drains.
const HARD_CAP_FRAMES: usize = ms_frames(300);
const FADE_FRAMES: usize = ms_frames(2);
const SPLICE_FADE_FRAMES: usize = ms_frames(5);
const HOLD_DECAY: f32 = 0.99;
/// Device buffer sizes (frames at 48 kHz) tried in order; past the end the
/// backend default is used.
const DEVICE_BUFFER_LADDER: [u32; 3] = [480, 960, 1920];
const LOCK_SPINS: usize = 256;
/// Frames searched for the smoothest point to drop or insert a frame.
const SEARCH_FRAMES: usize = 96;

const RETRY_INITIAL: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const HEALTHY_RUN: Duration = Duration::from_secs(10);
const XRUN_WARMUP: Duration = Duration::from_millis(500);
const XRUN_SPAN: Duration = Duration::from_secs(10);
const XRUN_STRIKES: u32 = 3;

/// Times are frames since `origin`.
struct PlaybackBuffer {
    samples: VecDeque<f32>,
    scratch: Vec<f32>,
    packet_frames: usize,
    origin: Instant,
    pushed: f64,
    consumed: f64,
    transit_window: f64,
    transit_history: VecDeque<f64>,
    late_window: f64,
    late_history: VecDeque<f64>,
    late_peak: f64,
    delay_window: f64,
    delay_history: VecDeque<f64>,
    window_frames: usize,
    primed: bool,
    fade_in: bool,
    rate: f64,
    acc: f64,
    pending_cut: usize,
    underruns: u64,
}

impl PlaybackBuffer {
    fn new(packet_frames: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(HARD_CAP_FRAMES * CHANNELS),
            scratch: Vec::new(),
            packet_frames: packet_frames.max(1),
            origin: Instant::now(),
            pushed: 0.0,
            consumed: 0.0,
            transit_window: f64::INFINITY,
            transit_history: VecDeque::with_capacity(TRANSIT_WINDOWS),
            late_window: 0.0,
            late_history: VecDeque::with_capacity(LATE_WINDOWS),
            late_peak: 0.0,
            delay_window: f64::INFINITY,
            delay_history: VecDeque::with_capacity(DELAY_WINDOWS),
            window_frames: 0,
            primed: false,
            fade_in: false,
            rate: 0.0,
            acc: 0.0,
            pending_cut: 0,
            underruns: 0,
        }
    }

    fn now(&self) -> f64 {
        self.origin.elapsed().as_secs_f64() * SAMPLE_RATE as f64
    }

    fn frames(&self) -> usize {
        self.samples.len() / CHANNELS
    }

    fn transit_min(&self) -> f64 {
        self.transit_history
            .iter()
            .copied()
            .fold(self.transit_window, f64::min)
    }

    fn target(&self) -> f64 {
        (self.late_peak + (self.packet_frames + SAFETY_FRAMES) as f64)
            .clamp(MIN_TARGET_FRAMES as f64, MAX_TARGET_FRAMES as f64)
    }

    fn delay(&self, now: f64) -> f64 {
        now - self.transit_min() - self.consumed
    }

    /// Drops queued audio for a new device; the lateness history (the
    /// network's) is kept.
    fn reset(&mut self) {
        self.samples.clear();
        self.pushed = 0.0;
        self.consumed = 0.0;
        self.transit_window = f64::INFINITY;
        self.transit_history.clear();
        self.delay_window = f64::INFINITY;
        self.delay_history.clear();
        self.window_frames = 0;
        self.primed = false;
        self.fade_in = false;
        self.rate = 0.0;
        self.acc = 0.0;
        self.pending_cut = 0;
    }

    fn push(&mut self, pcm: &[f32], discontinuity: bool) {
        let now = self.now();
        self.push_at(pcm, discontinuity, now);
    }

    fn push_at(&mut self, pcm: &[f32], discontinuity: bool, now: f64) {
        let start = self.samples.len();
        self.samples.extend(pcm);
        if discontinuity {
            fade_out_before(&mut self.samples, start, FADE_FRAMES);
            fade_in_from(&mut self.samples, start, FADE_FRAMES);
        }
        self.pushed += (pcm.len() / CHANNELS) as f64;
        let transit = now - self.pushed;
        let late = transit - self.transit_min();
        if late > REBASELINE_FRAMES {
            self.transit_history.clear();
            self.transit_window = transit;
        } else {
            self.transit_window = self.transit_window.min(transit);
            let late = late.max(0.0);
            self.late_window = self.late_window.max(late);
            self.late_peak = self.late_peak.max(late);
        }
        if self.frames() > HARD_CAP_FRAMES {
            let cut = self.frames() - (self.target() as usize + self.packet_frames);
            self.consumed += splice_front(&mut self.samples, cut, SPLICE_FADE_FRAMES) as f64;
        }
    }

    fn render(&mut self, out: &mut [f32], held: &mut [f32; CHANNELS]) {
        let now = self.now();
        self.render_at(out, held, now);
    }

    fn render_at(&mut self, out: &mut [f32], held: &mut [f32; CHANNELS], now: f64) {
        let n = out.len() / CHANNELS;
        if n == 0 {
            return;
        }
        if !self.primed {
            if self.frames() < n + self.packet_frames || self.delay(now) < self.target() {
                hold(out, held);
                return;
            }
            self.primed = true;
            self.fade_in = true;
        }
        if self.pending_cut > 0 && self.frames() >= self.pending_cut + SPLICE_FADE_FRAMES + n {
            self.consumed +=
                splice_front(&mut self.samples, self.pending_cut, SPLICE_FADE_FRAMES) as f64;
            self.pending_cut = 0;
        }
        let mut adjust = self.next_adjust(n);
        if self.frames() < n.saturating_add_signed(adjust) {
            self.acc += adjust as f64;
            adjust = 0;
        }
        if self.frames() < n {
            self.underrun(out, held);
            return;
        }
        self.take(out, adjust);
        self.consumed += n.saturating_add_signed(adjust) as f64;
        if self.fade_in {
            crossfade_from(held, out, FADE_FRAMES);
            self.fade_in = false;
        }
        held.copy_from_slice(&out[out.len() - CHANNELS..]);
        self.delay_window = self.delay_window.min(self.delay(now));
        self.observe(n);
    }

    /// Plays what is left, then fades; playback resumes once the target delay
    /// is back, which the gap itself restores.
    fn underrun(&mut self, out: &mut [f32], held: &mut [f32; CHANNELS]) {
        let got = self.frames() * CHANNELS;
        copy_front(&mut self.samples, &mut out[..got]);
        self.samples.clear();
        self.consumed += (got / CHANNELS) as f64;
        if got > 0 {
            held.copy_from_slice(&out[got - CHANNELS..got]);
        }
        hold(&mut out[got..], held);
        self.primed = false;
        self.underruns += 1;
    }

    /// Frames to drop (positive) or insert (negative) in the next `n` output frames.
    fn next_adjust(&mut self, n: usize) -> isize {
        if self.rate == 0.0 {
            return 0;
        }
        self.acc += self.rate * n as f64;
        let cap = (n / 8) as f64;
        let adjust = self.acc.trunc().clamp(-cap, cap);
        self.acc -= adjust;
        adjust as isize
    }

    fn take(&mut self, out: &mut [f32], adjust: isize) {
        if adjust == 0 {
            copy_front(&mut self.samples, out);
            return;
        }
        let input = (out.len() / CHANNELS).saturating_add_signed(adjust) * CHANNELS;
        let (front, back) = self.samples.as_slices();
        let from_front = input.min(front.len());
        self.scratch.clear();
        self.scratch.extend_from_slice(&front[..from_front]);
        self.scratch.extend_from_slice(&back[..input - from_front]);
        self.samples.drain(..input);
        if adjust > 0 {
            drop_frames(&self.scratch, out, adjust as usize);
        } else {
            insert_frames(&self.scratch, out, adjust.unsigned_abs());
        }
    }

    fn observe(&mut self, n: usize) {
        self.window_frames += n;
        if self.window_frames < WINDOW_FRAMES {
            return;
        }
        self.window_frames = 0;
        roll(
            &mut self.transit_history,
            TRANSIT_WINDOWS,
            self.transit_window,
        );
        self.transit_window = f64::INFINITY;
        roll(&mut self.late_history, LATE_WINDOWS, self.late_window);
        self.late_window = 0.0;
        let held_peak = self.late_history.iter().copied().fold(0.0, f64::max);
        self.late_peak = held_peak.max(self.late_peak - LATE_DECAY_PER_WINDOW);
        roll(&mut self.delay_history, DELAY_WINDOWS, self.delay_window);
        self.delay_window = f64::INFINITY;

        let delay = self
            .delay_history
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min);
        let error = delay - self.target();
        if !error.is_finite() || error.abs() <= DEADBAND_FRAMES {
            self.rate = 0.0;
        } else if error > SPLICE_EXCESS_FRAMES {
            self.pending_cut = (error - self.packet_frames as f64) as usize;
            self.rate = 0.0;
            self.acc = 0.0;
        } else {
            self.rate = (error / CORRECTION_FRAMES).clamp(-MAX_RATE, MAX_RATE);
        }
    }
}

fn roll(history: &mut VecDeque<f64>, cap: usize, value: f64) {
    if history.len() == cap {
        history.pop_front();
    }
    history.push_back(value);
}

fn copy_front(samples: &mut VecDeque<f32>, out: &mut [f32]) {
    let count = out.len();
    let (front, back) = samples.as_slices();
    let from_front = count.min(front.len());
    out[..from_front].copy_from_slice(&front[..from_front]);
    out[from_front..].copy_from_slice(&back[..count - from_front]);
    samples.drain(..count);
}

fn hold(out: &mut [f32], held: &mut [f32; CHANNELS]) {
    for frame in out.chunks_exact_mut(CHANNELS) {
        frame.copy_from_slice(held);
        for value in held.iter_mut() {
            *value = if value.abs() < 1e-6 {
                0.0
            } else {
                *value * HOLD_DECAY
            };
        }
    }
}

fn crossfade_from(held: &[f32; CHANNELS], out: &mut [f32], frames: usize) {
    let frames = frames.min(out.len() / CHANNELS);
    let mut anchor = *held;
    for (i, frame) in out.chunks_exact_mut(CHANNELS).take(frames).enumerate() {
        let t = (i + 1) as f32 / (frames + 1) as f32;
        for (value, a) in frame.iter_mut().zip(anchor.iter_mut()) {
            *value = *a + (*value - *a) * t;
            *a *= HOLD_DECAY;
        }
    }
}

fn fade_out_before(samples: &mut VecDeque<f32>, end: usize, frames: usize) {
    let frames = frames.min(end / CHANNELS);
    let start = end - frames * CHANNELS;
    for i in 0..frames {
        let gain = (frames - 1 - i) as f32 / frames as f32;
        for c in 0..CHANNELS {
            samples[start + i * CHANNELS + c] *= gain;
        }
    }
}

fn fade_in_from(samples: &mut VecDeque<f32>, start: usize, frames: usize) {
    let frames = frames.min((samples.len() - start) / CHANNELS);
    for i in 0..frames {
        let gain = (i + 1) as f32 / frames as f32;
        for c in 0..CHANNELS {
            samples[start + i * CHANNELS + c] *= gain;
        }
    }
}

/// Removes `cut` frames right after a crossfade from the front's continuation
/// into the audio past the cut. Returns the frames removed.
fn splice_front(samples: &mut VecDeque<f32>, cut: usize, fade: usize) -> usize {
    let frames = samples.len() / CHANNELS;
    let cut = cut.min(frames);
    let fade = fade.min(frames - cut);
    for i in 0..fade {
        let t = (i + 1) as f32 / (fade + 1) as f32;
        for c in 0..CHANNELS {
            let a = samples[i * CHANNELS + c];
            let b = samples[(i + cut) * CHANNELS + c];
            samples[i * CHANNELS + c] = a + (b - a) * t;
        }
    }
    samples.drain(fade * CHANNELS..(fade + cut) * CHANNELS);
    cut
}

fn frame_distance(input: &[f32], a: usize, b: usize) -> f32 {
    (0..CHANNELS)
        .map(|c| (input[a * CHANNELS + c] - input[b * CHANNELS + c]).abs())
        .sum()
}

/// Lowest-scoring index within `SEARCH_FRAMES` around the middle of `range`.
fn least(range: Range<usize>, score: impl Fn(usize) -> f32) -> Option<usize> {
    let mid = range.start + range.len() / 2;
    let lo = mid.saturating_sub(SEARCH_FRAMES / 2).max(range.start);
    (lo..(lo + SEARCH_FRAMES).min(range.end))
        .map(|i| (score(i), i))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, i)| i)
}

/// Writes `input` minus `k` frames, each dropped where its neighbours are closest.
fn drop_frames(input: &[f32], out: &mut [f32], k: usize) {
    let frames = input.len() / CHANNELS;
    let mut written = 0;
    for segment in 0..k {
        let (start, end) = (frames * segment / k, frames * (segment + 1) / k);
        let skip = least(start.max(1)..end.min(frames - 1), |j| {
            frame_distance(input, j - 1, j + 1)
        })
        .unwrap_or(start);
        for range in [start..skip, skip + 1..end] {
            let src = &input[range.start * CHANNELS..range.end * CHANNELS];
            out[written..written + src.len()].copy_from_slice(src);
            written += src.len();
        }
    }
    debug_assert_eq!(written, out.len());
}

/// Writes `input` plus `k` interpolated frames, each where the signal is flattest.
fn insert_frames(input: &[f32], out: &mut [f32], k: usize) {
    let frames = input.len() / CHANNELS;
    let mut written = 0;
    for segment in 0..k {
        let (start, end) = (frames * segment / k, frames * (segment + 1) / k);
        let at = least(start..end.min(frames - 1), |j| {
            frame_distance(input, j, j + 1)
        })
        .unwrap_or(end - 1);
        let head = &input[start * CHANNELS..(at + 1) * CHANNELS];
        out[written..written + head.len()].copy_from_slice(head);
        written += head.len();
        let next = (at + 1).min(frames - 1);
        for c in 0..CHANNELS {
            out[written + c] = 0.5 * (input[at * CHANNELS + c] + input[next * CHANNELS + c]);
        }
        written += CHANNELS;
        let tail = &input[(at + 1) * CHANNELS..end * CHANNELS];
        out[written..written + tail.len()].copy_from_slice(tail);
        written += tail.len();
    }
    debug_assert_eq!(written, out.len());
}

/// Linear 48 kHz -> device-rate converter for devices that reject 48 kHz.
struct Resampler {
    step: f64,
    pos: f64,
    a: [f32; CHANNELS],
    b: [f32; CHANNELS],
}

impl Resampler {
    fn new(output_rate: u32) -> Self {
        Self {
            step: SAMPLE_RATE as f64 / output_rate as f64,
            pos: 0.0,
            a: [0.0; CHANNELS],
            b: [0.0; CHANNELS],
        }
    }

    fn input_needed(&self, out_frames: usize) -> usize {
        let mut pos = self.pos;
        let mut pulls = 0;
        for _ in 0..out_frames {
            pos += self.step;
            while pos >= 1.0 {
                pos -= 1.0;
                pulls += 1;
            }
        }
        pulls
    }

    fn process(&mut self, input: &[f32], out: &mut [f32]) {
        let mut frames = input.chunks_exact(CHANNELS);
        for frame in out.chunks_exact_mut(CHANNELS) {
            let t = self.pos as f32;
            for (c, value) in frame.iter_mut().enumerate() {
                *value = self.a[c] + (self.b[c] - self.a[c]) * t;
            }
            self.pos += self.step;
            while self.pos >= 1.0 {
                self.pos -= 1.0;
                self.a = self.b;
                if let Some(next) = frames.next() {
                    self.b.copy_from_slice(next);
                }
            }
        }
        debug_assert_eq!(frames.len(), 0);
    }
}

fn write_device<T: cpal::Sample + cpal::FromSample<f32>>(
    stereo: &[f32],
    out: &mut [T],
    channels: usize,
) {
    for (src, dst) in stereo
        .chunks_exact(CHANNELS)
        .zip(out.chunks_exact_mut(channels))
    {
        if channels == 1 {
            dst[0] = T::from_sample(0.5 * (src[0] + src[1]));
            continue;
        }
        dst[0] = T::from_sample(src[0]);
        dst[1] = T::from_sample(src[1]);
        for value in &mut dst[2..] {
            *value = T::EQUILIBRIUM;
        }
    }
}

/// Flags late device callbacks (the device buffer running dry).
struct XrunWatch {
    limit: Option<Duration>,
    started: Option<Instant>,
    last: Option<Instant>,
    first_strike: Option<Instant>,
    strikes: u32,
}

impl XrunWatch {
    fn new(buffer_frames: Option<u32>, rate: u32) -> Self {
        Self {
            limit: buffer_frames.map(|frames| {
                Duration::from_secs_f64(1.5 * frames as f64 / rate as f64)
                    + Duration::from_millis(1)
            }),
            started: None,
            last: None,
            first_strike: None,
            strikes: 0,
        }
    }

    /// True once repeated late callbacks call for a larger device buffer.
    fn tick(&mut self, now: Instant) -> bool {
        let Some(limit) = self.limit else {
            return false;
        };
        let started = *self.started.get_or_insert(now);
        let late = self
            .last
            .replace(now)
            .is_some_and(|last| now.duration_since(last) > limit);
        if !late || now.duration_since(started) < XRUN_WARMUP {
            return false;
        }
        match self.first_strike {
            Some(first) if now.duration_since(first) <= XRUN_SPAN => self.strikes += 1,
            _ => {
                self.first_strike = Some(now);
                self.strikes = 1;
            }
        }
        self.strikes >= XRUN_STRIKES
    }
}

#[derive(Default)]
struct StreamHealth {
    failed: AtomicBool,
    escalate: AtomicBool,
}

#[derive(Clone, Copy)]
struct OutputFormat {
    channels: usize,
    rate: u32,
    sample: cpal::SampleFormat,
    buffer: cpal::BufferSize,
    rung: usize,
}

/// Device-callback side of the ring.
struct Renderer {
    ring: Arc<Mutex<PlaybackBuffer>>,
    health: Arc<StreamHealth>,
    xrun: XrunWatch,
    held: [f32; CHANNELS],
    channels: usize,
    resampler: Option<Resampler>,
    stereo: Vec<f32>,
    resampled: Vec<f32>,
    #[cfg(target_os = "linux")]
    promoted: bool,
}

impl Renderer {
    fn new(
        ring: Arc<Mutex<PlaybackBuffer>>,
        health: Arc<StreamHealth>,
        format: OutputFormat,
    ) -> Self {
        let buffer_frames = match format.buffer {
            cpal::BufferSize::Fixed(frames) => Some(frames),
            cpal::BufferSize::Default => None,
        };
        Self {
            ring,
            health,
            xrun: XrunWatch::new(buffer_frames, format.rate),
            held: [0.0; CHANNELS],
            channels: format.channels.max(1),
            resampler: (format.rate != SAMPLE_RATE).then(|| Resampler::new(format.rate)),
            stereo: Vec::with_capacity(8192),
            resampled: Vec::with_capacity(8192),
            #[cfg(target_os = "linux")]
            promoted: false,
        }
    }

    fn begin(&mut self) {
        // cpal's ALSA worker runs at normal priority; the small device buffer needs RT.
        #[cfg(target_os = "linux")]
        if !self.promoted {
            self.promoted = true;
            st_protocol::thread_priority::promote_current_thread(
                st_protocol::thread_priority::ThreadRole::Audio,
            );
        }
        if self.xrun.tick(Instant::now()) {
            self.health.escalate.store(true, Ordering::Relaxed);
        }
    }

    fn render_stereo(&mut self, out: &mut [f32]) {
        match lock_spin(&self.ring) {
            Some(mut ring) => ring.render(out, &mut self.held),
            None => hold(out, &mut self.held),
        }
    }

    fn fill_native(&mut self, out: &mut [f32]) {
        self.begin();
        self.render_stereo(out);
    }

    fn fill_adapted<T: cpal::Sample + cpal::FromSample<f32>>(&mut self, out: &mut [T]) {
        self.begin();
        let frames = out.len() / self.channels;
        let needed = self
            .resampler
            .as_ref()
            .map_or(frames, |r| r.input_needed(frames));
        let mut stereo = std::mem::take(&mut self.stereo);
        stereo.resize(needed * CHANNELS, 0.0);
        self.render_stereo(&mut stereo);
        let src = match self.resampler.as_mut() {
            Some(resampler) => {
                self.resampled.resize(frames * CHANNELS, 0.0);
                resampler.process(&stereo, &mut self.resampled);
                &self.resampled[..]
            }
            None => &stereo[..],
        };
        write_device(src, out, self.channels);
        self.stereo = stereo;
    }
}

fn lock(ring: &Mutex<PlaybackBuffer>) -> MutexGuard<'_, PlaybackBuffer> {
    ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The producer holds the lock for ~1 µs; spinning beats emitting a gap.
fn lock_spin(ring: &Mutex<PlaybackBuffer>) -> Option<MutexGuard<'_, PlaybackBuffer>> {
    for _ in 0..LOCK_SPINS {
        match ring.try_lock() {
            Ok(guard) => return Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => std::hint::spin_loop(),
        }
    }
    None
}

/// `(rung, size)` for ladder rungs from `step` up (scaled to `rate`, clamped to
/// the device), then the backend default as the last rung.
fn buffer_choices(
    step: usize,
    rate: u32,
    range: Option<(u32, u32)>,
) -> Vec<(usize, cpal::BufferSize)> {
    let mut choices: Vec<_> = DEVICE_BUFFER_LADDER
        .iter()
        .enumerate()
        .skip(step)
        .map(|(rung, &frames)| {
            let frames = (frames as u64 * rate as u64 / SAMPLE_RATE as u64) as u32;
            let frames = range.map_or(frames, |(min, max)| frames.clamp(min, max.max(min)));
            (rung, cpal::BufferSize::Fixed(frames))
        })
        .collect();
    // A clamped duplicate takes the higher rung so escalation always grows.
    choices.dedup_by(|later, kept| {
        let same = later.1 == kept.1;
        if same {
            kept.0 = later.0;
        }
        same
    });
    choices.push((DEVICE_BUFFER_LADDER.len(), cpal::BufferSize::Default));
    choices
}

fn buffer_range(size: &cpal::SupportedBufferSize) -> Option<(u32, u32)> {
    match size {
        cpal::SupportedBufferSize::Range { min, max } => Some((*min, *max)),
        cpal::SupportedBufferSize::Unknown => None,
    }
}

/// `Some(range)` when the device takes 48 kHz stereo f32 directly.
fn native_buffer_range(device: &cpal::Device) -> Option<Option<(u32, u32)>> {
    device
        .supported_output_configs()
        .ok()?
        .find(|config| {
            config.channels() as usize == CHANNELS
                && config.sample_format() == cpal::SampleFormat::F32
                && config.min_sample_rate().0 <= SAMPLE_RATE
                && SAMPLE_RATE <= config.max_sample_rate().0
        })
        .map(|config| buffer_range(config.buffer_size()))
}

fn build<T: cpal::SizedSample + 'static>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    health: &Arc<StreamHealth>,
    mut renderer: Renderer,
    fill: fn(&mut Renderer, &mut [T]),
) -> Result<cpal::Stream, String> {
    let errors = Arc::clone(health);
    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [T], _: &cpal::OutputCallbackInfo| fill(&mut renderer, out),
            move |err| {
                if !errors.failed.swap(true, Ordering::Relaxed) {
                    eprintln!("[audio] output stream error: {err}");
                }
            },
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

fn open_stream(
    device: &cpal::Device,
    ring: &Arc<Mutex<PlaybackBuffer>>,
    health: &Arc<StreamHealth>,
    format: OutputFormat,
) -> Result<cpal::Stream, String> {
    let config = cpal::StreamConfig {
        channels: format.channels as u16,
        sample_rate: cpal::SampleRate(format.rate),
        buffer_size: format.buffer,
    };
    let renderer = Renderer::new(Arc::clone(ring), Arc::clone(health), format);
    let native = format.channels == CHANNELS && format.rate == SAMPLE_RATE;
    match format.sample {
        cpal::SampleFormat::F32 if native => {
            build(device, &config, health, renderer, Renderer::fill_native)
        }
        cpal::SampleFormat::F32 => build(
            device,
            &config,
            health,
            renderer,
            Renderer::fill_adapted::<f32>,
        ),
        cpal::SampleFormat::I16 => build(
            device,
            &config,
            health,
            renderer,
            Renderer::fill_adapted::<i16>,
        ),
        cpal::SampleFormat::I32 => build(
            device,
            &config,
            health,
            renderer,
            Renderer::fill_adapted::<i32>,
        ),
        cpal::SampleFormat::U16 => build(
            device,
            &config,
            health,
            renderer,
            Renderer::fill_adapted::<u16>,
        ),
        other => Err(format!("unsupported sample format {other}")),
    }
}

struct ActiveOutput {
    stream: cpal::Stream,
    health: Arc<StreamHealth>,
    device: String,
    rung: usize,
    started: Instant,
}

/// Opens the default device at 48 kHz stereo when it accepts that, else in its
/// own format through the resampler/channel map.
fn build_output(ring: &Arc<Mutex<PlaybackBuffer>>, step: usize) -> Result<ActiveOutput, String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no audio output device")?;
    let name = device.name().unwrap_or_else(|_| "unknown device".into());
    let health = Arc::new(StreamHealth::default());
    let mut formats = Vec::new();
    if let Some(range) = native_buffer_range(&device) {
        formats.extend(buffer_choices(step, SAMPLE_RATE, range).into_iter().map(
            |(rung, buffer)| OutputFormat {
                channels: CHANNELS,
                rate: SAMPLE_RATE,
                sample: cpal::SampleFormat::F32,
                buffer,
                rung,
            },
        ));
    }
    match device.default_output_config() {
        Ok(default) => {
            let range = buffer_range(default.buffer_size());
            formats.extend(
                buffer_choices(step, default.sample_rate().0, range)
                    .into_iter()
                    .map(|(rung, buffer)| OutputFormat {
                        channels: default.channels() as usize,
                        rate: default.sample_rate().0,
                        sample: default.sample_format(),
                        buffer,
                        rung,
                    }),
            );
        }
        Err(e) if formats.is_empty() => return Err(format!("{name}: {e}")),
        Err(_) => {}
    }
    let mut last_error = String::from("no usable output format");
    for format in formats {
        match open_stream(&device, ring, &health, format) {
            Ok(stream) => {
                eprintln!(
                    "[audio] Playback on '{name}': {} Hz {}ch {}, buffer {:?}",
                    format.rate, format.channels, format.sample, format.buffer
                );
                return Ok(ActiveOutput {
                    stream,
                    health,
                    device: name,
                    rung: format.rung,
                    started: Instant::now(),
                });
            }
            Err(e) => last_error = e,
        }
    }
    Err(format!("{name}: {last_error}"))
}

fn default_device_name() -> Option<String> {
    cpal::default_host()
        .default_output_device()
        .and_then(|device| device.name().ok())
}

/// Keeps a cpal stream alive across device errors, xruns and default-device
/// changes. Never gives up; retries back off up to `RETRY_MAX`.
struct Output {
    ring: Arc<Mutex<PlaybackBuffer>>,
    active: Option<ActiveOutput>,
    ladder_step: usize,
    retry_at: Instant,
    backoff: Duration,
    next_device_check: Instant,
    last_error: Option<String>,
}

impl Output {
    fn new(ring: Arc<Mutex<PlaybackBuffer>>) -> Self {
        let now = Instant::now();
        Self {
            ring,
            active: None,
            ladder_step: 0,
            retry_at: now,
            backoff: RETRY_INITIAL,
            next_device_check: now + DEVICE_CHECK_INTERVAL,
            last_error: None,
        }
    }

    fn is_active(&self) -> bool {
        self.active.is_some()
    }

    fn stop(&mut self) {
        if let Some(active) = self.active.take() {
            let _ = active.stream.pause();
            eprintln!("[audio] Playback paused");
        }
        lock(&self.ring).reset();
    }

    fn ensure(&mut self, now: Instant) {
        if self.active.is_some() || now < self.retry_at {
            return;
        }
        lock(&self.ring).reset();
        match build_output(&self.ring, self.ladder_step) {
            Ok(active) => {
                self.last_error = None;
                self.next_device_check = now + DEVICE_CHECK_INTERVAL;
                self.active = Some(active);
            }
            Err(e) => {
                if self.last_error.as_deref() != Some(e.as_str()) {
                    eprintln!("[audio] output unavailable ({e}); retrying");
                    self.last_error = Some(e);
                }
                self.retry_at = now + self.backoff;
                self.backoff = (self.backoff * 2).min(RETRY_MAX);
            }
        }
    }

    fn maintain(&mut self, now: Instant) {
        let Some(active) = &self.active else {
            return;
        };
        let failed = active.health.failed.load(Ordering::Relaxed);
        let healthy = now.duration_since(active.started) >= HEALTHY_RUN;
        let reason = if failed {
            "stream error"
        } else if active.health.escalate.load(Ordering::Relaxed)
            && active.rung < DEVICE_BUFFER_LADDER.len()
        {
            self.ladder_step = active.rung + 1;
            "device buffer underruns"
        } else if now >= self.next_device_check {
            self.next_device_check = now + DEVICE_CHECK_INTERVAL;
            match default_device_name() {
                Some(name) if name != active.device => {
                    self.ladder_step = 0;
                    "default device changed"
                }
                _ => return,
            }
        } else {
            return;
        };
        eprintln!("[audio] Rebuilding output: {reason}");
        self.stop();
        if failed && !healthy {
            self.retry_at = now + self.backoff;
            self.backoff = (self.backoff * 2).min(RETRY_MAX);
        } else {
            self.retry_at = now;
            self.backoff = RETRY_INITIAL;
        }
    }
}

/// E1: derive per-packet audio timing from the negotiated Opus frame duration.
/// `packet_duration_ms == 0` (server declared none) falls back to the default.
/// Returns `(effective_ms, max_concealed_packets, packet_samples)`.
fn audio_timing(packet_duration_ms: u32) -> (usize, usize, usize) {
    let ms = if packet_duration_ms == 0 {
        DEFAULT_AUDIO_PACKET_DURATION_MS
    } else {
        packet_duration_ms as usize
    };
    let max_concealed_packets = (MAX_CONCEALED_AUDIO_MS / ms.max(1)).max(1);
    let packet_samples = (SAMPLE_RATE as usize * CHANNELS * ms) / 1000;
    (ms, max_concealed_packets, packet_samples)
}

/// CELT-only packets (TOC configs 16..=31) carry no in-band FEC.
fn carries_lbrr(packet: &[u8]) -> bool {
    packet.first().is_some_and(|toc| toc >> 3 < 16)
}

/// Conceals one packet slot: FEC from the following packet when given, else PLC.
/// `pcm` must be exactly one packet long; libopus conceals the whole buffer.
fn conceal(
    decoder: &mut opus::Decoder,
    fec_source: Option<&[u8]>,
    pcm: &mut [f32],
) -> Result<usize, opus::Error> {
    decoder.decode_float(fec_source.unwrap_or(&[]), pcm, fec_source.is_some())
}

/// Fills `missing` slots before `packet` (redundancy, then FEC, then PLC).
/// Returns how many slots each path recovered.
fn recover_gap(
    decoder: &mut opus::Decoder,
    packet: &AudioPacket,
    missing: usize,
    pcm_buf: &mut [f32],
    packet_samples: usize,
    mut push: impl FnMut(&[f32]),
) -> [usize; 3] {
    let mut via = [0usize; 3];
    let redundancy_count = packet.redundant_prev.len();
    let fec_source = carries_lbrr(&packet.data).then_some(packet.data.as_slice());
    for distance in (1..=missing).rev() {
        let redundant = redundancy_count.checked_sub(distance).and_then(|idx| {
            decoder
                .decode_float(&packet.redundant_prev[idx], pcm_buf, false)
                .ok()
        });
        let frames = if let Some(frames) = redundant {
            via[0] += 1;
            Some(frames)
        } else if let Some(frames) = fec_source
            .filter(|_| distance == 1)
            .and_then(|next| conceal(decoder, Some(next), &mut pcm_buf[..packet_samples]).ok())
        {
            via[1] += 1;
            Some(frames)
        } else {
            let frames = conceal(decoder, None, &mut pcm_buf[..packet_samples]).ok();
            via[2] += usize::from(frames.is_some());
            frames
        };
        if let Some(frames) = frames {
            push(&pcm_buf[..frames * CHANNELS]);
        }
    }
    via
}

pub fn run_audio_pipeline(
    opus_rx: Receiver<AudioPacket>,
    shutdown_rx: Receiver<()>,
    packet_duration_ms: u32,
    audio_enabled: Arc<AtomicBool>,
) -> Result<(), String> {
    let (_packet_duration_ms, max_concealed_packets, packet_samples) =
        audio_timing(packet_duration_ms);

    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
        .map_err(|e| format!("Opus decoder: {e}"))?;
    let mut pcm_buf = vec![0.0f32; MAX_OPUS_FRAME_SAMPLES * CHANNELS];
    let ring = Arc::new(Mutex::new(PlaybackBuffer::new(packet_samples / CHANNELS)));
    let mut output = Output::new(Arc::clone(&ring));
    let mut expected_seq = None::<u16>;
    let mut was_enabled = audio_enabled.load(Ordering::Relaxed);
    let trace = std::env::var_os("ST_TRACE").is_some();
    let mut concealment_logs = 0usize;

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        let enabled = audio_enabled.load(Ordering::Relaxed);
        if !enabled && was_enabled {
            output.stop();
            expected_seq = None;
            let _ = decoder.reset_state();
            while opus_rx.try_recv().is_ok() {}
        }
        was_enabled = enabled;
        output.maintain(Instant::now());

        let packet = match opus_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(d) => d,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        if !audio_enabled.load(Ordering::Relaxed) {
            continue;
        }
        output.ensure(Instant::now());
        let active = output.is_active();
        let enqueue = |pcm: &[f32], discontinuity: bool| {
            if active {
                lock(&ring).push(pcm, discontinuity);
            }
        };

        let mut discontinuity = false;
        if let Some(expected) = expected_seq {
            let delta = packet.seq.wrapping_sub(expected);
            if delta >= 0x8000 {
                continue;
            }
            let missing = delta as usize;
            if missing > max_concealed_packets {
                discontinuity = true;
                if trace && concealment_logs < 12 {
                    eprintln!(
                        "[trace][audio] large audio gap ({missing} packets), resyncing at seq={}",
                        packet.seq
                    );
                    concealment_logs += 1;
                }
            } else if missing > 0 {
                let [redundancy, fec, plc] = recover_gap(
                    &mut decoder,
                    &packet,
                    missing,
                    &mut pcm_buf,
                    packet_samples,
                    |pcm| enqueue(pcm, false),
                );
                if trace && concealment_logs < 12 {
                    eprintln!(
                        "[trace][audio] recovered {} missing packet(s) before seq={} via redundancy={redundancy} fec={fec} plc={plc}",
                        redundancy + fec + plc,
                        packet.seq,
                    );
                    concealment_logs += 1;
                }
            }
        }

        match decoder.decode_float(&packet.data, &mut pcm_buf, false) {
            Ok(frames) => {
                enqueue(&pcm_buf[..frames * CHANNELS], discontinuity);
                expected_seq = Some(packet.seq.wrapping_add(1));
            }
            Err(e) => eprintln!("[audio] decode error: {e}"),
        }
    }

    output.stop();
    eprintln!("[audio] Pipeline stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn sine_frames(frames: usize, hz: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = 0.5 * (i as f32 * hz * std::f32::consts::TAU / SAMPLE_RATE as f32).sin();
                [v, -v]
            })
            .collect()
    }

    fn max_step(samples: &[f32]) -> f32 {
        samples
            .windows(CHANNELS + 1)
            .map(|w| (w[CHANNELS] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn audio_timing_derives_from_wire_frame_duration() {
        assert_eq!(audio_timing(20), (20, 3, 1920));
        assert_eq!(audio_timing(5), (5, 12, 480));
        assert_eq!(audio_timing(10), (10, 6, 960));
        assert_eq!(
            audio_timing(0),
            (
                DEFAULT_AUDIO_PACKET_DURATION_MS,
                MAX_CONCEALED_AUDIO_MS / DEFAULT_AUDIO_PACKET_DURATION_MS,
                SAMPLE_RATE as usize * CHANNELS * DEFAULT_AUDIO_PACKET_DURATION_MS / 1000
            )
        );
        for ms in [5u32, 10, 20] {
            let (eff, max_concealed, _) = audio_timing(ms);
            assert_eq!(max_concealed * eff, MAX_CONCEALED_AUDIO_MS);
        }
    }

    #[test]
    fn concealment_covers_exactly_one_packet() {
        let (_, _, packet_samples) = audio_timing(5);
        let mut encoder = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::LowDelay,
        )
        .unwrap();
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).unwrap();
        let mut packet = [0u8; 1500];
        let len = encoder
            .encode_float(&sine_frames(packet_samples / CHANNELS, 440.0), &mut packet)
            .unwrap();
        let packet = AudioPacket {
            seq: 3,
            data: packet[..len].to_vec(),
            redundant_prev: Vec::new(),
        };
        assert!(!carries_lbrr(&packet.data));
        let mut pcm = vec![0.0; MAX_OPUS_FRAME_SAMPLES * CHANNELS];
        assert_eq!(
            decoder.decode_float(&packet.data, &mut pcm, false).unwrap(),
            240
        );

        let mut pushed = Vec::new();
        let via = recover_gap(&mut decoder, &packet, 3, &mut pcm, packet_samples, |p| {
            pushed.push(p.len())
        });
        assert_eq!(via, [0, 0, 3]);
        assert_eq!(pushed, vec![packet_samples; 3]);
        // The full buffer is what turned each lost 5 ms packet into 120 ms of PLC.
        assert_eq!(
            conceal(&mut decoder, None, &mut pcm).unwrap(),
            MAX_OPUS_FRAME_SAMPLES
        );
    }

    #[test]
    fn lbrr_gate_follows_toc_mode() {
        for config in 0u8..32 {
            assert_eq!(carries_lbrr(&[config << 3]), config < 16);
        }
        assert!(!carries_lbrr(&[]));
    }

    #[test]
    fn splice_crossfades_across_the_cut() {
        let mut samples: VecDeque<f32> = sine_frames(4800, 440.0).into();
        let before = samples.len();
        assert_eq!(splice_front(&mut samples, 1003, SPLICE_FADE_FRAMES), 1003);
        assert_eq!(samples.len(), before - 1003 * CHANNELS);
        let spliced: Vec<f32> = samples.into();
        assert!(max_step(&spliced) < 0.05, "step {}", max_step(&spliced));
    }

    #[test]
    fn drop_and_insert_keep_the_waveform_continuous() {
        let mut out = vec![0.0; 480 * CHANNELS];
        drop_frames(&sine_frames(482, 440.0), &mut out, 2);
        assert!(max_step(&out) < 0.05, "drop step {}", max_step(&out));
        insert_frames(&sine_frames(478, 440.0), &mut out, 2);
        assert!(max_step(&out) < 0.05, "insert step {}", max_step(&out));
    }

    #[test]
    fn underrun_fades_then_resumes_at_the_raised_target() {
        let n = 240;
        let p = ms_frames(5);
        let mut ring = PlaybackBuffer::new(p);
        let mut held = [0.0; CHANNELS];
        let mut out = vec![0.0; n * CHANNELS];
        let pcm = vec![0.5; p * CHANNELS];
        let mut t = 0.0;
        for _ in 0..40 {
            ring.push_at(&pcm, false, t);
            ring.render_at(&mut out, &mut held, t + 1.0);
            t += p as f64;
        }
        assert!(ring.primed && ring.underruns == 0);

        for _ in 0..4 {
            ring.render_at(&mut out, &mut held, t);
            t += p as f64;
        }
        assert_eq!(ring.underruns, 1);
        assert!(!ring.primed);
        assert!(out[out.len() - 1].abs() < 0.1);

        for _ in 0..4 {
            ring.push_at(&pcm, false, t);
        }
        assert!(ring.target() >= ms_frames(20) as f64);
        for _ in 0..2 {
            ring.render_at(&mut out, &mut held, t);
            t += p as f64;
        }
        assert!(ring.primed);
        assert!(ring.delay(t) >= ring.target());
    }

    struct SimResult {
        underruns: u64,
        max_fill_ms: f64,
    }

    /// Packets every 5 ms (server clock skewed by `skew_ppm`) with uniform
    /// jitter and optional periodic stalls `(period_s, stall_ms)`, the first at
    /// half a period (inside the warm-up); the device pulls `callback` frames on
    /// the client clock.
    fn simulate(
        skew_ppm: f64,
        callback: usize,
        jitter_ms: f64,
        stall: Option<(f64, f64)>,
        seconds: f64,
    ) -> SimResult {
        const WARMUP_S: f64 = 15.0;
        let packet = ms_frames(5);
        let rate = SAMPLE_RATE as f64;
        let mut ring = PlaybackBuffer::new(packet);
        let mut held = [0.0; CHANNELS];
        let mut out = vec![0.0; callback * CHANNELS];
        let pcm = sine_frames(packet, 440.0);
        let interval = 0.005 * (1.0 + skew_ppm * 1e-6);
        let callback_s = callback as f64 / rate;
        let mut rng = StdRng::seed_from_u64(7);
        let mut last_arrival = 0.0f64;
        let mut arrival = |i: u64, rng: &mut StdRng| {
            let sent = i as f64 * interval;
            let mut at = sent + 0.001 + rng.gen_range(0.0..=jitter_ms) / 1000.0;
            if let Some((period, stall_ms)) = stall {
                let phase = (sent + period / 2.0) % period;
                if phase < stall_ms / 1000.0 {
                    at = at.max(sent - phase + stall_ms / 1000.0);
                }
            }
            last_arrival = last_arrival.max(at);
            last_arrival
        };
        let mut seq = 0u64;
        let mut next_packet = arrival(seq, &mut rng);
        let mut next_callback = 0.0;
        let mut result = SimResult {
            underruns: 0,
            max_fill_ms: 0.0,
        };
        let mut underruns_at_warmup = None;
        while next_callback < seconds {
            if next_packet <= next_callback {
                ring.push_at(&pcm, false, next_packet * rate);
                seq += 1;
                next_packet = arrival(seq, &mut rng);
                continue;
            }
            ring.render_at(&mut out, &mut held, next_callback * rate);
            if next_callback >= WARMUP_S {
                let base = *underruns_at_warmup.get_or_insert(ring.underruns);
                result.underruns = ring.underruns - base;
                if ring.primed {
                    let fill = ring.frames() as f64 * 1000.0 / rate;
                    result.max_fill_ms = result.max_fill_ms.max(fill);
                }
            }
            next_callback += callback_s;
        }
        result
    }

    #[test]
    fn playout_absorbs_jitter_stalls_and_clock_skew() {
        // (skew ppm, callback frames, jitter ms, stall, max post-read fill ms)
        let cases = [
            (100.0, 240, 2.0, None, 26.0),
            (-100.0, 240, 2.0, None, 26.0),
            (100.0, 480, 8.0, None, 32.0),
            (-100.0, 1024, 5.0, None, 40.0),
            (100.0, 1200, 3.0, None, 40.0),
            (0.0, 480, 20.0, None, 42.0),
            (100.0, 480, 2.0, Some((3.0, 30.0)), 52.0),
            (-100.0, 480, 2.0, Some((20.0, 50.0)), 72.0),
            (-100.0, 480, 1.0, Some((0.02, 15.0)), 40.0),
        ];
        for (skew, callback, jitter, stall, max_fill) in cases {
            // 600 s at 100 ppm is 60 ms of drift the steering must absorb.
            let r = simulate(skew, callback, jitter, stall, 600.0);
            let label = format!("skew={skew} cb={callback} jitter={jitter} stall={stall:?}");
            assert_eq!(r.underruns, 0, "{label}: underruns");
            assert!(
                r.max_fill_ms <= max_fill,
                "{label}: max fill {}",
                r.max_fill_ms
            );
        }
    }

    #[test]
    fn excess_from_a_device_stall_drains_back() {
        let n = 480;
        let p = ms_frames(5);
        let mut ring = PlaybackBuffer::new(p);
        let mut held = [0.0; CHANNELS];
        let mut out = vec![0.0; n * CHANNELS];
        let pcm = sine_frames(p, 440.0);
        let rate = SAMPLE_RATE as f64;
        let mut t = 0.0;
        for i in 0..2400 {
            ring.push_at(&pcm, false, t);
            let device_stalled = (2.0..2.15).contains(&(t / rate));
            if i % 2 == 1 && !device_stalled {
                ring.render_at(&mut out, &mut held, t);
            }
            t += p as f64;
        }
        assert_eq!(ring.underruns, 0);
        let excess_ms = (ring.delay(t) - ring.target()) * 1000.0 / rate;
        assert!(excess_ms.abs() < 6.0, "excess {excess_ms} ms");
    }

    #[test]
    fn xrun_watch_escalates_only_on_repeated_late_callbacks() {
        let mut watch = XrunWatch::new(Some(480), SAMPLE_RATE);
        let mut t = Instant::now();
        for _ in 0..200 {
            t += Duration::from_millis(10);
            assert!(!watch.tick(t));
        }
        for strike in 1..=XRUN_STRIKES {
            t += Duration::from_millis(30);
            assert_eq!(watch.tick(t), strike == XRUN_STRIKES);
        }
        let mut unbounded = XrunWatch::new(None, SAMPLE_RATE);
        assert!(!unbounded.tick(t) && !unbounded.tick(t + Duration::from_secs(1)));
    }

    #[test]
    fn buffer_choices_clamp_to_the_device_and_fall_back() {
        use cpal::BufferSize::{Default, Fixed};
        assert_eq!(
            buffer_choices(0, 48000, Some((64, 4096))),
            [
                (0, Fixed(480)),
                (1, Fixed(960)),
                (2, Fixed(1920)),
                (3, Default)
            ]
        );
        assert_eq!(
            buffer_choices(1, 44100, None),
            [(1, Fixed(882)), (2, Fixed(1764)), (3, Default)]
        );
        assert_eq!(
            buffer_choices(0, 48000, Some((1024, 4096))),
            [(1, Fixed(1024)), (2, Fixed(1920)), (3, Default)]
        );
        assert_eq!(
            buffer_choices(DEVICE_BUFFER_LADDER.len(), 48000, None),
            [(3, Default)]
        );
    }

    #[test]
    fn resampler_consumes_exactly_what_it_requests() {
        let mut resampler = Resampler::new(44100);
        let (mut consumed, mut produced) = (0usize, 0usize);
        let mut out = Vec::new();
        for frames in [441usize, 512, 1, 999, 256].iter().cycle().take(200) {
            let needed = resampler.input_needed(*frames);
            out.resize(frames * CHANNELS, 0.0);
            resampler.process(&vec![0.5; needed * CHANNELS], &mut out);
            consumed += needed;
            produced += frames;
        }
        assert!((consumed as f64 / produced as f64 - 48000.0 / 44100.0).abs() < 1e-3);
        assert!(out.iter().all(|&v| (v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn device_channel_map_places_stereo_front_and_silences_the_rest() {
        let stereo = [0.5f32, -0.5, 0.25, 0.75];
        let mut six = [1i16; 12];
        write_device(&stereo, &mut six, 6);
        assert_eq!(six[..6], [16384, -16384, 0, 0, 0, 0]);
        let mut mono = [0.0f32; 2];
        write_device(&stereo, &mut mono, 1);
        assert_eq!(mono, [0.0, 0.5]);
    }
}
