use serde::{Deserialize, Serialize};

use crate::input::InputEvent;
use crate::validate::{Validate, ValidationError};

/// Whether a host capability works right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Availability {
    Available,
    /// The host user has not granted the OS permission (macOS privacy settings).
    PermissionDenied,
    Unavailable,
}

/// The host's report of what the viewer can expect from this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    pub screen: Availability,
    pub input: Availability,
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
}

impl Validate for ControlMessage {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            ControlMessage::Input(event) => event.validate(),
            ControlMessage::Ping { .. }
            | ControlMessage::Pong { .. }
            | ControlMessage::Disconnect
            | ControlMessage::RequestKeyframe
            | ControlMessage::HostStatus(_) => Ok(()),
        }
    }
}
