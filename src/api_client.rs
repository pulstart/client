use st_protocol::tunnel::{CryptoContext, TunnelKeys};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    tunnel_keys: Mutex<TunnelKeys>,
    /// Derived ChaCha20 shared key from the latest partner key, if available.
    shared_key: Mutex<Option<[u8; 32]>>,
    /// Partner (host) NAT candidates parsed as SocketAddr.
    pub partner_candidates: Mutex<Vec<SocketAddr>>,
    /// Process-lifetime UDP socket used for STUN and hole punching.
    punch_socket: Mutex<Option<UdpSocket>>,
    /// Local candidates advertised to the API server (ip:port strings).
    pub punch_candidates: Mutex<Vec<String>>,
    /// Monotonic punch-request nonce sent to the API server.
    next_punch_nonce: AtomicU64,
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
            tunnel_keys: Mutex::new(TunnelKeys::generate()),
            shared_key: Mutex::new(None),
            partner_candidates: Mutex::new(Vec::new()),
            punch_socket: Mutex::new(None),
            punch_candidates: Mutex::new(Vec::new()),
            next_punch_nonce: AtomicU64::new(1),
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

    /// Clone the process-lifetime punch socket for an individual attempt/session.
    pub fn clone_punch_socket(&self) -> Result<UdpSocket, String> {
        self.ensure_punch_socket()?;
        self.punch_socket
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| "punch socket unavailable".to_string())?
            .try_clone()
            .map_err(|e| format!("clone punch socket: {e}"))
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn public_key_b64(&self) -> String {
        let keys = self.tunnel_keys.lock().unwrap();
        base64_encode(&keys.public_key_bytes())
    }

    fn set_shared_key(&self, shared_key: Option<[u8; 32]>) {
        let mut current = self.shared_key.lock().unwrap();
        if *current != shared_key {
            let had_key = current.is_some();
            let has_key = shared_key.is_some();
            *current = shared_key;
            if has_key && !had_key {
                eprintln!("[api] Shared key derived");
            }
        }
    }

    fn update_shared_key_from_partner_b64(&self, partner_b64: Option<&str>) {
        let Some(partner_b64) = partner_b64 else {
            self.set_shared_key(None);
            return;
        };
        let Some(partner_bytes) = base64_decode(partner_b64) else {
            self.set_shared_key(None);
            return;
        };
        if partner_bytes.len() != 32 {
            self.set_shared_key(None);
            return;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&partner_bytes);
        let shared_key = {
            let keys = self.tunnel_keys.lock().unwrap();
            keys.derive_shared_key(&arr)
        };
        self.set_shared_key(Some(shared_key));
    }

    fn ensure_punch_socket(&self) -> Result<Vec<String>, String> {
        let has_socket = self.punch_socket.lock().unwrap().is_some();
        if has_socket {
            let candidates = self.punch_candidates.lock().unwrap().clone();
            if !candidates.is_empty() {
                return Ok(candidates);
            }
        }

        let mut socket_guard = self.punch_socket.lock().unwrap();
        if socket_guard.is_none() {
            let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind punch socket: {e}"))?;
            *socket_guard = Some(socket);
        }

        let socket = socket_guard
            .as_ref()
            .ok_or_else(|| "punch socket unavailable".to_string())?;
        let port = socket
            .local_addr()
            .map_err(|e| format!("punch socket local_addr: {e}"))?
            .port();
        let candidates = st_protocol::tunnel::gather_candidates_with_stun(port, Some(socket));
        drop(socket_guard);

        *self.punch_candidates.lock().unwrap() = candidates.clone();
        Ok(candidates)
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
        let peer_id = shared.peer_id.lock().unwrap().clone();
        let mut failures: u32 = 0;

        if let Err(e) = shared.ensure_punch_socket() {
            eprintln!("[api] Failed to prepare punch socket: {e}");
        }

        loop {
            let url = shared.api_url.lock().unwrap().clone();
            let token = shared.token.lock().unwrap().clone();

            if url.is_empty() || token.is_empty() {
                shared.connected.store(false, Ordering::Relaxed);
                let changed = clear_host(&shared);
                if changed {
                    ctx.request_repaint();
                }
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }

            let local_candidates = match shared.ensure_punch_socket() {
                Ok(candidates) => candidates,
                Err(e) => {
                    eprintln!("[api] Failed to prepare punch socket: {e}");
                    shared.punch_candidates.lock().unwrap().clear();
                    Vec::new()
                }
            };

            // 1. Register as client — this is the connectivity check.
            let reg_body = serde_json::json!({
                "token": token,
                "role": "client",
                "peer_id": peer_id,
                "candidates": local_candidates,
            })
            .to_string();
            let ok = ureq::post(&format!("{url}/api/register"))
                .set("Content-Type", "application/json")
                .send_string(&reg_body)
                .is_ok();

            if !ok {
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

            if failures > 0 || !shared.is_connected() {
                eprintln!("[api] Connected to API server");
            }
            failures = 0;
            let was_disconnected = !shared.connected.swap(true, Ordering::Relaxed);
            if was_disconnected {
                ctx.request_repaint();
            }

            // 2. Upload our public key and try to get host's key.
            let key_body = serde_json::json!({
                "token": token,
                "role": "client",
                "public_key": shared.public_key_b64(),
            })
            .to_string();
            match ureq::post(&format!("{url}/api/key"))
                .set("Content-Type", "application/json")
                .send_string(&key_body)
            {
                Ok(resp) => {
                    if let Ok(text) = resp.into_string() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                            shared.update_shared_key_from_partner_b64(v["partner_key"].as_str());
                        } else {
                            shared.set_shared_key(None);
                        }
                    } else {
                        shared.set_shared_key(None);
                    }
                }
                Err(_) => shared.set_shared_key(None),
            }

            // 3. Fetch fresh partner candidates.
            let cand_body = serde_json::json!({
                "token": token,
                "role": "client",
                "candidates": local_candidates,
            })
            .to_string();
            match ureq::post(&format!("{url}/api/candidates"))
                .set("Content-Type", "application/json")
                .send_string(&cand_body)
            {
                Ok(resp) => {
                    if let Ok(text) = resp.into_string() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                            let addrs: Vec<SocketAddr> = v["partner_candidates"]
                                .as_array()
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|value| value.as_str()?.parse().ok())
                                        .collect()
                                })
                                .unwrap_or_default();
                            *shared.partner_candidates.lock().unwrap() = addrs;
                        } else {
                            shared.partner_candidates.lock().unwrap().clear();
                        }
                    } else {
                        shared.partner_candidates.lock().unwrap().clear();
                    }
                }
                Err(_) => shared.partner_candidates.lock().unwrap().clear(),
            }

            // 4. Poll session status for UI.
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
                Err(_) => clear_host(&shared),
            };

            if changed {
                ctx.request_repaint();
            }

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

