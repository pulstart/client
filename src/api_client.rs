use st_protocol::tunnel::{CryptoContext, TunnelKeys};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};


#[derive(Clone, Debug)]
pub struct ApiDiscoveredHost {
    /// Address candidates advertised by the host, sorted: local first, then VPN, then public.
    pub candidates: Vec<String>,
    /// Hostname reported by the host.
    pub hostname: Option<String>,
    /// Stable host identifier from the signaling service.
    pub peer_id: Option<String>,
    pub last_seen: Instant,
}

/// Sort candidates: private LAN first (192.168/172.16-31), then VPN/CGNAT (10.x/100.64-127),
/// then everything else (public).
fn sort_candidates(candidates: &mut [String]) {
    fn priority(addr: &str) -> u8 {
        let ip_part = addr.rsplit_once(':').map(|(ip, _)| ip).unwrap_or(addr);
        if let Ok(ip) = ip_part.parse::<std::net::Ipv4Addr>() {
            let o = ip.octets();
            // 192.168.x.x or 172.16-31.x.x — local LAN
            if o[0] == 192 && o[1] == 168 {
                return 0;
            }
            if o[0] == 172 && (16..=31).contains(&o[1]) {
                return 0;
            }
            // 10.x.x.x or 100.64-127.x.x — VPN / CGNAT
            if o[0] == 10 {
                return 1;
            }
            if o[0] == 100 && (64..=127).contains(&o[1]) {
                return 1;
            }
            // 169.254.x.x — link-local
            if o[0] == 169 && o[1] == 254 {
                return 2;
            }
        }
        // Public or unknown
        3
    }
    candidates.sort_by_key(|c| priority(c));
}

/// Shared state between the API discovery thread and the UI / connection flow.
pub struct ApiDiscoveryShared {
    pub api_url: Mutex<String>,
    pub token: Mutex<String>,
    pub peer_id: Mutex<String>,
    pub host: Mutex<Option<ApiDiscoveredHost>>,
    /// Derived ChaCha20 shared key (set once key exchange completes).
    pub shared_key: Mutex<Option<[u8; 32]>>,
    /// Partner (host) NAT candidates parsed as SocketAddr.
    pub partner_candidates: Mutex<Vec<SocketAddr>>,
    /// Pre-bound UDP socket for hole punching (taken once by the connection flow).
    pub punch_socket: Mutex<Option<UdpSocket>>,
    /// Local candidates advertised to the API server (ip:port strings).
    pub punch_candidates: Mutex<Vec<String>>,
    /// Whether the last API request succeeded.
    pub connected: AtomicBool,
}

impl ApiDiscoveryShared {
    pub fn new(api_url: String, token: String, peer_id: String) -> Self {
        Self {
            api_url: Mutex::new(api_url),
            token: Mutex::new(token),
            peer_id: Mutex::new(peer_id),
            host: Mutex::new(None),
            shared_key: Mutex::new(None),
            partner_candidates: Mutex::new(Vec::new()),
            punch_socket: Mutex::new(None),
            punch_candidates: Mutex::new(Vec::new()),
            connected: AtomicBool::new(false),
        }
    }

    /// Build a CryptoContext for the client side if a shared key has been negotiated.
    pub fn crypto_context(&self) -> Option<Arc<CryptoContext>> {
        self.shared_key
            .lock()
            .unwrap()
            .map(|key| Arc::new(CryptoContext::new(key, false)))
    }

