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
| `dari-media` | `crates/media` | Display enumeration and capture, downscaling, H.264 encode/decode, the capture thread, system audio capture, Opus, playback | xcap, objc2 (ScreenCaptureKit, VideoToolbox), fast_image_resize, openh264, yuv, cpal, opus-rs |
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
| Screen capture and video | `xcap`, `openh264` (Cisco OpenH264 built from source), `fast_image_resize`, `yuv` (SIMD YUV to BGRA on the viewer); on macOS the `objc2` bindings for ScreenCaptureKit, CoreVideo, CoreMedia, and VideoToolbox |
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

The capture thread encodes a frame only when the consumer is keeping up: no encoded frame is waiting in the channel,
and fewer than `FRAMES_IN_FLIGHT` (2) frames are being encoded. Each frame reserves its channel slot (a tokio
`OwnedPermit`) before it goes to the encoder, and the encoder sends it into that slot the moment it is done, so a
frame never waits for the one after it. When the network falls behind, frames are skipped **before encoding**,
because dropping an encoded frame would break the reference chain for the next P-frame. The host session's video
channel holds `FRAMES_IN_FLIGHT` frames; with the rule above, at most one encoded frame waits there for the network,
as before, and the second slot only lets the encoder overlap frames. Capture sits behind the `ScreenCapturer`
trait, which comes in two kinds:

- **Polled** sources capture whenever asked. The thread paces them at the target frame rate and skips the capture
  itself while the consumer is behind. xcap (Windows) and the test `SyntheticCapturer` are polled.
- **Self-paced** sources deliver frames on their own clock (`paces_itself`). The thread waits for the newest frame
  (up to 50 ms, so a stop request is noticed) and drops it if the consumer is behind. A still screen sends no frames,
  so the thread keeps the last one to re-encode as a keyframe when the viewer asks for one. ScreenCaptureKit is
  self-paced.

| | macOS | Windows |
| --- | --- | --- |
| Capture | ScreenCaptureKit (`apple::ScreenCaptureKitCapturer`): frames arrive only when the screen changes, at most `max_fps`, already scaled and converted to NV12 on the GPU | xcap: a full RGBA screenshot per frame |
| Scaling | Done by ScreenCaptureKit | `FrameScaler` on the CPU |
| Encoding | VideoToolbox in hardware (`apple::HardwareEncoder`), reading the capture's IOSurface without a copy | OpenH264 |

Downscaling works on the long edge, rounds width and height to even numbers, and stays within OpenH264's limit
(3840×2160), which the decoder enforces too.

Both encoders produce the same stream: H.264 Constrained Baseline, Annex-B, BT.601 limited-range color, keyframes
only at the start, on a resolution change, and on request. OpenH264 runs in its `ScreenContentRealTime` mode. In
this mode, frame skipping must be on for the encoder to hold its target bitrate; a skipped frame simply isn't
output, so the reference chain is unaffected. Adaptive quantization and background detection, which screen content
doesn't support, are turned off.

VideoToolbox runs without frame reordering and with its real-time mode **off**. In real-time mode the encoder lowers
its clock after about three seconds to just keep up with `ExpectedFrameRate`, assuming frames overlap: a frame that
took 4.5 ms at first took 8–11 ms once it settled when frames were encoded one at a time, and 16 ms with three in
flight. Without it, a 1920×1246 frame stays at about 4.5 ms. Frames are submitted without waiting
(`VTCompressionSessionEncodeFrame` and no `CompleteFrames`), and the output callback hands each one, in order, to
the delivery it was submitted with. An isolated frame, like a keystroke on a still screen, leaves as soon as it is
encoded. The low-latency rate control (`EnableLowLatencyRateControl`) is left off. Measured with real-time mode
off, it made each frame slower (4.6 → 6.4 ms at 1920×1246 and 7.6 → 12.8 ms at 2560×1662 with the 144 Hz test
source, so 2560×1662 fell to 122 fps; 7.8 → 9.5 ms and 12 → 14.7 ms on the screen). On screen content it spent
only about a fifth of the target bitrate (0.65 instead of 3.5 Mbit/s at 4 Mbit/s). What it does well is even out
frame sizes: the largest frame was 32–53 KB instead of 109–130 KB, so it is worth trying again if bursts ever
overwhelm slow links. The output is AVCC with the SPS and PPS kept in the format description, so the encoder rewrites it as
Annex-B and puts the parameter sets in front of each keyframe. VideoToolbox picks the H.264 level from the
macroblock rate, and 2560×1662 at 144 fps comes out as level 6.0, whose parameter sets OpenH264 rejects. The
encoder lowers the SPS's `level_idc` to 5.2. The level only states a throughput, and frame size and reference
frames stay within 5.2's limits.

If VideoToolbox fails, the encoder falls back to OpenH264 for the rest of the stream and carries on with a keyframe.
Frames still in flight are delivered first, and anything VideoToolbox outputs after a failed frame is dropped. On a
still screen the thread re-encodes its last frame so the picture is not lost. When the capture resolution changes,
the old session is flushed and the new one starts with a keyframe. The `objc2` calls behind all of this live in
`crates/media/src/apple/`, the only place in the crate that needs `unsafe`.

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

