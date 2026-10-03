//! Full host ↔ viewer sessions over loopback QUIC with a synthetic screen and recorded input.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers may panic"
)]

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dari_input::{InjectError, InputBackend, RecordedAction};
use dari_media::{
    AudioCapturer, AudioChunk, AudioError, AudioOutput, AudioOutputFactory, CaptureError,
    DisplayInfo, PlaybackBuffer, ScreenCapturer, StreamSettings, SyntheticAudioCapturer,
    SyntheticCapturer,
};
use dari_net::{AccessPassword, DeviceIdentity};
use dari_proto::TransferEnd;
use dari_proto::{Availability, InputEvent, KeyCode, MouseButton, NamedKey, PointerPosition};
use dari_session::{
    ApprovalDecision, ClipboardAccess, ClipboardFactory, HostConfig, HostEvent, HostPlatform,
    HostPolicy, RelayStatus, SessionEndReason, Transfer, TransferDirection, TransferState,
    ViewerConfig, ViewerEvent, ViewerTarget, connect_viewer, start_host,
};
use tokio::sync::mpsc;

const DISPLAY: DisplayInfo = DisplayInfo {
    id: 7,
    name: String::new(),
    x: 0,
    y: 0,
    width: 1280,
    height: 720,
    scale_factor: 1.0,
    is_primary: true,
};

#[derive(Default)]
struct TestPlatform {
    actions: Arc<Mutex<Vec<RecordedAction>>>,
    deny_input: bool,
    deny_capture: bool,
    clipboard: MemoryClipboard,
    /// Display ids opened by the capturer, in order.
    captured: Arc<Mutex<Vec<u32>>>,
    /// Audio capturers open right now.
    audio_live: Arc<AtomicUsize>,
}

/// A synthetic tone that counts itself in `live` while open.
struct LiveAudio {
    tone: SyntheticAudioCapturer,
    live: Arc<AtomicUsize>,
}

impl AudioCapturer for LiveAudio {
    fn next_chunk(&mut self, timeout: Duration) -> Result<Option<AudioChunk>, AudioError> {
        self.tone.next_chunk(timeout)
    }
}

impl Drop for LiveAudio {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Default)]
struct MemoryClipboard(Arc<Mutex<Option<String>>>);

impl ClipboardAccess for MemoryClipboard {
    fn read_text(&mut self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
    fn write_text(&mut self, text: &str) -> bool {
        *self.0.lock().unwrap() = Some(text.into());
        true
    }
}

impl MemoryClipboard {
    fn factory(&self) -> ClipboardFactory {
        let clipboard = self.clone();
        Arc::new(move || Some(Box::new(clipboard.clone()) as Box<dyn ClipboardAccess>))
    }
    fn set(&self, text: &str) {
        *self.0.lock().unwrap() = Some(text.into());
    }
    fn get(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
}

struct SharedRecorder(Arc<Mutex<Vec<RecordedAction>>>);

impl InputBackend for SharedRecorder {
    fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
        self.0.lock().unwrap().push(RecordedAction::Move(x, y));
        Ok(())
    }
    fn button(&mut self, button: MouseButton, pressed: bool) -> Result<(), InjectError> {
        self.0
            .lock()
            .unwrap()
            .push(RecordedAction::Button(button, pressed));
        Ok(())
    }
    fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
        self.0.lock().unwrap().push(RecordedAction::Scroll(dx, dy));
        Ok(())
    }
    fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
        self.0
            .lock()
            .unwrap()
            .push(RecordedAction::Key(key, pressed));
        Ok(())
    }
    fn text(&mut self, text: &str) -> Result<(), InjectError> {
        self.0
            .lock()
            .unwrap()
            .push(RecordedAction::Text(text.into()));
        Ok(())
    }
}

