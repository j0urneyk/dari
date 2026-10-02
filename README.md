# Dari

Dari is a remote desktop app for macOS and Windows 11. The same app shares your screen (host) and controls
another computer (viewer), so a Mac can control a Windows PC and the other way around. The name is Korean for
"bridge" (다리).

Every session is end-to-end encrypted and authenticated with a one-time password shown on the host. By default,
the person at the host also has to approve each connection before anything is shared. It's written in Rust with a
[gpui-kit](https://github.com/longbridge/gpui-kit) UI.

What you can do with it:

- Connect on the same network by IP address, or pick the computer from a "Nearby devices" list (mDNS).
- Connect across networks by a nine-digit ID through a relay server you run yourself. The relay only forwards
  encrypted packets.
- Let the host choose **Allow control**, **View only**, or **Decline** for each connection.
- Stream the screen as H.264 and send keyboard, mouse, and wheel input. Text clipboard sharing, display
  switching, and speed/balanced/quality presets are included.
- Shortcuts that use ⌘ or Ctrl are translated between macOS and Windows, so ⌘C on a Mac copies on the Windows
  host.

The UI follows the system language: Korean or English.

## Status

Dari is early: 0.0.1 is its first release. Automated tests run real QUIC sessions against synthetic screens, and
GUI tests render the app on macOS. Some things haven't been checked on real hardware yet:

- A session between a physical Mac and a physical Windows 11 PC.
- Screen sharing and input injection with the macOS permissions actually granted.
- The Windows installer produced by the release workflow.

## Install

Download a build from [Releases](https://github.com/j0urneyk/dari/releases):

| Platform | File |
| --- | --- |
| macOS (Apple silicon, 12 or later) | `dari_<version>_macos_aarch64.dmg` |
| Windows 11 (x64) | `dari_<version>_x64-setup.exe` |
| Relay server (Linux x64) | `dari-relay_<version>_linux_x86_64.tar.gz` |

Release builds aren't code-signed unless signing secrets are configured. The first time you run the app on
macOS, right-click it in Finder and choose **Open**. On Windows, choose **More info → Run anyway** when
SmartScreen warns you.

### macOS permissions

A Mac that shares its screen needs these permissions, granted in System Settings → Privacy & Security. The app's
"This device" card shows which are missing and has **Request permission** and **Open System Settings** buttons.

- **Screen & System Audio Recording**: lets the viewer see the screen. Restart the app after granting it.
- **Accessibility**: lets the viewer control the keyboard and mouse.
- **Local Network**: macOS asks on first launch. If you deny it, LAN connections and discovery are blocked.

On Windows, allow private network access when the firewall asks on first launch.

## Quick start

To share a screen, open the app on the host. The "This device" card shows the host's addresses (UDP port 47821 by
default) and a one-time password like `K7MXQ-3PTWA`. Give both to the person who will connect.

To control that computer, open the app on the viewer. In "Control a remote device", enter the host's address and
the password, then press **Connect**. Computers on the same network also appear under "Nearby devices".

The host user then sees a connection request and picks **Allow control**, **View only**, or **Decline**. If
nobody answers within 30 seconds, the request is declined. Once allowed, the viewer window shows the remote
screen and forwards your input. Its toolbar shows frames per second and round-trip latency, and lets you switch
displays, change quality, or disconnect.

The password is used up the moment a viewer authenticates. The host gets a new one after the session ends.

### Connecting across networks

Computers behind different routers can't reach each other directly. For that case, run `dari-relay` on a
server with a public IP:

```bash
dari-relay --listen 0.0.0.0:47822 --data-dir /var/lib/dari-relay
```

Enter the relay's address in the **Relay server** field on both computers. The host then shows **My ID**. On the
viewer, type that nine-digit ID where you'd normally put an address.

The relay just forwards UDP between the ports it allocates. The session on top is the same end-to-end QUIC +
SPAKE2 session as a direct connection, so the relay never sees the screen, input, clipboard, or password. The
[relay guide](docs/relay.md) covers firewalls, Docker, and abuse limits.

### Command line

The `host` and `connect` subcommands run without the GUI, which is handy on servers and in testing. To host,
optionally registering with a relay:

```bash
dari host --port 47821 --relay relay.example.com
```

`host` prints its addresses, the current password, and its relay ID. There's nobody to approve requests, so a
headless host gives control to any viewer that knows the password, and it doesn't share the clipboard. Only run
it where that's acceptable.

To connect by address, or by ID when `--relay` is given:

```bash
dari connect 192.168.0.10
```

`connect` asks for the password, or reads it from stdin when piped, and then reports received frames and bitrate
every second. Set `RUST_LOG` to change the log level (default `info`).

## Security model

The goal is simple: a viewer that doesn't know the host's current one-time password can't see the screen or send
input, wherever it sits on the network.

- **Encryption and authentication:** connections use QUIC with TLS 1.3, and authentication is SPAKE2, a
  password-authenticated key exchange. Its key confirmations are bound to the TLS session and to both handshake
  hellos. So a man in the middle can't splice two sessions together, and an eavesdropper can't test password
  guesses offline.
- **Passwords:** 10 characters (about 50 bits), generated per session and discarded after one successful
  login.
- **Throttling:** failed attempts are throttled per source address and globally. A host serves one session at a
  time.
- **Approval:** with approval on (the default), nothing is captured, injected, or shared until the host user
  decides. View-only sessions never start input or clipboard handling.
- **Discovery:** LAN discovery data is an unauthenticated hint. Connecting always goes through the password
  check.

[docs/security.md](docs/security.md) covers the threat model, abuse limits, and known gaps.

## Known limitations

- Windows' secure desktop (UAC prompts, the lock screen, Ctrl+Alt+Del) can't be captured or controlled by a
  regular app.
- Video uses software H.264 encoding only. There's no audio, file transfer, or unattended access.
- Builds exist only for Apple silicon Macs and x64 Windows.
- If a relayed client's public address changes mid-session (NAT rebinding), you have to reconnect.

## Building from source

You need Rust 1.99.0; `rust-toolchain.toml` pins it along with rustfmt and clippy. macOS also needs the Xcode
Command Line Tools, and Windows needs the Visual Studio Build Tools (MSVC). The first build is slow because GPUI
and OpenH264 are large.

To run the app:

```bash
cargo run -p dari
```

To build the relay:

```bash
cargo build --release -p dari-relay
```

Before opening a pull request, run the checks CI runs. `cargo lint` is an alias for clippy with `-D warnings`:

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
cargo deny check
```

On macOS, `cargo test -p dari --test gui` also runs the headless Metal GUI tests and saves PNG snapshots to
`target/gui-snapshots/`.

The crates depend on each other in one direction, `app → session → {net, media, input} → proto`:

| Crate | Role |
| --- | --- |
| `dari-proto` | Messages, protocol version, length-bounded framing |
| `dari-net` | QUIC, device certificates, SPAKE2 authentication, throttling, LAN discovery, relay client |
| `dari-media` | Screen capture, scaling, H.264 encode/decode |
| `dari-input` | Input injection, held-key tracking, shortcut mapping |
| `dari-session` | Host and viewer sessions, approval, clipboard |
| `dari-relay` | Relay server |
| `dari` | gpui-kit desktop app and CLI |

## Documentation

More detailed docs:

- [User guide](docs/user-guide.md): every setting, where files live, and troubleshooting.
- [Architecture](docs/architecture.md): threads, the session lifecycle, and capture backpressure.
- [Wire protocol](docs/protocol.md): framing, the handshake, and a message reference.
- [Security model](docs/security.md): threats, defenses, and abuse limits.
- [Development](docs/development.md): quality gates, tests, CI, and the release process.
- [Relay operations](docs/relay.md): running and securing the relay.

Changes are listed in the [CHANGELOG](CHANGELOG.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your
option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in Dari
shall be dual licensed as above, without any additional terms or conditions.
