use rand::{rngs::OsRng, RngCore};
use st_protocol::tunnel::{
    derive_session_key, CryptoContext, SessionKeyContext, TunnelKeys, TunnelMode,
};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Shared ureq agent for all signaling calls. The default ureq agent has no
/// read timeout, so a black-holed or slow API host would hang the registration
/// thread forever — and, worse, `prepare_punch_attempt` runs inline on the
/// connect thread, so an unbounded request stalls the whole connect. Bound
/// connect/read/write so every signaling call fails fast and the caller can
/// fall back (direct-only / retry).
fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(10))
            .timeout_write(Duration::from_secs(10))
            .build()
    })
}

#[derive(Clone, Debug)]
pub struct ApiDiscoveredHost {
    /// Address candidates advertised by the host, sorted: local first, then VPN, then public.
    pub candidates: Vec<String>,
    /// Hostname reported by the host.
    pub hostname: Option<String>,
    /// Stable host identifier from the signaling service.
    pub peer_id: Option<String>,
    /// Process lease paired with `peer_id` by the signaling service.
    pub lease_id: Option<String>,
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
    /// Random identity for this process. Unlike peer_id, this is never persisted.
    pub lease_id: String,
    pub host: Mutex<Option<ApiDiscoveredHost>>,
    tunnel_keys: Mutex<TunnelKeys>,
    /// Derived ChaCha20 shared key from the latest partner key, if available.
    shared_key: Mutex<Option<[u8; 32]>>,
    /// Request-scoped CryptoContext cache. Idempotent retries reuse counters,
    /// while each API-generated request context derives a different key.
    crypto: Mutex<Option<([u8; 32], Arc<CryptoContext>)>>,
    /// Partner (host) NAT candidates parsed as SocketAddr.
    pub partner_candidates: Mutex<Vec<SocketAddr>>,
    /// Process-lifetime UDP socket used for STUN and hole punching.
    punch_socket: Mutex<Option<UdpSocket>>,
    /// Local candidates advertised to the API server (ip:port strings).
    pub punch_candidates: Mutex<Vec<String>>,
    /// Last time we refreshed `punch_candidates` via STUN. Used to age out
    /// stale NAT mappings so we don't advertise dead public ip:port pairs.
    last_stun: Mutex<Option<Instant>>,
    /// External `ip:port` granted by the router via PCP / NAT-PMP. Independent
    /// of the STUN-discovered mapping: the router gives us a static
    /// forwarding rule that survives idle periods AND works on symmetric
    /// NATs. Refreshed periodically by `start_port_mapping`.
    portmap_external: Mutex<Option<SocketAddr>>,
    /// Monotonic punch-request nonce sent to the API server.
    next_punch_nonce: AtomicU64,
    /// Monotonic TCP-relay-request nonce sent to the API server.
    next_relay_nonce: AtomicU64,
    /// TCP relay port advertised by the API server (None = relay disabled).
    relay_port: Mutex<Option<u16>>,
    /// True while a punched session owns the punch socket. STUN refresh
    /// (which would do an unauthenticated recv) is skipped while this is set.
    punch_session_active: AtomicBool,
    /// Whether the last API request succeeded.
    pub connected: AtomicBool,
}

/// Refresh STUN-derived candidates if they're older than this. UDP NAT
/// mappings typically expire after 30–120 s of silence, so 25 s leaves
/// margin to re-probe before the partner-advertised public address goes dead.
const STUN_REFRESH_TTL: Duration = Duration::from_secs(25);
const MAX_API_TOKEN_LEN: usize = 256;

fn valid_api_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_API_TOKEN_LEN
}

