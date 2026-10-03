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
the host reports both as available, that the decoded frame has real content shaped like the display (saved to
`target/real-platform/frame.png`), and that remote pointer moves land on the requested coordinates. It's ignored by
default because it needs Screen Recording and Accessibility on macOS and moves the real pointer:

```bash
cargo test -p dari-session --test real_platform -- --ignored --nocapture
```

The **Platform checks** workflow (`.github/workflows/platform.yml`, manual or on changes to it) runs that test on
Windows and macOS runners, and installs the published Windows installer silently to host and connect a session
with the installed `dari.exe`.

### Cross-device checklist

Hosted runners can't reach each other, so a session between two physical machines is still a manual check. Before a
release, on a Mac and a Windows 11 PC with the release builds installed:

1. Each side hosts while the other connects, by address and by relay ID.
2. Approve with **Allow control**: pointer, clicks, wheel, typing (including Korean IME on the host), and ⌘C/Ctrl+C
   shortcuts work in both directions.
3. Copy text on each side and paste it on the other.
4. With two monitors on the host, switch displays in the viewer toolbar; the pointer lands on the selected display.
5. On a scaled display (Retina, or Windows at 150%), the pointer lands where it's clicked.
6. **View only** blocks input and clipboard; **Disconnect** on either side ends the session.

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
the PNGs and `.icns` are generated with [librsvg](https://gitlab.gnome.org/GNOME/librsvg)'s `rsvg-convert` and
`iconutil` (run from `crates/app/assets`). The home window's header shows `icon-128.png`, embedded in the binary.

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