Measured on an M5 MacBook (built-in 120 Hz display) with `capture_bench` and the ignored
`hardware_stream_keeps_up_with_144_fps` test, which feeds the real stream path prepared frames from a 144 Hz
clock ([development guide](development.md#tests)):

| | Before: one frame at a time, real-time mode | Now |
| --- | --- | --- |
| 1920×1246, 144 Hz source (test), after it settles | 84 fps, 9.3 ms per frame | 144 fps, 4.5 ms per frame |
| 2560×1662, 144 Hz source (test), after it settles | 59 fps, 13.4 ms per frame | 144 fps, 7.7 ms per frame |
| 1920×1246, screen, 10 s (`capture_bench`) | 96 fps, 10 ms per frame | 117 fps, 7.8 ms per frame |
| 2560×1662, screen, 10 s (`capture_bench`) | 76 fps, 13 ms per frame, and viewers couldn't decode it | 118 fps, 12.3 ms per frame |

The test's "before" figures come from the new code with `FRAMES_IN_FLIGHT` set to 1 and real-time mode on; the
screen's come from the previous commit. Five-second runs used to look better (about 111 fps at 1920×1246), because
the slowdown starts about three seconds in. On the screen, the 120 Hz panel and the moving content cap the frame
rate, and real screen content takes longer to encode than the test's pattern. Both fixes are needed: with real-time
mode off but one frame at a time, 2560×1662 managed only 73 fps, because 8 ms per frame is longer than a 144 fps
frame interval. Before ScreenCaptureKit and VideoToolbox, the same Mac streamed 18 and 13.5 fps.

On the viewer, OpenH264 decodes each frame to I420, and the `yuv` crate (yuvutils-rs) converts it to BGRA in one
SIMD pass with the same BT.601 limited-range matrix the encoders use. OpenH264's own `write_rgba8` followed by an R/B
swap used to take about four times as long as decoding: on 2560×1662 screen content, about 2 ms decoding and 8.5 ms
converting. Allocating a fresh buffer per frame costs about 0.1 ms, and splitting the conversion across threads
saved only about 0.3 ms, so neither is done. Measured on an M5 MacBook with `capture_bench` (10 s of the screen, the
same build before and after) and the ignored `decoding_keeps_up_with_144_fps` test (a synthetic 2560×1662 stream):

| Decoding to BGRA | Before: `write_rgba8` and a swap | Now: `yuv420_to_bgra` |
| --- | --- | --- |
| 1920×1246, screen (`capture_bench`) | 5.2 ms per frame (about 190 fps) | 1.3 ms per frame (about 775 fps) |
| 2560×1662, screen (`capture_bench`) | 9.6 ms per frame (about 105 fps) | 2.4 ms per frame (about 420 fps) |
| 2560×1662, synthetic (test) | 9.0 ms per frame | 2.2 ms per frame |

The Quality preset used to cap the viewer at about 105–115 fps whatever the host sent; it now decodes 144 fps with
time to spare. The two conversions differ by at most one level in 98% of channels and are equally close to the exact
BT.601 math (about 0.5 levels on average). They part only below black (Y under 16), which `yuv` clamps to black.

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
  the latest frame, and takes it out of the channel with `ViewerHandle::take_frame` so its pixels move into the
  `RenderImage` without a copy. A decode failure sends `RequestKeyframe`.
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
| `text.rs` | Korean and English UI strings (chosen in the settings, or by system locale) |
| `backdrop.rs` | Prepares the background picture off the main thread: the picture, sharp at the top and softening and fading out towards the window's middle, a small blurred copy faded the same way for the sidebar, and the average color of the part that shows, which tints the veil and sets how much it covers; it also holds the loaded picture as a global, so every page's panels can frost it |
| `style.rs` | Dari's mostly monochrome light and dark palette over gpui-kit's theme (follows the system appearance), the 14px rem that sets the app's density, window options (transparent title bar, blurred translucent background), the extra Lucide icons and app icon the app embeds, and shared building blocks such as frosted-glass panels, sidebar rows, setting rows, callouts, and the segmented control |
| `cli.rs` | Headless `host`/`connect` |

The viewer window creates a new `RenderImage` for every frame and releases the previous image from the GPU atlas
with `window.drop_image`. A GUI test confirmed that without it, six seconds of streaming grows memory by about
280 MB. The decoded frame itself is not copied: the window takes it out of the frame channel
(`ViewerHandle::take_frame`) instead of cloning its `Arc`, so the decoder's buffer becomes the image's. The copy it
used to make cost about 0.4 ms at 1920×1246 inside the app, and about 0.3 ms at 2560×1662 for a bare copy of a
freshly decoded buffer, on the main thread of an M5 in a release build. Rendering the window, including the upload
to a fresh atlas texture, takes about 0.25 ms of the 6.9 ms a 144 fps frame allows on the main thread.

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