impl ApiDiscoveryShared {
    pub fn new(api_url: String, token: String, peer_id: String) -> Self {
        let mut lease_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut lease_bytes);
        let initial_nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        Self {
            api_url: Mutex::new(api_url),
            token: Mutex::new(token),
            peer_id: Mutex::new(peer_id),
            lease_id: base64_encode(&lease_bytes),
            host: Mutex::new(None),
            tunnel_keys: Mutex::new(TunnelKeys::generate()),
            shared_key: Mutex::new(None),
            crypto: Mutex::new(None),
            partner_candidates: Mutex::new(Vec::new()),
            punch_socket: Mutex::new(None),
            punch_candidates: Mutex::new(Vec::new()),
            last_stun: Mutex::new(None),
            portmap_external: Mutex::new(None),
            next_punch_nonce: AtomicU64::new(initial_nonce.max(1)),
            next_relay_nonce: AtomicU64::new(initial_nonce.saturating_add(1).max(1)),
            relay_port: Mutex::new(None),
            punch_session_active: AtomicBool::new(false),
            connected: AtomicBool::new(false),
        }
    }

    fn crypto_for_session_key(&self, key: [u8; 32]) -> Arc<CryptoContext> {
        let mut cache = self.crypto.lock().unwrap();
        if let Some((cached_key, ctx)) = cache.as_ref() {
            if *cached_key == key {
                return Arc::clone(ctx);
            }
        }
        let ctx = Arc::new(CryptoContext::new(key, false));
        *cache = Some((key, Arc::clone(&ctx)));
        ctx
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

    pub fn is_punch_session_active(&self) -> bool {
        self.punch_session_active.load(Ordering::Relaxed)
    }

    pub fn set_punch_session_active(&self, active: bool) {
        self.punch_session_active.store(active, Ordering::Relaxed);
    }

    /// Record (or clear) the PCP/NAT-PMP-granted external address. Called by
    /// the port-mapping renewal thread.
    pub fn set_portmap_external(&self, addr: Option<SocketAddr>) {
        let mut current = self.portmap_external.lock().unwrap();
        if *current != addr {
            *current = addr;
            if let Some(a) = addr {
                eprintln!("[portmap] External mapping acquired: {a}");
            } else {
                eprintln!("[portmap] External mapping cleared");
            }
        }
    }

    /// Local port that the punch socket is bound to, if known.
    pub fn punch_socket_port(&self) -> Option<u16> {
        let guard = self.punch_socket.lock().unwrap();
        guard
            .as_ref()
            .and_then(|s| s.local_addr().ok().map(|a| a.port()))
    }

    /// Append the router-mapped external `ip:port` to the candidate list
    /// (if any), de-duplicating against existing entries.
    fn augment_with_portmap(&self, mut candidates: Vec<String>) -> Vec<String> {
        if let Some(addr) = *self.portmap_external.lock().unwrap() {
            let c = addr.to_string();
            if !candidates.contains(&c) {
                candidates.push(c);
            }
        }
        candidates
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
        let cached = self.punch_candidates.lock().unwrap().clone();
        let stun_fresh = match *self.last_stun.lock().unwrap() {
            Some(t) => t.elapsed() < STUN_REFRESH_TTL,
            None => false,
        };
        // Reuse the cached candidate list if it's fresh OR if a live punched
        // session owns the socket (a STUN recv would steal its packets).
        let session_active = self.is_punch_session_active();
        if has_socket && !cached.is_empty() && (stun_fresh || session_active) {
            return Ok(self.augment_with_portmap(cached));
        }

        let mut socket_guard = self.punch_socket.lock().unwrap();
        if socket_guard.is_none() {
            let socket =
                UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind punch socket: {e}"))?;
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
        *self.last_stun.lock().unwrap() = Some(Instant::now());
        Ok(self.augment_with_portmap(candidates))
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
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
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

/// Spawn a background thread that maintains a PCP/NAT-PMP UDP port mapping
/// for the punch socket. When the client's router supports either protocol,
/// this gives the server a directly-reachable `ip:port` regardless of the
/// client's NAT type — sidestepping symmetric NAT issues entirely on the
/// client side. Quiet no-op if the gateway speaks neither protocol.
pub fn start_port_mapping(shared: Arc<ApiDiscoveryShared>) {
    std::thread::spawn(move || {
        // Wait for the punch socket to be bound; the renewal task can't
        // request a forward for a port that doesn't exist yet.
        let mut internal_port: u16 = loop {
            if let Some(p) = shared.punch_socket_port() {
                break p;
            }
            std::thread::sleep(Duration::from_millis(500));
        };

        let mut consecutive_failures: u32 = 0;
        loop {
            if let Some(p) = shared.punch_socket_port() {
                internal_port = p;
            }

            let next_sleep = match st_protocol::portmap::try_acquire(internal_port) {
                Some(mapping) => {
                    eprintln!(
                        "[portmap] {:?} mapping {} (lease {}s)",
                        mapping.method,
                        mapping.external_addr,
                        mapping.lifetime.as_secs()
                    );
                    shared.set_portmap_external(Some(mapping.external_addr));
                    consecutive_failures = 0;
                    // Renew at lifetime/2, clamped to [60s, 30min] so we don't
                    // spin too fast on tiny leases or wait too long on huge ones.
                    let half = mapping.lifetime / 2;
                    half.clamp(Duration::from_secs(60), Duration::from_secs(1800))
                }
                None => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    // Don't drop a previously-good mapping on a single timeout.
                    if consecutive_failures >= 2 {
                        shared.set_portmap_external(None);
                    }
                    match consecutive_failures {
                        0..=1 => Duration::from_secs(60),
                        2..=4 => Duration::from_secs(300),
                        _ => Duration::from_secs(900),
                    }
                }
            };
            std::thread::sleep(next_sleep);
        }
    });
}

pub fn start_api_discovery(shared: Arc<ApiDiscoveryShared>, ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let mut failures: u32 = 0;
        loop {
            let url = shared.api_url.lock().unwrap().clone();
            let token = shared.token.lock().unwrap().clone();

            if url.is_empty() || !valid_api_token(&token) {
                shared.connected.store(false, Ordering::Relaxed);
                let changed = clear_host(&shared);
                if changed {
                    ctx.request_repaint();
                }
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }

            // Idle discovery is deliberately token-only and read-only. It must
            // never reserve the session's sole active-client slot.
            let session_body = serde_json::json!({"token": token}).to_string();
            let result = http_agent()
                .post(&format!("{url}/api/session"))
                .set("Content-Type", "application/json")
                .send_string(&session_body);
            let (reachable, changed) = match result {
                Ok(response) => match response.into_string() {
                    Ok(text) => (true, parse_session_status(&shared, &text)),
                    Err(_) => (false, clear_host(&shared)),
                },
                Err(ureq::Error::Status(404, _)) => (true, clear_host(&shared)),
                Err(_) => (false, clear_host(&shared)),
            };
            let connection_changed =
                shared.connected.swap(reachable, Ordering::Relaxed) != reachable;
            if changed || connection_changed {
                ctx.request_repaint();
            }
            if reachable {
                if failures > 0 {
                    eprintln!("[api] Discovery connection restored");
                }
                failures = 0;
                std::thread::sleep(Duration::from_secs(3));
            } else {
                let wait = retry_interval(failures);
                failures = failures.saturating_add(1);
                std::thread::sleep(wait);
            }
        }
    });
}

