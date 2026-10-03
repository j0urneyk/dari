use serde::{Deserialize, Serialize};

use crate::input::InputEvent;
use crate::validate::{MAX_DEVICE_NAME_CHARS, Validate, ValidationError, validate_display_text};
use crate::version::ProtocolVersion;

/// Largest clipboard text either side sends, in bytes.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
/// Most displays a host announces.
pub const MAX_DISPLAYS: usize = 16;
/// Highest frame rate, in frames per second, either side may name.
pub const MAX_FRAME_RATE: u16 = 240;
/// The version that added [`ControlMessage::SetFrameRate`] and [`ControlMessage::FrameRate`].
/// Neither is sent to a peer that speaks an earlier version.
pub const FRAME_RATE_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 1 };

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
    // Messages added after 1.0 go below, so earlier messages keep their encoding.
    /// Viewer → host (since 1.1): the highest frame rate the viewer wants, in frames per second.
    SetFrameRate(u16),
    /// Host → viewer (since 1.1): the frame rate the host now streams at, at most what the
    /// viewer asked for.
    FrameRate(u16),
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
            ControlMessage::SetFrameRate(rate) | ControlMessage::FrameRate(rate) => {
                if (1..=MAX_FRAME_RATE).contains(rate) {
                    Ok(())
                } else {
                    Err(ValidationError::InvalidValue {
                        field: "frame rate",
                    })
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
    fn frame_rates_are_bounded() {
        for rate in [1, 60, 144, MAX_FRAME_RATE] {
            assert!(ControlMessage::SetFrameRate(rate).validate().is_ok());
            assert!(ControlMessage::FrameRate(rate).validate().is_ok());
        }
        for rate in [0, MAX_FRAME_RATE + 1, u16::MAX] {
            assert!(ControlMessage::SetFrameRate(rate).validate().is_err());
            assert!(ControlMessage::FrameRate(rate).validate().is_err());
        }
    }

    #[test]
    fn messages_from_1_0_keep_their_encoding() {
        // postcard encodes the variant index first; 1.0 peers rely on these staying put.
        let encoded = |message: &ControlMessage| postcard::to_stdvec(message).unwrap()[0];
        assert_eq!(encoded(&ControlMessage::Ping { token: 0 }), 0);
        assert_eq!(
            encoded(&ControlMessage::SetQuality(QualityPreset::Speed)),
            10
        );
        assert_eq!(encoded(&ControlMessage::Clipboard(String::new())), 11);
        assert_eq!(encoded(&ControlMessage::SetFrameRate(60)), 12);
        assert_eq!(encoded(&ControlMessage::FrameRate(60)), 13);
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
