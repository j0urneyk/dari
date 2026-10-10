# Answering the Windows secure desktop from a viewer

Status: approved. Option B, a per-machine-only installer, and secure-screen control on by default. Nothing here is
built yet.

When a Windows host shows a User Account Control (UAC) prompt, the lock screen, or the Ctrl+Alt+Del screen, Windows
switches to the Winlogon secure desktop. Dari's host can't see or touch that desktop, so the viewer can only tell the
user that someone at the host must answer it ([PR 36](https://github.com/j0urneyk/dari/pull/36)). This document
designs the change that lets the viewer see that desktop, answer it, and send Ctrl+Alt+Del. It needs a Windows
service that runs as SYSTEM, so most of the document is about keeping that service from becoming a way into the
machine.

## Goals

- While a session is live, the viewer sees the UAC consent and credential prompts, the lock screen, and the
  Ctrl+Alt+Del screen, on every display, and the session survives the switch in both directions.
- A viewer that the host user allowed to control the PC can click and type on those screens, and can send
  Ctrl+Alt+Del.
- A remote peer gets nothing new. The one-time password, approval, and view-only rules hold exactly as they do now.
- No SYSTEM process reads from the network. The only data from a peer that reaches SYSTEM code is input events and
  display IDs, after `dari.exe` has validated them.

## Non-goals

- Unattended access: signing in after a reboot, or hosting when no user runs Dari. The service starts the helper
  only for a running Dari app, so this design doesn't add it.
- Sessions other than the one the Dari app runs in (fast user switching to another user, Remote Desktop sessions).
- Weakening Windows' own settings, such as turning off `PromptOnSecureDesktop`. PR 36 already ruled that out.
- Code signing. The risks section covers what its absence costs.

## Why the host can't reach the secure desktop today

Windows runs each signed-in session in a window station (`WinSta0`) with several desktops. Apps draw on `Default`.
UAC, the lock screen, and the Ctrl+Alt+Del screen draw on `Winlogon`. Only one desktop receives input at a time: the
input desktop. Microsoft's documentation for `IDXGIOutput1::DuplicateOutput` says that only an application running
as LocalSystem can access the secure desktop.

The Dari host is a per-user process. The NSIS installer runs with `installer-mode = "currentUser"`
(`crates/app/Cargo.toml`), so `dari.exe` lives under `%LOCALAPPDATA%` and runs with the user's medium-integrity
token. Three things fail for it on the secure desktop:

- Windows.Graphics.Capture (`crates/media/src/win/capture.rs`) can't see the secure desktop. Behind the lock screen
  it delivers no frames, and behind a UAC prompt it keeps delivering the dimmed desktop. It reports no error in either
  case, which is why PR 36 checks the input desktop with `OpenInputDesktop` every 300 ms.
- `SendInput`, which enigo calls (`crates/input/src/backend.rs`), injects only into the calling thread's desktop. A
  process that can't open `Winlogon` can't attach a thread to it, so its input never reaches the prompt.
- Ctrl+Alt+Del is the secure attention sequence. Windows handles it below the input queue, so no injected key event
  produces it. The only programmatic way is `SendSAS`, which only a service or a `uiAccess` app may call.

So the work splits into three abilities, and each needs code running as SYSTEM in the user's session: capture
`Winlogon`, inject into it, and raise the secure attention sequence.

## Options

### Option A. Run the whole host as SYSTEM (the RustDesk model)

A Windows service starts, in the user's session, a copy of the host as `rustdesk.exe --server`, with the token of
that session's `winlogon.exe` (`LaunchProcessWin` in RustDesk's `src/platform/windows.cc`). That SYSTEM process owns
the network endpoint, authentication, capture, and input. Before it injects each input event it calls
`try_change_desktop` (`src/platform/windows.rs`), which attaches the thread to whatever `OpenInputDesktop` returns.
Its capture loop notices a desktop change and restarts. The user's UI (`--cm`) talks to it over a named pipe whose
server checks the client's session and executable path.

It works on every desktop with one capture path, and RustDesk proves it in the field. It also puts everything Dari
parses from the network under SYSTEM: QUIC, TLS, SPAKE2, postcard control messages, clipboard text, and file
streams. File transfer becomes a SYSTEM process writing into a user-controlled `Downloads` folder, a classic way to
turn a junction into an arbitrary write. Audio, the clipboard, and the Downloads path all belong to the user and
would need impersonation. One bug in any of that code would give a remote peer the machine.

### Option B. Keep the host as the user and add a small SYSTEM helper (recommended)

`dari.exe` stays a per-user process and keeps everything it does now, network and authentication included. A new
`dari-service.exe` adds two roles:

- The **service** runs as LocalSystem in session 0. It has no network code. It starts the helper on request and
  raises the secure attention sequence.
- The **helper** runs as SYSTEM in the user's session, started with that session's `winlogon.exe` token. It follows
  the input desktop, captures `Winlogon` with DXGI Desktop Duplication, and injects input into it. It has no
  network code either. Its only peer is the `dari.exe` the service vetted.

The app keeps Windows.Graphics.Capture and `SendInput` on `Default` and hands off to the helper only while
`Winlogon` is the input desktop.

### Option C. Rejected alternatives

| Alternative | Why not |
| --- | --- |
| Turn `PromptOnSecureDesktop` off | Moves UAC prompts to `Default` but does nothing for the lock screen or Ctrl+Alt+Del, and lowers the host's security without asking. PR 36 rejected it |
| Mark the app `uiAccess` | `uiAccess` lets a signed app in a secure folder send input to higher-integrity windows on its own desktop. It can't capture or reach `Winlogon`, and it would need code signing |
| Move all Windows capture into the helper (DXGI everywhere) | One capture path, but every frame of every session would cross the process boundary, and Windows.Graphics.Capture's measured pacing and zero-copy hardware encode would go. The secure desktop is rare and mostly still, so it can take a slower path |
| Encode H.264 in the helper | Keeps frame transfer small, but loads Media Foundation and OpenH264 into a SYSTEM process and gives the stream two encoders and two reference chains |

### Comparison

| | A. SYSTEM host | B. User host and SYSTEM helper |
| --- | --- | --- |
| Code running as SYSTEM | All of the host, including the network stack | Desktop following, DXGI capture, `SendInput`, `SendSAS`, two local pipes |
| A remote peer's way to SYSTEM | Any parser bug in the host | None directly. A peer must first take over `dari.exe`, then exploit the helper's pipe |
| Peer-chosen data that SYSTEM code parses | Everything: handshake, control messages, clipboard, file streams | `InputEvent` (including `Text` up to 256 characters) and display IDs, after `Validate` |
| Same-user malware answering UAC consent prompts | Possible (it can drive the approval UI and act as a viewer) | Possible (it can act as `dari.exe` on the pipe). See the security model |
| Capture paths | One (DXGI) | Two (Windows.Graphics.Capture, then DXGI while `Winlogon` is up) |
| Clipboard, files, audio | Need impersonation of the user | Unchanged |
| Path to unattended access later | Short | Longer: the host would have to move into a service then |
| Size | Larger: the host moves into a service, and the UI talks to it over IPC | Smaller: one new binary and a frame source plus an input route in the app |

## Recommendation

Build option B. It is the only option where a remote peer never talks to SYSTEM code, and it leaves the clipboard,
files, and audio untouched because they keep running as the user. The cost is a second capture path and a
handoff that the viewer can notice as a short pause, which is acceptable for a prompt the user answers in seconds.
Option A only pays off with unattended access, which is out of scope. If unattended access comes later, revisit A
then, with a privilege-separated network process.

## Design

### Processes

```text
viewer ══QUIC+SPAKE2══► dari.exe (user, medium integrity)
                          │  network, approval, Windows.Graphics.Capture, encoder, SendInput on Default
                          │
                          ├── \\.\pipe\dari-service ──► dari-service.exe service (LocalSystem, session 0)
                          │                               starts the helper, calls SendSAS
                          │
                          └── \\.\pipe\dari-helper-<random> ◄── dari-service.exe helper (SYSTEM, user's session)
                                 desktop changes, frames (shared memory), input       follows the input desktop,
                                                                                       DXGI capture, SendInput on Winlogon
```

`dari-service.exe` is a new binary from a new Windows-only crate, `crates/winsvc` (`dari-winsvc`). It depends on
`dari-proto` for framing and input types and on `dari-input` for enigo's key mapping. It doesn't depend on
`dari-media`, `dari-net`, or `dari-session`, and it never links `ws2_32.dll`. A CI step checks its import table for
that, so the rule holds without anyone remembering it.

### Session lifecycle

1. A viewer connects, authenticates, and the host user approves, exactly as now. Nothing below happens before
   approval.
2. The app creates a pipe with a random name, `\\.\pipe\dari-helper-<random>`. It then connects to
   `\\.\pipe\dari-service` and sends `StartHelper { pipe, input }`. `input` is false for a view-only session.
3. The service checks the client (see the security model) and keeps an open handle to the client process. It finds
   `winlogon.exe` in the client's session, duplicates its token, strips it (see least privilege), and starts
   `dari-service.exe helper` in that session with `CreateProcessAsUser`. It hands the helper the pipe name, `input`,
   and a duplicate of its handle to the client process, and puts the helper in a job object that kills it when the
   service stops.
4. The helper connects to the app's pipe. Each side checks the other (see the security model) before any message
   passes.
5. The helper reports every input desktop change. While `Default` is the input desktop, the app captures and injects
   as it does today. While `Winlogon` is, the app's capture stream reads frames from the helper, and its input goes
   to the helper.
6. When the session ends, the app closes both pipes. The helper releases every key and button it holds and exits
   when its pipe closes, so no SYSTEM process stays in the user's session between sessions.

The connection to the helper belongs to the host session, not to the capturer. `host_session.rs` stops and
respawns the capture thread on `SelectDisplay`, `SetQuality`, and `SetFrameRate`, and `open_capturer` runs inside
the new thread, so a capturer can't own anything that must outlive it. On Windows the session opens one
`SecureDesktopLink` after approval. The capturer and the input backend each borrow it, and the session forwards
`SelectDisplay` to the helper itself. A capture restart doesn't restart the helper.

If the helper dies, the link reports it, the app falls back to PR 36's notice for the rest of the session, and the
app doesn't start a new helper until the next session.

If the service isn't installed or refuses, the app behaves as PR 36 does: the viewer gets
`Availability::SecureDesktop` and a notice.

### Following the input desktop

The helper keeps one state value, the input desktop, as `Default`, `Winlogon`, or another name, and runs two
threads that each attach to it: capture and input. Neither thread creates a window or installs a hook, because
`SetThreadDesktop` fails on a thread that has either.

- Every 100 ms, and right after `AcquireNextFrame` fails with `DXGI_ERROR_ACCESS_LOST` or
  `DXGI_ERROR_INVALID_CALL`, the helper calls `OpenInputDesktop` and reads the desktop's name with
  `GetUserObjectInformationW(UOI_NAME)`, the same check PR 36 uses.
- When the name changes, the helper sends `DesktopChanged(kind)` to the app. Each thread then calls
  `SetThreadDesktop` on the new desktop before its next capture or injection, the same idea as RustDesk's
  `try_change_desktop`.
- Held keys need care on both sides. Before the app routes input to the new target, it sends releases for every
  held key and button through the old route. The helper also tracks what it holds itself, and releases it before a
  desktop switch, when its pipe closes, and when it exits. `InputSession::release_all` in the app reaches only
  `Default`, so without the helper's own tracking, an Alt held for Alt+Y would stay down on the lock screen if the
  viewer dropped mid-prompt.

### Capturing the secure desktop with DXGI Desktop Duplication

On a switch to `Winlogon`, the capture thread re-attaches, then finds the output whose `DXGI_OUTPUT_DESC.Monitor`
matches the viewer's display ID (the same low-32-bit `HMONITOR` that xcap and `find_monitor` use). It enumerates
every adapter for that, because `DuplicateOutput` must be called with a Direct3D 11 device created on the adapter
that owns the output, and a laptop with two GPUs can split outputs between them. A display ID that matches no output
reports the screen unavailable. The thread then calls `IDXGIOutput1::DuplicateOutput` and waits on
`AcquireNextFrame`:

| Result | What the helper does |
| --- | --- |
| A frame | Copies it to a staging texture, maps it, and publishes BGRA to the app |
| `DXGI_ERROR_WAIT_TIMEOUT` | Nothing. The screen is still, and like Windows.Graphics.Capture the source paces itself |
| `DXGI_ERROR_ACCESS_LOST` or `DXGI_ERROR_INVALID_CALL` from `AcquireNextFrame` | The desktop switched or the mode changed. In Phase 0, `AcquireNextFrame` returned `ACCESS_LOST` on the switch to `Winlogon` and `INVALID_CALL` on the switch back. Releases the duplication, re-checks the input desktop, re-attaches, and duplicates again |
| `E_ACCESSDENIED` from `DuplicateOutput` | Transient during a switch. Retries after the next desktop check, then reports the screen unavailable after 5 seconds |
| `DXGI_ERROR_UNSUPPORTED` or `DXGI_ERROR_SESSION_DISCONNECTED` | Reports the screen unavailable until the next desktop change |

In Phase 0 a new duplication succeeded 40 to 380 ms after either switch. The first frame from `Winlogon` arrived about
20 ms after the switch and showed only the dimmed desktop. The UAC prompt appeared in later frames. The helper
publishes every frame, so the viewer sees the prompt as soon as Windows draws it. A test that saves a `Winlogon`
frame must wait for one that shows the prompt.

Desktop Duplication doesn't draw the pointer. In Phase 0 the frame had no cursor while the frame info reported the
pointer visible. The helper reads `PointerPosition` and the shape from `GetFramePointerShape` and blends the pointer
into the copy, since a UAC prompt is answered with clicks.

Frames go to the app through an unnamed shared memory section that the helper creates. The helper duplicates a handle
with `FILE_MAP_READ` access only into the app's process, using the client process handle it got from the service, and
sends the duplicated value in `FrameSection`. Only the app can map it, and only for reading. The section holds two
frame buffers and a header. The helper writes the next frame into the buffer the app isn't reading, then publishes
that buffer's index with a new sequence number and signals an event, so the app never reads a half-written frame. When
the output's size changes, the helper creates a new section, sends a new `FrameSection`, and closes the old one only
after the app replies `SectionReleased`. On the app side, a new `SecureDesktopCapturer` implements `ScreenCapturer`,
reads the newest frame as `CapturedFrame::Rgba`, and returns `paces_itself() == true`. The app's existing scaler and
encoder handle the rest. A 4K frame is 33 MB, but the secure desktop changes only when the user acts on it, so the
copy cost doesn't matter.

The app's Windows capturer becomes a two-source capturer: Windows.Graphics.Capture while the helper reports
`Default`, and `SecureDesktopCapturer` while it reports `Winlogon`. A switch changes the frame source but keeps the
encoder, so the stream needs no restart. The encoder already starts a keyframe on a resolution change, and the app
requests one at each switch. `HostStatus.screen` stays `Available` throughout.

### Injecting input

On Windows, `HostPlatform::open_input` returns a routing backend. While the input desktop is `Default` it calls
`EnigoBackend` in-process, as now. While it is `Winlogon` it sends each event to the helper as `Input(InputEvent)`,
with the pointer already converted to physical virtual-desktop pixels. The helper validates each event with the
existing `Validate` rules and replays it with its own `EnigoBackend` and `SetCursorPos`. The helper process enables
Per-Monitor V2 DPI awareness, as the app does, so the coordinates mean the same thing in both processes.

The helper drops every input event if it was started with `input: false`. That check is for app bugs. The security
model explains why it doesn't stop malware.

### Ctrl+Alt+Del

The protocol gains one control message, `SecureAttention`, from viewer to host (protocol 2.2, after PR 36's 2.1). A
second new message, `SecureAttentionStatus(Availability)` from host to viewer, says whether the host can raise it:
`Available` only when the service runs and the session allows control. It is a message of its own, not a new
`HostStatus` field, because postcard encodes fields by position: PR 36's `HostStatus::for_version` can swap an enum
variant for an older peer but can't drop a field, and a 2.1 viewer would fail to decode a longer `HostStatus`. The
host sends it only to a 2.2 viewer. The viewer shows **Send Ctrl+Alt+Del** in the toolbar menu only then. The user
can't type the keys on the viewer: a Mac keyboard has no Del key in that position, and a Windows viewer's own
Ctrl+Alt+Del never reaches the app.

On `SecureAttention`, the host checks that the session allows control, then sends `SendSas` to the service. The
service calls `SendSAS(FALSE)`. A call from a service does nothing unless the `SoftwareSASGeneration` value under
`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System` allows services (1 or 3). When the value is
absent, the service does what RustDesk does: it sets the value to 1 for the call and deletes it afterwards. For
that moment, any service can raise the sequence, including ones that run as LocalService or NetworkService. Raising
it only shows the Ctrl+Alt+Del screen, so the exposure is small. When the value exists and doesn't allow services,
an administrator or a Group Policy chose that. The service then refuses, never writes the value, and reports
`SecureAttentionStatus(Unavailable)`.

Phase 0 called `SendSAS(FALSE)` from a LocalSystem service in three states. With the value absent, the call
returned and nothing happened. With the value set to 1, and with the value set only around the call, the
Ctrl+Alt+Del screen appeared. In that last run a script wrote and deleted the value, not the service. Phase 0
didn't try values 0, 2, or 3.

### IPC

Both pipes carry `dari-proto`'s length-bounded postcard framing with a 64 KiB limit, and every message passes
`Validate`. Frames never travel on a pipe; they go through the shared section.

| Pipe and server | From app | To app |
| --- | --- | --- |
| `dari-service`, created by the service | `StartHelper { pipe, input }`, `SendSas` | `HelperStarted`, `Refused(reason)` |
| `dari-helper-<random>`, created by the app | `Input(InputEvent)`, `SelectDisplay(id)`, `RequestFrame`, `SectionReleased` | `DesktopChanged(kind)`, `FrameSection(handle, width, height)`, `ScreenUnavailable` |

Both servers create their pipes with `PIPE_REJECT_REMOTE_CLIENTS` and `FILE_FLAG_FIRST_PIPE_INSTANCE`. The flag
doesn't stop another process from creating the name first. It makes the server's own creation fail loudly when that
happened. The helper pipe's random name, which nothing logs, is what keeps other processes from guessing it.

### Installer

`installer-mode` changes from `currentUser` to `perMachine`, so Dari installs under `C:\Program Files\Dari`. A
SYSTEM service must run a binary that the user can't replace. Under `%LOCALAPPDATA%`, any process of the user could
overwrite `dari-service.exe` and get SYSTEM at the next start, which is why a per-user install can't have the
service at all.

- `binaries` in `[package.metadata.packager]` gains `dari-service`. cargo-packager reads `installer-mode` as an alias
  of `install_mode`, so only the value changes.
- cargo-packager 0.11 has no post-install or pre-uninstall hook. Its one hook, `preinstall-section`, runs before
  files are copied, so it stops the service before an upgrade replaces the binary. Registering the service needs the
  `template` option: a copy of cargo-packager's `installer.nsi` in `crates/app/assets/`, with an `ExecWait` of
  `dari-service.exe install` at the end of `Section Install` and one of `dari-service.exe uninstall` at the start of
  `Section Uninstall`. A test diffs the copy against the pinned cargo-packager version's template, so a packager
  upgrade can't silently drop either line.
- `dari-service.exe install` creates or updates the service (LocalSystem, automatic start, restart on failure, a
  quoted image path) and starts it. Running it twice leaves the same state, so an upgrade or a repair runs the same
  command.
- The per-machine installer looks for the per-user install's uninstall key under `HKCU` and deletes the files,
  shortcuts, and keys that the per-user installer created, at that installer's fixed locations. It doesn't run the
  old uninstaller or read a path from the registry: any process of the user can replace both, and the installer
  runs as an administrator. It keeps the user's data in `%LOCALAPPDATA%\dari`, which the app still uses because it
  still runs as the user.
- Installing now shows a UAC prompt, and a user without administrator rights can't install Dari. That is the price
  of the service.
- `platform.yml`'s installer smoke test already looks for `dari.exe` under `%ProgramFiles%`. The test must also
  check that `DariService` runs and that the install folder's ACL denies users write access.

### Protocol and UI changes

- Protocol 2.2: `SecureAttention` and `SecureAttentionStatus`. A 2.1 peer never receives either, following the
  rule that a new kind is only ever sent to a peer whose version defines it.
- With the service running, the viewer sees the secure desktop instead of PR 36's notice. PR 36's notice remains
  for hosts without the service: per-user installs that haven't upgraded, or a refused `StartHelper`.
- The host's settings show whether secure-screen control is on (see the policy below), so the host user knows
  whether a viewer can answer UAC prompts.

## Security model

The goal in [security.md](../security.md) stays the same: a peer without the one-time password can neither see the
screen nor send input. This design adds two SYSTEM processes, so it must also answer who can reach them.

### What is authenticated where

| Boundary | Who is on the other side | Check |
| --- | --- | --- |
| Network to `dari.exe` | A remote peer | Unchanged: QUIC with TLS 1.3, SPAKE2 with the one-time password, throttling, approval, view-only |
| `dari.exe` to `\\.\pipe\dari-service` | Any local process | The pipe's DACL admits SYSTEM and interactive users only. The service reads the client's session with `GetNamedPipeClientSessionId` and its process with `GetNamedPipeClientProcessId`, and accepts only a process whose image is `C:\Program Files\Dari\dari.exe` in an active session. It holds the client's process handle from then on, so the vetted process can't exit and be replaced under the same ID. It starts at most one helper per session and rate-limits refusals |
| The helper's pipe, seen from `dari.exe` | The client that connects | The app created the pipe, so no other process can be its server. The helper connects with `SECURITY_SQOS_PRESENT \| SECURITY_IDENTIFICATION`. The app calls `ImpersonateNamedPipeClient`, which needs no privilege at identification level, and accepts the client only if its token's user is LocalSystem (`S-1-5-18`). This is the check that keeps a viewer's lock-screen password from reaching any process that isn't SYSTEM. Any SYSTEM process passes it, and a SYSTEM process can read keystrokes anyway |
| The helper's pipe, seen from the helper | The pipe's server | The helper compares `GetNamedPipeServerProcessId` with the process ID of the client handle the service passed it. The service and the helper hold that handle open, so the ID can't be reused by another process in the meantime |
| A squatter on `\\.\pipe\dari-service` | Any local process, while the service is stopped | It can only refuse or ignore `StartHelper`. No real helper connects to the app's pipe, so the app sees no SYSTEM client and falls back to PR 36's notice. The service fails to start loudly because of `FILE_FLAG_FIRST_PIPE_INSTANCE`. The worst case is denial of service |
| `SecureAttention` | A remote peer | Accepted only in a session that allows control. The service also refuses `SendSas` from a client without a live helper |

### Least privilege

- The service and the helper have no network code, and the import-table check in CI keeps it that way.
- The helper needs the SYSTEM SID to open `Winlogon` and duplicate it. It doesn't need
  SYSTEM's privileges. The service builds its token with `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE)` from the
  duplicated `winlogon.exe` token, which removes every privilege except `SeChangeNotifyPrivilege`. Phase 0 showed that
  this token is enough, as x64 under emulation and as native Arm64. With only that privilege, the helper attached to
  `Winlogon`, duplicated both of the VM's outputs, answered a UAC prompt with Alt+Y, and unlocked the lock screen.
  The restricted token still has the SYSTEM SID and System integrity. The helper keeps no other privilege.
- The helper creates no windows and runs no message loop, so other processes can't send it window messages.
  `dari-service.exe` is built with `windows_subsystem = "windows"`. In Phase 0 a console build of the helper,
  started with `CreateProcessAsUser`, opened a console window owned by SYSTEM on the user's desktop. The helper
  calls `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32)` before loading anything.
- The helper lives only while a session is live, and the job object ends it with the service.
- The service needs LocalSystem to open `winlogon.exe`'s token and to start a process with it, which needs
  `SeAssignPrimaryTokenPrivilege` and `SeIncreaseQuotaPrivilege`. The token already carries the user's session, so
  the service doesn't need to change it with `SeTcbPrivilege`. It does nothing else.

### Approval and view-only still hold

- The app starts the helper only after the host user approves, so a pending or declined viewer never sees the
  secure desktop.
- In a view-only session the app never creates its input thread, so it never routes input to the helper. The helper
  starts with `input: false` and drops input anyway. The host reports `SecureAttentionStatus(NotAllowed)` and
  refuses `SecureAttention`.
- A view-only viewer does see the secure desktop. It already sees everything on `Default`, and a prompt is just as
  visible to someone standing at the host.

### What this design can't stop

Code running as the signed-in user can do what `dari.exe` can do. It can inject into `dari.exe`, which runs with
the same token, and then talk to the service and the helper as if it were the app. The image-path check stops
other programs, not code running inside Dari. So once Dari is installed per machine, malware running as an
administrator in UAC's default consent mode can click **Yes** on its own consent prompts.

This is the main cost of the feature, and it was accepted with the default below. Three facts bound it:

- Microsoft doesn't treat UAC consent as a security boundary, and the default consent mode already has documented
  auto-elevation bypasses. Dari adds one more, not the first.
- Credential prompts, which standard users see, still need an administrator's password. The lock screen still needs
  the user's password or PIN. The helper lets code type them, not know them.
- A remote peer gains nothing. It still has to pass the password and approval to reach `dari.exe`.

Two controls limit it further:

- An `HKLM` policy value, `SecureDesktopControl`, that only an administrator can set. At 0 the service starts helpers
  with input off and refuses `SendSas`, so viewers can still see the secure desktop but not answer it. The installer
  offers it as a checkbox, **Let viewers answer UAC prompts and the lock screen**. Later, the host's settings change
  it through `dari-service.exe policy on` or `policy off`, started with `runas`, so every change shows a UAC prompt.
  While the value is 0, the helper sends no input, so malware can't answer the prompt that would turn it on.
- The service writes an Application event log entry each time it starts a helper with input, and each `SendSAS`.
  These entries record that something used the feature. They can't prove that a remote viewer did.

## Testing

### Unit and integration tests

- The helper's desktop-following state machine runs against a fake desktop source, including a switch that happens
  between a check and `SetThreadDesktop`, and a duplication that reports `DXGI_ERROR_ACCESS_LOST` mid-frame.
- Pipe messages round-trip and fail `Validate` on oversized or malformed input, as the protocol tests do now.
- The two-source capturer, on the synthetic platform, keeps one encoder and one reference chain across
  `Default` → `Winlogon` → `Default`, and the viewer receives a keyframe at each switch.
- The routing input backend releases held keys on the old target when the desktop changes, and the helper releases
  what it holds when its pipe closes.
- Peer checks: the service refuses a client with another image path. The app refuses a pipe client whose token isn't
  LocalSystem. The helper refuses a pipe server whose process ID differs from its client handle's.
- `SecureAttentionStatus` reaches only 2.2 viewers, and a 2.1 viewer still decodes every `HostStatus` from a 2.2
  host.

### End to end in the local Windows 11 VM

The VM from [development.md](../development.md#local-windows-11-vm) turns UAC off: `autounattend.xml` sets
`EnableLUA` to false, and `interactive.ps1` starts programs with `-RunLevel Highest`. With UAC off there is no
consent prompt to answer, and an elevated host would hide integrity problems. The tests need both changed.

1. A new script, `scripts/crosscheck/vm/enable-uac.sh`, sets `EnableLUA=1`, `ConsentPromptBehaviorAdmin=5`, and
   `PromptOnSecureDesktop=1` under `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System`, reboots the VM,
   and waits for it like `wait-vm.sh`. It does nothing when the values are already set. A `--off` flag restores the
   current state, so the existing cases keep their setup.
2. `interactive.ps1 start` gains `-RunLevel Limited`, so the host runs at medium integrity like the installed app.
3. The secure-desktop cases install Dari per machine from the build (`--release TAG`, or a local installer), so the
   service is real.
4. New `dari-check` cases, each judged on the host by what the helper and Windows report and on the viewer by
   saved frames:

| Case | Host action | Viewer action | Passes when |
| --- | --- | --- | --- |
| `uac-allow` | Starts `cmd.exe /c whoami /groups > result.txt` with `Start-Process -Verb RunAs` from a medium-integrity task | Waits for a frame from `Winlogon`, saves it, sends Alt+Y | `result.txt` lists `High Mandatory Level`, and the helper reported `Winlogon` then `Default` |
| `uac-deny` | Same | Sends Esc | No `result.txt`, and the desktop returns to `Default` |
| `lock-unlock` | `rundll32 user32.dll,LockWorkStation` | Waits for the lock screen, types the VM user's password from `~/.dari-check-vm`, presses Enter | The desktop returns to `Default` and the host's interactive task still runs |
| `cad` | None | Sends `SecureAttention`, waits for a `Winlogon` frame, sends Esc | The helper reported `Winlogon` then `Default` |
| `secure-view-only` | Same as `uac-allow` | Same, in a view-only session | No `result.txt`, the prompt stays up until the host check dismisses it, and `SecureAttention` is refused |
| `secure-policy-off` | Sets `SecureDesktopControl=0`, then as `uac-allow` | Same as `uac-allow`, in a session that allows control | No `result.txt`, the viewer still received a `Winlogon` frame, and `SecureAttention` is refused |
| `drop-mid-prompt` | Same as `uac-allow` | Holds Alt, then disconnects | The helper's log shows it released Alt on pipe close, and the host check can still answer the prompt with Esc |
| `helper-killed` | Same as `uac-allow`, then ends the helper process | Waits | The viewer gets PR 36's notice, and no key stays down |
| `secure-second-display` | Same as `uac-allow`, on the VM with two displays | Selects the second display while the prompt is up, then the first | Frames from both displays arrive, and Alt+Y still answers the prompt |

`lock-unlock` types the VM's password, so the case never saves its frames after the password field gets focus, and
the viewer's log redacts `Text` and key events in that case.

Two installer cases run outside `dari-check`. An upgrade from a 0.0.x per-user install checks the migration. Stopping
the service, starting a process that creates `\\.\pipe\dari-service`, and starting the service again checks that
the service fails loudly and the app falls back to PR 36's notice.

The VM runs Windows 11 on Arm with x64 Dari under emulation. Its two displays come from the
`Red Hat VirtIO GPU DOD controller` and from the IddCx `Virtual Display Driver` that `add-second-display.sh`
installs, which keeps the VM in test-signing mode. DXGI reports each one's adapter as `Microsoft Basic Render
Driver`. Phase 0 duplicated both outputs on `Default` and on `Winlogon`, as x64 and as native Arm64, so the
end-to-end cases run in the VM.

## Phases

Each phase ends in something a reviewer can run. Estimates assume one engineer who knows the codebase.

| Phase | Work | Done when | Estimate |
| --- | --- | --- | --- |
| 0. Spike | A throwaway service and helper in the VM with UAC on: winlogon token, restricted token, `SetThreadDesktop`, DXGI capture of a UAC prompt, a click on **Yes**, `SendSAS` | Screenshots of the prompt from the helper, an elevated `whoami`, and a written answer for each unknown: restricted token, DXGI on the VM's adapter, `SoftwareSASGeneration` | 3 days |
| 1. Per-machine install | `perMachine` installer, the empty service, `install` and `uninstall`, migration from the per-user install, smoke test | Upgrading a per-user 0.0.x install in the VM leaves one per-machine install, a running service, and the old settings and identity | 1 week |
| 2. See the secure desktop | `dari-winsvc` helper, both pipes and their checks, desktop following, DXGI capture, shared frames, the two-source capturer | The viewer sees a UAC prompt and the lock screen, and the session survives both switches. `uac-deny` passes with the host answering by hand | 2 weeks |
| 3. Answer it | The routing input backend, held-key release, `input: false`, the `SecureDesktopControl` policy, event log entries | `uac-allow`, `uac-deny`, `lock-unlock`, and `secure-view-only` pass | 1 week |
| 4. Ctrl+Alt+Del | Protocol 2.2, the viewer menu item, `SendSas` and its `SoftwareSASGeneration` handling | `cad` passes, and a 2.1 peer still connects | 3 days |
| 5. Documents | security.md threat model, user guide, README known limitations, architecture.md | The docs describe the shipped behavior | 2 days |

Phases 1 to 4 each ship as their own PR. Phase 1 can ship alone, since a per-machine install is harmless without the
helper. Phase 2 is useful alone too: seeing the prompt tells the user what to ask the person at the host.

Issue #41 tracks the work as seven slices. Phase 2 is split in two: #45 starts the helper and its pipes, and #46
shows the secure desktop. The VM setup from "End to end in the local Windows 11 VM" comes first as #42. Phase 5 has
no slice of its own, because each slice updates the documents it affects, as `docs/development.md` requires.

## Risks

| Risk | Effect | Mitigation |
| --- | --- | --- |
| Same-user malware uses the helper to approve its own consent prompts | A local elevation for malware already running as an administrator | The `SecureDesktopControl` policy and checkbox, event log entries, and a plain statement in security.md. The default is on, as decided below |
| A bug in the helper or service code | A local process gets SYSTEM | No network code, a fixed message set behind `Validate`, the restricted token, the helper alive only during sessions, and the Win32 `unsafe` kept in one module that review can read whole |
| DXGI Desktop Duplication fails on some adapters (hybrid laptop GPUs, some VMs, remote display drivers) | The viewer sees PR 36's notice instead of the prompt | The helper reports the screen unavailable, and the app falls back to PR 36's behavior. Phase 0 showed that the VM's VirtIO and IddCx displays work |
| Antivirus flags an unsigned service that starts SYSTEM processes with a `winlogon.exe` token | Quarantined installs, or a refused helper | Signing the installer and binaries, a separate decision. Until then, the user guide says what to expect |
| The per-machine migration breaks an existing install | A user loses the app or its identity | The migration keeps `%LOCALAPPDATA%\dari`, and Phase 1's VM test upgrades a real 0.0.x install |
| Installing now needs administrator rights | Users on managed PCs can't install | Accepted. Those users can't install a service anyway |
| A Windows update changes how the secure desktop behaves | Capture or input stops working on the secure desktop | The VM cases catch it, and the fallback is PR 36's notice, not a broken session |

## Decisions

- Option B is approved: the host stays a per-user process, and a SYSTEM service and helper handle only the secure
  desktop.
- The installer is per-machine only. A user without administrator rights can't install Dari, and no per-user
  layout needs testing.
- Secure-screen control is on by default (option 1 below). The installer's checkbox is checked, and its page shows
  the explanation. A silent install sets `SecureDesktopControl` to 1.

## Why secure-screen control is on by default

The `SecureDesktopControl` policy decides whether a viewer can answer the secure desktop. Seeing it is always on.
The choice was its value after an install that nobody customized, including a silent install.

| Option | Default | Who gets what | Cost |
| --- | --- | --- | --- |
| 1. On | The installer's checkbox is checked | The reported case works right after install | Every per-machine install has the local elevation path described in "What this design can't stop" |
| 2. Off | The checkbox is unchecked | Safe by default. A user turns it on in the installer or in settings, with a UAC prompt | A user who skipped the checkbox finds out while away from the host, the one moment they can't turn it on. They see the prompt but can't answer it |
| 3. Off for UAC consent prompts only | Lock screen, credential prompts, and Ctrl+Alt+Del on. Consent prompts need opt-in | Closes the risky case by default, because only a consent prompt can be answered without a password | The reported case is a consent prompt, so it fails by default. The helper must tell `consent.exe` apart from `LogonUI.exe` on `Winlogon`, which is fragile |

Option 1 was chosen, with the checkbox's explanation shown on its installer page. Three reasons support it:

- Dari is for one person reaching their own PC, and the person who installs it is the person who will connect.
  Option 2 fails that person at the worst moment. Option 3 fails the exact case this design exists for.
- The extra risk applies only when malware already runs in an administrator's session in UAC's default consent
  mode. Such malware already has documented auto-elevation bypasses, so Dari adds one more, not the first.
- Every way to change the value needs an administrator. An organization can set it to 0 with Group Policy, and a
  user can turn it off in settings.

Revisit this choice if Dari is ever offered to organizations or shared PCs. There, the person who installs it is
not the person who connects, and option 2 fits better.