fn expected_host_identity(shared: &ApiDiscoveryShared) -> Result<(String, String), String> {
    shared
        .host
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|host| Some((host.peer_id.clone()?, host.lease_id.clone()?)))
        .ok_or_else(|| "API host has no lease-bound identity; refresh discovery".to_string())
}

fn validate_partner_response(
    response: &serde_json::Value,
    expected_peer_id: &str,
    expected_lease_id: &str,
) -> Result<(), String> {
    if response["partner_peer_id"].as_str() != Some(expected_peer_id)
        || response["partner_lease_id"].as_str() != Some(expected_lease_id)
    {
        return Err("API host identity changed during signaling".into());
    }
    Ok(())
}

fn crypto_from_signal_response(
    shared: &ApiDiscoveryShared,
    response: &serde_json::Value,
    mode: TunnelMode,
    mode_label: &str,
    generation: u64,
    client_peer_id: &str,
    host_identity: (&str, &str),
) -> Result<Arc<CryptoContext>, String> {
    let (host_peer_id, host_lease_id) = host_identity;
    if response["mode"].as_str() != Some(mode_label)
        || response["generation"].as_u64() != Some(generation)
        || response["owner_peer_id"].as_str() != Some(client_peer_id)
        || response["owner_lease_id"].as_str() != Some(shared.lease_id.as_str())
        || response["partner_peer_id"].as_str() != Some(host_peer_id)
        || response["partner_lease_id"].as_str() != Some(host_lease_id)
    {
        return Err("API returned a mismatched tunnel request context".into());
    }
    let session_id = response["session_id"]
        .as_str()
        .ok_or_else(|| "API returned no tunnel session ID".to_string())?;
    let request_context = response["context"]
        .as_str()
        .ok_or_else(|| "API returned no tunnel request context".to_string())?;
    let shared_secret = shared
        .shared_key
        .lock()
        .unwrap()
        .ok_or_else(|| "shared tunnel secret not ready".to_string())?;
    let session_key = derive_session_key(
        &shared_secret,
        &SessionKeyContext {
            request_context,
            session_id,
            mode,
            generation,
            host_peer_id,
            host_lease_id,
            client_peer_id,
            client_lease_id: &shared.lease_id,
        },
    )?;
    Ok(shared.crypto_for_session_key(session_key))
}

