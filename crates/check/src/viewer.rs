//! The viewer side: connects to a `dari-check host`, walks every host display, sends the
//! scenario's pointer moves and shortcut, and checks what it can observe locally — the host's
//! status, the decoded frames, and what arrives on this machine's clipboard.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use dari_media::DecodedFrame;
use dari_net::AccessPassword;
use dari_proto::{
    Availability, DeviceId, DisplayDescription, HostStatus, InputEvent, KeyCode, MouseButton,
    NamedKey, Os,
};
use dari_session::{
    ClipboardAccess as _, SessionEndReason, SystemClipboard, ViewerConfig, ViewerEvent,
    ViewerHandle, ViewerTarget, connect_viewer,
};
use tokio::sync::{mpsc, watch};

use crate::probe::{HANGUL_KEYS, TYPED_TEXT, WHEEL_LINES};
use crate::scenario::{
    Approval, POINTER_TARGETS, Verdict, host_token, pointer_position, shortcut_modifier,
    viewer_token,
};

const DEFAULT_PORT: u16 = 47821;
/// Pause between pointer moves, so the host reads each landing before the next move.
const POINTER_PAUSE: Duration = Duration::from_millis(400);
/// Pause between key events, so the host's input method sees each one.
const KEY_PAUSE: Duration = Duration::from_millis(60);
/// How long the viewer waits for a new frame before asking the host for a keyframe.
pub(crate) const KEYFRAME_AFTER: Duration = Duration::from_secs(1);

#[derive(Debug, clap::Args)]
pub(crate) struct ViewArgs {
    /// The host's address (`IP` or `IP:port`), or with `--relay` its relay ID.
    address: String,
    /// Relay to connect through, by the host's relay ID.
    #[arg(long)]
    relay: Option<String>,
    /// File holding the host's access password; read from stdin when omitted.
    #[arg(long)]
    password_file: Option<PathBuf>,
    /// What the host was told to answer; decides which outcome is checked.
    #[arg(long, value_enum)]
    approve: Approval,
    /// Shared with the host; makes this run's clipboard texts unique.
    #[arg(long)]
    nonce: String,
    /// Fail unless the host offers exactly this many displays.
    #[arg(long)]
    expect_displays: Option<usize>,
    /// Where to save one decoded frame per host display.
    #[arg(long, default_value = "target/crosscheck")]
    out: PathBuf,
}

/// The session as the viewer has seen it so far.
pub(crate) struct Session {
    pub(crate) events: mpsc::UnboundedReceiver<ViewerEvent>,
    pub(crate) status: Option<HostStatus>,
    pub(crate) screen_history: Vec<Availability>,
    pub(crate) displays: Vec<DisplayDescription>,
    pub(crate) active: Option<u32>,
    pub(crate) ended: Option<SessionEndReason>,
}

impl Session {
    pub(crate) fn new(events: mpsc::UnboundedReceiver<ViewerEvent>) -> Self {
        Self {
            events,
            status: None,
            screen_history: Vec::new(),
            displays: Vec::new(),
            active: None,
            ended: None,
        }
    }

    /// Handles events until `done` holds or `timeout` passes; returns whether it held.
    pub(crate) async fn wait_until(
        &mut self,
        timeout: Duration,
        done: impl Fn(&Self) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while !done(self) {
            if self.ended.is_some() {
                return false;
            }
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Err(_elapsed) => return false,
                Ok(None) => {
                    self.ended.get_or_insert(SessionEndReason::ConnectionLost(
                        "the session's events stopped".into(),
                    ));
                }
                Ok(Some(event)) => self.apply(event),
            }
        }
        true
    }

    pub(crate) fn apply(&mut self, event: ViewerEvent) {
        match event {
            ViewerEvent::AwaitingApproval => println!("waiting for the host to approve"),
            ViewerEvent::HostStatus(status) => {
                println!(
                    "host screen: {:?}, host input: {:?}",
                    status.screen, status.input
                );
                self.screen_history.push(status.screen);
                self.status = Some(status);
            }
            ViewerEvent::Displays { displays, active } => {
                self.displays = displays;
                self.active = Some(active);
            }
            ViewerEvent::FrameRate(rate) => println!("host streams at up to {rate} fps"),
            ViewerEvent::Ended(reason) => {
                println!("session ended: {reason}");
                self.ended = Some(reason);
            }
            ViewerEvent::Transfer(_) => {}
        }
    }
}

