//! The secure-screen check: views a Windows host while a script shows it secure screens (a UAC
//! prompt, the lock screen) and checks each one reaches the viewer while the session survives.
//! The viewer gets no signal that the host switched desktops, so the check and the script
//! (`scripts/crosscheck/secure-desktop.sh`) take turns through this check's output: `READY` once
//! the baseline is saved, then for each screen `SEEN LABEL` once its picture has settled (the
//! script then dismisses it), `NOTICE LABEL` when the host reports the secure-desktop notice in
//! a screen that expects it, and `BACK LABEL` once the desktop is back.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use dari_media::DecodedFrame;
use dari_proto::{Availability, DisplayDescription};
use dari_session::{ViewerConfig, ViewerEvent, ViewerHandle, ViewerTarget, connect_viewer};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::picture::{CLEARLY_DIFFERENT, NEAR, Settle, Thumbnail};
use crate::scenario::Verdict;
use crate::viewer::{KEYFRAME_AFTER, Session, parse_address, read_password, save_png};

#[derive(Debug, clap::Args)]
pub(crate) struct SecureViewArgs {
    /// The host's address (`IP` or `IP:port`).
    address: String,
    /// File holding the host's access password; read from stdin when omitted.
    #[arg(long)]
    password_file: Option<PathBuf>,
    /// A secure screen the script shows, in order: `LABEL`, or `LABEL:notice` when the script
    /// ends the host's secure-desktop helper once the screen is seen.
    #[arg(long = "screen", required = true, value_parser = Screen::parse)]
    screens: Vec<Screen>,
    /// While each screen is up, also select every other display and save its frame.
    #[arg(long)]
    select_display: bool,
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

/// A secure screen the script shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Screen {
    label: String,
    expect: Expect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// The host streams the screen while it is up, and its screen status stays Available.
    Captured,
    /// Streamed until `SEEN`. Then the script ends the host's secure-desktop helper, and the
    /// host shows PR 36's notice (screen status `SecureDesktop`) until the screen is gone.
    Notice,
}

