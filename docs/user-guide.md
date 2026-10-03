# User guide

Installation and the basic flow are in the [README](../README.md#install). This guide covers each part of the
screens, the settings, where files are stored, and what to check when something goes wrong. The UI is in Korean
when the system language is Korean, and in English otherwise, and it follows the system's light or dark appearance.

The home window has a sidebar and a page beside it. **This device** in the sidebar sums up its state (accepting
connections, a pending connection request, a connected viewer, or remote access off) and opens the page for sharing
this device; **Control a remote device** opens the connect form. Below them are **Nearby devices** and **Recent**.
A connection request brings the **This device** page forward on its own, since it is declined after 30 seconds.
**Settings** at the bottom of the sidebar opens the settings page. The window's content runs up under a
transparent title bar.

## Home window

### This device (sharing your screen)

| Item | Description |
| --- | --- |
| **Allow remote access** | The switch at the top right of the page. When off, new connections are refused. A running session is not affected |
| **Addresses** | The `IP:port` that devices on the same network enter, with any further addresses under **Other addresses**. The default port is UDP 47821 |
| **One-time password** | Ten characters, like `K7MXQ-3PTWA`. Copy, show/hide, and **New password**. It's gone once a connection uses it, and a new one is made when the session ends |
| **My ID** | The nine-digit ID (`123 456 789`) shown when a relay server is set |
| **Relay server** | Enter `server` or `server:port` and press Enter. Leave it empty to not use a relay |
| **Connection request** card | Shown when approval is on: the connecting device's name with **Allow control / View only / Decline**. Declined if nothing is chosen within 30 seconds |
| Connected viewer | During a session, the peer's name and a **Disconnect** button |
| Permission notice (macOS) | When Screen Recording or Accessibility is missing, **Request permission** and **Open System Settings** buttons |
| **Ask before each connection** | On by default. When off, a viewer that knows the password gets control immediately |
| **Share clipboard** | On by default. Text is exchanged only in sessions that allow control |
| **Show this device on the local network** | On by default. Advertises the name over mDNS so it appears in the other side's "Nearby devices" |

Changes to the approval and clipboard settings apply from the next session.

### Control a remote device

In the address field, enter an IP address such as `192.168.0.10`, `192.168.0.10:47821`, or `[fe80::…]:47821`, a
hostname, or, when using a relay, a nine-digit ID. Without a port, 47821 is used. To connect by ID, your side needs
the same relay server set too. The password ignores case, spaces, and `-`. Press Enter to connect right away.

In the sidebar, **Recent** lists the last five addresses, and **Nearby devices** lists devices advertising on the
same network. Picking one opens this form with the address filled in and the cursor in the password field. Nearby-device information is an unauthenticated display hint; connecting always checks the password.

**Translate ⌘ and Ctrl shortcuts** (on by default) swaps ⌘ and Ctrl between macOS and Windows. When a Mac controls
Windows, ⌘C arrives as Ctrl+C and copies as usual.

### Settings

| Item | Description |
| --- | --- |
| **Theme** | **System** (follows macOS or Windows), **Light**, or **Dark** |
| **Translucent window** | On by default. What is behind the window shows through, blurred, through the background picture too. Viewer windows opened afterwards follow it too |
| **Background picture** | A PNG, JPEG, or WebP picture shown behind the home window. It fills the top half of the window and fades out into the theme's own surface below. The page's panels and the sidebar show it as frosted glass, so text stays readable over any picture. Pictures taller than 3:2 show their middle. **Choose…** picks one, **Remove** goes back |
| **Blur the picture** | Off by default. Blurs the whole picture, not just where it fades out |

## Viewer window

The remote screen is drawn to fit the window while keeping its aspect ratio, with letterboxing for the rest. Mouse
movement, clicks, the wheel, and key presses over the screen go to the remote device. Tab, Shift-Tab, and
⌘C/Ctrl+C go to the remote device too. Korean text is composed by the remote device's input method, so turn on the
Korean input method on the remote side (the Hangul/English key works only on Windows hosts).

The toolbar, which is also the window's title bar, shows frames per second and round-trip latency (`30 fps · 12 ms`), and offers **Display** (choose
among monitors), quality (**Speed / Balanced / Quality**), and **Disconnect**. The dot before the device name is
green while the session runs, amber while the host is still deciding, and gray once the session is over; a finished
session explains why and offers **Close**. Limits such as a view-only session appear as a notice at the top of the
screen. Closing the window also ends the
session. When the window loses focus, every held key and button is released, so nothing stays pressed on the
remote device.

