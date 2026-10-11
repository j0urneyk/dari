use std::sync::mpsc;
use std::time::Duration;

use dari_input::{Injector, InputBackend};
use dari_proto::{InputDesktop, OsInput};

pub(crate) const FOLLOW_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) trait InputDesk {
    fn input_desktop(&mut self) -> Option<InputDesktop>;
    fn attach(&mut self, desktop: &InputDesktop) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Note {
    Released { count: usize, on: InputDesktop },
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
        // The app injects on `Default` itself. A SYSTEM injector there would bypass UIPI.
        if let (Some(input), Attachment::To(desktop)) = (input, &self.attachment)
            && *desktop != InputDesktop::Default
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

    fn attach(&mut self, desktop: InputDesktop) -> Option<Note> {
        match self.desk.attach(&desktop) {
            Ok(()) => {
                self.attachment = Attachment::To(desktop.clone());
                // Windows already switched, so the key state the new desktop sees is what matters.
                let count = self.injector.release_all();
                (count > 0).then_some(Note::Released { count, on: desktop })
            }
            Err(error) => {
                let repeated = self.attachment == Attachment::Failed(desktop.clone());
                self.attachment = Attachment::Failed(desktop.clone());
                (!repeated).then_some(Note::AttachFailed { to: desktop, error })
            }
        }
    }

    pub(crate) fn finish(mut self) -> usize {
        self.injector.release_all()
    }
}

pub(crate) fn run<D: InputDesk, B: InputBackend>(
    mut follower: Follower<D, B>,
    inputs: &mpsc::Receiver<OsInput>,
    mut note: impl FnMut(Note),
) -> usize {
    loop {
        let input = match inputs.recv_timeout(FOLLOW_INTERVAL) {
            Ok(input) => Some(input),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return follower.finish(),
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
    use dari_proto::{DesktopName, KeyCode, MouseButton, NamedKey};

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
    fn input_is_replayed_only_off_default() {
        let (mut follower, timeline) = follower(vec![DEFAULT, WINLOGON], 0);
        assert_eq!(follower.step(Some(&OsInput::Move { x: 5, y: 6 })), []);
        assert_eq!(
            taken(&timeline),
            [Happened::Attached(InputDesktop::Default)]
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
    fn held_input_is_released_after_attaching_to_the_next_desktop() {
        let screen_saver = InputDesktop::Other(DesktopName::parse("Screen-saver").unwrap());
        let (mut follower, timeline) = follower(
            vec![WINLOGON, WINLOGON, Some(screen_saver.clone()), DEFAULT],
            0,
        );
        follower.step(Some(&alt(true)));
        follower.step(Some(&OsInput::Button {
            button: MouseButton::Left,
            pressed: true,
        }));
        taken(&timeline);

        assert_eq!(
            follower.step(Some(&alt(true))),
            [Note::Released {
                count: 2,
                on: screen_saver.clone()
            }]
        );
        assert_eq!(
            taken(&timeline),
            [
                Happened::Attached(screen_saver),
                Happened::Did(RecordedAction::Key(ALT, false)),
                Happened::Did(RecordedAction::Button(MouseButton::Left, false)),
                Happened::Did(RecordedAction::Key(ALT, true)),
            ]
        );

        assert_eq!(
            follower.step(Some(&alt(false))),
            [Note::Released {
                count: 1,
                on: InputDesktop::Default
            }]
        );
        assert_eq!(
            taken(&timeline),
            [
                Happened::Attached(InputDesktop::Default),
                Happened::Did(RecordedAction::Key(ALT, false)),
            ]
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
    fn what_was_held_before_a_failed_attach_is_released_after_the_next_one() {
        let (mut follower, timeline) = follower(vec![WINLOGON, DEFAULT, DEFAULT], 0);
        follower.step(Some(&alt(true)));
        follower.desk.failing = 1;
        follower.step(None);
        follower.step(None);
        assert_eq!(
            taken(&timeline),
            [
                Happened::Attached(InputDesktop::Winlogon),
                Happened::Did(RecordedAction::Key(ALT, true)),
                Happened::Attached(InputDesktop::Default),
                Happened::Did(RecordedAction::Key(ALT, false)),
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
            thread::spawn(move || run(follower, &received, |note| notes.lock().unwrap().push(note)))
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
        assert_eq!(thread.join().unwrap(), 2);
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
}
