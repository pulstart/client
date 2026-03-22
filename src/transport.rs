use st_protocol::packet::{PacketHeader, PayloadType, HEADER_SIZE};
use st_protocol::{CompletedFrame, FrameAssembler, TransportFeedback};
use std::io::ErrorKind;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

const FEEDBACK_INTERVAL: Duration = Duration::from_millis(500);

/// Demuxed data from the unified UDP stream.
pub enum ReceivedData {
    /// A fully assembled video frame (one or more packets reassembled).
    Video(CompletedFrame),
    /// A single audio packet (raw Opus data).
    Audio(Vec<u8>),
}

pub struct UdpReceiver {
    socket: UdpSocket,
    assembler: FrameAssembler,
    buf: Vec<u8>,
    feedback: FeedbackWindow,
    trace_packets_logged: usize,
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
}

impl TransportWindowStats {
    pub fn feedback(self) -> TransportFeedback {
        TransportFeedback {
            interval_ms: self.interval_ms,
            received_packets: self.received_packets,
            lost_packets: self.lost_packets,
            late_packets: self.late_packets,
            completed_frames: self.completed_frames,
            dropped_frames: self.dropped_frames,
        }
    }
}

impl UdpReceiver {
    pub fn from_socket(socket: UdpSocket) -> Result<Self, String> {
        socket
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking: {e}"))?;
        Ok(Self {
            socket,
            assembler: FrameAssembler::new(),
            buf: vec![0u8; 1500],
            feedback: FeedbackWindow::default(),
            trace_packets_logged: 0,
        })
    }

    /// Receive the next immediately-available piece of data.
    /// Returns `None` when the socket has no queued packets yet.
    pub fn try_receive(&mut self) -> Option<ReceivedData> {
        loop {
            match self.socket.recv_from(&mut self.buf) {
                Ok((n, addr)) => {
                    let raw = &self.buf[..n];

                    // Demux by payload type
                    if let Some(header) = PacketHeader::deserialize(raw) {
                        if std::env::var_os("ST_TRACE").is_some() && self.trace_packets_logged < 24 {
                            eprintln!(
                                "[trace][client] udp packet #{} from {addr}: type={:?} frame_id={} seq={} bytes={n}",
                                self.trace_packets_logged,
                                header.payload_type,
                                header.frame_id,
                                header.seq
                            );
                            self.trace_packets_logged += 1;
                        }
                        if header.payload_type == PayloadType::Audio {
                            if n > HEADER_SIZE {
                                self.feedback.received_bytes =
                                    self.feedback.received_bytes.saturating_add(n as u64);
                                self.feedback.received_audio_bytes =
                                    self.feedback.received_audio_bytes.saturating_add(n as u64);
                                return Some(ReceivedData::Audio(raw[HEADER_SIZE..].to_vec()));
                            }
                            continue;
                        }
                    }

                    // Video packet — pass to frame assembler
                    self.feedback.received_packets =
                        self.feedback.received_packets.saturating_add(1);
                    self.feedback.received_bytes =
                        self.feedback.received_bytes.saturating_add(n as u64);
                    self.feedback.received_video_bytes =
                        self.feedback.received_video_bytes.saturating_add(n as u64);
                    let outcome = self.assembler.ingest_with_feedback(raw);
                    self.feedback.lost_packets = self
                        .feedback
                        .lost_packets
                        .saturating_add(outcome.feedback.lost_packets);
                    self.feedback.late_packets = self
                        .feedback
                        .late_packets
                        .saturating_add(outcome.feedback.late_packets);
                    self.feedback.dropped_frames = self
                        .feedback
                        .dropped_frames
                        .saturating_add(outcome.feedback.dropped_frames);
                    if let Some(frame) = outcome.completed {
                        self.feedback.completed_frames =
                            self.feedback.completed_frames.saturating_add(1);
                        return Some(ReceivedData::Video(frame));
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => return None,
                Err(_) => return None,
            }
        }
    }

    pub fn take_stats(&mut self) -> Option<TransportWindowStats> {
        self.feedback.take_if_due()
    }
}

#[derive(Debug)]
struct FeedbackWindow {
    started_at: Instant,
    received_packets: u32,
    lost_packets: u32,
    late_packets: u32,
    completed_frames: u32,
    dropped_frames: u32,
    received_bytes: u64,
    received_video_bytes: u64,
    received_audio_bytes: u64,
}

impl Default for FeedbackWindow {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            received_packets: 0,
            lost_packets: 0,
            late_packets: 0,
            completed_frames: 0,
            dropped_frames: 0,
            received_bytes: 0,
            received_video_bytes: 0,
            received_audio_bytes: 0,
        }
    }
}

impl FeedbackWindow {
    fn take_if_due(&mut self) -> Option<TransportWindowStats> {
        let elapsed = self.started_at.elapsed();
        if elapsed < FEEDBACK_INTERVAL {
            return None;
        }

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
        };

        self.started_at = Instant::now();
        self.received_packets = 0;
        self.lost_packets = 0;
        self.late_packets = 0;
        self.completed_frames = 0;
        self.dropped_frames = 0;
        self.received_bytes = 0;
        self.received_video_bytes = 0;
        self.received_audio_bytes = 0;

        Some(stats)
    }
}
