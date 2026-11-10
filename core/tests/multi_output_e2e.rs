//! Multi-output streaming against a real server, end to end.
//!
//! Skipped unless `ST_E2E_SERVER_BIN` names an `st-server` binary. The test
//! starts it with synthetic outputs (`ST_CAPTURE=synthetic`, Linux), so it
//! needs no compositor and no capture privilege:
//!
//! ```text
//! ST_E2E_SERVER_BIN=$PWD/../../server/target/debug/st-server \
//!     cargo test --test multi_output_e2e -- --nocapture
//! ```

mod common;

use common::{assert_live, outputs};
use st_client_core::{Client, ClientConfig, StreamEvent, OUTPUT_DEFAULT};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOKEN: &str = "e2e-multi-output";

struct Server {
    child: Child,
    address: String,
    state_dir: PathBuf,
}

impl Server {
    fn start(binary: &str) -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port")
            .port();
        let state_dir = std::env::temp_dir().join(format!("st-e2e-{port}"));
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let log = std::fs::File::create(state_dir.join("server.log")).expect("log");
        let child = Command::new(binary)
            .env("ST_CAPTURE", "synthetic")
            .env("ST_SYNTHETIC_OUTPUTS", "2")
            .env("ST_PORT", port.to_string())
            .env("ST_TOKEN", TOKEN)
            .env("ST_STATE_DIR", &state_dir)
            .env("ST_SERVER_NO_TRAY", "1")
            .env("ST_WAKE_ON_CONNECT", "0")
            // Nothing here should reach the public signaling server.
            .env("ST_API_URL", "http://127.0.0.1:9")
            .env("ST_FPS", "30")
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("log"))
            .stderr(log)
            .spawn()
            .expect("start st-server");
        let server = Self {
            child,
            address: format!("127.0.0.1:{port}"),
            state_dir,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while TcpStream::connect(&server.address).is_err() {
            assert!(
                Instant::now() < deadline,
                "server never listened\n{}",
                server.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        server
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.state_dir.join("server.log")).unwrap_or_default()
    }

    fn connect(&self, output_id: u32, aux: bool) -> Arc<Client> {
        Client::start(ClientConfig {
            server: Some(self.address.clone()),
            api: None,
            token: TOKEN.into(),
            refresh_millihz: 30_000,
            output_id,
            aux,
            display_width: 1280,
            display_height: 720,
        })
        .expect("client")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() || std::env::var_os("ST_E2E_LOG").is_some() {
            eprintln!("--- server log ---\n{}", self.log());
        }
        let _ = std::fs::remove_dir_all(&self.state_dir);
    }
}

fn next_stream(client: &Client, what: &str, wanted: impl Fn(&StreamEvent) -> bool) -> StreamEvent {
    common::next_stream(client, what, Duration::from_secs(30), wanted)
}

#[test]
fn one_client_streams_and_switches_several_outputs() {
    let Ok(binary) = std::env::var("ST_E2E_SERVER_BIN") else {
        eprintln!("ST_E2E_SERVER_BIN not set; skipping");
        return;
    };
    let server = Server::start(&binary);

    // A session that asks for nothing gets the primary output.
    let main = server.connect(OUTPUT_DEFAULT, false);
    let first = next_stream(&main, "default output", |_| true);
    let listed = outputs(&main);
    assert_eq!(listed.len(), 2, "{listed:?}");
    let (primary, second) = (&listed[0], &listed[1]);
    assert!(primary.is_primary && !second.is_primary);
    assert_eq!(first.output_id, primary.id);
    assert_eq!((first.width, first.height), (primary.width, primary.height));
    assert_live(&main, "default output");

    // Moving the session: new output, new size, new epoch, video follows.
    let switch_started = Instant::now();
    main.select_output(second.id).expect("select");
    let moved = next_stream(&main, "switch", |event| event.output_id == second.id);
    let announced = switch_started.elapsed();
    assert_eq!((moved.width, moved.height), (second.width, second.height));
    assert_ne!(moved.video_epoch, first.video_epoch);
    assert_live(&main, "switched output");
    eprintln!(
        "output switch (cold pipeline): stream announced after {announced:?}, 10 frames past its keyframe after {:?}",
        switch_started.elapsed()
    );

    // An aux session shows the output the main one left, at the same time.
    let aux = server.connect(primary.id, true);
    let aux_stream = next_stream(&aux, "aux", |_| true);
    assert_eq!(aux_stream.output_id, primary.id);
    assert_ne!(aux_stream.video_epoch, moved.video_epoch);
    assert_live(&aux, "aux output");
    assert_live(&main, "main output while aux streams");
    assert!(
        aux.recv_audio_packet(Duration::from_millis(500)).is_none(),
        "aux sessions get no audio"
    );

    // A refused switch answers with the stream the session is still on.
    main.select_output(0x00C0_FFEE).expect("select");
    let kept = next_stream(&main, "refused switch", |_| true);
    assert_eq!(
        (kept.output_id, kept.video_epoch),
        (second.id, moved.video_epoch)
    );
    assert_live(&main, "after a refused switch");

    // Joining an output somebody already streams needs no pipeline start.
    let switch_started = Instant::now();
    main.select_output(primary.id).expect("select");
    let joined = next_stream(&main, "join", |event| event.output_id == primary.id);
    assert_eq!(joined.video_epoch, aux_stream.video_epoch);
    assert_live(&main, "joined output");
    eprintln!(
        "output switch (running pipeline): 10 frames past its keyframe after {:?}",
        switch_started.elapsed()
    );
    main.select_output(second.id).expect("select");
    let moved = next_stream(&main, "switch away again", |event| {
        event.output_id == second.id
    });
    assert_live(&main, "switched away again");

    // The aux session leaving stops its pipeline, not the main session's.
    aux.stop();
    drop(aux);
    std::thread::sleep(Duration::from_millis(500));
    assert_live(&main, "after the aux session left");

    // Back to the first output: its pipeline starts again with a fresh epoch.
    main.select_output(primary.id).expect("select");
    let back = next_stream(&main, "switch back", |event| event.output_id == primary.id);
    assert!(back.video_epoch != first.video_epoch && back.video_epoch != moved.video_epoch);
    assert_live(&main, "back on the first output");

    main.stop();
    let log = server.log();
    assert!(
        log.matches("Shared pipeline started").count() >= 3,
        "expected one pipeline per output, twice for the first:\n{log}"
    );
}
