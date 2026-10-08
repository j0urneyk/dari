# Dari documentation

Dari is a remote desktop app that lets macOS and Windows 11 connect to each other. Installation and basic use are
in the repository [README](../README.md); this directory goes deeper.

| Document | Contents |
| --- | --- |
| [User guide](user-guide.md) | Screens, settings, where files are stored, troubleshooting |
| [Relay operations](relay.md) | Running the relay that connects devices on different networks by ID, and its abuse limits |
| [Architecture](architecture.md) | Crates, threading model, session lifecycle, data flow |
| [Wire protocol](protocol.md) | Framing; handshake, control, video, input, and relay messages; constants |
| [Security model](security.md) | Threat model, authentication and encryption, abuse limits, known limitations |
| [Development](development.md) | Toolchain, quality gates, test suites, CI, release process |
| [Design: the Windows secure desktop](design/secure-desktop.md) | Approved, not built yet: answering UAC prompts, the lock screen, and Ctrl+Alt+Del from a viewer |

## Goals and scope

- One app is both the host (shares its screen) and the viewer (controls a remote screen).
- macOS ↔ Windows 11, macOS ↔ macOS, and Windows ↔ Windows all work.
- Connections are end-to-end encrypted, and a peer that doesn't know the one-time password can neither see the
  screen nor send input.
- Both the local network (direct IP, LAN discovery) and other networks (through a self-hosted relay) are supported.

Out of scope for now: hardware video encoders (VideoToolbox, Media Foundation), audio and file transfer, capturing
the Windows secure desktop (UAC prompts, the lock screen; this needs a system service), unattended access
(permanent passwords), multiple simultaneous sessions, and mobile clients.
