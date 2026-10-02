use serde::{Deserialize, Serialize};

use crate::validate::{Validate, ValidationError};

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
}

impl Validate for ControlMessage {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            ControlMessage::Ping { .. }
            | ControlMessage::Pong { .. }
            | ControlMessage::Disconnect => Ok(()),
        }
    }
}
