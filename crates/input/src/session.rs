use open_desk_proto::{InputEvent, KeyCode, MouseButton, PointerPosition};
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

/// Applies one remote session's input and guarantees held keys are released at the end.
#[derive(Debug)]
pub struct InputSession<B: InputBackend> {
    backend: B,
    geometry: DisplayGeometry,
    held_keys: Vec<KeyCode>,
    held_buttons: Vec<MouseButton>,
}

impl<B: InputBackend> InputSession<B> {
    pub fn new(backend: B, geometry: DisplayGeometry) -> Self {
        Self {
            backend,
            geometry,
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
        }
    }

    /// Switches the target display, e.g. when the host starts streaming another monitor.
    pub fn set_geometry(&mut self, geometry: DisplayGeometry) {
        self.geometry = geometry;
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn apply(&mut self, event: &InputEvent) -> Result<(), InjectError> {
        match event {
            InputEvent::PointerMove(position) => {
                let (x, y) = self.geometry.to_os(*position);
                self.backend.move_pointer(x, y)
            }
            InputEvent::PointerButton { button, pressed } => {
                if *pressed {
                    if !self.held_buttons.contains(button) {
                        self.ensure_capacity()?;
                        self.held_buttons.push(*button);
                    }
                } else {
                    self.held_buttons.retain(|held| held != button);
                }
                self.backend.button(*button, *pressed)
            }
            InputEvent::Scroll { dx, dy } => self.backend.scroll(i32::from(*dx), i32::from(*dy)),
            InputEvent::Key { key, pressed } => {
                if *pressed {
                    if !self.held_keys.contains(key) {
                        self.ensure_capacity()?;
                        self.held_keys.push(*key);
                    }
                } else {
                    self.held_keys.retain(|held| held != key);
                }
                self.backend.key(*key, *pressed)
            }
            InputEvent::Text(text) => self.backend.text(text),
        }
    }

    /// Releases every key and button the remote side still holds, newest first.
    pub fn release_all(&mut self) {
        for key in std::mem::take(&mut self.held_keys).into_iter().rev() {
            if let Err(error) = self.backend.key(key, false) {
                debug!(%error, ?key, "failed to release key");
            }
        }
        for button in std::mem::take(&mut self.held_buttons).into_iter().rev() {
            if let Err(error) = self.backend.button(button, false) {
                debug!(%error, ?button, "failed to release button");
            }
        }
    }

    fn ensure_capacity(&self) -> Result<(), InjectError> {
        if self.held_keys.len() + self.held_buttons.len() >= MAX_HELD_INPUTS {
            Err(InjectError::Backend("too many keys held at once".into()))
        } else {
            Ok(())
        }
    }
}

impl<B: InputBackend> Drop for InputSession<B> {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use open_desk_proto::NamedKey;

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
}