impl HostPlatform for TestPlatform {
    fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        Ok(vec![DISPLAY, SECOND_DISPLAY])
    }

    fn clipboard(&self) -> Option<ClipboardFactory> {
        Some(self.clipboard.factory())
    }
    fn open_capturer(&self, display: u32) -> Result<Box<dyn ScreenCapturer>, CaptureError> {
        self.captured.lock().unwrap().push(display);
        if self.deny_capture {
            return Err(CaptureError::PermissionDenied);
        }
        Ok(Box::new(SyntheticCapturer::new(320, 180)))
    }
    fn open_input(&self) -> Result<Box<dyn InputBackend>, InjectError> {
        if self.deny_input {
            return Err(InjectError::PermissionDenied);
        }
        Ok(Box::new(SharedRecorder(self.actions.clone())))
    }
    fn open_audio(&self) -> Result<Box<dyn AudioCapturer>, AudioError> {
        self.audio_live.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(LiveAudio {
            tone: SyntheticAudioCapturer::new(440.),
            live: self.audio_live.clone(),
        }))
    }
}

const SECOND_DISPLAY: DisplayInfo = DisplayInfo {
    id: 8,
    name: String::new(),
    x: 1280,
    y: 0,
    width: 1920,
    height: 1080,
    scale_factor: 1.0,
    is_primary: false,
};

struct Host {
    handle: dari_session::HostHandle,
    events: mpsc::UnboundedReceiver<HostEvent>,
    password: AccessPassword,
}

async fn start(platform: TestPlatform) -> Host {
    start_with(platform, false).await
}

async fn start_with(platform: TestPlatform, require_approval: bool) -> Host {
    start_host_with(platform, require_approval, None).await
}

async fn start_host_with(
    platform: TestPlatform,
    require_approval: bool,
    downloads: Option<PathBuf>,
) -> Host {
    let (handle, mut events) = start_host(
        HostConfig {
            bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            host_name: "test-host".into(),
            stream: StreamSettings {
                max_long_edge: 1920,
                max_fps: 30,
                bitrate_bps: 1_000_000,
            },
            policy: HostPolicy {
                require_approval,
                clipboard: true,
                file_transfer: downloads.is_some(),
                audio: true,
            },
            downloads,
            relay: None,
        },
        Arc::new(DeviceIdentity::generate().unwrap()),
        Arc::new(platform),
    )
    .unwrap();
    let password = match events.recv().await {
        Some(HostEvent::PasswordChanged(Some(password))) => password,
        other => panic!("expected a password, got {other:?}"),
    };
    Host {
        handle,
        events,
        password,
    }
}

fn viewer_config(host: &Host) -> ViewerConfig {
    ViewerConfig {
        target: ViewerTarget::Direct(host.handle.local_address()),
        client_name: "test-viewer".into(),
        map_shortcut_modifier: false,
        clipboard: None,
        downloads: None,
        audio: None,
        play_audio: true,
    }
}

async fn next_event<T>(events: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("event channel closed")
}

