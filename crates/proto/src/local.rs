//! Messages on the two local pipes between `dari.exe`, the SYSTEM service, and the SYSTEM helper
//! that answer the Windows secure desktop. They never cross the network.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::frame_section::{FrameLayout, FrameSlot};
use crate::input::{KeyCode, MouseButton, validate_key, validate_scroll, validate_text};
use crate::validate::{Validate, ValidationError, sanitize_display_text, validate_display_text};

/// Largest frame on either local pipe.
pub const LOCAL_FRAME_LIMIT: usize = 64 * 1024;
/// The service's pipe, which `DariService` creates.
pub const SERVICE_PIPE: &str = r"\\.\pipe\dari-service";
/// The access a client of either pipe asks for and the pipes' DACLs grant:
/// `FILE_GENERIC_READ | FILE_WRITE_DATA`. `GENERIC_WRITE` would include `FILE_APPEND_DATA`, which
/// on a pipe is `FILE_CREATE_PIPE_INSTANCE` and would let a client create instances of the
/// server's pipe.
pub const PIPE_CLIENT_RIGHTS: u32 = 0x0012_008b;
/// Largest pointer coordinate, of either sign, in an [`OsInput::Move`].
pub const MAX_OS_COORDINATE: i32 = 131_072;
/// Longest desktop name the helper reports.
pub const MAX_DESKTOP_NAME_CHARS: usize = 64;

const PIPE_PREFIX: &str = "dari-helper-";
/// Random bytes in a helper pipe's name.
pub const PIPE_RANDOM_BYTES: usize = 16;

/// The name of the pipe the app creates for the helper: `dari-helper-` and 32 lowercase hex
/// digits.
///
/// The SYSTEM service passes this name on, so it must never be an arbitrary path: parsing is the
/// only way to build one from outside, and deserializing goes through the parser.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HelperPipeName(String);

impl HelperPipeName {
    /// Names the pipe after random bytes, which the caller reads from the OS.
    pub fn from_random(bytes: [u8; PIPE_RANDOM_BYTES]) -> Self {
        let mut name = String::with_capacity(PIPE_PREFIX.len() + 2 * PIPE_RANDOM_BYTES);
        name.push_str(PIPE_PREFIX);
        for byte in bytes {
            name.push(hex_digit(byte >> 4));
            name.push(hex_digit(byte & 0xf));
        }
        Self(name)
    }

    pub fn parse(name: &str) -> Result<Self, ValidationError> {
        let invalid = || ValidationError::InvalidValue { field: "pipe" };
        let random = name.strip_prefix(PIPE_PREFIX).ok_or_else(invalid)?;
        if random.len() != 2 * PIPE_RANDOM_BYTES
            || !random
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(invalid());
        }
        Ok(Self(name.to_owned()))
    }

    /// The name without the `\\.\pipe\` prefix.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path to open: `\\.\pipe\dari-helper-...`.
    pub fn path(&self) -> String {
        format!(r"\\.\pipe\{}", self.0)
    }
}

fn hex_digit(value: u8) -> char {
    char::from_digit(u32::from(value), 16).unwrap_or('0')
}

impl TryFrom<String> for HelperPipeName {
    type Error = ValidationError;

    fn try_from(name: String) -> Result<Self, ValidationError> {
        Self::parse(&name)
    }
}

impl From<HelperPipeName> for String {
    fn from(name: HelperPipeName) -> Self {
        name.0
    }
}

/// Leaves the random part out of logs.
impl fmt::Debug for HelperPipeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HelperPipeName({PIPE_PREFIX}...)")
    }
}

impl Validate for HelperPipeName {
    fn validate(&self) -> Result<(), ValidationError> {
        Self::parse(&self.0).map(drop)
    }
}

/// From the app to the service, on `\\.\pipe\dari-service`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceRequest {
    /// Start a helper in the client's session that connects to `pipe`. `input` is false for a
    /// view-only session.
    StartHelper { pipe: HelperPipeName, input: bool },
    /// Raise the secure attention sequence (Ctrl+Alt+Del).
    SendSas,
}

impl Validate for ServiceRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::StartHelper { pipe, .. } => pipe.validate(),
            Self::SendSas => Ok(()),
        }
    }
}

/// From the service to the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceReply {
    HelperStarted,
    Refused(Refusal),
}

