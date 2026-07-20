use st_protocol::frame_assembler::AssemblyFeedback;
use st_protocol::packet::{parse_audio_packet, PacketHeader, PayloadType, HEADER_SIZE};
use st_protocol::{CompletedFrame, FrameAssembler, TransportFeedback};
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FEEDBACK_INTERVAL: Duration = Duration::from_millis(500);
const URGENT_FEEDBACK_DEBOUNCE: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub seq: u16,
    pub data: Vec<u8>,
    pub redundant_prev: Vec<Vec<u8>>,
}

pub enum ReceivedData {
    Video(CompletedFrame, u64, u64),
    Audio(AudioPacket),
    Keepalive,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TransportWindowStats {
    pub interval_ms: u32,
    pub received_packets: u32,
    pub lost_packets: u32,
    pub late_packets: u32,
    pub completed_frames: u32,
    pub dropped_frames: u32,
    pub received_bytes: u64,
    pub received_video_bytes: u64,
    pub received_audio_bytes: u64,
    pub owd_trend_us: i32,
}

impl TransportWindowStats {
    pub fn feedback(self) -> TransportFeedback {
        let recv_video_kbps = if self.interval_ms > 0 {
            (self.received_video_bytes.saturating_mul(8) / self.interval_ms as u64)
                .min(u32::MAX as u64) as u32
        } else {
            0
        };
        TransportFeedback {
            interval_ms: self.interval_ms,
            received_packets: self.received_packets,
            lost_packets: self.lost_packets,
            late_packets: self.late_packets,
            completed_frames: self.completed_frames,
            dropped_frames: self.dropped_frames,
            recv_video_kbps,
            owd_trend_us: self.owd_trend_us,
            ..Default::default()
        }
    }

    pub fn needs_recovery_keyframe(self) -> bool {
        self.dropped_frames > 0
    }
}

pub struct MediaDemux {
    assembler: FrameAssembler,
    feedback: FeedbackWindow,
    trace_packets_logged: usize,
}

impl Default for MediaDemux {
    fn default() -> Self {
        Self {
            assembler: FrameAssembler::new(),
            feedback: FeedbackWindow::default(),
            trace_packets_logged: 0,
        }
    }
}

impl MediaDemux {
    pub fn process_packet(
        &mut self,
        raw: &[u8],
        from_addr: Option<SocketAddr>,
    ) -> Option<ReceivedData> {
        let raw_len = raw.len();

        if let Some(header) = PacketHeader::deserialize(raw) {
            if std::env::var_os("ST_TRACE").is_some() && self.trace_packets_logged < 24 {
                match from_addr {
                    Some(addr) => eprintln!(
                        "[trace][client] udp packet #{} from {addr}: type={:?} frame_id={} seq={} bytes={raw_len}",
                        self.trace_packets_logged, header.payload_type, header.frame_id, header.seq
                    ),
                    None => eprintln!(
                        "[trace][client] bridged media packet #{}: type={:?} frame_id={} seq={} bytes={raw_len}",
                        self.trace_packets_logged, header.payload_type, header.frame_id, header.seq
                    ),
                }
                self.trace_packets_logged += 1;
            }
            if header.payload_type == PayloadType::Audio {
                if raw_len <= HEADER_SIZE {
                    return None;
                }
                let payload = &raw[HEADER_SIZE..];
                let view = parse_audio_packet(payload)?;
                let redundant_prev = view.redundant.iter().map(|chunk| chunk.to_vec()).collect();
                self.feedback.received_bytes =
                    self.feedback.received_bytes.saturating_add(raw_len as u64);
                self.feedback.received_audio_bytes = self
                    .feedback
                    .received_audio_bytes
                    .saturating_add(raw_len as u64);
                return Some(ReceivedData::Audio(AudioPacket {
                    seq: header.seq,
                    data: view.primary.to_vec(),
                    redundant_prev,
                }));
            }
            if header.payload_type == PayloadType::Keepalive {
                return Some(ReceivedData::Keepalive);
            }
        }

        self.feedback.received_packets = self.feedback.received_packets.saturating_add(1);
        self.feedback.received_bytes = self.feedback.received_bytes.saturating_add(raw_len as u64);
        self.feedback.received_video_bytes = self
            .feedback
            .received_video_bytes
            .saturating_add(raw_len as u64);
        let outcome = self.assembler.ingest_with_feedback(raw);
        self.feedback.record_assembly(outcome.feedback);
        if let Some(frame) = outcome.completed {
            self.feedback.completed_frames = self.feedback.completed_frames.saturating_add(1);
            let assembled_wall = unix_time_micros();
            let assembled_mono = mono_micros();
            self.feedback
                .record_owd(assembled_wall, frame.timing.send_ts_micros);
            return Some(ReceivedData::Video(frame, assembled_wall, assembled_mono));
        }
        None
    }

    pub fn take_stats(&mut self) -> Option<TransportWindowStats> {
        self.feedback.record_assembly(self.assembler.maintenance());
        self.feedback.take_if_due()
    }

    pub fn record_consumer_queue_overflow(&mut self) {
        self.feedback.dropped_frames = self.feedback.dropped_frames.saturating_add(1);
        self.feedback.urgent = true;
    }
}

pub fn unix_time_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u64::MAX as u128) as u64
}

