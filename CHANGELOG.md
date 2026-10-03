# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Frame rate choice in the viewer toolbar: Auto, 30, 60, 90, 120, or 144 fps, separate from the quality preset.
  Auto follows the viewer's fastest display, and the host caps the rate at its own display's refresh rate and
  reports what it settled on. `dari connect --fps` does the same from the command line. Faster streams get a
  higher bitrate. Hosts on 0.0.1 keep streaming at 30 fps.

### Changed

- macOS hosts capture with ScreenCaptureKit and encode with VideoToolbox in hardware, instead of taking a full
  screenshot and encoding it on the CPU for every frame. On an M5 MacBook the Balanced quality went from 18 fps to
  over 100 fps, and the host only sends frames when the screen changes. Windows hosts are unchanged.
- Dari now requires macOS 13 or later (was 12).
- The protocol version is 1.1. It adds the frame rate messages, which are only sent to peers that understand them,
  so 1.1 and 1.0 apps still connect to each other.
- Relayed sessions survive NAT rebinding: both sides refresh their relay binding every 10 seconds, and the relay
  follows a side to its new public address.
- Hosts throttle failed password attempts through a relay per viewer (by the viewer IP the relay reports) instead
  of per relay, so one viewer's guesses no longer lock out everyone else using the same relay.
- The relay protocol is now `dari-relay/2`. Update relays and apps together; 0.0.1 apps and relays can't talk to
  the new versions.

### Fixed

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