impl Validate for ServiceReply {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}

/// Why the service refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// The client isn't `dari.exe` from the service's own directory.
    NotDari,
    /// The client's session isn't an active one.
    InactiveSession,
    /// A helper that another app process asked for still runs in the client's session.
    HelperRunning,
    /// `SendSas` came from a session without a running helper.
    NoHelper,
    /// The service accepted the client but couldn't do what it asked.
    Failed,
    /// The machine's [`SecureDesktopControl`] policy is off.
    PolicyOff,
}

/// Under `HKLM`, the key that holds [`POLICY_VALUE`].
pub const POLICY_KEY: &str = r"SOFTWARE\Policies\Dari";
/// The DWORD that stores [`SecureDesktopControl`].
pub const POLICY_VALUE: &str = "SecureDesktopControl";

/// Whether this machine lets a viewer answer UAC prompts and the lock screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecureDesktopControl {
    On,
    Off,
}

impl SecureDesktopControl {
    /// `None` is a missing value. Missing or 1 is `On`; anything else is `Off`, so a garbled
    /// value fails closed. The caller reads a value of another type, or an unreadable key, as a
    /// value other than 1.
    pub fn from_stored(value: Option<u32>) -> Self {
        match value {
            None | Some(1) => Self::On,
            Some(_) => Self::Off,
        }
    }

    /// Whether a helper gets input: only when the app asked for it and the policy is on.
    pub fn helper_input(self, requested: bool) -> bool {
        requested && self == Self::On
    }

    pub fn as_stored(self) -> u32 {
        match self {
            Self::On => 1,
            Self::Off => 0,
        }
    }
}

/// One `InputBackend` call, as the app's input thread made it, for the helper to replay. `Move`
/// is in physical virtual-desktop pixels, so the helper needs no display geometry.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OsInput {
    Move {
        x: i32,
        y: i32,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    /// Wheel lines; positive scrolls down/right.
    Scroll {
        dx: i32,
        dy: i32,
    },
    Key {
        key: KeyCode,
        pressed: bool,
    },
    Text(String),
}

/// Leaves out keys and text: the lock screen's password goes through here.
impl fmt::Debug for OsInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Move { x, y } => f.debug_struct("Move").field("x", x).field("y", y).finish(),
            Self::Button { button, pressed } => f
                .debug_struct("Button")
                .field("button", button)
                .field("pressed", pressed)
                .finish(),
            Self::Scroll { dx, dy } => f
                .debug_struct("Scroll")
                .field("dx", dx)
                .field("dy", dy)
                .finish(),
            Self::Key { pressed, .. } => f
                .debug_struct("Key")
                .field("pressed", pressed)
                .finish_non_exhaustive(),
            Self::Text(_) => f.write_str("Text(..)"),
        }
    }
}

impl Validate for OsInput {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Move { x, y } => {
                let max = MAX_OS_COORDINATE.unsigned_abs();
                if x.unsigned_abs() > max || y.unsigned_abs() > max {
                    Err(ValidationError::InvalidValue { field: "move" })
                } else {
                    Ok(())
                }
            }
            Self::Button { .. } => Ok(()),
            Self::Scroll { dx, dy } => validate_scroll(*dx, *dy),
            Self::Key { key, .. } => validate_key(*key),
            Self::Text(text) => validate_text(text),
        }
    }
}

/// From the app to the helper, on the app's `dari-helper-...` pipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppToHelper {
    Input(OsInput),
    /// Capture the display with this ID, as the viewer names displays.
    SelectDisplay(u32),
    /// The credit: "I copied or discarded the frame of your last [`HelperToApp::Frame`]; that
    /// slot is yours again." The app answers every `Frame` with exactly one `RequestFrame`, and
    /// the helper starts each section with one credit and publishes one frame per credit, so it
    /// never writes the slot the app is reading. The app can't write the section, so this is
    /// its only way to say which slot it reads.
    RequestFrame,
    /// The app is ending the link: stop and close the pipe. The app reads for up to 5 seconds
    /// until the pipe closes, so it closes the handle of every [`HelperToApp::FrameSection`] the
    /// helper sent, even one it sent after this.
    Stop,
}

