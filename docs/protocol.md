# Wire protocol

Hosts and viewers talk over a single QUIC connection (TLS 1.3, ALPN `dari/1`). All message types live in
`dari-proto` (`crates/proto`); this document describes their format and order. The current protocol version is
**2.0** (`PROTOCOL_VERSION`).

## Version compatibility

`ClientHello` and `ServerHello` in the handshake exchange a `ProtocolVersion { major, minor }`. Peers with the same
`major` are compatible. A `minor` bump only adds messages, and a new message is never sent to a peer that didn't
advertise support for it. The host refuses a different `major` with `Rejected(IncompatibleVersion)`.

The ALPN stays `dari/1` across majors on purpose. The hello layout hasn't changed since 1.0, so a peer on another
major still completes TLS, decodes the hello, and gets that readable rejection instead of a TLS failure. Version 2.0
added the stream kind tag described below; 0.0.1 apps speak 1.0 and can't connect to newer ones.

## Framing

Every stream uses the same `MessageCodec<T>`.

```text
┌──────────────────────┬────────────────────────────┐
│ length: u32 (BE)     │ postcard(T), length bytes  │
└──────────────────────┴────────────────────────────┘
```

If the length header exceeds the channel's limit, the connection fails before the body is buffered. A decoded
message is handed to the caller only after it passes `Validate`.

| Channel | Message type | Frame limit | Stream |
| --- | --- | --- | --- |
| Handshake | `HandshakeMessage` | 4 KiB | Bidirectional stream opened by the viewer |
| Control | `ControlMessage` | 2 MiB | The handshake stream, reused once authenticated |
| Video | `VideoPacket` | 16 MiB | Unidirectional stream opened by the host |
| Audio | `AudioPacket` | One QUIC datagram (no length prefix) | Datagrams from the host |
| Relay control | `RelayRequest` / `RelayResponse` | 2 MiB | Bidirectional stream to the relay (ALPN `dari-relay/2`) |

The control channel's 2 MiB limit leaves room for clipboard text of up to 1 MiB, which travels on the same channel.
After authentication only the codec of the handshake stream is swapped (`map_decoder`/`map_encoder`) to turn it
into the control stream, so control messages that arrived right after the handshake and are already buffered
aren't lost. The QUIC transport configuration limits how many streams the peer may open: a viewer can open exactly
one bidirectional stream (handshake, then control) and no unidirectional streams. Once a session that allows file
transfer starts, the host raises the viewer's limit to 4 concurrent unidirectional streams, used only for files.
The host may open up to 4 at a time (video plus files).

### Stream kinds

Every unidirectional stream starts with one byte naming what it carries (`StreamKind`), followed by that kind's
framed messages. The receiver reads the tag before choosing a codec, so several kinds can share the connection
without depending on the order streams are opened in. A tag the receiver doesn't know is a protocol error and ends
the session; a new kind is only ever sent to a peer whose version defines it.

| Tag | Kind | Direction | Contents |
| --- | --- | --- | --- |
| `1` | `Video` | Host → viewer | Framed `VideoPacket`s, one stream for the whole session |
| `2` | `File` | Either | An 8-byte big-endian `TransferId`, then the raw bytes of one accepted file |

A file stream is finished after its last byte, so a clean end with exactly the offered size means the whole file
arrived. A cancelled file's stream is reset instead, never finished. File streams run at a lower priority than
video and control, so a large file doesn't delay frames or input. A stream that is reset before its header arrives
is skipped; it doesn't end the session.

## Handshake

```text
viewer                                         host
  │── ClientHello {version, name, os} ─────────►│  version check, precheck (busy/throttled/not accepting)
  │◄──────────── ServerHello {version, name, os}│  (or Outcome(Rejected))
  │── Pake(SPAKE2 A) ──────────────────────────►│
  │◄─────────────────────────── Pake(SPAKE2 B) ─│
  │── Confirmation(MAC_viewer) ────────────────►│  constant-time compare, claim session slot, consume password
  │◄──────────────────── Confirmation(MAC_host) ─│
  │◄────────────────────── Outcome(Accepted) ────│
```

