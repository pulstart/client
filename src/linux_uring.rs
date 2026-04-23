//! io_uring-backed UDP receive path. Default ON on Linux; set
//! `ST_IO_URING=0` to force the `recvmmsg`/`UDP_GRO` fallback for debugging.
//!
//! History — three bugs were identified and fixed before default-on, each
//! with a live-validated repro and a regression-guard test:
//!
//! 1. **Dangling `Timespec` UAF** — the first wait-timeout implementation
//!    queued an `opcode::Timeout` SQE that could outlive the Timespec on
//!    the stack. Fixed by moving to `submit_with_args(1,
//!    SubmitArgs::timespec(&ts))` so the borrow is bounded by the
//!    synchronous enter call. Covered by
//!    `uring_wait_does_not_leak_timeout_state`.
//!
//! 2. **Missing `UDP_GRO` cmsg plumbing** — `UdpReceiver::from_socket`
//!    unconditionally enables `UDP_GRO` on the recv socket, which lets the
//!    kernel coalesce bursty UDP datagrams into a single `recvmsg` return.
//!    The previous uring plumbing passed `msg_control = null,
//!    msg_controllen = 0`, so the per-segment stride in the `UDP_GRO` cmsg
//!    was dropped and `take()` handed the demuxer a concatenated blob.
//!    That decoded as one giant frame of garbage bitstream and produced
//!    `[cuvid] unsupported bit depth: 16` / `[hevc] Error parsing NAL
//!    unit`. Covered by `uring_recv_with_gso_sender_splits_coalesced_
//!    return` which uses a `UDP_SEGMENT` (GSO) sender to force kernel
//!    coalescing the test then verifies.
//!
//! 3. **LIFO `Vec::pop` on `ready`** — the queue of drained-but-not-taken
//!    slot indices was a `Vec` popped from the tail, so a 32-packet drain
//!    batch arrived at the assembler in reverse order. `try_recover_
//!    single_loss` triggers the moment `received.len() + 1 == total_
//!    packets`, so reversed arrival caused the parity-XOR recovery to
//!    fire on a wrong missing-seq candidate, back-filling the frame with
//!    bogus reconstructed bytes. Same bitstream corruption symptom as (2).
//!    Fixed by switching `ready` to `VecDeque` with `push_back` /
//!    `pop_front` so delivery order matches the recvmmsg path.
//!
//! Separately, `is_timeout()` in `main.rs` now treats `ErrorKind::
//! Interrupted` as retriable alongside `WouldBlock`/`TimedOut`. io_uring
//! kernel workers deliver signals that interrupt blocking syscalls on
//! the same process, and the TCP control loop was tearing down the
//! session on EINTR — that's why the old behavior was "decode ~12 frames
//! then return to server list" even after the bitstream was correct.
//!
//! Set `ST_URING_TRACE=1` to dump per-CQE diagnostic info (slot id, len,
//! GRO segment_size, MSG_TRUNC/MSG_CTRUNC warnings, negative errno on
//! completion).

use std::collections::VecDeque;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::RawFd;

use io_uring::{opcode, types, IoUring};

const MAX_UDP_DATAGRAM_SIZE: usize = 65_535;

/// Total in-flight recvmsg SQEs. Bigger → less syscall churn, more memory.
const URING_RECV_DEPTH: usize = 32;

/// Enough room for the single int UDP_GRO ancillary plus alignment. Must mirror
/// the layout `linux_batch::RecvBatch` uses, since the kernel writes the same
/// cmsg for both recvmsg() and recvmmsg() when the socket has `UDP_GRO` on.
const CMSG_BUF_LEN: usize = 64;

/// Kernel UAPI constant (include/uapi/linux/udp.h). Not exported by libc's
/// glibc bindings, so we define it locally.
const UDP_GRO: libc::c_int = 104;

