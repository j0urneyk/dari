//! Serving one authenticated viewer: approval, screen streaming, input, and clipboard.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use open_desk_input::{DisplayGeometry, InjectError, InputSession};
use open_desk_media::{
    CaptureError, CaptureStream, DisplayInfo, EncodedFrame, StreamError, StreamSettings,
    spawn_capture_stream,
};
use open_desk_net::{
    AuthenticatedConnection, MessageReceiver, MessageSender, PeerInfo, SessionLink,
};
use open_desk_proto::{
    Availability, ControlMessage, DisplayDescription, HostStatus, InputEvent,
    MAX_DEVICE_NAME_CHARS, MAX_DISPLAYS, QualityPreset, VideoPacket, sanitize_display_text,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::SessionEndReason;
use crate::clipboard::ClipboardSync;
use crate::host::{ApprovalDecision, ApprovalRequest, HostEvent};
use crate::platform::HostPlatform;

/// How long the host user has to allow or decline a viewer.
pub(crate) const APPROVAL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the host waits for the viewer to acknowledge a host-initiated end.
const DISCONNECT_GRACE: Duration = Duration::from_secs(1);
/// Input events buffered for injection; beyond this the viewer is flooding and events drop.
const INPUT_QUEUE: usize = 256;

/// How a host serves its viewers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionOptions {
    pub(crate) stream: StreamSettings,
    pub(crate) require_approval: bool,
    pub(crate) clipboard: bool,
}

/// Stream settings for a viewer's quality request, capped by what the host allows.
fn stream_settings(preset: QualityPreset, base: StreamSettings) -> StreamSettings {
    let (max_long_edge, bitrate_bps) = match preset {
        QualityPreset::Speed => (1280, 1_500_000),
        QualityPreset::Balanced => (1920, 4_000_000),
        QualityPreset::Quality => (2560, 10_000_000),
    };
    StreamSettings {
        max_long_edge,
        bitrate_bps,
        max_fps: base.max_fps,
    }
}

/// Serves one authenticated viewer until either side ends the session.
pub(crate) async fn serve_viewer(
    connection: AuthenticatedConnection,
    platform: Arc<dyn HostPlatform>,
    options: SessionOptions,
    mut end: oneshot::Receiver<()>,
    events: mpsc::UnboundedSender<HostEvent>,
) -> SessionEndReason {
    let (link, control_sender, mut control_receiver) = connection.split();
    let peer = link.peer().clone();
    let mut session = HostSession {
        link,
        control: control_sender,
        platform,
        options,
        events,
        status: HostStatus {
            screen: Availability::Unavailable,
            input: Availability::Unavailable,
        },
        displays: Vec::new(),
        active_display: None,
        stream: options.stream,
        input: None,
        capture: None,
        frames: None,
        pump: None,
        clipboard: None,
    };

    let reason = match session
        .await_approval(&peer, &mut control_receiver, &mut end)
        .await
    {
        Ok(decision) => session.run(decision, &mut control_receiver, &mut end).await,
        Err(reason) => reason,
    };
    session.shut_down(&reason).await;
    reason
}

struct HostSession {
    link: SessionLink,
    control: MessageSender<ControlMessage>,
    platform: Arc<dyn HostPlatform>,
    options: SessionOptions,
    events: mpsc::UnboundedSender<HostEvent>,
    status: HostStatus,
    displays: Vec<DisplayInfo>,
    active_display: Option<u32>,
    stream: StreamSettings,
    input: Option<InputQueue>,
    capture: Option<CaptureStream>,
    /// Feeds the video pump; each capture stream gets a clone.
    frames: Option<mpsc::Sender<Result<EncodedFrame, StreamError>>>,
    pump: Option<JoinHandle<Result<(), String>>>,
    clipboard: Option<ClipboardSync>,
}

impl HostSession {
    async fn send(&mut self, message: &ControlMessage) -> Result<(), SessionEndReason> {
        self.control
            .send(message)
            .await
            .map_err(|error| SessionEndReason::ConnectionLost(error.to_string()))
    }

