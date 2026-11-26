# Playwake for iOS

A low-latency Xbox Remote Play / Xbox Cloud Gaming client. The Linux
GStreamer worker is replaced by Google libwebrtc (via the `stasel/WebRTC`
Swift package) plus VideoToolbox, while all the Xbox protocol work is reused
from the desktop engine as a static Rust library.

## Layout

```
ios/
  Project.swift                Tuist manifest (source of truth)
  scripts/build-rust-core.sh   builds ios-core and lipos libplaywake_ffi.a into Rust/
  PlaywakeIOS/
    App/                       SwiftUI entry point
    Core/playwake_core.h       C ABI of the Rust core
    Core/PlaywakeCore.swift    Swift wrapper (JSON events, SDP/ICE, packets)
    Session/XboxRTC.swift      libwebrtc peer connection, handshake, input
    Session/SessionController.swift  app state per Rust event
    Session/Gamepad.swift      GCController -> Xbox wire masks
    Media/                     low-latency renderer + loopback lab
    UI/                        SessionView (real client) and LabView
ios-core/                      Rust staticlib `playwake_ffi`: the C ABI over core/
core/                          `playwake-core`, shared with the SteamOS app
```

There is one core for every device: `core/` (`playwake-core`) holds the Xbox
and GameStream protocols, catalogs, pairing, library, covers and sessions;
the SteamOS app, its media worker and this client all depend on it. Only the
UI and the platform's own media decode and present differ. On Apple targets
the core's `reqwest` uses `native-tls` (Security.framework), elsewhere rustls.
`ios-core` stays a separate Cargo workspace with its own lock file.

## What the core gives the iOS client

| Desktop | iOS |
| --- | --- |
| `core/src/{xbox,cloud,session_store,journal,library}.rs` | linked as `playwake-core`, exposed by `ios-core` |
| OAuth device code, XSTS, xHome/xCloud signaling, provisioning, keepalive | Rust core, async events over a C callback |
| SDP offer munging (`b=AS`), candidate exchange, Teredo synthesis | Rust core (`pw_munge_offer`, `pw_sdp`, `pw_ice`) |
| 38-byte input packet, 15-byte connect packet | Rust core (`pw_input_packet`, `pw_connect_packet`) |
| Handshake JSON on `control`/`message` | Rust core (`pw_handshake`, `pw_is_handshake_ack`) |
| GStreamer `webrtcbin` pipeline | libwebrtc `RTCPeerConnection` in Swift |
| VA-API/Vulkan/X11/evdev | VideoToolbox + `AVSampleBufferDisplayLayer`, GameController |

The iOS offer mirrors the worker: audio OPUS and video H264 recvonly
transceivers, `max-bundle`, four ordered data channels (`input`/`1.0`,
`control`/`controlV1`, `message`/`messageV1`, `chat`/`chatV1`), message-channel
`Handshake` open, and the full control handshake plus input loop only after
`HandshakeAck`.

## Build and run

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
cd ios
tuist generate
xcodebuild -workspace PlaywakeIOS.xcworkspace -scheme PlaywakeIOS \
  -destination 'platform=iOS Simulator,name=iPhone 18 Pro' build
```

Signing is automatic with team `Q8H6GWJ658` (Apple Development) for every
platform; set it in `Project.swift`, not in Xcode, because `tuist generate`
rewrites the project. A device build needs `-allowProvisioningUpdates` the
first time, so that Xcode can create the team profile:

```sh
xcodebuild -workspace PlaywakeIOS.xcworkspace -scheme PlaywakeIOS \
  -destination 'platform=iOS,name=<device>' -allowProvisioningUpdates build