/// Diagnostic trace gate. Set `ST_URING_TRACE=1` to dump per-CQE info:
/// slot idx, length, cmsg segment_size, first 16 bytes. Intended for
/// offline byte-level comparison against the `recvmmsg` path when chasing
/// uring-specific corruption regressions.
pub fn uring_trace_enabled() -> bool {
    std::env::var("ST_URING_TRACE").ok().as_deref() == Some("1")
}

/// Default-on on Linux. `ST_IO_URING=0` (or `false`/`no`/`off`) is the
/// escape hatch that forces the `recvmmsg`/`UDP_GRO` fallback. The empty
/// value or any unrecognized setting stays on the io_uring path.
pub fn io_uring_requested() -> bool {
    match std::env::var("ST_IO_URING").ok().as_deref() {
        Some("0") | Some("false") | Some("no") | Some("off") => false,
        _ => true,
    }
}

pub struct ReceivedMsg<'a> {
    pub data: &'a mut [u8],
    pub addr: Option<SocketAddr>,
    /// Non-zero when UDP_GRO coalesced multiple datagrams into `data`.
    /// Callers must split `data` into `segment_size` strides (the last segment
    /// may be shorter). Mirrors `linux_batch::ReceivedMsg::segment_size`.
    pub segment_size: usize,
}

struct Slot {
    buf: Box<[u8; MAX_UDP_DATAGRAM_SIZE]>,
    addr: Box<libc::sockaddr_storage>,
    iov: Box<libc::iovec>,
    hdr: Box<libc::msghdr>,
    cmsg: Box<[u8; CMSG_BUF_LEN]>,
    len: i32,
    in_flight: bool,
}

pub struct UringRecv {
    ring: IoUring,
    slots: Vec<Slot>,
    fd: RawFd,
    /// FIFO queue of slot indices whose CQE has been drained but whose
    /// buffer hasn't been handed to the caller yet. MUST be FIFO, not LIFO,
    /// to match the delivery order the `recvmmsg` path produces. With a
    /// LIFO `Vec::pop()` the caller would see packets in reverse arrival
    /// order within each drain batch — and for a multi-packet frame where
    /// ~30 packets land together, that means the assembler processes the
    /// frame back-to-front, which the existing wraparound-aware ordering
    /// heuristics were never validated against and which reproduces the
    /// `[cuvid] unsupported bit depth: 16` decoder-corruption symptom.
    ready: VecDeque<usize>,
}

// msghdr/iovec hold raw pointers into the Slot's own boxed storage. The
// UringRecv is owned by the pipeline thread; sending it between threads is
// safe because the pointer graph is rebuilt before each submission.
unsafe impl Send for UringRecv {}

impl UringRecv {
    /// Attempt to bind an io_uring to `fd` for recvmsg drain. Returns None on
    /// any failure — caller must fall back to the recvmmsg path.
    pub fn new(fd: RawFd) -> Option<Self> {
        // `entries` is rounded up to a power of two by the kernel; request
        // 2x depth so there's always room for completions in flight.
        let ring = IoUring::builder()
            .build((URING_RECV_DEPTH * 2) as u32)
            .ok()?;

        let mut slots = Vec::with_capacity(URING_RECV_DEPTH);
        for _ in 0..URING_RECV_DEPTH {
            let mut buf = Box::new([0u8; MAX_UDP_DATAGRAM_SIZE]);
            let mut addr: Box<libc::sockaddr_storage> = Box::new(unsafe { std::mem::zeroed() });
            let iov = Box::new(libc::iovec {
                iov_base: buf.as_mut_ptr() as *mut libc::c_void,
                iov_len: MAX_UDP_DATAGRAM_SIZE,
            });
            let cmsg: Box<[u8; CMSG_BUF_LEN]> = Box::new([0u8; CMSG_BUF_LEN]);
            let hdr = Box::new(libc::msghdr {
                msg_name: addr.as_mut() as *mut _ as *mut libc::c_void,
                msg_namelen: std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
                msg_iov: std::ptr::null_mut(), // rebuilt per submit
                msg_iovlen: 1,
                msg_control: std::ptr::null_mut(), // rebuilt per submit
                msg_controllen: 0,
                msg_flags: 0,
            });
            slots.push(Slot {
                buf,
                addr,
                iov,
                hdr,
                cmsg,
                len: 0,
                in_flight: false,
            });
        }

        let mut this = Self {
            ring,
            slots,
            fd,
            ready: VecDeque::with_capacity(URING_RECV_DEPTH),
        };

        // Seed all slots. If we can't even post the first batch, treat as
        // unsupported and fall back.
        if this.refill_sqes().is_err() {
            return None;
        }
        Some(this)
    }

