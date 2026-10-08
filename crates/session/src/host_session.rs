//! Serving one authenticated viewer: approval, screen and audio streaming, input, clipboard,
//! and files.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dari_input::{DisplayGeometry, InjectError, InputSession};
use dari_media::{
    AudioError, AudioStream, CaptureError, CaptureStream, DisplayInfo, EncodedFrame,
    FRAMES_IN_FLIGHT, PermissionState, StreamError, StreamSettings, spawn_audio_stream,
    spawn_capture_stream,
};
use dari_net::{
    AuthenticatedConnection, FileReceiver, IncomingStream, MessageReceiver, MessageSender,
    PeerInfo, SessionLink, SessionStreams,
};
use dari_proto::{
    AudioPacket, Availability, ControlMessage, DisplayDescription, HostStatus, InputEvent,
    MAX_DEVICE_NAME_CHARS, MAX_DISPLAYS, ProtocolVersion, QualityPreset, TransferId, VideoPacket,
    sanitize_display_text,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::SessionEndReason;
use crate::clipboard::ClipboardSync;
use crate::host::{ApprovalDecision, ApprovalRequest, HostEvent, HostPolicy};
use crate::platform::HostPlatform;
use crate::transfer::{
    PEER_FILE_STREAMS, TransferCommand, TransferPolicy, TransferStep, Transfers,
};

/// How long the host user has to allow or decline a viewer.
pub(crate) const APPROVAL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a host that admits viewers without asking waits for the viewer's stream request
/// before streaming anyway. The request is the first thing a viewer sends, so it normally
/// arrives one round trip after the handshake.
const STREAM_REQUEST_WAIT: Duration = Duration::from_secs(2);
/// How long the host waits for the viewer to acknowledge a host-initiated end.
const DISCONNECT_GRACE: Duration = Duration::from_secs(1);
/// Input events buffered for injection; beyond this the viewer is flooding and events drop.
const INPUT_QUEUE: usize = 256;
/// Encoded audio packets waiting to be sent; more means the network is stalled.
const AUDIO_QUEUE: usize = 16;

/// How a host serves its viewers.
#[derive(Debug, Clone)]
pub(crate) struct SessionOptions {
    pub(crate) stream: StreamSettings,
    pub(crate) policy: HostPolicy,
    /// Where files from the viewer are saved.
    pub(crate) downloads: Option<PathBuf>,
}

/// File streams the viewer opened, or why accepting them failed.
type IncomingFiles = mpsc::Receiver<Result<(TransferId, FileReceiver), SessionEndReason>>;

/// The frame rate the quality presets' bitrates are tuned for.
const PRESET_FRAME_RATE: u32 = 30;
/// The most bandwidth any stream may use.
const MAX_BITRATE_BPS: u32 = 50_000_000;

/// What the viewer has asked for so far; `None` until it asks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ViewerRequest {
    quality: Option<QualityPreset>,
    frame_rate: Option<u16>,
}

/// Stream settings for the viewer's requests on a display refreshing at `display_refresh` Hz
/// (0 if unknown), starting from the host's `base` settings.
///
/// The frame rate never exceeds the display's refresh rate: the screen cannot change faster.
/// The bitrate grows with the frame rate, though less than proportionally because consecutive
/// frames of a faster stream differ less.
fn stream_settings(
    request: ViewerRequest,
    display_refresh: u32,
    base: StreamSettings,
) -> StreamSettings {
    // Each bitrate belongs to a frame rate: the host's own, or the one the presets are tuned for.
    let (max_long_edge, bitrate_bps, bitrate_frame_rate) = match request.quality {
        None => (base.max_long_edge, base.bitrate_bps, base.max_fps),
        Some(QualityPreset::Speed) => (1280, 1_500_000, PRESET_FRAME_RATE),
        Some(QualityPreset::Balanced) => (1920, 4_000_000, PRESET_FRAME_RATE),
        Some(QualityPreset::Quality) => (2560, 10_000_000, PRESET_FRAME_RATE),
    };
    let requested = request.frame_rate.map_or(base.max_fps, u32::from);
    let max_fps = match display_refresh {
        0 => requested,
        refresh => requested.min(refresh),
    }
    .max(1);
    let scale = (f64::from(max_fps) / f64::from(bitrate_frame_rate.max(1))).powf(0.75);
    // In range: the result is clamped to `MAX_BITRATE_BPS` first.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bitrate_bps = (f64::from(bitrate_bps) * scale).min(f64::from(MAX_BITRATE_BPS)) as u32;
    StreamSettings {
        max_long_edge,
        max_fps,
        bitrate_bps,
        hardware_encoder: base.hardware_encoder,
    }
}

