# 0.13.0

Requires host 0.10.0: the wire format changed, update both together.

- Multi-screen. The floating menu has a display picker; the choice is
  remembered per server. Every other host monitor can open in its own
  window, and with several local monitors each one gets a host monitor
  automatically (setting "auto layout", on by default; on Wayland the
  compositor places the windows, so they open as normal windows to move).
  Rotated host monitors are shown upright. Android: display picker in the
  menu, and an attached external display shows a second host monitor.
- Connecting over the internet no longer gives up on hole punching after
  one failed signaling request: that sent the session straight to the
  relay. Android now makes two punch rounds like the desktop client and
  needs one signaling round trip instead of six.
- Hole-punched sessions no longer freeze and drop after packet loss (see
  host 0.10.0), and a client that disconnects tells the host, so an
  immediate reconnect is not held up for 5 s.

The extra windows and the Android external display have not been run on
real hardware yet.

# 0.12.18

Works with any 0.9.x host; update the host to 0.9.23 for the full effect.

- Audio latency and glitches. The playback buffer held a fixed 60 ms; it now
  sizes itself from the jitter it actually sees (20 ms floor, up to 60 ms)
  and steers clock drift by dropping or repeating single samples, so a
  quiet LAN plays with ~40 ms less delay. A lost 5 ms packet was concealed
  with 120 ms of made-up sound and then clipped with a click; it is now
  exactly 5 ms. The output device is opened with a small buffer, follows
  the system default device, and is rebuilt if the stream errors (a
  Bluetooth headset connecting used to silence audio for the session).
- Input: a key tapped and released within one frame is no longer lost; a
  lost key-up is repaired even while the mouse keeps moving (the repair
  never ran during continuous motion); slow mouse movement keeps its
  fractional part instead of vanishing; on Linux/Windows Ctrl chords no
  longer arrive as Ctrl+Super.
- Windows and NAT-traversed sessions wait for packets instead of sleeping in
  20 ms steps, which added 10 ms on average to every frame.
- Android: frames show as soon as they are decoded instead of one unit
  later.

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
