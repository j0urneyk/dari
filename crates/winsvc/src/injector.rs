use std::sync::mpsc;
use std::time::Duration;

use dari_input::{Injector, InputBackend};
use dari_proto::{InputDesktop, OsInput, PolicyReading, SecureDesktopControl};

pub(crate) const FOLLOW_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) trait InputDesk {
    fn input_desktop(&mut self) -> Option<InputDesktop>;
    fn attach(&mut self, desktop: &InputDesktop) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Note {
    Forgot { count: usize, on: InputDesktop },
    AttachFailed { to: InputDesktop, error: String },
    ReplayFailed { on: InputDesktop },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Attachment {
    None,
    To(InputDesktop),
    Failed(InputDesktop),
}

#[derive(Debug)]
pub(crate) struct Follower<D: InputDesk, B: InputBackend> {
    desk: D,
    injector: Injector<B>,
    attachment: Attachment,
    replay_failure_noted: bool,
}

impl<D: InputDesk, B: InputBackend> Follower<D, B> {
    pub(crate) fn new(desk: D, backend: B) -> Self {
        Self {
            desk,
            injector: Injector::new(backend),
            attachment: Attachment::None,
            replay_failure_noted: false,
        }
    }

    pub(crate) fn step(&mut self, input: Option<&OsInput>) -> Vec<Note> {
        let mut notes = Vec::new();
        if let Some(desktop) = self.desk.input_desktop()
            && self.attachment != Attachment::To(desktop.clone())
        {
            notes.extend(self.attach(desktop));
        }
        if let (Some(input), Attachment::To(desktop)) = (input, &self.attachment)
            && desktop.takes_helper_input()
            && self.injector.replay(input).is_err()
            && !self.replay_failure_noted
        {
            self.replay_failure_noted = true;
            notes.push(Note::ReplayFailed {
                on: desktop.clone(),
            });
        }
        notes
    }

    fn attach(&mut self, desktop: InputDesktop) -> Vec<Note> {
        let mut notes = Vec::new();
        // Windows cleared key state at the switch, so nothing is left to release.
        let count = self.injector.forget();
        if count > 0 {
            notes.push(Note::Forgot {
                count,
                on: desktop.clone(),
            });
        }
        match self.desk.attach(&desktop) {
            Ok(()) => self.attachment = Attachment::To(desktop),
            Err(error) => {
                if self.attachment != Attachment::Failed(desktop.clone()) {
                    notes.push(Note::AttachFailed {
                        to: desktop.clone(),
                        error,
                    });
                }
                self.attachment = Attachment::Failed(desktop);
            }
        }
        notes
    }

    /// Releases what it holds when attached to a desktop that takes helper input, and forgets
    /// it anywhere else. Returns how many inputs it released.
    pub(crate) fn finish(mut self) -> usize {
        match &self.attachment {
            Attachment::To(desktop) if desktop.takes_helper_input() => self.injector.release_all(),
            Attachment::To(_) | Attachment::Failed(_) | Attachment::None => {
                self.injector.forget();
                0
            }
        }
    }

    /// Never releases: a button up over **Yes** completes the click, and a Space up activates the
    /// focused button.
    pub(crate) fn revoke(mut self) -> usize {
        self.injector.forget()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    HungUp {
        released: usize,
    },
    Revoked {
        forgot: usize,
        reading: PolicyReading,
    },
}

/// `policy` is read after every receive, not only on idle ticks: the app is untrusted and can keep
/// the channel busy forever.
pub(crate) fn run<D: InputDesk, B: InputBackend>(
    mut follower: Follower<D, B>,
    mut policy: impl FnMut() -> PolicyReading,
    inputs: &mpsc::Receiver<OsInput>,
    mut note: impl FnMut(Note),
) -> Ended {
    loop {
        let received = inputs.recv_timeout(FOLLOW_INTERVAL);
        let reading = policy();
        if reading.control() == SecureDesktopControl::Off {
            return Ended::Revoked {
                forgot: follower.revoke(),
                reading,
            };
        }
        let input = match received {
            Ok(input) => Some(input),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Ended::HungUp {
                    released: follower.finish(),
                };
            }
        };
        follower
            .step(input.as_ref())
            .into_iter()
            .for_each(&mut note);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Instant;

    use dari_input::{InjectError, RecordedAction, RecordingBackend};
    use dari_proto::{KeyCode, MouseButton, NamedKey};

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Happened {
        Attached(InputDesktop),
        Did(RecordedAction),
    }

    type Timeline = Arc<Mutex<Vec<Happened>>>;

    #[derive(Debug)]
    struct ScriptedDesk {
        desktops: VecDeque<Option<InputDesktop>>,
        last: Option<InputDesktop>,
        failing: usize,
        timeline: Timeline,
    }

    impl InputDesk for ScriptedDesk {
        fn input_desktop(&mut self) -> Option<InputDesktop> {
            if let Some(next) = self.desktops.pop_front() {
                self.last.clone_from(&next);
            }
            self.last.clone()
        }

        fn attach(&mut self, desktop: &InputDesktop) -> Result<(), String> {
            if self.failing > 0 {
                self.failing -= 1;
                return Err("access denied".into());
            }
            self.timeline
                .lock()
                .unwrap()
                .push(Happened::Attached(desktop.clone()));
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Recorder {
        timeline: Timeline,
        failing: bool,
    }

    impl Recorder {
        fn record(
            &mut self,
            call: impl FnOnce(&mut RecordingBackend) -> Result<(), InjectError>,
        ) -> Result<(), InjectError> {
            let mut backend = RecordingBackend::default();
            call(&mut backend)?;
            if self.failing {
                return Err(InjectError::Backend("SendInput failed".into()));
            }
            let mut timeline = self.timeline.lock().unwrap();
            timeline.extend(backend.actions.into_iter().map(Happened::Did));
            Ok(())
        }
    }

    impl InputBackend for Recorder {
        fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
            self.record(|backend| backend.move_pointer(x, y))
        }
        fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
            self.record(|backend| backend.button(button, pressed))
        }
        fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
            self.record(|backend| backend.scroll(dx, dy))
        }
        fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
            self.record(|backend| backend.key(key, pressed))
        }
        fn text(&mut self, text: &str) -> Result<(), InjectError> {
            self.record(|backend| backend.text(text))
        }
    }

    const ALT: KeyCode = KeyCode::Named(NamedKey::Alt);
    const DEFAULT: Option<InputDesktop> = Some(InputDesktop::Default);
    const WINLOGON: Option<InputDesktop> = Some(InputDesktop::Winlogon);

    fn follower(
        desktops: Vec<Option<InputDesktop>>,
        failing_attaches: usize,
    ) -> (Follower<ScriptedDesk, Recorder>, Timeline) {
        follower_replaying(desktops, failing_attaches, false)
    }

    fn follower_replaying(
        desktops: Vec<Option<InputDesktop>>,
        failing_attaches: usize,
        failing_replays: bool,
    ) -> (Follower<ScriptedDesk, Recorder>, Timeline) {
        let timeline = Timeline::default();
        let desk = ScriptedDesk {
            desktops: desktops.into(),
            last: None,
            failing: failing_attaches,
            timeline: timeline.clone(),
        };
        let recorder = Recorder {
            timeline: timeline.clone(),
            failing: failing_replays,
        };
        (Follower::new(desk, recorder), timeline)
    }

    fn alt(pressed: bool) -> OsInput {
        OsInput::Key { key: ALT, pressed }
    }

    fn taken(timeline: &Timeline) -> Vec<Happened> {
        std::mem::take(&mut *timeline.lock().unwrap())
    }

    #[test]
    fn input_is_replayed_only_on_winlogon() {
        let screen_saver = InputDesktop::from_name("Screen-saver");
        let (mut follower, timeline) =
            follower(vec![DEFAULT, Some(screen_saver.clone()), WINLOGON], 0);
        assert_eq!(follower.step(Some(&OsInput::Move { x: 5, y: 6 })), []);
        assert_eq!(follower.step(Some(&alt(true))), []);
        assert_eq!(
            taken(&timeline),
            [
                Happened::Attached(InputDesktop::Default),
                Happened::Attached(screen_saver),
            ]
        );

        follower.step(Some(&OsInput::Move { x: 5, y: 6 }));
        follower.step(Some(&OsInput::Text("pw".into())));
        assert_eq!(
            taken(&timeline),
            [
                Happened::Attached(InputDesktop::Winlogon),
                Happened::Did(RecordedAction::Move(5, 6)),
                Happened::Did(RecordedAction::Text("pw".into())),
            ]
        );
    }

    #[test]
    fn held_input_is_forgotten_at_a_switch_without_injecting_anything() {
        let (mut follower, timeline) = follower(vec![WINLOGON, WINLOGON, DEFAULT, WINLOGON], 0);
        follower.step(Some(&alt(true)));
        follower.step(Some(&OsInput::Button {
            button: MouseButton::Left,
            pressed: true,
        }));
        taken(&timeline);

        assert_eq!(
            follower.step(Some(&alt(false))),
            [Note::Forgot {
                count: 2,
                on: InputDesktop::Default
            }]
        );
        assert_eq!(
            taken(&timeline),
            [Happened::Attached(InputDesktop::Default)]
        );

        assert_eq!(follower.step(None), []);
        assert_eq!(follower.finish(), 0);
        assert_eq!(
            taken(&timeline),
            [Happened::Attached(InputDesktop::Winlogon)]
        );
    }

    #[test]
    fn an_unreadable_desktop_keeps_the_attachment() {
        let (mut follower, timeline) = follower(vec![WINLOGON, None], 0);
        follower.step(None);
        follower.step(Some(&alt(true)));
        assert_eq!(
            taken(&timeline),
            [
                Happened::Attached(InputDesktop::Winlogon),
                Happened::Did(RecordedAction::Key(ALT, true)),
            ]
        );
    }

    #[test]
    fn a_failed_attach_drops_input_and_is_noted_once() {
        let (mut follower, timeline) = follower(vec![WINLOGON], 2);
        let failed = Note::AttachFailed {
            to: InputDesktop::Winlogon,
            error: "access denied".into(),
        };
        assert_eq!(follower.step(Some(&alt(true))), [failed]);
        assert_eq!(follower.step(Some(&alt(true))), []);
        assert_eq!(taken(&timeline), []);

        assert_eq!(follower.step(Some(&alt(true))), []);
        assert_eq!(
            taken(&timeline),
            [
                Happened::Attached(InputDesktop::Winlogon),
                Happened::Did(RecordedAction::Key(ALT, true)),
            ]
        );
    }

    #[test]
    fn what_was_held_is_forgotten_even_when_the_attach_fails() {
        let (mut follower, timeline) = follower(vec![WINLOGON, DEFAULT, DEFAULT], 0);
        follower.step(Some(&alt(true)));
        follower.desk.failing = 1;
        assert_eq!(
            follower.step(None),
            [
                Note::Forgot {
                    count: 1,
                    on: InputDesktop::Default
                },
                Note::AttachFailed {
                    to: InputDesktop::Default,
                    error: "access denied".into()
                },
            ]
        );
        follower.step(None);
        assert_eq!(follower.finish(), 0);
        assert_eq!(
            taken(&timeline),
            [
                Happened::Attached(InputDesktop::Winlogon),
                Happened::Did(RecordedAction::Key(ALT, true)),
                Happened::Attached(InputDesktop::Default),
            ]
        );
    }

    #[test]
    fn a_replay_failure_is_noted_once() {
        let (mut follower, _timeline) = follower_replaying(vec![WINLOGON], 0, true);
        assert_eq!(
            follower.step(Some(&OsInput::Move { x: 1, y: 1 })),
            [Note::ReplayFailed {
                on: InputDesktop::Winlogon
            }]
        );
        assert_eq!(follower.step(Some(&OsInput::Move { x: 1, y: 1 })), []);
    }

    #[test]
    fn the_thread_follows_while_idle_and_releases_when_the_reader_hangs_up() {
        let (follower, timeline) = follower(vec![WINLOGON], 0);
        let (inputs, received) = mpsc::sync_channel(4);
        let notes = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let notes = notes.clone();
            thread::spawn(move || {
                run(
                    follower,
                    || ON,
                    &received,
                    |note| {
                        notes.lock().unwrap().push(note);
                    },
                )
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !timeline
            .lock()
            .unwrap()
            .contains(&Happened::Attached(InputDesktop::Winlogon))
        {
            assert!(Instant::now() < deadline, "never attached while idle");
            thread::sleep(Duration::from_millis(10));
        }

        inputs.send(alt(true)).unwrap();
        inputs
            .send(OsInput::Button {
                button: MouseButton::Right,
                pressed: true,
            })
            .unwrap();
        drop(inputs);
        assert_eq!(thread.join().unwrap(), Ended::HungUp { released: 2 });
        assert_eq!(
            taken(&timeline),
            [
                Happened::Attached(InputDesktop::Winlogon),
                Happened::Did(RecordedAction::Key(ALT, true)),
                Happened::Did(RecordedAction::Button(MouseButton::Right, true)),
                Happened::Did(RecordedAction::Key(ALT, false)),
                Happened::Did(RecordedAction::Button(MouseButton::Right, false)),
            ]
        );
        assert_eq!(*notes.lock().unwrap(), []);
    }

    const ON: PolicyReading = PolicyReading::Stored(1);
    const OFF: PolicyReading = PolicyReading::Stored(0);

    fn scripted(readings: Vec<PolicyReading>) -> impl FnMut() -> PolicyReading {
        let mut readings = VecDeque::from(readings);
        let mut last = ON;
        move || {
            if let Some(next) = readings.pop_front() {
                last = next;
            }
            last.clone()
        }
    }

    fn sent_then_hung_up(inputs: Vec<OsInput>) -> mpsc::Receiver<OsInput> {
        let (sender, received) = mpsc::sync_channel(inputs.len());
        for input in inputs {
            sender.send(input).unwrap();
        }
        received
    }

    fn left(pressed: bool) -> OsInput {
        OsInput::Button {
            button: MouseButton::Left,
            pressed,
        }
    }

    #[test]
    fn turning_the_policy_off_forgets_held_input_without_releasing_it() {
        let (follower, timeline) = follower(vec![WINLOGON], 0);
        let received = sent_then_hung_up(vec![alt(true), left(true), OsInput::Move { x: 5, y: 6 }]);
        let mut notes = Vec::new();
        let ended = run(follower, scripted(vec![ON, ON, OFF]), &received, |note| {
            notes.push(note);
        });
        assert_eq!(
            ended,
            Ended::Revoked {
                forgot: 2,
                reading: OFF
            }
        );
        assert_eq!(
            taken(&timeline),
            [
                Happened::Attached(InputDesktop::Winlogon),
                Happened::Did(RecordedAction::Key(ALT, true)),
                Happened::Did(RecordedAction::Button(MouseButton::Left, true)),
            ]
        );
        assert_eq!(notes, []);
    }

    #[test]
    fn an_off_policy_at_the_first_read_injects_nothing() {
        let (follower, timeline) = follower(vec![WINLOGON], 0);
        let received = sent_then_hung_up(vec![alt(true)]);
        let ended = run(follower, scripted(vec![OFF]), &received, |_| {});
        assert_eq!(
            ended,
            Ended::Revoked {
                forgot: 0,
                reading: OFF
            }
        );
        assert_eq!(taken(&timeline), []);
    }

    #[test]
    fn an_unreadable_policy_ends_input_and_says_why() {
        let (follower, timeline) = follower(vec![WINLOGON], 0);
        let received = sent_then_hung_up(vec![alt(true)]);
        let unreadable = PolicyReading::Unreadable("Access is denied. (0x80070005)".into());
        let ended = run(
            follower,
            scripted(vec![unreadable.clone()]),
            &received,
            |_| {},
        );
        assert_eq!(
            ended,
            Ended::Revoked {
                forgot: 0,
                reading: unreadable
            }
        );
        assert_eq!(taken(&timeline), []);
    }

    #[test]
    fn an_off_policy_when_the_reader_hangs_up_forgets_instead_of_releasing() {
        let (follower, timeline) = follower(vec![WINLOGON], 0);
        let received = sent_then_hung_up(vec![alt(true)]);
        let ended = run(follower, scripted(vec![ON, OFF]), &received, |_| {});
        assert_eq!(
            ended,
            Ended::Revoked {
                forgot: 1,
                reading: OFF
            }
        );
        assert_eq!(
            taken(&timeline),
            [
                Happened::Attached(InputDesktop::Winlogon),
                Happened::Did(RecordedAction::Key(ALT, true)),
            ]
        );
    }

    #[test]
    fn an_idle_thread_notices_the_policy_within_a_second() {
        let (follower, timeline) = follower(vec![WINLOGON], 0);
        let (inputs, received) = mpsc::sync_channel::<OsInput>(1);
        let (ended, end) = mpsc::channel();
        thread::spawn(move || {
            let _sent = ended.send(run(
                follower,
                scripted(vec![ON, ON, OFF]),
                &received,
                |_| {},
            ));
        });
        assert_eq!(
            end.recv_timeout(Duration::from_secs(1)),
            Ok(Ended::Revoked {
                forgot: 0,
                reading: OFF
            })
        );
        assert_eq!(
            taken(&timeline),
            [Happened::Attached(InputDesktop::Winlogon)]
        );
        drop(inputs);
    }
}
