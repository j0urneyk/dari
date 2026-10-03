# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- File transfer in both directions for sessions that allow control. Drop files on the remote screen or use
  **Send file…** in the viewer toolbar or the host's session card. The host saves the viewer's files to Downloads;
  files from the host wait until the viewer user chooses **Save**. Names are normalized (no more separated Korean
  jamo from macOS), made safe for Windows, and never overwrite an existing file; cancelled or failed transfers
  leave no partial file. Turn it off with **Exchange files** on the host.
- Sound from the remote computer. The host records what it plays (WASAPI loopback on Windows, a Core Audio process
  tap on macOS 14.6 and later) and sends 20 ms Opus packets as QUIC datagrams; the viewer plays them through a small
  jitter buffer and conceals lost packets. **Sound on / Sound off** in the viewer toolbar mutes it, which also stops
  recording on the host. Hosts turn it off with **Share sound**. View-only viewers hear the host too.
- Frame rate choice in the viewer toolbar: Auto, 30, 60, 90, 120, or 144 fps, separate from the quality preset.
  Auto follows the viewer's fastest display, and the host caps the rate at its own display's refresh rate and
  reports what it settled on. `dari connect --fps` does the same from the command line. Faster streams get a
  higher bitrate.

### Changed

- macOS hosts capture with ScreenCaptureKit and encode with VideoToolbox in hardware, instead of taking a full
  screenshot and encoding it on the CPU for every frame. On an M5 MacBook the Balanced quality went from 18 fps to
  over 100 fps, and the host only sends frames when the screen changes. Windows hosts are unchanged.
- Dari now requires macOS 13 or later (was 12).
- Relayed sessions survive NAT rebinding: both sides refresh their relay binding every 10 seconds, and the relay
  follows a side to its new public address.
- Hosts throttle failed password attempts through a relay per viewer (by the viewer IP the relay reports) instead
  of per relay, so one viewer's guesses no longer lock out everyone else using the same relay.
- The relay protocol is now `dari-relay/2`. Update relays and apps together; 0.0.1 apps and relays can't talk to
  the new versions.
- The session protocol is now version 2.0: every unidirectional stream starts with a byte naming its kind, so file
  streams share the connection with video, audio travels as datagrams, and the viewer can ask for a frame rate.
  0.0.1 apps are refused with a clear "incompatible version" message; update both computers.

### Fixed

- A Mac host no longer quits when the viewer types. Each typed character made the host look up its key in the
  keyboard layout from the input thread, which macOS only allows on the main thread, so the app crashed; character
  keys are now pressed by their key code.
- A Windows host now accepts viewers that connect by an IPv4 address. It listened on IPv6 only, so connecting to a
  Windows PC by its `192.168.x.x` address timed out; connecting through a relay was not affected.

## [0.0.1] - 2026-10-03

The first release of Dari.

### Added

- Cargo workspace with the `dari` gpui-kit application skeleton.
- Quality gates: rustfmt, clippy (workspace lints), cargo-deny, and macOS/Windows CI.
- Wire protocol crate with versioning, validated messages, and length-bounded framing.
- Secure QUIC transport: self-signed device identity, one-time access passwords, SPAKE2
  authentication bound to the TLS session, failed-attempt throttling, and a single-session slot.
- Media pipeline: display capture (xcap), downscaling, H.264 encode/decode (OpenH264) on a
  paced capture thread that drops frames before encoding when the network falls behind.
- Remote input: pointer, buttons, wheel, keys, and text injected with enigo; held keys are
  released when a session ends; ⌘/Ctrl shortcut mapping between macOS and Windows.
- Host and viewer sessions with screen/input availability reporting, and the headless
  `dari host` / `dari connect` commands.
- Desktop app: share this device (addresses, one-time password, connected viewer, macOS
  permission guidance) and connect to another device in a viewer window that forwards
  keyboard, mouse, and wheel input. Korean and English UI.
- Connection approval (allow control, view only, or decline) before anything is shared;
  text clipboard sharing; nearby devices on the local network (mDNS); display switching and
  speed/balanced/quality stream presets.
- `dari-relay`: a self-hosted rendezvous and UDP relay so devices behind different NATs
  connect by a nine-digit ID; sessions stay end-to-end encrypted and authenticated.
- Packaging: macOS app/dmg and Windows installer via cargo-packager, app icon, macOS local
  network declarations, and a tag-triggered release workflow that also ships the Linux relay.
- Licensed under MIT OR Apache-2.0.

[Unreleased]: https://github.com/j0urneyk/dari/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/j0urneyk/dari/releases/tag/v0.0.1
