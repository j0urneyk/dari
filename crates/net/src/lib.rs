//! Secure transport for open-desk.
//!
//! Hosts listen on QUIC with a self-signed device certificate. Viewers connect and both sides
//! authenticate with the host's one-time access password using SPAKE2, bound to the TLS session
//! (see [`handshake`](crate::HandshakeError)). Only then are control and media streams exposed.

mod endpoint;
mod handshake;
mod identity;
mod limiter;
mod password;
mod session;
mod tls;

pub use endpoint::{
    ConnectError, EndpointError, HANDSHAKE_TIMEOUT, HostEndpoint, HostSettings, connect,
};
pub use handshake::HandshakeError;
pub use identity::{DeviceIdentity, Fingerprint, IdentityError};
pub use password::{AccessPassword, PASSWORD_LEN, PasswordError};
pub use session::{AuthenticatedConnection, MessageReceiver, MessageSender, PeerInfo, SessionLink};
pub use tls::TlsConfigError;
