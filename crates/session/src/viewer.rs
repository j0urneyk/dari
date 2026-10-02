//! The viewer side: authenticate, decode the host's screen, and send input.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use open_desk_input::ModifierMapping;
use open_desk_media::{DecodedFrame, VideoDecoder};
use open_desk_net::{AccessPassword, ConnectError, PeerInfo, SessionLink, connect};
use open_desk_proto::{ControlMessage, HostStatus, InputEvent, Os, VideoPacket};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::SessionEndReason;

/// Input events buffered towards the host. Pointer moves arrive at display rate; anything
/// beyond this means the network is stalled and dropping is better than lagging.
const INPUT_QUEUE: usize = 512;
/// Queue slots pointer moves may not use, kept for key and button events.
const INPUT_RESERVED_FOR_KEYS: usize = 128;
/// How long a leaving viewer waits for the host to acknowledge by closing the connection.
const DISCONNECT_GRACE: Duration = Duration::from_secs(1);
/// Encoded packets waiting for the decoder.
const DECODE_QUEUE: usize = 4;

#[derive(Debug, Clone)]
pub struct ViewerConfig {
    pub address: SocketAddr,
    pub client_name: String,
    /// Translate the shortcut modifier between macOS (⌘) and Windows (Ctrl).
    pub map_shortcut_modifier: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerEvent {
    HostStatus(HostStatus),
    /// The session is over; no further events follow.
    Ended(SessionEndReason),
}

/// Viewer-side counters.
#[derive(Debug, Default)]
pub struct ViewerStats {
    pub frames_decoded: AtomicU64,
    pub bytes_received: AtomicU64,
}

enum Outgoing {
    Input(InputEvent),
    RequestKeyframe,
    Disconnect,
}

/// A connected viewer session. Dropping it disconnects.
#[derive(Debug)]
pub struct ViewerHandle {
    peer: PeerInfo,
    outgoing: mpsc::Sender<Outgoing>,
    frames: watch::Receiver<Option<Arc<DecodedFrame>>>,
    stats: Arc<ViewerStats>,
    link: Arc<SessionLink>,
    supervisor: JoinHandle<()>,
}

impl std::fmt::Debug for Outgoing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Outgoing::Input(_) => "Input",
            Outgoing::RequestKeyframe => "RequestKeyframe",
            Outgoing::Disconnect => "Disconnect",
        })
    }
}

impl ViewerHandle {
    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    /// Queues input for the host. Returns `false` if it was dropped (session over or stalled).
    ///
    /// Pointer moves are only queued while there is headroom: when the network stalls they are
    /// the ones to drop, never the key and button releases behind them.
    pub fn send_input(&self, event: InputEvent) -> bool {
        admits(&event, self.outgoing.capacity())
            && self.outgoing.try_send(Outgoing::Input(event)).is_ok()
    }

    /// The most recent decoded frame. Older frames are skipped, never queued.
    pub fn frames(&self) -> watch::Receiver<Option<Arc<DecodedFrame>>> {
        self.frames.clone()
    }

    pub fn stats(&self) -> &ViewerStats {
        &self.stats
    }

    pub fn rtt(&self) -> Duration {
        self.link.rtt()
    }

    /// Tells the host the session is over and closes the connection.
    pub fn disconnect(&self) {
        let _sent = self.outgoing.try_send(Outgoing::Disconnect);
    }
}

impl Drop for ViewerHandle {
    fn drop(&mut self) {
        self.disconnect();
        self.supervisor.abort();
        self.link.close();
    }
}

/// Whether `event` may take a queue slot when `free` slots remain.
fn admits(event: &InputEvent, free: usize) -> bool {
    !matches!(event, InputEvent::PointerMove(_)) || free > INPUT_RESERVED_FOR_KEYS
}

