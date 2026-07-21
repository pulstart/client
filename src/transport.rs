use crossbeam_channel::{Receiver as PacketChannel, TryRecvError};
use st_client_core::MediaDemux;
pub use st_client_core::{AudioPacket, ReceivedData, TransportWindowStats};
use st_protocol::tunnel::CryptoContext;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;

const MAX_UDP_DATAGRAM_SIZE: usize = 65_535;
const DEFAULT_UDP_RECV_BUFFER: i32 = 4 * 1024 * 1024;

fn poll_timeout_ms(timeout: Duration) -> i32 {
    if timeout.is_zero() {
        return 0;
    }
    timeout.as_millis().max(1).min(i32::MAX as u128) as i32
}

#[cfg(target_os = "linux")]
const RECVMMSG_BATCH: usize = 32;

fn configured_udp_recv_buffer() -> i32 {
    std::env::var("ST_UDP_RCVBUF")
        .ok()
        .and_then(|raw| raw.parse::<i32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_UDP_RECV_BUFFER)
}

#[cfg(unix)]
fn tune_udp_recv_buffer(socket: &UdpSocket, target_bytes: i32) {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    unsafe {
        // Try SO_RCVBUFFORCE first (bypasses rmem_max when CAP_NET_ADMIN is granted),
        // then fall back to plain SO_RCVBUF which is clamped to net.core.rmem_max.
        #[cfg(target_os = "linux")]
        {
            let _ = libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUFFORCE,
                &target_bytes as *const _ as *const libc::c_void,
                std::mem::size_of::<i32>() as libc::socklen_t,
            );
        }
        let _ = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &target_bytes as *const _ as *const libc::c_void,
            std::mem::size_of::<i32>() as libc::socklen_t,
        );
    }
}

#[cfg(not(unix))]
fn tune_udp_recv_buffer(_socket: &UdpSocket, _target_bytes: i32) {}

#[cfg(target_os = "linux")]
mod linux_batch {
    use super::{MAX_UDP_DATAGRAM_SIZE, RECVMMSG_BATCH};
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
    use std::os::fd::RawFd;

    // Kernel UAPI constants (include/uapi/linux/udp.h). Not always exposed by
    // libc's glibc bindings.
    pub const UDP_GRO: libc::c_int = 104;

    // Enough room for the single int UDP_GRO ancillary plus alignment.
    const CMSG_BUF_LEN: usize = 64;

    pub struct RecvBatch {
        storage: Box<[[u8; MAX_UDP_DATAGRAM_SIZE]]>,
        addrs: Box<[libc::sockaddr_storage]>,
        iovecs: Box<[libc::iovec]>,
        msgs: Box<[libc::mmsghdr]>,
        cmsg_bufs: Box<[[u8; CMSG_BUF_LEN]]>,
        gro_enabled: bool,
    }

    // msghdr contains raw pointers back into the RecvBatch's own storage/addrs/iovecs/cmsg_bufs,
    // which makes the compiler treat it as !Send. The RecvBatch is only ever touched
    // from the receive pipeline thread, and the pointers are rebuilt on every
    // recv_batch call, so sending the whole batch between threads is safe.
    unsafe impl Send for RecvBatch {}

