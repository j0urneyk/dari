//! The viewer side: authenticate, decode the host's screen, and send input.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dari_input::ModifierMapping;
use dari_media::{DecodedFrame, VideoDecoder};
use dari_net::{
    AccessPassword, ConnectError, FileReceiver, IncomingStream, MessageReceiver, PeerInfo,
    SessionLink, SessionStreams, StreamError, connect, connect_via_relay,
};
use dari_proto::{
    Availability, ControlMessage, DeviceId, DisplayDescription, HostStatus, InputEvent, Os,
    QualityPreset, TransferId, VideoPacket,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::SessionEndReason;
use crate::clipboard::{ClipboardFactory, ClipboardSync};
use crate::transfer::{Transfer, TransferCommand, TransferPolicy, TransferStep, Transfers};

/// Input events buffered towards the host. Pointer moves arrive at display rate; anything
/// beyond this means the network is stalled and dropping is better than lagging.
const INPUT_QUEUE: usize = 512;
/// Queue slots pointer moves may not use, kept for key and button events.
const INPUT_RESERVED_FOR_KEYS: usize = 128;
/// How long a leaving viewer waits for the host to acknowledge by closing the connection.
const DISCONNECT_GRACE: Duration = Duration::from_secs(1);
/// Encoded packets waiting for the decoder.
const DECODE_QUEUE: usize = 4;

#[derive(Clone)]
pub struct ViewerConfig {
    pub target: ViewerTarget,
    pub client_name: String,
    /// Translate the shortcut modifier between macOS (⌘) and Windows (Ctrl).
    pub map_shortcut_modifier: bool,
    /// Share clipboard text once the host allows control; `None` disables it.
    pub clipboard: Option<ClipboardFactory>,
    /// Where files from the host are saved once the user accepts them; `None` declines them.
    pub downloads: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerEvent {
    /// The host user is being asked to allow this session.
    AwaitingApproval,
    HostStatus(HostStatus),
    /// The host's displays and the one being shown.
    Displays {
        displays: Vec<DisplayDescription>,
        active: u32,
    },
    /// A file transfer changed. Offers from the host wait in
    /// [`TransferState::Offered`](crate::TransferState::Offered) until
    /// [`ViewerHandle::accept_transfer`] or [`ViewerHandle::cancel_transfer`].
    Transfer(Transfer),
    /// The session is over; no further events follow.
    Ended(SessionEndReason),
}

/// Where the host is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerTarget {
    Direct(SocketAddr),
    /// Through a relay (`host` or `host:port`), by the host's relay ID.
    Relay {
        relay: String,
        id: DeviceId,
    },
}

impl std::fmt::Debug for ViewerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerConfig")
            .field("target", &self.target)
            .field("client_name", &self.client_name)
            .field("map_shortcut_modifier", &self.map_shortcut_modifier)
            .field("clipboard", &self.clipboard.is_some())
            .field("downloads", &self.downloads)
            .finish()
    }
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
    SelectDisplay(u32),
    SetQuality(QualityPreset),
    Clipboard(String),
    /// A file transfer message.
    Transfer(ControlMessage),
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
    transfers: mpsc::UnboundedSender<TransferCommand>,
    supervisor: JoinHandle<()>,
}