pub fn mono_micros() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH
        .get_or_init(Instant::now)
        .elapsed()
        .as_micros()
        .min(u64::MAX as u128) as u64
}

#[derive(Debug)]
struct FeedbackWindow {
    started_at: Instant,
    urgent: bool,
    last_urgent_flush: Option<Instant>,
    received_packets: u32,
    lost_packets: u32,
    late_packets: u32,
    completed_frames: u32,
    dropped_frames: u32,
    received_bytes: u64,
    received_video_bytes: u64,
    received_audio_bytes: u64,
    min_owd_micros: Option<i64>,
    prev_min_owd_micros: Option<i64>,
}

impl Default for FeedbackWindow {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            urgent: false,
            last_urgent_flush: None,
            received_packets: 0,
            lost_packets: 0,
            late_packets: 0,
            completed_frames: 0,
            dropped_frames: 0,
            received_bytes: 0,
            received_video_bytes: 0,
            received_audio_bytes: 0,
            min_owd_micros: None,
            prev_min_owd_micros: None,
        }
    }
}

impl FeedbackWindow {
    fn record_assembly(&mut self, feedback: AssemblyFeedback) {
        self.lost_packets = self.lost_packets.saturating_add(feedback.lost_packets);
        self.late_packets = self.late_packets.saturating_add(feedback.late_packets);
        self.dropped_frames = self.dropped_frames.saturating_add(feedback.dropped_frames);
        if feedback.lost_packets > 0 || feedback.dropped_frames > 0 {
            self.urgent = true;
        }
    }

    fn record_owd(&mut self, arrival_wall_micros: u64, send_ts_micros: u64) {
        if send_ts_micros == 0 {
            return;
        }
        let owd = arrival_wall_micros as i64 - send_ts_micros as i64;
        self.min_owd_micros = Some(match self.min_owd_micros {
            Some(current) => current.min(owd),
            None => owd,
        });
    }

    fn take_if_due(&mut self) -> Option<TransportWindowStats> {
        let elapsed = self.started_at.elapsed();
        let urgent_due = self.urgent
            && self
                .last_urgent_flush
                .map(|last| last.elapsed() >= URGENT_FEEDBACK_DEBOUNCE)
                .unwrap_or(true);
        if elapsed < FEEDBACK_INTERVAL && !urgent_due {
            return None;
        }
        if self.urgent {
            self.last_urgent_flush = Some(Instant::now());
        }

        let owd_trend_us = match (self.min_owd_micros, self.prev_min_owd_micros) {
            (Some(current), Some(previous)) => {
                (current - previous).clamp(i32::MIN as i64, i32::MAX as i64) as i32
            }
            _ => 0,
        };
        let stats = TransportWindowStats {
            interval_ms: elapsed.as_millis().min(u32::MAX as u128) as u32,
            received_packets: self.received_packets,
            lost_packets: self.lost_packets,
            late_packets: self.late_packets,
            completed_frames: self.completed_frames,
            dropped_frames: self.dropped_frames,
            received_bytes: self.received_bytes,
            received_video_bytes: self.received_video_bytes,
            received_audio_bytes: self.received_audio_bytes,
            owd_trend_us,
        };

        if self.min_owd_micros.is_some() {
            self.prev_min_owd_micros = self.min_owd_micros;
        }
        self.started_at = Instant::now();
        self.urgent = false;
        self.received_packets = 0;
        self.lost_packets = 0;
        self.late_packets = 0;
        self.completed_frames = 0;
        self.dropped_frames = 0;
        self.received_bytes = 0;
        self.received_video_bytes = 0;
        self.received_audio_bytes = 0;
        self.min_owd_micros = None;
        Some(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use st_protocol::packet::MAX_UDP;
    use st_protocol::tcp_tunnel::TCP_TUNNEL_MAX_MEDIA;
    use st_protocol::FrameSlicer;

    #[test]
    fn tcp_sized_slicer_packets_roundtrip_through_demux() {
        let mut slicer = FrameSlicer::with_max_udp(TCP_TUNNEL_MAX_MEDIA);
        slicer.set_parity_enabled(false);
        let original = vec![0xA6; TCP_TUNNEL_MAX_MEDIA * 2];
        let packets = slicer.slice(&original, 77).to_vec();
        assert!(packets.iter().any(|packet| packet.len() > MAX_UDP));

        let mut demux = MediaDemux::default();
        let mut completed = None;
        for packet in packets {
            if let Some(ReceivedData::Video(frame, _, _)) = demux.process_packet(&packet, None) {
                completed = Some(frame);
            }
        }
        let frame = completed.expect("TCP-sized frame should complete");
        assert_eq!(frame.frame_id, 77);
        assert_eq!(frame.data, original);
    }

    #[test]
    fn take_stats_expires_idle_incomplete_frame() {
        let mut slicer = FrameSlicer::new();
        let packets = slicer.slice(&vec![0x5C; 3_000], 88).to_vec();
        assert!(packets.len() > 1);

        let mut demux = MediaDemux::default();
        assert!(demux.process_packet(&packets[0], None).is_none());
        std::thread::sleep(Duration::from_millis(2_050));

        let stats = demux.take_stats().expect("expiry should flush feedback");
        assert_eq!(stats.dropped_frames, 1);
        assert!(stats.lost_packets >= 1);
        assert!(stats.needs_recovery_keyframe());
    }
}