    pub struct ReceivedMsg<'a> {
        pub data: &'a mut [u8],
        pub addr: Option<SocketAddr>,
        /// Non-zero when UDP_GRO coalesced multiple datagrams into this buffer.
        /// Callers must then split `data` into `segment_size` strides (last segment may be shorter).
        pub segment_size: usize,
    }

    impl RecvBatch {
        pub fn new() -> Self {
            let storage: Box<[[u8; MAX_UDP_DATAGRAM_SIZE]]> = (0..RECVMMSG_BATCH)
                .map(|_| [0u8; MAX_UDP_DATAGRAM_SIZE])
                .collect();
            let addrs: Box<[libc::sockaddr_storage]> = (0..RECVMMSG_BATCH)
                .map(|_| unsafe { std::mem::zeroed() })
                .collect();
            let iovecs: Box<[libc::iovec]> = (0..RECVMMSG_BATCH)
                .map(|_| libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                })
                .collect();
            let msgs: Box<[libc::mmsghdr]> = (0..RECVMMSG_BATCH)
                .map(|_| unsafe { std::mem::zeroed() })
                .collect();
            let cmsg_bufs: Box<[[u8; CMSG_BUF_LEN]]> =
                (0..RECVMMSG_BATCH).map(|_| [0u8; CMSG_BUF_LEN]).collect();
            Self {
                storage,
                addrs,
                iovecs,
                msgs,
                cmsg_bufs,
                gro_enabled: false,
            }
        }

        /// Attempt to enable UDP_GRO on the socket (Linux kernel ≥ 5.0).
        /// Returns true on success; on older kernels this is a no-op and the
        /// receiver keeps the plain recvmmsg path.
        pub fn try_enable_gro(&mut self, fd: RawFd) -> bool {
            let on: libc::c_int = 1;
            let rc = unsafe {
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_UDP,
                    UDP_GRO,
                    &on as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            self.gro_enabled = rc == 0;
            self.gro_enabled
        }

        #[allow(dead_code)]
        pub fn gro_enabled(&self) -> bool {
            self.gro_enabled
        }

        /// Non-blocking batch receive. Returns Ok(n) with 0..=RECVMMSG_BATCH messages
        /// or an io error (WouldBlock when no datagrams are queued).
        pub fn recv_batch(&mut self, fd: RawFd) -> io::Result<usize> {
            // Rebuild self-referential pointers fresh each call — Box slices are stable
            // between calls but we write the pointer graph every time to be safe.
            for i in 0..RECVMMSG_BATCH {
                self.iovecs[i] = libc::iovec {
                    iov_base: self.storage[i].as_mut_ptr() as *mut libc::c_void,
                    iov_len: MAX_UDP_DATAGRAM_SIZE,
                };
                let iov_ptr = &mut self.iovecs[i] as *mut libc::iovec;
                let addr_ptr = &mut self.addrs[i] as *mut libc::sockaddr_storage;
                let (ctrl_ptr, ctrl_len) = if self.gro_enabled {
                    (
                        self.cmsg_bufs[i].as_mut_ptr() as *mut libc::c_void,
                        CMSG_BUF_LEN,
                    )
                } else {
                    (std::ptr::null_mut(), 0)
                };
                self.msgs[i].msg_hdr = libc::msghdr {
                    msg_name: addr_ptr as *mut libc::c_void,
                    msg_namelen: std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
                    msg_iov: iov_ptr,
                    msg_iovlen: 1,
                    msg_control: ctrl_ptr,
                    msg_controllen: ctrl_len as _,
                    msg_flags: 0,
                };
                self.msgs[i].msg_len = 0;
            }

            let ret = unsafe {
                libc::recvmmsg(
                    fd,
                    self.msgs.as_mut_ptr(),
                    RECVMMSG_BATCH as libc::c_uint,
                    libc::MSG_DONTWAIT,
                    std::ptr::null_mut(),
                )
            };
            if ret < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(ret as usize)
            }
        }

        pub fn message(&mut self, i: usize) -> ReceivedMsg<'_> {
            let len = self.msgs[i].msg_len as usize;
            let namelen = self.msgs[i].msg_hdr.msg_namelen as usize;
            let addr = sockaddr_to_socket_addr(&self.addrs[i], namelen);
            let segment_size = if self.gro_enabled {
                extract_udp_gro_segment_size(&self.msgs[i].msg_hdr)
            } else {
                0
            };
            ReceivedMsg {
                data: &mut self.storage[i][..len],
                addr,
                segment_size,
            }
        }
    }

    /// Walk ancillary data looking for IPPROTO_UDP / UDP_GRO. Returns the per-segment
    /// size when present, else 0.
    fn extract_udp_gro_segment_size(hdr: &libc::msghdr) -> usize {
        if hdr.msg_control.is_null() || hdr.msg_controllen == 0 {
            return 0;
        }
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(hdr);
            while !cmsg.is_null() {
                let level = (*cmsg).cmsg_level;
                let ty = (*cmsg).cmsg_type;
                if level == libc::IPPROTO_UDP && ty == UDP_GRO {
                    let data_ptr = libc::CMSG_DATA(cmsg) as *const libc::c_int;
                    let seg = *data_ptr;
                    return seg.max(0) as usize;
                }
                cmsg = libc::CMSG_NXTHDR(hdr, cmsg);
            }
        }
        0
    }

    fn sockaddr_to_socket_addr(
        storage: &libc::sockaddr_storage,
        namelen: usize,
    ) -> Option<SocketAddr> {
        if namelen == 0 {
            return None;
        }
        match storage.ss_family as libc::c_int {
            libc::AF_INET => {
                let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                let port = u16::from_be(sin.sin_port);
                Some(SocketAddr::V4(SocketAddrV4::new(ip, port)))
            }
            libc::AF_INET6 => {
                let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
                let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
                let port = u16::from_be(sin6.sin6_port);
                let flowinfo = sin6.sin6_flowinfo;
                let scope_id = sin6.sin6_scope_id;
                Some(SocketAddr::V6(SocketAddrV6::new(
                    ip, port, flowinfo, scope_id,
                )))
            }
            _ => None,
        }
    }

    #[allow(dead_code)]
    fn _ip_unused(_a: IpAddr) {}
}

