//! The secure-screen check: views a Windows host while a script shows it secure screens (a UAC
//! prompt, the lock screen), answers each one as told, and checks each one reaches the viewer
//! while the session survives. The viewer gets no signal that the host switched desktops, so the
//! check and the script (`scripts/crosscheck/secure-desktop.sh`) take turns through this check's
//! output: `READY` once the baseline is saved, then for each screen `SEEN LABEL` once its picture
//! has settled, `SENT LABEL` once the screen's keys are sent (a held key stays down), `NOTICE
//! LABEL` when the host reports the secure-desktop notice in a screen that expects it, and `BACK
//! LABEL` once the desktop is back. A screen without keys is dismissed by the script.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use dari_media::DecodedFrame;
use dari_proto::{Availability, DisplayDescription, InputEvent, KeyCode, MouseButton, NamedKey};
use dari_session::{
    SessionEndReason, ViewerConfig, ViewerEvent, ViewerHandle, ViewerTarget, connect_viewer,
};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::picture::{CLEARLY_DIFFERENT, NEAR, Settle, Thumbnail};
use crate::scenario::{Verdict, pointer_position};
use crate::viewer::{
    KEY_PAUSE, KEYFRAME_AFTER, Session, parse_address, press, read_password, save_png, tap,
};

#[derive(Debug, clap::Args)]
pub(crate) struct SecureViewArgs {
    /// The host's address (`IP` or `IP:port`).
    address: String,
    /// File holding the host's access password; read from stdin when omitted.
    #[arg(long)]
    password_file: Option<PathBuf>,
    /// A secure screen the script shows, in order: `LABEL[:EXPECT][=KEYS]`. EXPECT is `notice`
    /// when the script ends the host's secure-desktop helper once the screen is seen, or
    /// `present` when the screen is already up as the viewer connects. KEYS is what the viewer
    /// sends once the screen has settled: `alt-y`, `esc`, `password` (a click, the
    /// --secret-file text, Enter), `hold-alt-leave` (holds Alt and disconnects; last screen
    /// only), or `hold-f20` (holds F20 until the session ends). Without KEYS the script
    /// dismisses the screen.
    #[arg(long = "screen", required = true, value_parser = Screen::parse)]
    screens: Vec<Screen>,
    /// While each screen is up, also select every other display and save its frame.
    #[arg(long)]
    select_display: bool,
    /// Send each screen's keys while another display is selected, then select the first again.
    #[arg(long, requires = "select_display")]
    answer_on_other_display: bool,
    /// Expect a view-only session: the host reports input `NotAllowed`.
    #[arg(long)]
    view_only: bool,
    /// File holding what a `password` screen types: ASCII letters and digits. No frame is saved
    /// once typing starts, and `RUST_LOG` is ignored for the whole run.
    #[arg(long)]
    secret_file: Option<PathBuf>,
    /// After the last screen, keep the session open until this file exists.
    #[arg(long)]
    finish_when: Option<PathBuf>,
    /// Fail unless the host offers exactly this many displays.
    #[arg(long)]
    expect_displays: Option<usize>,
    /// Seconds each step may take: a screen appearing, settling, or going away.
    #[arg(long, default_value_t = 120)]
    timeout: u64,
    /// Milliseconds the picture must stay still to count as settled. A UAC prompt appears a
    /// while after the dimmed desktop does.
    #[arg(long, default_value_t = 3000)]
    settle_ms: u64,
    /// Where to save the frames.
    #[arg(long, default_value = "target/crosscheck/secure")]
    out: PathBuf,
}

impl SecureViewArgs {
    pub(crate) fn types_secret(&self) -> bool {
        self.screens
            .iter()
            .any(|screen| screen.keys == Keys::Password)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Screen {
    label: String,
    expect: Expect,
    keys: Keys,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Captured,
    Notice,
    Present,
}

impl Expect {
    fn allows(self, before_seen: &[Availability], after_seen: &[Availability]) -> bool {
        let allowed_after: &[Availability] = match self {
            Expect::Captured | Expect::Present => &[Availability::Available],
            Expect::Notice => &[Availability::Available, Availability::SecureDesktop],
        };
        before_seen
            .iter()
            .all(|screen| *screen == Availability::Available)
            && after_seen
                .iter()
                .all(|screen| allowed_after.contains(screen))
    }
}

/// How long `hold-alt-leave` holds Alt before the viewer disconnects, so the helper has injected
/// it by then.
const HOLD_BEFORE_LEAVING: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keys {
    None,
    AltY,
    Escape,
    Password,
    HoldAltLeave,
    /// F20 has no Windows binding, so a key left down by a bug changes nothing the later cases
    /// type.
    HoldF20,
}

impl Keys {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "alt-y" => Self::AltY,
            "esc" => Self::Escape,
            "password" => Self::Password,
            "hold-alt-leave" => Self::HoldAltLeave,
            "hold-f20" => Self::HoldF20,
            _ => return None,
        })
    }
}

impl Screen {
    fn parse(text: &str) -> Result<Self, String> {
        let (text, keys) = match text.split_once('=') {
            None => (text, Keys::None),
            Some((text, keys)) => (
                text,
                Keys::parse(keys).ok_or_else(|| {
                    format!(
                        "unknown keys {keys:?}; try alt-y, esc, password, hold-alt-leave, or \
                         hold-f20"
                    )
                })?,
            ),
        };
        let (label, expect) = match text.split_once(':') {
            None => (text, Expect::Captured),
            Some((label, "notice")) => (label, Expect::Notice),
            Some((label, "present")) => (label, Expect::Present),
            Some((_, other)) => {
                return Err(format!(
                    "unknown expectation {other:?}; try `notice` or `present`"
                ));
            }
        };
        if label.is_empty()
            || !label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(format!(
                "{label:?} is not a label of lowercase letters, digits, and dashes"
            ));
        }
        Ok(Self {
            label: label.into(),
            expect,
            keys,
        })
    }
}

