use dari_proto::{KeyCode, MouseButton, NamedKey};
#[cfg(not(windows))]
use enigo::Coordinate;
use enigo::{Axis, Button, Direction, Enigo, Key, Keyboard, Mouse, NewConError, Settings};

use crate::InjectError;

/// Performs OS-level input. Coordinates are absolute in the OS pointer space.
pub trait InputBackend {
    fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError>;
    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError>;
    /// Wheel lines; positive scrolls down/right.
    fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError>;
    fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError>;
    fn text(&mut self, text: &str) -> Result<(), InjectError>;
}

impl<T: InputBackend + ?Sized> InputBackend for Box<T> {
    fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
        (**self).move_pointer(x, y)
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
        (**self).button(button, pressed)
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
        (**self).scroll(dx, dy)
    }

    fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
        (**self).key(key, pressed)
    }

    fn text(&mut self, text: &str) -> Result<(), InjectError> {
        (**self).text(text)
    }
}

/// Injects input through the OS with enigo. Create it on the thread that will use it.
pub struct EnigoBackend {
    enigo: Enigo,
}

impl std::fmt::Debug for EnigoBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnigoBackend").finish_non_exhaustive()
    }
}

impl EnigoBackend {
    /// Connects to the OS input system. On macOS this prompts for the Accessibility permission
    /// the first time and fails with [`InjectError::PermissionDenied`] until it is granted.
    pub fn new() -> Result<Self, InjectError> {
        let settings = Settings {
            open_prompt_to_get_permissions: true,
            release_keys_when_dropped: false,
            ..Settings::default()
        };
        match Enigo::new(&settings) {
            Ok(enigo) => Ok(Self { enigo }),
            Err(NewConError::NoPermission) => Err(InjectError::PermissionDenied),
            Err(error) => Err(InjectError::Backend(error.to_string())),
        }
    }
}

impl EnigoBackend {
    /// Shows the macOS Accessibility prompt if the permission is missing.
    #[cfg(target_os = "macos")]
    pub(crate) fn prompt_for_permission() -> bool {
        let settings = Settings {
            open_prompt_to_get_permissions: true,
            release_keys_when_dropped: false,
            ..Settings::default()
        };
        Enigo::new(&settings).is_ok()
    }
}

fn direction(pressed: bool) -> Direction {
    if pressed {
        Direction::Press
    } else {
        Direction::Release
    }
}

impl From<enigo::InputError> for InjectError {
    fn from(error: enigo::InputError) -> Self {
        InjectError::Backend(error.to_string())
    }
}

impl InputBackend for EnigoBackend {
    fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
        // enigo's absolute moves only cover the primary monitor on Windows; place the cursor
        // in virtual-desktop coordinates instead.
        #[cfg(windows)]
        {
            win32::set_cursor_position(x, y)
        }
        #[cfg(not(windows))]
        {
            self.enigo
                .move_mouse(x, y, Coordinate::Abs)
                .map_err(InjectError::from)
        }
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
        let button = match button {
            MouseButton::Left => Button::Left,
            MouseButton::Right => Button::Right,
            MouseButton::Middle => Button::Middle,
            MouseButton::Back => Button::Back,
            MouseButton::Forward => Button::Forward,
        };
        self.enigo
            .button(button, direction(pressed))
            .map_err(InjectError::from)
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
        if dy != 0 {
            self.enigo
                .scroll(dy, Axis::Vertical)
                .map_err(InjectError::from)?;
        }
        if dx != 0 {
            self.enigo
                .scroll(dx, Axis::Horizontal)
                .map_err(InjectError::from)?;
        }
        Ok(())
    }

    fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
        // enigo finds a character's key through the current keyboard layout, with an API macOS
        // only allows on the main thread: in an app (the GUI) a call from this input thread
        // aborts the process. A character names a physical US-layout key, so press that key.
        #[cfg(target_os = "macos")]
        if let KeyCode::Character(character) = key {
            return match mac_key_code(character) {
                Some(code) => self
                    .enigo
                    .raw(code, direction(pressed))
                    .map_err(InjectError::from),
                // Not on a US keyboard: type the character itself, which needs no layout.
                None if pressed => self.text(&character.to_string()),
                None => Ok(()),
            };
        }
        let mapped = enigo_key(key).ok_or_else(|| InjectError::Unsupported(format!("{key:?}")))?;
        self.enigo
            .key(mapped, direction(pressed))
            .map_err(InjectError::from)
    }

    fn text(&mut self, text: &str) -> Result<(), InjectError> {
        self.enigo.text(text).map_err(InjectError::from)
    }
}

