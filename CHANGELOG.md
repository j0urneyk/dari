# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- Text on a still remote screen sharpens within half a second after it stops changing, and the Quality preset streams
  screens up to 3840x2160 at their full resolution instead of scaling them to 2560 pixels.

### Fixed

- The remote computer's sound plays smoothly instead of in short fragments with ticks between them.
- From a Mac, switching your input source (Caps Lock, Ctrl+Space, or the Globe key) while the viewer window is active
  now switches a Windows host between Korean and English.
- Switching the Mac's input source with Ctrl+Space no longer opens the Start menu on a Windows host.
- When the Windows host shows a User Account Control prompt, the lock screen, or Ctrl+Alt+Del, the viewer says so
  instead of showing a frozen picture.
- A second User Account Control prompt that follows an unchanged screen is reported too, instead of leaving the viewer
  on a frozen picture.

## [0.0.3] - 2026-10-08

### Fixed

- The macOS app downloaded from a release no longer reports that it is damaged. Builds without a developer
  certificate are now ad-hoc signed as a whole bundle, so macOS lets you open the app from Privacy & Security.

## [0.0.2] - 2026-10-05

### Added

- File and folder transfer in both directions for sessions that allow control. Drop files or folders on the remote
  screen or use **Send file…** in the viewer toolbar or the host's session card. The host saves the viewer's files
  to Downloads; files from the host wait until the viewer user chooses **Save**. Names are normalized (no more separated Korean
  jamo from macOS), made safe for Windows, and never overwrite an existing file; cancelled or failed transfers
  leave no partial file. Folders keep their structure; colliding names inside them are numbered, and symbolic links
  are not followed. Turn it off with **Exchange files** on the host.
- Sound from the remote computer. The host records what it plays (WASAPI loopback on Windows, a Core Audio process
  tap on macOS 14.6 and later) and sends 20 ms Opus packets as QUIC datagrams; the viewer plays them through a small
  jitter buffer and conceals lost packets. **Sound on / Sound off** in the viewer toolbar mutes it, which also stops
  recording on the host. Hosts turn it off with **Share sound**. View-only viewers hear the host too.
- Frame rate choice in the viewer toolbar: Auto, 30, 60, 90, 120, or 144 fps, separate from the quality preset.
  Auto follows the viewer's fastest display, and the host caps the rate at its own display's refresh rate and
  reports what it settled on. `dari connect --fps` does the same from the command line. Faster streams get a
  higher bitrate.
- **Language** on the settings page: System, 한국어, or English. The choice is saved and every open window
  switches right away; System keeps following the system language as before.

### Changed

- The app has a new look. It uses Dari's blue throughout and follows the system's light or dark appearance. The
  home window puts the one-time password and address up front, shows connection requests as a prominent card,
  and lists nearby and recent devices as rows you can pick from. The viewer toolbar
  has a session status dot and segmented display and quality controls, and waiting, approval, and session-ended
  states appear as centered cards over the screen.
- A new app icon: an arch bridge (다리, the app's name) with a remote pointer on its crown, replacing the generic
  monitor. The home window's title bar shows the same icon.
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
- macOS hosts capture with ScreenCaptureKit and encode with VideoToolbox in hardware, instead of taking a full
  screenshot and encoding it on the CPU for every frame. On an M5 MacBook the Balanced quality went from 18 fps to
  the full refresh rate of its 120 Hz display, and the encoder keeps up with 144 fps; the host only sends frames
  when the screen changes.
- Windows hosts capture with Windows.Graphics.Capture, scale and convert frames on the GPU, and encode with the
  graphics card's hardware H.264 encoder (Intel, NVIDIA, AMD, or Qualcomm, through Media Foundation), instead of
  taking a full screenshot and encoding it on the CPU for every frame, which held them to about 20 fps at 1920px.
  Like macOS hosts, they only send frames when the screen changes. PCs without a hardware encoder, and any encoder
  failure, fall back to the CPU encoder as before.
- Viewers convert decoded frames to screen pixels about four times faster, on macOS and Windows alike. On an M5
  MacBook a 2560×1662 stream (the Quality preset) went from about 9.6 ms to 2.4 ms per frame, so viewers now keep
  up with 144 fps hosts instead of stalling near 110 fps. Colors are unchanged.
- Dari now requires macOS 13 or later (was 12).
- Two Macs and two Windows PCs are supported pairings, not only a Mac and a Windows PC; ⌘ and Ctrl shortcuts pass
  through unchanged between computers on the same OS. The nightly cross-device check now runs sessions between two
  macOS runners and between two Windows runners as well, in both directions, directly, through a relay, and view only.
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

- A Mac host whose user refused system audio recording no longer streams silence as if sharing its sound. macOS still
  lets a refused app record, but only silence, so the host now asks macOS whether it was refused and tells the viewer,
  whose toolbar shows **No sound permission** with where to allow it. While macOS is still asking the host's user, the
  viewer's sound button reads **Waiting for sound permission** instead of playing nothing, and sound starts once they
  allow it.
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

[Unreleased]: https://github.com/j0urneyk/dari/compare/v0.0.3...HEAD
[0.0.3]: https://github.com/j0urneyk/dari/compare/v0.0.2...v0.0.3
[0.0.2]: https://github.com/j0urneyk/dari/compare/v0.0.1...v0.0.2
[0.0.1]: https://github.com/j0urneyk/dari/releases/tag/v0.0.1
