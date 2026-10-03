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

`unsafe` appears only in three OS API calls in `crates/input/src/backend.rs`, each with its own
`allow(unsafe_code)`: `AXIsProcessTrusted` on macOS (checks Accessibility permission), and on Windows
`SetCursorPos` (moves the pointer in virtual-desktop coordinates, including secondary monitors) and
`SetProcessDpiAwarenessContext` (Per-Monitor V2 DPI awareness). Windows-only code can be checked from macOS too:

```bash
cargo clippy -p dari-input --target x86_64-pc-windows-msvc -- -D warnings
```

## Tests

| Kind | Location | Coverage |
| --- | --- | --- |
| Unit tests | Each crate's `src/` | Codec limits, message validation, password generation and parsing, attempt throttling, handshake (MITM, version mismatch), downscaling, encode/decode, key mapping, letterbox coordinates, relay forwarder |
| Transport E2E | `crates/net/tests/loopback.rs` | Real QUIC loopback: success, wrong password, consumed password, busy, throttling, oversized pre-auth frame, viewers barred from unidirectional streams |
| Session E2E | `crates/session/tests/loopback.rs` | The full host and viewer path with a synthetic screen and recorded input: frames arrive, input is injected, keys are released, permission status is reported, approval allow/deny/view-only, display switching, two-way clipboard, connecting through a relay |
| Relay E2E | `crates/relay/tests/relay.rs` | Connect by ID, wrong password rejected by the host, unknown ID, same ID after a relay restart |
| GUI | `crates/app/tests/gui.rs` | Renders real windows with the headless Metal renderer and injects input (below) |
| Cross-device | `crates/check`, `scripts/crosscheck/` | This Mac against a Windows VM or an x64 runner over SSH, in both directions; see [Cross-device checks](#cross-device-checks) |
| mDNS | `crates/net/src/discovery.rs` | Needs local-network multicast, so skipped by default. Run with `cargo test -p dari-net -- --ignored` |

Real capture and encoding performance is measured with an example that captures and encodes the primary display
for a few seconds and prints throughput. On macOS, even without Screen Recording permission, capture returns
full-size frames (wallpaper only), which is enough to measure the cost.

```bash
cargo run --release -p dari-media --example capture_bench -- 5 1920
```

The session layer swaps screen and input through the `HostPlatform` trait, so even environments without screen
permissions, like CI runners, verify the real path including QUIC and H.264. Time-dependent tests (allocation
expiry and the like) use tokio's paused clock.

### GUI tests

GPUI's macOS platform can only be created on the main thread, so the standard test harness can't be used.
`crates/app/tests/gui.rs` is a test with its own `main` (`harness = false`), and the app crate exposes a library
(`dari::test_support`). Each test renders windows with `HeadlessAppContext` and saves the result to
`target/gui-snapshots/*.png` for visual review.

- The home window shows addresses and the password.
- The viewer window connects to a synthetic host over real QUIC and draws the screen, and GPUI input events
  (mouse, keys, Tab, ⌘C) reach the host's input backend. The same test measures memory growth while streaming,
  with a bound proportional to the frames shown (20 MB + 0.4 MB per frame, at least 20 frames). A fixed frame
  count failed on a slow CI runner that only drew 48 frames.
- Filling the connect form with an address (or relay ID) and password brings up the approval card, and clicking
  "Allow control" establishes the session.

### Real screen and input

`crates/session/tests/real_platform.rs` runs a session on this machine's real display and input devices: it checks
the host reports both as available, that the decoded frame has real content shaped like the display (saved to
`target/real-platform/frame.png`), and that remote pointer moves land on the requested coordinates. It's ignored by
default because it needs Screen Recording and Accessibility on macOS and moves the real pointer:

```bash
cargo test -p dari-session --test real_platform -- --ignored --nocapture
```

The **Platform checks** workflow (`.github/workflows/platform.yml`, manual or on changes to it) runs that test on
Windows and macOS runners, and installs the published Windows installer silently to host and connect a session
with the installed `dari.exe`.

### Cross-device checks

`scripts/crosscheck/crosscheck.sh` runs sessions between this Mac and a Windows machine it reaches over SSH, in both
directions. The Windows machine is either a local Windows 11 VM (the everyday check) or an x64 GitHub Windows runner
joined to your tailnet (to cover the architecture the release ships). Each case runs `dari-check host` on one side and
`dari-check view` on the other. `dari-check` (`crates/check`) is a test-only binary that isn't packaged. The two
sides share nothing but the session, and each checks what it can observe on its own machine:

| Side | Checks |
| --- | --- |
| Host | It is asked to approve once; screen and input report Available; for every display, each of the viewer's pointer targets lands there in the real OS pointer position, computed from the display's own bounds, so a scaling or multi-monitor coordinate mismatch shows up as a miss; the viewer's ⌘/Ctrl arrives as this OS's shortcut modifier; the viewer's clipboard text reaches the system clipboard |
| Viewer | Status matches the approval; the expected number of displays; switching to each display; frames shaped like the display with real content (saved as PNGs); the host's clipboard reply arrives on this machine's clipboard |
| View only | No input or clipboard text reaches the host, and nothing comes back |

The cases are `mac-host-direct`, `windows-host-direct`, `mac-host-relay`, `windows-host-relay` (through a
`dari-relay` the script runs on the Mac), `mac-host-view-only` and `windows-host-view-only`. In an allowed session the
host ends the session after the clipboard round trip, and in a view-only session the viewer disconnects, which
covers both disconnect directions. Logs and frames go to `target/crosscheck/<time>/`. Don't touch the Mac's mouse
during a run: the host check reads the real pointer position, and the Mac's clipboard is overwritten and then
restored. The terminal running the script needs Screen Recording and Accessibility.

A host and a viewer on the same machine share one clipboard, so the clipboard round trip only passes between two
machines. The rest of `dari-check` can still be tried locally with `dari-check host --port 0 --approve view-only
--nonce x` and `dari-check view 127.0.0.1:<port> --approve view-only --nonce x`.

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
scripts/crosscheck/crosscheck.sh --windows dari@192.168.64.5 --identity ~/.dari-check-vm/id_ed25519 --known-hosts ~/.dari-check-vm/known_hosts --build
```

`vm/add-second-display.sh` then gives the VM a second monitor, 1920×1080 at 150% to the right of the 100% primary,
so the checks cover switching displays and pointer coordinates on a scaled display next to an unscaled one. UTM can't
add one itself (a second virtio-gpu device stops Windows on Arm from booting), so the script installs the
[Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) with
`windows/install-virtual-display.ps1`. Windows 11 24H2 and later refuse that Arm64 driver unless test signing is on,
so the script turns test signing on in the VM (`bcdedit /set testsigning off` undoes it). Then pass
`--expect-windows-displays 2`. For a Windows machine set up by hand, run `windows/setup-vm.ps1` in an elevated
PowerShell instead of the VM scripts.

```bash
scripts/crosscheck/vm/add-second-display.sh
```

Don't remove the VM's CD drives: that moves the system disk to another PCI address and Windows stops booting.

Scripts in `scripts/crosscheck/` that run on Windows are ASCII only, which CI checks: Windows PowerShell 5.1 reads
a script without a byte order mark in the system code page, and on Korean Windows a single "…" broke parsing.

#### x64 GitHub runner over Tailscale

`scripts/crosscheck/runner.sh` dispatches the **Cross-device check** workflow (`.github/workflows/crosscheck.yml`)
with a fresh SSH key. The runner builds `dari-check`, allows SSH and UDP 47821 from tailnet addresses only, joins the
tailnet as `dari-check-<run id>` (an ephemeral node), and waits. The script then runs `crosscheck.sh` against it and
tells it to finish. The runner's logs and frames are uploaded as the `crosscheck-windows` artifact. The runner is
Windows Server, not Windows 11; it covers the x64 build and a separate network path.

One-time setup:

1. In the Tailscale policy file, add the tag and keep the runner's reach narrow: it runs whatever code the
   dispatched branch has, so it may only send Dari's UDP traffic to your devices (the Mac's host on 47831, the relay on
   47822, and the relay's allocations on ephemeral ports), while your devices can still reach it over SSH:

   ```json
   "tagOwners": {"tag:ci": ["autogroup:admin"]},
   "grants": [
       {"src": ["autogroup:member"], "dst": ["*"], "ip": ["*"]},
       {"src": ["tag:ci"], "dst": ["autogroup:member"], "ip": ["udp:47822", "udp:47831", "udp:49152-65535"]},
   ],
   ```

2. Under **Trust credentials**, add an OpenID Connect credential with the GitHub issuer, subject
   `repo:j0urneyk@62772873/dari@1402251066:*`, the custom claim `job_workflow_ref` =
   `*/.github/workflows/crosscheck.yml@*` (so only this workflow can use it), and only the writable `auth_keys` scope
   for `tag:ci`. GitHub puts the owner's and repository's numeric IDs in the subject, so a renamed or recreated
   repository can't take over the trust; a subject without them never matches (the credential's page shows the
   subject it last received). The workflow signs in with
   GitHub's OIDC token, so nothing secret is stored: the credential's client ID and audience aren't secrets and go in
   the repository variables `TS_CLIENT_ID` and `TS_AUDIENCE`. If the tag isn't `tag:ci`, set `TS_TAGS` too.
3. Run Tailscale on the Mac. The workflow file must be on the default branch before it can be dispatched.

```bash
scripts/crosscheck/runner.sh --cases mac-host-direct,windows-host-direct
```

#### Still manual

Before a release, check on real hardware what the scripts don't cover: clicks, the wheel, and typing (including the
Korean IME on the host) having the right effect in apps, ⌘C/Ctrl+C actually copying, a real x64 Windows 11 PC with
physical monitors at mixed scaling, and the release builds installed from the DMG and installer.

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
macOS 12.0 minimum, per-user Windows NSIS install). The icon's source is `crates/app/assets/icon.svg`, from which
the PNGs and `.icns` (via `iconutil`) are generated. `assets/Info.plist` declares the local network usage
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
   creates a draft release whose notes are the matching CHANGELOG section. Extraction stops at the next `## [`
   heading or a `[x]: ` link-definition line. If a release for the tag already exists, only the assets are added
   (`--clobber`).
4. Check that the draft notes match the CHANGELOG section, then publish.

The macOS app is signed and notarized only when the repository secrets include an Apple certificate
(`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`) and notarization credentials
(`APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`); otherwise the app is unsigned. Running `workflow_dispatch` on a
branch only checks packaging, without the publish step. To attach missing assets to an already published tag,
re-run the workflow on the tag; the publish step then adds the assets to the existing release.

```bash
gh workflow run release.yml --ref vX.Y.Z
```