/// The macOS virtual key code (`kVK_ANSI_*`) of the US-layout key that types `character`
/// unshifted, as [`KeyCode::Character`] defines it.
#[cfg(any(target_os = "macos", test))]
fn mac_key_code(character: char) -> Option<u16> {
    Some(match character {
        'a' => 0x00,
        's' => 0x01,
        'd' => 0x02,
        'f' => 0x03,
        'h' => 0x04,
        'g' => 0x05,
        'z' => 0x06,
        'x' => 0x07,
        'c' => 0x08,
        'v' => 0x09,
        'b' => 0x0B,
        'q' => 0x0C,
        'w' => 0x0D,
        'e' => 0x0E,
        'r' => 0x0F,
        'y' => 0x10,
        't' => 0x11,
        '1' => 0x12,
        '2' => 0x13,
        '3' => 0x14,
        '4' => 0x15,
        '6' => 0x16,
        '5' => 0x17,
        '=' => 0x18,
        '9' => 0x19,
        '7' => 0x1A,
        '-' => 0x1B,
        '8' => 0x1C,
        '0' => 0x1D,
        ']' => 0x1E,
        'o' => 0x1F,
        'u' => 0x20,
        '[' => 0x21,
        'i' => 0x22,
        'p' => 0x23,
        'l' => 0x25,
        'j' => 0x26,
        '\'' => 0x27,
        'k' => 0x28,
        ';' => 0x29,
        '\\' => 0x2A,
        ',' => 0x2B,
        '/' => 0x2C,
        'n' => 0x2D,
        'm' => 0x2E,
        '.' => 0x2F,
        '`' => 0x32,
        _ => return None,
    })
}

/// The enigo key for `key`, or `None` if this OS has no such key.
fn enigo_key(key: KeyCode) -> Option<Key> {
    let named = match key {
        KeyCode::Character(character) => return Some(Key::Unicode(character)),
        KeyCode::Named(named) => named,
    };
    Some(match named {
        NamedKey::Enter => Key::Return,
        NamedKey::Tab => Key::Tab,
        NamedKey::Space => Key::Space,
        NamedKey::Backspace => Key::Backspace,
        NamedKey::Escape => Key::Escape,
        NamedKey::Delete => Key::Delete,
        NamedKey::Home => Key::Home,
        NamedKey::End => Key::End,
        NamedKey::PageUp => Key::PageUp,
        NamedKey::PageDown => Key::PageDown,
        NamedKey::ArrowUp => Key::UpArrow,
        NamedKey::ArrowDown => Key::DownArrow,
        NamedKey::ArrowLeft => Key::LeftArrow,
        NamedKey::ArrowRight => Key::RightArrow,
        NamedKey::Function(number) => function_key(number)?,
        NamedKey::Shift => Key::Shift,
        NamedKey::Control => Key::Control,
        NamedKey::Alt => Key::Alt,
        NamedKey::Meta => Key::Meta,
        NamedKey::CapsLock => Key::CapsLock,
        #[cfg(windows)]
        NamedKey::Insert => Key::Insert,
        #[cfg(windows)]
        NamedKey::PrintScreen => Key::PrintScr,
        #[cfg(windows)]
        NamedKey::Pause => Key::Pause,
        #[cfg(windows)]
        NamedKey::NumLock => Key::Numlock,
        #[cfg(windows)]
        NamedKey::HangulMode => Key::Hangul,
        #[cfg(windows)]
        NamedKey::HanjaMode => Key::Hanja,
        #[cfg(not(windows))]
        NamedKey::Insert
        | NamedKey::PrintScreen
        | NamedKey::Pause
        | NamedKey::NumLock
        | NamedKey::HangulMode
        | NamedKey::HanjaMode => return None,
    })
}

