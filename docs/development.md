# Developing Playwake

The interface, Xbox sign-in and discovery, and session signaling run in Rust
and GPUI Kit. A local PyGObject worker handles WebRTC, hardware decode, audio,
input and presentation through GStreamer. It receives SDP and ICE over a
subprocess pipe. The browser opens for Microsoft sign-in.

## Run from source

Install GStreamer, PyGObject with the Gst/GstWebRTC/GstSdp/GstVideo typelibs,
GStreamer WebRTC, ICE and SCTP plugins, H.264/Opus plugins, X11 video output,
and Python evdev. An AMD VA-API H.264 decoder is preferred. Run:

```sh
cargo run --locked
```

The media worker can be placed beside the executable or selected with
`ALLY_MEDIA_WORKER`. Set `ALLY_GST_PYTHON` for the interpreter and
`ALLY_GST_RUNTIME` for a private GStreamer library directory. On Gamescope,
`ALLY_GAMESCOPE_FOCUS=1` brings the X11 app window to the front. No SteamOS
system package changes are needed if these dependencies are installed in a
private runtime. The client defaults to a 1080p request at a 20 Mbit/s limit.
The resolution picker also offers 720p, 1440p, and experimental 800p and
900p requests. Xbox controls the transmitted size. The app and the journal
report the actual decoder dimensions separately from the requested size.

## Build a Flatpak

On x86-64 SteamOS or Fedora 44, install Flatpak, the Freedesktop 26.08 SDK from
Flathub, FFmpeg, binutils, libXRes and xprop, plus the Rust build dependencies
listed in the [Linux setup action](../.github/actions/setup-linux-rust/action.yml).

```sh
cargo test --release --locked
cargo build --release --locked
packaging/flatpak/build-bundle.sh target/release/playwake Playwake-steamos-x86_64.flatpak
```

## Publish a release

Pull requests run the Rust tests. Pushing a version tag runs the tests, then
builds and smoke-tests the Flatpak in a separate job. The publish job takes
the bundle and SHA-256 checksum from the build job.

To publish, update the package version in `Cargo.toml` and `Cargo.lock` and add
the matching AppStream release entry in `packaging/flatpak/`. On `main`, tag
the version commit as `vMAJOR.MINOR.PATCH` and push the tag. The workflow
verifies the version and publishes a GitHub Release after the Flatpak passes
its checks. GitHub generates the release notes. The release attaches
`Playwake-steamos-x86_64.flatpak` and `SHA256SUMS`.

## Platform ports

- Linux uses X11/XWayland embedding, VA-API and evdev.
- Windows has decoder selection for D3D11/D3D12, generic GStreamer video/audio
  sinks, and a Rust gilrs gamepad adapter. It needs a matching MSVC GPUI build,
  Python/PyGObject and GStreamer runtime.
- macOS has decoder selection for VideoToolbox, generic GStreamer video/audio
  sinks, and the same gilrs adapter. It needs a matching GPUI build and the
  GStreamer/PyGObject runtime.

Windows and macOS are portability paths, not validated builds. The prototype
opens a separate GStreamer video window on non-X11 platforms. Validation must
cover dependency packaging, graphics sink behavior, permissions, gamepad
mapping, and playback before either port is distributed.

## Sign-in and session storage

`XBOX_CLIENT_ID` can override the public OAuth app ID when deploying an
appropriately registered app. The default is the public Xbox client ID used
by the interoperable device flow. The app does not bundle a client secret,
copy any other application's credentials, or persist short-lived access
tokens. It saves the Microsoft refresh token locally and rotates it when
Microsoft returns a replacement. On Unix the session file is accessible only
to the current user (mode 0600). The file is not encrypted at rest. The
optional `ALLY_SESSION_FILE` environment variable overrides its location.
**Sign out** removes the stored session from this device. When Microsoft
expires or revokes the saved token, confirm sign-in again.

## LSFG on Linux

Enable **LSFG 2× Performance · Flow Scale 1.0** before choosing an Xbox.
The setting is saved locally and applied when each stream starts. LSFG uses
the optimized VA-API NV12-to-Vulkan video path and your configured bitrate limit.
To change the setting, stop the current stream first.

Install the lsfg-vk 2.x Vulkan layer and select the `lsfg-vk` beta branch
of Lossless Scaling in Steam. The app looks for the Vulkan layer in the
usual implicit-layer directories and for `lsfg-vk.dll` in Steam's default
library. On other installations set `ALLY_LSFG_LAYER_DIR` to the directory
containing the layer manifest and `ALLY_LSFG_DLL` to the DLL's path.
Vulkan output for the switch requires X11 or XWayland with libXRes and the
GStreamer VA-API and Vulkan plugins.

## Diagnostics

The app writes one JSON event per line to standard error. To save a local
journal, run the binary with `2>>playwake.log`, then follow it with
`tail -f playwake.log`. The journal contains sign-in and stream stages,
HTTP failure statuses, console counts, and negotiated video dimensions and
decoder. It omits codes, tokens, console IDs, and SDP/ICE payloads.
Run the binary with `--probe-resolutions` after signing in once to test 720p,
1080p and 1440p in sequence against a single available Xbox. The probe records
decoded frame sizes and five-second video counters and stops each stream
after 16 seconds of video.

Run the binary with `--probe-latency` for a 720p and 1080p comparison at an
SDP limit of 20,000 kbit/s. It requires NumPy and an active gameplay scene
with a stationary camera. It injects a repeatable right-stick motion at the
input loop and uses decoded frame motion to detect the response. It records
input-loop-to-data-channel time, first RTP packet to video-sink entry time
(before the jitter buffer when the sample is tagged `pre_jitter`), and
input-loop-to-first-detected-turn time. These measurements stop at the sink
and exclude display scanout. Put the console in the same scene for both modes.

`--probe-transport` runs the 1080p test and requests the RTP absolute
capture-time extension. It records capture metadata, the selected ICE
candidate type, and the remote video RTCP Sender Report clock gap. The
`*_wall_ms` fields compare different clocks. To estimate the capture time
on the local clock, add `sender_report_wall_gap_ms - RTT / 2` to the remote
timestamp. This requires a separately measured RTT when the WebRTC statistics
omit it, and assumes similar transit time in both directions.

Set `ALLY_VIDEO_RENDERER=vulkan` to use GStreamer's Vulkan output with the
existing hardware decoder. On the Linux VA-API path the app uploads NV12
and converts it in Vulkan when the converter is available. Set
`ALLY_VULKAN_CONVERSION=cpu` for the CPU BGRA path. The decoded-frame queue
holds at most two buffers and drops old queued frames under load.

Run `--probe-vulkan` for a 1080p, 20,000 kbit/s run with camera motion and
queue counters. On Linux/VA-API it analyzes downsized luma frames in a
separate, single-buffer branch. Set `ALLY_VULKAN_MOTION_PROBE=inline` to
reproduce the old inline benchmark. The stage profile reports queue wait,
CPU-side frame preparation, upload-to-sink time and inline analysis time,
followed by a flag indicating whether motion analysis was active. The
parallel analyzer is timed separately. The probe can run with an external
lsfg-vk layer. The base sink counts source buffers only. Count generated
frames after lsfg-vk in the Vulkan layer chain. Vulkan submission and
physical display scanout are different measurement points.
