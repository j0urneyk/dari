//! The host service: listening, password management, and serving one viewer at a time.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use open_desk_input::{DisplayGeometry, InjectError, InputSession};
use open_desk_media::{
    CaptureError, CaptureStream, DisplayInfo, EncodedFrame, StreamError, StreamSettings,
    spawn_capture_stream,
};
use open_desk_net::{
    AccessPassword, AuthenticatedConnection, DeviceIdentity, EndpointError, HostEndpoint,
    HostSettings, MessageSender, PasswordError, PeerInfo, SessionLink,
};
use open_desk_proto::{Availability, ControlMessage, HostStatus, InputEvent, VideoPacket};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::SessionEndReason;
use crate::platform::HostPlatform;

/// How long the host waits for the viewer to acknowledge a host-initiated disconnect.
const DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
/// Input events buffered for injection; beyond this the viewer is flooding and events drop.
const INPUT_QUEUE: usize = 256;

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub bind_address: SocketAddr,
    pub host_name: String,
    pub stream: StreamSettings,
}

#[derive(Debug, Error)]
pub enum HostError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error(transparent)]
    Password(#[from] PasswordError),
}

/// What the host service reports to the UI.
#[derive(Debug, Clone)]
pub enum HostEvent {
    /// The access password viewers need now; `None` while not accepting viewers.
    PasswordChanged(Option<AccessPassword>),
    SessionStarted(PeerInfo),
    /// Screen or input availability for the running session changed.
    SessionStatus(HostStatus),
    SessionEnded {
        peer: PeerInfo,
        reason: SessionEndReason,
    },
}

enum HostCommand {
    RegeneratePassword,
    SetAccepting(bool),
    EndSession,
}

/// Controls a running host service. Dropping it stops the service and ends any session.
#[derive(Debug)]
pub struct HostHandle {
    commands: mpsc::UnboundedSender<HostCommand>,
    local_address: SocketAddr,
    task: JoinHandle<()>,
}

impl HostHandle {
    pub fn local_address(&self) -> SocketAddr {
        self.local_address
    }

    /// Replaces the access password with a fresh one.
    pub fn regenerate_password(&self) {
        let _sent = self.commands.send(HostCommand::RegeneratePassword);
    }

    /// Starts or stops accepting new viewers. A running session is not affected.
    pub fn set_accepting(&self, accepting: bool) {
        let _sent = self.commands.send(HostCommand::SetAccepting(accepting));
    }

    /// Ends the running session, if any.
    pub fn end_session(&self) {
        let _sent = self.commands.send(HostCommand::EndSession);
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for HostCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            HostCommand::RegeneratePassword => "RegeneratePassword",
            HostCommand::SetAccepting(_) => "SetAccepting",
            HostCommand::EndSession => "EndSession",
        })
    }
}

/// Starts the host service on the current Tokio runtime.
pub fn start_host(
    config: HostConfig,
    identity: &DeviceIdentity,
    platform: Arc<dyn HostPlatform>,
) -> Result<(HostHandle, mpsc::UnboundedReceiver<HostEvent>), HostError> {
    let endpoint = HostEndpoint::bind(
        HostSettings {
            bind_address: config.bind_address,
            host_name: config.host_name,
        },
        identity,
    )?;
    let local_address = endpoint.local_address().map_err(EndpointError::from)?;
    let (commands, command_receiver) = mpsc::unbounded_channel();
    let (events, event_receiver) = mpsc::unbounded_channel();
    let service = HostService {
        endpoint,
        events,
        platform,
        stream: config.stream,
        accepting: true,
    };
    service.issue_password()?;
    let task = tokio::spawn(service.run(command_receiver));
    Ok((
        HostHandle {
            commands,
            local_address,
            task,
        },
        event_receiver,
    ))
}

struct RunningSession {
    peer: PeerInfo,
    end: Option<oneshot::Sender<()>>,
    task: JoinHandle<SessionEndReason>,
}