pub(crate) struct Keystrokes(Vec<InputEvent>);

impl Keystrokes {
    fn typing(secret: &str) -> Result<Self, &'static str> {
        if secret.is_empty() {
            return Err("the secret is empty");
        }
        let key = |key, pressed| InputEvent::Key { key, pressed };
        let shift = KeyCode::Named(NamedKey::Shift);
        let mut events = Vec::with_capacity(secret.len() * 4 + 2);
        for character in secret.chars() {
            if !character.is_ascii_alphanumeric() {
                return Err("the secret holds a character other than an ASCII letter or digit");
            }
            let plain = KeyCode::Character(character.to_ascii_lowercase());
            let shifted = character.is_ascii_uppercase();
            if shifted {
                events.push(key(shift, true));
            }
            events.extend([key(plain, true), key(plain, false)]);
            if shifted {
                events.push(key(shift, false));
            }
        }
        let enter = KeyCode::Named(NamedKey::Enter);
        events.extend([key(enter, true), key(enter, false)]);
        Ok(Self(events))
    }
}

impl fmt::Debug for Keystrokes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Keystrokes({} events)", self.0.len())
    }
}

fn prepare(args: &SecureViewArgs) -> anyhow::Result<Option<Keystrokes>> {
    for (index, screen) in args.screens.iter().enumerate() {
        if args.screens[..index]
            .iter()
            .any(|other| other.label == screen.label)
        {
            bail!("--screen {} is given twice", screen.label);
        }
        if screen.keys == Keys::HoldAltLeave && index + 1 != args.screens.len() {
            bail!(
                "--screen {}: hold-alt-leave ends the session, so it must be the last screen",
                screen.label
            );
        }
    }
    if !args.types_secret() {
        return Ok(None);
    }
    let file = args
        .secret_file
        .as_ref()
        .context("a password screen needs --secret-file")?;
    let text =
        std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    Keystrokes::typing(text.trim_end_matches(['\r', '\n']))
        .map(Some)
        .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))
}

struct Shot {
    frame: Arc<DecodedFrame>,
    picture: Thumbnail,
}

struct Album {
    out: PathBuf,
    sealed: bool,
}

impl Album {
    fn save(&self, shot: &Shot, name: &str) -> String {
        if self.sealed {
            return "not saved: a password was typed".into();
        }
        let path = self.out.join(format!("{name}.png"));
        match save_png(&shot.frame, &path) {
            Ok(()) => format!("saved to {}", path.display()),
            Err(error) => format!("could not save {}: {error}", path.display()),
        }
    }
}

struct Feed<'a> {
    viewer: &'a ViewerHandle,
    session: Session,
    frames: watch::Receiver<Option<Arc<DecodedFrame>>>,
    album: Album,
    say: &'a (dyn Fn(&str) + Sync),
}

impl Feed<'_> {
    async fn next_frame(&mut self, deadline: Instant) -> Option<Arc<DecodedFrame>> {
        loop {
            if self.session.ended.is_some() {
                return None;
            }
            let wake = deadline.min(Instant::now() + KEYFRAME_AFTER);
            tokio::select! {
                changed = self.frames.changed() => {
                    changed.ok()?;
                    if let Some(frame) = self.frames.borrow_and_update().clone() {
                        return Some(frame);
                    }
                }
                event = self.session.events.recv() => match event {
                    Some(event) => self.session.apply(event),
                    None => {
                        self.session.ended.get_or_insert(
                            SessionEndReason::ConnectionLost(
                                "the session's events stopped".into(),
                            ),
                        );
                    }
                },
                () = tokio::time::sleep_until(wake) => {
                    if wake >= deadline {
                        return None;
                    }
                    self.viewer.request_keyframe();
                }
            }
        }
    }

    async fn follow(
        &mut self,
        deadline: Instant,
        mut done: impl FnMut(&Session, &DecodedFrame, &Thumbnail) -> bool,
    ) -> Option<Shot> {
        loop {
            let frame = self.next_frame(deadline).await?;
            let picture = Thumbnail::of(frame.width, frame.height, &frame.bgra);
            if done(&self.session, &frame, &picture) {
                return Some(Shot { frame, picture });
            }
        }
    }

    async fn settled(
        &mut self,
        display: &DisplayDescription,
        quiet: Duration,
        deadline: Instant,
    ) -> Option<Shot> {
        let aspect = f64::from(display.width) / f64::from(display.height);
        let mut settle = Settle::new(quiet);
        self.follow(deadline, |_, frame, picture| {
            let frame_aspect = f64::from(frame.width) / f64::from(frame.height);
            (frame_aspect - aspect).abs() < 0.02 && settle.feed(picture, std::time::Instant::now())
        })
        .await
    }

    async fn select(&mut self, display: u32) -> bool {
        if self.session.active == Some(display) {
            return true;
        }
        self.viewer.select_display(display);
        self.session
            .wait_until(Duration::from_secs(10), |session| {
                session.active == Some(display)
            })
            .await
    }

    fn active_display(&self) -> Option<DisplayDescription> {
        self.session
            .displays
            .iter()
            .find(|display| Some(display.id) == self.session.active)
            .cloned()
    }
}

struct Baseline {
    display: DisplayDescription,
    picture: Thumbnail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Failed,
    Left,
}

