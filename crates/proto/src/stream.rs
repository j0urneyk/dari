/// What a unidirectional stream carries, written as the stream's first byte.
///
/// The receiver reads this tag before choosing a codec, so new stream kinds can share one
/// connection without relying on the order in which streams are opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StreamKind {
    /// Host → viewer: framed [`VideoPacket`](crate::VideoPacket)s.
    Video = 1,
    /// Either direction: the bytes of one accepted file, after an 8-byte big-endian
    /// [`TransferId`](crate::TransferId).
    File = 2,
}

impl StreamKind {
    /// The byte that opens a stream of this kind.
    pub const fn tag(self) -> u8 {
        self as u8
    }

    /// The kind a stream's first byte names, or `None` for a tag this version doesn't know.
    pub const fn from_tag(tag: u8) -> Option<StreamKind> {
        match tag {
            1 => Some(StreamKind::Video),
            2 => Some(StreamKind::File),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip() {
        for kind in [StreamKind::Video, StreamKind::File] {
            assert_eq!(StreamKind::from_tag(kind.tag()), Some(kind));
        }
    }

    #[test]
    fn unknown_tags_are_rejected() {
        assert_eq!(StreamKind::from_tag(0), None);
        assert_eq!(StreamKind::from_tag(u8::MAX), None);
    }
}
