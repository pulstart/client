use crossbeam_channel::{Receiver as PacketChannel, TryRecvError};
use st_protocol::packet::{PacketHeader, PayloadType, HEADER_SIZE};
use st_protocol::tunnel::CryptoContext;
use st_protocol::{CompletedFrame, FrameAssembler, TransportFeedback};
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FEEDBACK_INTERVAL: Duration = Duration::from_millis(500);
const URGENT_FEEDBACK_MIN_INTERVAL: Duration = Duration::from_millis(100);
const MAX_UDP_DATAGRAM_SIZE: usize = 65_535;

/// Demuxed data from the unified UDP stream.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub seq: u16,
    pub data: Vec<u8>,
}

pub enum ReceivedData {
    /// A fully assembled video frame (one or more packets reassembled).
    Video(CompletedFrame),
    /// A single audio packet (raw Opus data).
    Audio(AudioPacket),
}

pub struct UdpReceiver {
    socket: UdpSocket,
    buf: Vec<u8>,
    crypto: Option<Arc<CryptoContext>>,
    inner: PacketProcessor,
}

pub struct PacketReceiver {
    packet_rx: PacketChannel<Vec<u8>>,
    inner: PacketProcessor,
}

pub enum MediaReceiver {
    Udp(UdpReceiver),
    Packets(PacketReceiver),
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

    pub fn needs_recovery_keyframe(self) -> bool {
        self.lost_packets > 0 || self.dropped_frames > 0
    }
}

impl UdpReceiver {
    pub fn from_socket(socket: UdpSocket, crypto: Option<Arc<CryptoContext>>) -> Result<Self, String> {
        socket
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking: {e}"))?;
        Ok(Self {
            socket,
            // The server can tune UDP slice size at runtime, so the receive
            // buffer must handle the largest datagram the OS can deliver
            // instead of assuming an Ethernet-sized packet.
            buf: vec![0u8; MAX_UDP_DATAGRAM_SIZE],
            crypto,
            inner: PacketProcessor::default(),
        })
    }