    /// Asks the host user whether to admit the viewer. Nothing is captured or injected before a
    /// decision; declining, ignoring the request, or ending the session refuses the viewer.
    async fn await_approval(
        &mut self,
        peer: &PeerInfo,
        control: &mut MessageReceiver<ControlMessage>,
        end: &mut oneshot::Receiver<()>,
    ) -> Result<ApprovalDecision, SessionEndReason> {
        if !self.options.require_approval {
            return Ok(ApprovalDecision::AllowControl);
        }
        self.send(&ControlMessage::AwaitingApproval).await?;
        let (respond, decision) = oneshot::channel();
        let _sent = self.events.send(HostEvent::ApprovalRequested {
            peer: peer.clone(),
            request: ApprovalRequest::new(respond),
        });
        let timeout = tokio::time::sleep(APPROVAL_TIMEOUT);
        tokio::pin!(timeout);
        tokio::pin!(decision);
        let decision = loop {
            tokio::select! {
                decision = &mut decision => break decision.unwrap_or(ApprovalDecision::Deny),
                () = &mut timeout => break ApprovalDecision::Deny,
                _ = &mut *end => break ApprovalDecision::Deny,
                message = control.next() => match message {
                    None => return Err(SessionEndReason::ConnectionLost("control stream closed".into())),
                    Some(Err(error)) => return Err(SessionEndReason::from_control_error(&error)),
                    Some(Ok(ControlMessage::Disconnect)) => return Err(SessionEndReason::ViewerLeft),
                    Some(Ok(ControlMessage::Ping { token })) => {
                        self.send(&ControlMessage::Pong { token }).await?;
                    }
                    // Anything else (input, requests) is ignored until the host user decides.
                    Some(Ok(_)) => {}
                },
            }
        };
        if decision == ApprovalDecision::Deny {
            info!(peer = %peer.name, "session declined");
            let _sent = self.send(&ControlMessage::Declined).await;
            return Err(SessionEndReason::Declined);
        }
        Ok(decision)
    }

