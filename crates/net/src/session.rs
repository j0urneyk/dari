//! Authenticated connections and their typed streams.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dari_proto::{
    AudioPacket, CONTROL_FRAME_LIMIT, ControlMessage, MessageCodec, Os, StreamKind, TransferId,
    VIDEO_FRAME_LIMIT, Validate, VideoPacket,
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::handshake::HandshakeChannel;
use crate::identity::Fingerprint;

/// Sends typed messages on one QUIC stream.
pub type MessageSender<T> = FramedWrite<quinn::SendStream, MessageCodec<T>>;
/// Receives typed, validated messages from one QUIC stream.
pub type MessageReceiver<T> = FramedRead<quinn::RecvStream, MessageCodec<T>>;

/// The authenticated peer.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub name: String,
    pub os: Os,
    pub address: SocketAddr,
    /// The host's certificate fingerprint. Only known on the viewer side.
    pub fingerprint: Option<Fingerprint>,
}

/// Marks the host's single session slot as taken until dropped.
#[derive(Debug)]
pub(crate) struct SessionSlot(Arc<AtomicBool>);

impl SessionSlot {
    pub(crate) fn try_claim(active: &Arc<AtomicBool>) -> Option<Self> {
        active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(active.clone()))
    }
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A connection whose peer proved knowledge of the access password.
#[derive(Debug)]
pub struct AuthenticatedConnection {
    pub(crate) link: SessionLink,
    pub(crate) control_sender: MessageSender<ControlMessage>,
    pub(crate) control_receiver: MessageReceiver<ControlMessage>,
}

impl AuthenticatedConnection {
    pub(crate) fn new(
        link: SessionLink,
        channel: HandshakeChannel<quinn::RecvStream, quinn::SendStream>,
    ) -> Self {
        // Switching codecs keeps any bytes already buffered past the last handshake frame.
        Self {
            link,
            control_sender: channel
                .writer
                .map_encoder(|_| MessageCodec::new(CONTROL_FRAME_LIMIT)),
            control_receiver: channel
                .reader
                .map_decoder(|_| MessageCodec::new(CONTROL_FRAME_LIMIT)),
        }
    }

    pub fn peer(&self) -> &PeerInfo {
        &self.link.peer
    }

    /// Splits the connection so control sending and receiving can run in separate tasks.
    pub fn split(
        self,
    ) -> (
        SessionLink,
        MessageSender<ControlMessage>,
        MessageReceiver<ControlMessage>,
    ) {
        (self.link, self.control_sender, self.control_receiver)
    }
}

/// Owns the underlying QUIC connection of an authenticated session.
///
/// Dropping the link closes the connection and, on the host, frees the session slot.
#[derive(Debug)]
pub struct SessionLink {
    pub(crate) connection: quinn::Connection,
    pub(crate) peer: PeerInfo,
    pub(crate) slot: Option<SessionSlot>,
    /// Viewers own a private client endpoint that must outlive the connection.
    pub(crate) endpoint: Option<quinn::Endpoint>,
    /// Keeps a relayed session's binding fresh for as long as the session lives.
    pub(crate) relay_keepalive: Option<crate::relay_client::BindingKeepalive>,
}

impl SessionLink {
    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    /// A handle for opening and accepting this session's unidirectional streams.
    pub fn streams(&self) -> SessionStreams {
        SessionStreams {
            connection: self.connection.clone(),
        }
    }

    /// Current smoothed round-trip time estimate.
    pub fn rtt(&self) -> Duration {
        self.connection.rtt()
    }

    /// Resolves when the connection is closed by either side or times out.
    pub async fn closed(&self) -> quinn::ConnectionError {
        self.connection.closed().await
    }

    /// Closes the connection immediately.
    pub fn close(&self) {
        self.connection
            .close(VarIntCode::NORMAL.into(), b"session ended");
    }
}

impl Drop for SessionLink {
    fn drop(&mut self) {
        self.close();
        if let Some(endpoint) = &self.endpoint {
            endpoint.close(VarIntCode::NORMAL.into(), b"session ended");
        }
        drop(self.slot.take());
    }
}

