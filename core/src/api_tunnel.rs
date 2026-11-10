use rand::{rngs::OsRng, RngCore};
use st_protocol::reliable_udp::PunchedSocket;
use st_protocol::tcp_tunnel::{self, TcpTunnel, TunnelLink};
use st_protocol::tunnel::{
    self, derive_session_key, CryptoContext, SessionKeyContext, TunnelKeys, TunnelMode,
};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::client::ApiConnectionConfig;

const PUNCH_TIMEOUT: Duration = Duration::from_secs(10);
/// One failed round is usually timing, not topology: the host starts probing
/// only once its API poll shows the request, and the first round's probes have
/// opened both NAT mappings by the time the second starts.
const PUNCH_ROUNDS: u64 = 2;
/// How long a signaling call keeps retrying a transport error or a 5xx.
const SIGNAL_RETRY: Duration = Duration::from_secs(4);
const MAX_TOKEN_LEN: usize = 256;

struct ApiProcessState {
    active: AtomicBool,
    keys: Mutex<TunnelKeys>,
    crypto: Mutex<Option<([u8; 32], Arc<CryptoContext>)>>,
    lease_id: String,
}

impl ApiProcessState {
    fn new() -> Self {
        let mut lease = [0u8; 32];
        OsRng.fill_bytes(&mut lease);
        Self {
            active: AtomicBool::new(false),
            keys: Mutex::new(TunnelKeys::generate()),
            crypto: Mutex::new(None),
            lease_id: base64_encode(&lease),
        }
    }

    fn public_key_b64(&self) -> String {
        base64_encode(&self.keys.lock().unwrap().public_key_bytes())
    }

    fn shared_secret(&self, partner_public_key: [u8; 32]) -> [u8; 32] {
        self.keys
            .lock()
            .unwrap()
            .derive_shared_key(&partner_public_key)
    }

    fn crypto_for_session_key(&self, session_key: [u8; 32]) -> Arc<CryptoContext> {
        let mut cache = self.crypto.lock().unwrap();
        if let Some((cached_key, crypto)) = cache.as_ref() {
            if *cached_key == session_key {
                return Arc::clone(crypto);
            }
        }
        let crypto = Arc::new(CryptoContext::new(session_key, false));
        *cache = Some((session_key, Arc::clone(&crypto)));
        crypto
    }
}

/// The host as one API snapshot described it.
struct Host {
    peer_id: String,
    lease_id: String,
    shared_secret: [u8; 32],
    candidates: Vec<SocketAddr>,
}

struct ApiSessionLease {
    state: Arc<ApiProcessState>,
}

impl Drop for ApiSessionLease {
    fn drop(&mut self) {
        self.state.active.store(false, Ordering::Release);
    }
}

pub(crate) struct ApiTunnelConnection {
    link: Arc<dyn TunnelLink>,
    // Stop renewal before releasing the process lease to a reconnect attempt.
    heartbeat: ApiLeaseHeartbeat,
    lease: ApiSessionLease,
}

