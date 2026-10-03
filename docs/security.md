# Security model

A remote desktop shows the screen and hands over the keyboard and mouse, so a broken authentication step gives
away the whole machine. Dari has one security goal: **a peer that doesn't know the one-time password shown on the
host can neither see the screen nor send input, wherever it sits on the network.** This document explains which
attackers that goal holds against, how, and what it doesn't cover.

## Threat model

| Attacker | Capability | Defense |
| --- | --- | --- |
| Eavesdropper on the same network | Observes every packet | QUIC (TLS 1.3) encryption. Because authentication is a PAKE, an observed handshake doesn't allow offline password guessing |
| Active man in the middle (fake host, fake viewer, relaying) | Tampers with packets, impersonates either side | SPAKE2 confirmations are bound to the TLS exporter and the hello transcript. Splicing two TLS sessions yields different exporters and fails |
| Online password guesser | Repeated connection attempts | 50-bit one-time password, per-source and global attempt throttling, password discarded after success |
| Malicious viewer (knows the password) | Sends malformed messages | Message length limits and validation, host-user approval, view-only mode |
| Malicious host | Sends malformed data to the viewer, or files nobody asked for | Frame and decode size limits, display-string validation, clipboard limit; files from the host are saved only after the viewer user accepts them |
| Relay operator or fake relay | Observes or tampers with relayed traffic | The session is end-to-end QUIC + SPAKE2; the relay only sees ciphertext |
| mDNS spoofer on the same LAN | Advertises fake "nearby devices" | Advertisements are display hints only; connecting always uses PAKE authentication |

Out of scope: a host or viewer machine that is already compromised, a user who tells the attacker the password
(social engineering), and denial of service.

## Device identity

On first launch, rcgen creates a self-signed certificate that is stored in the data directory as
`identity-cert.der` and `identity-key.der`. On Unix the key file is created with mode 0600. If the files are
corrupt, a warning is logged and a new identity is created. The certificate's SHA-256 fingerprint is the device
identifier, used for the relay ID and for filtering a device's own mDNS advertisement.

There's no CA, so the viewer doesn't validate a chain for the host certificate. TLS 1.3 signature verification is
still always performed with the rustls crypto provider (ring), which proves the peer actually holds the
certificate's key. **Authenticating who you're talking to is entirely the job of the PAKE below.**

## One-time passwords

- Ten characters are drawn from a 32-symbol alphabet that leaves out easily confused characters (`I`, `O`, `0`,
  `1`), for 50 bits. 256 is a multiple of 32, so masking the low five bits is unbiased. Randomness comes from the
  OS random number generator (`getrandom`).
- Passwords are wiped from memory with `zeroize`, compared in constant time, and never appear in `Debug` output.
- A password is consumed the moment authentication succeeds. Until a new one exists, the host refuses new viewers
  with `NotAccepting`. During a session the password is cleared from the screen, and a new one is created when the
  session ends. The user can make a new one at any time.
