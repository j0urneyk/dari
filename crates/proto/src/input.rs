use serde::{Deserialize, Serialize};

use crate::validate::{Validate, ValidationError, validate_display_text};

/// Longest text a single [`InputEvent::Text`] may carry.
pub const MAX_INPUT_TEXT_CHARS: usize = 256;
/// Largest scroll step, in wheel lines, per event.
pub const MAX_SCROLL_LINES: i16 = 100;
/// Highest function key the protocol carries (F1..=F20 exist on every host OS).
pub const MAX_FUNCTION_KEY: u8 = 20;

/// Pointer position relative to the captured display: `0` is the left/top edge and
/// `u16::MAX` the right/bottom edge, independent of either side's resolution or DPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointerPosition {
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Keys that do not produce a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NamedKey {
    Enter,
    Tab,
    Space,
    Backspace,
    Escape,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    /// F1..=F20.
    Function(u8),
    Shift,
    Control,
    /// Option on macOS.
    Alt,
    /// Command on macOS, the Windows key on Windows.
    Meta,
    CapsLock,
    PrintScreen,
    Pause,
    NumLock,
    /// Korean input-mode toggle (한/영).
    HangulMode,
    /// Korean Hanja conversion.
    HanjaMode,
}

/// A key on the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyCode {
    /// The key that produces this character without modifiers on a US layout, e.g. `'a'`,
    /// `'7'`, `'/'`. The host's own layout and input method decide what is finally typed, the
    /// same way a physical keyboard behaves.
    Character(char),
    Named(NamedKey),
}

/// Keyboard and pointer input sent from the viewer to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    PointerMove(PointerPosition),
    PointerButton {
        button: MouseButton,
        pressed: bool,
    },
    /// Wheel scroll in lines. Positive `dy` scrolls down, positive `dx` scrolls right.
    Scroll {
        dx: i16,
        dy: i16,
    },
    Key {
        key: KeyCode,
        pressed: bool,
    },
    /// Text that cannot be expressed as key presses.
    Text(String),
}

impl Validate for InputEvent {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            InputEvent::PointerMove(_) | InputEvent::PointerButton { .. } => Ok(()),
            InputEvent::Scroll { dx, dy } => validate_scroll(i32::from(*dx), i32::from(*dy)),
            InputEvent::Key { key, .. } => validate_key(*key),
            InputEvent::Text(text) => validate_text(text),
        }
    }
}

pub(crate) fn validate_scroll(dx: i32, dy: i32) -> Result<(), ValidationError> {
    let max = u32::from(MAX_SCROLL_LINES.unsigned_abs());
    if dx.unsigned_abs() > max || dy.unsigned_abs() > max {
        Err(ValidationError::InvalidValue { field: "scroll" })
    } else {
        Ok(())
    }
}

pub(crate) fn validate_key(key: KeyCode) -> Result<(), ValidationError> {
    match key {
        KeyCode::Character(character) if character.is_control() => {
            Err(ValidationError::InvalidValue { field: "key" })
        }
        KeyCode::Named(NamedKey::Function(number)) if number == 0 || number > MAX_FUNCTION_KEY => {
            Err(ValidationError::InvalidValue { field: "key" })
        }
        KeyCode::Character(_) | KeyCode::Named(_) => Ok(()),
    }
}

pub(crate) fn validate_text(text: &str) -> Result<(), ValidationError> {
    if text.is_empty() {
        return Err(ValidationError::InvalidValue { field: "text" });
    }
    validate_display_text("text", text, MAX_INPUT_TEXT_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_bounds_untrusted_input() {
        let ok = [
            InputEvent::Scroll { dx: 0, dy: -3 },
            InputEvent::Key {
                key: KeyCode::Character('a'),
                pressed: true,
            },
            InputEvent::Key {
                key: KeyCode::Named(NamedKey::Function(12)),
                pressed: false,
            },
            InputEvent::Text("안녕하세요".into()),
        ];
        for event in ok {
            assert!(event.validate().is_ok(), "{event:?}");
        }
        let rejected = [
            InputEvent::Scroll {
                dx: 0,
                dy: i16::MIN,
            },
            InputEvent::Key {
                key: KeyCode::Character('\u{7}'),
                pressed: true,
            },
            InputEvent::Key {
                key: KeyCode::Named(NamedKey::Function(0)),
                pressed: true,
            },
            InputEvent::Key {
                key: KeyCode::Named(NamedKey::Function(21)),
                pressed: true,
            },
            InputEvent::Text(String::new()),
            InputEvent::Text("x".repeat(MAX_INPUT_TEXT_CHARS + 1)),
            InputEvent::Text("line\nbreak".into()),
        ];
        for event in rejected {
            assert!(event.validate().is_err(), "{event:?}");
        }
    }
}
