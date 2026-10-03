//! The host side: shares this machine's real screen and input, and checks what the viewer's
//! session actually did here — where the OS pointer landed, which keys arrived, and what reached
//! the system clipboard.

use std::net::Ipv6Addr;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use dari_input::{InjectError, InputBackend};
use dari_media::{CaptureError, DisplayInfo, ScreenCapturer, StreamSettings};
use dari_net::DeviceIdentity;
use dari_proto::{Availability, HostStatus, KeyCode, MouseButton, NamedKey, Os};
use dari_session::{
    ApprovalDecision, ClipboardAccess, ClipboardFactory, HostConfig, HostEvent, HostHandle,
    HostPlatform, RelayStatus, SessionEndReason, SystemClipboard, SystemPlatform, start_host,
};
use enigo::{Coordinate, Enigo, Mouse as _, Settings};
use tokio::sync::mpsc;

use crate::scenario::{
    Approval, POINTER_TARGETS, Verdict, expected_landing, host_token, lands_near,
    shortcut_modifier, viewer_token,
};

/// How long the host waits after copying its token before ending the session, so the viewer
/// sees the text arrive before the session goes away.
const END_AFTER_ANSWER: Duration = Duration::from_secs(3);
/// How long an injected pointer move may take to show up in the OS pointer position.
const LANDING_WAIT: Duration = Duration::from_millis(300);

#[derive(Debug, clap::Args)]
pub(crate) struct HostArgs {
    /// UDP port to listen on; 0 picks a free one.
    #[arg(long, default_value_t = 47821)]
    port: u16,
    /// Relay to register with (`host` or `host:port`), so the viewer can connect by ID.
    #[arg(long)]
    relay: Option<String>,
    /// What to answer when the viewer asks to connect.
    #[arg(long, value_enum)]
    approve: Approval,
    /// Shared with the viewer; makes this run's clipboard texts unique.
    #[arg(long)]
    nonce: String,
    /// Give up if no session has started and ended within this many seconds.
    #[arg(long, default_value_t = 180)]
    timeout: u64,
}

/// Everything the session did to this machine.
#[derive(Debug, Default)]
struct Observations {
    /// Where the OS pointer was after each injected pointer move; `None` if unreadable.
    landings: Vec<Option<(i32, i32)>>,
    keys: Vec<(KeyCode, bool)>,
    /// Buttons, wheel, and text: not part of the scenario, so any is unexpected.
    other_input: usize,
    inject_errors: Vec<String>,
    /// Text the session put on the system clipboard, as read back from it.
    clipboard_writes: Vec<Option<String>>,
    /// When the host put its own token on the clipboard in answer to the viewer's.
    answered_at: Option<Instant>,
}

type Shared = Arc<Mutex<Observations>>;

fn observe(shared: &Shared, record: impl FnOnce(&mut Observations)) {
    record(&mut shared.lock().unwrap_or_else(PoisonError::into_inner));
}

/// The real platform, with input and clipboard wrapped so their effects are recorded.
struct ObservedPlatform {
    observations: Shared,
    viewer_token: String,
    host_token: String,
}

impl HostPlatform for ObservedPlatform {
    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        SystemPlatform.displays()
    }

    fn open_capturer(&self, display: u32) -> Result<Box<dyn ScreenCapturer>, CaptureError> {
        SystemPlatform.open_capturer(display)
    }

    fn open_input(&self) -> Result<Box<dyn InputBackend>, InjectError> {
        Ok(Box::new(ObservedInput {
            inner: SystemPlatform.open_input()?,
            pointer: Enigo::new(&Settings::default()).ok(),
            observations: self.observations.clone(),
        }))
    }

    fn clipboard(&self) -> Option<ClipboardFactory> {
        let observations = self.observations.clone();
        let viewer_token = self.viewer_token.clone();
        let host_token = self.host_token.clone();
        Some(Arc::new(move || {
            let inner = SystemClipboard::open()?;
            Some(Box::new(AnsweringClipboard {
                inner,
                observations: observations.clone(),
                viewer_token: viewer_token.clone(),
                host_token: host_token.clone(),
            }) as Box<dyn ClipboardAccess>)
        }))
    }
}