pub struct UdpReceiver {
    socket: UdpSocket,
    #[cfg(not(target_os = "linux"))]
    buf: Vec<u8>,
    crypto: Option<Arc<CryptoContext>>,
    inner: MediaDemux,
    pending: VecDeque<ReceivedData>,
    #[cfg(target_os = "linux")]
    batch: linux_batch::RecvBatch,
    #[cfg(target_os = "linux")]
    uring: Option<crate::linux_uring::UringRecv>,
}

pub struct PacketReceiver {
    packet_rx: PacketChannel<Vec<u8>>,
    inner: MediaDemux,
}

// Boxing a variant would add heap indirection on the per-packet receive hot
// path; the enum is held once per session, so the size difference is harmless.
#[allow(clippy::large_enum_variant)]
pub enum MediaReceiver {
    Udp(UdpReceiver),
    Packets(PacketReceiver),
}

impl UdpReceiver {
    pub fn from_socket(
        socket: UdpSocket,
        crypto: Option<Arc<CryptoContext>>,
    ) -> Result<Self, String> {
        socket
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking: {e}"))?;
        tune_udp_recv_buffer(&socket, configured_udp_recv_buffer());
        #[cfg(target_os = "linux")]
        let batch = {
            use std::os::fd::AsRawFd;
            let mut batch = linux_batch::RecvBatch::new();
            let gro = batch.try_enable_gro(socket.as_raw_fd());
            if std::env::var_os("ST_TRACE").is_some() {
                eprintln!(
                    "[transport] UDP_GRO {}",
                    if gro { "enabled" } else { "unavailable" }
                );
            }
            batch
        };
        #[cfg(target_os = "linux")]
        let uring = if crate::linux_uring::io_uring_requested() {
            use std::os::fd::AsRawFd;
            match crate::linux_uring::UringRecv::new(socket.as_raw_fd()) {
                Some(u) => {
                    eprintln!("[transport] io_uring receive path enabled");
                    Some(u)
                }
                None => {
                    eprintln!(
                        "[transport] io_uring requested but unavailable; falling back to recvmmsg"
                    );
                    None
                }
            }
        } else {
            None
        };
        Ok(Self {
            socket,
            #[cfg(not(target_os = "linux"))]
            // The server can tune UDP slice size at runtime, so the receive
            // buffer must handle the largest datagram the OS can deliver
            // instead of assuming an Ethernet-sized packet.
            buf: vec![0u8; MAX_UDP_DATAGRAM_SIZE],
            crypto,
            inner: MediaDemux::default(),
            pending: VecDeque::with_capacity(64),
            #[cfg(target_os = "linux")]
            batch,
            #[cfg(target_os = "linux")]
            uring,
        })
    }