    /// Receive the next immediately-available piece of data.
    /// Returns `None` when the socket has no queued packets yet.
    pub fn try_receive(&mut self) -> Option<ReceivedData> {
        loop {
            match self.socket.recv_from(&mut self.buf) {
                Ok((n, addr)) => {
                    // Decrypt if crypto is active.
                    let raw: &[u8] = if let Some(ref crypto) = self.crypto {
                        match crypto.decrypt_in_place(&mut self.buf[..n]) {
                            Some(pt) => pt,
                            None => continue, // auth failure — skip packet
                        }
                    } else {
                        &self.buf[..n]
                    };
                    if let Some(data) = self.inner.process_packet(raw, Some(addr)) {
                        return Some(data);
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => return None,
                Err(_) => return None,
            }
        }
    }

    pub fn take_stats(&mut self) -> Option<TransportWindowStats> {
        self.inner.take_stats()
    }

    pub fn take_pending_recovery(&mut self) -> bool {
        self.inner.take_pending_recovery()
    }
}

impl PacketReceiver {
    pub fn from_channel(packet_rx: PacketChannel<Vec<u8>>) -> Self {
        Self {
            packet_rx,
            inner: PacketProcessor::default(),
        }
    }

    pub fn try_receive(&mut self) -> Option<ReceivedData> {
        loop {
            match self.packet_rx.try_recv() {
                Ok(packet) => {
                    if let Some(data) = self.inner.process_packet(&packet, None) {
                        return Some(data);
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return None,
            }
        }
    }

    pub fn take_stats(&mut self) -> Option<TransportWindowStats> {
        self.inner.take_stats()
    }

    pub fn take_pending_recovery(&mut self) -> bool {
        self.inner.take_pending_recovery()
    }
}

impl MediaReceiver {
    pub fn from_udp_socket(
        socket: UdpSocket,
        crypto: Option<Arc<CryptoContext>>,
    ) -> Result<Self, String> {
        Ok(Self::Udp(UdpReceiver::from_socket(socket, crypto)?))
    }

    pub fn from_packet_channel(packet_rx: PacketChannel<Vec<u8>>) -> Self {
        Self::Packets(PacketReceiver::from_channel(packet_rx))
    }

    pub fn try_receive(&mut self) -> Option<ReceivedData> {
        match self {
            Self::Udp(receiver) => receiver.try_receive(),
            Self::Packets(receiver) => receiver.try_receive(),
        }
    }

    pub fn take_stats(&mut self) -> Option<TransportWindowStats> {
        match self {
            Self::Udp(receiver) => receiver.take_stats(),
            Self::Packets(receiver) => receiver.take_stats(),
        }
    }

    pub fn take_pending_recovery(&mut self) -> bool {
        match self {
            Self::Udp(receiver) => receiver.take_pending_recovery(),
            Self::Packets(receiver) => receiver.take_pending_recovery(),
        }
    }
}

struct PacketProcessor {
    assembler: FrameAssembler,
    feedback: FeedbackWindow,
    pending_recovery: bool,
    trace_packets_logged: usize,
}

impl Default for PacketProcessor {
    fn default() -> Self {
        Self {
            assembler: FrameAssembler::new(),
            feedback: FeedbackWindow::default(),
            pending_recovery: false,
            trace_packets_logged: 0,
        }
    }
}

impl PacketProcessor {
    fn process_packet(
        &mut self,
        raw: &[u8],
        from_addr: Option<SocketAddr>,
    ) -> Option<ReceivedData> {
        let raw_len = raw.len();

        if let Some(header) = PacketHeader::deserialize(raw) {
            if std::env::var_os("ST_TRACE").is_some() && self.trace_packets_logged < 24 {
                if let Some(addr) = from_addr {
                    eprintln!(
                        "[trace][client] udp packet #{} from {addr}: type={:?} frame_id={} seq={} bytes={raw_len}",
                        self.trace_packets_logged,
                        header.payload_type,
                        header.frame_id,
                        header.seq
                    );
                } else {
                    eprintln!(
                        "[trace][client] bridged media packet #{}: type={:?} frame_id={} seq={} bytes={raw_len}",
                        self.trace_packets_logged,
                        header.payload_type,
                        header.frame_id,
                        header.seq
                    );
                }
                self.trace_packets_logged += 1;
            }
            if header.payload_type == PayloadType::Audio {
                if raw_len > HEADER_SIZE {
                    self.feedback.received_bytes =
                        self.feedback.received_bytes.saturating_add(raw_len as u64);
                    self.feedback.received_audio_bytes =
                        self.feedback.received_audio_bytes.saturating_add(raw_len as u64);
                    return Some(ReceivedData::Audio(AudioPacket {
                        seq: header.seq,
                        data: raw[HEADER_SIZE..].to_vec(),
                    }));
                }
                return None;
            }
        }

        self.feedback.received_packets = self.feedback.received_packets.saturating_add(1);
        self.feedback.received_bytes =
            self.feedback.received_bytes.saturating_add(raw_len as u64);
        self.feedback.received_video_bytes =
            self.feedback.received_video_bytes.saturating_add(raw_len as u64);
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
        if outcome.feedback.lost_packets > 0 || outcome.feedback.dropped_frames > 0 {
            self.pending_recovery = true;
            self.feedback.urgent = true;
        }
        if let Some(frame) = outcome.completed {
            self.feedback.completed_frames = self.feedback.completed_frames.saturating_add(1);
            return Some(ReceivedData::Video(frame));
        }
        None
    }

    fn take_stats(&mut self) -> Option<TransportWindowStats> {
        self.feedback.take_if_due()
    }

    fn take_pending_recovery(&mut self) -> bool {
        std::mem::take(&mut self.pending_recovery)
    }
}

#[derive(Debug)]
struct FeedbackWindow {
    started_at: Instant,
    urgent: bool,
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
            urgent: false,
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
        let urgent_due = self.urgent && elapsed >= URGENT_FEEDBACK_MIN_INTERVAL;
        if elapsed < FEEDBACK_INTERVAL && !urgent_due {
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
        self.urgent = false;
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
