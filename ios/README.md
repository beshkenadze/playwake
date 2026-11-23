# Playwake for iOS

A low-latency Xbox Remote Play / Xbox Cloud Gaming client. The Linux
GStreamer worker is replaced by Google libwebrtc (via the `stasel/WebRTC`
Swift package) plus VideoToolbox, while all the Xbox protocol work is reused
from the desktop engine as a static Rust library.

## Layout

```
ios/
  project.yml                  XcodeGen manifest (source of truth)
  scripts/build-rust-core.sh   builds ios-core and lipos it into Rust/
  PlaywakeIOS/
    App/                       SwiftUI entry point
    Core/playwake_core.h       C ABI of the Rust core
    Core/PlaywakeCore.swift    Swift wrapper (JSON events, SDP/ICE, packets)
    Session/XboxRTC.swift      libwebrtc peer connection, handshake, input
    Session/SessionController.swift  app state per Rust event
    Session/Gamepad.swift      GCController -> Xbox wire masks
    Media/                     low-latency renderer + loopback lab
    UI/                        SessionView (real client) and LabView
ios-core/                      Rust staticlib: protocol core + C ABI
```

`ios-core` is a separate Cargo workspace on purpose: it uses `reqwest` with
`native-tls` (Security.framework) while the desktop binary uses rustls, and
keeping them apart avoids feature unification.

## What is reused from the desktop engine

| Desktop | iOS |
| --- | --- |
| `src/xbox.rs`, `src/cloud.rs`, `src/session_store.rs`, `src/journal.rs` | compiled unchanged into `ios-core` via `#[path]` |
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
xcodegen generate
xcodebuild -project PlaywakeIOS.xcodeproj -scheme PlaywakeIOS \
  -destination 'platform=iOS Simulator,name=iPhone 18 Pro' build
```

The pre-build phase runs `scripts/build-rust-core.sh`, which builds the core
for the platform Xcode targets and writes `ios/Rust/libplaywake_core.a`.

Launch arguments:

* `-autosignin` starts the Microsoft device-code flow automatically.
* `-autolab` runs the in-process libwebrtc loopback lab instead of the client.

## Latency lab

`-autolab` sends synthetic 720p60 frames through H.264 encode, transport,
VideoToolbox decode and `AVSampleBufferDisplayLayer`. A binary barcode inside
the frame encodes the capture time, so the reported `e2e` latency is real
capture-to-render time and does not depend on clock sync. Measured on the iOS
Simulator: 60 fps decode/render/encode, jitter buffer ~13 ms, decode ~1.5 ms,
RTT ~0.3 ms, **~19-20 ms capture-to-render**.

## Notes

* The simulator shares the Mac network stack, so the lab ignores the WiFi and
  Ethernet adapters; the live client leaves them enabled to reach a console on
  the LAN.
* The app configures `RTCAudioSession` for playback with a 5 ms IO buffer.
* `NSLocalNetworkUsageDescription` is required and already set; libwebrtc
  prompts on the first UDP socket.