/// Refresh candidates and key material immediately before a punched connection attempt.
pub fn prepare_punch_attempt(
    shared: &ApiDiscoveryShared,
) -> Result<(Vec<SocketAddr>, Arc<CryptoContext>), String> {
    let url = shared.api_url.lock().unwrap().clone();
    let token = shared.token.lock().unwrap().clone();
    let peer_id = shared.peer_id.lock().unwrap().clone();
    if url.is_empty() || token.is_empty() {
        return Err("API discovery is not configured".into());
    }

    let local_candidates = shared.ensure_punch_socket()?;

    let reg_body = serde_json::json!({
        "token": token,
        "role": "client",
        "peer_id": peer_id,
        "candidates": local_candidates,
    })
    .to_string();
    ureq::post(&format!("{url}/api/register"))
        .set("Content-Type", "application/json")
        .send_string(&reg_body)
        .map_err(|e| format!("register with API: {e}"))?;

    let key_body = serde_json::json!({
        "token": token,
        "role": "client",
        "public_key": shared.public_key_b64(),
    })
    .to_string();
    let key_resp = ureq::post(&format!("{url}/api/key"))
        .set("Content-Type", "application/json")
        .send_string(&key_body)
        .map_err(|e| format!("exchange tunnel key: {e}"))?;
    let key_text = key_resp
        .into_string()
        .map_err(|e| format!("read key response: {e}"))?;
    let key_json: serde_json::Value =
        serde_json::from_str(&key_text).map_err(|e| format!("parse key response: {e}"))?;
    shared.update_shared_key_from_partner_b64(key_json["partner_key"].as_str());
    let crypto = shared
        .crypto_context()
        .ok_or_else(|| "shared tunnel key not ready".to_string())?;

    let cand_body = serde_json::json!({
        "token": token,
        "role": "client",
        "candidates": local_candidates,
    })
    .to_string();
    let cand_resp = ureq::post(&format!("{url}/api/candidates"))
        .set("Content-Type", "application/json")
        .send_string(&cand_body)
        .map_err(|e| format!("refresh punch candidates: {e}"))?;
    let cand_text = cand_resp
        .into_string()
        .map_err(|e| format!("read candidates response: {e}"))?;
    let cand_json: serde_json::Value = serde_json::from_str(&cand_text)
        .map_err(|e| format!("parse candidates response: {e}"))?;
    let partner_candidates: Vec<SocketAddr> = cand_json["partner_candidates"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|value| value.as_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    if partner_candidates.is_empty() {
        return Err("API session does not have any host punch candidates yet".into());
    }
    *shared.partner_candidates.lock().unwrap() = partner_candidates.clone();

    let punch_nonce = shared.next_punch_nonce.fetch_add(1, Ordering::Relaxed);
    let punch_body = serde_json::json!({
        "token": token,
        "role": "client",
        "nonce": punch_nonce,
    })
    .to_string();
    ureq::post(&format!("{url}/api/punch"))
        .set("Content-Type", "application/json")
        .send_string(&punch_body)
        .map_err(|e| format!("request punch from server: {e}"))?;

    Ok((partner_candidates, crypto))
}

fn parse_session_status(shared: &ApiDiscoveryShared, json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return clear_host(shared);
    };

    let host_joined = v["host_joined"].as_bool().unwrap_or(false);
    let mut host_guard = shared.host.lock().unwrap();
    if host_joined {
        let mut candidates: Vec<String> = v["host_candidates"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|value| value.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        sort_candidates(&mut candidates);

        let hostname = v["host_hostname"].as_str().map(String::from);
        let peer_id = v["host_peer_id"].as_str().map(String::from);

        let changed = match host_guard.as_ref() {
            Some(existing) => {
                existing.candidates != candidates
                    || existing.hostname != hostname
                    || existing.peer_id != peer_id
            }
            None => true,
        };
        *host_guard = Some(ApiDiscoveredHost {
            candidates,
            hostname,
            peer_id,
            last_seen: Instant::now(),
        });
        changed
    } else {
        let was_some = host_guard.is_some();
        drop(host_guard);
        if was_some {
            clear_host(shared);
        }
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

/// Immediately clear partner-derived state. Returns true if the visible host changed.
fn clear_host(shared: &ApiDiscoveryShared) -> bool {
    shared.partner_candidates.lock().unwrap().clear();
    shared.set_shared_key(None);
    let mut host_guard = shared.host.lock().unwrap();
    if host_guard.is_some() {
        *host_guard = None;
        true
    } else {
        false
    }
}
