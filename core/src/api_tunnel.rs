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
const HOST_SYNC_DELAY: Duration = Duration::from_millis(3_250);
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

struct HostIdentity {
    peer_id: String,
    lease_id: String,
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
    lease: ApiSessionLease,
}

impl ApiTunnelConnection {
    pub(crate) fn link(&self) -> Arc<dyn TunnelLink> {
        Arc::clone(&self.link)
    }

    pub(crate) fn close(self, config: &ApiConnectionConfig, token: &str) {
        unregister(config, token, &self.lease.state);
        drop(self.lease);
    }
}

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
        let port = socket
            .local_addr()
            .map_err(|error| format!("failed to inspect API punch socket: {error}"))?
            .port();
        let local_candidates = tunnel::gather_candidates_with_stun(port, Some(&socket));
        check_cancelled(stop)?;

        let (partner_candidates, shared_secret, host) =
            prepare_punch(config, token, &local_candidates, &state, stop)?;
        let signal = request(config, token, "punch", &state, &host)?;
        let crypto = crypto_from_signal_response(
            config,
            &state,
            &host,
            shared_secret,
            TunnelMode::Punch,
            "punch",
            &signal,
        )?;
        let peer = tunnel::hole_punch_cancellable(
            &socket,
            &partner_candidates,
            &crypto,
            PUNCH_TIMEOUT,
            || stop.load(Ordering::Acquire),
        )?;
        check_cancelled(stop)?;
        let link: Arc<dyn TunnelLink> = Arc::new(PunchedSocket::new(socket, peer, crypto));
        Ok(link)
    })();

    match result {
        Ok(link) => Ok(ApiTunnelConnection { link, lease }),
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
        Ok(link) => Ok(ApiTunnelConnection { link, lease }),
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
    let _ = post_json(&config.api_url, "unregister", &body);
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

fn prepare_punch(
    config: &ApiConnectionConfig,
    token: &str,
    local_candidates: &[String],
    state: &ApiProcessState,
    stop: &AtomicBool,
) -> Result<(Vec<SocketAddr>, [u8; 32], HostIdentity), String> {
    register(config, token, local_candidates, state)?;
    check_cancelled(stop)?;
    let mut host = verify_host(config, token, state)?;
    check_cancelled(stop)?;
    exchange_key(config, token, state, &host)?;
    check_cancelled(stop)?;
    exchange_candidates(config, token, local_candidates, state, &host)?;
    check_cancelled(stop)?;

    // An idle host can sleep for three seconds. Let it observe this key before
    // posting the nonce, then refresh both partner values once more so its
    // punch task cannot capture stale signaling state.
    interruptible_sleep(stop, HOST_SYNC_DELAY)?;
    host = verify_host(config, token, state)?;
    check_cancelled(stop)?;
    let shared_secret = exchange_key(config, token, state, &host)?;
    check_cancelled(stop)?;
    let partner_candidates = exchange_candidates(config, token, local_candidates, state, &host)?;
    check_cancelled(stop)?;
    Ok((partner_candidates, shared_secret, host))
}

fn prepare_relay(
    config: &ApiConnectionConfig,
    token: &str,
    local_candidates: &[String],
    state: &ApiProcessState,
    stop: &AtomicBool,
) -> Result<(Arc<CryptoContext>, String, Option<u16>), String> {
    register(config, token, local_candidates, state)?;
    check_cancelled(stop)?;
    let host = verify_host(config, token, state)?;
    check_cancelled(stop)?;
    let shared_secret = exchange_key(config, token, state, &host)?;
    check_cancelled(stop)?;
    let response = post_json(
        &config.api_url,
        "relay",
        &serde_json::json!({
            "token": token,
            "role": "client",
            "peer_id": config.client_peer_id,
            "lease_id": state.lease_id,
            "expected_partner_peer_id": host.peer_id,
            "expected_partner_lease_id": host.lease_id,
            "generation": config.request_nonce,
            "mode": "request",
        }),
    )?;
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
        shared_secret,
        TunnelMode::Relay,
        "relay",
        &response,
    )?;
    Ok((crypto, ticket, relay_port))
}