    /// Take the pre-bound punch socket for use in hole punching (one-shot).
    pub fn take_punch_socket(&self) -> Option<UdpSocket> {
        self.punch_socket.lock().unwrap().take()
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

fn retry_interval(consecutive_failures: u32) -> Duration {
    match consecutive_failures {
        0 => Duration::from_secs(10),
        1 => Duration::from_secs(30),
        _ => Duration::from_secs(60),
    }
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s = s.trim_end_matches('=');
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let a = val(*chunk.first()?)?;
        let b = val(chunk.get(1).copied()?)?;
        out.push((a << 2) | (b >> 4));
        if let Some(&c) = chunk.get(2) {
            let c = val(c)?;
            out.push((b << 4) | (c >> 2));
            if let Some(&d) = chunk.get(3) {
                let d = val(d)?;
                out.push((c << 6) | d);
            }
        }
    }
    Some(out)
}

/// Spawn a background thread that:
/// 1. Registers as "client" with the API server.
/// 2. Polls for the host to appear.
/// 3. Performs X25519 key exchange when the host is online.
/// 4. Stores the derived shared key and partner candidates.
pub fn start_api_discovery(shared: Arc<ApiDiscoveryShared>, ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let keys = TunnelKeys::generate();
        let pub_key_b64 = base64_encode(&keys.public_key_bytes());
        let keys = Mutex::new(Some(keys));
        let peer_id = shared.peer_id.lock().unwrap().clone();
        let mut failures: u32 = 0;

        // Bind a UDP socket for hole punching and gather local candidates.
        let punch_port = match UdpSocket::bind("0.0.0.0:0") {
            Ok(sock) => {
                let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
                *shared.punch_socket.lock().unwrap() = Some(sock);
                port
            }
            Err(e) => {
                eprintln!("[api] Failed to bind punch socket: {e}");
                0
            }
        };
        // Gather local candidates + discover public IP via STUN on the punch socket.
        let local_candidates = if punch_port > 0 {
            let sock_guard = shared.punch_socket.lock().unwrap();
            st_protocol::tunnel::gather_candidates_with_stun(
                punch_port,
                sock_guard.as_ref(),
            )
        } else {
            Vec::new()
        };
        *shared.punch_candidates.lock().unwrap() = local_candidates.clone();

        loop {
            let url = shared.api_url.lock().unwrap().clone();
            let token = shared.token.lock().unwrap().clone();

            if url.is_empty() || token.is_empty() {
                shared.connected.store(false, Ordering::Relaxed);
                let mut h = shared.host.lock().unwrap();
                if h.is_some() {
                    *h = None;
                    ctx.request_repaint();
                }
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }

            // 1. Register as client — this is the connectivity check.
            let reg_body = serde_json::json!({
                "token": token,
                "role": "client",
                "peer_id": peer_id,
                "candidates": local_candidates,
            }).to_string();
            let ok = ureq::post(&format!("{url}/api/register"))
                .set("Content-Type", "application/json")
                .send_string(&reg_body)
                .is_ok();

            if !ok {
                // Failed — backoff retry.
                let was_connected = shared.connected.swap(false, Ordering::Relaxed);
                if was_connected {
                    ctx.request_repaint();
                }
                let wait = retry_interval(failures);
                failures = failures.saturating_add(1);
                eprintln!("[api] Registration failed, retrying in {}s", wait.as_secs());
                std::thread::sleep(wait);
                continue;
            }

            // Connected.
            if failures > 0 || !shared.is_connected() {
                eprintln!("[api] Connected to API server");
            }
            failures = 0;
            let was_disconnected = !shared.connected.swap(true, Ordering::Relaxed);
            if was_disconnected {
                ctx.request_repaint();
            }

            // 2. Upload our public key and try to get host's key
            let key_body = serde_json::json!({
                "token": token,
                "role": "client",
                "public_key": pub_key_b64,
            }).to_string();
            if let Ok(resp) = ureq::post(&format!("{url}/api/key"))
                .set("Content-Type", "application/json")
                .send_string(&key_body)
            {
                if let Ok(text) = resp.into_string() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(partner_b64) = v["partner_key"].as_str() {
                            if let Some(partner_bytes) = base64_decode(partner_b64) {
                                if partner_bytes.len() == 32 {
                                    let mut arr = [0u8; 32];
                                    arr.copy_from_slice(&partner_bytes);
                                    if let Some(k) = keys.lock().unwrap().take() {
                                        let shared_key = k.derive_shared_key(&arr);
                                        *shared.shared_key.lock().unwrap() = Some(shared_key);
                                        eprintln!("[api] Shared key derived");
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // 3. Fetch candidates
            let cand_body = serde_json::json!({
                "token": token,
                "role": "client",
                "candidates": local_candidates,
            }).to_string();
            if let Ok(resp) = ureq::post(&format!("{url}/api/candidates"))
                .set("Content-Type", "application/json")
                .send_string(&cand_body)
            {
                if let Ok(text) = resp.into_string() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(arr) = v["partner_candidates"].as_array() {
                            let addrs: Vec<SocketAddr> = arr
                                .iter()
                                .filter_map(|v| v.as_str()?.parse().ok())
                                .collect();
                            if !addrs.is_empty() {
                                *shared.partner_candidates.lock().unwrap() = addrs;
                            }
                        }
                    }
                }
            }

            // 4. Poll session status for UI
            let session_body = format!(r#"{{"token":"{token}"}}"#);
            let changed = match ureq::post(&format!("{url}/api/session"))
                .set("Content-Type", "application/json")
                .send_string(&session_body)
            {
                Ok(resp) => {
                    if let Ok(text) = resp.into_string() {
                        parse_session_status(&shared, &text)
                    } else {
                        clear_host(&shared)
                    }
                }
                // 404 (session not found) or any error — no host.
                Err(_) => clear_host(&shared),
            };

            if changed {
                ctx.request_repaint();
            }

            // Normal poll interval.
            let has_key = shared.shared_key.lock().unwrap().is_some();
            let interval = if has_key {
                Duration::from_secs(30)
            } else {
                Duration::from_secs(3)
            };
            std::thread::sleep(interval);
        }
    });
}

fn parse_session_status(shared: &ApiDiscoveryShared, json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return clear_host(shared);
    };

    let host_joined = v["host_joined"].as_bool().unwrap_or(false);

    let mut h = shared.host.lock().unwrap();
    if host_joined {
        let mut candidates: Vec<String> = v["host_candidates"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        sort_candidates(&mut candidates);

        let hostname = v["host_hostname"].as_str().map(String::from);
        let peer_id = v["host_peer_id"].as_str().map(String::from);

        let was_none = h.is_none();
        *h = Some(ApiDiscoveredHost {
            candidates,
            hostname,
            peer_id,
            last_seen: Instant::now(),
        });
        was_none
    } else {
        let was_some = h.is_some();
        *h = None;
        was_some
    }
}

/// Call on app exit to unregister from the API server.
pub fn unregister(shared: &ApiDiscoveryShared) {
    let url = shared.api_url.lock().unwrap().clone();
    let token = shared.token.lock().unwrap().clone();
    let peer_id = shared.peer_id.lock().unwrap().clone();
    if url.is_empty() || token.is_empty() {
        return;
    }
    let body = format!(r#"{{"token":"{token}","role":"client","peer_id":"{peer_id}"}}"#);
    let _ = ureq::post(&format!("{url}/api/unregister"))
        .set("Content-Type", "application/json")
        .send_string(&body);
    shared.connected.store(false, Ordering::Relaxed);
    eprintln!("[api] Unregistered from API server");
}

/// Immediately clear the host entry. Returns true if state changed.
fn clear_host(shared: &ApiDiscoveryShared) -> bool {
    let mut h = shared.host.lock().unwrap();
    if h.is_some() {
        *h = None;
        true
    } else {
        false
    }
}
