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
| `dari-media` | `crates/media` | Display enumeration and capture, downscaling, H.264 encode/decode, paced capture thread | xcap, fast_image_resize, openh264 |
| `dari-input` | `crates/input` | Input injection, held-key tracking, ⌘↔Ctrl mapping, Windows DPI and cursor handling | enigo, windows |
| `dari-session` | `crates/session` | Host service, host sessions (approval, capture, input, clipboard), viewer sessions | tokio, arboard |
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
| Screen capture and video | `xcap`, `openh264` (Cisco OpenH264 built from source), `fast_image_resize` |
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

- **Capture thread** (host): capture → downscale → H.264 encode at the target frame rate. Platform capture handles
  aren't `Send`, so they're opened inside the thread through a factory closure.
- **Input thread** (host): injects received input events with enigo. The enigo backend stays on this thread too.
- **Decode thread** (viewer): decodes H.264 to BGRA and publishes only the latest frame on a `watch` channel.

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
   If control is allowed, the input thread and clipboard sync start.
4. **During the session**: control-stream messages are handled. `SelectDisplay` and `SetQuality` reopen only the
   capture stream (the new encoder starts with a keyframe); the video pump carries on and the input coordinate
   space follows the new display. `RequestKeyframe` asks the encoder for a keyframe.
5. **End**: the host sends `Disconnect` and waits up to 1 second for the peer to close the connection. Events left
   in the input queue are dropped, and keys and buttons still held are released. The service issues a new
   password.

Screen and input sit behind the `HostPlatform` trait (`displays`, `open_capturer`, `open_input`, `clipboard`). The
real app uses `SystemPlatform`; tests use a synthetic screen and recorded input. That's what lets CI verify the
real path end to end, QUIC and H.264 included.

A capture failure (missing permission, the secure desktop, and so on) doesn't end the session. Transient failures
are retried for up to 30 seconds, after which `HostStatus` reports `PermissionDenied` or `Unavailable`.

### Capture and backpressure

The capture thread captures and encodes only when the channel has room (`try_reserve`). When the network falls
behind, frames are skipped **before encoding**, because dropping an encoded frame would break the reference chain
for the next P-frame. Downscaling works on the long edge, rounds width and height to even numbers, and stays
within OpenH264's limit (3840×2160).

The encoder uses OpenH264's `ScreenContentRealTime` mode. In this mode, frame skipping must be on for the encoder
to hold its target bitrate; a skipped frame simply isn't output, so the reference chain is unaffected. Adaptive
quantization and background detection, which screen content doesn't support, are turned off. When the capture
resolution changes, the encoder is recreated and starts with a keyframe. Capture sits behind the `ScreenCapturer`
trait; the real implementation is `XcapCapturer`, and tests use `SyntheticCapturer`, which draws a moving pattern.

| Quality preset | Max long edge | Bitrate |
| --- | --- | --- |
| Speed | 1280px | 1.5 Mbps |
| Balanced (default) | 1920px | 4 Mbps |
| Quality | 2560px | 10 Mbps |

The frame rate is capped at 30. The viewer requests a preset and the host maps it to its own limits
(`host_session.rs`).

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

## Desktop app

`crates/app` is split into a library and a thin binary (`main.rs`). GPUI's macOS platform can only be created on
the main thread, so the GUI tests (`tests/gui.rs`, `harness = false`) need to start the library themselves.

| Module | Role |
| --- | --- |
| `lib.rs` | Logging setup and argument parsing. GUI without a subcommand, CLI with one |
| `home.rs` | Home window: a sidebar (this device's status, nearby and recent devices) beside one page at a time. The "This device" page has the addresses, password, relay ID, approval card, permission notice, and settings; the "Control a remote device" page has the connect form. A connection request switches to the "This device" page |
| `viewer.rs` | Viewer window: paints frames on a `canvas` with `paint_image` and turns input into protocol events. Toolbar (display, quality, frames per second and round-trip latency, disconnect) and the overlays for waiting, approval, and a finished session |
| `video_layout.rs` | Letterbox computation and window → normalized coordinate conversion |
| `keymap.rs` | GPUI `Keystroke` → protocol `KeyCode` |
| `state.rs`, `runtime.rs` | App state (device certificate, settings) and the tokio runtime, kept as GPUI globals |
| `settings.rs`, `config.rs` | Saving and loading `settings.toml`, the data directory, address and ID parsing, local address list |
| `permissions.rs` | Checking and requesting macOS Screen Recording and Accessibility permissions (refreshed every 3 seconds) |
| `text.rs` | Korean and English UI strings (chosen by system locale) |
| `style.rs` | Dari's mostly monochrome light and dark palette over gpui-kit's theme (follows the system appearance), the 14px rem that sets the app's density, window options (transparent title bar, blurred translucent background), the extra Lucide icons and app icon the app embeds, and shared building blocks such as sidebar rows, setting rows, callouts, and the segmented control |
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
