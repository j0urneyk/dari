//! Authenticated connections and their typed streams.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dari_proto::{
    CONTROL_FRAME_LIMIT, ControlMessage, MessageCodec, Os, VIDEO_FRAME_LIMIT, VideoPacket,
};
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

    /// Host side: opens the stream that carries encoded video to the viewer.
    pub async fn open_video_sender(
        &self,
    ) -> Result<MessageSender<VideoPacket>, quinn::ConnectionError> {
        let stream = self.connection.open_uni().await?;
        Ok(FramedWrite::new(
            stream,
            MessageCodec::new(VIDEO_FRAME_LIMIT),
        ))
    }

    /// Viewer side: waits for the host's video stream.
    pub async fn accept_video_receiver(
        &self,
    ) -> Result<MessageReceiver<VideoPacket>, quinn::ConnectionError> {
        let stream = self.connection.accept_uni().await?;
        Ok(FramedRead::new(
            stream,
            MessageCodec::new(VIDEO_FRAME_LIMIT),
        ))
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
