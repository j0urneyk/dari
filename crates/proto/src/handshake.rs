use serde::{Deserialize, Serialize};

use crate::validate::{MAX_DEVICE_NAME_CHARS, Validate, ValidationError, validate_display_text};
use crate::version::ProtocolVersion;

/// Length of a key-confirmation MAC (HMAC-SHA256).
pub const KEY_CONFIRMATION_LEN: usize = 32;

/// Longest SPAKE2 message we accept. Ed25519-group messages are 33 bytes.
const MAX_PAKE_MESSAGE_LEN: usize = 64;

/// Operating system a peer runs, used for keyboard modifier mapping and display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    MacOs,
    Windows,
    Linux,
    Other,
}

impl Os {
    /// The operating system this binary was compiled for.
    pub const fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        }
    }
}

/// First message of a session, sent by the viewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub version: ProtocolVersion,
    pub client_name: String,
    pub client_os: Os,
}

/// The host's answer to [`ClientHello`] when it is willing to authenticate the viewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub version: ProtocolVersion,
    pub host_name: String,
    pub host_os: Os,
}

/// Why a host refused a session. Deliberately coarse so it leaks nothing useful to an attacker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    IncompatibleVersion,
    AuthenticationFailed,
    /// The host already has an active session.
    Busy,
    /// Too many failed attempts recently; try again later.
    TooManyAttempts,
    /// The host is not currently accepting connections.
    NotAccepting,
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RejectReason::IncompatibleVersion => "the remote device runs an incompatible version",
            RejectReason::AuthenticationFailed => "the password was not accepted",
            RejectReason::Busy => "the remote device is already in a session",
            RejectReason::TooManyAttempts => "too many failed attempts, try again later",
            RejectReason::NotAccepting => "the remote device is not accepting connections",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthOutcome {
    Accepted,
    Rejected(RejectReason),
}

/// Messages exchanged on the handshake stream before a session is authenticated.
///
/// Sequence: viewer `ClientHello` → host `ServerHello` (or `Outcome(Rejected)`),
/// viewer `Pake` → host `Pake`, viewer `Confirmation` → host `Confirmation` + `Outcome`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandshakeMessage {
    ClientHello(ClientHello),
    ServerHello(ServerHello),
    Pake(Vec<u8>),
    Confirmation([u8; KEY_CONFIRMATION_LEN]),
    Outcome(AuthOutcome),
}

impl Validate for HandshakeMessage {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            HandshakeMessage::ClientHello(hello) => {
                validate_display_text("client_name", &hello.client_name, MAX_DEVICE_NAME_CHARS)
            }
            HandshakeMessage::ServerHello(hello) => {
                validate_display_text("host_name", &hello.host_name, MAX_DEVICE_NAME_CHARS)
            }
            HandshakeMessage::Pake(message) => {
                if message.is_empty() || message.len() > MAX_PAKE_MESSAGE_LEN {
                    Err(ValidationError::InvalidValue { field: "pake" })
                } else {
                    Ok(())
                }
            }
            HandshakeMessage::Confirmation(_) | HandshakeMessage::Outcome(_) => Ok(()),
        }
    }
}
