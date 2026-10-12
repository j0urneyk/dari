//! Translates GPUI keyboard events into protocol key events.

#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

use dari_proto::{InputEvent, KeyCode, MAX_FUNCTION_KEY, NamedKey};
use gpui_kit::{Keystroke, Modifiers};

/// The protocol key for a GPUI keystroke, or `None` for keys the protocol does not carry.
///
/// GPUI reports `key` as the character printed on the key (the ASCII equivalent on non-Latin
/// layouts), which is exactly what the host needs to replay the physical key press and let its
/// own layout and input method produce the final character.
pub(crate) fn key_code(keystroke: &Keystroke) -> Option<KeyCode> {
    let key = keystroke.key.as_str();
    let named = match key {
        "enter" => NamedKey::Enter,
        "tab" => NamedKey::Tab,
        "space" => NamedKey::Space,
        "backspace" => NamedKey::Backspace,
        "escape" => NamedKey::Escape,
        "delete" => NamedKey::Delete,
        "insert" => NamedKey::Insert,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "pageup" => NamedKey::PageUp,
        "pagedown" => NamedKey::PageDown,
        "up" => NamedKey::ArrowUp,
        "down" => NamedKey::ArrowDown,
        "left" => NamedKey::ArrowLeft,
        "right" => NamedKey::ArrowRight,
        _ => {
            if let Some(number) = key
                .strip_prefix('f')
                .and_then(|digits| digits.parse::<u8>().ok())
            {
                return (1..=MAX_FUNCTION_KEY)
                    .contains(&number)
                    .then_some(KeyCode::Named(NamedKey::Function(number)));
            }
            let mut characters = key.chars();
            let character = characters.next()?;
            if characters.next().is_some() || character.is_control() {
                return None;
            }
            return Some(KeyCode::Character(character.to_ascii_lowercase()));
        }
    };
    Some(KeyCode::Named(named))
}

/// Modifier key events that turn `previous` into `current`. Releases come before presses so a
/// quick switch between modifiers never holds both on the host.
pub(crate) fn modifier_changes(previous: Modifiers, current: Modifiers) -> Vec<InputEvent> {
    let pairs = [
        (previous.shift, current.shift, NamedKey::Shift),
        (previous.control, current.control, NamedKey::Control),
        (previous.alt, current.alt, NamedKey::Alt),
        (previous.platform, current.platform, NamedKey::Meta),
    ];
    let releases = pairs
        .iter()
        .filter(|(was, now, _)| *was && !*now)
        .map(|(_, _, key)| (*key, false));
    let presses = pairs
        .iter()
        .filter(|(was, now, _)| !*was && *now)
        .map(|(_, _, key)| (*key, true));
    releases
        .chain(presses)
        .map(|(key, pressed)| InputEvent::Key {
            key: KeyCode::Named(key),
            pressed,
        })
        .collect()
}

/// When the HID system last saw a key go down and a modifier change. Its state counts keys macOS
/// consumed before they reached the window, and it doesn't wait for the main thread.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
pub struct HidEvents {
    pub key_down: Option<Instant>,
    pub modifier_change: Instant,
}

#[cfg(target_os = "macos")]
impl HidEvents {
    pub(crate) fn now() -> Self {
        use objc2_core_graphics::{CGEventSource, CGEventSourceStateID, CGEventType};

        let now = Instant::now();
        let last = |event| {
            let seconds = CGEventSource::seconds_since_last_event_type(
                CGEventSourceStateID::HIDSystemState,
                event,
            );
            Duration::try_from_secs_f64(seconds)
                .ok()
                .and_then(|age| now.checked_sub(age))
        };
        Self {
            key_down: last(CGEventType::KeyDown),
            modifier_change: last(CGEventType::FlagsChanged).unwrap_or(now),
        }
    }

    /// For a release the viewer makes itself, such as on focus loss. The modifiers can still be
    /// down, so the last HID modifier change can be their press, while the release happens now.
    pub(crate) fn released_by_viewer(self) -> Self {
        Self {
            modifier_change: Instant::now(),
            ..self
        }
    }
}

