use std::fmt;

use dari_proto::{InputEvent, KeyCode, MouseButton, OsInput, PointerPosition};
use tracing::debug;

use crate::InjectError;
use crate::backend::InputBackend;

/// Most keys plus buttons that may be held at once. Real keyboards roll over far fewer; the cap
/// keeps a misbehaving peer from growing the held set without bound.
pub const MAX_HELD_INPUTS: usize = 32;

/// Where the captured display sits in the OS pointer coordinate space (points on macOS,
/// physical pixels on Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DisplayGeometry {
    /// Maps a normalized position onto this display, edges inclusive.
    pub fn to_os(&self, position: PointerPosition) -> (i32, i32) {
        fn axis(origin: i32, length: u32, value: u16) -> i32 {
            let span = i64::from(length.saturating_sub(1));
            let offset = (span * i64::from(value) + i64::from(u16::MAX) / 2) / i64::from(u16::MAX);
            i32::try_from(i64::from(origin) + offset).unwrap_or(origin)
        }
        (
            axis(self.x, self.width, position.x),
            axis(self.y, self.height, position.y),
        )
    }
}

/// Forwards input to a backend and remembers which keys and buttons it holds, so none stays
/// held when the injector goes away. It is an [`InputBackend`] itself.
pub struct Injector<B: InputBackend> {
    backend: B,
    held_keys: Vec<KeyCode>,
    held_buttons: Vec<MouseButton>,
}

impl<B: InputBackend> Injector<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// How many keys and buttons are held.
    pub fn held(&self) -> usize {
        self.held_keys.len() + self.held_buttons.len()
    }

    /// Releases every held key and button, newest first, and returns how many there were. A
    /// release the backend fails still counts: nothing is held afterwards.
    pub fn release_all(&mut self) -> usize {
        let released = self.held();
        for key in std::mem::take(&mut self.held_keys).into_iter().rev() {
            if let Err(error) = self.backend.key(key, false) {
                debug!(%error, "failed to release a key");
            }
        }
        for button in std::mem::take(&mut self.held_buttons).into_iter().rev() {
            if let Err(error) = self.backend.button(button, false) {
                debug!(%error, ?button, "failed to release a button");
            }
        }
        released
    }

    /// Makes the one backend call `input` describes.
    pub fn replay(&mut self, input: &OsInput) -> Result<(), InjectError> {
        match input {
            OsInput::Move { x, y } => self.move_pointer(*x, *y),
            OsInput::Button { button, pressed } => self.button(*button, *pressed),
            OsInput::Scroll { dx, dy } => self.scroll(*dx, *dy),
            OsInput::Key { key, pressed } => self.key(*key, *pressed),
            OsInput::Text(text) => self.text(text),
        }
    }

    fn ensure_capacity(&self) -> Result<(), InjectError> {
        if self.held() >= MAX_HELD_INPUTS {
            Err(InjectError::Backend("too many keys held at once".into()))
        } else {
            Ok(())
        }
    }
}

impl<B: InputBackend> InputBackend for Injector<B> {
    fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
        self.backend.move_pointer(x, y)
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
        if pressed {
            if !self.held_buttons.contains(&button) {
                self.ensure_capacity()?;
                self.held_buttons.push(button);
            }
        } else {
            self.held_buttons.retain(|held| *held != button);
        }
        self.backend.button(button, pressed)
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
        self.backend.scroll(dx, dy)
    }

    fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
        if pressed {
            if !self.held_keys.contains(&key) {
                self.ensure_capacity()?;
                self.held_keys.push(key);
            }
        } else {
            self.held_keys.retain(|held| *held != key);
        }
        self.backend.key(key, pressed)
    }

    fn text(&mut self, text: &str) -> Result<(), InjectError> {
        self.backend.text(text)
    }
}

impl<B: InputBackend> Drop for Injector<B> {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// Counts held keys instead of naming them: they may spell a password.
impl<B: InputBackend + fmt::Debug> fmt::Debug for Injector<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Injector")
            .field("backend", &self.backend)
            .field("held", &self.held())
            .finish_non_exhaustive()
    }
}

/// Applies one remote session's input and guarantees held keys are released at the end.
#[derive(Debug)]
pub struct InputSession<B: InputBackend> {
    injector: Injector<B>,
    geometry: DisplayGeometry,
}

impl<B: InputBackend> InputSession<B> {
    pub fn new(backend: B, geometry: DisplayGeometry) -> Self {
        Self {
            injector: Injector::new(backend),
            geometry,
        }
    }

    /// Switches the target display, e.g. when the host starts streaming another monitor.
    pub fn set_geometry(&mut self, geometry: DisplayGeometry) {
        self.geometry = geometry;
    }

    pub fn backend(&self) -> &B {
        self.injector.backend()
    }

