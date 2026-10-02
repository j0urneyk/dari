use std::marker::PhantomData;

use bytes::{Bytes, BytesMut};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

use crate::validate::{Validate, ValidationError};

/// Largest handshake frame. Handshake messages are tiny; anything bigger is hostile.
pub const HANDSHAKE_FRAME_LIMIT: usize = 4 * 1024;
/// Largest control frame (input events, clipboard text, settings).
pub const CONTROL_FRAME_LIMIT: usize = 2 * 1024 * 1024;
/// Largest video frame. Keyframes of a 4K desktop at high quality stay well below this.
pub const VIDEO_FRAME_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed message: {0}")]
    Malformed(#[from] postcard::Error),
    #[error("invalid message: {0}")]
    Invalid(#[from] ValidationError),
}

/// Frames messages as a 4-byte big-endian length prefix followed by a postcard body.
///
/// Frames longer than `max_frame_len` are rejected before their body is buffered, and every
/// decoded message must pass [`Validate`] before it is handed to the caller.
#[derive(Debug)]
pub struct MessageCodec<T> {
    frames: LengthDelimitedCodec,
    message: PhantomData<fn() -> T>,
}

impl<T> MessageCodec<T> {
    pub fn new(max_frame_len: usize) -> Self {
        Self {
            frames: LengthDelimitedCodec::builder()
                .length_field_length(4)
                .max_frame_length(max_frame_len)
                .new_codec(),
            message: PhantomData,
        }
    }
}

impl<T: DeserializeOwned + Validate> Decoder for MessageCodec<T> {
    type Item = T;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<T>, CodecError> {
        let Some(frame) = self.frames.decode(src)? else {
            return Ok(None);
        };
        let message: T = postcard::from_bytes(&frame)?;
        message.validate()?;
        Ok(Some(message))
    }
}

impl<T: Serialize> Encoder<&T> for MessageCodec<T> {
    type Error = CodecError;

    fn encode(&mut self, message: &T, dst: &mut BytesMut) -> Result<(), CodecError> {
        let body = postcard::to_allocvec(message)?;
        self.frames.encode(Bytes::from(body), dst)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientHello, ControlMessage, HandshakeMessage, Os, PROTOCOL_VERSION, VideoPacket};

    fn round_trip<T>(message: &T, limit: usize) -> T
    where
        T: Serialize + DeserializeOwned + Validate,
    {
        let mut codec = MessageCodec::<T>::new(limit);
        let mut buffer = BytesMut::new();
        codec.encode(message, &mut buffer).unwrap();
        let decoded = codec.decode(&mut buffer).unwrap().unwrap();
        assert!(buffer.is_empty());
        decoded
    }

    #[test]
    fn messages_round_trip() {
        let hello = HandshakeMessage::ClientHello(ClientHello {
            version: PROTOCOL_VERSION,
            client_name: "studio-mac".into(),
            client_os: Os::MacOs,
        });
        assert_eq!(round_trip(&hello, HANDSHAKE_FRAME_LIMIT), hello);

        let ping = ControlMessage::Ping { token: 7 };
        assert_eq!(round_trip(&ping, CONTROL_FRAME_LIMIT), ping);

        let packet = VideoPacket {
            sequence: 1,
            timestamp_us: 33_000,
            keyframe: true,
            width: 1920,
            height: 1080,
            data: vec![0, 0, 0, 1, 0x67],
        };
        assert_eq!(round_trip(&packet, VIDEO_FRAME_LIMIT), packet);
    }

    #[test]
    fn partial_frames_wait_for_more_bytes() {
        let mut codec = MessageCodec::<ControlMessage>::new(CONTROL_FRAME_LIMIT);
        let mut full = BytesMut::new();
        codec
            .encode(&ControlMessage::Disconnect, &mut full)
            .unwrap();
        let mut partial = full.split_to(full.len() - 1);
        assert!(codec.decode(&mut partial).unwrap().is_none());
        partial.unsplit(full);
        assert_eq!(
            codec.decode(&mut partial).unwrap(),
            Some(ControlMessage::Disconnect)
        );
    }

    #[test]
    fn oversized_frame_is_rejected_from_its_header() {
        let mut codec = MessageCodec::<HandshakeMessage>::new(HANDSHAKE_FRAME_LIMIT);
        // Only the 4-byte header announcing a 1 GiB frame has arrived.
        let mut buffer = BytesMut::from(&(1u32 << 30).to_be_bytes()[..]);
        assert!(matches!(codec.decode(&mut buffer), Err(CodecError::Io(_))));
    }

    #[test]
    fn oversized_message_cannot_be_encoded() {
        let mut codec = MessageCodec::<HandshakeMessage>::new(HANDSHAKE_FRAME_LIMIT);
        let message = HandshakeMessage::Pake(vec![0; HANDSHAKE_FRAME_LIMIT + 1]);
        assert!(codec.encode(&message, &mut BytesMut::new()).is_err());
    }

    #[test]
    fn invalid_messages_are_rejected_after_decoding() {
        let mut codec = MessageCodec::<HandshakeMessage>::new(HANDSHAKE_FRAME_LIMIT);
        let mut buffer = BytesMut::new();
        let hello = HandshakeMessage::ClientHello(ClientHello {
            version: PROTOCOL_VERSION,
            client_name: "x".repeat(200),
            client_os: Os::Windows,
        });
        codec.encode(&hello, &mut buffer).unwrap();
        assert!(matches!(
            codec.decode(&mut buffer),
            Err(CodecError::Invalid(_))
        ));
    }

    #[test]
    fn garbage_is_malformed() {
        let mut codec = MessageCodec::<HandshakeMessage>::new(HANDSHAKE_FRAME_LIMIT);
        let mut buffer = BytesMut::new();
        buffer.extend_from_slice(&3u32.to_be_bytes());
        buffer.extend_from_slice(&[0xff, 0xff, 0xff]);
        assert!(matches!(
            codec.decode(&mut buffer),
            Err(CodecError::Malformed(_))
        ));
    }
}
