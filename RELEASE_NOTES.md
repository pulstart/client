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