/// Opens and accepts an authenticated session's unidirectional streams. Cheap to clone, so
/// each task that streams media or files can hold its own; it does not keep the session alive.
#[derive(Debug, Clone)]
pub struct SessionStreams {
    connection: quinn::Connection,
}

/// Priority of file streams relative to video and control (0), so a large file doesn't delay
/// frames or input.
const FILE_STREAM_PRIORITY: i32 = -1;

impl SessionStreams {
    /// Host side: opens the stream that carries encoded video to the viewer.
    pub async fn open_video_sender(&self) -> Result<MessageSender<VideoPacket>, StreamError> {
        let stream = self.open(StreamKind::Video).await?;
        Ok(FramedWrite::new(
            stream,
            MessageCodec::new(VIDEO_FRAME_LIMIT),
        ))
    }

    /// Opens the stream that carries an accepted file's bytes. Finish it once the whole file is
    /// written; dropping it unfinished resets it, so a cut-short file never looks complete.
    pub async fn open_file_sender(&self, id: TransferId) -> Result<FileSender, StreamError> {
        let mut stream = self.open(StreamKind::File).await?;
        stream.set_priority(FILE_STREAM_PRIORITY)?;
        stream.write_u64(id.0).await?;
        Ok(FileSender(Some(stream)))
    }

