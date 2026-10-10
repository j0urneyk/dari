//! Host and viewer sessions on top of the secure transport.
//!
//! [`start_host`] runs the host service: it listens, manages the one-time password, and for
//! each authenticated viewer streams the screen and injects the viewer's input.
//! [`connect_viewer`] runs the viewer side: it authenticates, decodes the video into the latest
//! frame, and forwards input. Both report to the UI through event channels and never block it.

mod clipboard;
mod host;
mod host_session;
mod platform;
mod secure_desktop;
mod transfer;
mod viewer;

pub use clipboard::{ClipboardAccess, ClipboardFactory, SystemClipboard};
pub use host::{
    ApprovalDecision, ApprovalRequest, HostConfig, HostError, HostEvent, HostHandle, HostPolicy,
    RelayStatus, start_host,
};
pub use platform::{HostPlatform, SystemPlatform};
pub use secure_desktop::{SecureDesktopEvent, SecureDesktopLink};
pub use transfer::{Transfer, TransferDirection, TransferState};
pub use viewer::{
    ViewerConfig, ViewerEvent, ViewerHandle, ViewerStats, ViewerTarget, connect_viewer,
};

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEndReason {
    /// The viewer disconnected.
    ViewerLeft,
    /// The host ended the session.
    HostEnded,
    /// The host user declined the viewer (or did not answer in time).
    Declined,
    /// The connection dropped or timed out.
    ConnectionLost(String),
    /// The peer broke the protocol.
    ProtocolError(String),
}

impl SessionEndReason {
    /// Classifies a failure reading the control stream.
    pub(crate) fn from_control_error(error: &dari_proto::CodecError) -> Self {
        match error {
            dari_proto::CodecError::Io(error) => {
                SessionEndReason::ConnectionLost(error.to_string())
            }
            dari_proto::CodecError::Malformed(_) | dari_proto::CodecError::Invalid(_) => {
                SessionEndReason::ProtocolError(error.to_string())
            }
        }
    }
}

impl std::fmt::Display for SessionEndReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionEndReason::ViewerLeft => f.write_str("the viewer disconnected"),
            SessionEndReason::HostEnded => f.write_str("the host ended the session"),
            SessionEndReason::Declined => f.write_str("the host declined the session"),
            SessionEndReason::ConnectionLost(detail) => write!(f, "connection lost: {detail}"),
            SessionEndReason::ProtocolError(detail) => write!(f, "protocol error: {detail}"),
        }
    }
}