    async fn run(
        &mut self,
        decision: ApprovalDecision,
        control: &mut MessageReceiver<ControlMessage>,
        end: &mut oneshot::Receiver<()>,
    ) -> SessionEndReason {
        let control_allowed = decision == ApprovalDecision::AllowControl;
        let (status_updates, mut status_receiver) = mpsc::channel(4);
        let (clipboard_out, mut clipboard_changes) = mpsc::channel(4);
        if let Err(reason) = self
            .start(control_allowed, status_updates, clipboard_out)
            .await
        {
            return reason;
        }

        loop {
            tokio::select! {
                _ = &mut *end => {
                    let _sent = self.send(&ControlMessage::Disconnect).await;
                    return SessionEndReason::HostEnded;
                }
                screen = status_receiver.recv() => {
                    let Some(screen) = screen else { continue };
                    self.status.screen = screen;
                    if let Err(reason) = self.publish_status().await {
                        return reason;
                    }
                }
                text = clipboard_changes.recv() => {
                    let Some(text) = text else { continue };
                    if let Err(reason) = self.send(&ControlMessage::Clipboard(text)).await {
                        return reason;
                    }
                }
                result = async {
                    match self.pump.as_mut() {
                        Some(pump) => pump.await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.pump = None;
                    if let Ok(Err(error)) = result {
                        return SessionEndReason::ConnectionLost(error);
                    }
                }
                message = control.next() => match message {
                    None => return SessionEndReason::ConnectionLost("control stream closed".into()),
                    Some(Err(error)) => return SessionEndReason::from_control_error(&error),
                    Some(Ok(message)) => {
                        if let Err(reason) = self.handle(message).await {
                            return reason;
                        }
                    }
                },
            }
        }
    }

    /// Starts input, the video stream, and clipboard sync, and tells the viewer what it gets.
    async fn start(
        &mut self,
        control_allowed: bool,
        status_updates: mpsc::Sender<Availability>,
        clipboard_out: mpsc::Sender<String>,
    ) -> Result<(), SessionEndReason> {
        self.displays = match self.platform.displays() {
            Ok(mut displays) => {
                displays.truncate(MAX_DISPLAYS);
                displays
            }
            Err(error) => {
                warn!(%error, "cannot list displays");
                Vec::new()
            }
        };
        self.active_display = self.displays.first().map(|display| display.id);

        if let Some(display) = self.displays.first() {
            let geometry = geometry(display);
            if control_allowed {
                let (input, availability) = spawn_input_thread(self.platform.clone(), geometry);
                self.status.input = match availability {
                    Some(availability) => availability.await.unwrap_or(Availability::Unavailable),
                    None => Availability::Unavailable,
                };
                self.input = input;
            } else {
                self.status.input = Availability::NotAllowed;
            }

            let video = self
                .link
                .open_video_sender()
                .await
                .map_err(|error| SessionEndReason::ConnectionLost(error.to_string()))?;
            let (frames, frame_receiver) = mpsc::channel(1);
            self.frames = Some(frames);
            self.pump = Some(tokio::spawn(pump_video(
                frame_receiver,
                video,
                status_updates,
            )));
            self.restart_capture().await;
        } else {
            self.status.input = if control_allowed {
                Availability::Unavailable
            } else {
                Availability::NotAllowed
            };
        }

        if control_allowed && self.options.clipboard {
            let factory = self.platform.clipboard();
            self.clipboard =
                factory.and_then(|factory| ClipboardSync::start(factory, clipboard_out));
        }

        self.publish_status().await?;
        self.publish_displays().await
    }

    async fn publish_status(&mut self) -> Result<(), SessionEndReason> {
        let _sent = self.events.send(HostEvent::SessionStatus(self.status));
        self.send(&ControlMessage::HostStatus(self.status)).await
    }

    async fn publish_displays(&mut self) -> Result<(), SessionEndReason> {
        let Some(active) = self.active_display else {
            return Ok(());
        };
        let displays = self
            .displays
            .iter()
            .map(|display| DisplayDescription {
                id: display.id,
                name: sanitize_display_text(&display.name, MAX_DEVICE_NAME_CHARS),
                width: display.width,
                height: display.height,
                primary: display.is_primary,
            })
            .collect();
        self.send(&ControlMessage::Displays { displays, active })
            .await
    }

    /// (Re)starts screen capture for the active display with the current stream settings.
    async fn restart_capture(&mut self) {
        if let Some(previous) = self.capture.take() {
            let _joined = tokio::task::spawn_blocking(move || previous.stop()).await;
        }
        let (Some(display), Some(frames)) = (self.active_display, self.frames.clone()) else {
            return;
        };
        let platform = self.platform.clone();
        match spawn_capture_stream(move || platform.open_capturer(display), self.stream, frames) {
            Ok(capture) => {
                self.capture = Some(capture);
                self.status.screen = Availability::Available;
            }
            Err(error) => {
                warn!(%error, "cannot start the capture thread");
                self.status.screen = Availability::Unavailable;
            }
        }
    }

    async fn handle(&mut self, message: ControlMessage) -> Result<(), SessionEndReason> {
        match message {
            ControlMessage::Input(event) => {
                // View-only sessions have no input queue; their input is ignored.
                if let Some(input) = &self.input
                    && !input.push(InputCommand::Event(event))
                {
                    debug!("input queue full; dropping event");
                }
            }
            ControlMessage::RequestKeyframe => {
                if let Some(capture) = &self.capture {
                    capture.request_keyframe();
                }
            }
            ControlMessage::SelectDisplay(id) => {
                let Some(display) = self
                    .displays
                    .iter()
                    .find(|display| display.id == id)
                    .cloned()
                else {
                    debug!(id, "viewer selected an unknown display");
                    return Ok(());
                };
                if self.active_display != Some(id) {
                    self.active_display = Some(id);
                    if let Some(input) = &self.input {
                        input.push(InputCommand::SetGeometry(geometry(&display)));
                    }
                    self.restart_capture().await;
                    self.publish_status().await?;
                    self.publish_displays().await?;
                }
            }
            ControlMessage::SetQuality(preset) => {
                let settings = stream_settings(preset, self.options.stream);
                if settings != self.stream {
                    self.stream = settings;
                    self.restart_capture().await;
                }
            }
            ControlMessage::Clipboard(text) => {
                if let Some(clipboard) = &self.clipboard {
                    clipboard.apply_remote(text);
                }
            }
            ControlMessage::Ping { token } => self.send(&ControlMessage::Pong { token }).await?,
            ControlMessage::Pong { .. } => {}
            ControlMessage::Disconnect => return Err(SessionEndReason::ViewerLeft),
            ControlMessage::HostStatus(_)
            | ControlMessage::AwaitingApproval
            | ControlMessage::Declined
            | ControlMessage::Displays { .. } => {
                return Err(SessionEndReason::ProtocolError(
                    "viewer sent a host message".into(),
                ));
            }
        }
        Ok(())
    }

    async fn shut_down(mut self, reason: &SessionEndReason) {
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
        // Stopping joins the capture thread; keep that off the async workers.
        if let Some(capture) = self.capture.take() {
            let _joined = tokio::task::spawn_blocking(move || capture.stop()).await;
        }
        // Dropping the queue stops injection at once and releases every held key and button.
        drop(self.input.take());
        drop(self.clipboard.take());
        let _finished = self.control.close().await;
        if matches!(
            reason,
            SessionEndReason::HostEnded | SessionEndReason::Declined
        ) {
            // Closing now would discard the last message still in flight; the viewer closes the
            // connection once it has read it.
            let _closed = tokio::time::timeout(DISCONNECT_GRACE, self.link.closed()).await;
        }
        self.link.close();
    }
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

/// Forwards encoded frames to the viewer for the whole session, across capture restarts. A
/// capture failure is reported as a status change; a transport failure ends the session.
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
            }
        }
    }
    Ok(())
}

