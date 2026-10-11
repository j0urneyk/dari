# Development

## Setup

| Item | Details |
| --- | --- |
| Rust | 1.99.0. `rust-toolchain.toml` pins the version and the `rustfmt`/`clippy` components, so rustup installs them automatically |
| macOS | Xcode Command Line Tools. The GUI tests need Metal |
| Windows 11 | Visual Studio Build Tools (MSVC) |
| Tools | [cargo-deny](https://github.com/EmbarkStudios/cargo-deny), and [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) 0.11.8 for packaging |

If another version manager such as asdf puts an older `cargo` earlier on `PATH`, `rust-toolchain.toml` can be
ignored. Check that `cargo --version` reports 1.99.0 first.

gpui-kit re-exports a GPUI snapshot pinned with an `=` version. Upgrading gpui-kit changes all of GPUI, so do it
in its own PR, never mixed with other changes.

The workspace uses resolver 3, edition 2024, and `rust-version = "1.99"`, and shared dependency versions are
managed in `[workspace.dependencies]` in the root `Cargo.toml`. Dependencies are built with `opt-level = 2` even in
the dev profile, because GPUI and the codecs are unusably slow without optimization; our own crates stay
unoptimized so they remain debuggable. The release profile uses thin LTO and `codegen-units = 1`.

## Quality gates

Every PR must pass all of the following, and CI runs the same commands.

```bash
cargo fmt --all --check
```

```bash
cargo lint
```

```bash
cargo test --workspace --locked
```

```bash
cargo test -p dari --test gui
```

```bash
cargo deny check
```

- **Formatter**: `rustfmt.toml` (stable options only).
- **Linter**: `cargo lint` is an alias in `.cargo/config.toml` for
  `clippy --workspace --all-targets --locked -- -D warnings`. The rules live in `[workspace.lints]` in the root
  `Cargo.toml`: `unsafe_code = "deny"`, clippy `all` and `pedantic`, and warnings for `unwrap_used`, `expect_used`,
  `print_stdout`, `print_stderr`, `dbg_macro`, `todo`, and `unimplemented`. `wildcard_imports` (for the gpui-kit
  prelude), `similar_names`, and noisy documentation lints are allowed. In tests, `clippy.toml` allows
  `unwrap`/`expect`.
- **cargo-deny** (`deny.toml`): security advisories apply to the whole dependency tree, and `unmaintained`
  advisories only to direct dependencies, because we can't replace the crates GPUI pulls in transitively. It also
  checks the license allowlist and bans yanked crates, wildcard versions, and unknown registries or git sources.

`unsafe` appears in six places, each behind its own `allow(unsafe_code)`:

- Three OS API calls in `crates/input/src/backend.rs`: `AXIsProcessTrusted` on macOS (checks Accessibility
  permission), and on Windows `SetCursorPos` (moves the pointer in virtual-desktop coordinates, including secondary
  monitors) and `SetProcessDpiAwarenessContext` (Per-Monitor V2 DPI awareness).
- The macOS capture and encoding module `crates/media/src/apple/`, which drives ScreenCaptureKit, CoreVideo,
  CoreMedia, and VideoToolbox through the objc2 bindings. Every unsafe block there carries a `SAFETY` comment.
- The Windows capture and encoding module `crates/media/src/win/`, which drives Windows.Graphics.Capture's interop,
  Direct3D 11, Media Foundation, and the input-desktop probe through the `windows` crate, which marks every COM and
  Win32 call unsafe. Every unsafe block there carries a `SAFETY` comment too.
- The development-only power sampler `crates/media/examples/power_sample.rs`, which calls the private
  `libIOReport.dylib`. Its unsafe blocks carry `SAFETY` comments too.
- `crates/winsvc/src/win32/`, every Win32 call of `dari-service.exe`'s service and helper: the pipes, client
  vetting, the check of `winlogon.exe`'s image and user and its restricted token, `CreateProcessAsUserW`, the job
  object, the event log, the input-desktop probe and `SetThreadDesktop` (the input thread attaches through its own
  handle, which has only `DESKTOP_JOURNALPLAYBACK`), DXGI Desktop Duplication and Direct3D 11 (`dxgi.rs`), and the
  frame sections and the read-only handle duplicated into the app (`section.rs`). It returns safe types, and the
  policy built on them (`service.rs`, `helper.rs`, `injector.rs`, `screen.rs`, `channel.rs`, `pointer.rs`) has no
  `unsafe`, so review can read the crate's `unsafe` in one directory.
- `crates/session/src/secure_desktop/win.rs` and its child `win/policy.rs`, the app's end of the helper's pipe:
  its security descriptor, the wait for a free instance of the service pipe and the handle handed to tokio, the
  impersonation that checks the helper is LocalSystem, and the read-only mapping of the helper's frame sections and
  the copy out of them; and the host setting's `ShellExecuteExW` with the `runas` verb, which runs
  `dari-service.exe policy on|off` elevated and waits for its exit code.

`.cargo/config.toml` sets `MACOSX_DEPLOYMENT_TARGET=13.0`, the oldest macOS with the ScreenCaptureKit features the
capture uses. Windows-only code without C dependencies can be checked from macOS too (`dari-media` can't: OpenH264's
C++ build doesn't cross-compile to MSVC, so Windows CI covers it):

```bash
cargo clippy -p dari-input --target x86_64-pc-windows-msvc -- -D warnings
```

```bash
cargo clippy -p dari-winsvc --target x86_64-pc-windows-msvc --all-targets -- -D warnings
```

Outside `win32/`, `dari-winsvc` has no `unsafe`. `policy.rs` writes the registry through the safe API of the
`windows-registry` crate, and `dari-proto`'s `registry.rs` reads the policy through the same API. The `unsafe` block inside `windows-service`'s `define_windows_service!` comes from
another crate's macro, and rustc doesn't report `unsafe_code` there.

## Tests

| Kind | Location | Coverage |
| --- | --- | --- |
| Unit tests | Each crate's `src/` | Codec limits, message validation, password generation and parsing, attempt throttling, handshake (MITM, version mismatch), downscaling, encode/decode with OpenH264 and each hardware backend (VideoToolbox on macOS; on Windows the machine's Media Foundation hardware encoder if it has one, and Media Foundation's software encoder through the same code so it runs on CI too), GPU conversion to NV12 matching OpenH264's colors (Windows, on WARP where there is no GPU), AVCC to Annex-B conversion, still-screen keyframes and refinement, frame rate and bitrate selection, key mapping, letterbox coordinates, relay forwarder |
| Transport E2E | `crates/net/tests/loopback.rs` | Real QUIC loopback: success, wrong password, consumed password, busy, throttling, oversized pre-auth frame, viewers barred from unidirectional streams |
| Session E2E | `crates/session/tests/loopback.rs` | The full host and viewer path with a synthetic screen and recorded input: frames arrive, input is injected, keys are released, permission status is reported, approval allow/deny/view-only, display switching, frame rate requests capped by each display's refresh rate (the first stream already runs at the requested rate, with or without approval), two-way clipboard, file transfer both ways (NFC names, no overwrite, decline, cancel cleanup, view-only refusal), audio from a synthetic tone to a recording output (mute stops capture, view-only still hears, no capture unless asked), connecting through a relay, the secure-desktop helper link (opened only after approval, without input in view-only sessions, dropped when it ends and at the session's end), the secure desktop in one stream (the helper's frames replace the screen and back with a keyframe at each switch while the status stays Available, a frame of another display never shows, and a helper that dies on the secure desktop brings back the notice), and input routing (input on the user's desktop stays local, a key held there is released there when the secure desktop appears, input on the secure desktop reaches the helper in the selected display's physical pixels and nothing reaches the local backend, a view-only session sends the helper no input, and input goes back to the user's desktop when the helper's link ends) |
| Relay E2E | `crates/relay/tests/relay.rs` | Connect by ID, wrong password rejected by the host, unknown ID, same ID after a relay restart |
| GUI | `crates/app/tests/gui.rs` | Renders real windows with the headless Metal renderer and injects input (below) |
| Secure-desktop helper | `crates/winsvc`, `crates/session/src/secure_desktop/`, `crates/session/src/input_route.rs`, `crates/input/src/session.rs` | The desktop tracker against a scripted desktop source, the refusal limit, which app may replace a session's helper, the helper's arguments and `policy on\|off`, and the local pipe messages (`crates/proto/src/local.rs`) and the frame section layout (`crates/proto/src/frame_section.rs`) on every platform. The pipe message tests cover `OsInput`'s limits (pointer coordinates, scroll, keys, and text), its `Debug` output without keys or text, `PolicyOff`'s wire index, and the `SecureDesktopControl` rule (missing or 1 is on, anything else is off, and a helper gets input only when the app asked and the policy is on). Also on every platform: the held-input `Injector` (a press past the cap is refused, dropping it releases newest first, replayed `OsInput` is released, a forgotten `Injector` sends nothing, and `InputSession::retarget` releases through the old target before it changes), and the app's input router (switching to the helper releases on the user's desktop first and switching back releases through the helper first, input on a desktop other than `Winlogon` stays local, input stays local without a live link, a dropped link fails helper input instead of queueing it, and random route sequences never leave input held on the side they left). Also on every platform: the screen machine against a scripted DXGI (every DXGI result's action, `ReleaseFrame` failures, a pointer-only first frame publishes nothing, the 1-second no-image limit, `E_ACCESSDENIED` reported once after 5 seconds, a switch between the desktop check and the attach, a desktop change forgets the pointer, an ended duplication discards its unpublished frame, and duplications that keep failing or show no image retry at most about 10 times a second, are reported once after 5 seconds, and log a bounded number of notes), pointer blending, a randomized model of the frame credit (the helper never writes the slot the app owns), the app's link state, the two-source capturer (a source change drops the last frame, and a capturer returns only its own display's frames), the app's frame handover (every `Frame` answered once, the old view unmapped when the next section arrives), and the helper's input thread against a scripted desktop and a recording backend (it injects only on `Winlogon`, forgets what it holds at a switch without injecting anything, even when the attach fails, releases it when the app hangs up on `Winlogon`, follows the desktop while idle, and drops input while an attach fails). On Windows: the service refuses a client whose image isn't `dari.exe` beside it and lets refused clients go at once, so clients that never hang up can't hold its pipe; a pipe read that times out loses nothing; the helper refuses a pipe server that isn't the app it was handed; a helper started without input drops input and keeps serving, and one with input hands it to its input thread, waiting instead of dropping input when that thread falls behind; the service's `admit` grants a helper input only when the app asked and the policy is on and refuses `SendSas` with `PolicyOff` while it is off, and the service reads `SecureDesktopControl` for each request; under a scratch `HKCU` key the policy value round-trips, `dari-proto`'s one policy reader reads a missing value as on and any value but a DWORD of 1 as off, also beside the key's default value, and the event source key is registered and removed idempotently; the app shows no setting without `dari-service.exe` beside it, and sends the session's input to the helper's pipe; an ignored test runs a command through the `runas` verb and reads its exit code; the app refuses a helper pipe client that isn't LocalSystem, and ends the link at once when DariService isn't running; `dari-service.exe` imports no Windows networking DLL (`crates/winsvc/tests/imports.rs`); a section handle duplicated with `FILE_MAP_READ` can't be mapped for writing; the app copies a slot only while its sequence word matches; and an ignored test captures the primary display with the real DXGI code in the signed-in session |
| Installer template | `crates/app/tests/installer_template.rs` | `assets/installer.nsi` is cargo-packager's template plus Dari's six additions (the `DariService` install and uninstall steps, the `SecureDesktopControl` page and its strings, the `policy on\|off` step, and the uninstaller's deletion of the value), each once at its place, and the workflows install the cargo-packager version it was copied from (see [Packaging and releases](#packaging-and-releases)) |
| Cross-device | `crates/check`, `scripts/crosscheck/` | Two machines in both directions, for every pairing: Mac and Windows, two Macs, two Windows PCs; see [Cross-device checks](#cross-device-checks) |
| mDNS | `crates/net/src/discovery.rs` | Needs local-network multicast, so skipped by default. Run with `cargo test -p dari-net -- --ignored` |

Real capture and encoding performance is measured with an example that captures and encodes the primary display
for a few seconds and prints the frame rate, bitrate, encoder (hardware or software), the time from capture to
encoded frame, and then how long the viewer's decoder takes per frame. The arguments are seconds, the longest
edge, the frame rate, and `hardware` or `software`. Capture only produces frames while the screen changes, so keep
something moving on the primary display (a video, scrolling). On macOS the terminal needs Screen Recording
permission. The display's refresh rate caps the result.

```bash
cargo run --release -p dari-media --example capture_bench -- 5 1920 144 hardware
```

To measure the hardware stream past the display's refresh rate, an ignored test feeds the stream prepared frames
from a 144 Hz clock and checks that it keeps up after VideoToolbox settles:

```bash
cargo test --release -p dari-media -- --ignored --nocapture hardware_stream_keeps_up
```

To see what a change costs in power on Apple silicon, `power_sample` reads IOReport's "Energy Model" counters, the
source `sudo powermetrics` uses, through the private `libIOReport.dylib`, so it needs no password. The arguments are
the interval in milliseconds, the number of samples (0 runs until interrupted), and optionally the channels to
print. `AVE` is the hardware video encoder (the media engine); `GPU Energy`, `DRAM`, and `CPU Energy` are the other
useful ones, and leaving the names out prints every channel that used energy. Each line is a Unix timestamp and
`CHANNEL=watts` for that interval, so you can average the lines inside a benchmark's window; with a fixed count, a
final `mean` line averages the whole run. Idle, `AVE` reads about 0.001 W; streaming 1920×1246 at 144 fps on an M5,
about 0.14 W.

```bash
cargo run --release -p dari-media --example power_sample -- 1000 0 AVE "GPU Energy" DRAM "CPU Energy"
```

Measure with the machine otherwise idle, and subtract an idle sample taken the same way. Busy machines hide
differences: with a VM and compilers keeping the CPU near 20 W, VideoToolbox's real-time mode never lowered its clock,
and real-time on and off measured the same. Leave a few seconds of warm-up out of the window, since the encoder
takes a moment to settle.

The session layer swaps screen and input through the `HostPlatform` trait, so even environments without screen
permissions, like CI runners, verify the real path including QUIC and H.264. Time-dependent tests (allocation
expiry and the like) use tokio's paused clock.

### GUI tests

GPUI's macOS platform can only be created on the main thread, so the standard test harness can't be used.
`crates/app/tests/gui.rs` is a test with its own `main` (`harness = false`), and the app crate exposes a library
(`dari::test_support`). Each test renders windows with `HeadlessAppContext` and saves the result to
`target/gui-snapshots/*.png` for visual review. The home window is also rendered in the dark theme
(`home-dark.png`), and the viewer once more after the host ends the session (`viewer-ended.png`). The
background-picture test draws a synthetic picture; to review the design over a real one, set `DARI_GUI_BACKGROUND`
to its path.

- The home window shows addresses and the password.
- The viewer window connects to a synthetic host over real QUIC and draws the screen, and GPUI input events
  (mouse, keys, Tab, ⌘C) reach the host's input backend. The same test measures memory growth while streaming,
  with a bound proportional to the frames shown (20 MB + 0.4 MB per frame, at least 20 frames). A fixed frame
  count failed on a slow CI runner that only drew 48 frames.
- Filling the connect form with an address (or relay ID) and password brings up the approval card, and clicking
  "Allow control" establishes the session.

### Real screen and input

`crates/session/tests/real_platform.rs` runs a session on this machine's real display and input devices: it checks
the host reports both as available, that it streams at the display's refresh rate when asked for 144 fps, that the
decoded frame has real content shaped like the display (saved to `target/real-platform/frame.png`), and that remote
pointer moves land on the requested coordinates. It's ignored by
default because it needs Screen Recording and Accessibility on macOS and moves the real pointer:

```bash
cargo test -p dari-session --test real_platform -- --ignored --nocapture
```

The **Platform checks** workflow (`.github/workflows/platform.yml`, manual or on changes to it or the installer) runs
that test on Windows and macOS runners, and installs the published Windows installer silently to host and connect a
session with the installed `dari.exe`. It also builds the installer from the branch and checks the install itself:

1. It uploads the branch's installer as the `windows-installer-branch` artifact, for installing in a VM.
2. It creates a standard user, installs 0.0.3 for that user, and runs the branch's installer silently as that user.
   The installer must exit with an error and change nothing: no `DariService`, no `C:\Program Files\Dari`, and the
   user's per-user copy still in place. It then deletes the user.
3. It installs 0.0.3, the last per-user release, for the runner's account. The per-user copy, its Start menu and
   desktop shortcuts, and its `HKCU` keys must exist. It runs `dari.exe host` so it creates a device identity, adds
   a `settings.toml`, and records the SHA-256 of every file in `%LOCALAPPDATA%\dari\dari\data`.
4. It points the per-user uninstall key's `UninstallString` at a canary script that leaves a marker file when it
   runs, and checks that it does when started the way NSIS's `ExecWait` starts a program.
5. It installs the branch's installer silently over the per-user install. The canary's marker must not exist.
   `DariService` must be running as LocalSystem, start automatically, restart on failure, and have the quoted image
   path `"C:\Program Files\Dari\dari-service.exe" service`. Both executables must be in `C:\Program Files\Dari`,
   and the per-user copy, its uninstaller, its two shortcuts, its `HKCU` uninstall key, and `HKCU\Software\dari\Dari`
   must be gone. The install folder and `dari-service.exe` must grant no write right to Users, Everyone,
   Authenticated Users, or INTERACTIVE, compared by SID. The newest `DariService` entry in the Application log must
   render its text. `SecureDesktopControl` must be the DWORD 1. The data files must be unchanged.
6. It sets `SecureDesktopControl` to 0 and runs the installer again, as a repair would, and checks the same, with
   the value still 0. It then runs the installed uninstaller with `/P`, as the reinstall page does before an
   upgrade, checks that the value is still 0, installs again, and checks the same.
7. It uninstalls silently. The service, both executables, the uninstall key, `SecureDesktopControl`, and the
   `HKLM\SOFTWARE\Policies\Dari` key must be gone, and the data files must be unchanged.

### Cross-device checks

`scripts/crosscheck/crosscheck.sh` runs sessions between two peers, A and B, in both directions. Either peer can be
a Mac or a Windows PC, so it covers every pairing Dari supports: a Mac and a Windows PC, two Macs, and two Windows
PCs. A peer is this Mac (`--a local`) or a machine the script reaches over SSH (`--b windows:USER@HOST`, `--b
macos:USER@HOST`); the script itself runs on macOS or Linux. Every night and on pull requests that touch the crates,
the **Cross-device check** workflow runs all three pairings on hosted runners; before a release, run it from your Mac
against a local Windows 11 VM, which adds what the runners can't have: the Korean input method, a second monitor, and
150% scaling. Each case runs `dari-check host` on one peer and `dari-check view` on the other. `dari-check` (`crates/check`) is a test-only binary that isn't packaged. The two
sides share nothing but the session, and each checks what it can observe on its own machine:

| Side | Checks |
| --- | --- |
| Host | It is asked to approve once; screen and input report Available; for every display, each of the viewer's pointer targets lands there in the real OS pointer position, computed from the display's own bounds, so a scaling or multi-monitor coordinate mismatch shows up as a miss; the viewer's ⌘/Ctrl arrives as this OS's shortcut modifier; the viewer's clipboard text reaches the system clipboard |
| Host's input window | `dari-check host` opens a window at the centre of the primary display and records what an app there receives: the viewer's click at the display's centre lands in it and focuses it, the wheel scrolls it, typed keys arrive as text, and the viewer's copy shortcut arrives as this OS's (⌘C or Ctrl+C). On a Windows host with Korean installed, the Korean input method must also compose 한 from the keys the viewer types after the Hangul key. On a Mac host the window types with an ASCII input source while it is open and then restores the user's |
| Viewer | Status matches the approval; the expected number of displays; switching to each display; frames shaped like the display with real content (saved as PNGs); the host's clipboard reply arrives on this machine's clipboard |
| View only | No input or clipboard text reaches the host, and nothing comes back |

The checks are the same for every pairing: each side judges by its own OS, so a Mac host expects ⌘ from a Mac
viewer just as it does from a Windows viewer, whose Ctrl the session translates, and a Windows viewer still sends
the Hangul key to a Windows host. Between two Macs or two Windows PCs nothing is translated, and the host's "no
untranslated foreign modifier" check proves that end to end.

The cases are named by which peer hosts: `a-host-direct`, `b-host-direct`, `a-host-relay`, `b-host-relay` (through
a `dari-relay` the script runs on its own machine), `a-host-view-only`, `b-host-view-only`, and the audio check
`a-host-audio`, `b-host-audio` (see [Audio check](#audio-check); `--no-audio` reports them as skipped). With
`--release TAG` it also downloads that release, installs on each peer the package for its OS (the DMG's app on a Mac,
the installer on Windows), and connects the installed apps to each other in both directions with their own headless
`host` and `connect` commands (`a-host-installed`, `b-host-installed`), which catches packaging problems a source
build can't. `--cases` picks a subset. In an allowed session the host ends the session after the clipboard round
trip, and in a view-only session the viewer disconnects, which covers both disconnect directions. Logs and frames go
to `target/crosscheck/<time>/`. Don't touch a Mac peer's mouse during a run: the host check reads the real pointer
position, and the Mac's clipboard is overwritten and then restored. When this Mac is a peer, the terminal running the
script needs Screen Recording and Accessibility.

Programs started over SSH can't capture the screen or inject input, so on each peer the script starts them in the
signed-in session through a helper: `windows/interactive.ps1` runs them as a scheduled task for the signed-in user,
and on a Mac `macos/interactive.sh serve`, left running in a Terminal on that Mac (or a job step on a runner), starts
what the script asks for over SSH as its own child, so the program gets that Terminal's grants. A local Mac peer goes
through the same helper, which the script starts itself. Hosts listen on UDP 47831 on a Mac, so a Dari app running
there (47821) doesn't get in the way, and on 47821 on Windows; the relay listens on 47822 on the driving machine,
which may also be peer A.

A host and a viewer on the same machine share one clipboard, so the clipboard round trip only passes between two
machines. The rest of `dari-check` can still be tried locally with `dari-check host --port 0 --approve view-only
--nonce x` and `dari-check view 127.0.0.1:<port> --approve view-only --nonce x`.

#### Audio check

`dari-check audio-host` plays a 997 Hz tone through this machine's speakers and shares its system audio;
`dari-check audio-view ADDRESS` connects, asks for audio, records what arrives instead of playing it, and passes when
the received sound is loud enough and strongest at 997 Hz. The host side needs a real output device (a VM needs a
virtual sound card). On macOS the host must run from an app bundle that declares `NSAudioCaptureUsageDescription`,
or macOS records silence without asking; `scripts/crosscheck/mac-check-app.sh` wraps `dari-check` in an ad-hoc
signed `target/crosscheck/DariCheck.app`. Approve the system audio prompt once. An ad-hoc signature is tied to the
binary, so after every rebuild of `dari-check` macOS asks again. While it asks, the host reports
`AwaitingPermission` and `audio-view` waits up to two minutes for the answer; a refusal makes the host report
`PermissionDenied`, which `audio-view` reports as a failure.

```bash
scripts/crosscheck/mac-check-app.sh
```

```bash
open -n --stdout target/crosscheck/host.log target/crosscheck/DariCheck.app --args audio-host --port 0
```

Then run `dari-check audio-view 127.0.0.1:PORT` with the port and password from `host.log` (here, or from another
machine with this Mac's address). The tone is audible during the run. `crosscheck.sh` runs the same pair as its
`a-host-audio` and `b-host-audio` cases: it builds `DariCheck.app` on every Mac peer and opens it there, so approve
the system audio prompt on each Mac on the first run.

On the Windows VM, give it a sound card first; `create-vm.sh` doesn't, and without one the host reports audio
Unavailable. This shuts the VM down, restarts UTM with an `intel-hda` device, and starts the VM again (it does nothing
if the VM already has one):

```bash
scripts/crosscheck/vm/add-sound.sh
```

`crosscheck.sh` runs the Windows host in the desktop session through `interactive.ps1`, since WASAPI loopback needs
the signed-in user's audio session; by hand, that's `interactive.ps1 start -Name audio-host -Exe
C:\dari-check\dari-check.exe -Arguments 'audio-host','--port','47821' -Log ...`, and the viewer anywhere. A copy of `dari-check.exe` outside `C:\dari-check` needs its
own firewall program rule, as `prepare-peer.ps1` makes for that path: otherwise Windows answers the first listen with
block rules and the viewer times out.

#### Local Windows 11 VM

On Apple silicon, `scripts/crosscheck/vm/create-vm.sh` creates a Windows 11 on Arm VM in UTM (`brew install --cask
utm`) and installs it unattended. Download the Windows 11 Arm64 ISO from
[microsoft.com](https://www.microsoft.com/software-download/windows11arm64) first; the page refuses scripted
downloads. The answer file (`vm/autounattend.xml`, based on the one in UTM's guest tools) installs Windows with a
local account that signs in automatically, because the checks run in the signed-in desktop session through a
scheduled task: a program started over SSH can't see the desktop. At first sign-in, `vm/bootstrap.ps1` installs the
guest tools, keeps the desktop awake and unlocked, turns off the guest agent's clipboard sharing with the Mac (it
would carry clipboard text outside Dari and race the clipboard check), and runs `windows/setup-vm.ps1`: SSH with
key-only login, the firewall rules, Rust, and the Visual Studio Build Tools. The SSH key and the VM user's password
are in `~/.dari-check-vm`. Windows isn't activated; that doesn't affect the checks.

```bash
scripts/crosscheck/vm/create-vm.sh --iso ~/Downloads/Windows11_Client_arm64_ko-kr_26300_9457.iso
```

```bash
scripts/crosscheck/vm/wait-vm.sh
```

`wait-vm.sh` prints the `crosscheck.sh` command once the VM is ready (about an hour after creating it). `--build`
copies this checkout to `C:\dari-check\src` and builds `dari-check` there for x64, the architecture the release
ships, so it runs under emulation; leave it off to reuse the last build. The app running the script needs Local
Network access (System Settings → Privacy & Security → Local Network), or every connection to the VM fails with
"No route to host".

```bash
scripts/crosscheck/crosscheck.sh --a local --b windows:dari@192.168.64.5 --identity ~/.dari-check-vm/id_ed25519 --known-hosts ~/.dari-check-vm/known_hosts --build
```

`vm/add-second-display.sh` then gives the VM a second monitor, 1920×1080 at 150% to the right of the 100% primary,
so the checks cover switching displays and pointer coordinates on a scaled display next to an unscaled one. UTM can't
add one itself (a second virtio-gpu device stops Windows on Arm from booting), so the script installs the
[Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) with
`windows/install-virtual-display.ps1`. Windows 11 24H2 and later refuse that Arm64 driver unless test signing is on,
so the script turns test signing on in the VM (`bcdedit /set testsigning off` undoes it). Then pass
`--b-displays 2`. For a Windows machine set up by hand, run `windows/setup-vm.ps1` in an elevated
PowerShell instead of the VM scripts.

```bash
scripts/crosscheck/vm/add-second-display.sh
```

The answer file turns UAC off, so Windows shows no prompts. `vm/enable-uac.sh` turns on the Windows defaults
(`EnableLUA=1`, `ConsentPromptBehaviorAdmin=5`, `PromptOnSecureDesktop=1`) and restarts the VM. A second run changes
nothing and doesn't restart. `--off` turns UAC off again, as the answer file left it. With UAC
on, the VM user's SSH session still gets an elevated token, so the scripts can still write `HKLM` and register tasks
over SSH. `windows/interactive.ps1 start -RunLevel Limited` then runs a program at medium integrity, like the
installed app; `Highest`, the default, runs it elevated. The existing cases pass with UAC on too, so `crosscheck.sh`
works with either setting.

```bash
scripts/crosscheck/vm/enable-uac.sh
scripts/crosscheck/vm/enable-uac.sh --off
```

With UAC on and Dari installed per machine, `windows/squat-service-pipe.ps1`, run in the VM from an elevated
PowerShell (the SSH session is one), checks that DariService fails loudly when another process took its pipe first.
It stops DariService, holds `\\.\pipe\dari-service` from a medium-integrity PowerShell in the signed-in session,
starts the service, prints the service's state and the Application and System log entries, and then ends the
squatter and starts the service again. `-HoldSeconds 120` keeps the squatter that long, so a session can be tried
meanwhile.

```powershell
C:\dari-check\src\scripts\crosscheck\windows\squat-service-pipe.ps1 -HoldSeconds 120
```

`secure-desktop.sh` checks that a viewer on this Mac can see and answer the VM's secure screens. It needs UAC on and
a per-machine install of the build under test. The script starts the installed `dari.exe host` at medium integrity,
so `DariService` and its helper are real. Each case connects its own `dari-check secure-view` from this Mac, so each
case is a new session with a new helper. The script shows a secure screen in the VM. The viewer waits for it to
settle, saves its frame, and sends the case's keys. The cases run in this order:

- `uac-allow`: a medium-integrity task asks to elevate `cmd.exe /c whoami /groups > result.txt`, and the viewer
  answers the UAC prompt with Alt+Y. The case passes when `result.txt` holds the High Mandatory Level SID
  (`S-1-16-12288`) and the host log shows the helper's `DesktopChanged(Winlogon)` and then `DesktopChanged(Default)`.
  Windows names that label in the display language, so the script matches the SID.
- `uac-deny`: the same prompt, answered with Esc. The case passes when no `result.txt` exists and the helper reports
  `Winlogon` and then `Default`.
- `secure-second-display`: `uac-allow`, with Alt+Y sent while the viewer has the second display selected. It runs
  only with `--second-display`.
- `drop-mid-prompt`: the viewer holds Alt on the prompt and disconnects. The case passes when DariService's
  Application log shows `helper: released N held inputs` with N of 1 or more, and Esc from a new viewer still cancels
  the prompt. The new viewer uses the next password the host prints.
- `helper-killed`: the viewer holds F20, and the script ends the helper. The viewer must get the secure-desktop
  notice. After the script cancels the prompt, a medium-integrity task on `Default` must find no key down with
  `GetAsyncKeyState` while the viewer still holds F20. F20 has no Windows binding, so a key that a bug leaves down
  doesn't change what later cases type.
- `secure-policy-off`: the script runs `dari-service.exe policy off` over SSH, and the viewer sends Alt+Y. The case
  passes when the viewer saw the prompt and no `result.txt` exists. The script cancels the prompt itself. It runs
  `policy on` after the case and again when it exits.
- `secure-view-only`: the same steps against `dari.exe host --view-only`.
- `lock-unlock`: the script locks the VM. The viewer clicks the lock screen, types the VM user's password from
  `~/.dari-check-vm/password` (`--vm-password` names another file), and presses Enter. The case passes when the
  helper reports `Winlogon` and then `Default` and the host's task still runs. The viewer saves no frame once it
  starts typing and ignores `RUST_LOG`. The case fails if any log in the output directory holds the password.

`--cases` picks a subset. `--second-display` says that the VM has a second display (`vm/add-second-display.sh`).
Every viewer then expects two displays and checks that both show each screen. After a failed case, the script
cancels any open prompt and goes on with the next case. The frames and logs go to `target/crosscheck/secure-<time>/`.

```bash
scripts/crosscheck/secure-desktop.sh --b windows:dari@192.168.64.5 --identity ~/.dari-check-vm/id_ed25519 --known-hosts ~/.dari-check-vm/known_hosts --second-display
```

`crosscheck.sh` runs the same script when `--cases` names `b-host-secure` (or `a-host-secure`). It passes that peer's
address and the SSH options. `--b-displays 2` adds `--second-display`, and `--secure-cases` picks the subset.

Don't remove the VM's CD drives: that moves the system disk to another PCI address and Windows stops booting.

Scripts in `scripts/crosscheck/` that run on Windows are ASCII only, which CI checks: Windows PowerShell 5.1 reads
a script without a byte order mark in the system code page, and on Korean Windows a single "…" broke parsing.

#### A second Mac or Windows PC

To check two Macs or two PCs on real hardware, prepare each machine you reach over SSH once. On a Windows PC, run
`windows/setup-vm.ps1` in an elevated PowerShell, or `windows/prepare-peer.ps1 -PublicKey ...` if it already has
`dari-check.exe` in `C:\dari-check`; it must stay signed in. On a Mac, `macos/prepare-peer.sh` turns on Remote Login
with key-only login and authorizes your key, and `macos/interactive.sh serve` must run in a Terminal on it, with
Screen Recording and Accessibility granted to Terminal, for as long as the checks run. On a Mac with macOS's
firewall on, allow incoming connections for `dari-check` when it asks. `--build` copies this checkout to each SSH
peer and builds `dari-check` there (a Mac peer needs Rust and the Xcode command line tools).

```bash
scripts/crosscheck/macos/prepare-peer.sh "$(cat ~/.ssh/id_ed25519.pub)"
```

```bash
scripts/crosscheck/macos/interactive.sh serve
```

Then, from your Mac:

```bash
scripts/crosscheck/crosscheck.sh --a local --b macos:me@192.168.0.12 --build
```

For two Windows PCs, run it from a Mac or Linux machine that reaches both; it runs the relay itself:

```bash
scripts/crosscheck/crosscheck.sh --a windows:me@192.168.0.21 --b windows:me@192.168.0.22 --build
```

The checks don't cover everything in a same-OS pair; the manual checklist in
[#20](https://github.com/j0urneyk/dari/issues/20) lists what to try by hand (Nearby devices, the approval prompt's
timeout, quality presets, Korean input, file transfer, and macOS permissions).

#### Every night: hosted runners

`.github/workflows/crosscheck.yml` runs every night, on demand, and on pull requests from this repository that touch
`crates/`, `Cargo.lock`, or `scripts/crosscheck/`, for three pairings: a macOS runner and an x64 Windows runner, two
macOS runners, and two Windows runners. The repository is public, so the runner minutes cost nothing and every
pairing runs on pull requests too. Hosted runners can't reach each other, so all of them join the tailnet as
ephemeral `tag:ci` nodes through Tailscale workload identity federation. A first job makes an SSH key for the run.
Peer jobs build `dari-check`, open SSH, join as `dari-check-<run id>-<pairing>-<a|b>`, and wait: a Windows peer
allows SSH and UDP 47821 from tailnet addresses only and waits for a `done` file; a macOS peer runs
`macos/interactive.sh serve` in a job step until its `done` file appears, because only the job's own processes have
the runner image's Screen Recording grant (the image grants SSH sessions Accessibility but not Screen Recording).
A driver job per pairing joins as `dari-check-<run id>-<pairing>-driver` and runs `crosscheck.sh` with
`scripts/crosscheck/ci-driver.sh`, then tells its peers to finish. When the pairing has a Mac, the driver is that
macOS runner and is peer A itself; two Windows peers are driven from an Ubuntu runner, which runs the relay. Logs and
frames are the `crosscheck-<pairing>-<driver|a|b>` artifacts, and a failing nightly run sends GitHub's usual
failed-workflow notification. Windows runners are Windows Server in English with one display and no GPU, so Hangul
input, a second monitor, scaling, and the hardware encoder are left to the VM and real machines. CI skips the audio cases:
Windows runners have no sound device at all, and a macOS runner's app can't be granted system audio recording, so its
host reports sound Available but records nothing.

One-time setup:

1. In the Tailscale policy file, add the tag and keep the runners' reach narrow: they run whatever code the branch
   has, so they may reach each other but only send Dari's UDP traffic to your own devices (a Mac host on 47831, a relay
   on 47822, and the relay's allocations on ephemeral ports):

   ```json
   "tagOwners": {"tag:ci": ["autogroup:admin"]},
   "grants": [
       {"src": ["autogroup:member"], "dst": ["*"], "ip": ["*"]},
       {"src": ["tag:ci"], "dst": ["tag:ci"], "ip": ["*"]},
       {"src": ["tag:ci"], "dst": ["autogroup:member"], "ip": ["udp:47822", "udp:47831", "udp:49152-65535"]},
   ],
   ```

2. Under **Trust credentials**, add an OpenID Connect credential with the GitHub issuer, subject
   `repo:j0urneyk@62772873/dari@1402251066:*`, the custom claim `job_workflow_ref` =
   `*/.github/workflows/crosscheck.yml@*` (so only this workflow can use it), and only the writable `auth_keys` scope
   for `tag:ci`. GitHub puts the owner's and repository's numeric IDs in the subject, so a renamed or recreated
   repository can't take over the trust; a subject without them never matches (the credential's page shows the
   subject it last received). The workflow signs in with GitHub's OIDC token, so nothing secret is stored: the
   credential's client ID and audience aren't secrets and go in the repository variables `TS_CLIENT_ID` and
   `TS_AUDIENCE`. If the tag isn't `tag:ci`, set `TS_TAGS` too.

#### Before a release: the local VM

With the VM running (`utmctl start dari-win11`, then `vm/wait-vm.sh`), check the release candidate's source and its
packaged builds from your Mac. The audio cases need the VM's sound card (`vm/add-sound.sh`); pass `--no-audio`
without one:

```bash
scripts/crosscheck/crosscheck.sh --a local --b windows:dari@192.168.64.5 --identity ~/.dari-check-vm/id_ed25519 --known-hosts ~/.dari-check-vm/known_hosts --build --b-displays 2 --release vX.Y.Z
```

#### Keeping the VM small

The VM's disk is a qcow2 file that only grows: the 96 GB is its ceiling, not its size. To give space back, clean up
inside Windows (`DISM /Online /Cleanup-Image /StartComponentCleanup /ResetBase`, the temp folders), fill its free
space with zeros, shut it down, and rewrite the image; `qemu-img` (`brew install qemu`) leaves zeroed clusters out:

```bash
qemu-img convert -O qcow2 dari-win11.qcow2 compact.qcow2
```

#### Still manual

Before a release, check on real hardware what the scripts don't cover: a real x64 Windows 11 PC with physical monitors
at mixed scaling, the Korean input method on a Mac host, and the release apps' GUI (pairing through the approval card
and the viewer window).

## CI

`.github/workflows/ci.yml` runs on pull requests and on pushes to `main`.

| Job | Runner | Steps |
| --- | --- | --- |
| `rustfmt` | ubuntu | `cargo fmt --all --check` |
| `cargo-deny` | ubuntu | `cargo deny check` |
| `clippy + test` | macos-latest, windows-latest | `cargo lint`, `cargo test --workspace --locked`, plus the GUI tests on macOS |

Actions are pinned to commit SHAs and the token is read-only. A newer run cancels older runs on the same branch.
Debug info (Windows PDBs) greatly lengthens cold builds, so it's turned off with `CARGO_PROFILE_DEV_DEBUG=0`. The
cache is saved only on pushes to `main`, so a PR branch's first build is mostly cold. Because of GPUI, the Windows
runner takes about 15 minutes for clippy and about 20 minutes for tests.

## Workflow

1. Create a work branch from `main`.
2. Implement the change and pass the quality gates above locally.
3. Open a PR, filling in Summary, Verification, and Release notes from the PR template
   (`.github/pull_request_template.md`).
4. Repeat code review and security review until there are no findings. Add a regression test for each fixed
   defect.
5. Record user-visible changes under `## [Unreleased]` in `CHANGELOG.md`. If the design changes, update the
   relevant document in this directory (architecture, protocol, security model, and so on) in the same PR.

## Packaging and releases

Packaging is configured in `[package.metadata.packager]` in `crates/app/Cargo.toml` (bundle ID `dev.dari.app`,
macOS 13.0 minimum, per-machine Windows NSIS install). The packages carry two binaries: `dari` and `dari-service`
from `crates/winsvc`. cargo-packager has no per-platform binaries, so the macOS app also carries the
`dari-service` stub, which only exits with an error.

The Windows installer installs for all users in `C:\Program Files\Dari` and needs administrator rights.
`DariService` runs `dari-service.exe` as LocalSystem, so the binary must live where only administrators can
replace it. cargo-packager 0.11.8 has no post-install or pre-uninstall hook, so the NSIS settings add the service in
two places:

- `preinstall-section` runs before files are copied. Its sections run in this order:
  - `RequireAdministrator` stops the installer unless it runs as an administrator. The template's
    `RequestExecutionLevel highest` shows no UAC prompt to a standard user, so the installer would otherwise run
    unelevated and fail partway.
  - `StopDariService` stops `DariService`, so an upgrade or a repair can replace `dari-service.exe`.
  - `RemovePerUserInstallAtFixedPaths` finds a per-user install from 0.0.3 or earlier by its uninstall key under `HKCU`. It
    closes a running per-user `dari.exe` and deletes what 0.0.3 created at 0.0.3's fixed locations: `dari.exe` and
    `uninstall.exe` in `%LOCALAPPDATA%\Dari`, the user's Start menu and desktop shortcuts, the `HKCU` uninstall key,
    and `HKCU\Software\dari\Dari`. It doesn't run the old uninstaller or read a path from the registry, because any
    process of the user can change both, and the installer runs as an administrator. `%LOCALAPPDATA%\Dari` is the
    same folder as the app's data in `%LOCALAPPDATA%\dari`, so nothing is deleted recursively and the identity and
    settings stay.
- `template` points at `crates/app/assets/installer.nsi`, a copy of cargo-packager 0.11.8's template with six
  additions:
  - `PAGE_ADDITION`, after the directory page: a page with the checkbox **Let viewers answer UAC prompts and the
    lock screen** and its explanation. The checkbox starts from the stored `SecureDesktopControl`, read with the
    service's rule (missing or the DWORD 1 is checked, anything else is unchecked). Silent and passive installs
    skip the page and keep that value.
  - `STRINGS_ADDITION`, after the language files: the page's English strings. cargo-packager builds the installer
    in English only, and a second language would make the uninstaller show NSIS's language picker.
  - `INSTALL_ADDITION` and then `POLICY_INSTALL_ADDITION`, at the end of `Section Install`: `dari-service.exe
    install`, then `dari-service.exe policy on` or `policy off` as the page left it.
  - `UNINSTALL_ADDITION`, at the start of `Section Uninstall`: `dari-service.exe uninstall`.
  - `POLICY_UNINSTALL_ADDITION`, before the uninstaller's closing `/P` check: delete `SecureDesktopControl`, and
    `HKLM\SOFTWARE\Policies\Dari` if it is then empty, unless the uninstaller runs with `/P`. The reinstall page
    runs the old uninstaller with `/P` before an upgrade, so an upgrade keeps an administrator's 0.

  Any of the three `dari-service.exe` commands failing stops the installer or the uninstaller. They converge, so
  running them again changes nothing. `crates/app/tests/installer_template.rs` fails when the copy differs from
  upstream in anything but those six additions, when an addition isn't at its place, or when a workflow installs
  another cargo-packager version. After a cargo-packager upgrade, copy the new version's template, apply the six
  additions again, and update the test's version and hash.

The Platform checks workflow checks that a standard user is refused, installs the branch's installer over a
per-user 0.0.3 install, runs it again, upgrades it, and uninstalls it, checking `SecureDesktopControl` after each
step (see [Real screen and input](#real-screen-and-input)).

The icon's source is `crates/app/assets/icon.svg`, from which
the PNGs and `.icns` are generated with [librsvg](https://gitlab.gnome.org/GNOME/librsvg)'s `rsvg-convert` and
`iconutil` (run from `crates/app/assets`). The home window's title bar shows `icon-128.png`, embedded in the binary.

```bash
for s in 32 128 256 512 1024; do rsvg-convert -w $s -h $s icon.svg -o icons/icon-$s.png; done
```

```bash
mkdir -p Dari.iconset && for s in 16 32 128 256 512; do rsvg-convert -w $s -h $s icon.svg -o Dari.iconset/icon_${s}x${s}.png; rsvg-convert -w $((s*2)) -h $((s*2)) icon.svg -o Dari.iconset/icon_${s}x${s}@2x.png; done && iconutil -c icns Dari.iconset -o icons/icon.icns && rm -r Dari.iconset
```

`assets/Info.plist` declares the local network usage
description (`NSLocalNetworkUsageDescription`) and the Bonjour service (`_dari._udp`); since macOS 15, LAN
connections and mDNS are blocked without them. macOS grants Screen Recording and Accessibility per bundle ID, so
changing the bundle ID makes users grant them again.

To package locally on macOS, run the following. The DMG is built with `hdiutil`, because cargo-packager's dmg
format scripts Finder with AppleScript to lay out the window and fails without automation permission.

```bash
cd crates/app && cargo packager --release --formats app
```

```bash
cd target/release && hdiutil create -volname Dari -srcfolder Dari.app -ov -format UDZO dari.dmg
```

Release steps:

1. Move the `## [Unreleased]` entries in `CHANGELOG.md` into a `## [x.y.z] - YYYY-MM-DD` section, leaving an empty
   `## [Unreleased]` and the compare/tag links (Keep a Changelog). Update the version in the root `Cargo.toml` too.
2. Merge that through a PR, then push a `vx.y.z` tag.
3. `.github/workflows/release.yml` builds the macOS dmg, the Windows installer, and the Linux relay tar.gz, and
   creates a draft release titled with the bare version (`x.y.z`) whose notes are the matching CHANGELOG section. Extraction stops at the next `## [`
   heading or a `[x]: ` link-definition line. If a release for the tag already exists, only the assets are added
   (`--clobber`).
4. Check that the draft notes match the CHANGELOG section, then publish.

The macOS app is ad-hoc signed, which macOS lets the user allow in Privacy & Security. Developer ID signing and
notarization aren't set up: cargo-packager signs only when `signing-identity` is set under
`[package.metadata.packager.macos]`, and the ad-hoc step in `release.yml` would then have to go.

Running `workflow_dispatch` on a branch only checks packaging, without the publish step. To attach missing assets to
an already published tag, re-run the workflow on the tag; the publish step then adds the assets to the existing
release.

```bash
gh workflow run release.yml --ref vX.Y.Z
```