    /// Receive the next immediately-available piece of data.
    /// Returns `None` when the socket has no queued packets yet.
    pub fn try_receive(&mut self) -> Option<ReceivedData> {
        if let Some(data) = self.pending.pop_front() {
            return Some(data);
        }
        self.refill_pending();
        self.pending.pop_front()
    }

    /// Block up to `timeout` waiting for socket data. Returns immediately once
    /// data has arrived. On platforms without `poll`, degrades to a short sleep.
    pub fn wait_for_data(&mut self, timeout: Duration) {
        #[cfg(target_os = "linux")]
        {
            if let Some(uring) = self.uring.as_mut() {
                let _ = uring.wait(poll_timeout_ms(timeout));
                return;
            }
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = self.socket.as_raw_fd();
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe {
                let _ = libc::poll(&mut pfd as *mut libc::pollfd, 1, poll_timeout_ms(timeout));
            }
        }
        #[cfg(not(unix))]
        {
            std::thread::sleep(timeout);
        }
    }

    pub fn take_stats(&mut self) -> Option<TransportWindowStats> {
        self.inner.take_stats()
    }

    fn reset_video(&mut self) {
        self.inner.reset_video();
        self.pending
            .retain(|data| !matches!(data, ReceivedData::Video(_, _, _)));
    }

