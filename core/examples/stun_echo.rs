//! Minimal STUN Binding responder for offline NAT test beds
//! (`scripts/punch-nat-e2e.sh`): answers every request with the
//! XOR-MAPPED-ADDRESS it was sent from.
//!
//! ```text
//! cargo run --example stun_echo -- 10.0.0.1:3478 10.0.0.1:3479
//! ```

use std::net::{SocketAddr, UdpSocket};

const MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];

fn serve(socket: UdpSocket) {
    let mut request = [0u8; 512];
    loop {
        let Ok((len, SocketAddr::V4(from))) = socket.recv_from(&mut request) else {
            continue;
        };
        if len < 20 || request[..2] != [0x00, 0x01] || request[4..8] != MAGIC_COOKIE {
            continue;
        }
        let mut reply = [0u8; 32];
        reply[..4].copy_from_slice(&[0x01, 0x01, 0x00, 0x0c]);
        reply[4..20].copy_from_slice(&request[4..20]);
        reply[20..25].copy_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00]);
        reply[25] = 0x01;
        reply[26..28].copy_from_slice(&(from.port() ^ 0x2112).to_be_bytes());
        for (out, (ip, mask)) in reply[28..]
            .iter_mut()
            .zip(from.ip().octets().into_iter().zip(MAGIC_COOKIE))
        {
            *out = ip ^ mask;
        }
        let _ = socket.send_to(&reply, from);
    }
}

fn main() {
    let workers: Vec<_> = std::env::args()
        .skip(1)
        .map(|address| {
            let socket = UdpSocket::bind(&address).expect("bind STUN address");
            std::thread::spawn(move || serve(socket))
        })
        .collect();
    assert!(!workers.is_empty(), "usage: stun_echo <ip:port>...");
    for worker in workers {
        let _ = worker.join();
    }
}