struct ApiLeaseHeartbeat {
    stop: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ApiLeaseHeartbeat {
    fn start(config: &ApiConnectionConfig, token: &str, state: &ApiProcessState) -> Self {
        let (stop, stopped) = std::sync::mpsc::channel();
        let url = config.api_url.clone();
        let body = serde_json::json!({"token": token, "role": "client",
            "peer_id": config.client_peer_id, "lease_id": state.lease_id});
        let worker = std::thread::spawn(move || {
            while matches!(
                stopped.recv_timeout(Duration::from_secs(30)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                let _ = post_once(&url, "heartbeat", &body);
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for ApiLeaseHeartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl ApiTunnelConnection {
    pub(crate) fn link(&self) -> Arc<dyn TunnelLink> {
        Arc::clone(&self.link)
    }

    pub(crate) fn close(self, config: &ApiConnectionConfig, token: &str) {
        drop(self.heartbeat);
        unregister(config, token, &self.lease.state);
        drop(self.lease);
    }
}

fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .timeout_connect(Duration::from_secs(3))
            .timeout_read(Duration::from_secs(5))
            .timeout_write(Duration::from_secs(5))
            .build()
    })
}

pub(crate) fn connect_punch(
    config: &ApiConnectionConfig,
    token: &str,
    stop: &AtomicBool,
) -> Result<ApiTunnelConnection, String> {
    validate_config(config)?;
    validate_token(token)?;
    check_cancelled(stop)?;
    let state = process_state(config);
    let lease = acquire_session(Arc::clone(&state), stop)?;
    let result = (|| {
        let socket = UdpSocket::bind("0.0.0.0:0")
            .map_err(|error| format!("failed to bind API punch socket: {error}"))?;
        let mut round = 0;
        let (peer, crypto) = loop {
            match punch_round(config, token, &state, &socket, round, stop) {
                Ok(punched) => break punched,
                Err(error) => {
                    check_cancelled(stop)?;
                    round += 1;
                    if round == PUNCH_ROUNDS {
                        return Err(error);
                    }
                    eprintln!("[punch] round {round}/{PUNCH_ROUNDS} failed: {error}");
                }
            }
        };
        check_cancelled(stop)?;
        let link: Arc<dyn TunnelLink> = Arc::new(PunchedSocket::new(socket, peer, crypto));
        Ok(link)
    })();

    match result {
        Ok(link) => Ok(ApiTunnelConnection {
            link,
            heartbeat: ApiLeaseHeartbeat::start(config, token, &state),
            lease,
        }),
        Err(error) => {
            unregister(config, token, &state);
            Err(error)
        }
    }
}

pub(crate) fn connect_relay(
    config: &ApiConnectionConfig,
    token: &str,
    stop: &AtomicBool,
) -> Result<ApiTunnelConnection, String> {
    validate_config(config)?;
    validate_token(token)?;
    check_cancelled(stop)?;
    let state = process_state(config);
    let lease = acquire_session(Arc::clone(&state), stop)?;
    let result =
        prepare_relay(config, token, &[], &state, stop).and_then(|(crypto, ticket, relay_port)| {
            dial_relay(config, crypto, &ticket, relay_port, stop)
        });
    match result {
        Ok(link) => Ok(ApiTunnelConnection {
            link,
            heartbeat: ApiLeaseHeartbeat::start(config, token, &state),
            lease,
        }),
        Err(error) => {
            unregister(config, token, &state);
            Err(error)
        }
    }
}

fn unregister(config: &ApiConnectionConfig, token: &str, state: &ApiProcessState) {
    let body = serde_json::json!({
        "token": token,
        "role": "client",
        "peer_id": config.client_peer_id,
        "lease_id": state.lease_id,
    });
    let _ = post_once(&config.api_url, "unregister", &body);
}

fn validate_config(config: &ApiConnectionConfig) -> Result<(), String> {
    if config.api_url.trim().is_empty() {
        return Err("API URL is empty".into());
    }
    if config.client_peer_id.trim().is_empty() || config.client_peer_id.len() > MAX_TOKEN_LEN {
        return Err("Android client peer identity is empty".into());
    }
    if config.host_peer_id.trim().is_empty() || config.host_peer_id.len() > MAX_TOKEN_LEN {
        return Err("API host peer identity is empty".into());
    }
    if config.request_nonce == 0 {
        return Err("API tunnel request nonce is invalid".into());
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("API token is empty".into());
    }
    if token.len() > MAX_TOKEN_LEN {
        return Err(format!("API token exceeds {MAX_TOKEN_LEN} bytes"));
    }
    Ok(())
}

fn check_cancelled(stop: &AtomicBool) -> Result<(), String> {
    if stop.load(Ordering::Acquire) {
        Err("connection cancelled".into())
    } else {
        Ok(())
    }
}

fn process_state(config: &ApiConnectionConfig) -> Arc<ApiProcessState> {
    static STATES: OnceLock<Mutex<HashMap<String, Arc<ApiProcessState>>>> = OnceLock::new();
    let key = format!(
        "{}\n{}",
        config.api_url.trim_end_matches('/'),
        config.client_peer_id
    );
    let mut states = STATES.get_or_init(Default::default).lock().unwrap();
    Arc::clone(
        states
            .entry(key)
            .or_insert_with(|| Arc::new(ApiProcessState::new())),
    )
}

fn acquire_session(
    state: Arc<ApiProcessState>,
    stop: &AtomicBool,
) -> Result<ApiSessionLease, String> {
    loop {
        check_cancelled(stop)?;
        if state
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(ApiSessionLease { state });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One punch round on `socket`: fresh candidates, a fresh request (each round
/// has its own generation, so the host drops the previous one), then probing.
fn punch_round(
    config: &ApiConnectionConfig,
    token: &str,
    state: &ApiProcessState,
    socket: &UdpSocket,
    round: u64,
    stop: &AtomicBool,
) -> Result<(SocketAddr, Arc<CryptoContext>), String> {
    check_cancelled(stop)?;
    let port = socket
        .local_addr()
        .map_err(|error| format!("failed to inspect API punch socket: {error}"))?
        .port();
    let local_candidates = tunnel::gather_candidates_with_stun(port, Some(socket));
    check_cancelled(stop)?;
    let host = register(config, token, &local_candidates, state, stop)?;
    if host.candidates.is_empty() {
        return Err("API host has no usable punch candidates".into());
    }
    let generation = config.request_nonce.saturating_add(round);
    let signal = post_json(
        &config.api_url,
        "punch",
        &request_body(config, token, state, &host, generation),
        stop,
    )?;
    let crypto =
        crypto_from_signal_response(config, state, &host, TunnelMode::Punch, generation, &signal)?;
    let peer =
        tunnel::hole_punch_cancellable(socket, &host.candidates, &crypto, PUNCH_TIMEOUT, || {
            stop.load(Ordering::Acquire)
        })?;
    Ok((peer, crypto))
}

fn prepare_relay(
    config: &ApiConnectionConfig,
    token: &str,
    local_candidates: &[String],
    state: &ApiProcessState,
    stop: &AtomicBool,
) -> Result<(Arc<CryptoContext>, String, Option<u16>), String> {
    let host = register(config, token, local_candidates, state, stop)?;
    let mut body = request_body(config, token, state, &host, config.request_nonce);
    body["mode"] = "request".into();
    let response = post_json(&config.api_url, "relay", &body, stop)?;
    let ticket = response["ticket"]
        .as_str()
        .filter(|ticket| !ticket.is_empty())
        .ok_or_else(|| "API server returned no relay ticket".to_string())?
        .to_string();
    let relay_port = response["relay_port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0);
    let crypto = crypto_from_signal_response(
        config,
        state,
        &host,
        TunnelMode::Relay,
        config.request_nonce,
        &response,
    )?;
    Ok((crypto, ticket, relay_port))
}

/// Registers this client and reads the host out of the same atomic snapshot
/// the host itself polls: identity, key and candidates of one lease, in one
/// round trip instead of five.
fn register(
    config: &ApiConnectionConfig,
    token: &str,
    candidates: &[String],
    state: &ApiProcessState,
    stop: &AtomicBool,
) -> Result<Host, String> {
    let response = post_json(
        &config.api_url,
        "register",
        &serde_json::json!({
            "token": token,
            "role": "client",
            "peer_id": config.client_peer_id,
            "lease_id": state.lease_id,
            "candidates": candidates,
            "public_key": state.public_key_b64(),
        }),
        stop,
    )?;
    let host = &response["session"]["host"];
    let peer_id = host["peer_id"]
        .as_str()
        .ok_or_else(|| "API host is no longer online".to_string())?;
    if peer_id != config.host_peer_id {
        return Err("API host identity changed; reload the server list".into());
    }
    let lease_id = host["lease_id"]
        .as_str()
        .ok_or_else(|| "API host has no active process lease".to_string())?;
    let public_key: [u8; 32] = host["public_key"]
        .as_str()
        .and_then(base64_decode)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| "API host tunnel key is not ready".to_string())?;
    Ok(Host {
        peer_id: peer_id.to_string(),
        lease_id: lease_id.to_string(),
        shared_secret: state.shared_secret(public_key),
        candidates: host["candidates"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str()?.parse().ok())
            .collect(),
    })
}

fn request_body(
    config: &ApiConnectionConfig,
    token: &str,
    state: &ApiProcessState,
    host: &Host,
    generation: u64,
) -> serde_json::Value {
    serde_json::json!({
        "token": token,
        "role": "client",
        "peer_id": config.client_peer_id,
        "lease_id": state.lease_id,
        "expected_partner_peer_id": host.peer_id,
        "expected_partner_lease_id": host.lease_id,
        "generation": generation,
    })
}

fn crypto_from_signal_response(
    config: &ApiConnectionConfig,
    state: &ApiProcessState,
    host: &Host,
    mode: TunnelMode,
    generation: u64,
    response: &serde_json::Value,
) -> Result<Arc<CryptoContext>, String> {
    let mode_label = match mode {
        TunnelMode::Punch => "punch",
        TunnelMode::Relay => "relay",
    };
    if response["mode"].as_str() != Some(mode_label)
        || response["generation"].as_u64() != Some(generation)
        || response["owner_peer_id"].as_str() != Some(config.client_peer_id.as_str())
        || response["owner_lease_id"].as_str() != Some(state.lease_id.as_str())
        || response["partner_peer_id"].as_str() != Some(host.peer_id.as_str())
        || response["partner_lease_id"].as_str() != Some(host.lease_id.as_str())
    {
        return Err("API returned a mismatched tunnel request context".into());
    }
    let session_id = response["session_id"]
        .as_str()
        .ok_or_else(|| "API returned no tunnel session ID".to_string())?;
    let request_context = response["context"]
        .as_str()
        .ok_or_else(|| "API returned no tunnel request context".to_string())?;
    let key = derive_session_key(
        &host.shared_secret,
        &SessionKeyContext {
            request_context,
            session_id,
            mode,
            generation,
            host_peer_id: &host.peer_id,
            host_lease_id: &host.lease_id,
            client_peer_id: &config.client_peer_id,
            client_lease_id: &state.lease_id,
        },
    )?;
    Ok(state.crypto_for_session_key(key))
}

fn dial_relay(
    config: &ApiConnectionConfig,
    crypto: Arc<CryptoContext>,
    ticket: &str,
    relay_port: Option<u16>,
    stop: &AtomicBool,
) -> Result<Arc<dyn TunnelLink>, String> {
    check_cancelled(stop)?;
    let relay_addr = tcp_tunnel::resolve_relay_addr(&config.api_url, relay_port, None)
        .ok_or_else(|| "API server does not advertise a TCP relay".to_string())?;
    let socket_addrs: Vec<_> = relay_addr
        .to_socket_addrs()
        .map_err(|error| format!("failed to resolve relay {relay_addr}: {error}"))?
        .collect();
    let mut stream = socket_addrs
        .into_iter()
        .find_map(|address| TcpStream::connect_timeout(&address, Duration::from_secs(10)).ok())
        .ok_or_else(|| format!("failed to connect relay {relay_addr}"))?;
    let _ = stream.set_nodelay(true);
    stream
        .write_all(tcp_tunnel::relay_hello_line("client", ticket).as_bytes())
        .map_err(|error| format!("failed to send relay hello: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|error| format!("failed to configure relay timeout: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut response = [0u8; 3];
    let mut received = 0;
    while received < response.len() {
        check_cancelled(stop)?;
        if Instant::now() >= deadline {
            return Err("relay pairing timed out".into());
        }
        match stream.read(&mut response[received..]) {
            Ok(0) => return Err("relay closed before pairing completed".into()),
            Ok(size) => received += size,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(format!("relay pairing failed: {error}")),
        }
    }
    if response != *b"OK\n" {
        return Err("relay pairing failed (unexpected response)".into());
    }
    let _ = stream.set_read_timeout(None);
    let tunnel = TcpTunnel::new(stream, Some(crypto), Vec::new())?;
    Ok(Arc::new(tunnel))
}

enum PostError {
    /// The API answered and said no; asking again cannot help.
    Refused(u16),
    Unreachable(String),
}

fn post_once(
    api_url: &str,
    endpoint: &str,
    body: &serde_json::Value,
) -> Result<ureq::Response, PostError> {
    http_agent()
        .post(&format!("{}/api/{endpoint}", api_url.trim_end_matches('/')))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|error| match error {
            ureq::Error::Status(code, _) if code < 500 => PostError::Refused(code),
            error => PostError::Unreachable(error.to_string()),
        })
}

/// Every signaling call is idempotent, so a transport error or a 5xx is
/// retried for `SIGNAL_RETRY`: one hiccup used to end the punch and send the
/// session to the relay.
fn post_json(
    api_url: &str,
    endpoint: &str,
    body: &serde_json::Value,
    stop: &AtomicBool,
) -> Result<serde_json::Value, String> {
    let deadline = Instant::now() + SIGNAL_RETRY;
    let response = loop {
        check_cancelled(stop)?;
        match post_once(api_url, endpoint, body) {
            Ok(response) => break response,
            Err(PostError::Refused(code)) => {
                return Err(format!("API {endpoint} request refused: status {code}"));
            }
            Err(PostError::Unreachable(error)) if Instant::now() >= deadline => {
                return Err(format!("API {endpoint} request failed: {error}"));
            }
            Err(PostError::Unreachable(_)) => std::thread::sleep(Duration::from_millis(300)),
        }
    };
    let text = response
        .into_string()
        .map_err(|error| format!("failed to read API {endpoint} response: {error}"))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("failed to parse API {endpoint} response: {error}"))
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 0x3f) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            CHARS[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            CHARS[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(value: &str) -> Option<Vec<u8>> {
    fn decode_char(value: u8) -> Option<u8> {
        match value {
            b'A'..=b'Z' => Some(value - b'A'),
            b'a'..=b'z' => Some(value - b'a' + 26),
            b'0'..=b'9' => Some(value - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let value = value.trim().trim_end_matches('=');
    let bytes = value.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let first = decode_char(*chunk.first()?)?;
        let second = decode_char(*chunk.get(1)?)?;
        out.push((first << 2) | (second >> 4));
        if let Some(third) = chunk.get(2).copied().and_then(decode_char) {
            out.push((second << 4) | (third >> 2));
            if let Some(fourth) = chunk.get(3).copied().and_then(decode_char) {
                out.push((third << 6) | fourth);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_tunnel_keys() {
        let input = [0x5au8; 32];
        assert_eq!(base64_decode(&base64_encode(&input)), Some(input.to_vec()));
    }

    #[test]
    fn rejects_invalid_api_tunnel_configuration() {
        let valid = ApiConnectionConfig {
            api_url: "https://example.test".into(),
            client_peer_id: "client".into(),
            host_peer_id: "host".into(),
            request_nonce: 1,
        };
        assert!(validate_config(&valid).is_ok());
        assert!(validate_config(&ApiConnectionConfig {
            host_peer_id: String::new(),
            ..valid
        })
        .is_err());
    }

    #[test]
    fn process_state_reuses_only_the_same_request_context() {
        let state = ApiProcessState::new();
        let partner = TunnelKeys::generate();
        let shared = state.shared_secret(partner.public_key_bytes());
        let context = SessionKeyContext {
            request_context: "request-context",
            session_id: "session",
            mode: TunnelMode::Punch,
            generation: 1,
            host_peer_id: "host",
            host_lease_id: "host-lease",
            client_peer_id: "client",
            client_lease_id: &state.lease_id,
        };
        let key = derive_session_key(&shared, &context).unwrap();
        let first = state.crypto_for_session_key(key);
        let second = state.crypto_for_session_key(key);
        assert!(Arc::ptr_eq(&first, &second));

        let next_key = derive_session_key(
            &shared,
            &SessionKeyContext {
                request_context: "next-context",
                generation: 2,
                ..context
            },
        )
        .unwrap();
        let next = state.crypto_for_session_key(next_key);
        assert!(!Arc::ptr_eq(&first, &next));
    }

    #[test]
    fn session_lease_serializes_connection_attempts() {
        let state = Arc::new(ApiProcessState::new());
        let running = AtomicBool::new(false);
        let lease = acquire_session(Arc::clone(&state), &running).unwrap();
        let cancelled = AtomicBool::new(true);
        assert!(acquire_session(Arc::clone(&state), &cancelled).is_err());
        drop(lease);
        assert!(acquire_session(state, &running).is_ok());
    }
}
