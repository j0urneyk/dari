use std::io;
use std::time::Instant;

use bytes::BytesMut;
use dari_proto::{LOCAL_FRAME_LIMIT, MessageCodec, Validate};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::codec::{Decoder, Encoder};

use crate::win32::Pipe;

/// Reads one kind of message from a pipe, keeping bytes that arrived past the last message.
#[derive(Debug)]
pub(crate) struct MessageReader<T> {
    codec: MessageCodec<T>,
    buffer: BytesMut,
}

impl<T: DeserializeOwned + Validate> MessageReader<T> {
    pub(crate) fn new() -> Self {
        Self {
            codec: MessageCodec::new(LOCAL_FRAME_LIMIT),
            buffer: BytesMut::new(),
        }
    }

    /// The next valid message, or `None` once the other end closed. Gives up with `TimedOut` at
    /// `deadline`, however slowly the bytes trickle in.
    pub(crate) fn read(&mut self, pipe: &Pipe, deadline: Option<Instant>) -> io::Result<Option<T>> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(message) = self
                .codec
                .decode(&mut self.buffer)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            {
                return Ok(Some(message));
            }
            let timeout = match deadline {
                Some(deadline) => Some(
                    deadline
                        .checked_duration_since(Instant::now())
                        .ok_or(io::ErrorKind::TimedOut)?,
                ),
                None => None,
            };
            let read = pipe.read(&mut chunk, timeout)?;
            if read == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

pub(crate) fn write_message<T: Serialize>(
    pipe: &Pipe,
    message: &T,
    deadline: Option<Instant>,
) -> io::Result<()> {
    let mut frame = BytesMut::new();
    MessageCodec::<T>::new(LOCAL_FRAME_LIMIT)
        .encode(message, &mut frame)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let timeout = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
    pipe.write_all(&frame, timeout)
}