impl Validate for AppToHelper {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Input(input) => input.validate(),
            Self::SelectDisplay(_) | Self::RequestFrame | Self::Stop => Ok(()),
        }
    }
}

/// From the helper to the app. One thread in the helper writes every message, so the pipe's
/// order is the order things happened: a `Frame` after `DesktopChanged(x)` shows desktop `x`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelperToApp {
    DesktopChanged(InputDesktop),
    /// A read-only handle, valid in the app's process, to a shared frame section laid out per
    /// `FrameLayout::new(width, height)`. Every later `Frame` refers to this section. The helper
    /// sends a new one only while the app owns no slot, so the app may unmap the previous section
    /// as soon as it maps this one. The app's own view keeps the section alive after the helper
    /// closes its handle.
    FrameSection {
        handle: u64,
        width: u32,
        height: u32,
    },
    /// `slot` of the current section holds frame `sequence` of display `display`, with the
    /// pointer drawn in. The app owns that slot until it sends [`AppToHelper::RequestFrame`].
    /// `display` lets the app drop a frame captured before the helper read its `SelectDisplay`.
    /// `sequence` is never 0 and matches the slot's header word while the app owns the slot.
    Frame {
        display: u32,
        slot: FrameSlot,
        sequence: u64,
    },
    /// The selected display can't be captured right now. Names the display for the same reason
    /// `Frame` does. Cleared by the next `Frame` or `DesktopChanged`.
    ScreenUnavailable {
        display: u32,
    },
}

impl Validate for HelperToApp {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::DesktopChanged(desktop) => desktop.validate(),
            Self::FrameSection {
                handle,
                width,
                height,
            } => {
                if *handle == 0 {
                    return Err(ValidationError::InvalidValue { field: "handle" });
                }
                FrameLayout::new(*width, *height).map(drop)
            }
            Self::Frame { sequence, .. } => {
                if *sequence == 0 {
                    return Err(ValidationError::InvalidValue { field: "sequence" });
                }
                Ok(())
            }
            Self::ScreenUnavailable { .. } => Ok(()),
        }
    }
}

/// The input desktop the helper sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputDesktop {
    /// Where apps run.
    Default,
    /// The secure desktop: UAC prompts, the lock screen, and the Ctrl+Alt+Del screen.
    Winlogon,
    Other(DesktopName),
}

impl InputDesktop {
    /// Classifies a desktop name as Windows reports it, ignoring case as Windows does.
    pub fn from_name(name: &str) -> Self {
        if name.eq_ignore_ascii_case("Default") {
            Self::Default
        } else if name.eq_ignore_ascii_case("Winlogon") {
            Self::Winlogon
        } else {
            Self::Other(DesktopName::sanitized(name))
        }
    }
}

impl Validate for InputDesktop {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Default | Self::Winlogon => Ok(()),
            Self::Other(name) => name.validate(),
        }
    }
}

impl fmt::Display for InputDesktop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("Default"),
            Self::Winlogon => f.write_str("Winlogon"),
            Self::Other(name) => f.write_str(name.as_str()),
        }
    }
}

/// The name of a desktop other than `Default` and `Winlogon`: at most
/// [`MAX_DESKTOP_NAME_CHARS`] characters, with no control characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DesktopName(String);

impl DesktopName {
    pub fn parse(name: &str) -> Result<Self, ValidationError> {
        validate_display_text("desktop", name, MAX_DESKTOP_NAME_CHARS)?;
        Ok(Self(name.to_owned()))
    }

