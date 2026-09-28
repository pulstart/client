# 0.12.17

Works with any 0.9.x host.

- Lower, self-tuning playout latency. The video buffer held every frame a
  fixed ~6 ms (at 120 fps), and after a single Wi-Fi hiccup it kept every
  later frame waiting as long as that hiccup for the rest of the session (up
  to 25 ms). Frames now show as soon as they are decoded; small jitter is
  still evened out, holding a frame at most half a frame interval, and that
  hold drains back to zero within about half a second once the network calms.
- macOS: when the video is presented through Metal (already synced to the
  display), the menu layer no longer also waits for vsync, which could
  delay a new frame by one extra refresh. Decided automatically.

# 0.12.16

Works with any 0.9.x host; update the host to 0.9.17 for load-resistant
streaming.

- Scroll sensitivity setting (0.1x-5x) in Settings and live in the floating
  menu while connected. Low settings accumulate fractional steps instead of
  dropping scroll.
- Receive/decode, input and audio threads run at elevated priority; Windows
  uses 1 ms timers, disables EcoQoS and raises the process class so a busy
  client machine does not stall playback.
- Built against FFmpeg 9-compatible bindings.
