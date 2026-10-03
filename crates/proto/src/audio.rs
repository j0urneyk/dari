use serde::{Deserialize, Serialize};

use crate::validate::{Validate, ValidationError};

/// Largest Opus packet (RFC 6716).
pub const MAX_AUDIO_PACKET_BYTES: usize = 1276;

/// 20 ms of the host's system audio as one Opus packet (48 kHz stereo), sent host → viewer as
/// a QUIC datagram: a lost packet is concealed, never resent.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioPacket {
    /// Increases by one per packet, wrapping; lets the viewer spot gaps and late packets.
    pub sequence: u32,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for AudioPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioPacket")
            .field("sequence", &self.sequence)
            .field("data_len", &self.data.len())
            .finish()
    }
}

impl Validate for AudioPacket {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.data.is_empty() || self.data.len() > MAX_AUDIO_PACKET_BYTES {
            Err(ValidationError::InvalidValue { field: "audio" })
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_packets_are_bounded() {
        let packet = |len| AudioPacket {
            sequence: 1,
            data: vec![0; len],
        };
        assert!(packet(1).validate().is_ok());
        assert!(packet(MAX_AUDIO_PACKET_BYTES).validate().is_ok());
        assert!(packet(0).validate().is_err());
        assert!(packet(MAX_AUDIO_PACKET_BYTES + 1).validate().is_err());
    }
}