pub(crate) async fn run(args: ViewArgs) -> anyhow::Result<ExitCode> {
    let password = read_password(args.password_file.as_ref()).await?;
    let target = match &args.relay {
        Some(relay) => ViewerTarget::Relay {
            relay: relay.clone(),
            id: DeviceId::parse(&args.address).context("with --relay, give the host's relay ID")?,
        },
        None => ViewerTarget::Direct(parse_address(&args.address)?),
    };
    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("cannot create {}", args.out.display()))?;
    let original_clipboard =
        SystemClipboard::open().and_then(|mut clipboard| clipboard.read_text());

    let (viewer, events) = connect_viewer(
        ViewerConfig {
            target,
            client_name: crate::device_name(),
            map_shortcut_modifier: true,
            clipboard: Some(SystemClipboard::factory()),
            // The host's default; the checks are about correctness, not speed.
            frame_rate: 30,
            downloads: None,
            audio: None,
            play_audio: false,
        },
        &password,
    )
    .await
    .context("cannot connect")?;
    println!(
        "connected to {} ({:?})",
        viewer.peer().name,
        viewer.peer().os
    );
    let mut session = Session::new(events);
    let mut verdict = Verdict::default();
    run_scenario(&args, &viewer, &mut session, &mut verdict).await;

    if session.ended.is_none() {
        // Wait for the host to close the connection, so it reads the Disconnect instead of
        // seeing the connection drop.
        viewer.disconnect();
        session
            .wait_until(Duration::from_secs(5), |session| session.ended.is_some())
            .await;
    }
    drop(viewer);
    if let Some(text) = original_clipboard
        && let Some(mut clipboard) = SystemClipboard::open()
    {
        let _restored = clipboard.write_text(&text);
    }
    Ok(verdict.finish())
}