```

The pre-build phase runs `scripts/build-rust-core.sh`, which builds the core
for the platform Xcode targets and writes `ios/Rust/libplaywake_ffi.a`.

Launch arguments:

* `-autosignin` starts the Microsoft device-code flow automatically.
* `-autolab` runs the in-process libwebrtc loopback lab instead of the client.
* `-autostream` starts the first console as soon as the console list arrives.
* `-inputMs <n>` sets the input send cadence in milliseconds, clamped to 1–16
  (default 8, i.e. 125 Hz). `lab.log` records the achieved packets/s and the
  first non-idle controller snapshot, so a live run shows the wire rate and
  that physical input reached the console protocol.
* `-streamHeight <720|1080|1440>` picks the initial stream resolution
  (default 720); the in-app picker still overrides it afterwards.

For quick live testing there is `scripts/live.sh`: it builds the Catalyst
target, launches it with `-autostream`, activates the window (GameController
needs focus on macOS) and tails `~/Documents/lab.log`. Example:
`./scripts/live.sh --height 1080 --input-ms 4`.

On Mac Catalyst the GameController framework only exposes controllers to the
focused app, so a pad that was connected before launch is adopted as soon as
the Playwake window is focused (the app polls for it once per second).

## Latency lab

`-autolab` sends synthetic 720p60 frames through H.264 encode, transport,
VideoToolbox decode and `AVSampleBufferDisplayLayer`. A binary barcode inside
the frame encodes the capture time, so the reported `e2e` latency is real
capture-to-render time and does not depend on clock sync. Measured on the iOS
Simulator after forcing zero playout delay: 60 fps decode/render/encode,
jitter buffer ~0.5 ms, decode ~2 ms, RTT ~0.5 ms, **~6 ms
capture-to-renderer callback**. A `CADisplayLink` also measures the
enqueue-to-next-vsync wait (`pres`): ~5.5 ms on the simulator, so
capture-to-refresh is ~11.5 ms. On Mac Catalyst the same lab measures ~11 ms
to the callback, but ~9 ms of that is the harness H.264 encoder; the client
receive path stays ~2 ms.

## Live stream

With a saved session (the app keeps `playwake-session*.json` in Documents),
`-autostream` connects to the first console and starts streaming. The
`XboxRTC` stats timer appends one `LabStats` summary per second to `lab.log`,
in the same format as the lab, so live jitter buffer, decode, submit, vsync
wait and RTT can be read from the device or simulator log. Measured on the
iOS Simulator at 720p60: jitter buffer ~2.3 ms, decode ~4.1 ms, submit
~0.08 ms, vsync wait ~6.4 ms, RTT ~4 ms, no dropped frames. The barcode-based
`e2e` stays 0 live because console frames carry no timestamp.

`lab.log` is a Debug-only diagnostic: `LabLog.write` compiles to nothing in
Release, so the shipping build never touches the disk. The file is rotated at
2 MiB in Documents (`lab.log` moves to `lab.log.1`, replacing the previous
one, for at most ~4 MiB total). Production telemetry uses
`SessionEvents.shared.publisher` instead: `SessionController` publishes each
sample there and the bus throttles it to one `stats` event every 5 seconds,
so an analytics or telemetry sink can attach without touching the streaming
code.

The video view's `CADisplayLink` asks for 120 Hz, and `Info.plist` sets
`CADisableMinimumFrameDurationOnPhone`, so ProMotion iPhones present each
decoded frame on the next vsync instead of waiting up to a 60 Hz interval.

## Notes

* The simulator shares the Mac network stack, so the lab ignores the WiFi and
  Ethernet adapters; the live client leaves them enabled to reach a console on
  the LAN.
* Audio goes through `PlayoutAudioDevice`, a playout-only `RTCAudioDevice` on
  RemoteIO with a 5 ms IO buffer. The default libwebrtc ADM opens
  VoiceProcessingIO, which enables the microphone: iOS terminates the app
  without `NSMicrophoneUsageDescription`, and voice processing adds latency.
* The audio jitter buffer (NetEq) targets the 0.6 quantile of packet arrival
  delay instead of WebRTC's 0.95: on Wi-Fi that held ~70-90 ms of audio
  instead of ~115 ms with under 0.05% concealed samples (`-netEqQuantile <q>`
  overrides it).
* `WebRTC-Network-UseNWPathMonitor` is enabled. Without it this libwebrtc
  build types interfaces by name, so on a Mac every `en*` is Wi-Fi and real
  Wi-Fi (`en0`) outranks a wired adapter (`en7`): the stream ran over Wi-Fi
  with Ethernet plugged in. With the monitor the wired interface wins. The
  5 s `net` line in `lab.log` shows the selected path and its addresses.
* Smoothing (forced playout delay) defaults to 10 ms: it costs ~0.3 ms over
  Off and cuts render stalls by a third, while 50 ms added ~33 ms and still
  froze (`-playoutMs <n>` overrides it).
* `scripts/device-trials.sh` measures a live stream on a physical iPhone:
  input-to-camera-turn trials plus the client pipeline (jitter buffer,
  decode, renderer, display, input wait). It needs a game with a
  right-stick camera on the console.
* `NSLocalNetworkUsageDescription` is required and already set; libwebrtc
  prompts on the first UDP socket.
