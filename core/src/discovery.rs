use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DISCOVERY_PORT: u16 = 28_481;
const DISCOVERY_EXPIRY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredServer {
    pub hostname: String,
    pub address: String,
    pub token: String,
    pub peer_id: String,
}

struct Entry {
    server: DiscoveredServer,
    last_seen: Instant,
}

pub struct LanDiscovery {
    servers: Arc<Mutex<Vec<Entry>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl LanDiscovery {
    pub fn start() -> Result<Self, String> {
        let socket = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT))
            .map_err(|error| format!("failed to bind LAN discovery: {error}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(|error| format!("failed to configure LAN discovery: {error}"))?;
        let servers = Arc::new(Mutex::new(Vec::<Entry>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_servers = Arc::clone(&servers);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("st-lan-discovery".into())
            .spawn(move || run_discovery(socket, worker_servers, worker_stop))
            .map_err(|error| format!("failed to start LAN discovery: {error}"))?;
        Ok(Self {
            servers,
            stop,
            worker: Some(worker),
        })
    }

    pub fn snapshot(&self, token: Option<&str>) -> Vec<DiscoveredServer> {
        let Some(token) = token.filter(|value| !value.is_empty()) else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut servers = self.servers.lock().unwrap();
        servers.retain(|entry| now.duration_since(entry.last_seen) <= DISCOVERY_EXPIRY);
        let mut snapshot: Vec<_> = servers
            .iter()
            .filter(|entry| entry.server.token == token)
            .map(|entry| entry.server.clone())
            .collect();
        snapshot.sort_by(|left, right| {
            left.hostname
                .to_ascii_lowercase()
                .cmp(&right.hostname.to_ascii_lowercase())
                .then_with(|| left.address.cmp(&right.address))
        });
        snapshot
    }
}

impl Drop for LanDiscovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake recv_from so lifecycle shutdown does not wait for the read timeout.
        if let Ok(waker) = UdpSocket::bind(("127.0.0.1", 0)) {
            let _ = waker.send_to(&[], ("127.0.0.1", DISCOVERY_PORT));
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_discovery(socket: UdpSocket, servers: Arc<Mutex<Vec<Entry>>>, stop: Arc<AtomicBool>) {
    let mut buffer = [0u8; 2048];
    while !stop.load(Ordering::Acquire) {
        match socket.recv_from(&mut buffer) {
            Ok((size, source)) => {
                let Some(server) = parse_beacon(&buffer[..size], source) else {
                    continue;
                };
                let now = Instant::now();
                let mut servers = servers.lock().unwrap();
                if let Some(entry) = servers.iter_mut().find(|entry| {
                    (!server.peer_id.is_empty() && entry.server.peer_id == server.peer_id)
                        || entry.server.address == server.address
                }) {
                    entry.server = server;
                    entry.last_seen = now;
                } else {
                    servers.push(Entry {
                        server,
                        last_seen: now,
                    });
                }
                servers.retain(|entry| now.duration_since(entry.last_seen) <= DISCOVERY_EXPIRY);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                eprintln!("[client-core] LAN discovery stopped: {error}");
                break;
            }
        }
    }
}

fn parse_beacon(data: &[u8], source: SocketAddr) -> Option<DiscoveredServer> {
    let text = std::str::from_utf8(data).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "ST_DISCOVER" {
        return None;
    }
    let hostname = lines.next()?.trim();
    let port = lines.next()?.trim().parse::<u16>().ok()?;
    let token = lines.next().unwrap_or_default().trim();
    let peer_id = lines.next().unwrap_or_default().trim();
    if hostname.is_empty() || peer_id.is_empty() {
        return None;
    }
    Some(DiscoveredServer {
        hostname: hostname.to_string(),
        address: SocketAddr::new(source.ip(), port).to_string(),
        token: token.to_string(),
        peer_id: peer_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_discovery_beacon() {
        let server = parse_beacon(
            b"ST_DISCOVER\nhost-pc\n28480\nsecret\npeer-123\n",
            "192.168.1.20:28481".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(server.hostname, "host-pc");
        assert_eq!(server.address, "192.168.1.20:28480");
        assert_eq!(server.token, "secret");
        assert_eq!(server.peer_id, "peer-123");
    }

    #[test]
    fn rejects_unrelated_datagram() {
        assert!(parse_beacon(
            b"not-st\nhost\n28480\n",
            "192.168.1.20:28481".parse().unwrap(),
        )
        .is_none());
    }

    #[test]
    fn rejects_beacon_without_peer_identity() {
        assert!(parse_beacon(
            b"ST_DISCOVER\nhost\n28480\nsecret\n",
            "192.168.1.20:28481".parse().unwrap(),
        )
        .is_none());
    }

    #[test]
    fn empty_token_never_exposes_discovered_servers() {
        let discovery = LanDiscovery {
            servers: Arc::new(Mutex::new(vec![Entry {
                server: DiscoveredServer {
                    hostname: "host".into(),
                    address: "192.168.1.20:28480".into(),
                    token: "secret".into(),
                    peer_id: "peer-123".into(),
                },
                last_seen: Instant::now(),
            }])),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        };
        assert!(discovery.snapshot(None).is_empty());
        assert!(discovery.snapshot(Some("")).is_empty());
        assert_eq!(discovery.snapshot(Some("secret")).len(), 1);
    }
}