/// Serves one authenticated viewer until either side ends the session.
pub(crate) async fn serve_viewer(
    connection: AuthenticatedConnection,
    platform: Arc<dyn HostPlatform>,
    options: SessionOptions,
    mut end: oneshot::Receiver<()>,
    mut transfer_commands: mpsc::UnboundedReceiver<TransferCommand>,
    events: mpsc::UnboundedSender<HostEvent>,
) -> SessionEndReason {
    let (link, control_sender, mut control_receiver) = connection.split();
    let peer = link.peer().clone();
    let transfer_events = events.clone();
    let (transfers, mut transfer_steps) = Transfers::new(
        true,
        link.streams(),
        TransferPolicy {
            allowed: false,
            receive_dir: options.downloads.clone(),
            // The viewer already controls this machine; asking the host user adds nothing.
            auto_accept: true,
        },
        Arc::new(move |transfer| {
            let _sent = transfer_events.send(HostEvent::Transfer(transfer));
        }),
    );
    let stream = options.stream;
    let mut session = HostSession {
        link,
        control: control_sender,
        platform,
        options,
        events,
        status: HostStatus {
            screen: Availability::Unavailable,
            input: Availability::Unavailable,
            files: Availability::Unavailable,
            audio: Availability::Unavailable,
        },
        peer_version: peer.version,
        displays: Vec::new(),
        active_display: None,
        request: ViewerRequest::default(),
        stream,
        reported_frame_rate: None,
        transfers,
        input: None,
        capture: None,
        frames: None,
        pump: None,
        audio: AudioState::Off,
        audio_generation: 0,
        audio_opened: None,
        clipboard: None,
    };

    let reason = match session
        .await_approval(&peer, &mut control_receiver, &mut end)
        .await
    {
        Ok(decision) => {
            session
                .run(
                    decision,
                    &mut control_receiver,
                    &mut end,
                    &mut transfer_commands,
                    &mut transfer_steps,
                )
                .await
        }
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
    peer_version: ProtocolVersion,
    displays: Vec<DisplayInfo>,
    active_display: Option<u32>,
    request: ViewerRequest,
    stream: StreamSettings,
    /// The frame rate last reported to the viewer.
    reported_frame_rate: Option<u32>,
    transfers: Transfers,
    input: Option<InputQueue>,
    capture: Option<CaptureStream>,
    /// Feeds the video pump; each capture stream gets a clone.
    frames: Option<mpsc::Sender<Result<EncodedFrame, StreamError>>>,
    pump: Option<JoinHandle<Result<(), String>>>,
    /// System audio, while the viewer asks for it.
    audio: AudioState,
    /// Counts audio starts, so a capturer that finishes opening after a mute is recognized.
    audio_generation: u64,
    /// Where opened capturers report back; set while the session runs.
    audio_opened: Option<mpsc::UnboundedSender<AudioOpened>>,
    clipboard: Option<ClipboardSync>,
}

enum AudioState {
    Off,
    /// The capturer for this generation is opening on a blocking thread.
    Opening(u64),
    On(SharedAudio),
}

/// A capturer that finished opening (or failed to), with the generation that asked for it.
struct AudioOpened {
    generation: u64,
    result: Result<(AudioStream, mpsc::Receiver<Vec<u8>>), AudioError>,
}

/// The audio capture thread and the task sending its packets as datagrams.
struct SharedAudio {
    capture: AudioStream,
    sender: JoinHandle<()>,
}

impl SharedAudio {
    fn stop_in_background(self) {
        self.sender.abort();
        tokio::task::spawn_blocking(move || self.capture.stop());
    }
}

/// Sends encoded audio as datagrams until the capture thread stops.
async fn send_audio(streams: SessionStreams, mut encoded: mpsc::Receiver<Vec<u8>>) {
    let mut sequence = 0u32;
    while let Some(data) = encoded.recv().await {
        let packet = AudioPacket { sequence, data };
        sequence = sequence.wrapping_add(1);
        if let Err(error) = streams.send_audio(&packet) {
            debug!(%error, "cannot send audio");
        }
    }
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
        if !self.options.policy.require_approval {
            self.await_stream_request(control, end).await?;
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
                message = control.next() => self.handle_before_start(message).await?,
            }
        };
        if decision == ApprovalDecision::Deny {
            info!(peer = %peer.name, "session declined");
            let _sent = self.send(&ControlMessage::Declined).await;
            return Err(SessionEndReason::Declined);
        }
        Ok(decision)
    }

    /// Waits for the viewer's first control message, its stream request, so the stream starts at
    /// the frame rate the viewer wants instead of starting at the default and restarting.
    async fn await_stream_request(
        &mut self,
        control: &mut MessageReceiver<ControlMessage>,
        end: &mut oneshot::Receiver<()>,
    ) -> Result<(), SessionEndReason> {
        tokio::select! {
            () = tokio::time::sleep(STREAM_REQUEST_WAIT) => {
                debug!("no stream request from the viewer; streaming with the defaults");
                Ok(())
            }
            _ = &mut *end => {
                let _sent = self.send(&ControlMessage::Disconnect).await;
                Err(SessionEndReason::HostEnded)
            }
            message = control.next() => self.handle_before_start(message).await,
        }
    }

    /// Handles a control message that arrives before the stream starts.
    async fn handle_before_start(
        &mut self,
        message: Option<Result<ControlMessage, dari_proto::CodecError>>,
    ) -> Result<(), SessionEndReason> {
        match message {
            None => Err(SessionEndReason::ConnectionLost(
                "control stream closed".into(),
            )),
            Some(Err(error)) => Err(SessionEndReason::from_control_error(&error)),
            Some(Ok(ControlMessage::Disconnect)) => Err(SessionEndReason::ViewerLeft),
            Some(Ok(ControlMessage::Ping { token })) => {
                self.send(&ControlMessage::Pong { token }).await
            }
            // Stream preferences only take effect once the stream starts.
            Some(Ok(ControlMessage::SetQuality(preset))) => {
                self.request.quality = Some(preset);
                Ok(())
            }
            Some(Ok(ControlMessage::SetFrameRate(rate))) => {
                self.request.frame_rate = Some(rate);
                Ok(())
            }
            // Anything else (input, requests) is ignored until the session starts.
            Some(Ok(_)) => Ok(()),
        }
    }

    async fn send_reply(&mut self, reply: Option<ControlMessage>) -> Result<(), SessionEndReason> {
        match reply {
            Some(message) => self.send(&message).await,
            None => Ok(()),
        }
    }

    async fn run(
        &mut self,
        decision: ApprovalDecision,
        control: &mut MessageReceiver<ControlMessage>,
        end: &mut oneshot::Receiver<()>,
        transfer_commands: &mut mpsc::UnboundedReceiver<TransferCommand>,
        transfer_steps: &mut mpsc::UnboundedReceiver<TransferStep>,
    ) -> SessionEndReason {
        let control_allowed = decision == ApprovalDecision::AllowControl;
        let (status_updates, mut status_receiver) = mpsc::channel(4);
        let (clipboard_out, mut clipboard_changes) = mpsc::channel(4);
        let (audio_opened, mut opened_audio) = mpsc::unbounded_channel();
        self.audio_opened = Some(audio_opened);
        let mut incoming_files = match self
            .start(control_allowed, status_updates, clipboard_out)
            .await
        {
            Ok(incoming_files) => incoming_files,
            Err(reason) => return reason,
        };

        loop {
            tokio::select! {
                _ = &mut *end => {
                    let _sent = self.send(&ControlMessage::Disconnect).await;
                    return SessionEndReason::HostEnded;
                }
                screen = status_receiver.recv() => {
                    let Some(screen) = screen else { continue };
                    if self.status.screen != screen {
                        self.status.screen = screen;
                        if let Err(reason) = self.publish_status().await {
                            return reason;
                        }
                    }
                }
                text = clipboard_changes.recv() => {
                    let Some(text) = text else { continue };
                    if let Err(reason) = self.send(&ControlMessage::Clipboard(text)).await {
                        return reason;
                    }
                }
                opened = opened_audio.recv() => {
                    let Some(opened) = opened else { continue };
                    if let Err(reason) = self.audio_opened(opened).await {
                        return reason;
                    }
                }
                command = transfer_commands.recv() => {
                    let Some(command) = command else { continue };
                    let reply = self.transfers.command(command).await;
                    if let Err(reason) = self.send_reply(reply).await {
                        return reason;
                    }
                }
                step = transfer_steps.recv() => {
                    let Some(step) = step else { continue };
                    let reply = self.transfers.step(step).await;
                    if let Err(reason) = self.send_reply(reply).await {
                        return reason;
                    }
                }
                file = async {
                    match incoming_files.as_mut() {
                        Some(files) => files.recv().await,
                        None => std::future::pending().await,
                    }
                } => match file {
                    Some(Ok((id, stream))) => self.transfers.incoming_stream(id, stream),
                    Some(Err(reason)) => return reason,
                    None => incoming_files = None,
                },
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

    /// Starts input, the video stream, clipboard sync, and file transfer, and tells the viewer
    /// what it gets. Returns the viewer's file streams when file transfer is allowed.
    async fn start(
        &mut self,
        control_allowed: bool,
        status_updates: mpsc::Sender<Availability>,
        clipboard_out: mpsc::Sender<String>,
    ) -> Result<Option<IncomingFiles>, SessionEndReason> {
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
                .streams()
                .open_video_sender()
                .await
                .map_err(|error| SessionEndReason::ConnectionLost(error.to_string()))?;
            // Room for the frames the encoder overlaps; the capture thread still drops a frame
            // before encoding while an encoded one waits for the network.
            let (frames, frame_receiver) = mpsc::channel(FRAMES_IN_FLIGHT);
            self.frames = Some(frames);
            self.pump = Some(tokio::spawn(pump_video(
                frame_receiver,
                video,
                status_updates,
            )));
            self.stream = self.requested_stream();
            self.restart_capture().await;
        } else {
            self.status.input = if control_allowed {
                Availability::Unavailable
            } else {
                Availability::NotAllowed
            };
        }

        if control_allowed && self.options.policy.clipboard {
            let factory = self.platform.clipboard();
            self.clipboard =
                factory.and_then(|factory| ClipboardSync::start(factory, clipboard_out));
        }

        let incoming_files = if !control_allowed {
            self.status.files = Availability::NotAllowed;
            None
        } else if self.options.policy.file_transfer && self.options.downloads.is_some() {
            self.status.files = Availability::Available;
            self.transfers.set_allowed(true);
            let streams = self.link.streams();
            streams.allow_peer_streams(PEER_FILE_STREAMS);
            let (files, incoming) = mpsc::channel(PEER_FILE_STREAMS as usize);
            tokio::spawn(accept_file_streams(streams, files));
            Some(incoming)
        } else {
            self.status.files = Availability::Unavailable;
            None
        };

        // Audio is output like the screen, so view-only viewers may have it too. It starts only
        // once the viewer asks.
        self.status.audio = if self.options.policy.audio {
            Availability::Available
        } else {
            Availability::Unavailable
        };

        self.publish_status().await?;
        self.publish_displays().await?;
        self.publish_frame_rate().await?;
        Ok(incoming_files)
    }

    async fn publish_status(&mut self) -> Result<(), SessionEndReason> {
        let _sent = self.events.send(HostEvent::SessionStatus(self.status));
        self.send(&ControlMessage::HostStatus(
            self.status.for_version(self.peer_version),
        ))
        .await
    }

    /// Tells the viewer the frame rate the stream now runs at.
    async fn publish_frame_rate(&mut self) -> Result<(), SessionEndReason> {
        if self.capture.is_none() || self.reported_frame_rate == Some(self.stream.max_fps) {
            return Ok(());
        }
        self.reported_frame_rate = Some(self.stream.max_fps);
        let rate = u16::try_from(self.stream.max_fps).unwrap_or(u16::MAX);
        self.send(&ControlMessage::FrameRate(rate)).await
    }

    /// The stream the viewer's requests call for on the active display.
    fn requested_stream(&self) -> StreamSettings {
        let refresh = self
            .displays
            .iter()
            .find(|display| Some(display.id) == self.active_display)
            .map_or(0, |display| display.refresh_rate);
        stream_settings(self.request, refresh, self.options.stream)
    }

    /// Restarts capture if the viewer's requests changed the stream, and reports the result.
    async fn apply_request(&mut self) -> Result<(), SessionEndReason> {
        let settings = self.requested_stream();
        if settings != self.stream {
            self.stream = settings;
            self.restart_capture().await;
        }
        self.publish_frame_rate().await
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
        let settings = self.stream;
        match spawn_capture_stream(
            move || platform.open_capturer(display, settings),
            settings,
            frames,
        ) {
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
                    // The new display may refresh at a different rate.
                    self.stream = self.requested_stream();
                    self.restart_capture().await;
                    self.publish_status().await?;
                    self.publish_displays().await?;
                    self.publish_frame_rate().await?;
                }
            }
            ControlMessage::SetAudio(enabled) => {
                if enabled {
                    self.start_audio().await?;
                } else {
                    self.stop_audio().await;
                }
            }
            ControlMessage::SetQuality(preset) => {
                self.request.quality = Some(preset);
                self.apply_request().await?;
            }
            ControlMessage::SetFrameRate(rate) => {
                self.request.frame_rate = Some(rate);
                self.apply_request().await?;
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
                self.send_reply(reply).await?;
            }
            ControlMessage::Ping { token } => self.send(&ControlMessage::Pong { token }).await?,
            ControlMessage::Pong { .. } => {}
            ControlMessage::Disconnect => return Err(SessionEndReason::ViewerLeft),
            ControlMessage::HostStatus(_)
            | ControlMessage::AwaitingApproval
            | ControlMessage::Declined
            | ControlMessage::Displays { .. }
            | ControlMessage::FrameRate(_) => {
                return Err(SessionEndReason::ProtocolError(
                    "viewer sent a host message".into(),
                ));
            }
        }
        Ok(())
    }

    /// Starts sharing system audio if the policy allows it and it isn't running or opening yet.
    ///
    /// Opening can take as long as the user leaves macOS's permission prompt up, so it happens
    /// on a blocking thread and the session keeps serving input and control meanwhile; the
    /// result comes back as an [`AudioOpened`]. When opening will ask, the viewer is told the
    /// host is waiting for its user.
    async fn start_audio(&mut self) -> Result<(), SessionEndReason> {
        if !matches!(
            self.status.audio,
            Availability::Available | Availability::AwaitingPermission
        ) {
            return Ok(());
        }
        match &self.audio {
            AudioState::Opening(_) => return Ok(()),
            // The sender ends when the capture thread stopped (e.g. the device went away).
            AudioState::On(audio) if !audio.sender.is_finished() => return Ok(()),
            AudioState::On(_) | AudioState::Off => {}
        }
        if let AudioState::On(stale) = std::mem::replace(&mut self.audio, AudioState::Off) {
            stale.stop_in_background();
        }
        let Some(opened) = self.audio_opened.clone() else {
            return Ok(());
        };
        self.audio_generation += 1;
        let generation = self.audio_generation;
        self.audio = AudioState::Opening(generation);
        let platform = self.platform.clone();
        tokio::task::spawn_blocking(move || {
            let (packets, encoded) = mpsc::channel(AUDIO_QUEUE);
            let result = spawn_audio_stream(move || platform.open_audio(), packets)
                .map(|capture| (capture, encoded));
            if let Err(unsent) = opened.send(AudioOpened { generation, result })
                && let Ok((capture, _)) = unsent.0.result
            {
                // The session ended while the capturer was opening.
                capture.stop();
            }
        });
        if self.platform.audio_access() == PermissionState::NotDetermined
            && self.status.audio != Availability::AwaitingPermission
        {
            self.status.audio = Availability::AwaitingPermission;
            self.publish_status().await?;
        }
        Ok(())
    }

    /// Finishes [`HostSession::start_audio`]: starts sending if the capturer opened and the
    /// viewer still wants audio, or tells the viewer audio is unavailable.
    ///
    /// Any result settles a pending permission prompt, even one for an open the viewer has
    /// since muted, so the viewer is never left waiting on an answer that already came.
    async fn audio_opened(&mut self, opened: AudioOpened) -> Result<(), SessionEndReason> {
        let current = matches!(self.audio, AudioState::Opening(generation) if generation == opened.generation);
        let status = match opened.result {
            Ok((capture, encoded)) if current => {
                let sender = tokio::spawn(send_audio(self.link.streams(), encoded));
                self.audio = AudioState::On(SharedAudio { capture, sender });
                Availability::Available
            }
            // Muted, or asked again, while it was opening.
            Ok((capture, _)) => {
                tokio::task::spawn_blocking(move || capture.stop());
                Availability::Available
            }
            Err(error) => {
                warn!(%error, "system audio is unavailable");
                if current {
                    self.audio = AudioState::Off;
                }
                match error {
                    AudioError::PermissionDenied => Availability::PermissionDenied,
                    _ if current => Availability::Unavailable,
                    // A stale failure says nothing about the open that replaced it.
                    _ => Availability::Available,
                }
            }
        };
        let settles = self.status.audio == Availability::AwaitingPermission
            && !matches!(self.audio, AudioState::Opening(_));
        if self.status.audio != status
            && (status == Availability::PermissionDenied || settles || current)
        {
            self.status.audio = status;
            self.publish_status().await?;
        }
        Ok(())
    }

    async fn stop_audio(&mut self) {
        // An opening capturer is stopped when its result arrives and no longer matches.
        if let AudioState::On(audio) = std::mem::replace(&mut self.audio, AudioState::Off) {
            audio.sender.abort();
            // Stopping joins the capture thread; keep that off the async workers.
            let _joined = tokio::task::spawn_blocking(move || audio.capture.stop()).await;
        }
    }

    async fn shut_down(mut self, reason: &SessionEndReason) {
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
        self.stop_audio().await;
        // Stopping joins the capture thread; keep that off the async workers.
        if let Some(capture) = self.capture.take() {
            let _joined = tokio::task::spawn_blocking(move || capture.stop()).await;
        }
        // Dropping the queue stops injection at once and releases every held key and button.
        drop(self.input.take());
        drop(self.clipboard.take());
        self.transfers.shut_down().await;
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

/// Hands every file stream the viewer opens to the session until the connection ends. The
/// viewer may open nothing else.
async fn accept_file_streams(
    streams: SessionStreams,
    files: mpsc::Sender<Result<(TransferId, FileReceiver), SessionEndReason>>,
) {
    loop {
        let item = match streams.accept().await {
            Ok(IncomingStream::File { id, stream }) => Ok((id, stream)),
            Ok(IncomingStream::Video(_)) => Err(SessionEndReason::ProtocolError(
                "viewer opened a video stream".into(),
            )),
            Err(dari_net::StreamError::UnknownKind(kind)) => Err(SessionEndReason::ProtocolError(
                format!("viewer opened a stream of unknown kind {kind}"),
            )),
            // The control stream reports a lost connection.
            Err(_) => return,
        };
        let failed = item.is_err();
        if files.send(item).await.is_err() || failed {
            return;
        }
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
        StreamError::Capture(CaptureError::SecureDesktop) => Availability::SecureDesktop,
        StreamError::Capture(
            CaptureError::DisplayNotFound(_) | CaptureError::NoDisplay | CaptureError::Backend(_),
        )
        | StreamError::Codec(_) => Availability::Unavailable,
    }
}

async fn pump_video(
    mut frames: mpsc::Receiver<Result<EncodedFrame, StreamError>>,
    mut video: MessageSender<VideoPacket>,
    status: mpsc::Sender<Availability>,
) -> Result<(), String> {
    let started = Instant::now();
    let mut sequence = 0u64;
    let mut reported = Availability::Available;
    while let Some(frame) = frames.recv().await {
        match frame {
            Ok(frame) => {
                if reported != Availability::Available {
                    reported = Availability::Available;
                    let _sent = status.send(reported).await;
                }
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
                warn!(%error, "screen capture interrupted");
                reported = availability_of_capture(&error);
                let _sent = status.send(reported).await;
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
        .name("dari-input".into())
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

    fn request(quality: Option<QualityPreset>, frame_rate: Option<u16>) -> ViewerRequest {
        ViewerRequest {
            quality,
            frame_rate,
        }
    }

    #[test]
    fn quality_presets_scale_resolution_and_bitrate() {
        let base = StreamSettings::default();
        let speed = stream_settings(request(Some(QualityPreset::Speed), None), 0, base);
        let quality = stream_settings(request(Some(QualityPreset::Quality), None), 0, base);
        assert!(speed.max_long_edge < quality.max_long_edge);
        assert!(speed.bitrate_bps < quality.bitrate_bps);
        assert_eq!(speed.max_fps, base.max_fps);
    }

    #[test]
    fn without_requests_the_host_settings_apply() {
        let base = StreamSettings {
            max_long_edge: 1600,
            max_fps: 50,
            bitrate_bps: 3_000_000,
            hardware_encoder: false,
        };
        assert_eq!(stream_settings(ViewerRequest::default(), 120, base), base);
        // A faster request scales the host's bitrate from the host's own frame rate.
        let faster = stream_settings(request(None, Some(100)), 120, base);
        assert!(faster.bitrate_bps > base.bitrate_bps && faster.bitrate_bps < 2 * base.bitrate_bps);
    }

    #[test]
    fn frame_rate_follows_the_viewer_up_to_the_display_refresh() {
        let base = StreamSettings::default();
        let at = |rate, refresh| stream_settings(request(None, Some(rate)), refresh, base).max_fps;
        assert_eq!(at(144, 120), 120);
        assert_eq!(at(90, 120), 90);
        assert_eq!(at(144, 0), 144, "an unknown refresh rate does not limit");
        assert_eq!(at(60, 60), 60);
    }

    #[test]
    fn bitrate_grows_with_the_frame_rate_but_less_than_proportionally() {
        let base = StreamSettings::default();
        let bitrate = |rate| {
            stream_settings(request(Some(QualityPreset::Balanced), Some(rate)), 0, base).bitrate_bps
        };
        assert_eq!(bitrate(30), 4_000_000);
        assert!(bitrate(60) > bitrate(30) && bitrate(60) < 2 * bitrate(30));
        assert!(bitrate(144) > bitrate(120));
        let fastest = stream_settings(
            request(
                Some(QualityPreset::Quality),
                Some(dari_proto::MAX_FRAME_RATE),
            ),
            0,
            base,
        );
        assert!(fastest.bitrate_bps <= MAX_BITRATE_BPS);
    }
}