enum InputCommand {
    Event(InputEvent),
    SetGeometry(DisplayGeometry),
}

/// Feeds the input thread. Dropping it stops injection at once: events still queued are
/// discarded, not applied after the session ended.
struct InputQueue {
    commands: mpsc::Sender<InputCommand>,
    stopped: Arc<AtomicBool>,
}

impl InputQueue {
    /// Queues a command; returns `false` if the queue is full or closed.
    fn push(&self, command: InputCommand) -> bool {
        self.commands.try_send(command).is_ok()
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
    let (commands, mut queue) = mpsc::channel(INPUT_QUEUE);
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
            while let Some(command) = queue.blocking_recv() {
                if thread_stopped.load(Ordering::Acquire) {
                    break;
                }
                match command {
                    InputCommand::Event(event) => {
                        if let Err(error) = session.apply(&event) {
                            debug!(%error, "could not apply remote input");
                        }
                    }
                    InputCommand::SetGeometry(geometry) => session.set_geometry(geometry),
                }
            }
            // Dropping the session releases everything the viewer still held.
        });
    match spawned {
        Ok(_thread) => (Some(InputQueue { commands, stopped }), Some(availability)),
        Err(error) => {
            warn!(%error, "cannot start the input thread");
            (None, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_presets_scale_resolution_and_bitrate() {
        let base = StreamSettings::default();
        let speed = stream_settings(QualityPreset::Speed, base);
        let quality = stream_settings(QualityPreset::Quality, base);
        assert!(speed.max_long_edge < quality.max_long_edge);
        assert!(speed.bitrate_bps < quality.bitrate_bps);
        assert_eq!(speed.max_fps, base.max_fps);
    }
}