struct ObservedInput {
    inner: Box<dyn InputBackend>,
    /// Reads the OS pointer position back; `None` if it cannot be opened.
    pointer: Option<Enigo>,
    observations: Shared,
}

impl ObservedInput {
    fn pointer_location(&self, aimed: (i32, i32)) -> Option<(i32, i32)> {
        let pointer = self.pointer.as_ref()?;
        let deadline = Instant::now() + LANDING_WAIT;
        loop {
            let location = pointer.location().ok()?;
            if lands_near(location, aimed) || Instant::now() > deadline {
                return Some(location);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn record<T>(&self, result: Result<T, InjectError>) -> Result<T, InjectError> {
        if let Err(error) = &result {
            let error = error.to_string();
            observe(&self.observations, |seen| seen.inject_errors.push(error));
        }
        result
    }
}

impl InputBackend for ObservedInput {
    fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
        let result = self.inner.move_pointer(x, y);
        let landed = self.pointer_location((x, y));
        observe(&self.observations, |seen| {
            seen.landings.push(landed);
        });
        self.record(result)
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
        observe(&self.observations, |seen| seen.other_input += 1);
        let result = self.inner.button(button, pressed);
        self.record(result)
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
        observe(&self.observations, |seen| seen.other_input += 1);
        let result = self.inner.scroll(dx, dy);
        self.record(result)
    }

    fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
        observe(&self.observations, |seen| seen.keys.push((key, pressed)));
        let result = self.inner.key(key, pressed);
        self.record(result)
    }

    fn text(&mut self, text: &str) -> Result<(), InjectError> {
        observe(&self.observations, |seen| seen.other_input += 1);
        let result = self.inner.text(text);
        self.record(result)
    }
}

/// The system clipboard; when the viewer's token arrives it copies the host's token in reply,
/// as a person copying text on the host would.
struct AnsweringClipboard {
    inner: SystemClipboard,
    observations: Shared,
    viewer_token: String,
    host_token: String,
}

impl ClipboardAccess for AnsweringClipboard {
    fn read_text(&mut self) -> Option<String> {
        self.inner.read_text()
    }

    fn write_text(&mut self, text: &str) -> bool {
        let written = self.inner.write_text(text);
        let read_back = self.inner.read_text();
        let is_token = written && read_back.as_deref() == Some(self.viewer_token.as_str());
        observe(&self.observations, |seen| {
            seen.clipboard_writes.push(read_back);
        });
        if is_token {
            let host_token = self.host_token.clone();
            let observations = self.observations.clone();
            // Copy from another clipboard handle, so the session's poll sees a local change.
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(500));
                if SystemClipboard::open()
                    .is_some_and(|mut clipboard| clipboard.write_text(&host_token))
                {
                    observe(&observations, |seen| {
                        seen.answered_at = Some(Instant::now());
                    });
                }
            });
        }
        written
    }
}

/// Session events that matter for the verdict.
#[derive(Debug, Default)]
struct Timeline {
    approvals: usize,
    status: Option<HostStatus>,
    started: bool,
    ended: Option<SessionEndReason>,
}