struct HostService {
    endpoint: HostEndpoint,
    events: mpsc::UnboundedSender<HostEvent>,
    platform: Arc<dyn HostPlatform>,
    stream: StreamSettings,
    accepting: bool,
}

impl HostService {
    /// Sets a fresh password (or none when not accepting) and tells the UI.
    fn issue_password(&self) -> Result<(), PasswordError> {
        let password = self.accepting.then(AccessPassword::generate).transpose()?;
        self.endpoint.set_password(password.clone());
        let _sent = self.events.send(HostEvent::PasswordChanged(password));
        Ok(())
    }

    fn reissue_password(&self) {
        if let Err(error) = self.issue_password() {
            warn!(%error, "could not create a new password; not accepting viewers");
            self.endpoint.set_password(None);
            let _sent = self.events.send(HostEvent::PasswordChanged(None));
        }
    }

    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<HostCommand>) {
        let mut session: Option<RunningSession> = None;
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    None => break,
                    Some(HostCommand::RegeneratePassword) => self.reissue_password(),
                    Some(HostCommand::SetAccepting(accepting)) => {
                        self.accepting = accepting;
                        // Mid-session the password stays consumed; a new one is issued when
                        // the session ends.
                        if session.is_none() || !accepting {
                            self.reissue_password();
                        }
                    }
                    Some(HostCommand::EndSession) => {
                        if let Some(end) = session.as_mut().and_then(|running| running.end.take()) {
                            let _sent = end.send(());
                        }
                    }
                },
                connection = self.endpoint.accept(), if session.is_none() => {
                    let Some(connection) = connection else { break };
                    let peer = connection.peer().clone();
                    info!(peer = %peer.name, address = %peer.address, "session started");
                    let _sent = self.events.send(HostEvent::SessionStarted(peer.clone()));
                    let (end, end_receiver) = oneshot::channel();
                    let task = tokio::spawn(serve_viewer(
                        connection,
                        self.platform.clone(),
                        self.stream,
                        end_receiver,
                        self.events.clone(),
                    ));
                    session = Some(RunningSession { peer, end: Some(end), task });
                }
                reason = async {
                    match session.as_mut() {
                        Some(running) => (&mut running.task).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let reason = reason.unwrap_or_else(|error| {
                        SessionEndReason::ConnectionLost(format!("session task failed: {error}"))
                    });
                    if let Some(running) = session.take() {
                        info!(peer = %running.peer.name, %reason, "session ended");
                        let _sent = self.events.send(HostEvent::SessionEnded { peer: running.peer, reason });
                    }
                    // The previous password was consumed by the session; issue a new one.
                    self.reissue_password();
                }
            }
        }
        if let Some(running) = session {
            running.task.abort();
        }
    }
}

