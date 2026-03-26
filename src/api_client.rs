use st_protocol::tunnel::{CryptoContext, TunnelKeys};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const API_EXPIRY: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct ApiDiscoveredHost {
    /// Address candidates advertised by the host via the API server.
    pub candidates: Vec<String>,
    pub last_seen: Instant,
}

/// Shared state between the API discovery thread and the UI / connection flow.
pub struct ApiDiscoveryShared {
    pub api_url: Mutex<String>,
    pub token: Mutex<String>,
    pub host: Mutex<Option<ApiDiscoveredHost>>,
    /// Derived ChaCha20 shared key (set once key exchange completes).
    pub shared_key: Mutex<Option<[u8; 32]>>,
    /// Partner (host) NAT candidates parsed as SocketAddr.
    pub partner_candidates: Mutex<Vec<SocketAddr>>,
    /// Whether the last API request succeeded.
    pub connected: AtomicBool,
}

impl ApiDiscoveryShared {
    pub fn new(api_url: String, token: String) -> Self {
        Self {
            api_url: Mutex::new(api_url),
            token: Mutex::new(token),
            host: Mutex::new(None),
            shared_key: Mutex::new(None),
            partner_candidates: Mutex::new(Vec::new()),
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
        let mut failures: u32 = 0;

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
            let reg_body =
                format!(r#"{{"token":"{token}","role":"client","candidates":[]}}"#);
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
            failures = 0;
            let was_disconnected = !shared.connected.swap(true, Ordering::Relaxed);
            if was_disconnected {
                ctx.request_repaint();
            }

            // 2. Upload our public key and try to get host's key
            let key_body = format!(
                r#"{{"token":"{token}","role":"client","public_key":"{pub_key_b64}"}}"#,
            );
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
            let cand_body =
                format!(r#"{{"token":"{token}","role":"client","candidates":[]}}"#);
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
            let changed = match ureq::get(&format!("{url}/api/session/{token}")).call() {
                Ok(resp) => {
                    if let Ok(text) = resp.into_string() {
                        parse_session_status(&shared, &text)
                    } else {
                        expire_stale(&shared)
                    }
                }
                Err(_) => expire_stale(&shared),
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
        return expire_stale(shared);
    };

    let host_joined = v["host_joined"].as_bool().unwrap_or(false);

    let mut h = shared.host.lock().unwrap();
    if host_joined {
        let candidates: Vec<String> = v["host_candidates"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let was_none = h.is_none();
        *h = Some(ApiDiscoveredHost {
            candidates,
            last_seen: Instant::now(),
        });
        was_none
    } else {
        let was_some = h.is_some();
        *h = None;
        was_some
    }
}

fn expire_stale(shared: &ApiDiscoveryShared) -> bool {
    let mut h = shared.host.lock().unwrap();
    if h.as_ref()
        .map(|host| host.last_seen.elapsed() > API_EXPIRY)
        .unwrap_or(false)
    {
        *h = None;
        true
    } else {
        false
    }
}
