use dari_input::{InjectError, InputBackend, InputSession};
use dari_proto::{KeyCode, MouseButton, OsInput};
use tracing::info;

use crate::secure_desktop::SecureInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Local,
    Helper,
}

pub(crate) struct Router {
    local: Box<dyn InputBackend>,
    secure: Option<SecureInput>,
    target: Target,
}

impl Router {
    pub(crate) fn new(local: Box<dyn InputBackend>, secure: Option<SecureInput>) -> Self {
        Self {
            local,
            secure,
            target: Target::Local,
        }
    }

    fn wanted(&self) -> Target {
        if self.secure.as_ref().is_some_and(SecureInput::takes_input) {
            Target::Helper
        } else {
            Target::Local
        }
    }

    fn to_helper(&self, input: OsInput) -> Result<(), InjectError> {
        if self
            .secure
            .as_ref()
            .is_some_and(|secure| secure.send(input))
        {
            Ok(())
        } else {
            Err(InjectError::Backend(
                "the secure-desktop helper's link is gone".into(),
            ))
        }
    }
}

impl InputBackend for Router {
    fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
        match self.target {
            Target::Local => self.local.move_pointer(x, y),
            Target::Helper => self.to_helper(OsInput::Move { x, y }),
        }
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
        match self.target {
            Target::Local => self.local.button(button, pressed),
            Target::Helper => self.to_helper(OsInput::Button { button, pressed }),
        }
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
        match self.target {
            Target::Local => self.local.scroll(dx, dy),
            Target::Helper => self.to_helper(OsInput::Scroll { dx, dy }),
        }
    }

    fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
        match self.target {
            Target::Local => self.local.key(key, pressed),
            Target::Helper => self.to_helper(OsInput::Key { key, pressed }),
        }
    }

    fn text(&mut self, text: &str) -> Result<(), InjectError> {
        match self.target {
            Target::Local => self.local.text(text),
            Target::Helper => self.to_helper(OsInput::Text(text.to_owned())),
        }
    }
}

