# Architecture

In Dari, one app is both the host (shares its screen) and the viewer (controls a remote screen). The code is split
into layered crates whose dependencies flow in one direction only. That makes the whole session layer testable
without a UI, and keeps networking and media running even when the GUI stalls for a moment.

```text
dari (app) ──► dari-session ──┬──► dari-net ───┐
                              ├──► dari-media ─┼──► dari-proto
                              └──► dari-input ─┘
dari-relay ──► dari-net, dari-proto
```

## Crates

| Crate | Path | Responsibility | Key dependencies |
| --- | --- | --- | --- |
| `dari-proto` | `crates/proto` | Message types, protocol version, length-bounded framing, message validation. No I/O | serde, postcard, tokio-util |
| `dari-net` | `crates/net` | Device certificates, one-time passwords, SPAKE2 handshake, attempt throttling, QUIC endpoints, mDNS discovery, relay client | quinn, rustls (ring), rcgen, spake2, mdns-sd |
| `dari-media` | `crates/media` | Display enumeration and capture, downscaling, H.264 encode/decode, the capture thread, system audio capture, Opus, playback | xcap, objc2 (ScreenCaptureKit, VideoToolbox), windows (Windows.Graphics.Capture, Direct3D 11, Media Foundation), fast_image_resize, openh264, cpal, opus-rs |
| `dari-input` | `crates/input` | Input injection, held-key tracking, ⌘↔Ctrl mapping, Windows DPI and cursor handling | enigo, windows |
| `dari-session` | `crates/session` | Host service, host sessions (approval, capture, input, clipboard, file transfer), viewer sessions | tokio, arboard |
| `dari-relay` | `crates/relay` | Rendezvous (ID issuing) and UDP forwarding server binary | quinn, tokio |
| `dari` | `crates/app` | gpui-kit desktop app and the headless CLI (`host`, `connect`) | gpui-kit, clap, directories, toml |

### External dependencies

Rather than reinventing anything, each area uses a widely adopted crate.

| Area | Crates |
| --- | --- |
| UI | `gpui-kit` 0.7 (re-exports a GPUI snapshot pinned with an `=` version) |
| Async and networking | `tokio`, `tokio-util`, `quinn`, `rustls` (ring provider), `rcgen` |
| Authentication and crypto | `spake2`, `hmac`, `sha2`, `subtle`, `zeroize`, `getrandom` |
| Serialization | `serde`, `postcard` |
| Screen capture and video | `xcap` (display lists), `openh264` (Cisco OpenH264 built from source), `fast_image_resize`; on macOS the `objc2` bindings for ScreenCaptureKit, CoreVideo, CoreMedia, and VideoToolbox; on Windows the `windows` crate for Windows.Graphics.Capture, Direct3D 11, and Media Foundation |
| System audio | `cpal` (WASAPI loopback, Core Audio process tap, playback), `opus-rs` (pure-Rust Opus) |
| Input injection | `enigo`, plus the `windows` crate on Windows |
| Clipboard and LAN discovery | `arboard`, `mdns-sd` |
| Settings, logging, errors, CLI | `directories`, `toml`, `tracing`, `tracing-subscriber`, `thiserror`, `anyhow`, `clap`, `sys-locale` |
| Packaging | `cargo-packager` |

## Threads and executors

The app runs two executors side by side. GPUI runs the UI on the main thread, and a multi-threaded tokio runtime
runs networking and sessions. The tokio runtime is installed as a GPUI global (`runtime::TokioRuntime`), and the
UI side gets results by awaiting tokio channels or `JoinHandle`s from GPUI tasks. tokio's synchronization
primitives work no matter which executor awaits them, so no bridge between the two is needed.

Work that is CPU-heavy or uses blocking APIs runs on dedicated OS threads.

- **Capture thread** (host): capture → downscale → H.264 encode at up to the target frame rate. Platform capture
  handles aren't `Send`, so they're opened inside the thread through a factory closure.
