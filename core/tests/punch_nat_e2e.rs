//! API-signaled hole punching through real NATs, end to end.
//!
//! Skipped unless `ST_NAT_E2E_API` is set. `scripts/punch-nat-e2e.sh` builds
//! the network (kernel NATs in unprivileged namespaces, a local API server and
//! STUN responder, an `st-server` with synthetic outputs) and runs this test
//! behind the client-side NAT.

mod common;

use common::{accept_announced_stream, assert_live, next_stream, outputs};
use st_client_core::{ApiConnectionConfig, Client, ClientConfig, TransportKind, OUTPUT_DEFAULT};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Bed {
    api: String,
    token: String,
    expect: TransportKind,
    rounds: u32,
    hold: Duration,
    connect_budget: Duration,
    server_log: Option<String>,
}

impl Bed {
    fn from_env() -> Option<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let number = |name: &str, default: u64| {
            var(name)
                .map(|value| value.parse().expect(name))
                .unwrap_or(default)
        };
        Some(Self {
            api: var("ST_NAT_E2E_API")?,
            token: var("ST_NAT_E2E_TOKEN").expect("ST_NAT_E2E_TOKEN"),
            expect: match var("ST_NAT_E2E_EXPECT").as_deref() {
                Some("relay") => TransportKind::ApiRelay,
                _ => TransportKind::ApiPunch,
            },
            rounds: number("ST_NAT_E2E_ROUNDS", 3) as u32,
            hold: Duration::from_secs(number("ST_NAT_E2E_HOLD_SECS", 20)),
            connect_budget: Duration::from_secs(number("ST_NAT_E2E_CONNECT_SECS", 30)),
            server_log: var("ST_NAT_E2E_SERVER_LOG"),
        })
    }

    fn host_peer_id(&self) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let host = ureq::post(&format!("{}/api/session", self.api))
                .set("Content-Type", "application/json")
                .send_string(&serde_json::json!({ "token": self.token }).to_string())
                .ok()
                .and_then(|response| response.into_string().ok())
                .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                .and_then(|session| Some(session["host"]["peer_id"].as_str()?.to_owned()));
            if let Some(host) = host {
                return host;
            }
            assert!(Instant::now() < deadline, "host never registered");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn connect(&self, host_peer_id: &str) -> Arc<Client> {
        let request_nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64;
        Client::start(ClientConfig {
            server: None,
            api: Some(ApiConnectionConfig {
                api_url: self.api.clone(),
                client_peer_id: "nat-e2e-client".into(),
                host_peer_id: host_peer_id.into(),
                request_nonce,
            }),
            token: self.token.clone(),
            refresh_millihz: 60_000,
            output_id: OUTPUT_DEFAULT,
            aux: false,
            display_width: 1280,
            display_height: 720,
        })
        .expect("client")
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        if std::thread::panicking() {
            if let Some(log) = self.server_log.as_deref() {
                let log = std::fs::read_to_string(log).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(80).collect();
                eprintln!("--- server log (tail) ---");
                for line in tail.into_iter().rev() {
                    eprintln!("{line}");
                }
            }
        }
    }
}

struct Held {
    longest_gap: Duration,
    video_kbps: u64,
}

/// Streams for `hold`, moving the session between outputs every few seconds:
/// each move needs the reliable control channel in both directions, and video
/// must follow it.
fn hold_session(client: &Client, hold: Duration, round: u32) -> Held {
    let listed = outputs(client);
    let started = Instant::now();
    let end = started + hold;
    let mut last_unit = Instant::now();
    let mut longest_gap = Duration::ZERO;
    let mut video_bytes = 0u64;
    let mut next_switch = Instant::now() + Duration::from_secs(3);
    let mut current = 0usize;
    while Instant::now() < end {
        accept_announced_stream(client);
        if let Some(unit) = client.recv_access_unit(Duration::from_millis(50)) {
            video_bytes += unit.data.len() as u64;
            longest_gap = longest_gap.max(last_unit.elapsed());
            last_unit = Instant::now();
        }
        assert!(
            last_unit.elapsed() < Duration::from_secs(4),
            "round {round}: video stalled (status: {})",
            client.status()
        );
        if listed.len() > 1 && Instant::now() >= next_switch {
            current = (current + 1) % listed.len();
            let wanted = listed[current].id;
            client.select_output(wanted).expect("select output");
            next_stream(client, "output switch", Duration::from_secs(8), |event| {
                event.output_id == wanted
            });
            assert_live(client, "switched output");
            last_unit = Instant::now();
            next_switch = Instant::now() + Duration::from_secs(3);
        }
    }
    Held {
        longest_gap,
        video_kbps: video_bytes * 8 / started.elapsed().as_millis().max(1) as u64,
    }
}

#[test]
fn every_attempt_connects_and_the_session_holds() {
    let Some(bed) = Bed::from_env() else {
        eprintln!("ST_NAT_E2E_API not set; skipping");
        return;
    };
    let host = bed.host_peer_id();
    for round in 1..=bed.rounds {
        let started = Instant::now();
        let client = bed.connect(&host);
        let stream = next_stream(&client, "first stream", bed.connect_budget, |_| true);
        assert_eq!(
            stream.transport, bed.expect,
            "round {round}: wrong transport"
        );
        let announced = started.elapsed();
        assert_live(&client, "first picture");
        let live = started.elapsed();
        let held = hold_session(&client, bed.hold, round);
        eprintln!(
            "round {round}: {:?} stream after {announced:?}, live after {live:?}, {} kbit/s, longest video gap {:?}",
            stream.transport, held.video_kbps, held.longest_gap
        );
        client.stop();
        drop(client);
        std::thread::sleep(Duration::from_millis(500));
    }
}