fn function_key(number: u8) -> Option<Key> {
    const KEYS: [Key; 20] = [
        Key::F1,
        Key::F2,
        Key::F3,
        Key::F4,
        Key::F5,
        Key::F6,
        Key::F7,
        Key::F8,
        Key::F9,
        Key::F10,
        Key::F11,
        Key::F12,
        Key::F13,
        Key::F14,
        Key::F15,
        Key::F16,
        Key::F17,
        Key::F18,
        Key::F19,
        Key::F20,
    ];
    KEYS.get(usize::from(number).checked_sub(1)?).copied()
}

/// One call an [`Injector`](crate::Injector) made, as seen by [`RecordingBackend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedAction {
    Move(i32, i32),
    Button(MouseButton, bool),
    Scroll(i32, i32),
    Key(KeyCode, bool),
    Text(String),
}

/// Records input instead of performing it. For tests and headless hosts.
#[derive(Debug, Default)]
pub struct RecordingBackend {
    pub actions: Vec<RecordedAction>,
}

impl InputBackend for RecordingBackend {
    fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
        self.actions.push(RecordedAction::Move(x, y));
        Ok(())
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
        self.actions.push(RecordedAction::Button(button, pressed));
        Ok(())
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
        self.actions.push(RecordedAction::Scroll(dx, dy));
        Ok(())
    }

    fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
        self.actions.push(RecordedAction::Key(key, pressed));
        Ok(())
    }

    fn text(&mut self, text: &str) -> Result<(), InjectError> {
        self.actions.push(RecordedAction::Text(text.to_owned()));
        Ok(())
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "one argument-less Accessibility API call")]
pub(crate) mod macos {
    /// Whether macOS lists this process as a trusted accessibility client. Never prompts.
    pub(crate) fn process_is_trusted() -> bool {
        // SAFETY: AXIsProcessTrusted takes no arguments and only reads process state.
        unsafe { objc2_application_services::AXIsProcessTrusted() }
    }
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "two Win32 calls with no memory-safety preconditions"
)]
pub(crate) mod win32 {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

    use crate::InjectError;

    pub(crate) fn set_cursor_position(x: i32, y: i32) -> Result<(), InjectError> {
        // SAFETY: SetCursorPos takes two integers and touches no caller memory.
        unsafe { SetCursorPos(x, y) }.map_err(|error| InjectError::Backend(error.to_string()))
    }

    pub(crate) fn enable_per_monitor_dpi_awareness() {
        // SAFETY: takes a predefined context constant and touches no caller memory. It fails
        // harmlessly when the awareness was already set (by a manifest or an earlier call).
        if let Err(error) =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        {
            tracing::debug!(%error, "DPI awareness unchanged");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unshifted_us_key_has_its_own_mac_key_code() {
        // Character keys are pressed by key code on macOS; looking one up through the layout
        // aborted the GUI host. Every key a US keyboard types without Shift needs a code.
        let keys = "`1234567890-=qwertyuiop[]\\asdfghjkl;'zxcvbnm,./";
        let mut codes: Vec<u16> = keys
            .chars()
            .map(|character| {
                mac_key_code(character).unwrap_or_else(|| panic!("no key code for {character:?}"))
            })
            .collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(
            codes.len(),
            keys.chars().count(),
            "two characters share a key code"
        );
        assert_eq!(mac_key_code('A'), None, "shifted characters are not keys");
    }

    #[test]
    fn function_keys_are_bounded() {
        assert_eq!(function_key(1), Some(Key::F1));
        assert_eq!(function_key(20), Some(Key::F20));
        assert_eq!(function_key(0), None);
        assert_eq!(function_key(21), None);
    }

    #[test]
    fn characters_map_to_unicode_keys() {
        assert_eq!(enigo_key(KeyCode::Character('q')), Some(Key::Unicode('q')));
        assert_eq!(
            enigo_key(KeyCode::Named(NamedKey::Enter)),
            Some(Key::Return)
        );
    }
}