- SPAKE2 uses the Ed25519 group, with identity strings `dari viewer` for the viewer and `dari host` for the host.
  The password is the ASCII bytes of its normalized form (ten uppercase characters, no separator). It's displayed
  as `K7MXQ-3PTWA`, and input ignores case, spaces, and `-`.
- `exporter` is the TLS `export_keying_material(32, "EXPORTER-dari-auth-v1")`.
- `transcript` is the SHA-256 hash of both hellos' postcard encodings, each prefixed with its 4-byte length.
- The confirmation is `HMAC-SHA256(K, "dari key confirmation v1" ‖ role ‖ exporter ‖ transcript)`, where `role` is
  `"viewer"` or `"host\0\0"`.
- The host sends its own confirmation **only after** verifying the viewer's. A viewer that doesn't know the
  password learns nothing from the host that would let it check a guess.
- The whole handshake must finish within 10 seconds.

Rejection reasons (`RejectReason`) are deliberately coarse: `IncompatibleVersion`, `AuthenticationFailed`, `Busy`,
`TooManyAttempts`, `NotAccepting`. When rejecting, the host calls `finish()` on the stream and waits up to
2 seconds for the viewer to close before closing the connection, because an immediate QUIC close discards data
that hasn't been sent yet.

## Control messages

| Message | Direction | Meaning |
| --- | --- | --- |
| `Ping { token }` / `Pong { token }` | Both | Round-trip measurement |
| `Disconnect` | Both | Orderly end. The sender waits up to 1 second for the peer to close |
| `Input(InputEvent)` | Viewer → host | Keyboard or pointer input (see below) |
| `RequestKeyframe` | Viewer → host | The decoder lost its state; send a keyframe |
| `HostStatus { screen, input }` | Host → viewer | Capability status, sent at session start and whenever it changes |
| `AwaitingApproval` | Host → viewer | The host user is being asked to approve |
| `Declined` | Host → viewer | The host user declined; the connection closes next |
| `Displays { displays, active }` | Host → viewer | Displays that can be shown (at most 16) and the current one |
| `SelectDisplay(id)` | Viewer → host | Switch to another display |
| `SetQuality(preset)` | Viewer → host | `Speed` / `Balanced` / `Quality` |
| `SetAudio(on)` | Viewer → host | Start or stop sending system audio. Hosts send none until asked |
| `Clipboard(text)` | Both | Clipboard text changed (at most 1 MiB, no NUL) |
| `FileOffer { id, name, size }` | Both | The sender would like to transfer a file (see below) |
| `FileAccept(id)` | Both | The receiver accepted; the sender opens the file's stream |
| `FileDone(id)` | Both | The receiver saved the whole file |
| `FileCancel { id, reason }` | Both | Declined, cancelled, or failed (`Declined` / `Cancelled` / `Failed`) |
| `SetFrameRate(fps)` | Viewer → host | The highest frame rate the viewer wants, 1..=240. The viewer sends it as its first control message; a host that admits viewers without approval waits for it (up to 2 seconds) before streaming |
| `FrameRate(fps)` | Host → viewer | The frame rate the host now streams at (the request capped by its display's refresh rate), 1..=240. Sent once streaming starts and whenever it changes |

`HostStatus` reports `screen`, `input`, `files`, and `audio`. `Availability` is one of `Available`, `PermissionDenied`
(macOS permission missing), `Unavailable`, or `NotAllowed` (input and files in a view-only session).

## File transfer

```text
sender                                         receiver
  │── FileOffer {id, name, size} ─────────────►│  checks policy; asks its user (viewer) or accepts (host)
  │◄─────────────────────────── FileAccept(id) ─│  (or FileCancel {Declined})
  │══ File stream: id, bytes…, finish ════════►│  writes <name>.part, checks the size, renames
  │◄───────────────────────────── FileDone(id) ─│  (or FileCancel {Failed})
```

- The offering side picks `id`: hosts use even numbers and viewers odd ones, so a message about an id is never
  ambiguous. An offer with the receiver's own parity, or a repeated id, is a protocol error. Messages about an id
  that already finished are ignored, because a cancel and an accept can cross.
- Either side can send `FileCancel` at any time; the sender resets the stream and the receiver deletes the partial
  file.
- Files flow only while `HostStatus.files` is `Available`, which needs a session that allows control and a host
  with file transfer on. Otherwise offers are declined.
- `name` is a single path component of at most 255 UTF-8 bytes, without `/`, `\`, control characters,
  bidirectional overrides, or zero-width characters, and not `.` or `..`. Senders NFC-normalize it (macOS stores
  names decomposed, which Windows would show as separate jamo) and apply the same sanitizing as receivers.
- Receivers map the name onto the file system with `sanitize_file_name`: Windows-forbidden characters become `_`,
  trailing dots and spaces are dropped, reserved device names (`CON`, `NUL`, `COM1`, …) get a `_` prefix, and long
  names are shortened to 200 bytes, keeping the extension. A file never replaces an existing one; it is saved as
  `name (1).ext`, `name (2).ext`, and so on.
- A receiver tracks at most 32 offers at once and declines the rest.

## Input events

| Event | Contents and limits |
| --- | --- |
| `PointerMove(PointerPosition { x, y })` | Coordinates normalized to the captured display: `0` is the left/top edge and `u16::MAX` the right/bottom. Independent of either side's resolution or DPI |
| `PointerButton { button, pressed }` | `Left`, `Right`, `Middle`, `Back`, `Forward` |
| `Scroll { dx, dy }` | Wheel lines, ±100 per axis. Positive `dy` scrolls down, positive `dx` right |
| `Key { key, pressed }` | `KeyCode::Character(c)` or `KeyCode::Named(NamedKey)`. No control characters; `Function(n)` is 1..=20 |
| `Text(String)` | Text that can't be expressed as keys, 1..=256 characters, no control or invisible formatting characters |

`KeyCode::Character` is "the key that produces this character without modifiers on a US layout". The host's
keyboard layout and IME decide the final character, so it behaves exactly like a physical keyboard, and remote
Korean composition works as is. `NamedKey` covers arrows, editing keys, F1–F20, modifiers (`Shift`, `Control`,
`Alt` = Option, `Meta` = ⌘/Windows key), `CapsLock`, `PrintScreen`, `Pause`, `NumLock`, `HangulMode`
(Hangul/English), and `HanjaMode` (Hanja). enigo provides `Insert`, `PrintScreen`, `Pause`, `NumLock`, Hangul, and
Hanja only on Windows, so macOS hosts ignore them.

The ⌘↔Ctrl mapping is applied by the viewer (`ModifierMapping`). When the two sides' shortcut modifiers differ
(`Meta` on macOS, `Control` elsewhere) and the setting is on, the two keys are swapped. So ⌘C from a macOS viewer
arrives on a Windows host as Ctrl+C, and Ctrl+C from a Windows viewer arrives on a macOS host as ⌘C.

## Audio

```text
AudioPacket { sequence: u32, data: Vec<u8> }
```

`data` is one Opus packet (RFC 6716, at most 1,276 bytes) holding 20 ms of 48 kHz stereo at about 96 kbit/s. Each
packet is postcard-encoded into one QUIC datagram, so a lost packet is never resent: the viewer conceals up to three
missing packets in a row with Opus packet loss concealment and drops packets that arrive late. `sequence` grows by
one per packet and wraps.

Audio flows only after the viewer sends `SetAudio(true)` and only while `HostStatus.audio` is `Available`, which
needs the host's **Share sound** setting on and a platform that can record its output (Windows, or macOS 14.6 and
later). It's available in view-only sessions too, since sound is output like the screen. If capture fails to start,
the host reports `audio: Unavailable`. `SetAudio(false)` stops capture on the host.

Only hosts send datagrams. A viewer accepts up to 64 KiB of buffered datagrams; a host announces a one-byte limit, so
no viewer datagram fits and the host never reads any.

The viewer plays with a jitter buffer: it waits until 60 ms are buffered, keeps at most 200 ms (dropping the oldest
beyond that so latency can't grow), and re-buffers after running dry. Audio isn't synchronized to video; each plays
as soon as it can.

## Video

```text
VideoPacket { sequence: u64, timestamp_us: u64, keyframe: bool, width: u32, height: u32, data: Vec<u8> }
```

`data` is an H.264 Annex-B bitstream. Width and height must be 1..=8192, and `data` must not be empty. The decoder
rejects output frames larger than 3840×2160. The encoder emits a keyframe at session start, on resolution change,
on display or quality switch, and on `RequestKeyframe`.

## String validation

Display strings such as names (`client_name`, `host_name`, display names) are at most 64 characters and are
rejected if they contain control characters or invisible formatting characters (U+200B–U+200F, U+202A–U+202E,
U+2060–U+206F, U+FEFF). This stops bidirectional-override tricks that make a name render as something else. When
sending its own name, each side cleans it to the same rules with `sanitize_display_text`.

## Relay

The relay control connection is a separate QUIC connection with ALPN `dari-relay/2`. The default port is UDP
47822.

| Message | Direction | Meaning |
| --- | --- | --- |
| `RelayRequest::Register` | Host → relay | Register under the ID bound to the client certificate's fingerprint |
| `RelayRequest::Connect { id }` | Viewer → relay | Ask to reach the host with this ID |
| `RelayResponse::Registered { id }` | Relay → host | The registered nine-digit ID |
| `RelayResponse::Incoming { allocation, viewer }` | Relay → host | A viewer is coming; bind to this allocation and accept QUIC on it. `viewer` is the viewer's IP as the relay sees it, used by the host to throttle failed attempts per viewer |
| `RelayResponse::Allocated(Allocation)` | Relay → viewer | Connect through this allocation |
| `RelayResponse::Refused(RelayError)` | Relay → device | `NotFound`, `TooManyRequests`, `CertificateRequired`, `Unavailable` |

`Allocation { port, token }` is the relay UDP port one side sends to and its 16-byte token. Each side binds its
address by sending a `"DRRB" ‖ token` datagram from its socket to that port, and the relay confirms with
`"DRRA" ‖ token`. The client resends every 400 ms, up to 6 times, until it's confirmed. Once both sides are bound,
the relay forwards datagrams (up to 65,535 bytes) between the two addresses without looking at them, and the QUIC
session above runs over them unchanged.

For the rest of the session, each side repeats its binding datagram every 10 seconds from the same socket. A binding
datagram with a side's token always moves that side to the address it came from, so when a NAT gives a device a new
public address or port mid-session, the relay follows it within one refresh, well inside QUIC's 30-second idle
timeout. The relay acknowledges a binding only when the address changes; a refresh from the bound address gets no
reply. A `DeviceId` is in the range 100000000..=999999999, displayed as
`123 456 789`; input ignores spaces and `-`.

## Key constants

| Constant | Value | Location |
| --- | --- | --- |
| Default host port | UDP 47821 | `crates/app/src/config.rs` |
| Default relay port | UDP 47822 | `crates/proto/src/relay.rs` |
| QUIC idle timeout / keep-alive | 30 s / 5 s | `crates/net/src/tls.rs` |
| Handshake timeout, concurrent handshakes | 10 s, 8 | `crates/net/src/endpoint.rs` |
| Approval wait | 30 s | `crates/session/src/host_session.rs` |
| Clipboard polling interval | 250 ms | `crates/session/src/clipboard.rs` |
| File streams the viewer may open, offers tracked | 4, 32 | `crates/session/src/transfer.rs` |
| Audio frame, bitrate, jitter buffer | 20 ms, 96 kbit/s, 60–200 ms | `crates/media/src/audio.rs` |
| Datagram buffer: viewer / host | 64 KiB / 1 byte | `crates/net/src/tls.rs` |
| Viewer input queue / reserved for keys | 512 / 128 | `crates/session/src/viewer.rs` |
| mDNS service | `_dari._udp.local.` | `crates/net/src/discovery.rs` |
