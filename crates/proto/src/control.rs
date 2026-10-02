use serde::{Deserialize, Serialize};

use crate::input::InputEvent;
use crate::validate::{MAX_DEVICE_NAME_CHARS, Validate, ValidationError, validate_display_text};

/// Largest clipboard text either side sends, in bytes.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
/// Most displays a host announces.
pub const MAX_DISPLAYS: usize = 16;

/// Whether a host capability works right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Availability {
    Available,
    /// The host user has not granted the OS permission (macOS privacy settings).
    PermissionDenied,
    Unavailable,
    /// The host user allowed this session to view only.
    NotAllowed,
}

/// The host's report of what the viewer can expect from this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    pub screen: Availability,
    pub input: Availability,
}

/// One of the host's displays, as offered to the viewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayDescription {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Stream quality the viewer asks for; the host maps it to its own limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QualityPreset {
    /// Lower resolution and bitrate for slow networks.
    Speed,
    Balanced,
    /// Higher resolution and bitrate for fast networks.
    Quality,
}

/// Messages on the authenticated, bidirectional control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Round-trip measurement; the peer answers with `Pong` carrying the same token.
    Ping {
        token: u64,
    },
    Pong {
        token: u64,
    },
    /// Orderly end of the session.
    Disconnect,
    /// Viewer → host: keyboard or pointer input.
    Input(InputEvent),
    /// Viewer → host: the decoder lost its state; send a keyframe.
    RequestKeyframe,
    /// Host → viewer: capability status, sent at session start and whenever it changes.
    HostStatus(HostStatus),
    /// Host → viewer: the host user is being asked to allow this session.
    AwaitingApproval,
    /// Host → viewer: the host user declined the session; the connection closes next.
    Declined,
    /// Host → viewer: the displays that can be shown and the one being streamed.
    Displays {
        displays: Vec<DisplayDescription>,
        active: u32,
    },
    /// Viewer → host: stream another display.
    SelectDisplay(u32),
    /// Viewer → host: change the stream quality.
    SetQuality(QualityPreset),
    /// Either direction: the sender's clipboard text changed.
    Clipboard(String),
}

impl Validate for ControlMessage {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            ControlMessage::Input(event) => event.validate(),
            ControlMessage::Displays { displays, active } => {
                if displays.is_empty()
                    || displays.len() > MAX_DISPLAYS
                    || !displays.iter().any(|display| display.id == *active)
                {
                    return Err(ValidationError::InvalidValue { field: "displays" });
                }
                for display in displays {
                    validate_display_text("display name", &display.name, MAX_DEVICE_NAME_CHARS)?;
                }
                Ok(())
            }
            ControlMessage::Clipboard(text) => {
                if text.len() > MAX_CLIPBOARD_BYTES || text.contains('\0') {
                    Err(ValidationError::InvalidValue { field: "clipboard" })
                } else {
                    Ok(())
                }
            }
            ControlMessage::Ping { .. }
            | ControlMessage::Pong { .. }
            | ControlMessage::Disconnect
            | ControlMessage::RequestKeyframe
            | ControlMessage::HostStatus(_)
            | ControlMessage::AwaitingApproval
            | ControlMessage::Declined
            | ControlMessage::SelectDisplay(_)
            | ControlMessage::SetQuality(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u32, name: &str) -> DisplayDescription {
        DisplayDescription {
            id,
            name: name.into(),
            width: 1920,
            height: 1080,
            primary: id == 1,
        }
    }

    #[test]
    fn display_lists_must_be_consistent() {
        let ok = ControlMessage::Displays {
            displays: vec![display(1, "Built-in"), display(2, "LG")],
            active: 2,
        };
        assert!(ok.validate().is_ok());
        let unknown_active = ControlMessage::Displays {
            displays: vec![display(1, "A")],
            active: 9,
        };
        assert!(unknown_active.validate().is_err());
        let empty = ControlMessage::Displays {
            displays: Vec::new(),
            active: 1,
        };
        assert!(empty.validate().is_err());
        let spoofed = ControlMessage::Displays {
            displays: vec![display(1, "A\u{202E}B")],
            active: 1,
        };
        assert!(spoofed.validate().is_err());
    }

    #[test]
    fn clipboard_text_is_bounded() {
        assert!(
            ControlMessage::Clipboard("multi\nline\ttext".into())
                .validate()
                .is_ok()
        );
        assert!(
            ControlMessage::Clipboard("x".repeat(MAX_CLIPBOARD_BYTES + 1))
                .validate()
                .is_err()
        );
        assert!(
            ControlMessage::Clipboard("nul\0".into())
                .validate()
                .is_err()
        );
    }
}