/// Refresh candidates and key material immediately before a punched connection attempt.
pub fn prepare_punch_attempt(
    shared: &ApiDiscoveryShared,
) -> Result<(Vec<SocketAddr>, Arc<CryptoContext>), String> {
    let url = shared.api_url.lock().unwrap().clone();
    let token = shared.token.lock().unwrap().clone();
    let peer_id = shared.peer_id.lock().unwrap().clone();
    let lease_id = &shared.lease_id;
    let (expected_host_peer_id, expected_host_lease_id) = expected_host_identity(shared)?;
    if url.is_empty() || !valid_api_token(&token) {
        return Err("API discovery is not configured".into());
    }

    let local_candidates = shared.ensure_punch_socket()?;

    let reg_body = serde_json::json!({
        "token": token,
        "role": "client",
        "peer_id": peer_id,
        "lease_id": lease_id,
        "candidates": local_candidates,
        "public_key": shared.public_key_b64(),
    })
    .to_string();
    http_agent()
        .post(&format!("{url}/api/register"))
        .set("Content-Type", "application/json")
        .send_string(&reg_body)
        .map_err(|e| format!("register with API: {e}"))?;

    let key_body = serde_json::json!({
        "token": token,
        "role": "client",
        "peer_id": peer_id,
        "lease_id": lease_id,
        "expected_partner_peer_id": expected_host_peer_id,
        "expected_partner_lease_id": expected_host_lease_id,
        "public_key": shared.public_key_b64(),
    })
    .to_string();
    let key_resp = http_agent()
        .post(&format!("{url}/api/key"))
        .set("Content-Type", "application/json")
        .send_string(&key_body)
        .map_err(|e| format!("exchange tunnel key: {e}"))?;
    let key_text = key_resp
        .into_string()
        .map_err(|e| format!("read key response: {e}"))?;
    let key_json: serde_json::Value =
        serde_json::from_str(&key_text).map_err(|e| format!("parse key response: {e}"))?;
    validate_partner_response(&key_json, &expected_host_peer_id, &expected_host_lease_id)?;
    shared.update_shared_key_from_partner_b64(key_json["partner_key"].as_str());

    let cand_body = serde_json::json!({
        "token": token,
        "role": "client",
        "peer_id": peer_id,
        "lease_id": lease_id,
        "expected_partner_peer_id": expected_host_peer_id,
        "expected_partner_lease_id": expected_host_lease_id,
        "candidates": local_candidates,
    })
    .to_string();
    let cand_resp = http_agent()
        .post(&format!("{url}/api/candidates"))
        .set("Content-Type", "application/json")
        .send_string(&cand_body)
        .map_err(|e| format!("refresh punch candidates: {e}"))?;
    let cand_text = cand_resp
        .into_string()
        .map_err(|e| format!("read candidates response: {e}"))?;
    let cand_json: serde_json::Value =
        serde_json::from_str(&cand_text).map_err(|e| format!("parse candidates response: {e}"))?;
    validate_partner_response(&cand_json, &expected_host_peer_id, &expected_host_lease_id)?;
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
        "peer_id": peer_id,
        "lease_id": lease_id,
        "expected_partner_peer_id": expected_host_peer_id,
        "expected_partner_lease_id": expected_host_lease_id,
        "generation": punch_nonce,
    })
    .to_string();
    let punch_response = http_agent()
        .post(&format!("{url}/api/punch"))
        .set("Content-Type", "application/json")
        .send_string(&punch_body)
        .map_err(|e| format!("request punch from server: {e}"))?;
    let punch_text = punch_response
        .into_string()
        .map_err(|e| format!("read punch response: {e}"))?;
    let punch_json: serde_json::Value =
        serde_json::from_str(&punch_text).map_err(|e| format!("parse punch response: {e}"))?;
    let crypto = crypto_from_signal_response(
        shared,
        &punch_json,
        TunnelMode::Punch,
        "punch",
        punch_nonce,
        &peer_id,
        (&expected_host_peer_id, &expected_host_lease_id),
    )?;

    Ok((partner_candidates, crypto))
}