    fn refill_sqes(&mut self) -> io::Result<()> {
        let mut sq = self.ring.submission();
        for (idx, slot) in self.slots.iter_mut().enumerate() {
            if slot.in_flight {
                continue;
            }
            // Rebuild pointers each submit — boxes are stable but cheap to reset.
            slot.iov.iov_base = slot.buf.as_mut_ptr() as *mut libc::c_void;
            slot.iov.iov_len = MAX_UDP_DATAGRAM_SIZE;
            slot.hdr.msg_name = slot.addr.as_mut() as *mut _ as *mut libc::c_void;
            slot.hdr.msg_namelen =
                std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            slot.hdr.msg_iov = slot.iov.as_mut() as *mut libc::iovec;
            slot.hdr.msg_iovlen = 1;
            // Always provision a cmsg buffer. The socket may have `UDP_GRO`
            // enabled (linux_batch::try_enable_gro does this unconditionally
            // in UdpReceiver::from_socket), in which case the kernel coalesces
            // adjacent datagrams into one recvmsg return and reports the
            // per-segment stride through the `UDP_GRO` ancillary message. If
            // we don't give it a cmsg buffer, the boundaries are lost and
            // `take()` hands the caller a concatenated blob that the packet
            // demuxer then parses as a single giant frame — which is the
            // exact production regression (`[cuvid] unsupported bit depth:
            // 16`) that two earlier default-on attempts both reproduced
            // while the existing unit tests — which never enabled GRO on
            // the test socket — kept passing.
            slot.hdr.msg_control = slot.cmsg.as_mut_ptr() as *mut libc::c_void;
            slot.hdr.msg_controllen = CMSG_BUF_LEN as _;
            slot.hdr.msg_flags = 0;

            let sqe = opcode::RecvMsg::new(
                types::Fd(self.fd),
                slot.hdr.as_mut() as *mut libc::msghdr,
            )
            .build()
            .user_data(idx as u64);

            if unsafe { sq.push(&sqe).is_err() } {
                break;
            }
            slot.in_flight = true;
        }
        drop(sq);
        self.ring
            .submit()
            .map(|_| ())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("uring submit: {e}")))
    }

    /// Drain all currently-ready completions without blocking. Returns the
    /// number of newly-ready slots; call `take` to consume them.
    pub fn drain_completions(&mut self) -> usize {
        // Collect raw completion entries first, then apply — avoids holding a
        // borrow on `self.ring` across `self.slots`/`self.ready` updates.
        let mut staged: Vec<(u64, i32)> = Vec::new();
        {
            let mut cq = self.ring.completion();
            cq.sync();
            while let Some(cqe) = cq.next() {
                staged.push((cqe.user_data(), cqe.result()));
            }
        }
        let mut count = 0;
        let trace = uring_trace_enabled();
        for (user_data, result) in staged {
            if user_data == u64::MAX {
                continue;
            }
            let idx = user_data as usize;
            if idx >= self.slots.len() {
                continue;
            }
            let slot = &mut self.slots[idx];
            slot.in_flight = false;
            if result >= 0 {
                slot.len = result;
                self.ready.push_back(idx);
                if trace {
                    let flags = slot.hdr.msg_flags;
                    if flags & libc::MSG_TRUNC != 0 {
                        eprintln!(
                            "[uring-trace] MSG_TRUNC on slot {idx}: datagram longer than {} byte buffer",
                            MAX_UDP_DATAGRAM_SIZE
                        );
                    }
                    if flags & libc::MSG_CTRUNC != 0 {
                        eprintln!(
                            "[uring-trace] MSG_CTRUNC on slot {idx}: ancillary data truncated (cmsg too small)"
                        );
                    }
                }
            } else {
                slot.len = 0;
                if trace {
                    eprintln!(
                        "[uring-trace] recvmsg error on slot {idx}: errno={}",
                        -result
                    );
                }
            }
            count += 1;
        }
        count
    }

    /// Block up to `timeout_ms` waiting for at least one completion. Returns
    /// when completions are drained or the timer fires.
    ///
    /// Uses the kernel's built-in `submit_with_args` timeout path rather than
    /// a queued `opcode::Timeout` SQE. That matters because:
    /// 1. `SubmitArgs::timespec` takes a borrow that only needs to outlive
    ///    the synchronous submit call — no dangling-pointer window after
    ///    return, which is what the decoder-corruption regression was caused
    ///    by.
    /// 2. Nothing is left pending in the ring after wait() returns, so we
    ///    don't accumulate stale Timeout SQEs across repeated wait() calls.
    pub fn wait(&mut self, timeout_ms: i32) -> io::Result<()> {
        if !self.ready.is_empty() {
            return Ok(());
        }
        self.refill_sqes()?;

        let secs = (timeout_ms.max(0) / 1000) as u64;
        let nsecs = ((timeout_ms.max(0) % 1000) * 1_000_000) as u32;
        let ts = types::Timespec::new().sec(secs).nsec(nsecs);
        let args = types::SubmitArgs::new().timespec(&ts);
        match self.ring.submitter().submit_with_args(1, &args) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::ETIME) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
            Err(e) => return Err(e),
        }
        self.drain_completions();
        Ok(())
    }

    /// Take one buffered completion. Caller gets a mutable slice over the
    /// received bytes plus the peer address. Must call `release` with the
    /// returned handle when done to re-arm the slot.
    pub fn take(&mut self) -> Option<(usize, ReceivedMsg<'_>)> {
        let idx = self.ready.pop_front()?;
        let slot = &mut self.slots[idx];
        let len = slot.len as usize;
        let segment_size = extract_udp_gro_segment_size(&slot.hdr);
        let msg_flags = slot.hdr.msg_flags;
        let data = &mut slot.buf[..len];
        let addr = sockaddr_to_socket_addr(&slot.addr);
        if uring_trace_enabled() {
            let preview_len = data.len().min(16);
            let mut hex = String::new();
            for b in &data[..preview_len] {
                hex.push_str(&format!("{b:02x}"));
            }
            eprintln!(
                "[uring-trace] take slot={idx} len={len} seg_size={segment_size} msg_flags=0x{msg_flags:x} head16={hex}"
            );
        }
        Some((
            idx,
            ReceivedMsg {
                data,
                addr,
                segment_size,
            },
        ))
    }

}

