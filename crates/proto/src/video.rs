use serde::{Deserialize, Serialize};

use crate::validate::{Validate, ValidationError};

/// Largest frame dimension a host may announce (8K plus headroom).
const MAX_DIMENSION: u32 = 8192;

/// One encoded video frame, sent host → viewer on the video stream.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoPacket {
    /// Monotonic frame sequence number.
    pub sequence: u64,
    /// Capture time in microseconds since the session started.
    pub timestamp_us: u64,
    /// Whether this frame can be decoded without earlier frames.
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
    /// H.264 Annex-B bitstream.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for VideoPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoPacket")
            .field("sequence", &self.sequence)
            .field("timestamp_us", &self.timestamp_us)
            .field("keyframe", &self.keyframe)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("data_len", &self.data.len())
            .finish()
    }
}

impl Validate for VideoPacket {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.width == 0 || self.width > MAX_DIMENSION {
            return Err(ValidationError::InvalidValue { field: "width" });
        }
        if self.height == 0 || self.height > MAX_DIMENSION {
            return Err(ValidationError::InvalidValue { field: "height" });
        }
        if self.data.is_empty() {
            return Err(ValidationError::InvalidValue { field: "data" });
        }
        Ok(())
    }
}
