//! A self-hosted relay that lets Dari devices reach each other across NATs.
//!
//! Hosts register with their device certificate and get a stable nine-digit ID. A viewer asks
//! for an ID; the relay opens a UDP port per side and forwards datagrams between them once both
//! have bound with their tokens. Sessions through the relay are the normal end-to-end
//! QUIC + SPAKE2 sessions, so the relay never sees screens, input, or passwords.

mod forward;
mod ids;
mod server;

pub use dari_proto::DEFAULT_RELAY_PORT;
pub use server::{RelayConfig, RelayServer};