/// Called before each command, which is soon enough: Windows clears key state when the input
/// desktop switches, so a release that waits for the next event finds nothing left to undo.
pub(crate) fn follow_route(session: &mut InputSession<Router>) {
    let wanted = session.backend().wanted();
    if wanted != session.backend().target {
        info!(?wanted, "remote input changes target");
        session.retarget(|router| router.target = wanted);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests may panic")]

    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    use dari_input::{DisplayGeometry, RecordedAction};
    use dari_proto::{InputDesktop, InputEvent, NamedKey, PointerPosition};
    use futures_util::FutureExt as _;

    use super::*;
    use crate::secure_desktop::{LinkCommand, LinkDriver, SecureDesktopLink};

    const GEOMETRY: DisplayGeometry = DisplayGeometry {
        x: 100,
        y: 50,
        width: 1001,
        height: 501,
    };
    const ALT: KeyCode = KeyCode::Named(NamedKey::Alt);
    const Y: KeyCode = KeyCode::Character('y');

    struct Shared(Rc<RefCell<Vec<RecordedAction>>>);

    impl InputBackend for Shared {
        fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
            self.0.borrow_mut().push(RecordedAction::Move(x, y));
            Ok(())
        }
        fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
            self.0
                .borrow_mut()
                .push(RecordedAction::Button(button, pressed));
            Ok(())
        }
        fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
            self.0.borrow_mut().push(RecordedAction::Scroll(dx, dy));
            Ok(())
        }
        fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
            self.0.borrow_mut().push(RecordedAction::Key(key, pressed));
            Ok(())
        }
        fn text(&mut self, text: &str) -> Result<(), InjectError> {
            self.0
                .borrow_mut()
                .push(RecordedAction::Text(text.to_owned()));
            Ok(())
        }
    }

    struct Rig {
        session: InputSession<Router>,
        local: Rc<RefCell<Vec<RecordedAction>>>,
        link: SecureDesktopLink,
        driver: LinkDriver,
    }

    impl Rig {
        fn new() -> Self {
            let (link, driver) = SecureDesktopLink::pair();
            let local = Rc::default();
            let router = Router::new(Box::new(Shared(Rc::clone(&local))), Some(link.input()));
            Self {
                session: InputSession::new(router, GEOMETRY),
                local,
                link,
                driver,
            }
        }

        fn apply(&mut self, event: &InputEvent) {
            follow_route(&mut self.session);
            let _applied = self.session.apply(event);
        }

        fn local(&self) -> Vec<RecordedAction> {
            self.local.borrow_mut().drain(..).collect()
        }

        fn helper(&mut self) -> Vec<OsInput> {
            let mut received = Vec::new();
            while let Some(Some(command)) = self.driver.command().now_or_never() {
                match command {
                    LinkCommand::Input(input) => received.push(input),
                    LinkCommand::SelectDisplay(_) => {}
                }
            }
            received
        }
    }

    fn key(key: KeyCode, pressed: bool) -> InputEvent {
        InputEvent::Key { key, pressed }
    }

    fn text(text: &str) -> InputEvent {
        InputEvent::Text(text.into())
    }

    #[test]
    fn switching_to_the_helper_releases_on_the_users_desktop_first() {
        let mut rig = Rig::new();
        rig.driver.desktop_changed(InputDesktop::Default);
        rig.apply(&key(ALT, true));
        rig.apply(&InputEvent::PointerButton {
            button: MouseButton::Left,
            pressed: true,
        });
        assert_eq!(
            rig.local(),
            [
                RecordedAction::Key(ALT, true),
                RecordedAction::Button(MouseButton::Left, true)
            ]
        );

        rig.driver.desktop_changed(InputDesktop::Winlogon);
        rig.apply(&key(Y, true));
        assert_eq!(
            rig.local(),
            [
                RecordedAction::Key(ALT, false),
                RecordedAction::Button(MouseButton::Left, false)
            ]
        );
        assert_eq!(
            rig.helper(),
            [OsInput::Key {
                key: Y,
                pressed: true
            }]
        );
    }

    #[test]
    fn switching_back_releases_through_the_helper_first() {
        let mut rig = Rig::new();
        rig.driver.desktop_changed(InputDesktop::Winlogon);
        rig.apply(&key(ALT, true));
        rig.apply(&InputEvent::PointerMove(PointerPosition {
            x: 0,
            y: u16::MAX,
        }));
        assert_eq!(
            rig.helper(),
            [
                OsInput::Key {
                    key: ALT,
                    pressed: true
                },
                OsInput::Move { x: 100, y: 550 }
            ]
        );

        rig.driver.desktop_changed(InputDesktop::Default);
        rig.apply(&text("hi"));
        assert_eq!(
            rig.helper(),
            [OsInput::Key {
                key: ALT,
                pressed: false
            }]
        );
        assert_eq!(rig.local(), [RecordedAction::Text("hi".into())]);
    }

    #[test]
    fn input_on_a_desktop_other_than_winlogon_stays_local() {
        let mut rig = Rig::new();
        rig.driver.desktop_changed(InputDesktop::Winlogon);
        rig.apply(&key(ALT, true));
        assert_eq!(
            rig.helper(),
            [OsInput::Key {
                key: ALT,
                pressed: true
            }]
        );

        rig.driver
            .desktop_changed(InputDesktop::from_name("Screen-saver"));
        rig.apply(&key(Y, true));
        assert_eq!(
            rig.helper(),
            [OsInput::Key {
                key: ALT,
                pressed: false
            }]
        );
        assert_eq!(rig.local(), [RecordedAction::Key(Y, true)]);
    }

    #[test]
    fn input_stays_local_without_a_live_link() {
        let local = Rc::default();
        let mut session = InputSession::new(
            Router::new(Box::new(Shared(Rc::clone(&local))), None),
            GEOMETRY,
        );
        follow_route(&mut session);
        session.apply(&key(Y, true)).unwrap();
        assert_eq!(*local.borrow(), [RecordedAction::Key(Y, true)]);

        let mut rig = Rig::new();
        rig.apply(&text("connecting"));
        assert_eq!(rig.local(), [RecordedAction::Text("connecting".into())]);

        rig.driver.desktop_changed(InputDesktop::Winlogon);
        rig.apply(&key(ALT, true));
        assert_eq!(
            rig.helper(),
            [OsInput::Key {
                key: ALT,
                pressed: true
            }]
        );
        let Rig {
            mut session,
            local,
            link,
            driver,
        } = rig;
        drop(driver);
        follow_route(&mut session);
        session.apply(&text("ended")).unwrap();
        drop(link);
        follow_route(&mut session);
        session.apply(&text("dropped")).unwrap();
        assert_eq!(
            *local.borrow(),
            [
                RecordedAction::Text("ended".into()),
                RecordedAction::Text("dropped".into())
            ]
        );
    }

    #[test]
    fn a_dropped_link_fails_helper_input_instead_of_queueing_it() {
        let (link, mut driver) = SecureDesktopLink::pair();
        driver.desktop_changed(InputDesktop::Winlogon);
        let mut router = Router::new(Box::new(Shared(Rc::default())), Some(link.input()));
        router.target = router.wanted();
        assert_eq!(router.target, Target::Helper);
        drop(link);
        assert!(router.key(Y, true).is_err());
        assert_eq!(driver.command().now_or_never(), Some(None));
    }

    #[derive(Default)]
    struct Held(HashSet<String>);

    impl Held {
        fn press(&mut self, what: String, pressed: bool) {
            if pressed {
                self.0.insert(what);
            } else {
                self.0.remove(&what);
            }
        }

        fn local(&mut self, action: &RecordedAction) {
            match action {
                RecordedAction::Key(key, pressed) => self.press(format!("{key:?}"), *pressed),
                RecordedAction::Button(button, pressed) => {
                    self.press(format!("{button:?}"), *pressed);
                }
                RecordedAction::Move(..) | RecordedAction::Scroll(..) | RecordedAction::Text(_) => {
                }
            }
        }

        fn helper(&mut self, input: &OsInput) {
            match input {
                OsInput::Key { key, pressed } => self.press(format!("{key:?}"), *pressed),
                OsInput::Button { button, pressed } => self.press(format!("{button:?}"), *pressed),
                OsInput::Move { .. } | OsInput::Scroll { .. } | OsInput::Text(_) => {}
            }
        }
    }

    struct Sequence(u64);

    impl Sequence {
        fn below(&mut self, bound: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % bound
        }
    }

    #[test]
    fn random_routes_never_leave_input_held_on_the_side_they_left() {
        let keys = [
            ALT,
            Y,
            KeyCode::Named(NamedKey::Shift),
            KeyCode::Character('a'),
        ];
        let buttons = [MouseButton::Left, MouseButton::Right];
        let mut switches = 0;
        for seed in 1..=64u64 {
            let mut random = Sequence(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut rig = Rig::new();
            let (mut on_local, mut on_helper) = (Held::default(), Held::default());
            for _ in 0..200 {
                let event = match random.below(6) {
                    0 => {
                        let desktop = match random.below(3) {
                            0 => InputDesktop::Default,
                            1 => InputDesktop::Winlogon,
                            _ => InputDesktop::from_name("Screen-saver"),
                        };
                        rig.driver.desktop_changed(desktop);
                        continue;
                    }
                    1 | 2 => key(
                        keys[usize::try_from(random.below(4)).unwrap()],
                        random.below(2) == 0,
                    ),
                    3 => InputEvent::PointerButton {
                        button: buttons[usize::try_from(random.below(2)).unwrap()],
                        pressed: random.below(2) == 0,
                    },
                    4 => InputEvent::PointerMove(PointerPosition { x: 7, y: 9 }),
                    _ => text("x"),
                };
                let before = rig.session.backend().target;
                follow_route(&mut rig.session);
                rig.local().iter().for_each(|action| on_local.local(action));
                rig.helper()
                    .iter()
                    .for_each(|input| on_helper.helper(input));
                if rig.session.backend().target != before {
                    switches += 1;
                    let left = match before {
                        Target::Local => &on_local,
                        Target::Helper => &on_helper,
                    };
                    assert!(left.0.is_empty(), "seed {seed}: {:?} still held", left.0);
                }
                let _applied = rig.session.apply(&event);
                rig.local().iter().for_each(|action| on_local.local(action));
                rig.helper()
                    .iter()
                    .for_each(|input| on_helper.helper(input));
                let idle = match rig.session.backend().target {
                    Target::Local => &on_helper,
                    Target::Helper => &on_local,
                };
                assert!(idle.0.is_empty(), "seed {seed}: the idle side holds input");
            }
        }
        assert!(switches > 500, "only {switches} switches were exercised");
    }
}