- **Input thread** (host): injects received input events with enigo. The enigo backend stays on this thread too.
- **Decode thread** (viewer): decodes H.264 to BGRA and publishes only the latest frame on a `watch` channel.
- **Audio capture thread** (host): reads the cpal loopback stream, maps it to 48 kHz stereo, and encodes 20 ms Opus
  packets. A task sends them as datagrams. It runs only while the viewer asks for audio.
- **Audio playback thread** (viewer): decodes Opus packets (concealing lost ones), converts them to the output
  device's rate and channels, and feeds a jitter buffer that the device's callback drains.

## Host flow

`dari_session::start_host` opens a `HostEndpoint` (the QUIC server) and spawns the host service task. The UI
sends commands through a `HostHandle` (regenerate password, accept on/off, change approval and clipboard policy,
end session) and receives state on a `HostEvent` channel (`PasswordChanged`, `ApprovalRequested`,
`SessionStarted`, `SessionStatus`, `SessionEnded`, `Relay`).

1. **Authentication**: `HostEndpoint` runs a handshake for each incoming connection, concurrently (at most 8,
   10 seconds each). Throttled sources are refused before TLS. On success the connection claims the session slot
   and consumes the password. Details are in the [security model](security.md) and the
   [protocol](protocol.md#handshake).
2. **Approval**: with approval on, the host sends `AwaitingApproval` and waits for the host user's decision (Allow
   control / View only / Decline, 30-second limit). Nothing is created in the meantime: no capture, no input, no
   clipboard. Input that arrives is dropped.
3. **Session start**: the host sends the display list and `HostStatus` and opens the capture stream. A video pump
   task lives for the whole session and writes packets from the capture stream to the unidirectional video stream.
   If control is allowed, the input thread and clipboard sync start, and with file transfer on the host grants the
   viewer stream credit and starts accepting its file streams.
4. **During the session**: control-stream messages are handled. `SelectDisplay`, `SetQuality`, and `SetFrameRate`
   reopen only the capture stream (the new encoder starts with a keyframe); the video pump carries on and the
   input coordinate space follows the new display. A quality or frame rate request sent while approval is pending
   is remembered and applies from the start. Without approval, the host waits for the viewer's first control
   message, its `SetFrameRate`, before streaming (up to 2 seconds), so the stream doesn't start at the default rate
   and restart one round trip later. `RequestKeyframe` asks the encoder for a keyframe.
5. **End**: the host sends `Disconnect` and waits up to 1 second for the peer to close the connection. Events left
   in the input queue are dropped, and keys and buttons still held are released. The service issues a new
   password.

Screen and input sit behind the `HostPlatform` trait (`displays`, `open_capturer`, `open_input`, `clipboard`). The
real app uses `SystemPlatform`; tests use a synthetic screen and recorded input. That's what lets CI verify the
real path end to end, QUIC and H.264 included.

A capture failure (missing permission, the secure desktop, and so on) doesn't end the session. Transient failures
are retried for up to 30 seconds, after which `HostStatus` reports `PermissionDenied` or `Unavailable`.

### Capture and backpressure

The capture thread captures and encodes only when the channel has room. When the network falls behind, frames are
skipped **before encoding**, because dropping an encoded frame would break the reference chain for the next
P-frame. Capture sits behind the `ScreenCapturer` trait, which comes in two kinds:

- **Polled** sources capture whenever asked. The thread paces them at the target frame rate and skips the capture
  itself while the channel is full. The test `SyntheticCapturer` is polled.
- **Self-paced** sources deliver frames on their own clock (`paces_itself`). The thread waits for the newest frame
  (up to 50 ms, so a stop request is noticed) and drops it if the channel is full. A still screen sends no frames,
  so the thread keeps the last one to re-encode as a keyframe when the viewer asks for one. ScreenCaptureKit and
  Windows.Graphics.Capture are self-paced.