/// macOS keeps some chords for itself (Ctrl+Space, ⌘Space), so the window sees only their
/// modifiers go down and up. Forwarded alone, that is a lone modifier tap, and Windows opens Start
/// on a lone Win tap, which is what the Mac's Control becomes once shortcuts are translated.
#[cfg(target_os = "macos")]
pub(crate) struct Chord {
    started: Instant,
    key_sent: bool,
}

#[cfg(target_os = "macos")]
impl Chord {
    pub(crate) fn new(started: Instant) -> Self {
        Self {
            started,
            key_sent: false,
        }
    }

    pub(crate) fn key_sent(&mut self) {
        self.key_sent = true;
    }

    pub(crate) fn consumed(&self, hid: HidEvents) -> bool {
        !self.key_sent
            && hid
                .key_down
                .is_some_and(|key_down| self.started < key_down && key_down < hid.modifier_change)
    }
}

/// `-[NSApplication sendEvent:]` never hands a window the release of a key let go while ⌘ is down,
/// so ⌘A would leave A pressed on the host until the window lost focus.
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "installs an AppKit event monitor")]
pub fn deliver_command_key_ups() {
    use std::ptr::NonNull;

    use block2::RcBlock;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags};

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit passes a live event for the length of the call.
        let key_up = unsafe { event.as_ref() };
        if key_up
            .modifierFlags()
            .contains(NSEventModifierFlags::Command)
            && let Some(window) = key_up.window(main_thread)
        {
            window.sendEvent(key_up);
            return std::ptr::null_mut();
        }
        event.as_ptr()
    });
    // SAFETY: the handler returns the event AppKit passed it, or null.
    let monitor = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyUp, &handler)
    };
    std::mem::forget(monitor);
}

/// F20, which no shortcut binds on the host, so tapping it only turns a modifier tap into a chord.
#[cfg(target_os = "macos")]
pub(crate) const CHORD_MASK: [InputEvent; 2] = [
    InputEvent::Key {
        key: KeyCode::Named(NamedKey::Function(MAX_FUNCTION_KEY)),
        pressed: true,
    },
    InputEvent::Key {
        key: KeyCode::Named(NamedKey::Function(MAX_FUNCTION_KEY)),
        pressed: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(key: &str) -> Keystroke {
        Keystroke {
            key: key.into(),
            ..Keystroke::default()
        }
    }

    #[test]
    fn named_and_character_keys_map() {
        assert_eq!(
            key_code(&keystroke("enter")),
            Some(KeyCode::Named(NamedKey::Enter))
        );
        assert_eq!(
            key_code(&keystroke("left")),
            Some(KeyCode::Named(NamedKey::ArrowLeft))
        );
        assert_eq!(
            key_code(&keystroke("f12")),
            Some(KeyCode::Named(NamedKey::Function(12)))
        );
        assert_eq!(key_code(&keystroke("a")), Some(KeyCode::Character('a')));
        assert_eq!(key_code(&keystroke("A")), Some(KeyCode::Character('a')));
        assert_eq!(key_code(&keystroke("/")), Some(KeyCode::Character('/')));
    }

    #[test]
    fn unsupported_keys_are_skipped() {
        assert_eq!(key_code(&keystroke("f24")), None);
        assert_eq!(key_code(&keystroke("f0")), None);
        assert_eq!(key_code(&keystroke("")), None);
        assert_eq!(key_code(&keystroke("unknownkey")), None);
    }

    #[test]
    fn modifier_changes_release_before_press() {
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        let command = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        assert_eq!(
            modifier_changes(shift, command),
            vec![
                InputEvent::Key {
                    key: KeyCode::Named(NamedKey::Shift),
                    pressed: false
                },
                InputEvent::Key {
                    key: KeyCode::Named(NamedKey::Meta),
                    pressed: true
                },
            ]
        );
        assert_eq!(modifier_changes(shift, shift), Vec::new());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_release_on_focus_loss_masks_a_key_consumed_while_held() {
        let ago = |millis| Instant::now().checked_sub(Duration::from_millis(millis));
        let pressed = ago(30).unwrap();
        let still_held = HidEvents {
            key_down: ago(15),
            modifier_change: pressed,
        };
        let chord = Chord::new(pressed);
        assert!(!chord.consumed(still_held));
        assert!(chord.consumed(still_held.released_by_viewer()));
    }
}
