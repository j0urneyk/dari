# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- Renamed the project from open-desk to Dari. The app, CLI (`dari`), relay (`dari-relay`), crates (`dari-*`),
  bundle ID (`dev.dari.app`), data directory, mDNS service (`_dari._udp`), and protocol identifiers (ALPN,
  TLS exporter label, SPAKE2 identities, relay datagram tags) all use the new name. Dari cannot connect to
  open-desk 0.1.0 devices or relays. On macOS, grant Screen Recording and Accessibility again for the new
  bundle. The device certificate and relay ID are regenerated because the data directory moved.

## [0.1.0] - 2026-10-03

### Added

- Cargo workspace with the `open-desk` gpui-kit application skeleton.
- Quality gates: rustfmt, clippy (workspace lints), cargo-deny, and macOS/Windows CI.
- Wire protocol crate with versioning, validated messages, and length-bounded framing.
- Secure QUIC transport: self-signed device identity, one-time access passwords, SPAKE2
  authentication bound to the TLS session, failed-attempt throttling, and a single-session slot.
- Media pipeline: display capture (xcap), downscaling, H.264 encode/decode (OpenH264) on a
  paced capture thread that drops frames before encoding when the network falls behind.
- Remote input: pointer, buttons, wheel, keys, and text injected with enigo; held keys are
  released when a session ends; ⌘/Ctrl shortcut mapping between macOS and Windows.
- Host and viewer sessions with screen/input availability reporting, and the headless
  `open-desk host` / `open-desk connect` commands.
- Desktop app: share this device (addresses, one-time password, connected viewer, macOS
  permission guidance) and connect to another device in a viewer window that forwards
  keyboard, mouse, and wheel input. Korean and English UI.
- Connection approval (allow control, view only, or decline) before anything is shared;
  text clipboard sharing; nearby devices on the local network (mDNS); display switching and
  speed/balanced/quality stream presets.
- `open-desk-relay`: a self-hosted rendezvous and UDP relay so devices behind different NATs
  connect by a nine-digit ID; sessions stay end-to-end encrypted and authenticated.
- Packaging: macOS app/dmg and Windows installer via cargo-packager, app icon, macOS local
  network declarations, and a tag-triggered release workflow that also ships the Linux relay.

[Unreleased]: https://github.com/j0urneyk/open-desk/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/j0urneyk/open-desk/releases/tag/v0.1.0