    #[cfg(target_os = "linux")]
    fn refill_pending(&mut self) {
        use std::os::fd::AsRawFd;
        // io_uring fast path: drain any completions and process them. Slot
        // re-arming happens on the next `wait_for_data`/`wait()` call via
        // `refill_sqes`; we deliberately do NOT rearm here because rearming
        // can interact with something in the live client that triggers
        // pipeline shutdown. Behavior matches the original path — SQE
        // starvation isn't visible in practice with a 32-deep ring and
        // typical frame sizes.
        if let Some(uring) = self.uring.as_mut() {
            uring.drain_completions();
            while let Some((_idx, msg)) = uring.take() {
                let addr = msg.addr;
                let seg_size = msg.segment_size;
                let total_len = msg.data.len();
                if seg_size > 0 && total_len > seg_size {
                    // UDP_GRO coalesced multiple datagrams — walk the buffer
                    // in stride-sized chunks and decrypt/demux each one. Same
                    // split logic as the recvmmsg path above; without it the
                    // demuxer treats the concatenated blob as one giant frame
                    // and the decoder parses garbage (`[cuvid] unsupported
                    // bit depth: 16`).
                    let mut offset = 0;
                    while offset < total_len {
                        let end = (offset + seg_size).min(total_len);
                        let segment = &mut msg.data[offset..end];
                        let raw: Option<&[u8]> = if let Some(ref crypto) = self.crypto {
                            crypto.decrypt_in_place(segment)
                        } else {
                            Some(&*segment)
                        };
                        if let Some(raw) = raw {
                            if let Some(data) = self.inner.process_packet(raw, addr) {
                                self.pending.push_back(data);
                            }
                        }
                        offset = end;
                    }
                } else {
                    let raw: Option<&[u8]> = if let Some(ref crypto) = self.crypto {
                        crypto.decrypt_in_place(msg.data)
                    } else {
                        Some(&*msg.data)
                    };
                    if let Some(raw) = raw {
                        if let Some(data) = self.inner.process_packet(raw, addr) {
                            self.pending.push_back(data);
                        }
                    }
                }
            }
            return;
        }
        let fd = self.socket.as_raw_fd();
        loop {
            match self.batch.recv_batch(fd) {
                Ok(0) => return,
                Ok(n) => {
                    for i in 0..n {
                        let msg = self.batch.message(i);
                        let addr = msg.addr;
                        let seg_size = msg.segment_size;
                        let total_len = msg.data.len();
                        if seg_size > 0 && total_len > seg_size {
                            // UDP_GRO coalesced multiple datagrams — split back into
                            // per-datagram strides before running the packet processor.
                            let mut offset = 0;
                            while offset < total_len {
                                let end = (offset + seg_size).min(total_len);
                                let segment = &mut msg.data[offset..end];
                                let raw: Option<&[u8]> = if let Some(ref crypto) = self.crypto {
                                    crypto.decrypt_in_place(segment)
                                } else {
                                    Some(&*segment)
                                };
                                if let Some(raw) = raw {
                                    if let Some(data) = self.inner.process_packet(raw, addr) {
                                        self.pending.push_back(data);
                                    }
                                }
                                offset = end;
                            }
                        } else {
                            let raw: Option<&[u8]> = if let Some(ref crypto) = self.crypto {
                                crypto.decrypt_in_place(msg.data)
                            } else {
                                Some(&*msg.data)
                            };
                            if let Some(raw) = raw {
                                if let Some(data) = self.inner.process_packet(raw, addr) {
                                    self.pending.push_back(data);
                                }
                            }
                        }
                    }
                    // recvmmsg returns up to RECVMMSG_BATCH — if it filled the batch
                    // fully, there may be more queued datagrams. Stop here anyway so
                    // feedback/recovery checks get a chance to run between batches.
                    return;
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => return,
                Err(err) if err.raw_os_error() == Some(libc::EINTR) => continue,
                Err(_) => return,
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn refill_pending(&mut self) {
        let crypto = self.crypto.as_ref().map(Arc::clone);
        loop {
            match self.socket.recv_from(&mut self.buf) {
                Ok((n, addr)) => {
                    let raw: Option<&[u8]> = match crypto.as_ref() {
                        Some(c) => c.decrypt_in_place(&mut self.buf[..n]).map(|pt| &*pt),
                        None => Some(&self.buf[..n]),
                    };
                    if let Some(raw) = raw {
                        if let Some(data) = self.inner.process_packet(raw, Some(addr)) {
                            self.pending.push_back(data);
                        }
                    }
                    if self.pending.len() >= 32 {
                        return;
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => return,
                Err(_) => return,
            }
        }
    }
}

impl PacketReceiver {
    pub fn from_channel(packet_rx: PacketChannel<Vec<u8>>) -> Self {
        Self {
            packet_rx,
            inner: MediaDemux::default(),
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

    pub fn wait_for_data(&mut self, timeout: Duration) {
        // crossbeam try_recv has no wakeup we can latch to without consuming,
        // so fall back to a short sleep. Most real deployments take the UDP path.
        std::thread::sleep(timeout);
    }

    pub fn take_stats(&mut self) -> Option<TransportWindowStats> {
        self.inner.take_stats()
    }

    fn reset_video(&mut self) {
        self.inner.reset_video();
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

    pub fn wait_for_data(&mut self, timeout: Duration) {
        match self {
            Self::Udp(receiver) => receiver.wait_for_data(timeout),
            Self::Packets(receiver) => receiver.wait_for_data(timeout),
        }
    }

    pub fn take_stats(&mut self) -> Option<TransportWindowStats> {
        match self {
            Self::Udp(receiver) => receiver.take_stats(),
            Self::Packets(receiver) => receiver.take_stats(),
        }
    }

    pub fn reset_video(&mut self) {
        match self {
            Self::Udp(receiver) => receiver.reset_video(),
            Self::Packets(receiver) => receiver.reset_video(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::poll_timeout_ms;
    use std::time::Duration;

    #[test]
    fn poll_timeout_rounds_nonzero_sub_millisecond_waits_up() {
        assert_eq!(poll_timeout_ms(Duration::ZERO), 0);
        assert_eq!(poll_timeout_ms(Duration::from_nanos(1)), 1);
        assert_eq!(poll_timeout_ms(Duration::from_micros(999)), 1);
        assert_eq!(poll_timeout_ms(Duration::from_millis(1)), 1);
        assert_eq!(poll_timeout_ms(Duration::from_micros(1_001)), 1);
        assert_eq!(poll_timeout_ms(Duration::from_millis(2)), 2);
    }
}