async fn run_scenario(
    args: &ViewArgs,
    viewer: &ViewerHandle,
    session: &mut Session,
    verdict: &mut Verdict,
) {
    let input = match args.approve {
        Approval::Allow => Availability::Available,
        Approval::ViewOnly => Availability::NotAllowed,
    };
    let reported = session
        .wait_until(Duration::from_secs(40), |session| session.status.is_some())
        .await;
    verdict.check(
        reported
            && session.status.map(|status| (status.screen, status.input))
                == Some((Availability::Available, input)),
        format!(
            "the host reports screen Available, input {input:?} (got {:?})",
            session.status
        ),
    );
    if !session
        .wait_until(Duration::from_secs(10), |session| session.active.is_some())
        .await
    {
        verdict.fail("the host lists its displays");
        return;
    }
    let displays = session.displays.clone();
    println!(
        "host displays: {}",
        displays
            .iter()
            .map(|display| format!("{} {}x{}", display.id, display.width, display.height))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if let Some(expected) = args.expect_displays {
        verdict.check(
            displays.len() == expected,
            format!(
                "the host offers {expected} displays (got {})",
                displays.len()
            ),
        );
    }

    let mut frames = viewer.frames();
    for display in &displays {
        if session.active != Some(display.id) {
            viewer.select_display(display.id);
            let switched = session
                .wait_until(Duration::from_secs(10), |session| {
                    session.active == Some(display.id)
                })
                .await;
            verdict.check(switched, format!("switching to display {}", display.id));
            if !switched {
                continue;
            }
        }
        check_frame(args, viewer, display, &mut frames, verdict).await;
        for target in POINTER_TARGETS {
            viewer.send_input(InputEvent::PointerMove(pointer_position(target)));
            tokio::time::sleep(POINTER_PAUSE).await;
        }
    }

    use_input_window(viewer, session, verdict).await;
    clipboard_round_trip(args, session, verdict).await;
}

/// Clicks, scrolls, types, and copies in the host's input window, which sits at the centre of the
/// host's primary display; the host checks what reached it. A view-only host must ignore it all.
async fn use_input_window(viewer: &ViewerHandle, session: &mut Session, verdict: &mut Verdict) {
    let primary = session
        .displays
        .iter()
        .find(|display| display.primary)
        .or_else(|| session.displays.first())
        .map(|display| display.id);
    let Some(primary) = primary else { return };
    if session.active != Some(primary) {
        viewer.select_display(primary);
        let switched = session
            .wait_until(Duration::from_secs(10), |session| {
                session.active == Some(primary)
            })
            .await;
        verdict.check(switched, "switching back to the primary display");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    viewer.send_input(InputEvent::PointerMove(pointer_position((0.5, 0.5))));
    tokio::time::sleep(POINTER_PAUSE).await;
    for pressed in [true, false] {
        viewer.send_input(InputEvent::PointerButton {
            button: MouseButton::Left,
            pressed,
        });
        tokio::time::sleep(KEY_PAUSE).await;
    }
    // Let the click bring the window to the front before keys arrive.
    tokio::time::sleep(Duration::from_millis(500)).await;
    viewer.send_input(InputEvent::Scroll {
        dx: 0,
        dy: WHEEL_LINES,
    });
    tokio::time::sleep(POINTER_PAUSE).await;

    type_keys(viewer, TYPED_TEXT).await;
    // Only Windows has a Hangul key to switch its Korean input method with.
    if viewer.peer().os == Os::Windows {
        tap(viewer, KeyCode::Named(NamedKey::HangulMode)).await;
        type_keys(viewer, HANGUL_KEYS).await;
        tap(viewer, KeyCode::Named(NamedKey::Space)).await;
        tap(viewer, KeyCode::Named(NamedKey::HangulMode)).await;
    }

    // This machine's copy shortcut; the session translates the modifier to the host's.
    let modifier = KeyCode::Named(shortcut_modifier(Os::current()));
    press(viewer, modifier, true).await;
    tap(viewer, KeyCode::Character('c')).await;
    press(viewer, modifier, false).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
}

async fn type_keys(viewer: &ViewerHandle, keys: &str) {
    for character in keys.chars() {
        tap(viewer, KeyCode::Character(character)).await;
    }
}

async fn tap(viewer: &ViewerHandle, key: KeyCode) {
    press(viewer, key, true).await;
    press(viewer, key, false).await;
}

/// Sends one key event and gives the host's input method time to handle it.
async fn press(viewer: &ViewerHandle, key: KeyCode, pressed: bool) {
    viewer.send_input(InputEvent::Key { key, pressed });
    tokio::time::sleep(KEY_PAUSE).await;
}

/// Copies text here; with control allowed the host answers with its own once it arrives
/// there and then ends the session.
async fn clipboard_round_trip(args: &ViewArgs, session: &mut Session, verdict: &mut Verdict) {
    // Clipboard sync only sends changes made after it started, when the host status arrived.
    let sent = viewer_token(&args.nonce);
    let answer = host_token(&args.nonce);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let copied = SystemClipboard::open().is_some_and(|mut clipboard| clipboard.write_text(&sent));
    verdict.check(copied, "copying text on the viewer");
    match args.approve {
        Approval::Allow => {
            let received = wait_for_clipboard(&answer, Duration::from_secs(20)).await;
            verdict.check(
                received,
                "the host's clipboard text reaches this machine's clipboard",
            );
            let ended = session
                .wait_until(Duration::from_secs(20), |session| session.ended.is_some())
                .await;
            verdict.check(
                ended && session.ended == Some(SessionEndReason::HostEnded),
                format!("the host ends the session (ended: {:?})", session.ended),
            );
        }
        Approval::ViewOnly => {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let current = SystemClipboard::open().and_then(|mut clipboard| clipboard.read_text());
            verdict.check(
                current.as_deref() == Some(sent.as_str()),
                format!("nothing from the host replaces the clipboard (holds {current:?})"),
            );
            verdict.check(
                session.ended.is_none(),
                format!("the session is still running (ended: {:?})", session.ended),
            );
        }
    }
}

/// Checks the next frames of the active display: shaped like it, and not blank.
async fn check_frame(
    args: &ViewArgs,
    viewer: &ViewerHandle,
    display: &DisplayDescription,
    frames: &mut watch::Receiver<Option<Arc<DecodedFrame>>>,
    verdict: &mut Verdict,
) {
    let aspect = f64::from(display.width) / f64::from(display.height);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    // The first frames after a switch may still show the previous display.
    let mut seen = 0;
    let frame = loop {
        let wake = deadline.min(tokio::time::Instant::now() + KEYFRAME_AFTER);
        match tokio::time::timeout_at(wake, frames.changed()).await {
            Ok(Ok(())) => {}
            // Hosts send frames only when their screen changes; a still one is sent again on
            // request.
            Err(_) if wake < deadline => {
                viewer.request_keyframe();
                continue;
            }
            Ok(Err(_)) | Err(_) => break None,
        }
        let Some(frame) = frames.borrow_and_update().clone() else {
            continue;
        };
        seen += 1;
        let frame_aspect = f64::from(frame.width) / f64::from(frame.height);
        if seen >= 3 && (frame_aspect - aspect).abs() < 0.02 {
            break Some(frame);
        }
    };
    let Some(frame) = frame else {
        verdict.fail(format!(
            "frames of display {} ({}x{}) arrive within 20s",
            display.id, display.width, display.height
        ));
        return;
    };
    let (mean, spread) = luminance(&frame);
    let path = args.out.join(format!("frame-display-{}.png", display.id));
    if let Err(error) = save_png(&frame, &path) {
        println!("could not save {}: {error}", path.display());
    }
    verdict.check(
        mean > 10.0 || spread > 5.0,
        format!(
            "display {} streams real content: {}x{} frame, luminance mean {mean:.1}, spread \
             {spread:.1}, saved to {}",
            display.id,
            frame.width,
            frame.height,
            path.display()
        ),
    );
}

async fn wait_for_clipboard(expected: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let current = SystemClipboard::open().and_then(|mut clipboard| clipboard.read_text());
        if current.as_deref() == Some(expected) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

pub(crate) async fn read_password(file: Option<&PathBuf>) -> anyhow::Result<AccessPassword> {
    let text = match file {
        Some(file) => std::fs::read_to_string(file)
            .with_context(|| format!("cannot read {}", file.display()))?,
        None => {
            tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).map(|_| line)
            })
            .await??
        }
    };
    Ok(AccessPassword::parse(text.trim())?)
}