    pub fn apply(&mut self, event: &InputEvent) -> Result<(), InjectError> {
        match event {
            InputEvent::PointerMove(position) => {
                let (x, y) = self.geometry.to_os(*position);
                self.injector.move_pointer(x, y)
            }
            InputEvent::PointerButton { button, pressed } => {
                self.injector.button(*button, *pressed)
            }
            InputEvent::Scroll { dx, dy } => self.injector.scroll(i32::from(*dx), i32::from(*dy)),
            InputEvent::Key { key, pressed } => self.injector.key(*key, *pressed),
            InputEvent::Text(text) => self.injector.text(text),
        }
    }

    /// Releases every key and button the remote side still holds, newest first.
    pub fn release_all(&mut self) {
        self.injector.release_all();
    }

    /// Lets `change` point the backend somewhere else, after releasing everything held through
    /// the backend as it is, so nothing stays held where input no longer goes.
    pub fn retarget(&mut self, change: impl FnOnce(&mut B)) {
        self.injector.release_all();
        change(&mut self.injector.backend);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use dari_proto::NamedKey;

    use super::*;
    use crate::backend::{RecordedAction, RecordingBackend};

    const GEOMETRY: DisplayGeometry = DisplayGeometry {
        x: -1920,
        y: 0,
        width: 1920,
        height: 1080,
    };

    fn key(key: KeyCode, pressed: bool) -> InputEvent {
        InputEvent::Key { key, pressed }
    }

    /// Records each call with the target it went to, in a log that outlives the backend.
    #[derive(Debug, Default)]
    struct Routed {
        target: u8,
        log: Rc<RefCell<Vec<(u8, RecordedAction)>>>,
    }

    impl Routed {
        fn record(&mut self, action: RecordedAction) {
            self.log.borrow_mut().push((self.target, action));
        }
    }

    impl InputBackend for Routed {
        fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
            self.record(RecordedAction::Move(x, y));
            Ok(())
        }

        fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
            self.record(RecordedAction::Button(button, pressed));
            Ok(())
        }

        fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
            self.record(RecordedAction::Scroll(dx, dy));
            Ok(())
        }

        fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
            self.record(RecordedAction::Key(key, pressed));
            Ok(())
        }

        fn text(&mut self, text: &str) -> Result<(), InjectError> {
            self.record(RecordedAction::Text(text.to_owned()));
            Ok(())
        }
    }

    #[test]
    fn pointer_positions_map_onto_the_display_edges() {
        assert_eq!(GEOMETRY.to_os(PointerPosition { x: 0, y: 0 }), (-1920, 0));
        assert_eq!(
            GEOMETRY.to_os(PointerPosition {
                x: u16::MAX,
                y: u16::MAX
            }),
            (-1, 1079)
        );
        assert_eq!(
            GEOMETRY.to_os(PointerPosition {
                x: u16::MAX / 2,
                y: u16::MAX / 2
            }),
            (-961, 539)
        );
    }

    #[test]
    fn events_reach_the_backend() {
        let mut session = InputSession::new(RecordingBackend::default(), GEOMETRY);
        session
            .apply(&InputEvent::PointerMove(PointerPosition {
                x: 0,
                y: u16::MAX,
            }))
            .unwrap();
        session.apply(&InputEvent::Scroll { dx: 0, dy: 3 }).unwrap();
        session.apply(&InputEvent::Text("한글".into())).unwrap();
        assert_eq!(
            session.backend().actions,
            vec![
                RecordedAction::Move(-1920, 1079),
                RecordedAction::Scroll(0, 3),
                RecordedAction::Text("한글".into()),
            ]
        );
    }

    #[test]
    fn held_keys_and_buttons_are_released_when_the_session_ends() {
        let ctrl = KeyCode::Named(NamedKey::Control);
        let c = KeyCode::Character('c');
        let mut session = InputSession::new(RecordingBackend::default(), GEOMETRY);
        session.apply(&key(ctrl, true)).unwrap();
        session.apply(&key(c, true)).unwrap();
        session.apply(&key(c, true)).unwrap(); // auto-repeat
        session
            .apply(&InputEvent::PointerButton {
                button: MouseButton::Left,
                pressed: true,
            })
            .unwrap();
        session.release_all();
        let actions = &session.backend().actions;
        assert_eq!(
            &actions[actions.len() - 3..],
            &[
                RecordedAction::Key(c, false),
                RecordedAction::Key(ctrl, false),
                RecordedAction::Button(MouseButton::Left, false),
            ]
        );
        // Nothing is left to release.
        let count = session.backend().actions.len();
        session.release_all();
        assert_eq!(session.backend().actions.len(), count);
    }

    #[test]
    fn released_keys_are_not_released_again() {
        let shift = KeyCode::Named(NamedKey::Shift);
        let mut session = InputSession::new(RecordingBackend::default(), GEOMETRY);
        session.apply(&key(shift, true)).unwrap();
        session.apply(&key(shift, false)).unwrap();
        let count = session.backend().actions.len();
        session.release_all();
        assert_eq!(session.backend().actions.len(), count);
    }

    #[test]
    fn held_set_is_bounded() {
        let mut session = InputSession::new(RecordingBackend::default(), GEOMETRY);
        let mut letters = ('a'..='z').chain('0'..='9');
        for _ in 0..MAX_HELD_INPUTS {
            session
                .apply(&key(KeyCode::Character(letters.next().unwrap()), true))
                .unwrap();
        }
        let overflow = key(KeyCode::Character(letters.next().unwrap()), true);
        assert!(session.apply(&overflow).is_err());
    }

    #[test]
    fn retarget_releases_through_the_old_target_before_changing_it() {
        let ctrl = KeyCode::Named(NamedKey::Control);
        let backend = Routed::default();
        let log = Rc::clone(&backend.log);
        let mut session = InputSession::new(backend, GEOMETRY);
        session.apply(&key(ctrl, true)).unwrap();
        session
            .apply(&InputEvent::PointerButton {
                button: MouseButton::Left,
                pressed: true,
            })
            .unwrap();
        session.retarget(|backend| backend.target = 1);
        session
            .apply(&InputEvent::PointerMove(PointerPosition { x: 0, y: 0 }))
            .unwrap();
        drop(session);
        assert_eq!(
            *log.borrow(),
            [
                (0, RecordedAction::Key(ctrl, true)),
                (0, RecordedAction::Button(MouseButton::Left, true)),
                (0, RecordedAction::Key(ctrl, false)),
                (0, RecordedAction::Button(MouseButton::Left, false)),
                (1, RecordedAction::Move(-1920, 0)),
            ]
        );
    }

    #[test]
    fn dropping_an_injector_releases_what_it_holds_newest_first() {
        let shift = KeyCode::Named(NamedKey::Shift);
        let a = KeyCode::Character('a');
        let backend = Routed::default();
        let log = Rc::clone(&backend.log);
        let mut injector = Injector::new(backend);
        injector.key(shift, true).unwrap();
        injector.key(a, true).unwrap();
        injector.button(MouseButton::Right, true).unwrap();
        assert_eq!(injector.held(), 3);
        log.borrow_mut().clear();
        drop(injector);
        assert_eq!(
            *log.borrow(),
            [
                (0, RecordedAction::Key(a, false)),
                (0, RecordedAction::Key(shift, false)),
                (0, RecordedAction::Button(MouseButton::Right, false)),
            ]
        );
    }

    #[test]
    fn an_injector_refuses_a_press_past_the_cap_without_injecting_it() {
        let mut injector = Injector::new(RecordingBackend::default());
        let buttons = [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Back,
            MouseButton::Forward,
        ];
        for button in buttons {
            injector.button(button, true).unwrap();
        }
        let mut letters = ('a'..='z').chain('0'..='9');
        for _ in buttons.len()..MAX_HELD_INPUTS {
            injector
                .key(KeyCode::Character(letters.next().unwrap()), true)
                .unwrap();
        }
        let injected = injector.backend().actions.len();
        let overflow = KeyCode::Character(letters.next().unwrap());
        assert!(injector.key(overflow, true).is_err());
        assert_eq!(injector.backend().actions.len(), injected);
        assert_eq!(injector.held(), MAX_HELD_INPUTS);

        injector.button(MouseButton::Left, false).unwrap();
        injector.key(overflow, true).unwrap();
        assert_eq!(injector.release_all(), MAX_HELD_INPUTS);
        assert_eq!(injector.held(), 0);
    }

    #[test]
    fn replayed_input_reaches_the_backend_and_is_released() {
        let alt = KeyCode::Named(NamedKey::Alt);
        let mut injector = Injector::new(RecordingBackend::default());
        for input in [
            OsInput::Move { x: -3840, y: 2159 },
            OsInput::Button {
                button: MouseButton::Middle,
                pressed: true,
            },
            OsInput::Scroll { dx: 2, dy: -1 },
            OsInput::Key {
                key: alt,
                pressed: true,
            },
            OsInput::Text("암호".into()),
        ] {
            injector.replay(&input).unwrap();
        }
        assert_eq!(injector.held(), 2);
        assert_eq!(injector.release_all(), 2);
        assert_eq!(
            injector.backend().actions,
            [
                RecordedAction::Move(-3840, 2159),
                RecordedAction::Button(MouseButton::Middle, true),
                RecordedAction::Scroll(2, -1),
                RecordedAction::Key(alt, true),
                RecordedAction::Text("암호".into()),
                RecordedAction::Key(alt, false),
                RecordedAction::Button(MouseButton::Middle, false),
            ]
        );
    }
}