/// Connects to a host and starts the viewer session on the current Tokio runtime.
pub async fn connect_viewer(
    config: ViewerConfig,
    password: &AccessPassword,
) -> Result<(ViewerHandle, mpsc::UnboundedReceiver<ViewerEvent>), ConnectError> {
    let connection = connect(config.address, password, config.client_name).await?;
    let peer = connection.peer().clone();
    let mapping = config
        .map_shortcut_modifier
        .then(|| ModifierMapping::between(Os::current(), peer.os))
        .flatten();
    let (link, control_sender, control_receiver) = connection.split();
    let link = Arc::new(link);

    let (outgoing, outgoing_receiver) = mpsc::channel(INPUT_QUEUE);
    let (frame_sender, frames) = watch::channel(None);
    let (events, event_receiver) = mpsc::unbounded_channel();
    let stats = Arc::new(ViewerStats::default());

    let supervisor = tokio::spawn(supervise(
        link.clone(),
        control_sender,
        control_receiver,
        outgoing.clone(),
        outgoing_receiver,
        frame_sender,
        events,
        stats.clone(),
        mapping,
    ));
    Ok((
        ViewerHandle {
            peer,
            outgoing,
            frames,
            stats,
            link,
            supervisor,
        },
        event_receiver,
    ))
}

#[expect(
    clippy::too_many_arguments,
    reason = "wires the session's tasks together once"
)]
async fn supervise(
    link: Arc<SessionLink>,
    control_sender: open_desk_net::MessageSender<ControlMessage>,
    mut control_receiver: open_desk_net::MessageReceiver<ControlMessage>,
    outgoing: mpsc::Sender<Outgoing>,
    outgoing_receiver: mpsc::Receiver<Outgoing>,
    frames: watch::Sender<Option<Arc<DecodedFrame>>>,
    events: mpsc::UnboundedSender<ViewerEvent>,
    stats: Arc<ViewerStats>,
    mapping: Option<ModifierMapping>,
) {
    let mut writer = tokio::spawn(write_control(control_sender, outgoing_receiver, mapping));
    let mut video = Some(tokio::spawn(receive_video(
        link.clone(),
        frames,
        outgoing,
        stats,
    )));

    let reason = loop {
        tokio::select! {
            message = control_receiver.next() => match message {
                None => break SessionEndReason::ConnectionLost("control stream closed".into()),
                Some(Err(error)) => break SessionEndReason::from_control_error(&error),
                Some(Ok(ControlMessage::HostStatus(status))) => {
                    let _sent = events.send(ViewerEvent::HostStatus(status));
                }
                Some(Ok(ControlMessage::Disconnect)) => break SessionEndReason::HostEnded,
                Some(Ok(ControlMessage::Pong { .. } | ControlMessage::Ping { .. })) => {}
                Some(Ok(ControlMessage::Input(_) | ControlMessage::RequestKeyframe)) => {
                    break SessionEndReason::ProtocolError("host sent a viewer message".into());
                }
            },
            result = &mut writer => {
                break match result {
                    Ok(Ok(())) => SessionEndReason::ViewerLeft,
                    Ok(Err(error)) => SessionEndReason::ConnectionLost(error),
                    Err(error) => SessionEndReason::ConnectionLost(error.to_string()),
                };
            }
            result = async {
                match video.as_mut() {
                    Some(video) => video.await,
                    None => std::future::pending().await,
                }
            } => {
                video = None;
                match result {
                    // The host stopped streaming (e.g. capture permission revoked) but keeps the
                    // session; its HostStatus explains why.
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => break error,
                    Err(error) => break SessionEndReason::ConnectionLost(error.to_string()),
                }
            }
        }
    };
    if !writer.is_finished() {
        // Give the writer a moment to flush a pending Disconnect.
        let _flushed = tokio::time::timeout(Duration::from_millis(500), &mut writer).await;
        writer.abort();
    }
    if let Some(video) = video {
        video.abort();
    }
    if reason == SessionEndReason::ViewerLeft {
        // Closing now would discard a Disconnect still in flight; the host closes the
        // connection once it has read it.
        let _closed = tokio::time::timeout(DISCONNECT_GRACE, link.closed()).await;
    }
    link.close();
    let _sent = events.send(ViewerEvent::Ended(reason));
}