pub(crate) fn parse_address(input: &str) -> anyhow::Result<SocketAddr> {
    if let Ok(address) = input.parse::<SocketAddr>() {
        return Ok(address);
    }
    match input
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
    {
        Ok(ip) => Ok(SocketAddr::new(ip, DEFAULT_PORT)),
        Err(_) => bail!("{input} is not an IP address or IP:port"),
    }
}

/// Mean and standard deviation of the frame's luminance; a failed capture is flat black.
#[expect(
    clippy::cast_precision_loss,
    reason = "statistics over a sample of pixels"
)]
fn luminance(frame: &DecodedFrame) -> (f64, f64) {
    let values: Vec<f64> = frame
        .bgra
        .as_chunks::<4>()
        .0
        .iter()
        .step_by(7)
        .map(|pixel| {
            0.114 * f64::from(pixel[0]) + 0.587 * f64::from(pixel[1]) + 0.299 * f64::from(pixel[2])
        })
        .collect();
    let count = values.len().max(1) as f64;
    let mean = values.iter().sum::<f64>() / count;
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / count;
    (mean, variance.sqrt())
}

pub(crate) fn save_png(frame: &DecodedFrame, path: &std::path::Path) -> anyhow::Result<()> {
    let rgba: Vec<u8> = frame
        .bgra
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], 255])
        .collect();
    image::RgbaImage::from_raw(frame.width, frame.height, rgba)
        .context("the frame buffer does not match its size")?
        .save(path)?;
    Ok(())
}