    /// Drops what [`DesktopName::parse`] would reject, for a name read from Windows.
    pub fn sanitized(name: &str) -> Self {
        Self(sanitize_display_text(name, MAX_DESKTOP_NAME_CHARS))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for DesktopName {
    type Error = ValidationError;

    fn try_from(name: String) -> Result<Self, ValidationError> {
        Self::parse(&name)
    }
}

impl From<DesktopName> for String {
    fn from(name: DesktopName) -> Self {
        name.0
    }
}

impl Validate for DesktopName {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_display_text("desktop", &self.0, MAX_DESKTOP_NAME_CHARS)
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use tokio_util::codec::{Decoder, Encoder};

    use super::*;
    use crate::input::{MAX_FUNCTION_KEY, MAX_INPUT_TEXT_CHARS, MAX_SCROLL_LINES, NamedKey};
    use crate::{CodecError, MessageCodec};

    fn pipe() -> HelperPipeName {
        HelperPipeName::from_random([0xab; PIPE_RANDOM_BYTES])
    }

    fn encode<T: Serialize + DeserializeOwned + Validate>(message: &T) -> BytesMut {
        let mut buffer = BytesMut::new();
        MessageCodec::<T>::new(LOCAL_FRAME_LIMIT)
            .encode(message, &mut buffer)
            .unwrap();
        buffer
    }

    fn decode<T: Serialize + DeserializeOwned + Validate>(
        mut buffer: BytesMut,
    ) -> Result<Option<T>, CodecError> {
        MessageCodec::<T>::new(LOCAL_FRAME_LIMIT).decode(&mut buffer)
    }

    fn round_trip<T>(message: &T)
    where
        T: Serialize + DeserializeOwned + Validate + PartialEq + fmt::Debug,
    {
        assert_eq!(
            decode::<T>(encode(message)).unwrap().as_ref(),
            Some(message)
        );
    }

    fn frame(prefix: &[u8], text: &str, suffix: &[u8]) -> BytesMut {
        let mut body = prefix.to_vec();
        body.extend(postcard::to_allocvec(text).unwrap());
        body.extend_from_slice(suffix);
        raw(&body)
    }

    fn raw(body: &[u8]) -> BytesMut {
        let mut frame = BytesMut::new();
        frame.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(body);
        frame
    }

    #[test]
    fn every_local_message_round_trips() {
        round_trip(&ServiceRequest::StartHelper {
            pipe: pipe(),
            input: true,
        });
        round_trip(&ServiceRequest::SendSas);
        round_trip(&ServiceReply::HelperStarted);
        for refusal in [
            Refusal::NotDari,
            Refusal::InactiveSession,
            Refusal::HelperRunning,
            Refusal::NoHelper,
            Refusal::Failed,
            Refusal::PolicyOff,
        ] {
            round_trip(&ServiceReply::Refused(refusal));
        }
        for input in [
            OsInput::Move {
                x: -MAX_OS_COORDINATE,
                y: MAX_OS_COORDINATE,
            },
            OsInput::Button {
                button: MouseButton::Forward,
                pressed: true,
            },
            OsInput::Scroll { dx: -100, dy: 100 },
            OsInput::Key {
                key: KeyCode::Named(NamedKey::Alt),
                pressed: true,
            },
            OsInput::Key {
                key: KeyCode::Character('ㅎ'),
                pressed: false,
            },
            OsInput::Text("암호 123".into()),
        ] {
            round_trip(&AppToHelper::Input(input));
        }
        round_trip(&AppToHelper::SelectDisplay(65_537));
        round_trip(&AppToHelper::RequestFrame);
        round_trip(&AppToHelper::Stop);
        round_trip(&HelperToApp::DesktopChanged(InputDesktop::Default));
        round_trip(&HelperToApp::DesktopChanged(InputDesktop::Winlogon));
        round_trip(&HelperToApp::DesktopChanged(InputDesktop::Other(
            DesktopName::parse("Screen-saver").unwrap(),
        )));
        round_trip(&HelperToApp::FrameSection {
            handle: 0x2a4,
            width: 3840,
            height: 2160,
        });
        for slot in FrameSlot::ALL {
            round_trip(&HelperToApp::Frame {
                display: 65_537,
                slot,
                sequence: u64::MAX,
            });
        }
        round_trip(&HelperToApp::ScreenUnavailable { display: 2 });
    }

    #[test]
    fn pipe_names_are_the_prefix_and_32_lowercase_hex_digits() {
        let name = HelperPipeName::from_random([
            0x00, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xff, 0x10, 0x20, 0x30, 0x40,
            0x50, 0x60,
        ]);
        assert_eq!(
            name.as_str(),
            "dari-helper-000123456789abcdefff102030405060"
        );
        assert_eq!(
            name.path(),
            r"\\.\pipe\dari-helper-000123456789abcdefff102030405060"
        );
        assert_eq!(HelperPipeName::parse(name.as_str()), Ok(name.clone()));
        for bad in [
            "",
            "dari-helper-",
            "dari-helper-000123456789ABCDEFFF102030405060",
            "dari-helper-000123456789abcdefff10203040506",
            "dari-helper-000123456789abcdefff1020304050600",
            r"dari-helper-0001234567\..\dari-service0405060",
            "dari-service",
            r"\\.\pipe\dari-helper-000123456789abcdefff102030405060",
        ] {
            assert!(HelperPipeName::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_service_cannot_be_handed_an_arbitrary_pipe_path() {
        assert!(matches!(
            decode::<ServiceRequest>(frame(&[0], r"\\.\pipe\dari-service", &[1])),
            Err(CodecError::Malformed(_))
        ));
        assert_eq!(
            decode::<ServiceRequest>(frame(&[0], pipe().as_str(), &[1])).unwrap(),
            Some(ServiceRequest::StartHelper {
                pipe: pipe(),
                input: true
            })
        );
    }

    #[test]
    fn debug_output_leaves_out_the_random_part() {
        let request = ServiceRequest::StartHelper {
            pipe: pipe(),
            input: false,
        };
        let printed = format!("{request:?}");
        assert!(!printed.contains("abab"), "{printed}");
        assert!(printed.contains("dari-helper-"), "{printed}");
    }

    #[test]
    fn desktop_names_are_bounded_and_printable() {
        let long = "d".repeat(MAX_DESKTOP_NAME_CHARS + 1);
        assert!(matches!(
            decode::<HelperToApp>(frame(&[0, 2], &long, &[])),
            Err(CodecError::Malformed(_))
        ));
        assert!(matches!(
            decode::<HelperToApp>(frame(&[0, 2], "evil\u{1b}[2J", &[])),
            Err(CodecError::Malformed(_))
        ));
        assert_eq!(
            decode::<HelperToApp>(frame(&[0, 2], &long[1..], &[])).unwrap(),
            Some(HelperToApp::DesktopChanged(InputDesktop::Other(
                DesktopName::parse(&long[1..]).unwrap()
            )))
        );
    }

    #[test]
    fn desktop_names_from_windows_are_classified() {
        assert_eq!(InputDesktop::from_name("Default"), InputDesktop::Default);
        assert_eq!(InputDesktop::from_name("default"), InputDesktop::Default);
        assert_eq!(InputDesktop::from_name("Winlogon"), InputDesktop::Winlogon);
        assert_eq!(InputDesktop::from_name("WINLOGON"), InputDesktop::Winlogon);
        let other = InputDesktop::from_name("Screen\u{7}saver");
        assert_eq!(
            other,
            InputDesktop::Other(DesktopName::parse("Screensaver").unwrap())
        );
        assert!(InputDesktop::from_name(&"x".repeat(300)).validate().is_ok());
    }

    #[test]
    fn invalid_input_fails_on_the_helper_pipe() {
        let invalid = [
            OsInput::Move {
                x: MAX_OS_COORDINATE + 1,
                y: 0,
            },
            OsInput::Move {
                x: 0,
                y: -MAX_OS_COORDINATE - 1,
            },
            OsInput::Move { x: i32::MIN, y: 0 },
            OsInput::Scroll { dx: 0, dy: 101 },
            OsInput::Scroll {
                dx: i32::MIN,
                dy: 0,
            },
            OsInput::Key {
                key: KeyCode::Named(NamedKey::Function(0)),
                pressed: true,
            },
            OsInput::Key {
                key: KeyCode::Named(NamedKey::Function(MAX_FUNCTION_KEY + 1)),
                pressed: false,
            },
            OsInput::Key {
                key: KeyCode::Character('\u{7}'),
                pressed: true,
            },
            OsInput::Text(String::new()),
            OsInput::Text("x".repeat(MAX_INPUT_TEXT_CHARS + 1)),
            OsInput::Text("line\nbreak".into()),
        ];
        for input in invalid {
            let message = AppToHelper::Input(input);
            assert!(
                matches!(
                    decode::<AppToHelper>(encode(&message)),
                    Err(CodecError::Invalid(_))
                ),
                "{message:?}"
            );
        }
        let valid = [
            OsInput::Move {
                x: MAX_OS_COORDINATE,
                y: -MAX_OS_COORDINATE,
            },
            OsInput::Scroll {
                dx: -i32::from(MAX_SCROLL_LINES),
                dy: i32::from(MAX_SCROLL_LINES),
            },
            OsInput::Key {
                key: KeyCode::Named(NamedKey::Function(MAX_FUNCTION_KEY)),
                pressed: true,
            },
            OsInput::Text("x".repeat(MAX_INPUT_TEXT_CHARS)),
        ];
        for input in valid {
            assert!(input.validate().is_ok(), "{input:?}");
        }
    }

    #[test]
    fn debug_output_leaves_out_keys_and_text() {
        let printed = format!(
            "{:?}",
            [
                AppToHelper::Input(OsInput::Key {
                    key: KeyCode::Character('q'),
                    pressed: true,
                }),
                AppToHelper::Input(OsInput::Key {
                    key: KeyCode::Named(NamedKey::Backspace),
                    pressed: false,
                }),
                AppToHelper::Input(OsInput::Text("hunter2".into())),
            ]
        );
        for secret in ["'q'", "Character", "Backspace", "Named", "hunter2"] {
            assert!(!printed.contains(secret), "{printed}");
        }
        assert!(printed.contains("Key { pressed: true, .. }"), "{printed}");
        assert!(printed.contains("Text(..)"), "{printed}");
        assert_eq!(
            format!("{:?}", OsInput::Move { x: -5, y: 7 }),
            "Move { x: -5, y: 7 }"
        );
    }

    #[test]
    fn refusals_keep_their_wire_indexes() {
        assert_eq!(postcard::to_allocvec(&Refusal::Failed).unwrap(), [4]);
        assert_eq!(
            postcard::to_allocvec(&ServiceReply::Refused(Refusal::Failed)).unwrap(),
            [1, 4]
        );
        assert_eq!(postcard::to_allocvec(&Refusal::PolicyOff).unwrap(), [5]);
    }

    #[test]
    fn a_missing_or_1_policy_is_on_and_anything_else_is_off() {
        use SecureDesktopControl::{Off, On};
        for (stored, control) in [
            (None, On),
            (Some(1), On),
            (Some(0), Off),
            (Some(2), Off),
            (Some(u32::MAX), Off),
        ] {
            assert_eq!(
                SecureDesktopControl::from_stored(stored),
                control,
                "{stored:?}"
            );
        }
        for control in [On, Off] {
            assert_eq!(
                SecureDesktopControl::from_stored(Some(control.as_stored())),
                control
            );
        }
        assert_eq!(On.as_stored(), 1);
        assert_eq!(Off.as_stored(), 0);
        assert!(On.helper_input(true));
        assert!(!On.helper_input(false));
        assert!(!Off.helper_input(true));
        assert!(!Off.helper_input(false));
    }

    #[test]
    fn frame_sections_need_a_handle_and_a_real_size() {
        for (handle, width, height) in [
            (0, 1920, 1080),
            (4, 0, 1080),
            (4, 1920, 0),
            (4, 8193, 1080),
            (4, 1920, 8193),
        ] {
            let message = HelperToApp::FrameSection {
                handle,
                width,
                height,
            };
            assert!(message.validate().is_err(), "{message:?}");
        }
        assert!(
            HelperToApp::FrameSection {
                handle: 4,
                width: 8192,
                height: 8192
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn frames_name_one_of_two_slots_and_a_nonzero_sequence() {
        assert_eq!(
            decode::<HelperToApp>(raw(&[2, 7, 1, 9])).unwrap(),
            Some(HelperToApp::Frame {
                display: 7,
                slot: FrameSlot::Second,
                sequence: 9
            })
        );
        assert!(matches!(
            decode::<HelperToApp>(raw(&[2, 7, 2, 9])),
            Err(CodecError::Malformed(_))
        ));
        assert!(matches!(
            decode::<HelperToApp>(raw(&[2, 7, 0, 0])),
            Err(CodecError::Invalid(_))
        ));
    }

    #[test]
    fn oversized_local_frames_fail() {
        let header = |len: usize| BytesMut::from(&u32::try_from(len).unwrap().to_be_bytes()[..]);
        assert!(matches!(
            decode::<AppToHelper>(header(LOCAL_FRAME_LIMIT + 1)),
            Err(CodecError::Io(_))
        ));
        assert!(
            decode::<AppToHelper>(header(LOCAL_FRAME_LIMIT))
                .unwrap()
                .is_none()
        );
    }
}