pub(crate) async fn run(args: HostArgs) -> anyhow::Result<ExitCode> {
    let displays = SystemPlatform
        .displays()
        .context("cannot list this machine's displays")?;
    print_displays(&displays);
    let original_pointer = Enigo::new(&Settings::default())
        .ok()
        .and_then(|pointer| pointer.location().ok());
    let original_clipboard =
        SystemClipboard::open().and_then(|mut clipboard| clipboard.read_text());

    let observations = Shared::default();
    let platform = ObservedPlatform {
        observations: observations.clone(),
        viewer_token: viewer_token(&args.nonce),
        host_token: host_token(&args.nonce),
    };
    let (handle, mut events) = start_host(
        HostConfig {
            bind_address: (Ipv6Addr::UNSPECIFIED, args.port).into(),
            host_name: crate::device_name(),
            stream: StreamSettings::default(),
            require_approval: true,
            clipboard: true,
            relay: args.relay.clone(),
        },
        Arc::new(DeviceIdentity::generate().context("cannot create a device identity")?),
        Arc::new(platform),
    )
    .context("cannot start hosting")?;
    println!("port: {}", handle.local_address().port());

    let timeline = serve(&args, &handle, &mut events, &observations).await;
    drop(handle);

    if let Some((x, y)) = original_pointer
        && let Ok(mut pointer) = Enigo::new(&Settings::default())
    {
        let _restored = pointer.move_mouse(x, y, Coordinate::Abs);
    }
    if let Some(text) = original_clipboard
        && let Some(mut clipboard) = SystemClipboard::open()
    {
        let _restored = clipboard.write_text(&text);
    }

    let observations =
        std::mem::take(&mut *observations.lock().unwrap_or_else(PoisonError::into_inner));
    Ok(judge(&args, &displays, &timeline, &observations).finish())
}

fn print_displays(displays: &[DisplayInfo]) {
    println!("os: {:?}", Os::current());
    for display in displays {
        println!(
            "display {}: {}x{} at ({}, {}), scale {}, {}{}",
            display.id,
            display.width,
            display.height,
            display.x,
            display.y,
            display.scale_factor,
            display.name,
            if display.is_primary { " (primary)" } else { "" }
        );
    }
}

/// Answers the viewer, ends an allowed session after the clipboard round trip, and records
/// the session's course until it ends or the timeout passes.
async fn serve(
    args: &HostArgs,
    handle: &HostHandle,
    events: &mut mpsc::UnboundedReceiver<HostEvent>,
    observations: &Shared,
) -> Timeline {
    let mut timeline = Timeline::default();
    let mut password_shown = false;
    let mut ending = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout);
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => break,
            _ = tick.tick() => {
                // In an allowed session the host ends it once the viewer had time to receive the
                // host's clipboard text, which tests a host-side disconnect.
                let answered = observations
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .answered_at;
                if !ending && answered.is_some_and(|at| at.elapsed() >= END_AFTER_ANSWER) {
                    ending = true;
                    handle.end_session();
                }
            }
            event = events.recv() => match event {
                None => break,
                Some(HostEvent::PasswordChanged(Some(password))) if !password_shown => {
                    password_shown = true;
                    println!("password: {}", password.display_text().as_str());
                }
                Some(HostEvent::PasswordChanged(_)) => {}
                Some(HostEvent::Relay(RelayStatus::Registered(id))) => {
                    println!("relay-id: {}", id.to_string().replace(' ', ""));
                }
                Some(HostEvent::Relay(status)) => println!("relay: {status:?}"),
                Some(HostEvent::ApprovalRequested { peer, request }) => {
                    timeline.approvals += 1;
                    println!("{} ({:?}) asks to connect; answering {:?}", peer.name, peer.os, args.approve);
                    request.respond(match args.approve {
                        Approval::Allow => ApprovalDecision::AllowControl,
                        Approval::ViewOnly => ApprovalDecision::ViewOnly,
                    });
                }
                Some(HostEvent::SessionStarted(peer)) => {
                    timeline.started = true;
                    println!("session started with {} from {}", peer.name, peer.address);
                }
                Some(HostEvent::SessionStatus(status)) => {
                    println!("screen: {:?}, input: {:?}", status.screen, status.input);
                    timeline.status = Some(status);
                }
                Some(HostEvent::SessionEnded { reason, .. }) => {
                    println!("session ended: {reason}");
                    timeline.ended = Some(reason);
                    break;
                }
            }
        }
    }
    timeline
}