- If authentication succeeds but the session fails before it starts (for example, the confirmation can't be sent),
  the password is restored, but only if it hasn't changed in the meantime, which is checked with a generation
  counter. Without this, the host could end up silently refusing every viewer with no password at all.

## SPAKE2 handshake

The sequence is in the [protocol document](protocol.md#handshake). The properties that matter for security are:

- **No offline guessing**: observing SPAKE2 messages, or talking to a party once as a man in the middle, gives no
  information to test password candidates offline. An attacker can test one candidate per attempt.
- **Channel binding**: the confirmation includes the TLS exporter, so an attacker that runs one TLS session with
  the viewer and another with the host and relays messages gets different exporters on each side, and confirmation
  fails (tested: MITM with mismatched exporters).
- **Transcript binding**: the hash of both hellos goes into the confirmation, so the version, names, and OS can't
  be swapped.
- **The host doesn't reveal first**: the host sends its confirmation only after verifying the viewer's in constant
  time. Claiming the session slot and consuming the password also happen atomically at this point.
- **Coarse rejection reasons**: there are only five rejection reasons, which tell an attacker nothing useful.

## Abuse limits

Host (`crates/net/src/limiter.rs`, `endpoint.rs`):

- Failures are recorded per source, with IPv6 grouped by /64. The first 3 failures are treated as typos; after that,
  exponential backoff applies (up to 5 minutes). A source's history is forgotten after an hour of quiet.
- Global limit: 20 failures per minute across all sources briefly locks out everyone. At most 4,096 sources are
  tracked; beyond that the quietest source is evicted (the global limit still bounds the overall guessing rate).
- Throttled sources are refused with `refuse()` before the TLS handshake. Every failure and timeout after the
  ServerHello (the point where a password guess becomes possible) counts as a failure.
- At most 8 concurrent handshakes, 10 seconds each. One session at a time; others get `Busy`.
- Relayed viewers are throttled by the viewer IP the relay reports with each allocation, not by the relay's own
  address, so one viewer guessing through a relay doesn't lock out others behind the same relay. That IP is the
  relay's claim: a malicious relay could misreport it to spread guesses across buckets, but the global limit still
  caps the overall rate, and such a relay could already disrupt every relayed connection.

Relay (`crates/relay/src/server.rs`, `forward.rs`): 30 requests per minute per source IP, 4 pending connects per
host, a cap on total allocations (`--max-allocations`, default 256), and allocations freed if not bound within
30 seconds or idle for 120 seconds. IDs have nine digits (about 900 million), so finding online devices by brute
force is hard, and a found device still requires the password.

## Approval and scope of access

With connection approval on (the default), even a viewer that proved the password receives nothing until the host
user decides. Capture, the input thread, the clipboard, and the display list are all created after the decision,
and input that arrives while waiting is dropped. No answer within 30 seconds means decline.

A **view-only** session never creates the input thread or clipboard sync, never grants the viewer stream credit,
and reports `HostStatus.input = NotAllowed` and `HostStatus.files = NotAllowed`. When the viewer receives this, it
stops its own clipboard sharing and refuses to send or accept files too.

The headless CLI host (`dari host`) has nobody to approve requests, so it gives control to any viewer that knows the
password and turns off the clipboard and file transfer. Use it only on servers or for testing.

## Audio

System audio can carry private sound (calls, notifications), so the host shares it only while **Share sound** is on,
and only after the session was approved and the viewer asked with `SetAudio(true)`. Nothing is recorded while the
host user decides. View-only viewers hear the host too, the same way they see its screen; turn **Share sound** off
to prevent that. Audio travels as datagrams from host to viewer only: hosts announce a one-byte datagram limit, so a
viewer can't send them any. Each datagram is decoded and validated (at most 1,276 bytes of Opus) before playback.

## File transfer safety

- **Who may transfer:** files flow only in sessions that allow control, with file transfer on at the host. A viewer
  with control can already do anything the host user can, so the host saves its files to Downloads without asking.
  Files from the host are different: the viewer user sees each offer (name and size) and saves or declines it, so a
  host can't fill the viewer's disk unasked.
- **Stream credit:** the QUIC limit on unidirectional streams a viewer may open stays at zero through the handshake
  and approval, and in view-only sessions. The host raises it to 4 only when it enables file transfer, so an
  unauthenticated or view-only peer can't push streams at all.
- **Names:** a peer's file name is rejected unless it is a single component without separators, control
  characters, bidirectional overrides, or zero-width characters (an `exe` disguised as `photo‮gnp.exe` is
  refused). The receiver then makes it safe for both macOS and Windows and never overwrites an existing file. Files
  are only ever written inside the downloads folder.
- **Integrity and cleanup:** a file is written as `<name>.part` and renamed only when exactly the offered number of
  bytes arrived on a cleanly finished stream. A reset stream, a short or long stream, a cancel from either side,
  or the session ending deletes the partial file. QUIC already authenticates every byte end to end.
- **Limits:** at most 32 offers are tracked per session; more are declined. File streams run below video and
  control priority.

## Input and clipboard safety

- When a session ends, events left in the input queue are dropped instead of injected, and every held key and
  button is released.
- When the viewer window loses focus, it sends releases for every held key, button, and modifier.
- The clipboard never sends what was already on it when the session started; only text copied during the session
  is shared. Received values are remembered so they aren't echoed back, and a 1 MiB limit and a NUL ban apply.

## Message validation

Every message is read through length-bounded framing (4 KiB for the pre-authentication handshake) and must pass
`Validate` after decoding. Display strings from the peer, such as names, are at most 64 characters and reject
control characters, bidirectional overrides, and zero-width characters. This prevents a peer's name from rendering
as a different name on the approval card or in a window title. Video accepts only frames up to 8192 per side and
decoded output up to 3840×2160, and tests check that feeding arbitrary bytes to the decoder doesn't panic.

## Relay

The relay is an untrusted component. Host and viewer run the same QUIC + SPAKE2 session over the relay's UDP
forwarding as a direct connection, so the relay can't see the screen, input, clipboard, or password, and can't
impersonate the host (exporter binding). That's why clients don't verify the relay's own certificate: the most a
fake relay can do is disrupt connections.

Hosts present their device certificate as a TLS client certificate when registering. The TLS signature proves key
possession, so one device can't take over another's ID. Binding tokens are 16 random bytes, different for every
allocation and every side, and datagrams from anywhere other than the two bound addresses are dropped (tested:
stranger injection and forged bindings are ignored). A binding datagram carrying a side's token moves that side to
the sender's address, which is how sessions survive NAT rebinding; only a holder of the token can do it, and the
session itself stays protected end to end.

What the relay can learn: which devices are online, when connections happen, how much traffic flows, and both
sides' public IPs.

## Known limitations

- There's no TOFU (pinning the host certificate fingerprint on first sight). The one-time password authenticates
  every session, so it wasn't considered necessary, but it should be revisited if unattended access (permanent
  passwords) is ever added.
- The Windows secure desktop (UAC, the lock screen) and Ctrl+Alt+Del can't be captured or injected from a regular
  user process.
- The private key and settings are stored in plain files in the user data directory (no OS keychain).

## Hardening found in review

Every change went through code and security review until no findings remained. No vulnerability at or above the
reporting threshold was found. These below-threshold hardening items were fixed anyway:

- Remote input still queued when a session ended used to be injected.
- Bidirectional overrides and zero-width characters in peer names.
- The viewer kept sending clipboard text after the host later reported view-only.
- Relay datagrams larger than 1500 bytes were truncated (a functional defect).
