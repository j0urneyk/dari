use serde::{Deserialize, Serialize};

/// Protocol version spoken by this build.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 1 };

/// Protocol version advertised during the handshake.
///
/// Peers with the same `major` version can talk to each other; a `minor` bump only adds
/// messages that older peers never receive unless they advertised support for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub fn is_compatible_with(self, other: ProtocolVersion) -> bool {
        self.major == other.major
    }

    /// Whether a peer speaking this version understands messages added in `version`.
    pub fn understands(self, version: ProtocolVersion) -> bool {
        self.major == version.major && self.minor >= version.minor
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_follows_major_version() {
        let v1_0 = ProtocolVersion { major: 1, minor: 0 };
        let v1_3 = ProtocolVersion { major: 1, minor: 3 };
        let v2_0 = ProtocolVersion { major: 2, minor: 0 };
        assert!(v1_0.is_compatible_with(v1_3));
        assert!(!v1_0.is_compatible_with(v2_0));
    }

    #[test]
    fn later_minor_versions_understand_earlier_messages() {
        let v1_0 = ProtocolVersion { major: 1, minor: 0 };
        let v1_1 = ProtocolVersion { major: 1, minor: 1 };
        let v2_1 = ProtocolVersion { major: 2, minor: 1 };
        assert!(v1_1.understands(v1_1));
        assert!(v1_1.understands(v1_0));
        assert!(!v1_0.understands(v1_1));
        assert!(!v2_1.understands(v1_1));
    }
}