async fn wait_for(actions: &Arc<Mutex<Vec<RecordedAction>>>, expected: &RecordedAction) {
    for _ in 0..200 {
        if actions.lock().unwrap().contains(expected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "{expected:?} was never injected; got {:?}",
        actions.lock().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_sees_the_screen_and_controls_the_host() {
    let platform = TestPlatform::default();
    let actions = platform.actions.clone();
    let mut host = start(platform).await;

    let (viewer, mut viewer_events) = connect_viewer(viewer_config(&host), &host.password)
        .await
        .unwrap();
    assert_eq!(viewer.peer().name, "test-host");
    assert!(matches!(
        next_event(&mut host.events).await,
        HostEvent::SessionStarted(_)
    ));
    assert_eq!(
        next_event(&mut viewer_events).await,
        ViewerEvent::HostStatus(dari_proto::HostStatus {
            screen: Availability::Available,
            input: Availability::Available,
            files: Availability::Unavailable,
            audio: Availability::Available,
        })
    );

    // Frames arrive decoded at the stream size.
    let mut frames = viewer.frames();
    tokio::time::timeout(Duration::from_secs(10), frames.wait_for(Option::is_some))
        .await
        .unwrap()
        .unwrap();
    let frame = frames.borrow().clone().unwrap();
    assert_eq!((frame.width, frame.height), (320, 180));

    // Input lands on the host display.
    assert!(viewer.send_input(InputEvent::PointerMove(PointerPosition {
        x: u16::MAX,
        y: 0
    })));
    wait_for(&actions, &RecordedAction::Move(1279, 0)).await;
    let ctrl = KeyCode::Named(NamedKey::Control);
    assert!(viewer.send_input(InputEvent::Key {
        key: ctrl,
        pressed: true
    }));
    wait_for(&actions, &RecordedAction::Key(ctrl, true)).await;

    // Ending the session releases the key the viewer still held.
    viewer.disconnect();
    assert_eq!(
        wait_for_event(&mut viewer_events, |event| matches!(
            event,
            ViewerEvent::Ended(_)
        ))
        .await,
        ViewerEvent::Ended(SessionEndReason::ViewerLeft)
    );
    loop {
        match next_event(&mut host.events).await {
            HostEvent::SessionEnded { reason, .. } => {
                assert_eq!(reason, SessionEndReason::ViewerLeft);
                break;
            }
            HostEvent::SessionStatus(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    wait_for(&actions, &RecordedAction::Key(ctrl, false)).await;

    // The used password is replaced.
    match next_event(&mut host.events).await {
        HostEvent::PasswordChanged(Some(fresh)) => assert_ne!(fresh, host.password),
        other => panic!("expected a new password, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn host_can_end_the_session() {
    let mut host = start(TestPlatform::default()).await;
    let (_viewer, mut viewer_events) = connect_viewer(viewer_config(&host), &host.password)
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut host.events).await,
        HostEvent::SessionStarted(_)
    ));
    assert!(matches!(
        next_event(&mut viewer_events).await,
        ViewerEvent::HostStatus(_)
    ));
    host.handle.end_session();
    assert_eq!(
        wait_for_event(&mut viewer_events, |event| matches!(
            event,
            ViewerEvent::Ended(_)
        ))
        .await,
        ViewerEvent::Ended(SessionEndReason::HostEnded)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_input_permission_is_reported_and_the_screen_still_streams() {
    let host = start(TestPlatform {
        deny_input: true,
        ..TestPlatform::default()
    })
    .await;
    let (viewer, mut viewer_events) = connect_viewer(viewer_config(&host), &host.password)
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut viewer_events).await,
        ViewerEvent::HostStatus(dari_proto::HostStatus {
            screen: Availability::Available,
            input: Availability::PermissionDenied,
            files: Availability::Unavailable,
            audio: Availability::Available,
        })
    );
    let mut frames = viewer.frames();
    tokio::time::timeout(Duration::from_secs(10), frames.wait_for(Option::is_some))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stopped_host_does_not_accept_viewers() {
    let mut host = start(TestPlatform::default()).await;
    host.handle.set_accepting(false);
    assert!(matches!(
        next_event(&mut host.events).await,
        HostEvent::PasswordChanged(None)
    ));
    let result = connect_viewer(viewer_config(&host), &host.password).await;
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_capture_permission_keeps_the_session_and_says_why() {
    let host = start(TestPlatform {
        deny_capture: true,
        ..TestPlatform::default()
    })
    .await;
    let (viewer, mut viewer_events) = connect_viewer(viewer_config(&host), &host.password)
        .await
        .unwrap();
    let mut screen = None;
    while screen != Some(Availability::PermissionDenied) {
        match next_event(&mut viewer_events).await {
            ViewerEvent::HostStatus(status) => screen = Some(status.screen),
            ViewerEvent::Ended(reason) => panic!("session ended: {reason}"),
            ViewerEvent::AwaitingApproval
            | ViewerEvent::Displays { .. }
            | ViewerEvent::Transfer(_) => {}
        }
    }
    // The session survives the end of the video stream.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(event) = tokio::time::timeout_at(deadline, viewer_events.recv()).await {
        assert!(
            !matches!(event, Some(ViewerEvent::Ended(_)) | None),
            "unexpected {event:?}"
        );
    }
    assert!(viewer.send_input(InputEvent::Text("still here".into())));
}

/// Skips events until one matches.
async fn wait_for_event<T: std::fmt::Debug>(
    events: &mut mpsc::UnboundedReceiver<T>,
    mut matches: impl FnMut(&T) -> bool,
) -> T {
    loop {
        let event = next_event(events).await;
        if matches(&event) {
            return event;
        }
    }
}

async fn approve(host: &mut Host, decision: ApprovalDecision) {
    match wait_for_event(&mut host.events, |event| {
        matches!(event, HostEvent::ApprovalRequested { .. })
    })
    .await
    {
        HostEvent::ApprovalRequested { peer, request } => {
            assert_eq!(peer.name, "test-viewer");
            request.respond(decision);
        }
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_shared_until_the_host_user_allows_it() {
    let platform = TestPlatform::default();
    let captured = platform.captured.clone();
    let mut host = start_with(platform, true).await;
    let (viewer, mut viewer_events) = connect_viewer(viewer_config(&host), &host.password)
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut viewer_events).await,
        ViewerEvent::AwaitingApproval
    );
    // Input sent while waiting is ignored and nothing is captured.
    assert!(viewer.send_input(InputEvent::Text("too early".into())));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(captured.lock().unwrap().is_empty());

    approve(&mut host, ApprovalDecision::AllowControl).await;
    let status = wait_for_event(&mut viewer_events, |event| {
        matches!(event, ViewerEvent::HostStatus(_))
    })
    .await;
    assert_eq!(
        status,
        ViewerEvent::HostStatus(dari_proto::HostStatus {
            screen: Availability::Available,
            input: Availability::Available,
            files: Availability::Unavailable,
            audio: Availability::Available,
        })
    );
    let mut frames = viewer.frames();
    tokio::time::timeout(Duration::from_secs(10), frames.wait_for(Option::is_some))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn declined_viewers_are_told_and_disconnected() {
    let mut host = start_with(TestPlatform::default(), true).await;
    let (_viewer, mut viewer_events) = connect_viewer(viewer_config(&host), &host.password)
        .await
        .unwrap();
    approve(&mut host, ApprovalDecision::Deny).await;
    let ended = wait_for_event(&mut viewer_events, |event| {
        matches!(event, ViewerEvent::Ended(_))
    })
    .await;
    assert_eq!(ended, ViewerEvent::Ended(SessionEndReason::Declined));
    match wait_for_event(&mut host.events, |event| {
        matches!(event, HostEvent::SessionEnded { .. })
    })
    .await
    {
        HostEvent::SessionEnded { reason, .. } => assert_eq!(reason, SessionEndReason::Declined),
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn view_only_sessions_ignore_input() {
    let platform = TestPlatform::default();
    let actions = platform.actions.clone();
    let mut host = start_with(platform, true).await;
    let (viewer, mut viewer_events) = connect_viewer(viewer_config(&host), &host.password)
        .await
        .unwrap();
    approve(&mut host, ApprovalDecision::ViewOnly).await;
    let status = wait_for_event(&mut viewer_events, |event| {
        matches!(event, ViewerEvent::HostStatus(_))
    })
    .await;
    assert_eq!(
        status,
        ViewerEvent::HostStatus(dari_proto::HostStatus {
            screen: Availability::Available,
            input: Availability::NotAllowed,
            files: Availability::NotAllowed,
            audio: Availability::Available,
        })
    );
    assert!(viewer.send_input(InputEvent::Text("ignored".into())));
    let mut frames = viewer.frames();
    tokio::time::timeout(Duration::from_secs(10), frames.wait_for(Option::is_some))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(actions.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_can_switch_displays() {
    let platform = TestPlatform::default();
    let captured = platform.captured.clone();
    let actions = platform.actions.clone();
    let host = start(platform).await;
    let (viewer, mut viewer_events) = connect_viewer(viewer_config(&host), &host.password)
        .await
        .unwrap();
    let displays = wait_for_event(&mut viewer_events, |event| {
        matches!(event, ViewerEvent::Displays { .. })
    })
    .await;
    let ViewerEvent::Displays { displays, active } = displays else {
        unreachable!()
    };
    assert_eq!(displays.len(), 2);
    assert_eq!(active, DISPLAY.id);

    viewer.select_display(SECOND_DISPLAY.id);
    let switched = wait_for_event(&mut viewer_events, |event| {
        matches!(event, ViewerEvent::Displays { .. })
    })
    .await;
    assert!(
        matches!(switched, ViewerEvent::Displays { active, .. } if active == SECOND_DISPLAY.id)
    );
    // The capturer opens on its own thread, so the switch may be reported before it ran.
    wait_until(|| *captured.lock().unwrap() == [DISPLAY.id, SECOND_DISPLAY.id]).await;

    // Pointer input now lands on the second display.
    assert!(viewer.send_input(InputEvent::PointerMove(PointerPosition { x: 0, y: 0 })));
    wait_for(&actions, &RecordedAction::Move(SECOND_DISPLAY.x, 0)).await;

    // Unknown displays are ignored.
    viewer.select_display(999);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(captured.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn clipboard_text_flows_both_ways_when_control_is_allowed() {
    let platform = TestPlatform::default();
    let host_clipboard = platform.clipboard.clone();
    let host = start(platform).await;
    let viewer_clipboard = MemoryClipboard::default();
    let mut config = viewer_config(&host);
    config.clipboard = Some(viewer_clipboard.factory());
    let (_viewer, mut viewer_events) = connect_viewer(config, &host.password).await.unwrap();
    wait_for_event(&mut viewer_events, |event| {
        matches!(event, ViewerEvent::HostStatus(_))
    })
    .await;
    // Let both sides record their starting clipboard before changing it.
    tokio::time::sleep(Duration::from_millis(600)).await;

    viewer_clipboard.set("copied on the viewer");
    wait_until(|| host_clipboard.get().as_deref() == Some("copied on the viewer")).await;
    host_clipboard.set("copied on the host");
    wait_until(|| viewer_clipboard.get().as_deref() == Some("copied on the host")).await;
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition never became true");
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_reaches_the_host_service_by_relay_id() {
    let data = tempfile::tempdir().unwrap();
    let relay = dari_relay::RelayServer::start(&dari_relay::RelayConfig {
        listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        data_directory: data.path().to_owned(),
        max_allocations: 4,
    })
    .unwrap();
    let relay_address = relay.local_address().unwrap().to_string();
    let (_handle, mut events) = start_host(
        HostConfig {
            bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            host_name: "relayed".into(),
            stream: StreamSettings::default(),
            policy: HostPolicy {
                require_approval: false,
                clipboard: false,
                file_transfer: false,
                audio: false,
            },
            downloads: None,
            relay: Some(relay_address.clone()),
        },
        Arc::new(DeviceIdentity::generate().unwrap()),
        Arc::new(TestPlatform::default()),
    )
    .unwrap();
    let mut password = None;
    let mut id = None;
    while password.is_none() || id.is_none() {
        match next_event(&mut events).await {
            HostEvent::PasswordChanged(Some(fresh)) => password = Some(fresh),
            HostEvent::Relay(RelayStatus::Registered(registered)) => id = Some(registered),
            HostEvent::Relay(RelayStatus::Unavailable(error)) => {
                panic!("relay unavailable: {error}")
            }
            _ => {}
        }
    }

    let (viewer, _viewer_events) = connect_viewer(
        ViewerConfig {
            target: ViewerTarget::Relay {
                relay: relay_address,
                id: id.unwrap(),
            },
            client_name: "remote".into(),
            map_shortcut_modifier: false,
            clipboard: None,
            downloads: None,
            audio: None,
            play_audio: true,
        },
        &password.unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(viewer.peer().name, "relayed");
    let mut frames = viewer.frames();
    tokio::time::timeout(Duration::from_secs(10), frames.wait_for(Option::is_some))
        .await
        .unwrap()
        .unwrap();
}

/// A file with recognizable contents in its own directory.
fn sample_file(dir: &Path, name: &str, size: usize) -> (PathBuf, Vec<u8>) {
    let contents: Vec<u8> = (0..size)
        .map(|index| u8::try_from(index * 31 % 251).unwrap())
        .collect();
    let path = dir.join(name);
    std::fs::write(&path, &contents).unwrap();
    (path, contents)
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn host_transfer(event: HostEvent) -> Option<Transfer> {
    match event {
        HostEvent::Transfer(transfer) => Some(transfer),
        _ => None,
    }
}

fn viewer_transfer(event: ViewerEvent) -> Option<Transfer> {
    match event {
        ViewerEvent::Transfer(transfer) => Some(transfer),
        _ => None,
    }
}

/// Waits for a transfer update in `state` (or any finished state when `state` is `None`).
async fn wait_for_transfer<T: std::fmt::Debug>(
    events: &mut mpsc::UnboundedReceiver<T>,
    as_transfer: fn(T) -> Option<Transfer>,
    finished_or: Option<TransferState>,
) -> Transfer {
    loop {
        let Some(transfer) = as_transfer(next_event(events).await) else {
            continue;
        };
        let wanted = match finished_or {
            Some(state) => transfer.state == state || transfer.state.is_finished(),
            None => transfer.state.is_finished(),
        };
        if wanted {
            return transfer;
        }
    }
}

struct FileSession {
    host: Host,
    viewer: dari_session::ViewerHandle,
    viewer_events: mpsc::UnboundedReceiver<ViewerEvent>,
    host_downloads: tempfile::TempDir,
    viewer_downloads: tempfile::TempDir,
    sources: tempfile::TempDir,
}

async fn file_session() -> FileSession {
    let host_downloads = tempfile::tempdir().unwrap();
    let viewer_downloads = tempfile::tempdir().unwrap();
    let host = start_host_with(
        TestPlatform::default(),
        false,
        Some(host_downloads.path().to_owned()),
    )
    .await;
    let mut config = viewer_config(&host);
    config.downloads = Some(viewer_downloads.path().to_owned());
    let (viewer, mut viewer_events) = connect_viewer(config, &host.password).await.unwrap();
    let status = wait_for_event(&mut viewer_events, |event| {
        matches!(event, ViewerEvent::HostStatus(_))
    })
    .await;
    assert!(
        matches!(status, ViewerEvent::HostStatus(status) if status.files == Availability::Available)
    );
    FileSession {
        host,
        viewer,
        viewer_events,
        host_downloads,
        viewer_downloads,
        sources: tempfile::tempdir().unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn files_flow_both_ways() {
    let mut session = file_session().await;

    // Viewer → host: the host saves it without asking. The name arrives composed (NFC) even
    // though it was decomposed on disk, as macOS stores it.
    let decomposed = "\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}.bin";
    let (upload, contents) = sample_file(session.sources.path(), decomposed, 300_001);
    session.viewer.send_file(upload);
    let received = wait_for_transfer(&mut session.host.events, host_transfer, None).await;
    assert_eq!(received.state, TransferState::Completed);
    assert_eq!(received.direction, TransferDirection::Receiving);
    assert_eq!(received.name, "한글.bin");
    let saved = received.saved_to.unwrap();
    assert_eq!(saved, session.host_downloads.path().join("한글.bin"));
    assert_eq!(std::fs::read(&saved).unwrap(), contents);
    let sent = wait_for_transfer(&mut session.viewer_events, viewer_transfer, None).await;
    assert_eq!(sent.state, TransferState::Completed);
    assert_eq!(sent.transferred, 300_001);

    // Host → viewer: the viewer user accepts first. An existing file is never replaced.
    std::fs::write(session.viewer_downloads.path().join("report.pdf"), b"mine").unwrap();
    let (download, contents) = sample_file(session.sources.path(), "report.pdf", 1_000_000);
    session.host.handle.send_file(download);
    let offer = wait_for_transfer(
        &mut session.viewer_events,
        viewer_transfer,
        Some(TransferState::Offered),
    )
    .await;
    assert_eq!(offer.state, TransferState::Offered);
    assert_eq!((offer.name.as_str(), offer.size), ("report.pdf", 1_000_000));
    session.viewer.accept_transfer(offer.id);
    let received = wait_for_transfer(&mut session.viewer_events, viewer_transfer, None).await;
    assert_eq!(received.state, TransferState::Completed);
    let saved = received.saved_to.unwrap();
    assert_eq!(
        saved,
        session.viewer_downloads.path().join("report (1).pdf")
    );
    assert_eq!(std::fs::read(&saved).unwrap(), contents);
    let sent = wait_for_transfer(&mut session.host.events, host_transfer, None).await;
    assert_eq!(sent.state, TransferState::Completed);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_viewer_can_decline_a_file() {
    let mut session = file_session().await;
    let (download, _contents) = sample_file(session.sources.path(), "unwanted.exe", 1000);
    session.host.handle.send_file(download);
    let offer = wait_for_transfer(
        &mut session.viewer_events,
        viewer_transfer,
        Some(TransferState::Offered),
    )
    .await;
    session.viewer.cancel_transfer(offer.id);
    let declined = wait_for_transfer(&mut session.host.events, host_transfer, None).await;
    assert_eq!(declined.state, TransferState::Ended(TransferEnd::Declined));
    assert_eq!(
        files_in(session.viewer_downloads.path()),
        Vec::<String>::new()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_transfer_leaves_no_partial_file() {
    let mut session = file_session().await;
    let (download, _contents) = sample_file(session.sources.path(), "big.iso", 8_000_000);
    session.host.handle.send_file(download);
    let offer = wait_for_transfer(
        &mut session.viewer_events,
        viewer_transfer,
        Some(TransferState::Offered),
    )
    .await;
    // Accept and cancel at once: the host may already be streaming when the cancel arrives.
    session.viewer.accept_transfer(offer.id);
    session.viewer.cancel_transfer(offer.id);
    let ended = wait_for_transfer(&mut session.host.events, host_transfer, None).await;
    assert_eq!(ended.state, TransferState::Ended(TransferEnd::Cancelled));
    let ended = wait_for_transfer(&mut session.viewer_events, viewer_transfer, None).await;
    assert_eq!(ended.state, TransferState::Ended(TransferEnd::Cancelled));
    // A late stream for the cancelled transfer is dropped, and the session goes on.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        files_in(session.viewer_downloads.path()),
        Vec::<String>::new()
    );
    let (next, _contents) = sample_file(session.sources.path(), "next.txt", 10);
    session.viewer.send_file(next);
    let received = wait_for_transfer(&mut session.host.events, host_transfer, None).await;
    assert_eq!(received.state, TransferState::Completed);
}

#[tokio::test(flavor = "multi_thread")]
async fn view_only_sessions_transfer_no_files() {
    let host_downloads = tempfile::tempdir().unwrap();
    let mut host = start_host_with(
        TestPlatform::default(),
        true,
        Some(host_downloads.path().to_owned()),
    )
    .await;
    let viewer_downloads = tempfile::tempdir().unwrap();
    let mut config = viewer_config(&host);
    config.downloads = Some(viewer_downloads.path().to_owned());
    let (viewer, mut viewer_events) = connect_viewer(config, &host.password).await.unwrap();
    approve(&mut host, ApprovalDecision::ViewOnly).await;
    wait_for_event(&mut viewer_events, |event| {
        matches!(event, ViewerEvent::HostStatus(status) if status.files == Availability::NotAllowed)
    })
    .await;

    let sources = tempfile::tempdir().unwrap();
    let (upload, _contents) = sample_file(sources.path(), "upload.txt", 10);
    viewer.send_file(upload);
    let refused = wait_for_transfer(&mut viewer_events, viewer_transfer, None).await;
    assert_eq!(refused.state, TransferState::Ended(TransferEnd::Declined));

    let (download, _contents) = sample_file(sources.path(), "download.txt", 10);
    host.handle.send_file(download);
    let refused = wait_for_transfer(&mut host.events, host_transfer, None).await;
    assert_eq!(refused.state, TransferState::Ended(TransferEnd::Declined));

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(files_in(host_downloads.path()), Vec::<String>::new());
    assert_eq!(files_in(viewer_downloads.path()), Vec::<String>::new());
}

/// An output device that plays its buffer in real time and remembers the loudest sample.
struct RecordingOutput {
    buffer: PlaybackBuffer,
    stop: Arc<AtomicBool>,
}

impl AudioOutput for RecordingOutput {
    fn buffer(&self) -> &PlaybackBuffer {
        &self.buffer
    }
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn channels(&self) -> u16 {
        2
    }
}

impl Drop for RecordingOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Records the loudest sample played into `peak`, in thousandths.
fn recording_output(peak: Arc<AtomicUsize>) -> AudioOutputFactory {
    Arc::new(move || {
        let buffer = PlaybackBuffer::new(48_000, 2);
        let stop = Arc::new(AtomicBool::new(false));
        let (playing, stopped, peak) = (buffer.clone(), stop.clone(), peak.clone());
        std::thread::spawn(move || {
            let mut period = vec![0.; 960];
            while !stopped.load(Ordering::SeqCst) {
                playing.fill(&mut period);
                let loudest = period
                    .iter()
                    .fold(0f32, |loudest, sample| loudest.max(sample.abs()));
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "test"
                )]
                peak.fetch_max((loudest * 1000.) as usize, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(Box::new(RecordingOutput { buffer, stop }) as Box<dyn AudioOutput>)
    })
}

fn audio_viewer_config(host: &Host, peak: &Arc<AtomicUsize>) -> ViewerConfig {
    let mut config = viewer_config(host);
    config.audio = Some(recording_output(peak.clone()));
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hosts_audio_plays_on_the_viewer_and_stops_when_muted() {
    let platform = TestPlatform::default();
    let live = platform.audio_live.clone();
    let host = start(platform).await;
    let peak = Arc::new(AtomicUsize::new(0));
    let (viewer, _viewer_events) =
        connect_viewer(audio_viewer_config(&host, &peak), &host.password)
            .await
            .unwrap();
    // The 0.5-amplitude tone survives capture, resampling, Opus, datagrams, and playback.
    wait_until(|| peak.load(Ordering::SeqCst) > 300).await;
    assert_eq!(live.load(Ordering::SeqCst), 1);

    // Muting stops capture on the host, and unmuting starts it again.
    viewer.set_audio(false);
    wait_until(|| live.load(Ordering::SeqCst) == 0).await;
    viewer.set_audio(true);
    wait_until(|| live.load(Ordering::SeqCst) == 1).await;
    drop(viewer);
    wait_until(|| live.load(Ordering::SeqCst) == 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn view_only_sessions_still_hear_the_host() {
    let platform = TestPlatform::default();
    let live = platform.audio_live.clone();
    let mut host = start_with(platform, true).await;
    let peak = Arc::new(AtomicUsize::new(0));
    let (_viewer, _viewer_events) =
        connect_viewer(audio_viewer_config(&host, &peak), &host.password)
            .await
            .unwrap();
    // Nothing is captured while the host user decides.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(live.load(Ordering::SeqCst), 0);
    approve(&mut host, ApprovalDecision::ViewOnly).await;
    wait_until(|| peak.load(Ordering::SeqCst) > 300).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hosts_capture_audio_only_for_viewers_that_ask() {
    let platform = TestPlatform::default();
    let live = platform.audio_live.clone();
    let host = start(platform).await;
    // This viewer has no audio output, so it never asks.
    let (_viewer, mut viewer_events) = connect_viewer(viewer_config(&host), &host.password)
        .await
        .unwrap();
    let status = wait_for_event(&mut viewer_events, |event| {
        matches!(event, ViewerEvent::HostStatus(_))
    })
    .await;
    assert!(
        matches!(status, ViewerEvent::HostStatus(status) if status.audio == Availability::Available)
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(live.load(Ordering::SeqCst), 0);
}
