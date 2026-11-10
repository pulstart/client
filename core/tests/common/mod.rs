#![allow(dead_code)]

use st_client_core::{Client, OutputInfo, StreamEvent};
use std::time::{Duration, Instant};

pub const FRAME_IDR: u8 = 2;
pub const FRAME_RECOVERY: u8 = 5;

/// The next stream the client is told about that `wanted` accepts; the
/// client must accept a stream before its video is delivered. An announcement
/// the server has already replaced (an encoder rebuild right behind it) is
/// refused, and the next one follows.
pub fn next_stream(
    client: &Client,
    what: &str,
    timeout: Duration,
    wanted: impl Fn(&StreamEvent) -> bool,
) -> StreamEvent {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(event) = client.take_stream_event() {
            if wanted(&event)
                && client.accept_stream_generation(event.generation, event.video_epoch)
            {
                return event;
            }
        }
        assert!(
            Instant::now() < deadline,
            "{what}: no matching stream (status: {})",
            client.status()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn outputs(client: &Client) -> Vec<OutputInfo> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let listed = client
            .take_control_snapshot()
            .and_then(|snapshot| snapshot.outputs)
            .map(|outputs| outputs.value.outputs);
        if let Some(listed) = listed {
            return listed;
        }
        assert!(Instant::now() < deadline, "server never listed its outputs");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What an app does every frame: adopt the stream the server announced. Every
/// encoder replacement (bitrate, frame rate) is a new epoch, and its video
/// stays gated until accepted.
pub fn accept_announced_stream(client: &Client) {
    if let Some(event) = client.take_stream_event() {
        client.accept_stream_generation(event.generation, event.video_epoch);
    }
}

/// A stream is live once a keyframe arrived and frames keep following it.
pub fn assert_live(client: &Client, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut after_keyframe = None::<usize>;
    let mut units = 0usize;
    while after_keyframe.is_none_or(|count| count < 10) {
        assert!(
            Instant::now() < deadline,
            "{what}: no live video ({units} units, {after_keyframe:?} after a keyframe, status: {})",
            client.status()
        );
        accept_announced_stream(client);
        let Some(unit) = client.recv_access_unit(Duration::from_millis(50)) else {
            continue;
        };
        units += 1;
        match after_keyframe.as_mut() {
            Some(count) => *count += 1,
            None if matches!(unit.frame_type, FRAME_IDR | FRAME_RECOVERY) => {
                after_keyframe = Some(0)
            }
            None => {}
        }
    }
}