fn interruptible_sleep(stop: &AtomicBool, duration: Duration) -> Result<(), String> {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        check_cancelled(stop)?;
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

fn register(
    config: &ApiConnectionConfig,
    token: &str,
    candidates: &[String],
    state: &ApiProcessState,
) -> Result<(), String> {
    post_json(
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
    )?;
    Ok(())
}

fn verify_host(
    config: &ApiConnectionConfig,
    token: &str,
    state: &ApiProcessState,
) -> Result<HostIdentity, String> {
    let discovery = post_json(
        &config.api_url,
        "session",
        &serde_json::json!({"token": token}),
    )?;
    let actual_peer = discovery["host"]["peer_id"]
        .as_str()
        .ok_or_else(|| "API host is no longer online".to_string())?;
    let host_lease_id = discovery["host"]["lease_id"]
        .as_str()
        .ok_or_else(|| "API host has no active process lease".to_string())?;
    if actual_peer != config.host_peer_id {
        return Err("API host identity changed; reload the server list".into());
    }
    let host = HostIdentity {
        peer_id: actual_peer.to_string(),
        lease_id: host_lease_id.to_string(),
    };
    let session = post_json(
        &config.api_url,
        "session",
        &serde_json::json!({
            "token": token,
            "role": "client",
            "peer_id": config.client_peer_id,
            "lease_id": state.lease_id,
            "expected_partner_peer_id": host.peer_id,
            "expected_partner_lease_id": host.lease_id,
        }),
    )?;
    let actual_peer = session["host"]["peer_id"]
        .as_str()
        .ok_or_else(|| "API host is no longer online".to_string())?;
    if actual_peer != config.host_peer_id {
        return Err("API host identity changed; reload the server list".into());
    }
    if session["host"]["lease_id"].as_str() != Some(host.lease_id.as_str()) {
        return Err("API host lease changed during session validation".into());
    }
    Ok(host)
}

fn exchange_key(
    config: &ApiConnectionConfig,
    token: &str,
    state: &ApiProcessState,
    host: &HostIdentity,
) -> Result<[u8; 32], String> {
    let response = post_json(
        &config.api_url,
        "key",
        &serde_json::json!({
            "token": token,
            "role": "client",
            "peer_id": config.client_peer_id,
            "lease_id": state.lease_id,
            "expected_partner_peer_id": host.peer_id,
            "expected_partner_lease_id": host.lease_id,
            "public_key": state.public_key_b64(),
        }),
    )?;
    validate_partner_response(&response, host)?;
    let partner_key = response["partner_key"]
        .as_str()
        .ok_or_else(|| "API host tunnel key is not ready".to_string())?;
    let decoded = base64_decode(partner_key)
        .filter(|bytes| bytes.len() == 32)
        .ok_or_else(|| "API host returned an invalid tunnel key".to_string())?;
    let mut public_key = [0u8; 32];
    public_key.copy_from_slice(&decoded);
    Ok(state.shared_secret(public_key))
}

fn exchange_candidates(
    config: &ApiConnectionConfig,
    token: &str,
    candidates: &[String],
    state: &ApiProcessState,
    host: &HostIdentity,
) -> Result<Vec<SocketAddr>, String> {
    let response = post_json(
        &config.api_url,
        "candidates",
        &serde_json::json!({
            "token": token,
            "role": "client",
            "peer_id": config.client_peer_id,
            "lease_id": state.lease_id,
            "expected_partner_peer_id": host.peer_id,
            "expected_partner_lease_id": host.lease_id,
            "candidates": candidates,
        }),
    )?;
    validate_partner_response(&response, host)?;
    let candidates: Vec<SocketAddr> = response["partner_candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str()?.parse().ok())
        .collect();
    if candidates.is_empty() {
        return Err("API host has no usable punch candidates".into());
    }
    Ok(candidates)
}

fn request(
    config: &ApiConnectionConfig,
    token: &str,
    endpoint: &str,
    state: &ApiProcessState,
    host: &HostIdentity,
) -> Result<serde_json::Value, String> {
    post_json(
        &config.api_url,
        endpoint,
        &serde_json::json!({
            "token": token,
            "role": "client",
            "peer_id": config.client_peer_id,
            "lease_id": state.lease_id,
            "expected_partner_peer_id": host.peer_id,
            "expected_partner_lease_id": host.lease_id,
            "generation": config.request_nonce,
        }),
    )
}

fn validate_partner_response(
    response: &serde_json::Value,
    host: &HostIdentity,
) -> Result<(), String> {
    if response["partner_peer_id"].as_str() != Some(host.peer_id.as_str())
        || response["partner_lease_id"].as_str() != Some(host.lease_id.as_str())
    {
        return Err("API host lease changed during signaling".into());
    }
    Ok(())
}

fn crypto_from_signal_response(
    config: &ApiConnectionConfig,
    state: &ApiProcessState,
    host: &HostIdentity,
    shared_secret: [u8; 32],
    mode: TunnelMode,
    mode_label: &str,
    response: &serde_json::Value,
) -> Result<Arc<CryptoContext>, String> {
    if response["mode"].as_str() != Some(mode_label)
        || response["generation"].as_u64() != Some(config.request_nonce)
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
        &shared_secret,
        &SessionKeyContext {
            request_context,
            session_id,
            mode,
            generation: config.request_nonce,
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

fn post_json(
    api_url: &str,
    endpoint: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let url = format!("{}/api/{endpoint}", api_url.trim_end_matches('/'));
    let response = http_agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|error| format!("API {endpoint} request failed: {error}"))?;
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