impl Expect {
    /// Whether the screen statuses the host reported during a screen, before and after `SEEN`,
    /// are the ones it allows.
    fn allows(self, before_seen: &[Availability], after_seen: &[Availability]) -> bool {
        let allowed_after: &[Availability] = match self {
            Expect::Captured => &[Availability::Available],
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

impl Screen {
    fn parse(text: &str) -> Result<Self, String> {
        let (label, expect) = match text.split_once(':') {
            None => (text, Expect::Captured),
            Some((label, "notice")) => (label, Expect::Notice),
            Some((_, other)) => return Err(format!("unknown expectation {other:?}; try `notice`")),
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
        })
    }
}

/// A frame and its thumbnail.
struct Shot {
    frame: Arc<DecodedFrame>,
    picture: Thumbnail,
}

/// The viewer's frames and events, read together so each status change is recorded while the
/// check is still in the step it belongs to.
struct Feed<'a> {
    viewer: &'a ViewerHandle,
    session: Session,
    frames: watch::Receiver<Option<Arc<DecodedFrame>>>,
    /// Prints a line the script waits for.
    say: &'a (dyn Fn(&str) + Sync),
}

impl Feed<'_> {
    /// The next frame, handling session events meanwhile. Hosts send frames only when their
    /// screen changes, so a pause brings a keyframe request. None once `deadline` passes or the
    /// session ends.
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
                            dari_session::SessionEndReason::ConnectionLost(
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

    /// The first frame for which `done` holds, before `deadline`.
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

    /// The display's picture once it has stopped changing.
    async fn settled(
        &mut self,
        display: &DisplayDescription,
        quiet: Duration,
        deadline: Instant,
    ) -> Option<Shot> {
        let aspect = f64::from(display.width) / f64::from(display.height);
        let mut settle = Settle::new(quiet);
        self.follow(deadline, |_, frame, picture| {
            // The first frames after a switch may still show the previous display.
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
}

/// A display and its picture before any secure screen.
struct Baseline {
    display: DisplayDescription,
    picture: Thumbnail,
}

pub(crate) async fn run(args: SecureViewArgs) -> anyhow::Result<ExitCode> {
    for (index, screen) in args.screens.iter().enumerate() {
        if args.screens[..index]
            .iter()
            .any(|other| other.label == screen.label)
        {
            bail!("--screen {} is given twice", screen.label);
        }
    }
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
    Ok(check(&args, &viewer, events, &say).await.finish())
}

/// Runs the check on a connected viewer, saying each line the script waits for with `say`.
async fn check(
    args: &SecureViewArgs,
    viewer: &ViewerHandle,
    events: mpsc::UnboundedReceiver<ViewerEvent>,
    say: &(dyn Fn(&str) + Sync),
) -> Verdict {
    let mut feed = Feed {
        viewer,
        session: Session::new(events),
        frames: viewer.frames(),
        say,
    };
    let mut verdict = Verdict::default();
    view_screens(args, &mut feed, &mut verdict).await;

    let session = &mut feed.session;
    verdict.check(
        session.ended.is_none(),
        format!("the session is still running (ended: {:?})", session.ended),
    );
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

async fn view_screens(args: &SecureViewArgs, feed: &mut Feed<'_>, verdict: &mut Verdict) {
    let session = &mut feed.session;
    let reported = session
        .wait_until(Duration::from_secs(40), |session| session.status.is_some())
        .await;
    verdict.check(
        reported
            && session.status.map(|status| (status.screen, status.input))
                == Some((Availability::Available, Availability::Available)),
        format!(
            "the host reports screen and input Available (got {:?})",
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
    if let Some(expected) = args.expect_displays {
        verdict.check(
            displays.len() == expected,
            format!(
                "the host offers {expected} displays (got {})",
                displays.len()
            ),
        );
    }
    let Some(home) = displays
        .iter()
        .find(|display| Some(display.id) == session.active)
        .cloned()
    else {
        verdict.fail("the active display is one of the host's displays");
        return;
    };

    let step = Duration::from_secs(args.timeout);
    let quiet = Duration::from_millis(args.settle_ms);
    let others: Vec<_> = displays
        .iter()
        .filter(|display| args.select_display && display.id != home.id)
        .collect();
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
            return;
        };
        save(args, &shot, &format!("baseline-display-{}", display.id));
        baselines.push(Baseline {
            display: display.clone(),
            picture: shot.picture,
        });
    }
    (feed.say)("READY");

    for screen in &args.screens {
        if !view_screen(args, feed, verdict, screen, &baselines).await {
            return;
        }
    }
}

/// Follows one screen from its appearance to its end. Returns false when the turn-taking with
/// the script broke and later screens can't be told apart.
async fn view_screen(
    args: &SecureViewArgs,
    feed: &mut Feed<'_>,
    verdict: &mut Verdict,
    screen: &Screen,
    baselines: &[Baseline],
) -> bool {
    let label = &screen.label;
    let step = Duration::from_secs(args.timeout);
    let Some(home) = baselines.last() else {
        return false;
    };
    let start = feed.session.screen_history.len();

    if !see(args, feed, verdict, label, baselines).await {
        return false;
    }
    (feed.say)(&format!("SEEN {label}"));
    let seen = feed.session.screen_history.len();

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
            return false;
        }
        (feed.say)(&format!("NOTICE {label}"));
    }

    let back = feed
        .follow(Instant::now() + step, |session, _, picture| {
            home.picture.difference(picture) <= NEAR
                && session.status.map(|status| status.screen) == Some(Availability::Available)
        })
        .await;
    verdict.check(
        back.is_some(),
        format!(
            "{label}: the desktop comes back within {}s: the picture is near the baseline and the \
             host reports screen Available",
            args.timeout
        ),
    );
    if back.is_none() {
        let last = feed.frames.borrow().clone();
        if let Some(frame) = last {
            let picture = Thumbnail::of(frame.width, frame.height, &frame.bgra);
            let difference = home.picture.difference(&picture);
            let path = save(
                args,
                &Shot { frame, picture },
                &format!("secure-{label}-last"),
            );
            println!(
                "{label}: the last frame differs {:.0}% from the baseline, saved to {}",
                difference * 100.0,
                path.display()
            );
        }
        return false;
    }
    (feed.say)(&format!("BACK {label}"));

    let history = &feed.session.screen_history;
    let after = match screen.expect {
        Expect::Captured => "",
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
    true
}

/// Waits for a screen to appear and settle on the active display, and saves it and every other
/// display's picture of it. Returns false when it never appeared or settled.
async fn see(
    args: &SecureViewArgs,
    feed: &mut Feed<'_>,
    verdict: &mut Verdict,
    label: &str,
    baselines: &[Baseline],
) -> bool {
    let step = Duration::from_secs(args.timeout);
    let quiet = Duration::from_millis(args.settle_ms);
    let Some((home, others)) = baselines.split_last() else {
        return false;
    };
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
    save(args, &first, &format!("secure-{label}-first"));
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
    let path = save(args, &settled, &format!("secure-{label}"));
    let difference = home.picture.difference(&settled.picture);
    verdict.check(
        difference >= CLEARLY_DIFFERENT,
        format!(
            "{label}: display {} streams the secure screen: {}x{} frame, {:.0}% differs from the \
             baseline, saved to {}",
            home.display.id,
            settled.frame.width,
            settled.frame.height,
            difference * 100.0,
            path.display()
        ),
    );

    for other in others {
        view_other_display(args, feed, verdict, label, other).await;
    }
    if !others.is_empty() {
        let back_home = feed.select(home.display.id).await;
        let shot = if back_home {
            feed.settled(&home.display, quiet, Instant::now() + step)
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
    true
}

/// Selects another display while a secure screen is up, and checks it shows its own secure
/// desktop rather than its picture from before.
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
    let path = save(args, &shot, &format!("secure-{label}-display-{id}"));
    let difference = other.picture.difference(&shot.picture);
    verdict.check(
        difference >= CLEARLY_DIFFERENT,
        format!(
            "{label}: display {id} streams the secure screen: {}x{} frame, {:.0}% differs from \
             its baseline, saved to {}",
            shot.frame.width,
            shot.frame.height,
            difference * 100.0,
            path.display()
        ),
    );
}

fn save(args: &SecureViewArgs, shot: &Shot, name: &str) -> PathBuf {
    let path = args.out.join(format!("{name}.png"));
    if let Err(error) = save_png(&shot.frame, &path) {
        println!("could not save {}: {error}", path.display());
    }
    path
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Mutex;

    use clap::Parser as _;
    use dari_input::{InjectError, InputBackend, RecordingBackend};
    use dari_media::{
        CaptureError, CapturedFrame, DisplayInfo, RgbaFrame, ScreenCapturer, StreamSettings,
    };
    use dari_net::DeviceIdentity;
    use dari_session::{HostConfig, HostEvent, HostPlatform, HostPolicy, start_host};

    use super::*;

    /// What the fake host's screen shows.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Scene {
        Desktop,
        /// The secure desktop's first frames, before the prompt draws.
        Dimmed,
        Prompt,
        /// The secure desktop with no helper to capture it.
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

    struct ScenePlatform(Arc<Mutex<Scene>>);

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
            Ok(Box::new(SceneCapturer(self.0.clone())))
        }
        fn open_input(&self) -> Result<Box<dyn InputBackend>, InjectError> {
            Ok(Box::new(RecordingBackend::default()))
        }
    }

    /// Hosts the scene, connects a viewer, and runs the check with these arguments in the
    /// background; returns the lines it says and its verdict.
    async fn start_check(
        scene: &Arc<Mutex<Scene>>,
        arguments: &[&str],
    ) -> (
        mpsc::UnboundedReceiver<String>,
        tokio::task::JoinHandle<ExitCode>,
        PathBuf,
    ) {
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
            Arc::new(ScenePlatform(scene.clone())),
        )
        .unwrap();
        let Some(HostEvent::PasswordChanged(Some(password))) = host_events.recv().await else {
            panic!("the host has no password");
        };
        let out = std::env::temp_dir().join(format!(
            "dari-check-secure-{}-{}",
            std::process::id(),
            arguments.join("-").replace([':', '/'], "_")
        ));
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
            check(&args, &viewer, events, &say).await.finish()
        });
        (said, check, out)
    }