impl std::fmt::Debug for Outgoing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Outgoing::Input(_) => "Input",
            Outgoing::RequestKeyframe => "RequestKeyframe",
            Outgoing::SelectDisplay(_) => "SelectDisplay",
            Outgoing::SetQuality(_) => "SetQuality",
            Outgoing::Clipboard(_) => "Clipboard",
            Outgoing::Transfer(_) => "Transfer",
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

    /// Asks the host to stream another of its displays.
    pub fn select_display(&self, id: u32) {
        let _sent = self.outgoing.try_send(Outgoing::SelectDisplay(id));
    }

    /// Asks the host for a different stream quality.
    pub fn set_quality(&self, preset: QualityPreset) {
        let _sent = self.outgoing.try_send(Outgoing::SetQuality(preset));
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

    /// Offers a file to the host. Progress arrives as [`ViewerEvent::Transfer`].
    pub fn send_file(&self, path: PathBuf) {
        let _sent = self.transfers.send(TransferCommand::Send(path));
    }

    /// Accepts a file the host offered; it is saved to the configured downloads folder.
    pub fn accept_transfer(&self, id: TransferId) {
        let _sent = self.transfers.send(TransferCommand::Accept(id));
    }

    /// Declines an offer from the host, or stops a running transfer in either direction.
    pub fn cancel_transfer(&self, id: TransferId) {
        let _sent = self.transfers.send(TransferCommand::Cancel(id));
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
    let connection = match &config.target {
        ViewerTarget::Direct(address) => connect(*address, password, config.client_name).await?,
        ViewerTarget::Relay { relay, id } => {
            let relay = crate::host::resolve_relay(relay)
                .await
                .map_err(ConnectError::RelayUnavailable)?;
            connect_via_relay(relay, *id, password, config.client_name).await?
        }
    };
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
    let (transfers, transfer_commands) = mpsc::unbounded_channel();

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
        config.clipboard,
        config.downloads,
        transfer_commands,
    ));
    Ok((
        ViewerHandle {
            peer,
            outgoing,
            frames,
            stats,
            link,
            transfers,
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
    control_sender: dari_net::MessageSender<ControlMessage>,
    mut control_receiver: dari_net::MessageReceiver<ControlMessage>,
    outgoing: mpsc::Sender<Outgoing>,
    outgoing_receiver: mpsc::Receiver<Outgoing>,
    frames: watch::Sender<Option<Arc<DecodedFrame>>>,
    events: mpsc::UnboundedSender<ViewerEvent>,
    stats: Arc<ViewerStats>,
    mapping: Option<ModifierMapping>,
    clipboard_factory: Option<ClipboardFactory>,
    downloads: Option<PathBuf>,
    mut transfer_commands: mpsc::UnboundedReceiver<TransferCommand>,
) {
    let mut writer = tokio::spawn(write_control(control_sender, outgoing_receiver, mapping));
    let (files, mut incoming_files) = mpsc::channel(4);
    let mut streams = Some(tokio::spawn(receive_streams(
        link.streams(),
        frames,
        outgoing.clone(),
        stats,
        files,
    )));
    let (clipboard_out, mut clipboard_changes) = mpsc::channel(4);
    let (mut session, mut transfer_steps) = ViewerSession::new(
        &link,
        outgoing.clone(),
        events.clone(),
        clipboard_factory,
        clipboard_out,
        downloads,
    );

    let reason = loop {
        tokio::select! {
            message = control_receiver.next() => match message {
                None => break SessionEndReason::ConnectionLost("control stream closed".into()),
                Some(Err(error)) => break SessionEndReason::from_control_error(&error),
                Some(Ok(message)) => {
                    if let Err(reason) = session.handle(message).await {
                        break reason;
                    }
                }
            },
            text = clipboard_changes.recv() => {
                if let Some(text) = text {
                    let _sent = outgoing.try_send(Outgoing::Clipboard(text));
                }
            }
            command = transfer_commands.recv() => {
                if let Some(command) = command {
                    let reply = session.transfers.command(command).await;
                    session.send_transfer_reply(reply).await;
                }
            }
            step = transfer_steps.recv() => {
                if let Some(step) = step {
                    let reply = session.transfers.step(step).await;
                    session.send_transfer_reply(reply).await;
                }
            }
            file = incoming_files.recv() => {
                if let Some((id, stream)) = file {
                    session.transfers.incoming_stream(id, stream);
                }
            }
            result = &mut writer => {
                break match result {
                    Ok(Ok(())) => SessionEndReason::ViewerLeft,
                    Ok(Err(error)) => SessionEndReason::ConnectionLost(error),
                    Err(error) => SessionEndReason::ConnectionLost(error.to_string()),
                };
            }
            result = async {
                match streams.as_mut() {
                    Some(streams) => streams.await,
                    None => std::future::pending().await,
                }
            } => {
                streams = None;
                match result {
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
    if let Some(streams) = streams {
        streams.abort();
    }
    session.transfers.shut_down().await;
    if reason == SessionEndReason::ViewerLeft {
        // Closing now would discard a Disconnect still in flight; the host closes the
        // connection once it has read it.
        let _closed = tokio::time::timeout(DISCONNECT_GRACE, link.closed()).await;
    }
    link.close();
    let _sent = events.send(ViewerEvent::Ended(reason));
}

async fn write_control(
    mut control: dari_net::MessageSender<ControlMessage>,
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
            Outgoing::SelectDisplay(id) => ControlMessage::SelectDisplay(id),
            Outgoing::SetQuality(preset) => ControlMessage::SetQuality(preset),
            Outgoing::Clipboard(text) => ControlMessage::Clipboard(text),
            Outgoing::Transfer(message) => message,
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

/// The viewer's side of the control conversation: everything that reacts to host messages.
struct ViewerSession {
    outgoing: mpsc::Sender<Outgoing>,
    events: mpsc::UnboundedSender<ViewerEvent>,
    clipboard: Option<ClipboardSync>,
    clipboard_factory: Option<ClipboardFactory>,
    clipboard_out: mpsc::Sender<String>,
    transfers: Transfers,
}

impl ViewerSession {
    fn new(
        link: &SessionLink,
        outgoing: mpsc::Sender<Outgoing>,
        events: mpsc::UnboundedSender<ViewerEvent>,
        clipboard_factory: Option<ClipboardFactory>,
        clipboard_out: mpsc::Sender<String>,
        downloads: Option<PathBuf>,
    ) -> (Self, mpsc::UnboundedReceiver<TransferStep>) {
        let transfer_events = events.clone();
        let (transfers, steps) = Transfers::new(
            false,
            link.streams(),
            TransferPolicy {
                // Until the host reports that this session may transfer files.
                allowed: false,
                receive_dir: downloads,
                auto_accept: false,
            },
            Arc::new(move |transfer| {
                let _sent = transfer_events.send(ViewerEvent::Transfer(transfer));
            }),
        );
        let session = Self {
            outgoing,
            events,
            clipboard: None,
            clipboard_factory,
            clipboard_out,
            transfers,
        };
        (session, steps)
    }

    async fn handle(&mut self, message: ControlMessage) -> Result<(), SessionEndReason> {
        match message {
            ControlMessage::HostStatus(status) => {
                // Clipboard sharing follows the host user's "allow control" decision, which is
                // independent of whether input injection itself works on the host.
                self.transfers
                    .set_allowed(status.files == Availability::Available);
                if status.input == Availability::NotAllowed {
                    self.clipboard = None;
                } else if self.clipboard.is_none() {
                    self.clipboard = self.clipboard_factory.clone().and_then(|factory| {
                        ClipboardSync::start(factory, self.clipboard_out.clone())
                    });
                }
                let _sent = self.events.send(ViewerEvent::HostStatus(status));
            }
            ControlMessage::AwaitingApproval => {
                let _sent = self.events.send(ViewerEvent::AwaitingApproval);
            }
            ControlMessage::Declined => return Err(SessionEndReason::Declined),
            ControlMessage::Displays { displays, active } => {
                let _sent = self.events.send(ViewerEvent::Displays { displays, active });
            }
            ControlMessage::Clipboard(text) => {
                if let Some(clipboard) = &self.clipboard {
                    clipboard.apply_remote(text);
                }
            }
            message @ (ControlMessage::FileOffer(_)
            | ControlMessage::FileAccept(_)
            | ControlMessage::FileDone(_)
            | ControlMessage::FileCancel { .. }) => {
                let reply = self
                    .transfers
                    .message(message)
                    .await
                    .map_err(SessionEndReason::ProtocolError)?;
                self.send_transfer_reply(reply).await;
            }
            ControlMessage::Disconnect => return Err(SessionEndReason::HostEnded),
            ControlMessage::Pong { .. } | ControlMessage::Ping { .. } => {}
            ControlMessage::Input(_)
            | ControlMessage::RequestKeyframe
            | ControlMessage::SelectDisplay(_)
            | ControlMessage::SetQuality(_) => {
                return Err(SessionEndReason::ProtocolError(
                    "host sent a viewer message".into(),
                ));
            }
        }
        Ok(())
    }

    async fn send_transfer_reply(&self, reply: Option<ControlMessage>) {
        if let Some(message) = reply {
            // Waits for room rather than dropping: a lost answer would stall the transfer.
            let _sent = self.outgoing.send(Outgoing::Transfer(message)).await;
        }
    }
}

type VideoTask = Pin<Box<dyn Future<Output = Result<(), SessionEndReason>> + Send>>;

/// Accepts the host's streams for the whole session: the video stream goes to the decoder,
/// file streams to the session's transfers. Ends with an error only if the session should.
async fn receive_streams(
    streams: SessionStreams,
    frames: watch::Sender<Option<Arc<DecodedFrame>>>,
    outgoing: mpsc::Sender<Outgoing>,
    stats: Arc<ViewerStats>,
    files: mpsc::Sender<(TransferId, FileReceiver)>,
) -> Result<(), SessionEndReason> {
    let mut frames = Some(frames);
    let mut video: Option<VideoTask> = None;
    loop {
        tokio::select! {
            incoming = streams.accept() => match incoming {
                Ok(IncomingStream::Video(stream)) => {
                    let Some(frames) = frames.take() else {
                        return Err(SessionEndReason::ProtocolError(
                            "host opened a second video stream".into(),
                        ));
                    };
                    video = Some(Box::pin(receive_video(
                        stream,
                        frames,
                        outgoing.clone(),
                        stats.clone(),
                    )));
                }
                Ok(IncomingStream::File { id, stream }) => {
                    if files.send((id, stream)).await.is_err() {
                        return Ok(());
                    }
                }
                Err(error @ StreamError::UnknownKind(_)) => {
                    return Err(SessionEndReason::ProtocolError(error.to_string()));
                }
                Err(error) => return Err(SessionEndReason::ConnectionLost(error.to_string())),
            },
            result = async {
                match video.as_mut() {
                    Some(video) => video.await,
                    None => std::future::pending().await,
                }
            } => {
                video = None;
                // A cleanly ended video stream means the host stopped streaming (e.g. capture
                // permission revoked) but keeps the session; its HostStatus explains why.
                result?;
            }
        }
    }
}

async fn receive_video(
    mut video: MessageReceiver<VideoPacket>,
    frames: watch::Sender<Option<Arc<DecodedFrame>>>,
    outgoing: mpsc::Sender<Outgoing>,
    stats: Arc<ViewerStats>,
) -> Result<(), SessionEndReason> {
    let (packets, packet_receiver) = mpsc::channel(DECODE_QUEUE);
    let decode_stats = stats.clone();
    let decoder = std::thread::Builder::new()
        .name("dari-decode".into())
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
    use dari_proto::{KeyCode, NamedKey, PointerPosition};

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