| | macOS | Windows |
| --- | --- | --- |
| Capture | ScreenCaptureKit (`apple::ScreenCaptureKitCapturer`): frames arrive only when the screen changes, at most `max_fps`, already scaled and converted to NV12 on the GPU | Windows.Graphics.Capture (`win::GraphicsCaptureCapturer`): frames arrive in a free-threaded frame pool only when the screen changes, and the capture thread takes the newest at most `max_fps` times a second (Windows 11 24H2 and later also stop drawing faster, through the session's minimum update interval) |
| Scaling | Done by ScreenCaptureKit | Two pixel shader passes (`win::convert`) scale the frame and render it straight into the planes of an NV12 texture, with OpenH264's own coefficients and 2×2 chroma averaging |
| Encoding | VideoToolbox in hardware (`apple::HardwareEncoder`), reading the capture's IOSurface without a copy | The adapter's Media Foundation hardware H.264 encoder (`win::HardwareEncoder`), reading the capture's texture without a copy; OpenH264 on machines without one |

Downscaling works on the long edge, rounds width and height to even numbers, and stays within OpenH264's limit
(3840×2160), which the decoder enforces too.

Every encoder produces the same stream: H.264 Constrained Baseline, Annex-B, BT.601 limited-range color, keyframes
only at the start, on a resolution change, and on request. OpenH264 runs in its `ScreenContentRealTime` mode. In
this mode, frame skipping must be on for the encoder to hold its target bitrate; a skipped frame simply isn't
output, so the reference chain is unaffected. Adaptive quantization and background detection, which screen content
doesn't support, are turned off. VideoToolbox runs a real-time session without frame reordering. Its low-latency
rate control is left off: it made each encode 15–20% slower on Apple silicon. Each frame is encoded synchronously (`CompleteFrames`), which keeps the
one-frame-at-a-time backpressure above. Its output is AVCC with the SPS and PPS kept in the format description, so
the encoder rewrites it as Annex-B and puts the parameter sets in front of each keyframe. If VideoToolbox fails, the
encoder falls back to OpenH264 for the rest of the stream and carries on with a keyframe. When the capture resolution
changes, the encoder is recreated and starts with a keyframe. The `objc2` calls behind all of this live in
`crates/media/src/apple/`.

On Windows, the hardware encoder is found by what it does rather than by vendor: Media Foundation lists the hardware
transforms that turn NV12 into H.264 on the adapter the capture's device belongs to, and the first one that starts
is used (Intel Quick Sync, NVIDIA NVENC, AMD AMF, and Qualcomm all register one). It runs in low-latency mode with
Constrained Baseline (Baseline where an encoder only knows the older name, which is the same without FMO and ASO), no
B-frames, constant bitrate, and BT.601 limited-range color. Hardware transforms are asynchronous: they ask for input
and announce output through events, which arrive on a Media Foundation thread and are waited for with a one-second
limit, so a stuck encoder falls back to OpenH264 instead of stalling the stream. Each frame is still submitted and
then waited for before the next, as with VideoToolbox. Output is Annex-B already; the parameter sets from the output
type are put in front of a keyframe that lacks them. A machine without a hardware encoder (a VM, a CI runner, a server
without a GPU) uses OpenH264 from the start. The capture shares one Direct3D 11 device with the conversion and the
encoder, and a lost device or a closed capture item restarts the capture on a new device; the encoder follows the
frames to it. The shaders rather than Direct3D 11's video processor do the conversion because they run on any
feature level 10 device, including WARP, the software rasterizer of VMs and CI runners, which have no video
processor, so the same path is tested everywhere. The COM calls live in `crates/media/src/win/`. These two modules are
the only places in the crate that need `unsafe`.

| Quality preset | Max long edge | Bitrate at 30 fps |
| --- | --- | --- |
| Speed | 1280px | 1.5 Mbps |
| Balanced (default) | 1920px | 4 Mbps |
| Quality | 2560px | 10 Mbps |

The frame rate is a separate choice. The viewer asks for one with `SetFrameRate` (its "Auto" is the fastest refresh
rate among its own displays, up to 144), and the host streams at that rate, capped by the refresh rate of the
display it captures: the screen can't change faster than that. The host reports the result with `FrameRate`, and
sends it again when switching displays changes it. A viewer that doesn't ask gets the host's default, 30 fps. The bitrate
grows with the frame rate as `bitrate × (fps / 30)^0.75`, up to 50 Mbps: a faster stream needs more bits, but less
than proportionally more, because consecutive frames differ less. The host maps all of this in
`host_session.rs` (`stream_settings`).

On an M5 MacBook, encoding a 1920×1246 frame takes about 8 ms, enough for about 120 fps, and a 2560×1662 frame
about 10 ms (about 100 fps). Before ScreenCaptureKit and VideoToolbox, the same Mac streamed 18 and 13.5 fps. Encoding one frame at a time is what limits it: the
hardware itself encodes faster when frames overlap. Measure your own setup with `capture_bench`
([development guide](development.md#tests)).

### Input coordinates and DPI

The viewer sends the pointer position as coordinates normalized to the captured display (0..=65535), and the host
converts them to the target display's OS coordinates: points on macOS, physical pixels on Windows. On Windows,
enigo's absolute moves are relative to the primary monitor, which puts the pointer in the wrong place on secondary
monitors. So the pointer is moved with `SetCursorPos`, which takes physical virtual-desktop coordinates, and the
process enables Per-Monitor V2 DPI awareness with `SetProcessDpiAwarenessContext` at startup (at runtime rather
than through a manifest). When the display changes, the input coordinate space follows it.

## Viewer flow

`connect_viewer` connects to the target (`ViewerTarget::Direct(address)` or `Relay { relay, id }`), authenticates,
and returns a `ViewerHandle` and a `ViewerEvent` channel.

- Video stream → decode thread (queue of 4) → `watch::Sender<Option<Arc<DecodedFrame>>>`. The UI always sees only
  the latest frame. A decode failure sends `RequestKeyframe`.
- Input goes through a queue of 512, of which 128 slots are reserved for key and button events. Even when the
  network falls behind, pointer moves can't fill the queue and cause key releases to be dropped.
- A cleanly ended video stream doesn't end the session; the control stream decides when the session is over.
- Clipboard sharing turns on only when the host allows control, and turns off if the host later reports view-only.
- `ViewerConfig::frame_rate` is always sent as `SetFrameRate`, the first control message, right after
  authentication, and `set_frame_rate` changes it later.
- One task accepts every unidirectional stream the host opens for the whole session and routes it by its kind: the
  video stream to the decoder, file streams to the session's transfers.

## Audio

`dari-media`'s `audio.rs` holds the pipeline and `HostPlatform::open_audio` opens the capturer, so tests use a
synthetic tone. cpal records system output with a Core Audio process tap on macOS, whose functions exist only from
macOS 14.2. The app's `build.rs` therefore weak-links Core Audio, which keeps the app launching on macOS 13,
and `SystemAudioCapturer::open` refuses below macOS 14.6 without touching those functions (a unit test checks the
binary's Core Audio link is weak). Devices nearly always run at 48 kHz; other rates are converted by linear
interpolation.

## File transfer

`transfer.rs` holds `Transfers`, one state machine shared by host and viewer sessions. The session task feeds it
control messages (`FileOffer`, `FileAccept`, `FileDone`, `FileCancel`), incoming file streams, the local user's
commands (send, accept, cancel), and results from its send and receive tasks, and sends whatever control message
it returns. Each accepted file gets a task that copies between disk and its stream in 64 KiB chunks and reports
progress at most every 200 ms. Snapshots (`Transfer`) reach the UI as `HostEvent::Transfer` and
`ViewerEvent::Transfer`. The host accepts the viewer's files itself; the viewer waits for its user. Received files
go to the user's Downloads folder (`config::downloads_directory`).

## Desktop app

`crates/app` is split into a library and a thin binary (`main.rs`). GPUI's macOS platform can only be created on
the main thread, so the GUI tests (`tests/gui.rs`, `harness = false`) need to start the library themselves.

| Module | Role |
| --- | --- |
| `lib.rs` | Logging setup and argument parsing. GUI without a subcommand, CLI with one |
| `home.rs` | Home window: a sidebar (this device's status, nearby and recent devices) beside one page at a time. The "This device" page has the addresses, password, relay ID, approval card, permission notice, and settings; the "Control a remote device" page has the connect form. A connection request switches to the "This device" page |
| `viewer.rs` | Viewer window: paints frames on a `canvas` with `paint_image` and turns input into protocol events. Toolbar (display, quality, frame rate menu, frames per second and round-trip latency, disconnect) and the overlays for waiting, approval, and a finished session |
| `video_layout.rs` | Letterbox computation and window → normalized coordinate conversion |
| `transfers.rs` | The transfer list shown in the host panel and under the viewer toolbar (progress, save/decline, cancel, show in folder) |
| `keymap.rs` | GPUI `Keystroke` → protocol `KeyCode` |
| `state.rs`, `runtime.rs` | App state (device certificate, settings) and the tokio runtime, kept as GPUI globals |
| `settings.rs`, `config.rs` | Saving and loading `settings.toml`, the data directory, address and ID parsing, local address list |
| `permissions.rs` | Checking and requesting macOS Screen Recording and Accessibility permissions (refreshed every 3 seconds) |
| `text.rs` | Korean and English UI strings (chosen by system locale) |
| `backdrop.rs` | Prepares the background picture off the main thread: the picture, sharp at the top and softening and fading out towards the window's middle, a small blurred copy faded the same way for the sidebar, and the average color of the part that shows, which tints the veil and sets how much it covers; it also holds the loaded picture as a global, so every page's panels can frost it |
| `style.rs` | Dari's mostly monochrome light and dark palette over gpui-kit's theme (follows the system appearance), the 14px rem that sets the app's density, window options (transparent title bar, blurred translucent background), the extra Lucide icons and app icon the app embeds, and shared building blocks such as frosted-glass panels, sidebar rows, setting rows, callouts, and the segmented control |
| `cli.rs` | Headless `host`/`connect` |

The viewer window creates a new `RenderImage` for every frame and releases the previous image from the GPU atlas
with `window.drop_image`. A GUI test confirmed that without it, six seconds of streaming grows memory by about
280 MB.

gpui-kit's `Root` uses Tab, Shift-Tab, and ⌘C/Ctrl+C for focus movement and copying. In the remote screen's key
context (`RemoteScreen`) these keys are unbound with `NoAction` so they reach the remote device. When the window
loses focus, every held key, button, and modifier is released so nothing stays pressed on the remote side.

## Relay

The relay knows nothing about session contents. The host registers by presenting its device certificate as a TLS
client certificate and receives a nine-digit ID. When a viewer asks to connect by ID, the relay allocates a pair of
UDP ports, and the two sides run the same QUIC + SPAKE2 session as a direct connection through them. On the host,
relayed connections go through `RelayedAcceptor` into `HostEndpoint`'s authentication path unchanged, so the
password, single-session slot, and approval apply exactly as for direct connections; failed attempts are throttled
by the viewer IP the relay reports. Both sides refresh their binding every 10 seconds, so the relay follows a device
whose NAT mapping changes mid-session. If registration
drops, the host service re-registers with exponential backoff from 2 to 60 seconds.

Operations are covered in [relay operations](relay.md) and the message format in the [protocol](protocol.md#relay).

## LAN discovery

Hosts advertise the `_dari._udp.local.` service over mDNS (name, OS, and the first 4 bytes of the certificate
fingerprint). Viewers only show this in the "Nearby devices" list; connecting always goes through password
authentication. Instance names are truncated to the DNS label limit (63 bytes), and a device's own advertisement
is filtered out by its fingerprint hint. Loopback addresses are useless to other devices and are dropped from
results (included only in same-machine tests).