    /// The brightness of the centre of a saved frame.
    fn centre(path: &std::path::Path) -> u8 {
        let image = image::open(path).unwrap().into_luma8();
        image.get_pixel(image.width() / 2, image.height() / 2).0[0]
    }

    async fn expect_line(said: &mut mpsc::UnboundedReceiver<String>, expected: &str) {
        let line = tokio::time::timeout(Duration::from_secs(30), said.recv())
            .await
            .unwrap_or_else(|_| panic!("no {expected:?} within 30s"));
        assert_eq!(line.as_deref(), Some(expected));
    }

    /// The check waits for the script's next move.
    async fn expect_silence(said: &mut mpsc::UnboundedReceiver<String>) {
        if let Ok(line) = tokio::time::timeout(Duration::from_secs(2), said.recv()).await {
            panic!("said {line:?} before the screen changed");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn follows_a_prompt_and_then_the_notice_when_capture_is_lost() {
        let scene = Arc::new(Mutex::new(Scene::Desktop));
        let set = |next| *scene.lock().unwrap() = next;
        let (mut said, check, out) = start_check(
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

        expect_line(&mut said, "READY").await;
        set(Scene::Dimmed);
        tokio::time::sleep(Duration::from_millis(300)).await;
        set(Scene::Prompt);
        expect_line(&mut said, "SEEN uac").await;
        expect_silence(&mut said).await;
        set(Scene::Desktop);
        expect_line(&mut said, "BACK uac").await;

        set(Scene::Prompt);
        expect_line(&mut said, "SEEN helper-killed").await;
        expect_silence(&mut said).await;
        set(Scene::Uncapturable);
        expect_line(&mut said, "NOTICE helper-killed").await;
        expect_silence(&mut said).await;
        set(Scene::Desktop);
        expect_line(&mut said, "BACK helper-killed").await;
        assert_eq!(check.await.unwrap(), ExitCode::SUCCESS);
        assert!(
            centre(&out.join("secure-uac-first.png")) < 100,
            "the first changed frame is the dimmed desktop"
        );
        assert!(
            centre(&out.join("secure-uac.png")) > 200,
            "the saved frame waited for the prompt"
        );
        std::fs::remove_dir_all(out).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fails_when_the_host_shows_the_notice_instead_of_the_secure_screen() {
        let scene = Arc::new(Mutex::new(Scene::Desktop));
        let (mut said, check, out) = start_check(
            &scene,
            &["--screen", "uac", "--settle-ms", "500", "--timeout", "4"],
        )
        .await;
        expect_line(&mut said, "READY").await;
        *scene.lock().unwrap() = Scene::Uncapturable;
        assert_eq!(check.await.unwrap(), ExitCode::FAILURE);
        assert_eq!(
            said.recv().await,
            None,
            "the check says nothing after READY"
        );
        std::fs::remove_dir_all(out).unwrap();
    }

    #[test]
    fn screens_parse_from_labels_with_an_optional_expectation() {
        assert_eq!(
            Screen::parse("uac"),
            Ok(Screen {
                label: "uac".into(),
                expect: Expect::Captured
            })
        );
        assert_eq!(
            Screen::parse("helper-killed:notice"),
            Ok(Screen {
                label: "helper-killed".into(),
                expect: Expect::Notice
            })
        );
        for bad in ["", ":notice", "UAC", "uac:answer", "../uac", "uac prompt"] {
            assert!(Screen::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_a_notice_screen_allows_the_secure_desktop_notice_and_only_after_seen() {
        use Availability::{Available, SecureDesktop, Unavailable};
        assert!(Expect::Captured.allows(&[Available], &[Available, Available]));
        assert!(Expect::Captured.allows(&[], &[]));
        assert!(!Expect::Captured.allows(&[Available], &[SecureDesktop, Available]));
        assert!(!Expect::Captured.allows(&[Unavailable], &[]));

        assert!(Expect::Notice.allows(&[Available], &[SecureDesktop, Available]));
        assert!(!Expect::Notice.allows(&[SecureDesktop], &[Available]));
        assert!(!Expect::Notice.allows(&[], &[SecureDesktop, Unavailable, Available]));
    }
}
