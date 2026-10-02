//! Messages between devices and a relay server (rendezvous and UDP forwarding).
//!
//! The relay never sees session contents: after rendezvous, host and viewer run their normal
//! QUIC + SPAKE2 session end to end through UDP ports the relay forwards blindly.

use serde::{Deserialize, Serialize};

use crate::validate::{Validate, ValidationError};

/// UDP port relays listen on unless configured otherwise.
pub const DEFAULT_RELAY_PORT: u16 = 47822;
/// ALPN of the relay control protocol.
pub const RELAY_ALPN: &[u8] = b"dari-relay/1";
/// Prefix of the datagram each side sends to bind its address to an allocation.
pub const RELAY_BIND_MAGIC: &[u8; 4] = b"DRRB";
/// Prefix of the relay's reply confirming a binding datagram.
pub const RELAY_ACK_MAGIC: &[u8; 4] = b"DRRA";
/// Length of an allocation token.
pub const RELAY_TOKEN_LEN: usize = 16;

/// A host's public relay ID: nine digits, shown as `123 456 789`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(u32);

impl DeviceId {
    pub const MIN: u32 = 100_000_000;
    pub const MAX: u32 = 999_999_999;

    pub fn new(value: u32) -> Option<Self> {
        (Self::MIN..=Self::MAX)
            .contains(&value)
            .then_some(Self(value))
    }

    pub fn value(self) -> u32 {
        self.0
    }

    /// Parses what a person typed: nine digits, ignoring spaces and dashes.
    pub fn parse(input: &str) -> Option<Self> {
        let digits: String = input
            .chars()
            .filter(|character| !matches!(character, ' ' | '-'))
            .collect();
        if digits.len() != 9 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Self::new(digits.parse().ok()?)
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = self.0;
        write!(
            f,
            "{:03} {:03} {:03}",
            value / 1_000_000,
            value / 1_000 % 1_000,
            value % 1_000
        )
    }
}

/// Sent by a device to the relay on its control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayRequest {
    /// Host: announce availability under the ID bound to its client certificate.
    Register,
    /// Viewer: reach the host with this ID.
    Connect { id: DeviceId },
}

/// Why the relay refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayError {
    /// No host with that ID is online.
    NotFound,
    /// Too many requests from this address or for this host; try again later.
    TooManyRequests,
    /// Registration needs a client certificate.
    CertificateRequired,
    /// The relay is out of capacity.
    Unavailable,
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RelayError::NotFound => "no device with that ID is online",
            RelayError::TooManyRequests => "too many requests, try again later",
            RelayError::CertificateRequired => "the relay needs a device certificate",
            RelayError::Unavailable => "the relay is busy",
        })
    }
}

/// A UDP port pair the relay forwards between, as seen by one side.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allocation {
    /// The relay port this side sends to (same host as the relay's control address).
    pub port: u16,
    /// Proves this side's binding datagram: `RELAY_BIND_MAGIC || token`.
    pub token: [u8; RELAY_TOKEN_LEN],
}

impl std::fmt::Debug for Allocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Allocation")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Allocation {
    /// The datagram that binds a sender's address to this allocation.
    pub fn binding_datagram(&self) -> [u8; RELAY_BIND_MAGIC.len() + RELAY_TOKEN_LEN] {
        tagged(*RELAY_BIND_MAGIC, &self.token)
    }

    /// The relay's confirmation that the binding datagram arrived.
    pub fn ack_datagram(&self) -> [u8; RELAY_ACK_MAGIC.len() + RELAY_TOKEN_LEN] {
        tagged(*RELAY_ACK_MAGIC, &self.token)
    }
}

fn tagged(magic: [u8; 4], token: &[u8; RELAY_TOKEN_LEN]) -> [u8; 4 + RELAY_TOKEN_LEN] {
    let mut datagram = [0u8; 4 + RELAY_TOKEN_LEN];
    datagram[..4].copy_from_slice(&magic);
    datagram[4..].copy_from_slice(token);
    datagram
}

/// Sent by the relay on a device's control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayResponse {
    /// Host: registered under this ID.
    Registered {
        id: DeviceId,
    },
    /// Host: a viewer wants to connect; bind to this allocation and accept QUIC on it.
    Incoming(Allocation),
    /// Viewer: connect through this allocation.
    Allocated(Allocation),
    Refused(RelayError),
}

impl Validate for RelayRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}

impl Validate for RelayResponse {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            RelayResponse::Incoming(allocation) | RelayResponse::Allocated(allocation)
                if allocation.port == 0 =>
            {
                Err(ValidationError::InvalidValue { field: "port" })
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_parse_and_display() {
        let id = DeviceId::parse("123 456 789").unwrap();
        assert_eq!(id.value(), 123_456_789);
        assert_eq!(id.to_string(), "123 456 789");
        assert_eq!(DeviceId::parse("123-456-789"), Some(id));
        assert_eq!(DeviceId::parse("12345678"), None);
        assert_eq!(DeviceId::parse("012345678"), None, "IDs never start with 0");
        assert_eq!(DeviceId::parse("12345678x"), None);
        assert_eq!(DeviceId::new(5), None);
    }

    #[test]
    fn binding_datagram_is_magic_then_token() {
        let allocation = Allocation {
            port: 5000,
            token: [7; RELAY_TOKEN_LEN],
        };
        let datagram = allocation.binding_datagram();
        assert_eq!(&datagram[..4], RELAY_BIND_MAGIC);
        assert_eq!(&datagram[4..], &[7; RELAY_TOKEN_LEN]);
        assert!(
            !format!("{allocation:?}").contains('7'),
            "tokens are not printed"
        );
    }
}