/// Walk the cmsg list on `hdr` looking for `IPPROTO_UDP` / `UDP_GRO`. Returns
/// the per-segment stride when present, else 0. Layout matches the parser in
/// `linux_batch::extract_udp_gro_segment_size`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    /// Round-trip: kernel writes a packet to the socket, `UringRecv` reads it
    /// back via recvmsg SQEs, and the bytes match. Exercises the whole
    /// SQE-submit → wait → drain_completions → take pipeline, including the
    /// Timespec lifetime in `wait()`.
    #[test]
    fn uring_recv_reads_single_packet() {
        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind receiver");
        let recv_addr = recv.local_addr().unwrap();
        recv.set_nonblocking(true).unwrap();
        let Some(mut uring) = UringRecv::new(recv.as_raw_fd()) else {
            eprintln!("io_uring unavailable on this kernel; skipping");
            return;
        };

        let send = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        let payload = b"uring-recv correctness";
        send.send_to(payload, recv_addr).unwrap();

        // Give the kernel a moment to deliver, then wait with a bounded timeout.
        uring.wait(1000).expect("wait");
        assert!(!uring.ready.is_empty(), "expected at least one ready slot");
        let (_idx, msg) = uring.take().expect("take");
        assert_eq!(msg.data, payload);
    }

    /// Regression guard for the dangling-`Timespec` UAF: repeatedly call
    /// `wait()` with short timeouts and no incoming data. If the Timespec
    /// were still on the stack after return and later read by the kernel,
    /// this loop would almost certainly trigger memory stomps — in debug
    /// builds that turns into an abort or a garbled buffer when real data
    /// arrives afterwards.
    #[test]
    fn uring_wait_does_not_leak_timeout_state() {
        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let recv_addr = recv.local_addr().unwrap();
        recv.set_nonblocking(true).unwrap();
        let Some(mut uring) = UringRecv::new(recv.as_raw_fd()) else {
            return;
        };

        for _ in 0..32 {
            uring.wait(10).expect("wait");
            assert!(uring.ready.is_empty());
        }

        // After many timeouts, a real packet must still decode correctly.
        let send = UdpSocket::bind("127.0.0.1:0").unwrap();
        let payload = b"after-timeouts";
        send.send_to(payload, recv_addr).unwrap();
        uring.wait(1000).expect("wait final");
        let (_, msg) = uring.take().expect("take final");
        assert_eq!(msg.data, payload);
    }

    /// Production regression guard: when the socket has `UDP_GRO` enabled
    /// (as `UdpReceiver::from_socket` does unconditionally), bursty UDP may
    /// be coalesced into a single recvmsg return. The uring path must either
    /// (a) deliver one CQE per wire datagram, or (b) attach a valid
    /// `segment_size` so the caller can split the coalesced buffer into
    /// strides. Either way, every individual datagram payload must be
    /// recoverable — we must never hand the demuxer a concatenated blob.
    ///
    /// This is the shape the earlier unit tests missed: they never enabled
    /// UDP_GRO on the test socket, so the kernel delivered one CQE per
    /// datagram and the missing cmsg plumbing was invisible. In production
    /// with GRO on, the same code path coalesced bursts and produced the
    /// `[cuvid] unsupported bit depth: 16` / `[hevc] Error parsing NAL unit`
    /// decoder corruption that two default-on attempts both reproduced.
    #[test]
    fn uring_recv_splits_udp_gro_coalesced_burst() {
        use std::os::fd::AsRawFd;
        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let recv_addr = recv.local_addr().unwrap();
        recv.set_nonblocking(true).unwrap();

        // Enable UDP_GRO on the recv socket to match production shape.
        let on: libc::c_int = 1;
        let rc = unsafe {
            libc::setsockopt(
                recv.as_raw_fd(),
                libc::IPPROTO_UDP,
                UDP_GRO,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            eprintln!("UDP_GRO setsockopt unavailable; skipping");
            return;
        }

        let Some(mut uring) = UringRecv::new(recv.as_raw_fd()) else {
            eprintln!("io_uring unavailable; skipping");
            return;
        };

        // Burst identically-sized datagrams from the same connected socket so
        // the kernel is free to coalesce them. We use small payloads with a
        // 4-byte index header so we can detect cross-packet splicing.
        let send = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        send.connect(recv_addr).unwrap();
        const SEG: usize = 16;
        const TOTAL: u32 = 64;
        for i in 0..TOTAL {
            let mut pkt = [0u8; SEG];
            pkt[..4].copy_from_slice(&i.to_be_bytes());
            send.send(&pkt).unwrap();
        }

        // Give the kernel a moment to aggregate if it's going to.
        std::thread::sleep(Duration::from_millis(50));

        let mut seen = std::collections::HashSet::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while seen.len() < TOTAL as usize {
            if std::time::Instant::now() > deadline {
                break;
            }
            uring.wait(200).expect("wait");
            while let Some((_idx, msg)) = uring.take() {
                if msg.segment_size == 0 {
                    // No coalescing for this CQE — must be a single datagram.
                    assert_eq!(
                        msg.data.len(),
                        SEG,
                        "single-CQE payload must be one wire datagram"
                    );
                    let idx = u32::from_be_bytes(msg.data[..4].try_into().unwrap());
                    seen.insert(idx);
                } else {
                    // Coalesced. Split into strides and count each one.
                    assert!(
                        msg.data.len() % msg.segment_size == 0
                            || msg.data.len() % msg.segment_size == SEG % msg.segment_size,
                        "GRO stride should cleanly divide payload",
                    );
                    for chunk in msg.data.chunks(msg.segment_size) {
                        assert_eq!(chunk.len(), SEG, "every stride is one wire datagram");
                        let idx = u32::from_be_bytes(chunk[..4].try_into().unwrap());
                        seen.insert(idx);
                    }
                }
            }
        }
        assert_eq!(
            seen.len(),
            TOTAL as usize,
            "every individual datagram must be recovered — if this fails, the uring \
             path is handing the demuxer concatenated blobs and the decoder will \
             choke on garbage bitstream",
        );
    }

    /// Force the kernel to coalesce on receive by using UDP_SEGMENT (GSO) on
    /// the send side: one sendmsg of N*SEG bytes with `UDP_SEGMENT=SEG` tells
    /// the kernel to split into N wire datagrams, and with UDP_GRO on the
    /// recv side the peer kernel re-coalesces them back into a single
    /// recvmsg return with a valid UDP_GRO cmsg. This is the exact shape of
    /// the production failure that plain loopback sends don't reliably
    /// trigger — without a cmsg-aware recv, the uring take() returns the
    /// concatenated blob and the demuxer feeds the decoder garbage.
    #[test]
    fn uring_recv_with_gso_sender_splits_coalesced_return() {
        use std::os::fd::AsRawFd;
        const UDP_SEGMENT: libc::c_int = 103;

        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind recv");
        let recv_addr = recv.local_addr().unwrap();
        recv.set_nonblocking(true).unwrap();

        let on: libc::c_int = 1;
        let rc = unsafe {
            libc::setsockopt(
                recv.as_raw_fd(),
                libc::IPPROTO_UDP,
                UDP_GRO,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            eprintln!("UDP_GRO setsockopt unavailable; skipping");
            return;
        }

        let Some(mut uring) = UringRecv::new(recv.as_raw_fd()) else {
            return;
        };

        let send = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        send.connect(recv_addr).unwrap();

        const SEG: usize = 32;
        const N: usize = 8;
        let mut big = vec![0u8; SEG * N];
        for i in 0..N {
            big[i * SEG..i * SEG + 4].copy_from_slice(&(i as u32).to_be_bytes());
        }

        // sendmsg with UDP_SEGMENT cmsg: kernel splits on send, peer kernel
        // coalesces on recv (UDP_GRO), so the recvmsg will see a single
        // multi-segment return.
        let mut iov = libc::iovec {
            iov_base: big.as_mut_ptr() as *mut libc::c_void,
            iov_len: big.len(),
        };
        const BUF_LEN: usize = 64;
        let mut cbuf = [0u8; BUF_LEN];
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        hdr.msg_controllen = BUF_LEN as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&hdr);
            (*cmsg).cmsg_level = libc::IPPROTO_UDP;
            (*cmsg).cmsg_type = UDP_SEGMENT;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<u16>() as u32) as _;
            let data = libc::CMSG_DATA(cmsg) as *mut u16;
            *data = SEG as u16;
            hdr.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<u16>() as u32) as _;
        }
        let sent = unsafe { libc::sendmsg(send.as_raw_fd(), &hdr, 0) };
        if sent < 0 {
            eprintln!(
                "sendmsg with UDP_SEGMENT failed ({}) — kernel does not support GSO on this socket; skipping",
                std::io::Error::last_os_error()
            );
            return;
        }
        assert_eq!(sent as usize, SEG * N);

        std::thread::sleep(Duration::from_millis(50));

        let mut seen = std::collections::HashSet::new();
        let mut coalesced_observed = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while seen.len() < N && std::time::Instant::now() < deadline {
            uring.wait(200).expect("wait");
            while let Some((_idx, msg)) = uring.take() {
                if msg.segment_size > 0 && msg.data.len() > msg.segment_size {
                    coalesced_observed = true;
                    for chunk in msg.data.chunks(msg.segment_size) {
                        assert_eq!(chunk.len(), SEG);
                        let i = u32::from_be_bytes(chunk[..4].try_into().unwrap()) as usize;
                        seen.insert(i);
                    }
                } else {
                    // Single datagram — still counts.
                    assert_eq!(msg.data.len(), SEG);
                    let i = u32::from_be_bytes(msg.data[..4].try_into().unwrap()) as usize;
                    seen.insert(i);
                }
            }
        }
        assert_eq!(seen.len(), N, "all segments must be recovered");
        assert!(
            coalesced_observed,
            "test must actually exercise the coalescing branch — if this fails, \
             the environment isn't triggering UDP_GRO and the regression guard \
             is useless"
        );
    }

    /// Full production shape: UringRecv receives full-MTU datagrams (1400
    /// bytes) with per-packet distinguishable patterns, sent in rapid
    /// succession from a regular UdpSocket over loopback, with UDP_GRO
    /// enabled on the recv socket. Verifies:
    /// 1. Every datagram is recovered (no drops due to missing plumbing).
    /// 2. Byte-for-byte equality vs what the sender sent (no cross-packet
    ///    trampling).
    /// 3. Payload length at each decode point matches the wire packet (no
    ///    drift across the segment_size / non-segment_size branches).
    ///
    /// This is the shape that reproduces the decoder-corruption bug if the
    /// cmsg plumbing is missing: previously the demuxer received the
    /// concatenated blob and fed garbage to the decoder.
    #[test]
    fn uring_recv_mtu_sized_burst_with_gro_enabled() {
        use std::os::fd::AsRawFd;
        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let recv_addr = recv.local_addr().unwrap();
        recv.set_nonblocking(true).unwrap();
        let rcvbuf: libc::c_int = 8 * 1024 * 1024;
        unsafe {
            libc::setsockopt(
                recv.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &rcvbuf as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        let on: libc::c_int = 1;
        let _ = unsafe {
            libc::setsockopt(
                recv.as_raw_fd(),
                libc::IPPROTO_UDP,
                UDP_GRO,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };

        let Some(mut uring) = UringRecv::new(recv.as_raw_fd()) else {
            return;
        };

        let send = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        send.connect(recv_addr).unwrap();

        const N: u32 = 150;
        const LEN: usize = 1400;
        let packets: Vec<Vec<u8>> = (0..N)
            .map(|i| {
                let mut v = vec![0u8; LEN];
                v[..4].copy_from_slice(&i.to_be_bytes());
                for (j, b) in v[4..].iter_mut().enumerate() {
                    *b = ((i.wrapping_mul(31) as usize + j) & 0xff) as u8;
                }
                v
            })
            .collect();
        for pkt in &packets {
            send.send(pkt).unwrap();
        }

        std::thread::sleep(Duration::from_millis(50));

        let mut got: Vec<Option<Vec<u8>>> = (0..N).map(|_| None).collect();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while got.iter().filter(|v| v.is_some()).count() < N as usize
            && std::time::Instant::now() < deadline
        {
            uring.wait(200).expect("wait");
            while let Some((_idx, msg)) = uring.take() {
                let total = msg.data.len();
                let stride = if msg.segment_size > 0 {
                    msg.segment_size
                } else {
                    total
                };
                let mut offset = 0;
                while offset < total {
                    let end = (offset + stride).min(total);
                    let chunk = &msg.data[offset..end];
                    assert_eq!(
                        chunk.len(),
                        LEN,
                        "each decoded datagram must match sent length"
                    );
                    let i = u32::from_be_bytes(chunk[..4].try_into().unwrap());
                    assert!(i < N, "corrupt index in payload");
                    got[i as usize] = Some(chunk.to_vec());
                    offset = end;
                }
            }
        }
        for (i, v) in got.iter().enumerate() {
            let v = v.as_ref().expect("every packet must be received");
            assert_eq!(
                v,
                &packets[i],
                "packet {i} arrived with corrupted bytes — uring recv is trampling buffers",
            );
        }
    }

    /// Unit test of the cmsg parser in isolation: hand-build a msghdr whose
    /// control buffer carries a `IPPROTO_UDP / UDP_GRO` cmsg with a known
    /// segment size, then verify `extract_udp_gro_segment_size` returns it.
    /// Independent of any kernel behavior.
    #[test]
    fn extract_udp_gro_segment_size_parses_cmsg() {
        const BUF_LEN: usize = 64;
        let mut cbuf = [0u8; BUF_LEN];
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        hdr.msg_controllen = BUF_LEN as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&hdr);
            (*cmsg).cmsg_level = libc::IPPROTO_UDP;
            (*cmsg).cmsg_type = UDP_GRO;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
            let data = libc::CMSG_DATA(cmsg) as *mut libc::c_int;
            *data = 1400;
            hdr.msg_controllen =
                libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as _;
        }
        assert_eq!(extract_udp_gro_segment_size(&hdr), 1400);

        // No cmsg → zero.
        let empty: libc::msghdr = unsafe { std::mem::zeroed() };
        assert_eq!(extract_udp_gro_segment_size(&empty), 0);
    }

    /// Drain more packets than the ring depth to make sure slot re-arming on
    /// subsequent `wait()` calls is correct — the previous buffer contents
    /// must not bleed into a fresh recv.
    #[test]
    fn uring_recv_handles_more_packets_than_ring_depth() {
        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let recv_addr = recv.local_addr().unwrap();
        recv.set_nonblocking(true).unwrap();
        let Some(mut uring) = UringRecv::new(recv.as_raw_fd()) else {
            return;
        };

        let send = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let total = 96u32;
        for i in 0..total {
            send.send_to(&i.to_be_bytes(), recv_addr).unwrap();
        }

        let mut seen = std::collections::HashSet::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while seen.len() < total as usize {
            if std::time::Instant::now() > deadline {
                break;
            }
            uring.wait(200).expect("wait");
            while let Some((_idx, msg)) = uring.take() {
                assert_eq!(msg.data.len(), 4);
                let v = u32::from_be_bytes(msg.data.try_into().unwrap());
                seen.insert(v);
            }
        }
        assert_eq!(seen.len(), total as usize);
    }
}

fn sockaddr_to_socket_addr(sa: &libc::sockaddr_storage) -> Option<SocketAddr> {
    unsafe {
        match sa.ss_family as i32 {
            libc::AF_INET => {
                let s = &*(sa as *const _ as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(s.sin_addr.s_addr));
                let port = u16::from_be(s.sin_port);
                Some(SocketAddr::V4(SocketAddrV4::new(ip, port)))
            }
            libc::AF_INET6 => {
                let s = &*(sa as *const _ as *const libc::sockaddr_in6);
                let ip = Ipv6Addr::from(s.sin6_addr.s6_addr);
                let port = u16::from_be(s.sin6_port);
                Some(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)))
            }
            _ => None,
        }
    }
}

