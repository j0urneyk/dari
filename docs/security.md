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
  are only ever written inside the downloads folder. A folder's paths are checked component by component, so `..`
  or a separator can't reach outside it, and colliding names (including by case) are numbered rather than
  overwriting each other. Senders don't follow symbolic links, so a link inside a shared folder can't expose files
  outside it.
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

## The Windows secure-desktop helper

On Windows, Dari adds two SYSTEM processes, `dari-service.exe service` and `dari-service.exe helper`, so it can later
show and answer UAC prompts and the lock screen ([design](design/secure-desktop.md)). Neither has network code: a test
reads `dari-service.exe`'s import table and fails if it links `ws2_32.dll` or another Windows networking DLL. A remote
peer still reaches only `dari.exe`, and only after the password and approval: the app starts the helper only for an
approved session, and with input off for a view-only one. So far the helper only reports which desktop receives input,
and the app ignores every other message from it.

| Boundary | Who is on the other side | Check |
| --- | --- | --- |
| `dari.exe` to `\\.\pipe\dari-service` | Any local process | The service creates all four instances of the pipe at start, the first with `FILE_FLAG_FIRST_PIPE_INSTANCE`, each with `PIPE_REJECT_REMOTE_CLIENTS` and a DACL that lets interactive users read and write data, but not create pipe instances. Only SYSTEM, the service's own user, may also create instances, which the service needs for the other three. Before reading a byte, the service reads the client's process ID and session from the pipe, opens the process, and accepts it only if its image is `dari.exe` in the service's own folder (compared without case) and its session is active. It keeps that process handle while the helper runs, so the ID can't be reused. A refused client is disconnected at once. A client that passed gets 2 seconds to send its request and 2 more, counted after the service acts on it, to read the reply. One helper per session: a `StartHelper` from the app process the running helper serves ends that helper and starts a new one, and one from another process is refused. At most 10 refusals a minute are answered and logged. An interactive user's process can keep connecting to all four instances, which at worst makes the app give up after 5 seconds and run the session without the helper: it denies service, and learns nothing but a refusal |
| The helper's pipe, seen from `dari.exe` | The client that connects | The app creates `\\.\pipe\dari-helper-<32 random hex digits>` (one instance, local clients only, readable and writable by SYSTEM and the app's logon SID). The random part comes from the OS and is never logged. It is passed on the helper's command line, which a medium-integrity process couldn't read in the test VM. It is no secret anyway: any local process can list pipe names. A process of the user's logon session that connects before the helper is refused by the check below, and the helper then can't connect, so the worst it does is deny service. The app reads the client's first message, impersonates the client at identification level, and accepts it only if the token's user is LocalSystem (`S-1-5-18`), before acting on that message. Any SYSTEM process passes, and a SYSTEM process can read keystrokes anyway |
| The helper's pipe, seen from the helper | The pipe's server | The helper compares `GetNamedPipeServerProcessId` with the process ID of the handle the service let it inherit before it sends anything. The service and the helper hold that handle open, so the ID can't be reused |
| A squatter on `\\.\pipe\dari-service` | Any local process, while the service is stopped | It can refuse or ignore `StartHelper`, but no SYSTEM client connects to the app's pipe, so the app logs that the link ended and the session carries on with the `SecureDesktop` notice. The service then fails to create its pipe, logs an error, and stops with a service-specific exit code, which `scripts/crosscheck/windows/squat-service-pipe.ps1` checks in the VM |

Before it duplicates a token, the service checks that the session's `winlogon.exe` process has the image `winlogon.exe`
in the system directory (`GetSystemDirectoryW`) and that its token's user is LocalSystem, passing over any process that
only shares the name. The helper runs with that token restricted by `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE)`,
which keeps the SYSTEM SID and System integrity but removes every privilege except `SeChangeNotifyPrivilege`. Its first
event log entry lists its user, integrity level, session, and privileges. It inherits only the app's process handle,
runs in a job that ends it when the service exits, loads DLLs only from System32, creates no windows, and exits when its
pipe closes or the app exits. `dari-service.exe` has no console, because a console program started as SYSTEM opens a
console window on the user's desktop.

Code running as the signed-in user can do what `dari.exe` can, including talking to the service and the helper as if it
were the app. The image check stops other programs, not code injected into Dari. Such code can start `dari.exe`, inject
into it, and drive the helper, which today reveals only desktop names and with #46 and #47 will see and answer the
secure desktop. The design's [What this design can't stop](design/secure-desktop.md#what-this-design-cant-stop) explains
why that cost was accepted.

## Known limitations

- There's no TOFU (pinning the host certificate fingerprint on first sight). The one-time password authenticates
  every session, so it wasn't considered necessary, but it should be revisited if unattended access (permanent
  passwords) is ever added.
- The Windows secure desktop (UAC, the lock screen) and Ctrl+Alt+Del can't be captured or injected yet. The helper
  above only reports when the secure desktop is up.
- The private key and settings are stored in plain files in the user data directory (no OS keychain).

## Hardening found in review

Every change went through code and security review until no findings remained. No vulnerability at or above the
reporting threshold was found. These below-threshold hardening items were fixed anyway:

- Remote input still queued when a session ended used to be injected.
- Bidirectional overrides and zero-width characters in peer names.
- The viewer kept sending clipboard text after the host later reported view-only.
- Relay datagrams larger than 1500 bytes were truncated (a functional defect).