async fn write_control(
    mut control: open_desk_net::MessageSender<ControlMessage>,
    mut outgoing: mpsc::Receiver<Outgoing>,
    mapping: Option<ModifierMapping>,
) -> Result<(), String> {
    while let Some(item) = outgoing.recv().await {
        let message = match item {
            Outgoing::Input(event) => ControlMessage::Input(match mapping {
                Some(mapping) => mapping.apply(event),
                None => event,
            }),
            Outgoing::RequestKeyframe => ControlMessage::RequestKeyframe,
            Outgoing::Disconnect => {
                let _sent = control.send(&ControlMessage::Disconnect).await;
                let _closed = control.close().await;
                return Ok(());
            }
        };
        control
            .send(&message)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn receive_video(
    link: Arc<SessionLink>,
    frames: watch::Sender<Option<Arc<DecodedFrame>>>,
    outgoing: mpsc::Sender<Outgoing>,
    stats: Arc<ViewerStats>,
) -> Result<(), SessionEndReason> {
    let mut video = link
        .accept_video_receiver()
        .await
        .map_err(|error| SessionEndReason::ConnectionLost(error.to_string()))?;
    let (packets, packet_receiver) = mpsc::channel(DECODE_QUEUE);
    let decode_stats = stats.clone();
    let decoder = std::thread::Builder::new()
        .name("open-desk-decode".into())
        .spawn(move || decode_loop(packet_receiver, &frames, &outgoing, &decode_stats))
        .map_err(|error| SessionEndReason::ConnectionLost(error.to_string()))?;

    let result = loop {
        match video.next().await {
            None => break Ok(()),
            Some(Err(error)) => break Err(SessionEndReason::ProtocolError(error.to_string())),
            Some(Ok(packet)) => {
                stats
                    .bytes_received
                    .fetch_add(packet.data.len() as u64, Ordering::Relaxed);
                // Waiting here pushes back through QUIC flow control to the host, which then
                // skips frames before encoding.
                if packets.send(packet).await.is_err() {
                    break Ok(());
                }
            }
        }
    };
    drop(packets);
    let _joined = tokio::task::spawn_blocking(move || decoder.join()).await;
    result
}

fn decode_loop(
    mut packets: mpsc::Receiver<VideoPacket>,
    frames: &watch::Sender<Option<Arc<DecodedFrame>>>,
    outgoing: &mpsc::Sender<Outgoing>,
    stats: &ViewerStats,
) {
    let mut decoder = match VideoDecoder::new() {
        Ok(decoder) => decoder,
        Err(error) => {
            warn!(%error, "cannot start the video decoder");
            return;
        }
    };
    // Until a keyframe arrives, predicted frames would decode into garbage.
    let mut awaiting_keyframe = true;
    while let Some(packet) = packets.blocking_recv() {
        if awaiting_keyframe && !packet.keyframe {
            continue;
        }
        match decoder.decode(&packet.data) {
            Ok(Some(frame)) => {
                awaiting_keyframe = false;
                stats.frames_decoded.fetch_add(1, Ordering::Relaxed);
                frames.send_replace(Some(Arc::new(frame)));
            }
            Ok(None) => {}
            Err(error) => {
                debug!(%error, sequence = packet.sequence, "decode failed; requesting a keyframe");
                awaiting_keyframe = true;
                let _sent = outgoing.try_send(Outgoing::RequestKeyframe);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use open_desk_proto::{KeyCode, NamedKey, PointerPosition};

    use super::*;

    #[test]
    fn pointer_moves_leave_room_for_key_releases() {
        let movement = InputEvent::PointerMove(PointerPosition { x: 1, y: 1 });
        let release = InputEvent::Key {
            key: KeyCode::Named(NamedKey::Shift),
            pressed: false,
        };
        assert!(admits(&movement, INPUT_QUEUE));
        assert!(!admits(&movement, INPUT_RESERVED_FOR_KEYS));
        assert!(admits(&release, INPUT_RESERVED_FOR_KEYS));
        assert!(admits(&release, 1));
    }
}