fn judge(
    args: &HostArgs,
    displays: &[DisplayInfo],
    timeline: &Timeline,
    seen: &Observations,
) -> Verdict {
    let mut verdict = Verdict::default();
    if !timeline.started {
        verdict.fail(format!("no viewer connected within {}s", args.timeout));
        return verdict;
    }
    verdict.check(
        timeline.approvals == 1,
        format!(
            "the host was asked to approve once (asked {})",
            timeline.approvals
        ),
    );
    for error in &seen.inject_errors {
        verdict.fail(format!("input injection failed: {error}"));
    }
    match args.approve {
        Approval::Allow => judge_allowed(args, displays, timeline, seen, &mut verdict),
        Approval::ViewOnly => judge_view_only(timeline, seen, &mut verdict),
    }
    verdict
}

fn judge_allowed(
    args: &HostArgs,
    displays: &[DisplayInfo],
    timeline: &Timeline,
    seen: &Observations,
    verdict: &mut Verdict,
) {
    verdict.check(
        timeline.status
            == Some(HostStatus {
                screen: Availability::Available,
                input: Availability::Available,
            }),
        format!(
            "screen and input are available (got {:?}); grant Screen Recording and Accessibility \
             on macOS",
            timeline.status
        ),
    );
    for display in displays {
        for target in POINTER_TARGETS {
            let expected = expected_landing(display, target);
            let landed = seen
                .landings
                .iter()
                .filter_map(|landed| *landed)
                .find(|landed| lands_near(*landed, expected));
            let closest = seen
                .landings
                .iter()
                .filter_map(|landed| *landed)
                .min_by_key(|landed| (landed.0 - expected.0).abs() + (landed.1 - expected.1).abs());
            verdict.check(
                landed.is_some(),
                format!(
                    "pointer reaches {target:?} on display {} at {expected:?} (closest landing \
                     {closest:?})",
                    display.id
                ),
            );
        }
    }
    let modifier = KeyCode::Named(shortcut_modifier(Os::current()));
    verdict.check(
        seen.keys.contains(&(modifier, true)) && seen.keys.contains(&(modifier, false)),
        format!(
            "the viewer's shortcut modifier arrives as {modifier:?} (keys {:?})",
            seen.keys
        ),
    );
    let foreign = match Os::current() {
        Os::MacOs => NamedKey::Control,
        _ => NamedKey::Meta,
    };
    verdict.check(
        !seen
            .keys
            .iter()
            .any(|(key, _)| *key == KeyCode::Named(foreign)),
        format!("no untranslated {foreign:?} key arrives"),
    );
    verdict.check(
        seen.other_input == 0,
        format!(
            "no unexpected buttons, wheel, or text ({})",
            seen.other_input
        ),
    );
    let token = viewer_token(&args.nonce);
    verdict.check(
        seen.clipboard_writes
            .iter()
            .any(|text| text.as_deref() == Some(token.as_str())),
        format!(
            "the viewer's clipboard text reaches the system clipboard (writes {:?})",
            seen.clipboard_writes
        ),
    );
    verdict.check(
        seen.answered_at.is_some(),
        "the host copies its own text in reply",
    );
    verdict.check(
        timeline.ended == Some(SessionEndReason::HostEnded),
        format!("the host ends the session (ended: {:?})", timeline.ended),
    );
}

fn judge_view_only(timeline: &Timeline, seen: &Observations, verdict: &mut Verdict) {
    verdict.check(
        timeline.status
            == Some(HostStatus {
                screen: Availability::Available,
                input: Availability::NotAllowed,
            }),
        format!(
            "the screen is shared and input is not allowed (got {:?})",
            timeline.status
        ),
    );
    verdict.check(
        seen.landings.is_empty() && seen.keys.is_empty() && seen.other_input == 0,
        format!(
            "no input is injected ({} pointer moves, {} keys, {} other)",
            seen.landings.len(),
            seen.keys.len(),
            seen.other_input
        ),
    );
    verdict.check(
        seen.clipboard_writes.is_empty(),
        format!(
            "nothing reaches the clipboard (writes {:?})",
            seen.clipboard_writes
        ),
    );
    verdict.check(
        timeline.ended == Some(SessionEndReason::ViewerLeft),
        format!("the viewer ends the session (ended: {:?})", timeline.ended),
    );
}
