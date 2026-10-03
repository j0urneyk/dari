# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- The app has a new look. It uses Dari's blue throughout and follows the system's light or dark appearance. The
  home window puts the one-time password and address up front, shows connection requests as a prominent card,
  groups settings into a list, and lists nearby and recent devices as rows you can pick from. The viewer toolbar
  has a session status dot and segmented display and quality controls, and waiting, approval, and session-ended
  states appear as centered cards over the screen.
- A new app icon: an arch bridge (다리, the app's name) with a remote pointer on its crown, replacing the generic
  monitor. The home window's header shows the same icon.
- The home window is now a sidebar beside one page at a time: this device's status, nearby devices, and recent
  addresses sit in the sidebar, and picking a device opens the connect form ready for its password. The look is
  quieter and denser, mostly in grays with blue kept for the main action, and both windows run their content up
  under a transparent title bar with the desktop showing through blurred.
- A new **Settings** page, at the bottom of the sidebar, chooses the theme (system, light, or dark; the blur behind the windows follows it), turns the
  window's translucency on or off, and sets a background picture (PNG, JPEG, or WebP) behind the home window,
  optionally blurred. The picture fills the top half of the window behind the page and fades out below
  it into the theme's own surface. Each group of settings, the password and addresses, and every notice sit on a
  panel of frosted glass, the picture blurred under a tint of the theme, so text and icons read the same over any
  picture; the sidebar is frosted the same way. Without a picture the panels are a faint wash. They are saved in `settings.toml` as `theme`, `translucent_window`, `background_image`, and
  `blur_background`. The device page's settings are now headed **Sharing**.

- Relayed sessions survive NAT rebinding: both sides refresh their relay binding every 10 seconds, and the relay
  follows a side to its new public address.
- Hosts throttle failed password attempts through a relay per viewer (by the viewer IP the relay reports) instead
  of per relay, so one viewer's guesses no longer lock out everyone else using the same relay.
- The relay protocol is now `dari-relay/2`. Update relays and apps together; 0.0.1 apps and relays can't talk to
  the new versions.

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