pub(crate) async fn run(args: SecureViewArgs) -> anyhow::Result<ExitCode> {
    let keystrokes = prepare(&args)?;
    let password = read_password(args.password_file.as_ref()).await?;
    let address = parse_address(&args.address)?;
    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("cannot create {}", args.out.display()))?;
    let (viewer, events) = connect_viewer(
        ViewerConfig {
            target: ViewerTarget::Direct(address),
            client_name: crate::device_name(),
            map_shortcut_modifier: true,
            clipboard: None,
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
    let say = |line: &str| println!("{line}");
    Ok(check(&args, keystrokes.as_ref(), &viewer, events, &say)
        .await
        .finish())
}

async fn check(
    args: &SecureViewArgs,
    keystrokes: Option<&Keystrokes>,
    viewer: &ViewerHandle,
    events: mpsc::UnboundedReceiver<ViewerEvent>,
    say: &(dyn Fn(&str) + Sync),
) -> Verdict {
    let mut feed = Feed {
        viewer,
        session: Session::new(events),
        frames: viewer.frames(),
        album: Album {
            out: args.out.clone(),
            sealed: false,
        },
        say,
    };
    let mut verdict = Verdict::default();
    let outcome = view_screens(args, keystrokes, &mut feed, &mut verdict).await;
    if outcome == Outcome::Done
        && let Some(file) = &args.finish_when
    {
        finish_when(args, &mut feed.session, &mut verdict, file).await;
    }

    let session = &mut feed.session;
    if outcome == Outcome::Left {
        verdict.check(
            session.ended == Some(SessionEndReason::ViewerLeft),
            format!(
                "the session ends as the viewer leaves (ended: {:?})",
                session.ended
            ),
        );
    } else {
        verdict.check(
            session.ended.is_none(),
            format!("the session is still running (ended: {:?})", session.ended),
        );
    }
    println!(
        "host screen statuses: {}",
        session
            .screen_history
            .iter()
            .map(|screen| format!("{screen:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if session.ended.is_none() {
        viewer.disconnect();
        session
            .wait_until(Duration::from_secs(5), |session| session.ended.is_some())
            .await;
    }
    verdict
}

async fn finish_when(
    args: &SecureViewArgs,
    session: &mut Session,
    verdict: &mut Verdict,
    file: &Path,
) {
    let deadline = Instant::now() + Duration::from_secs(args.timeout);
    while !file.exists() {
        if session.ended.is_some() || Instant::now() >= deadline {
            verdict.fail(format!(
                "{} appears within {}s while the session runs",
                file.display(),
                args.timeout
            ));
            return;
        }
        session
            .wait_until(Duration::from_millis(250), |session| {
                session.ended.is_some()
            })
            .await;
    }
}

async fn view_screens(
    args: &SecureViewArgs,
    keystrokes: Option<&Keystrokes>,
    feed: &mut Feed<'_>,
    verdict: &mut Verdict,
) -> Outcome {
    let session = &mut feed.session;
    let reported = session
        .wait_until(Duration::from_secs(40), |session| session.status.is_some())
        .await;
    let input = if args.view_only {
        Availability::NotAllowed
    } else {
        Availability::Available
    };
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
        return Outcome::Failed;
    }
    let displays = session.displays.clone();
    if let Some(expected) = args.expect_displays {
        verdict.check(
            displays.len() == expected,
            format!(
                "the host offers {expected} displays (got {})",
                displays.len()
            ),
        );
    }
    let Some(home) = feed.active_display() else {
        verdict.fail("the active display is one of the host's displays");
        return Outcome::Failed;
    };

    let step = Duration::from_secs(args.timeout);
    let quiet = Duration::from_millis(args.settle_ms);
    let present = args.screens.first().map(|screen| screen.expect) == Some(Expect::Present);
    let others: Vec<_> = displays
        .iter()
        .filter(|display| args.select_display && !present && display.id != home.id)
        .collect();
    if args.answer_on_other_display && others.is_empty() {
        verdict.fail("the host offers another display to answer on");
        return Outcome::Failed;
    }
    let mut baselines = Vec::new();
    for display in others.into_iter().chain([&home]) {
        let shot = if feed.select(display.id).await {
            feed.settled(display, quiet, Instant::now() + step).await
        } else {
            None
        };
        let Some(shot) = shot else {
            verdict.fail(format!(
                "display {} settles within {}s for a baseline",
                display.id, args.timeout
            ));
            return Outcome::Failed;
        };
        feed.album
            .save(&shot, &format!("baseline-display-{}", display.id));
        baselines.push(Baseline {
            display: display.clone(),
            picture: shot.picture,
        });
    }
    (feed.say)("READY");

    for screen in &args.screens {
        let outcome = view_screen(args, keystrokes, feed, verdict, screen, &baselines).await;
        if outcome != Outcome::Done {
            return outcome;
        }
    }
    Outcome::Done
}

async fn view_screen(
    args: &SecureViewArgs,
    keystrokes: Option<&Keystrokes>,
    feed: &mut Feed<'_>,
    verdict: &mut Verdict,
    screen: &Screen,
    baselines: &[Baseline],
) -> Outcome {
    let label = &screen.label;
    let step = Duration::from_secs(args.timeout);
    let Some((home, others)) = baselines.split_last() else {
        return Outcome::Failed;
    };
    let start = feed.session.screen_history.len();

    if screen.expect != Expect::Present {
        if !see(args, feed, verdict, label, home, others).await {
            return Outcome::Failed;
        }
        if !args.answer_on_other_display {
            return_home(args, feed, verdict, label, home).await;
        }
    }
    (feed.say)(&format!("SEEN {label}"));
    let seen = feed.session.screen_history.len();

    if screen.keys != Keys::None {
        if !send_keys(args, keystrokes, feed, verdict, screen).await {
            return Outcome::Failed;
        }
        (feed.say)(&format!("SENT {label}"));
        if screen.keys == Keys::HoldAltLeave {
            feed.viewer.disconnect();
            feed.session
                .wait_until(Duration::from_secs(5), |session| session.ended.is_some())
                .await;
            return Outcome::Left;
        }
    }
    if args.answer_on_other_display && !others.is_empty() {
        let back_home = feed.select(home.display.id).await;
        verdict.check(
            back_home,
            format!("{label}: display {} is selected again", home.display.id),
        );
        if !back_home {
            return Outcome::Failed;
        }
    }

    if screen.expect == Expect::Notice {
        let notice = feed
            .session
            .wait_until(step, |session| {
                session.status.map(|status| status.screen) == Some(Availability::SecureDesktop)
            })
            .await;
        verdict.check(
            notice,
            format!("{label}: the host reports screen SecureDesktop once its helper is gone"),
        );
        if !notice {
            return Outcome::Failed;
        }
        (feed.say)(&format!("NOTICE {label}"));
    }

    if !desktop_returns(args, feed, verdict, label, home, screen.expect).await {
        return Outcome::Failed;
    }
    (feed.say)(&format!("BACK {label}"));

    let history = &feed.session.screen_history;
    let after = match screen.expect {
        Expect::Captured | Expect::Present => "",
        Expect::Notice => ", then SecureDesktop until the screen is gone",
    };
    verdict.check(
        screen
            .expect
            .allows(&history[start..seen], &history[seen..]),
        format!(
            "{label}: the host's screen status stays Available{after} (reported {:?})",
            &history[start..]
        ),
    );
    Outcome::Done
}

async fn desktop_returns(
    args: &SecureViewArgs,
    feed: &mut Feed<'_>,
    verdict: &mut Verdict,
    label: &str,
    home: &Baseline,
    expect: Expect,
) -> bool {
    let present = expect == Expect::Present;
    let step = Duration::from_secs(args.timeout);
    let back = feed
        .follow(Instant::now() + step, |session, _, picture| {
            let difference = home.picture.difference(picture);
            let desktop = if present {
                difference >= CLEARLY_DIFFERENT
            } else {
                difference <= NEAR
            };
            desktop && session.status.map(|status| status.screen) == Some(Availability::Available)
        })
        .await;
    let compared = if present {
        "clearly differs from the screen it showed at connect"
    } else {
        "is near the baseline"
    };
    verdict.check(
        back.is_some(),
        format!(
            "{label}: the desktop comes back within {}s: the picture {compared} and the host \
             reports screen Available",
            args.timeout
        ),
    );
    if back.is_some() {
        return true;
    }
    let Some(frame) = feed.frames.borrow().clone() else {
        return false;
    };
    let picture = Thumbnail::of(frame.width, frame.height, &frame.bgra);
    let difference = home.picture.difference(&picture);
    let saved = feed
        .album
        .save(&Shot { frame, picture }, &format!("secure-{label}-last"));
    println!(
        "{label}: the last frame differs {:.0}% from the baseline, {saved}",
        difference * 100.0
    );
    false
}

async fn send_keys(
    args: &SecureViewArgs,
    keystrokes: Option<&Keystrokes>,
    feed: &mut Feed<'_>,
    verdict: &mut Verdict,
    screen: &Screen,
) -> bool {
    let viewer = feed.viewer;
    let alt = KeyCode::Named(NamedKey::Alt);
    match screen.keys {
        Keys::None => {}
        Keys::AltY => {
            press(viewer, alt, true).await;
            tap(viewer, KeyCode::Character('y')).await;
            press(viewer, alt, false).await;
        }
        Keys::Escape => tap(viewer, KeyCode::Named(NamedKey::Escape)).await,
        Keys::HoldAltLeave => {
            press(viewer, alt, true).await;
            tokio::time::sleep(HOLD_BEFORE_LEAVING).await;
        }
        Keys::HoldF20 => press(viewer, KeyCode::Named(NamedKey::Function(20)), true).await,
        Keys::Password => {
            let Some(keystrokes) = keystrokes else {
                verdict.fail(format!("{}: no secret to type", screen.label));
                return false;
            };
            return type_secret(args, keystrokes, feed, verdict, &screen.label).await;
        }
    }
    true
}

async fn type_secret(
    args: &SecureViewArgs,
    keystrokes: &Keystrokes,
    feed: &mut Feed<'_>,
    verdict: &mut Verdict,
    label: &str,
) -> bool {
    let viewer = feed.viewer;
    let Some(display) = feed.active_display() else {
        verdict.fail(format!("{label}: a display is selected"));
        return false;
    };
    viewer.send_input(InputEvent::PointerMove(pointer_position((0.5, 0.5))));
    tokio::time::sleep(KEY_PAUSE).await;
    for pressed in [true, false] {
        viewer.send_input(InputEvent::PointerButton {
            button: MouseButton::Left,
            pressed,
        });
        tokio::time::sleep(KEY_PAUSE).await;
    }
    let deadline = Instant::now() + Duration::from_secs(args.timeout);
    let quiet = Duration::from_millis(args.settle_ms);
    let Some(shot) = feed.settled(&display, quiet, deadline).await else {
        verdict.fail(format!(
            "{label}: the sign-in screen settles after the click within {}s",
            args.timeout
        ));
        return false;
    };
    let saved = feed.album.save(&shot, &format!("secure-{label}-sign-in"));
    println!("{label}: the sign-in screen before typing, {saved}");
    feed.album.sealed = true;
    for event in &keystrokes.0 {
        viewer.send_input(event.clone());
        tokio::time::sleep(KEY_PAUSE).await;
    }
    true
}

async fn see(
    args: &SecureViewArgs,
    feed: &mut Feed<'_>,
    verdict: &mut Verdict,
    label: &str,
    home: &Baseline,
    others: &[Baseline],
) -> bool {
    let step = Duration::from_secs(args.timeout);
    let quiet = Duration::from_millis(args.settle_ms);
    let Some(first) = feed
        .follow(Instant::now() + step, |_, _, picture| {
            home.picture.difference(picture) >= CLEARLY_DIFFERENT
        })
        .await
    else {
        verdict.fail(format!(
            "{label}: the picture changes clearly within {}s",
            args.timeout
        ));
        return false;
    };
    feed.album.save(&first, &format!("secure-{label}-first"));
    let Some(settled) = feed
        .settled(&home.display, quiet, Instant::now() + step)
        .await
    else {
        verdict.fail(format!(
            "{label}: the picture settles within {}s",
            args.timeout
        ));
        return false;
    };
    let saved = feed.album.save(&settled, &format!("secure-{label}"));
    let difference = home.picture.difference(&settled.picture);
    verdict.check(
        difference >= CLEARLY_DIFFERENT,
        format!(
            "{label}: display {} streams the secure screen: {}x{} frame, {:.0}% differs from the \
             baseline, {saved}",
            home.display.id,
            settled.frame.width,
            settled.frame.height,
            difference * 100.0,
        ),
    );

    for other in others {
        view_other_display(args, feed, verdict, label, other).await;
    }
    true
}

async fn return_home(
    args: &SecureViewArgs,
    feed: &mut Feed<'_>,
    verdict: &mut Verdict,
    label: &str,
    home: &Baseline,
) {
    if feed.session.active == Some(home.display.id) {
        return;
    }
    let shot = if feed.select(home.display.id).await {
        feed.settled(
            &home.display,
            Duration::from_millis(args.settle_ms),
            Instant::now() + Duration::from_secs(args.timeout),
        )
        .await
    } else {
        None
    };
    verdict.check(
        shot.is_some_and(|shot| home.picture.difference(&shot.picture) >= CLEARLY_DIFFERENT),
        format!(
            "{label}: selecting display {} again shows its secure screen",
            home.display.id
        ),
    );
}

async fn view_other_display(
    args: &SecureViewArgs,
    feed: &mut Feed<'_>,
    verdict: &mut Verdict,
    label: &str,
    other: &Baseline,
) {
    let id = other.display.id;
    let shot = if feed.select(id).await {
        feed.settled(
            &other.display,
            Duration::from_millis(args.settle_ms),
            Instant::now() + Duration::from_secs(args.timeout),
        )
        .await
    } else {
        None
    };
    let Some(shot) = shot else {
        verdict.fail(format!(
            "{label}: display {id} is selected and its picture settles"
        ));
        return;
    };
    let saved = feed
        .album
        .save(&shot, &format!("secure-{label}-display-{id}"));
    let difference = other.picture.difference(&shot.picture);
    verdict.check(
        difference >= CLEARLY_DIFFERENT,
        format!(
            "{label}: display {id} streams the secure screen: {}x{} frame, {:.0}% differs from \
             its baseline, {saved}",
            shot.frame.width,
            shot.frame.height,
            difference * 100.0,
        ),
    );
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Mutex;

    use clap::Parser as _;
    use dari_input::{InjectError, InputBackend, RecordedAction};
    use dari_media::{
        CaptureError, CapturedFrame, DisplayInfo, RgbaFrame, ScreenCapturer, StreamSettings,
    };
    use dari_net::DeviceIdentity;
    use dari_session::{HostConfig, HostEvent, HostPlatform, HostPolicy, start_host};

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Scene {
        Desktop,
        Dimmed,
        Prompt,
        Uncapturable,
    }

    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 180;

    struct SceneCapturer(Arc<Mutex<Scene>>);

    impl ScreenCapturer for SceneCapturer {
        fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
            let scene = *self.0.lock().unwrap();
            let (level, prompt) = match scene {
                Scene::Desktop => (120, false),
                Scene::Dimmed => (60, false),
                Scene::Prompt => (60, true),
                Scene::Uncapturable => {
                    std::thread::sleep(timeout);
                    return Err(CaptureError::SecureDesktop);
                }
            };
            let mut pixels = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    let inside = (WIDTH / 3..WIDTH * 2 / 3).contains(&x)
                        && (HEIGHT / 4..HEIGHT * 3 / 4).contains(&y);
                    let value = if prompt && inside { 230 } else { level };
                    pixels.extend([value, value, value, 255]);
                }
            }
            Ok(RgbaFrame::new(WIDTH, HEIGHT, pixels).map(CapturedFrame::Rgba))
        }
    }

    type Recorded = Arc<Mutex<Vec<RecordedAction>>>;

    struct Recorder(Recorded);

    impl InputBackend for Recorder {
        fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
            self.0.lock().unwrap().push(RecordedAction::Move(x, y));
            Ok(())
        }
        fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
            self.0
                .lock()
                .unwrap()
                .push(RecordedAction::Button(button, pressed));
            Ok(())
        }
        fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
            self.0.lock().unwrap().push(RecordedAction::Scroll(dx, dy));
            Ok(())
        }
        fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
            self.0
                .lock()
                .unwrap()
                .push(RecordedAction::Key(key, pressed));
            Ok(())
        }
        fn text(&mut self, text: &str) -> Result<(), InjectError> {
            self.0
                .lock()
                .unwrap()
                .push(RecordedAction::Text(text.into()));
            Ok(())
        }
    }

    struct ScenePlatform {
        scene: Arc<Mutex<Scene>>,
        recorded: Recorded,
    }

    impl HostPlatform for ScenePlatform {
        fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
            Ok(vec![DisplayInfo {
                id: 1,
                name: "fake".into(),
                x: 0,
                y: 0,
                width: WIDTH,
                height: HEIGHT,
                scale_factor: 1.0,
                is_primary: true,
                refresh_rate: 60,
            }])
        }
        fn open_capturer(
            &self,
            _display: u32,
            _settings: StreamSettings,
        ) -> Result<Box<dyn ScreenCapturer>, CaptureError> {
            Ok(Box::new(SceneCapturer(self.scene.clone())))
        }
        fn open_input(&self) -> Result<Box<dyn InputBackend>, InjectError> {
            Ok(Box::new(Recorder(self.recorded.clone())))
        }
    }

    struct Run {
        said: mpsc::UnboundedReceiver<String>,
        check: tokio::task::JoinHandle<ExitCode>,
        out: PathBuf,
        recorded: Recorded,
    }

    impl Run {
        async fn expect_line(&mut self, expected: &str) {
            let line = tokio::time::timeout(Duration::from_secs(30), self.said.recv())
                .await
                .unwrap_or_else(|_| panic!("no {expected:?} within 30s"));
            assert_eq!(line.as_deref(), Some(expected));
        }

        async fn expect_silence(&mut self) {
            if let Ok(line) = tokio::time::timeout(Duration::from_secs(2), self.said.recv()).await {
                panic!("said {line:?} before the screen changed");
            }
        }

        async fn keys(&self, count: usize) -> Vec<(KeyCode, bool)> {
            let keys = || {
                self.recorded
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|action| match action {
                        RecordedAction::Key(key, pressed) => Some((*key, *pressed)),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            for _ in 0..100 {
                if keys().len() >= count {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            keys()
        }

        async fn finish(self) -> ExitCode {
            let code = self.check.await.unwrap();
            std::fs::remove_dir_all(&self.out).unwrap();
            code
        }
    }

    fn out_dir(arguments: &[&str]) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dari-check-secure-{}-{}",
            std::process::id(),
            arguments.join("-").replace([':', '/', '=', '\\', '.'], "_")
        ))
    }

    async fn start_check(scene: &Arc<Mutex<Scene>>, arguments: &[&str]) -> Run {
        let recorded = Recorded::default();
        let (host, mut host_events) = start_host(
            HostConfig {
                bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                host_name: "fake-windows".into(),
                stream: StreamSettings::default(),
                policy: HostPolicy {
                    require_approval: false,
                    clipboard: false,
                    file_transfer: false,
                    audio: false,
                },
                downloads: None,
                relay: None,
            },
            Arc::new(DeviceIdentity::generate().unwrap()),
            Arc::new(ScenePlatform {
                scene: scene.clone(),
                recorded: recorded.clone(),
            }),
        )
        .unwrap();
        let Some(HostEvent::PasswordChanged(Some(password))) = host_events.recv().await else {
            panic!("the host has no password");
        };
        let out = out_dir(arguments);
        let crate::Command::SecureView(args) = crate::Cli::parse_from(
            [
                "dari-check",
                "secure-view",
                "127.0.0.1:1",
                "--out",
                out.to_str().unwrap(),
            ]
            .iter()
            .chain(arguments),
        )
        .command
        else {
            panic!("not parsed as secure-view");
        };
        let keystrokes = prepare(&args).unwrap();
        std::fs::create_dir_all(&args.out).unwrap();
        let (viewer, events) = connect_viewer(
            ViewerConfig {
                target: ViewerTarget::Direct(host.local_address()),
                client_name: "test-viewer".into(),
                map_shortcut_modifier: true,
                clipboard: None,
                frame_rate: 30,
                downloads: None,
                audio: None,
                play_audio: false,
            },
            &password,
        )
        .await
        .unwrap();
        let (lines, said) = mpsc::unbounded_channel();
        let check = tokio::spawn(async move {
            let _host = (host, host_events);
            let say = move |line: &str| {
                println!("{line}");
                let _ = lines.send(line.to_owned());
            };
            check(&args, keystrokes.as_ref(), &viewer, events, &say)
                .await
                .finish()
        });
        Run {
            said,
            check,
            out,
            recorded,
        }
    }

    fn centre(path: &Path) -> u8 {
        let image = image::open(path).unwrap().into_luma8();
        image.get_pixel(image.width() / 2, image.height() / 2).0[0]
    }

    fn named(key: NamedKey) -> KeyCode {
        KeyCode::Named(key)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn follows_a_prompt_and_then_the_notice_when_capture_is_lost() {
        let scene = Arc::new(Mutex::new(Scene::Desktop));
        let set = |next| *scene.lock().unwrap() = next;
        let mut run = start_check(
            &scene,
            &[
                "--screen",
                "uac",
                "--screen",
                "helper-killed:notice",
                "--settle-ms",
                "500",
                "--timeout",
                "15",
            ],
        )
        .await;

        run.expect_line("READY").await;
        set(Scene::Dimmed);
        tokio::time::sleep(Duration::from_millis(300)).await;
        set(Scene::Prompt);
        run.expect_line("SEEN uac").await;
        run.expect_silence().await;
        set(Scene::Desktop);
        run.expect_line("BACK uac").await;

        set(Scene::Prompt);
        run.expect_line("SEEN helper-killed").await;
        run.expect_silence().await;
        set(Scene::Uncapturable);
        run.expect_line("NOTICE helper-killed").await;
        run.expect_silence().await;
        set(Scene::Desktop);
        run.expect_line("BACK helper-killed").await;
        assert!(
            centre(&run.out.join("secure-uac-first.png")) < 100,
            "the first changed frame is the dimmed desktop"
        );
        assert!(
            centre(&run.out.join("secure-uac.png")) > 200,
            "the saved frame waited for the prompt"
        );
        assert_eq!(run.keys(0).await, [], "a screen without keys sends none");
        assert_eq!(run.finish().await, ExitCode::SUCCESS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fails_when_the_host_shows_the_notice_instead_of_the_secure_screen() {
        let scene = Arc::new(Mutex::new(Scene::Desktop));
        let mut run = start_check(
            &scene,
            &["--screen", "uac", "--settle-ms", "500", "--timeout", "4"],
        )
        .await;
        run.expect_line("READY").await;
        *scene.lock().unwrap() = Scene::Uncapturable;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), run.said.recv())
                .await
                .unwrap(),
            None,
            "the check says nothing after READY"
        );
        assert_eq!(run.finish().await, ExitCode::FAILURE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_each_screen_with_its_keys_once_it_settles() {
        let scene = Arc::new(Mutex::new(Scene::Desktop));
        let set = |next| *scene.lock().unwrap() = next;
        let mut run = start_check(
            &scene,
            &[
                "--screen",
                "uac-allow=alt-y",
                "--screen",
                "uac-deny=esc",
                "--settle-ms",
                "500",
                "--timeout",
                "15",
            ],
        )
        .await;
        run.expect_line("READY").await;
        set(Scene::Prompt);
        run.expect_line("SEEN uac-allow").await;
        run.expect_line("SENT uac-allow").await;
        let alt = named(NamedKey::Alt);
        let y = KeyCode::Character('y');
        assert_eq!(
            run.keys(4).await,
            [(alt, true), (y, true), (y, false), (alt, false)]
        );
        set(Scene::Desktop);
        run.expect_line("BACK uac-allow").await;

        set(Scene::Prompt);
        run.expect_line("SEEN uac-deny").await;
        run.expect_line("SENT uac-deny").await;
        let escape = named(NamedKey::Escape);
        assert_eq!(run.keys(6).await[4..], [(escape, true), (escape, false)]);
        set(Scene::Desktop);
        run.expect_line("BACK uac-deny").await;
        assert_eq!(run.finish().await, ExitCode::SUCCESS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_present_screen_is_answered_at_once_and_back_once_the_picture_leaves_it() {
        let scene = Arc::new(Mutex::new(Scene::Prompt));
        let mut run = start_check(
            &scene,
            &[
                "--screen",
                "drop-esc:present=esc",
                "--settle-ms",
                "500",
                "--timeout",
                "15",
            ],
        )
        .await;
        run.expect_line("READY").await;
        run.expect_line("SEEN drop-esc").await;
        run.expect_line("SENT drop-esc").await;
        run.expect_silence().await;
        *scene.lock().unwrap() = Scene::Desktop;
        run.expect_line("BACK drop-esc").await;
        assert_eq!(run.finish().await, ExitCode::SUCCESS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn leaves_with_alt_down() {
        let scene = Arc::new(Mutex::new(Scene::Desktop));
        let mut run = start_check(
            &scene,
            &[
                "--screen",
                "drop-mid-prompt=hold-alt-leave",
                "--settle-ms",
                "500",
                "--timeout",
                "15",
            ],
        )
        .await;
        run.expect_line("READY").await;
        *scene.lock().unwrap() = Scene::Prompt;
        run.expect_line("SEEN drop-mid-prompt").await;
        run.expect_line("SENT drop-mid-prompt").await;
        assert_eq!(run.keys(1).await[0], (named(NamedKey::Alt), true));
        assert_eq!(run.finish().await, ExitCode::SUCCESS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn holds_the_key_until_the_finish_file_appears() {
        let scene = Arc::new(Mutex::new(Scene::Desktop));
        let set = |next| *scene.lock().unwrap() = next;
        let finish = out_dir(&["finish"]).with_extension("done");
        let _absent = std::fs::remove_file(&finish);
        let mut run = start_check(
            &scene,
            &[
                "--screen",
                "helper-killed:notice=hold-f20",
                "--finish-when",
                finish.to_str().unwrap(),
                "--settle-ms",
                "500",
                "--timeout",
                "15",
            ],
        )
        .await;
        run.expect_line("READY").await;
        set(Scene::Prompt);
        run.expect_line("SEEN helper-killed").await;
        run.expect_line("SENT helper-killed").await;
        set(Scene::Uncapturable);
        run.expect_line("NOTICE helper-killed").await;
        set(Scene::Desktop);
        run.expect_line("BACK helper-killed").await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!run.check.is_finished(), "the viewer waits for the file");
        let f20 = named(NamedKey::Function(20));
        assert_eq!(run.keys(1).await, [(f20, true)], "F20 is still down");
        std::fs::write(&finish, "").unwrap();
        assert_eq!(run.finish().await, ExitCode::SUCCESS);
        std::fs::remove_file(finish).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn types_the_secret_and_saves_no_frame_once_typing_starts() {
        let scene = Arc::new(Mutex::new(Scene::Desktop));
        let secret = out_dir(&["secret"]).with_extension("txt");
        std::fs::write(&secret, "aB3\n").unwrap();
        let mut run = start_check(
            &scene,
            &[
                "--screen",
                "lock-unlock=password",
                "--secret-file",
                secret.to_str().unwrap(),
                "--settle-ms",
                "500",
                "--timeout",
                "5",
            ],
        )
        .await;
        run.expect_line("READY").await;
        *scene.lock().unwrap() = Scene::Prompt;
        run.expect_line("SEEN lock-unlock").await;
        run.expect_line("SENT lock-unlock").await;
        let (shift, enter) = (named(NamedKey::Shift), named(NamedKey::Enter));
        let (a, b, three) = (
            KeyCode::Character('a'),
            KeyCode::Character('b'),
            KeyCode::Character('3'),
        );
        assert_eq!(
            run.keys(10).await,
            [
                (a, true),
                (a, false),
                (shift, true),
                (b, true),
                (b, false),
                (shift, false),
                (three, true),
                (three, false),
                (enter, true),
                (enter, false),
            ]
        );
        assert!(
            run.recorded
                .lock()
                .unwrap()
                .contains(&RecordedAction::Button(MouseButton::Left, true)),
            "a click lifts the curtain first"
        );
        let out = run.out.clone();
        let code = run.check.await.unwrap();
        assert_eq!(code, ExitCode::FAILURE);
        assert!(out.join("secure-lock-unlock-sign-in.png").exists());
        assert!(
            !out.join("secure-lock-unlock-last.png").exists(),
            "no frame is saved after typing"
        );
        std::fs::remove_dir_all(out).unwrap();
        std::fs::remove_file(secret).unwrap();
    }

    #[test]
    fn screens_parse_from_labels_with_an_expectation_and_keys() {
        let screen = |label: &str, expect, keys| Screen {
            label: label.into(),
            expect,
            keys,
        };
        for (text, parsed) in [
            ("uac", screen("uac", Expect::Captured, Keys::None)),
            (
                "helper-killed:notice",
                screen("helper-killed", Expect::Notice, Keys::None),
            ),
            (
                "uac-allow=alt-y",
                screen("uac-allow", Expect::Captured, Keys::AltY),
            ),
            ("deny=esc", screen("deny", Expect::Captured, Keys::Escape)),
            (
                "lock-unlock=password",
                screen("lock-unlock", Expect::Captured, Keys::Password),
            ),
            (
                "drop=hold-alt-leave",
                screen("drop", Expect::Captured, Keys::HoldAltLeave),
            ),
            (
                "killed:notice=hold-f20",
                screen("killed", Expect::Notice, Keys::HoldF20),
            ),
            (
                "again:present=esc",
                screen("again", Expect::Present, Keys::Escape),
            ),
        ] {
            assert_eq!(Screen::parse(text), Ok(parsed), "{text}");
        }
        for bad in [
            "",
            ":notice",
            "UAC",
            "uac:answer",
            "../uac",
            "uac prompt",
            "uac=",
            "uac=alt-n",
            "uac=esc:notice",
            "uac:notice:present",
        ] {
            assert!(Screen::parse(bad).is_err(), "{bad:?}");
        }
    }

    fn parsed(arguments: &[&str]) -> SecureViewArgs {
        let crate::Command::SecureView(args) = crate::Cli::parse_from(
            ["dari-check", "secure-view", "127.0.0.1:1"]
                .iter()
                .chain(arguments),
        )
        .command
        else {
            panic!("not parsed as secure-view");
        };
        args
    }

    #[test]
    fn leaving_must_be_last_and_typing_needs_a_secret() {
        assert!(prepare(&parsed(&["--screen", "a=hold-alt-leave", "--screen", "b"])).is_err());
        assert!(prepare(&parsed(&["--screen", "a", "--screen", "b=hold-alt-leave"])).is_ok());
        assert!(prepare(&parsed(&["--screen", "a", "--screen", "a=esc"])).is_err());
        let error = prepare(&parsed(&["--screen", "lock=password"])).unwrap_err();
        assert!(error.to_string().contains("--secret-file"), "{error}");
        assert!(
            crate::Cli::try_parse_from([
                "dari-check",
                "secure-view",
                "127.0.0.1:1",
                "--screen",
                "a",
                "--answer-on-other-display",
            ])
            .is_err(),
            "answering on another display needs --select-display"
        );
    }

    #[test]
    fn only_a_password_screen_turns_logging_off() {
        assert!(parsed(&["--screen", "a", "--screen", "lock=password"]).types_secret());
        assert!(!parsed(&["--screen", "a=alt-y", "--screen", "b=hold-f20"]).types_secret());
    }

    #[test]
    fn a_secret_is_typed_as_keys_and_never_shown() {
        let typed = Keystrokes::typing("Zz9").unwrap();
        assert_eq!(format!("{typed:?}"), "Keystrokes(10 events)");
        let key = |key, pressed| InputEvent::Key { key, pressed };
        let (shift, z) = (KeyCode::Named(NamedKey::Shift), KeyCode::Character('z'));
        assert_eq!(
            typed.0[..4],
            [
                key(shift, true),
                key(z, true),
                key(z, false),
                key(shift, false)
            ]
        );
        for bad in ["", "pass word", "p@ss", "한글"] {
            let error = Keystrokes::typing(bad).err().unwrap();
            assert!(
                !error.contains(bad) || bad.is_empty(),
                "{error} quotes {bad:?}"
            );
        }
    }

    #[test]
    fn only_a_notice_screen_allows_the_secure_desktop_notice_and_only_after_seen() {
        use Availability::{Available, SecureDesktop, Unavailable};
        assert!(Expect::Captured.allows(&[Available], &[Available, Available]));
        assert!(Expect::Captured.allows(&[], &[]));
        assert!(!Expect::Captured.allows(&[Available], &[SecureDesktop, Available]));
        assert!(!Expect::Captured.allows(&[Unavailable], &[]));
        assert!(!Expect::Present.allows(&[], &[SecureDesktop, Available]));

        assert!(Expect::Notice.allows(&[Available], &[SecureDesktop, Available]));
        assert!(!Expect::Notice.allows(&[SecureDesktop], &[Available]));
        assert!(!Expect::Notice.allows(&[], &[SecureDesktop, Unavailable, Available]));
    }
}
