//! Secure transport for Dari.
//!
//! Hosts listen on QUIC with a self-signed device certificate. Viewers connect and both sides
//! authenticate with the host's one-time access password using SPAKE2, bound to the TLS session
//! (see [`handshake`](crate::HandshakeError)). Only then are control and media streams exposed.

mod discovery;
mod endpoint;
mod handshake;
mod identity;
mod limiter;
mod password;
mod relay_client;
mod session;
mod tls;

pub use discovery::{
    Advertisement, Browser, DiscoveryError, DiscoveryEvent, NearbyDevice, fingerprint_hint,
};
pub use endpoint::{
    ConnectError, EndpointError, HANDSHAKE_TIMEOUT, HostEndpoint, HostSettings, RelayedAcceptor,
    connect,
};
pub use handshake::HandshakeError;
pub use identity::{DeviceIdentity, Fingerprint, IdentityError};
pub use password::{AccessPassword, PASSWORD_LEN, PasswordError};
pub use relay_client::{
    RelayBinding, RelayClientError, RelayIncoming, RelayRegistration, bind_to_allocation,
    connect_via_relay,
};
pub use session::{
    AuthenticatedConnection, IncomingStream, MessageReceiver, MessageSender, PeerInfo, SessionLink,
    StreamError,
};
pub use tls::TlsConfigError;

/// TLS configuration for relay servers (used by `dari-relay`).
pub mod relay_tls {
    pub use crate::tls::relay_server_config as server_config;
}