/// Refresh key material and signal the host to dial the API server's TCP
/// relay. Returns the crypto context, relay address, and one-use client ticket.
/// Used as the last-resort
/// fallback when both direct TCP and UDP hole punching fail (e.g. UDP is
/// blocked entirely by a firewall or proxy).
pub fn prepare_relay_attempt(
    shared: &ApiDiscoveryShared,
) -> Result<(Arc<CryptoContext>, String, String), String> {
    let url = shared.api_url.lock().unwrap().clone();
    let token = shared.token.lock().unwrap().clone();
    let peer_id = shared.peer_id.lock().unwrap().clone();
    let lease_id = &shared.lease_id;
    let (expected_host_peer_id, expected_host_lease_id) = expected_host_identity(shared)?;
    if url.is_empty() || !valid_api_token(&token) {
        return Err("API discovery is not configured".into());
    }

    // Register (liveness) — without candidates, so an existing list is kept.
    let reg_body = serde_json::json!({
        "token": token,
        "role": "client",
        "peer_id": peer_id,
        "lease_id": lease_id,
        "candidates": [],
        "public_key": shared.public_key_b64(),
    })
    .to_string();
    http_agent()
        .post(&format!("{url}/api/register"))
        .set("Content-Type", "application/json")
        .send_string(&reg_body)
        .map_err(|e| format!("register with API: {e}"))?;

    // Key exchange — the relay only ever carries ciphertext.
    let key_body = serde_json::json!({
        "token": token,
        "role": "client",
        "peer_id": peer_id,
        "lease_id": lease_id,
        "expected_partner_peer_id": expected_host_peer_id,
        "expected_partner_lease_id": expected_host_lease_id,
        "public_key": shared.public_key_b64(),
    })
    .to_string();
    let key_resp = http_agent()
        .post(&format!("{url}/api/key"))
        .set("Content-Type", "application/json")
        .send_string(&key_body)
        .map_err(|e| format!("exchange tunnel key: {e}"))?;
    let key_text = key_resp
        .into_string()
        .map_err(|e| format!("read key response: {e}"))?;
    let key_json: serde_json::Value =
        serde_json::from_str(&key_text).map_err(|e| format!("parse key response: {e}"))?;
    validate_partner_response(&key_json, &expected_host_peer_id, &expected_host_lease_id)?;
    shared.update_shared_key_from_partner_b64(key_json["partner_key"].as_str());

    // Create a real relay request and obtain a short-lived ticket over HTTPS.
    let relay_nonce = shared.next_relay_nonce.fetch_add(1, Ordering::Relaxed);
    let relay_body = serde_json::json!({
        "token": token,
        "role": "client",
        "peer_id": peer_id,
        "lease_id": lease_id,
        "expected_partner_peer_id": expected_host_peer_id,
        "expected_partner_lease_id": expected_host_lease_id,
        "generation": relay_nonce,
        "mode": "request",
    })
    .to_string();
    let relay_response = http_agent()
        .post(&format!("{url}/api/relay"))
        .set("Content-Type", "application/json")
        .send_string(&relay_body)
        .map_err(|e| format!("request relay from server: {e}"))?;
    let relay_text = relay_response
        .into_string()
        .map_err(|e| format!("read relay response: {e}"))?;
    let relay_json: serde_json::Value =
        serde_json::from_str(&relay_text).map_err(|e| format!("parse relay response: {e}"))?;
    let crypto = crypto_from_signal_response(
        shared,
        &relay_json,
        TunnelMode::Relay,
        "relay",
        relay_nonce,
        &peer_id,
        (&expected_host_peer_id, &expected_host_lease_id),
    )?;
    let ticket = relay_json["ticket"]
        .as_str()
        .filter(|ticket| !ticket.is_empty())
        .ok_or_else(|| "API server returned no relay ticket".to_string())?
        .to_string();
    let relay_port = relay_json["relay_port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0);
    *shared.relay_port.lock().unwrap() = relay_port;
    let relay_addr = st_protocol::tcp_tunnel::resolve_relay_addr(
        &url,
        relay_port,
        std::env::var("ST_RELAY_ADDR").ok(),
    )
    .ok_or_else(|| "API server does not advertise a TCP relay".to_string())?;

    Ok((crypto, relay_addr, ticket))
}

/// Dial the relay and complete the pairing handshake. Blocks until the host
/// arrives (it dials once it sees the relay nonce on its session poll).
/// Returns the stream positioned exactly at the start of tunnel traffic.
pub fn connect_relay(addr: &str, ticket: &str) -> Result<std::net::TcpStream, String> {
    st_protocol::tcp_tunnel::connect_relay(addr, "client", ticket, Duration::from_secs(30))
}

fn parse_session_status(shared: &ApiDiscoveryShared, json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return clear_host(shared);
    };

    *shared.relay_port.lock().unwrap() = v["relay_port"].as_u64().map(|p| p as u16);

    let mut host_guard = shared.host.lock().unwrap();
    if let Some(host) = v["host"].as_object() {
        let mut candidates: Vec<String> = host["candidates"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|value| value.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        sort_candidates(&mut candidates);

        let hostname = host["hostname"].as_str().map(String::from);
        let peer_id = host["peer_id"].as_str().map(String::from);
        let lease_id = host["lease_id"].as_str().map(String::from);

        let changed = match host_guard.as_ref() {
            Some(existing) => {
                existing.candidates != candidates
                    || existing.hostname != hostname
                    || existing.peer_id != peer_id
                    || existing.lease_id != lease_id
            }
            None => true,
        };
        *host_guard = Some(ApiDiscoveredHost {
            candidates,
            hostname,
            peer_id,
            lease_id,
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
    if url.is_empty() || !valid_api_token(&token) {
        return;
    }
    let body = serde_json::json!({
        "token": token,
        "role": "client",
        "peer_id": peer_id,
        "lease_id": shared.lease_id,
    })
    .to_string();
    let _ = http_agent()
        .post(&format!("{url}/api/unregister"))
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
