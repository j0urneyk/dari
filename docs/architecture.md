# Architecture

In Dari, one app is both the host (shares its screen) and the viewer (controls a remote screen). The code is split
into layered crates whose dependencies flow in one direction only. That makes the whole session layer testable
without a UI, and keeps networking and media running even when the GUI stalls for a moment.

```text
dari (app) ──► dari-session ──┬──► dari-net ───┐
                              ├──► dari-media ─┼──► dari-proto
                              └──► dari-input ─┘
dari-relay ──► dari-net, dari-proto
dari-winsvc (Windows only) ──► dari-input, dari-proto
```

## Crates

| Crate | Path | Responsibility | Key dependencies |
| --- | --- | --- | --- |
| `dari-proto` | `crates/proto` | Message types, protocol version, length-bounded framing, message validation. No I/O | serde, postcard, tokio-util |
| `dari-net` | `crates/net` | Device certificates, one-time passwords, SPAKE2 handshake, attempt throttling, QUIC endpoints, mDNS discovery, relay client | quinn, rustls (ring), rcgen, spake2, mdns-sd |
| `dari-media` | `crates/media` | Display enumeration and capture, downscaling, H.264 encode/decode, the capture thread, system audio capture, Opus, playback | xcap, objc2 (ScreenCaptureKit, VideoToolbox), windows (Windows.Graphics.Capture, Direct3D 11, Media Foundation), fast_image_resize, openh264, yuv, cpal, opus-rs |
| `dari-input` | `crates/input` | Input injection, held-key tracking, ⌘↔Ctrl mapping, Windows DPI and cursor handling | enigo, windows |
| `dari-session` | `crates/session` | Host service, host sessions (approval, capture, input, clipboard, file transfer), viewer sessions | tokio, arboard |
| `dari-relay` | `crates/relay` | Rendezvous (ID issuing) and UDP forwarding server binary | quinn, tokio |
| `dari` | `crates/app` | gpui-kit desktop app and the headless CLI (`host`, `connect`) | gpui-kit, clap, directories, toml |
| `dari-winsvc` | `crates/winsvc` | `dari-service.exe`, Windows only: the `DariService` LocalSystem service, the SYSTEM helper it starts for a session, the `install` and `uninstall` commands the installer runs, and `policy on` and `policy off`, which write the `SecureDesktopControl` policy (see [the secure-desktop helper](#the-secure-desktop-helper-windows)). No network code | windows-service, windows, windows-registry |

### External dependencies

Rather than reinventing anything, each area uses a widely adopted crate.

| Area | Crates |
| --- | --- |
| UI | `gpui-kit` 0.7 (re-exports a GPUI snapshot pinned with an `=` version) |
| Async and networking | `tokio`, `tokio-util`, `quinn`, `rustls` (ring provider), `rcgen` |
| Authentication and crypto | `spake2`, `hmac`, `sha2`, `subtle`, `zeroize`, `getrandom` |
| Serialization | `serde`, `postcard` |
| Screen capture and video | `xcap` (display lists), `openh264` (Cisco OpenH264 built from source), `fast_image_resize`, `yuv` (SIMD YUV to BGRA on the viewer); on macOS the `objc2` bindings for ScreenCaptureKit, CoreVideo, CoreMedia, and VideoToolbox; on Windows the `windows` crate for Windows.Graphics.Capture, Direct3D 11, and Media Foundation |
| System audio | `cpal` (WASAPI loopback, Core Audio process tap, playback), `opus-rs` (pure-Rust Opus) |
| Input injection | `enigo`, plus the `windows` crate on Windows |
| Clipboard and LAN discovery | `arboard`, `mdns-sd` |
| Settings, logging, errors, CLI | `directories`, `toml`, `tracing`, `tracing-subscriber`, `thiserror`, `anyhow`, `clap`, `sys-locale` |
| Windows service | `windows-service`, and the `windows` crate for the pipes, tokens, and processes |
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

Screen and input sit behind the `HostPlatform` trait (`displays`, `open_capturer`, `open_input`, `clipboard`,
`open_secure_desktop`). The
real app uses `SystemPlatform`; tests use a synthetic screen and recorded input. That's what lets CI verify the
real path end to end, QUIC and H.264 included.

A capture failure (missing permission, a display mode change, and so on) doesn't end the session. Transient failures
are retried for up to 30 seconds, after which `HostStatus` reports `PermissionDenied` or `Unavailable`. Windows'
secure desktop (a UAC prompt, the lock screen, Ctrl+Alt+Del) is not a failure either. Windows.Graphics.Capture can't
see it: behind the lock screen it delivers nothing, and behind a UAC prompt it keeps delivering the dimmed desktop. The
Windows capturer checks the input desktop every 300 ms, drops frames and reports `SecureDesktop` while the input
desktop is not the user's, and reports `Available` again with the first frame after it. When the probe sees the
user's desktop again, it restarts the capture, because the frames around the switch were dropped and a still screen
sends no more. A new capture session's first frame shows the desktop that came back. The capture thread also forgets
its still frame during a secure desktop, so it never resends a picture of a screen that is gone. While the
[secure-desktop helper](#the-secure-desktop-helper-windows) runs, the stream shows the helper's frames of that
desktop instead, and `HostStatus` stays `Available`.

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
  itself while the consumer is behind. The test `SyntheticCapturer` is polled.
- **Self-paced** sources deliver frames on their own clock (`paces_itself`). The thread waits for the newest frame
  (up to 50 ms, so a stop request is noticed) and drops it if the consumer is behind. A still screen sends no frames,
  so the thread keeps the last one to re-encode as a keyframe when the viewer asks for one, and to refine the
  picture (below). ScreenCaptureKit and Windows.Graphics.Capture are self-paced.

The frame that ends a change (the last scroll step, the keyframe of a page that just opened) is encoded under the
bitrate budget of a moving screen, and nothing follows it while the screen is still, so the viewer would keep that
coarse picture until the next change. Once a self-paced source has delivered nothing for 100 ms, the thread
re-encodes its last frame 12 times, 33 ms apart, and then goes quiet again (`StillRefinement`, explicit state in
the capture loop; a keyframe sent on request is refined the same way). The encoder spends the bits a still screen
saves on the picture it already shows. Measured with the `quality_probe` example (a synthetic 2560×1440 page of
1 px text, 10 Mbps, 30 fps, PSNR-Y of the decoded picture against the source): VideoToolbox lifts a still page from
35.1 dB to 40.5 dB with 12 refinement frames (0.71 MB, 0.4 s); more frames or wider spacing add nothing, and the
same 12 frames reach 40.6 dB at 60 fps and 42.8 dB at 144 fps, where the bitrate is higher. OpenH264 skips most of
the refinement frames to pay off the keyframe and only reaches 35.1 dB (35.8 dB with 20 frames), and sends nothing
at all when they are 100 ms or more apart. Media Foundation's software encoder (the ignored
`still_screen_refinement_through_media_foundation` test, on a Windows 11 VM without a hardware encoder) spends
42 KB on the first refinement frame, from 36.5 dB to 39.1 dB, and 64 bytes on each of the rest; the hardware
encoders of real Windows machines are not measured. The 100 ms delay is three frame intervals at 30 fps, so a dropped frame
does not start a refinement, whose first frame costs about 110 KB; a slow scroll with longer pauses does.

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
doesn't support, are turned off.

VideoToolbox's key frame interval is set to its maximum: left at the default, it put a keyframe every 30 frames,
each as large as the stream's first and encoded under a moving screen's budget, so once a second the picture fell
back to 29.8 dB from the 42 dB the P-frames had reached (`quality_probe`, scrolling at 10 Mbps). VideoToolbox runs
without frame reordering and with its real-time mode **off**. In real-time mode the encoder lowers
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

Turning real-time mode off costs little power. While streaming, the media engine (the `AVE` channel of IOReport's
energy counters, the source `powermetrics` reads; the `power_sample` example reads it without sudo, see
[development.md](development.md)) draws 0.06–0.2 W with real-time mode off.
Real-time mode saves 0.04–0.1 W of that, at most 0.1 Wh per hour of streaming, and it saves it by lowering the
clock, which is where the latency comes from. Every frame gets slower, including isolated ones and frames of slow
streams, so real-time mode can't be enabled only for low frame rates: a keystroke on a still screen would take more
than twice as long to encode. `MaximizePowerEfficiency` behaves the same way (12 ms isolated frames, 99–122 fps from
the 144 Hz source). Real-time mode with `ExpectedFrameRate` raised to 240 kept 30 fps frames at 4.9 ms, but isolated
frames still took 12.5 ms and the 144 Hz source reached only 107 fps. GPU, DRAM, and CPU power differed by less
than the noise, and the stream's own CPU energy stays under 10 mW. Measured on the same M5 with nothing else
running, averaged over 10 s after a 4 s warm-up (test source) or from 6 s on (screen):

| Workload | Real-time off | Real-time on |
| --- | --- | --- |
| 1920×1246, test source at 2 fps (isolated frames) | 5.5 ms, 0.064 W | 12.6 ms, 0.018 W |
| 1920×1246, test source at 30 fps | 4.7 ms, 0.079 W | 9.8 ms, 0.040 W |
| 1920×1246, test source at 144 fps | 144 fps, 4.5 ms, 0.138 W | 106 fps, 14 ms, 0.068 W |
| 2560×1662, test source at 144 fps | 144 fps, 8.0 ms, 0.200 W | 95 fps, 18 ms, 0.121 W |
| 1920×1246, screen at 144 fps (`capture_bench`) | 118 fps, 7.7 ms, 0.135 W | 98 fps, 14.5 ms, 0.057 W |

The clock only drops when the rest of the Mac is quiet. With a virtual machine and compilers keeping the CPU near
20 W, real-time mode kept full speed and both modes measured the same, so compare the modes on an idle machine.

If VideoToolbox fails, the encoder falls back to OpenH264 for the rest of the stream and carries on with a keyframe.
Frames still in flight are delivered first, and anything VideoToolbox outputs after a failed frame is dropped. On a
still screen the thread re-encodes its last frame so the picture is not lost. When the capture resolution changes,
the old session is flushed and the new one starts with a keyframe. The `objc2` calls behind all of this live in
`crates/media/src/apple/`.

On Windows, the hardware encoder is found by what it does rather than by vendor: Media Foundation lists the hardware
transforms that turn NV12 into H.264 on the adapter the capture's device belongs to, and the first one that starts
is used (Intel Quick Sync, NVIDIA NVENC, AMD AMF, and Qualcomm all register one). It runs in low-latency mode with
Constrained Baseline (Baseline where an encoder only knows the older name, which is the same without FMO and ASO), no
B-frames, constant bitrate, and BT.601 limited-range color. Hardware transforms are asynchronous: they ask for input
and announce output through events, which arrive on a Media Foundation thread and are waited for with a one-second
limit, so a stuck encoder falls back to OpenH264 instead of stalling the stream. Each frame is still submitted and
then waited for before the next: unlike VideoToolbox, the Windows encoder doesn't overlap frames, so it delivers
each one before `submit` returns. Output is Annex-B already; the parameter sets from the output
type are put in front of a keyframe that lacks them, and an SPS above level 5.2 is capped the same way as
VideoToolbox's. A machine without a hardware encoder (a VM, a CI runner, a server
without a GPU) uses OpenH264 from the start. The capture shares one Direct3D 11 device with the conversion and the
encoder, and a lost device or a closed capture item restarts the capture on a new device; the encoder follows the
frames to it. The shaders rather than Direct3D 11's video processor do the conversion because they run on any
feature level 10 device, including WARP, the software rasterizer of VMs and CI runners, which have no video
processor, so the same path is tested everywhere. The COM calls live in `crates/media/src/win/`. These two modules are
the only places in the library that need `unsafe`.

| Quality preset | Max long edge | Bitrate at 30 fps |
| --- | --- | --- |
| Speed | 1280px | 1.5 Mbps |
| Balanced (default) | 1920px | 4 Mbps |
| Quality | 3840px (native, up to the encoder's 3840×2160) | 10 Mbps |

Quality sends the screen at its own resolution, within the 3840×2160 the encoder and decoder allow. It used to cap
the long edge at 2560px, but on a Retina screen that scaling blurs text before the codec runs: with the
`quality_probe` example, a 3840px screen scaled to 2560px and back, with no codec at all, came back at only 14.9 dB
PSNR-Y for 1-px text strokes and 32.2 dB for a real Retina screenshot.

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
time to spare. The same test also decodes a synthetic 3840×2160 stream, the largest the Quality preset sends, in
5.0 ms per frame (about 200 fps; 4.9–5.1 ms over three runs, against 2.4–2.6 ms for 2560×1662 in the same runs).
That was measured on the same M5 while a virtual machine kept four of its ten cores busy, and 2560×1662 came out
about 0.3 ms slower than in the table. The cost grows with the pixel count and stays under a 144 fps frame interval
(6.9 ms). The two conversions differ by at most one level in 98% of channels and are equally close to the exact
BT.601 math (about 0.5 levels on average). They part only below black (Y under 16), which `yuv` clamps to black.

### Input coordinates and DPI

The viewer sends the pointer position as coordinates normalized to the captured display (0..=65535), and the host
converts them to the target display's OS coordinates: points on macOS, physical pixels on Windows. On Windows,
enigo's absolute moves are relative to the primary monitor, which puts the pointer in the wrong place on secondary
monitors. So the pointer is moved with `SetCursorPos`, which takes physical virtual-desktop coordinates, and the
process enables Per-Monitor V2 DPI awareness with `SetProcessDpiAwarenessContext` at startup (at runtime rather
than through a manifest). When the display changes, the input coordinate space follows it.

### The secure-desktop helper (Windows)

Windows.Graphics.Capture and `SendInput` can't reach the Winlogon desktop, which shows UAC prompts, the lock screen,
and the Ctrl+Alt+Del screen. Two more processes, both `dari-service.exe`, can. The
[secure desktop design](design/secure-desktop.md) describes the whole plan. The helper reports which desktop receives
input, captures every desktop other than `Default` with DXGI Desktop Duplication, and injects the viewer's input
on `Winlogon`. Ctrl+Alt+Del comes later (#48).

```text
dari.exe (user, medium integrity)
  ├── \\.\pipe\dari-service ──► dari-service.exe service (LocalSystem, session 0)
  │                                   vets the client, starts the helper
  └── \\.\pipe\dari-helper-<random> ◄── dari-service.exe helper (SYSTEM, the user's session)
        ▲                               reports the input desktop every 100 ms,
        │                               duplicates the selected display off Default,
        │                               injects the app's OsInput off Default
        └── frame section (read-only in dari.exe) ◄── the helper writes RGBA frames
```

1. After the host user approves a viewer, the host session calls `HostPlatform::open_secure_desktop` with `input`
   set only if control is allowed. `SystemPlatform` returns a `SecureDesktopLink` on Windows and `None` elsewhere.
2. The link's task creates `\\.\pipe\dari-helper-` plus 32 random hex digits (one instance, local clients only,
   open to SYSTEM and the app's logon SID), connects to `\\.\pipe\dari-service` (waiting with `WaitNamedPipeW`
   while every instance is busy, 5 seconds at most for the whole exchange), sends `StartHelper { pipe, input }`, and
   reads `HelperStarted` or `Refused(reason)`.
3. The service serves up to four clients at once, one per pipe instance and thread. Before it reads a byte, it opens
   the client's process and checks that its image is `dari.exe` in the service's own folder and that its session is
   active. It then finds that session's `winlogon.exe` (the system directory's, running as LocalSystem), duplicates
   its token, removes every privilege but `SeChangeNotifyPrivilege`, and starts
   `dari-service.exe helper <pipe> <input|no-input> <handle>` suspended on `winsta0\default`, inheriting exactly
   one handle: a duplicate of the app's process handle. The helper joins a job that kills it when the service
   exits, and then resumes. The service keeps one helper per session. A second `StartHelper` from the app process
   that helper serves means the app's earlier session ended, so the service ends that helper and starts another.
4. The helper restricts DLL loading to System32, logs its identity, connects to the app's pipe, and checks that the
   pipe's server is the process behind its inherited handle. Its first message names the input desktop. The app
   reads it, impersonates the client to check that it is LocalSystem, and only then accepts it.
5. The session logs each `DesktopChanged` and drops the link when it ends (the service is missing or refused, the
   helper never connected or failed the check, or its pipe closed), without starting another helper that session.
   At the session's end it drops the link first. The link's task then sends `Stop`, and the helper exits.

#### Capturing the secure desktop

The session owns the link, and each capture thread borrows a read-only `SecureDesktopView` of it, so `SelectDisplay`,
`SetQuality`, and `SetFrameRate` restart capture without touching the helper. The session tells the link which
display the viewer watches, before the first capture and before each display switch, and the link forwards it to the
helper as `SelectDisplay`.

The helper runs two threads, and a third for input when it was started with input (see
[Answering the secure desktop](#answering-the-secure-desktop)). The main thread is the pipe's only reader and passes
the app's messages to the screen thread. The screen thread is the pipe's only writer, so the pipe's order is the
order things happened: a `Frame`
after `DesktopChanged(Winlogon)` shows Winlogon. The screen thread polls the input desktop every 100 ms
(`ScreenMachine` in `screen.rs`). On any desktop but `Default` it attaches to that desktop with `SetThreadDesktop`,
finds the DXGI output whose `HMONITOR` matches the selected display on any adapter, creates a Direct3D 11 device on
that adapter, and duplicates the output. `dxgi_result.rs` maps every DXGI result to an action:

- `DXGI_ERROR_WAIT_TIMEOUT` from `AcquireNextFrame` means the screen is still.
- Any other failure of `AcquireNextFrame`, of the copy, of `GetFramePointerShape`, or of `ReleaseFrame` means the
  desktop switched or the mode changed. The helper drops the duplication, checks the desktop at once, and
  duplicates again: at once if the duplication showed an image, otherwise 100 ms later. In the VM, `ACCESS_LOST`
  comes on the way back to `Default`.
- `E_ACCESSDENIED` from `DuplicateOutput` is retried after each desktop check, and reported as `ScreenUnavailable`
  after 5 seconds. `DXGI_ERROR_UNSUPPORTED`, `DXGI_ERROR_SESSION_DISCONNECTED`, a rotated output, and a display
  that matches no output are reported at once, until the desktop or display changes.

A new duplication publishes nothing until its first frame with `AccumulatedFrames` above 0: the first frame can be a
pointer-only update whose texture is black. A duplication that has no image after 1 second is replaced. When no
attempt shows an image for 5 seconds after a desktop or display change, or after a duplication with an image ended,
the helper reports `ScreenUnavailable` once and keeps retrying. Frames don't include the pointer, so the helper draws
the shape from `GetFramePointerShape` at the frame's pointer position (`pointer.rs`), and forgets the pointer when the
desktop changes. It logs each duplication's start, its first image's `AccumulatedFrames`, and a summary when it ends.
Of attempts that show no image, it logs only the first and one summary of the rest.

Pixels never cross the pipe. The helper creates an unnamed section laid out by `dari_proto::FrameLayout` (a header page,
then two page-aligned slots of tightly packed RGBA) and duplicates a handle to it into the app's process with
`FILE_MAP_READ` only, sent as `FrameSection`. The app can't write to the section, so it acknowledges frames over the
pipe. `AppChannel` (`channel.rs`) publishes a frame as `Frame { display, slot, sequence }`. The app owns that slot until
it answers with `RequestFrame`, which it sends for every `Frame`, kept or not. The helper writes only the other slot,
keeps the newest frame there as a draft, and publishes the draft when the credit comes back. A desktop change, a display
change, the end of the duplication that drew the draft, or `ScreenUnavailable` discards the draft. A size change waits
until the app owns no slot, then sends a new `FrameSection` and closes the helper's handle to the old one. The app's
view keeps the old section alive until the app maps the new one. However the link ends, the app sends `Stop` and reads
for up to 5 seconds until the helper closes its pipe, closing the handle of every `FrameSection` it reads meanwhile.
Otherwise a section the helper made as the link ended would stay allocated in `dari.exe` until the app exits.

In the app, the link's task maps each section read-only, copies a published slot into an `RgbaFrame` on a blocking
thread, and checks the slot's sequence word before and after the copy. It copies only frames of the selected display
on a desktop other than `Default`. The capture stream runs `TwoSourceCapturer` (`secure_desktop/capturer.rs`): the
platform's capturer on `Default`, and the helper's frames on any other desktop while the helper is connected. A
switch changes only where frames come from. The stream keeps its encoder. The capture that sees the switch reports it
through `ScreenCapturer::take_source_change`, and the stream then drops its still frame and makes the next frame a
keyframe, so nothing from the old source is sent again. Each capturer serves only the display its stream was opened
for. On each return to `Default` it opens
Windows.Graphics.Capture again, so a frame queued before the switch, such as the dimmed desktop behind a prompt, is
never shown. While the helper is connected, the platform capturer's own `SecureDesktop` report is ignored, because the
helper reports every switch. Before the helper connects and after its link ends, that report passes through, and the
viewer gets PR 36's notice.

#### Answering the secure desktop

The host session's input thread runs one `InputSession` over a `Router` backend (`input_route.rs`).
`HostPlatform::open_input` still returns the platform's backend, and the router holds it next to a `SecureInput`
handle on the link. The handle doesn't keep the link open, so dropping the link still stops the helper. Before each
input command, `follow_route` reads the link's route. While the helper is connected and reports `Winlogon`, input goes
to the helper. Otherwise it goes to the platform's backend. When the target changes,
`InputSession::retarget` releases every held key and button through the old target first. Windows clears key state
when the input desktop switches, so releasing at the next command instead of at the switch leaves nothing down. To
the helper, the router sends each `InputBackend` call as `OsInput` (`Move` in physical virtual-desktop pixels,
`Button`, `Scroll`, `Key`, `Text`), which the link's task writes to the pipe as `AppToHelper::Input`. A view-only
session has no input thread, so it routes nothing.

The helper makes itself Per-Monitor V2 DPI aware before it starts any thread, as the app does. With input, its main
thread hands each `Input` to the input thread over a queue of 64. When the queue is full, the main thread waits, so
the pipe pushes back on the app and no input is lost. The input
thread (`injector.rs`, which has no `unsafe` and is tested on every platform) checks the input desktop every 100 ms
while idle and before each event. When the desktop changes, it opens a new handle with only
`DESKTOP_JOURNALPLAYBACK`, attaches to it with `SetThreadDesktop`, and forgets what its `Injector` holds without injecting anything, because
Windows cleared key state at the switch. It replays
input through the `Injector` and `EnigoBackend` only while it is attached to `Winlogon`, and
drops input while an attach fails. When the app sends `Stop`, closes its pipe, or exits while the thread is attached to
`Winlogon`, the thread releases what it holds there, and the helper logs how many inputs it released and why. A helper started without input has no input thread
and drops every `Input`.

The service reads the `SecureDesktopControl` DWORD under `HKLM\SOFTWARE\Policies\Dari` for every request
(`policy.rs`, through the safe `windows-registry` API). A missing value or 1 is on, and anything else is off. At
off, `admit` turns `StartHelper { input: true }` into `input: false` and refuses `SendSas` with `PolicyOff`.
`dari-service.exe policy on|off` writes the value and needs an administrator. The host settings' switch
(`secure_desktop/win/policy.rs`, re-exported from `dari-session`) reads the value the same way and appears only when
`dari-service.exe` sits beside `dari.exe`. A change runs `dari-service.exe policy on|off` with `ShellExecuteExW` and
the `runas` verb on a background thread, waits for the UAC prompt and the command, and reads the value again.

Both pipes carry `dari-proto`'s local messages (`crates/proto/src/local.rs`) in 64 KiB postcard frames, and every
message passes `Validate`. The service and the helper write to the Application event log under the source
`DariService`, which `dari-service.exe install` registers with `%SystemRoot%\System32\EventCreate.exe` as its
message file and `uninstall` removes it. They log the service's start, a pipe it can't create, each helper it starts
and each client it refuses (at most 10 refusals a minute; more are closed without a reply or a log entry), and the
helper's user, integrity level, session, privileges, and why it exited. The entry for a helper started with input
is a warning with ID 3, and each `policy on|off` writes ID 4, naming the user who changed the value.
`dari-service.exe` is built without a console, and a test reads its import
table to check that it never links `ws2_32.dll` or another Windows networking DLL.

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
| `keymap.rs` | GPUI `Keystroke` → protocol `KeyCode`, and on macOS, telling a modifier chord macOS consumed from a deliberate modifier tap |
| `input_source.rs` (macOS) | Reads the Mac's selected keyboard input source and decides when a switch of it becomes a Hangul/English tap on the host |
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

macOS consumes the keys that switch its input source (Caps Lock, Ctrl+Space, the Globe key), so no Hangul/English
key ever reaches the viewer. Instead, the viewer subscribes to GPUI's `on_keyboard_layout_change`, which the macOS
platform fires on `NSTextInputContextKeyboardSelectionDidChangeNotification`, and reads the selected source from
`NSTextInputContext`. While the viewer window is active, a source id that differs from the last one seen since the
window became active sends one `HangulMode` press and release if the host allows control. The window activates
before the host's status arrives, so the source is followed even before control is known. The notification also fires
when the window activates (macOS restores the window's own source) and twice per Ctrl+Space, with the same id both
times; comparing ids ignores both.

macOS also keeps the key of some modifier chords for itself (Ctrl+Space, ⌘Space for Spotlight), so the viewer sees
only the modifiers go down and up. Forwarded as they are, they reach the host as a lone modifier tap. On Windows a
lone Win tap opens Start, and with shortcut translation on, the Mac's Control becomes Win. The input source
notification arrives only after Control is up (5 to 210 ms later in testing), too late to put the Hangul key
inside the chord. Instead, on every modifier change the viewer asks Core Graphics how long ago a key went down at the
HID level (`CGEventSourceSecondsSinceLastEventType`), which counts the keys macOS kept. If a key went down while the
modifiers were held and none reached the viewer, it taps F20, a key no shortcut uses, before the modifiers come up.
A deliberate modifier tap has no key under it and still reaches the host as a tap. Releasing everything when the
window loses focus takes the same path, so the record of held modifiers starts over.

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