/// Serves one authenticated viewer until either side ends the session.
async fn serve_viewer(
    connection: AuthenticatedConnection,
    platform: Arc<dyn HostPlatform>,
    stream: StreamSettings,
    mut end: oneshot::Receiver<()>,
    events: mpsc::UnboundedSender<HostEvent>,
) -> SessionEndReason {
    let (link, mut control_sender, mut control_receiver) = connection.split();

    let SessionMedia {
        mut status,
        input_events,
        capture,
        mut pump,
        mut status_receiver,
    } = match start_media(&link, &platform, stream).await {
        Ok(media) => media,
        Err(reason) => return reason,
    };

    let _sent = events.send(HostEvent::SessionStatus(status));
    if control_sender
        .send(&ControlMessage::HostStatus(status))
        .await
        .is_err()
    {
        return SessionEndReason::ConnectionLost("could not send the session status".into());
    }

    let reason = loop {
        tokio::select! {
            _ = &mut end => {
                let _sent = control_sender.send(&ControlMessage::Disconnect).await;
                break SessionEndReason::HostEnded;
            }
            screen = status_receiver.recv() => {
                let Some(screen) = screen else { continue };
                status.screen = screen;
                let _sent = events.send(HostEvent::SessionStatus(status));
                if control_sender.send(&ControlMessage::HostStatus(status)).await.is_err() {
                    break SessionEndReason::ConnectionLost("control stream closed".into());
                }
            }
            result = async {
                match pump.as_mut() {
                    Some(pump) => pump.await,
                    None => std::future::pending().await,
                }
            } => {
                pump = None;
                if let Ok(Err(error)) = result {
                    break SessionEndReason::ConnectionLost(error);
                }
            }
            message = control_receiver.next() => match message {
                None => break SessionEndReason::ConnectionLost("control stream closed".into()),
                Some(Err(error)) => break SessionEndReason::from_control_error(&error),
                Some(Ok(message)) => match message {
                    ControlMessage::Input(event) => {
                        if let Some(input_events) = &input_events
                            && !input_events.push(event)
                        {
                            debug!("input queue full; dropping event");
                        }
                    }
                    ControlMessage::RequestKeyframe => {
                        if let Some(capture) = &capture {
                            capture.request_keyframe();
                        }
                    }
                    ControlMessage::Ping { token } => {
                        if control_sender.send(&ControlMessage::Pong { token }).await.is_err() {
                            break SessionEndReason::ConnectionLost("control stream closed".into());
                        }
                    }
                    ControlMessage::Disconnect => break SessionEndReason::ViewerLeft,
                    ControlMessage::Pong { .. } => {}
                    ControlMessage::HostStatus(_) => {
                        break SessionEndReason::ProtocolError("viewer sent a host message".into());
                    }
                },
            },
        }
    };

    if let Some(pump) = pump {
        pump.abort();
    }
    // Stopping joins the capture thread; keep that off the async workers.
    if let Some(capture) = capture {
        let _joined = tokio::task::spawn_blocking(move || capture.stop()).await;
    }
    // Closing the queue ends the input thread, which releases every held key and button.
    drop(input_events);
    let _finished = control_sender.close().await;
    if reason == SessionEndReason::HostEnded {
        // Closing now would discard the Disconnect still in flight; the viewer closes the
        // connection once it has read it.
        let _closed = tokio::time::timeout(DISCONNECT_GRACE, link.closed()).await;
    }
    link.close();
    reason
}

/// Everything that streams the screen and applies input for one session.
struct SessionMedia {
    status: HostStatus,
    input_events: Option<InputQueue>,
    capture: Option<CaptureStream>,
    pump: Option<JoinHandle<Result<(), String>>>,
    status_receiver: mpsc::Receiver<Availability>,
}

/// Starts input injection and screen streaming on the primary display, recording what works.
async fn start_media(
    link: &SessionLink,
    platform: &Arc<dyn HostPlatform>,
    stream: StreamSettings,
) -> Result<SessionMedia, SessionEndReason> {
    let display = match platform
        .displays()
        .map(|displays| displays.into_iter().next())
    {
        Ok(display) => display,
        Err(error) => {
            warn!(%error, "cannot list displays");
            None
        }
    };
    let (status_updates, status_receiver) = mpsc::channel(4);
    let mut media = SessionMedia {
        status: HostStatus {
            screen: Availability::Unavailable,
            input: Availability::Unavailable,
        },
        input_events: None,
        capture: None,
        pump: None,
        status_receiver,
    };
    let Some(display) = display else {
        return Ok(media);
    };

    let (input_events, input_status) = spawn_input_thread(platform.clone(), geometry(&display));
    media.input_events = input_events;
    if let Some(input_status) = input_status {
        media.status.input = input_status.await.unwrap_or(Availability::Unavailable);
    }

    let (frames, frame_receiver) = mpsc::channel(1);
    let capture_platform = platform.clone();
    let display_id = display.id;
    match spawn_capture_stream(
        move || capture_platform.open_capturer(display_id),
        stream,
        frames,
    ) {
        Ok(capture) => {
            let video = link
                .open_video_sender()
                .await
                .map_err(|error| SessionEndReason::ConnectionLost(error.to_string()))?;
            media.pump = Some(tokio::spawn(pump_video(
                frame_receiver,
                video,
                status_updates,
            )));
            media.capture = Some(capture);
            media.status.screen = Availability::Available;
        }
        Err(error) => warn!(%error, "cannot start the capture thread"),
    }
    Ok(media)
}