    /// Waits for the next unidirectional stream the peer opens and reads its kind.
    ///
    /// A stream the peer resets before its header arrives (a file cancelled at once) is skipped;
    /// only a lost connection or an unknown kind is an error.
    pub async fn accept(&self) -> Result<IncomingStream, StreamError> {
        loop {
            let mut stream = self.connection.accept_uni().await?;
            match read_stream_header(&mut stream).await {
                Ok(StreamHeader::Video) => {
                    return Ok(IncomingStream::Video(FramedRead::new(
                        stream,
                        MessageCodec::new(VIDEO_FRAME_LIMIT),
                    )));
                }
                Ok(StreamHeader::File(id)) => {
                    return Ok(IncomingStream::File {
                        id,
                        stream: FileReceiver(stream),
                    });
                }
                Err(StreamError::Io(_)) if self.connection.close_reason().is_none() => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Host side: sends one audio packet as a datagram. When the network can't keep up, quinn
    /// drops the oldest unsent datagrams, which is what late audio deserves.
    pub fn send_audio(&self, packet: &AudioPacket) -> Result<(), StreamError> {
        let datagram = postcard::to_allocvec(packet).map_err(std::io::Error::other)?;
        self.connection
            .send_datagram(datagram.into())
            .map_err(|error| match error {
                quinn::SendDatagramError::ConnectionLost(error) => StreamError::Connection(error),
                other => StreamError::Io(std::io::Error::other(other)),
            })
    }

    /// Viewer side: the next valid audio packet from the host. Malformed datagrams are skipped.
    pub async fn receive_audio(&self) -> Result<AudioPacket, StreamError> {
        loop {
            let datagram = self.connection.read_datagram().await?;
            match postcard::from_bytes::<AudioPacket>(&datagram) {
                Ok(packet) if packet.validate().is_ok() => return Ok(packet),
                _ => tracing::debug!(len = datagram.len(), "dropping a malformed audio datagram"),
            }
        }
    }

    /// Lets the peer open up to `count` unidirectional streams at once. Hosts grant viewers
    /// none until a session allows file transfer.
    pub fn allow_peer_streams(&self, count: u32) {
        self.connection
            .set_max_concurrent_uni_streams(quinn::VarInt::from_u32(count));
    }

    async fn open(&self, kind: StreamKind) -> Result<quinn::SendStream, StreamError> {
        let mut stream = self.connection.open_uni().await?;
        stream.write_u8(kind.tag()).await?;
        Ok(stream)
    }
}

/// Writes one file's bytes. Dropped before [`FileSender::finish`], it resets the stream.
#[derive(Debug)]
pub struct FileSender(Option<quinn::SendStream>);

impl FileSender {
    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<(), StreamError> {
        if let Some(stream) = &mut self.0 {
            stream
                .write_all(bytes)
                .await
                .map_err(std::io::Error::from)?;
        }
        Ok(())
    }

    /// Marks the file complete; the receiver sees the stream end cleanly.
    pub fn finish(mut self) -> Result<(), StreamError> {
        if let Some(mut stream) = self.0.take() {
            stream
                .finish()
                .map_err(|_closed| StreamError::Io(std::io::ErrorKind::BrokenPipe.into()))?;
        }
        Ok(())
    }
}

impl Drop for FileSender {
    fn drop(&mut self) {
        if let Some(mut stream) = self.0.take() {
            // quinn finishes a dropped stream; a cancelled file must not look complete.
            let _reset = stream.reset(quinn::VarInt::from_u32(0));
        }
    }
}

/// Reads one file's bytes. Dropping it stops the stream, telling the sender to give up.
#[derive(Debug)]
pub struct FileReceiver(quinn::RecvStream);

impl FileReceiver {
    /// Reads the next bytes into `buffer`; `None` once the sender finished the file. A stream
    /// the sender reset (a cancelled file) is an error, never a clean end.
    pub async fn read(&mut self, buffer: &mut [u8]) -> Result<Option<usize>, StreamError> {
        self.0
            .read(buffer)
            .await
            .map_err(|error| StreamError::Io(error.into()))
    }
}

/// A unidirectional stream the peer opened, ready to read with the codec its kind calls for.
#[derive(Debug)]
pub enum IncomingStream {
    Video(MessageReceiver<VideoPacket>),
    /// The bytes of an accepted file, after its id. The stream ends cleanly after the last byte.
    File {
        id: TransferId,
        stream: FileReceiver,
    },
}

#[derive(Debug, Error)]
pub enum StreamError {
    #[error("connection lost: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("stream i/o failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection closed")]
    Closed(#[from] quinn::ClosedStream),
    /// The peer opened a stream whose kind this protocol version doesn't define.
    #[error("the peer opened a stream of unknown kind {0}")]
    UnknownKind(u8),
}

enum StreamHeader {
    Video,
    File(TransferId),
}

async fn read_stream_header<R: AsyncRead + Unpin>(
    stream: &mut R,
) -> Result<StreamHeader, StreamError> {
    let tag = stream.read_u8().await?;
    match StreamKind::from_tag(tag) {
        Some(StreamKind::Video) => Ok(StreamHeader::Video),
        Some(StreamKind::File) => Ok(StreamHeader::File(TransferId(stream.read_u64().await?))),
        None => Err(StreamError::UnknownKind(tag)),
    }
}

/// Application close codes.
struct VarIntCode(u32);

impl VarIntCode {
    const NORMAL: VarIntCode = VarIntCode(0);
}

impl From<VarIntCode> for quinn::VarInt {
    fn from(code: VarIntCode) -> Self {
        quinn::VarInt::from_u32(code.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn video_header_is_its_kind_alone() {
        let mut stream: &[u8] = &[StreamKind::Video.tag(), 0xaa];
        assert!(matches!(
            read_stream_header(&mut stream).await,
            Ok(StreamHeader::Video)
        ));
        assert_eq!(stream, [0xaa]);
    }

    #[tokio::test]
    async fn file_header_carries_the_transfer_id() {
        let mut stream: &[u8] = &[StreamKind::File.tag(), 0, 0, 0, 0, 0, 0, 1, 2, 0xaa];
        assert!(matches!(
            read_stream_header(&mut stream).await,
            Ok(StreamHeader::File(TransferId(0x102)))
        ));
        assert_eq!(stream, [0xaa]);
    }

    #[tokio::test]
    async fn unknown_stream_kind_is_an_error() {
        let mut stream: &[u8] = &[0xee];
        assert!(matches!(
            read_stream_header(&mut stream).await,
            Err(StreamError::UnknownKind(0xee))
        ));
    }

    #[tokio::test]
    async fn truncated_headers_are_an_error() {
        for mut stream in [&[][..], &[StreamKind::File.tag(), 0, 0][..]] {
            assert!(matches!(
                read_stream_header(&mut stream).await,
                Err(StreamError::Io(_))
            ));
        }
    }
}