Status messages:

- "Waiting for the remote side to allow the connection…": the host user hasn't decided yet.
- "View-only session: the remote side did not allow control.": input and clipboard are not forwarded.
- "The remote device has not granted Screen Recording permission." / "The remote device cannot be controlled
  (Accessibility permission needed). View only.": the permission has to be granted on the remote Mac.

## Where files are stored

The device certificate and settings live in the app data directory.

| OS | Location |
| --- | --- |
| macOS | `~/Library/Application Support/dev.dari.dari/` |
| Windows | `%LOCALAPPDATA%\dari\dari\data\` |

| File | Contents |
| --- | --- |
| `identity-cert.der`, `identity-key.der` | The device certificate and private key. Deleting them makes this a new device, and its relay ID changes |
| `settings.toml` | The settings below |

`settings.toml` holds the following. Most can be changed in the home window; `port` can only be changed in this
file. If the file can't be read, a warning is logged and the defaults are used.

```toml
hosting_enabled = true        # Allow remote access
port = 47821                  # UDP port the host listens on
map_shortcut_modifier = true  # Translate ⌘ and Ctrl shortcuts
recent_addresses = []         # Recent addresses (up to 5)
require_approval = true       # Ask before each connection
clipboard_sync = true         # Share clipboard
lan_discovery = true          # Show this device on the local network
relay_address = ""            # Relay server
theme = "system"              # Theme: "system", "light", or "dark"
translucent_window = true     # Let what is behind the windows show through
blur_background = false       # Blur the background picture
# background_image = "/path/to/picture.jpg"  # Background picture; absent shows the desktop
```

## Command line

These subcommands run without the GUI, for servers and testing.

```bash
dari host --port 47821 --relay relay.example.com
```

```bash
dari connect 192.168.0.10 --relay relay.example.com
```

`host` prints its addresses, device fingerprint, password (it changes every session), and relay ID, and waits
until Ctrl+C. There's nobody to approve requests, so it gives control to any viewer that knows the password and
turns off the clipboard. `connect` asks for the password (it can also be piped in), connects, and prints received
frames and bitrate every second.

Set the log level with the `RUST_LOG` environment variable (default `info`), for example
`RUST_LOG=dari_net=debug dari host`.

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| Can't connect on the same network | That "Allow remote access" is on and the address and port are right. That Windows Firewall allows private network access. On macOS 15 or later, that Dari is enabled in System Settings → Privacy & Security → Local Network |
| The other device isn't in "Nearby devices" | That "Show this device on the local network" is on over there. Public Wi-Fi and corporate networks sometimes block mDNS; enter the address directly instead |
| "The password is not correct" | The password changes after each use. Check the value shown on the other screen right now |
| "Too many failed attempts. Try again later" | After several wrong attempts you're blocked for a while (up to 5 minutes). Wait and try again |
| "The remote device is already in a session" | Only one session at a time is allowed |
| The screen is black or shows only the wallpaper | The remote Mac needs Screen Recording permission, and the app must be restarted after granting it |
| The screen is visible but control doesn't work | Check the remote Mac's Accessibility permission, or whether the other side chose "View only" |
| UAC prompts or the lock screen don't appear on Windows | A known limitation: regular apps can't capture or control the secure desktop |
| The stream stutters or lags | Lower the quality to "Speed" in the toolbar (1280px, 1.5 Mbps) |
| "Set a relay server to connect by ID" | Enter the same relay address as the other side in your "Relay server" field |
| "Cannot reach the relay (…). Retrying…" | Check the relay address and firewall. The app keeps retrying every 2 to 60 seconds. See [relay operations](relay.md) |
| A relayed session suddenly drops | Check that both devices and the relay run the same Dari version. A changed public address (NAT rebinding) is picked up within about 10 seconds; if the network was gone for 30 seconds or more, reconnect |