fn geometry(display: &DisplayInfo) -> DisplayGeometry {
    DisplayGeometry {
        x: display.x,
        y: display.y,
        width: display.width,
        height: display.height,
    }
}

fn availability_of_capture(error: &StreamError) -> Availability {
    match error {
        StreamError::Capture(CaptureError::PermissionDenied) => Availability::PermissionDenied,
        StreamError::Capture(_) | StreamError::Codec(_) => Availability::Unavailable,
    }
}

/// Forwards encoded frames to the viewer. A capture failure is reported as a status change and
/// ends the pump; a transport failure ends the session.
async fn pump_video(
    mut frames: mpsc::Receiver<Result<EncodedFrame, StreamError>>,
    mut video: MessageSender<VideoPacket>,
    status: mpsc::Sender<Availability>,
) -> Result<(), String> {
    let started = Instant::now();
    let mut sequence = 0u64;
    while let Some(frame) = frames.recv().await {
        match frame {
            Ok(frame) => {
                let packet = VideoPacket {
                    sequence,
                    timestamp_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                    keyframe: frame.keyframe,
                    width: frame.width,
                    height: frame.height,
                    data: frame.data,
                };
                sequence += 1;
                video
                    .send(&packet)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Err(error) => {
                warn!(%error, "screen capture stopped");
                let _sent = status.send(availability_of_capture(&error)).await;
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Feeds the input thread. Dropping it stops injection at once: events still queued are
/// discarded, not applied after the session ended.
struct InputQueue {
    events: mpsc::Sender<InputEvent>,
    stopped: Arc<AtomicBool>,
}

impl InputQueue {
    /// Queues an event; returns `false` if the queue is full or closed.
    fn push(&self, event: InputEvent) -> bool {
        self.events.try_send(event).is_ok()
    }
}

impl Drop for InputQueue {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

/// Runs input injection on its own thread (OS input handles are not `Send`). Returns the queue
/// and a report of whether injection works.
fn spawn_input_thread(
    platform: Arc<dyn HostPlatform>,
    geometry: DisplayGeometry,
) -> (Option<InputQueue>, Option<oneshot::Receiver<Availability>>) {
    let (queue, mut events) = mpsc::channel(INPUT_QUEUE);
    let stopped = Arc::new(AtomicBool::new(false));
    let thread_stopped = stopped.clone();
    let (report, availability) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("open-desk-input".into())
        .spawn(move || {
            let backend = match platform.open_input() {
                Ok(backend) => {
                    let _sent = report.send(Availability::Available);
                    backend
                }
                Err(error) => {
                    warn!(%error, "remote input is unavailable");
                    let _sent = report.send(match error {
                        InjectError::PermissionDenied => Availability::PermissionDenied,
                        InjectError::Unsupported(_) | InjectError::Backend(_) => {
                            Availability::Unavailable
                        }
                    });
                    return;
                }
            };
            let mut session = InputSession::new(backend, geometry);
            while let Some(event) = events.blocking_recv() {
                if thread_stopped.load(Ordering::Acquire) {
                    break;
                }
                if let Err(error) = session.apply(&event) {
                    debug!(%error, "could not apply remote input");
                }
            }
            // Dropping the session releases everything the viewer still held.
        });
    match spawned {
        Ok(_thread) => (
            Some(InputQueue {
                events: queue,
                stopped,
            }),
            Some(availability),
        ),
        Err(error) => {
            warn!(%error, "cannot start the input thread");
            (None, None)
        }
    }
}
