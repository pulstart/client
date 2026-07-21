# st Android Client

Android client using the shared Rust `st-client-core`, Android `MediaCodec`
presentation, and low-latency `AudioTrack` playback.

Android 7.0 / API 24 or newer is required. The Gradle `minSdk` and cargo-ndk
platform are both API 24 because the Rust network interface discovery path uses
`getifaddrs`, which Android exposes from API 24.

## Current scope

- Direct IPv4 TCP + UDP connections on LAN/manual-address paths
- API-signaled remote connections using X25519 key exchange, same-socket STUN
  candidate gathering, ChaCha20-Poly1305 encrypted UDP hole punching, reliable
  control ARQ with safe-MTU fragmentation, and an end-to-end encrypted TCP
  relay fallback
- H.264, SDR, YUV420 video
- Hardware decode to a `SurfaceView`
- Opus stereo audio with redundant-packet, in-band FEC, and PLC recovery
- Server cursor metadata with locally predicted movement; a visible trackpad
  cursor uses cumulative absolute positioning to stay aligned with the host
- Notebook-style trackpad input across the screen: swipe to move, tap to
  left-click, long-press then move or double-tap then move to drag, two-finger
  tap to right-click, and two-finger swipe to scroll
- Floating `K` keyboard control with the Android IME plus a visible, compact,
  horizontally scrollable PC-key panel above it, hardware-key forwarding,
  full-state repair heartbeats, and video resizing above the IME
- Loss feedback and keyframe recovery
- Live LAN server discovery with token filtering
- Token-based API host presence discovery and remote connection setup
- Unified saved, LAN, and API server list with search, reload, path badges,
  stable peer identity, last-connected ordering, and add/remove actions
- Fit, fill/cover, and stretch video scaling
- Immersive fullscreen and a native-style floating connected menu
- Settings for refresh rate, audio, touch input, and screen wake behavior
- Desktop-style Servers, Settings, Update, and About navigation

The authentication token is global, matching the desktop client, and existing
Android profiles are migrated from the old per-server token format. API-only
hosts are connectable: Android persists a distinct client peer identity,
verifies the selected host identity, registers and exchanges candidates through
the API, then carries media/input on encrypted channel 0 and reliable control on
channel 1. Fresh LAN beacons try the lower-overhead direct path first and fall
back to the API tunnel on transport failure. Clipboard, file transfer, output
selection, and game-style pointer capture are not yet
implemented; trackpad input does not provide game-capture mode.

The keyboard keeps physical keys separate from committed text. Android's
installed IME continues to provide its language layout, composition, and
prediction, and committed text is sent as exact Unicode on server input
backends that support it; other backends retain the explicitly
US-layout-dependent ASCII fallback. The PC-key panel exposes latchable
Ctrl/Shift/Alt/Win keys, navigation and function keys through F24, media keys,
and a true numpad. Its `IME` button restores text-keyboard focus. Closing the
panel, disconnecting, or leaving the activity releases all held remote keys.

A stationary long-press sends one left-button hold and releases it on finger
up; it does not add a second click. Moving beyond Android's long-press slop
before activation cancels the hold while preserving normal pointer movement.

The floating menu reports connection, decoder, and audio state. Decoder progress
includes `waiting for H.264 keyframe`, `decoder configured`, and `video active`.

## Build

Install the Android Rust targets and `cargo-ndk` once:

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-ndk
```

```bash
export ANDROID_HOME="$HOME/Android/Sdk"
export ANDROID_SDK_ROOT="$ANDROID_HOME"
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/29.0.14206865"
./gradlew assembleDebug
```

The APK is written to `app/build/outputs/apk/debug/app-debug.apk`.

For an optimized, manually installable APK:

```bash
./gradlew assembleRelease
adb install -r app/build/outputs/apk/release/app-release.apk
```

Release builds use the local Android debug key by default, so they are signed
and installable without Google Play. To keep the same signature across build
machines, set `ST_ANDROID_KEYSTORE`, `ST_ANDROID_STORE_PASSWORD`,
`ST_ANDROID_KEY_ALIAS`, and `ST_ANDROID_KEY_PASSWORD` before building.

Use `10.0.2.2:28480` to reach a server on the host from the Android emulator.
The signaling URL defaults to production and can be overridden at build time
with `./gradlew -PstApiUrl=https://example.test assembleDebug`.
The build includes `armeabi-v7a`, `arm64-v8a`, and `x86_64` native libraries.

The `android/` directory can also be opened directly in Android Studio.
