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
    DisplayInfo, PermissionState, PlaybackBuffer, ScreenCapturer, StreamSettings,
    SyntheticAudioCapturer, SyntheticCapturer,
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
    refresh_rate: 120,
};

#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent switches, each simulating one host condition"
)]
struct TestPlatform {
    actions: Arc<Mutex<Vec<RecordedAction>>>,
    deny_input: bool,
    deny_capture: bool,
    clipboard: MemoryClipboard,
    /// Display ids opened by the capturer, in order.
    captured: Arc<Mutex<Vec<u32>>>,
    /// The stream settings each capturer was opened with, in order.
    opened: Arc<Mutex<Vec<StreamSettings>>>,
    /// Audio capturers open right now.
    audio_live: Arc<AtomicUsize>,
    /// How long opening the audio capturer takes, like macOS's permission prompt.
    audio_open_delay: Duration,
    /// The host user refused system audio recording (macOS privacy settings).
    deny_audio: bool,
    /// The host user hasn't been asked about system audio yet, so opening it asks.
    audio_unasked: bool,
    /// Capture like Windows.Graphics.Capture and ScreenCaptureKit: one frame, then nothing
    /// until the screen changes, which it never does.
    still_screen: bool,
    secure_desktop: Arc<AtomicBool>,
}

struct HideableCapturer {
    screen: SyntheticCapturer,
    hidden: Arc<AtomicBool>,
}

impl ScreenCapturer for HideableCapturer {
    fn capture(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<dari_media::CapturedFrame>, CaptureError> {
        if self.hidden.load(Ordering::SeqCst) {
            std::thread::sleep(timeout);
            return Err(CaptureError::SecureDesktop);
        }
        self.screen.capture(timeout)
    }
}

/// A screen that never changes after its first frame.
struct StillCapturer {
    screen: SyntheticCapturer,
    shown: bool,
}

impl ScreenCapturer for StillCapturer {
    fn capture(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<dari_media::CapturedFrame>, CaptureError> {
        if self.shown {
            std::thread::sleep(timeout);
            return Ok(None);
        }
        self.shown = true;
        self.screen.capture(timeout)
    }
    fn paces_itself(&self) -> bool {
        true
    }
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
    fn open_capturer(
        &self,
        display: u32,
        settings: StreamSettings,
    ) -> Result<Box<dyn ScreenCapturer>, CaptureError> {
        self.captured.lock().unwrap().push(display);
        self.opened.lock().unwrap().push(settings);
        if self.deny_capture {
            return Err(CaptureError::PermissionDenied);
        }
        if self.still_screen {
            return Ok(Box::new(StillCapturer {
                screen: SyntheticCapturer::new(320, 180),
                shown: false,
            }));
        }
        Ok(Box::new(HideableCapturer {
            screen: SyntheticCapturer::new(320, 180),
            hidden: self.secure_desktop.clone(),
        }))
    }
    fn open_input(&self) -> Result<Box<dyn InputBackend>, InjectError> {
        if self.deny_input {
            return Err(InjectError::PermissionDenied);
        }
        Ok(Box::new(SharedRecorder(self.actions.clone())))
    }
    fn audio_access(&self) -> PermissionState {
        if self.audio_unasked {
            PermissionState::NotDetermined
        } else {
            PermissionState::NotRequired
        }
    }
    fn open_audio(&self) -> Result<Box<dyn AudioCapturer>, AudioError> {
        std::thread::sleep(self.audio_open_delay);
        if self.deny_audio {
            return Err(AudioError::PermissionDenied);
        }
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
    refresh_rate: 60,
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
                hardware_encoder: true,
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
        frame_rate: 30,
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
async fn a_still_screen_is_sent_again_when_the_viewer_asks_for_a_keyframe() {
    let host = start(TestPlatform {
        still_screen: true,
        ..TestPlatform::default()
    })
    .await;
    let (viewer, _viewer_events) = connect_viewer(viewer_config(&host), &host.password)
        .await
        .unwrap();
    let mut frames = viewer.frames();
    tokio::time::timeout(Duration::from_secs(10), frames.wait_for(Option::is_some))
        .await
        .unwrap()
        .unwrap();
    frames.borrow_and_update();

    // Nothing changes on the host, so nothing new arrives...
    assert!(
        tokio::time::timeout(Duration::from_millis(500), frames.changed())
            .await
            .is_err()
    );
    // ...until the viewer asks.
    viewer.request_keyframe();
    tokio::time::timeout(Duration::from_secs(10), frames.changed())
        .await
        .expect("the host sent its still screen again")
        .unwrap();
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
            | ViewerEvent::FrameRate(_)
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
async fn frame_rate_follows_the_viewer_up_to_the_display_refresh() {
    let platform = TestPlatform::default();
    let opened = platform.opened.clone();
    let host = start(platform).await;
    let config = ViewerConfig {
        frame_rate: 144,
        ..viewer_config(&host)
    };
    let (viewer, mut viewer_events) = connect_viewer(config, &host.password).await.unwrap();
    let wait_for_rate = async |events: &mut mpsc::UnboundedReceiver<ViewerEvent>, rate| {
        wait_for_event(events, |event| *event == ViewerEvent::FrameRate(rate)).await;
    };

    // The first display refreshes at 120 Hz, so that is as fast as the stream goes.
    wait_for_rate(&mut viewer_events, 120).await;
    wait_until(|| opened.lock().unwrap().last().map(|stream| stream.max_fps) == Some(120)).await;
    let fast = *opened.lock().unwrap().last().unwrap();
    assert!(
        fast.bitrate_bps > 1_000_000,
        "a faster stream gets more bandwidth"
    );
    let frames = viewer.frames();
    wait_until(|| frames.borrow().is_some()).await;

    viewer.set_frame_rate(30);
    wait_for_rate(&mut viewer_events, 30).await;
    wait_until(|| opened.lock().unwrap().last().map(|stream| stream.max_fps) == Some(30)).await;

    // The second display refreshes at 60 Hz.
    viewer.set_frame_rate(144);
    wait_for_rate(&mut viewer_events, 120).await;
    viewer.select_display(SECOND_DISPLAY.id);
    wait_for_rate(&mut viewer_events, 60).await;
    wait_until(|| opened.lock().unwrap().last().map(|stream| stream.max_fps) == Some(60)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_approval_the_stream_starts_at_the_requested_rate() {
    let platform = TestPlatform::default();
    let opened = platform.opened.clone();
    let host = start(platform).await;
    let config = ViewerConfig {
        frame_rate: 60,
        ..viewer_config(&host)
    };
    let (_viewer, mut viewer_events) = connect_viewer(config, &host.password).await.unwrap();
    // The first rate reported is the requested one, not the default the host would otherwise
    // start with.
    let first = wait_for_event(&mut viewer_events, |event| {
        matches!(event, ViewerEvent::FrameRate(_))
    })
    .await;
    assert_eq!(first, ViewerEvent::FrameRate(60));
    assert_single_capture_at(&opened, 60).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_frame_rate_asked_for_before_approval_applies_from_the_start() {
    let platform = TestPlatform::default();
    let opened = platform.opened.clone();
    let mut host = start_with(platform, true).await;
    let config = ViewerConfig {
        frame_rate: 60,
        ..viewer_config(&host)
    };
    let (_viewer, mut viewer_events) = connect_viewer(config, &host.password).await.unwrap();
    // The host user takes a moment to decide; the request has long arrived by then.
    tokio::time::sleep(Duration::from_millis(200)).await;
    approve(&mut host, ApprovalDecision::AllowControl).await;
    wait_for_event(&mut viewer_events, |event| {
        *event == ViewerEvent::FrameRate(60)
    })
    .await;
    assert_single_capture_at(&opened, 60).await;
}

/// Checks that capture opened once, at `max_fps`, rather than at a default and again after.
async fn assert_single_capture_at(opened: &Arc<Mutex<Vec<StreamSettings>>>, max_fps: u32) {
    // The capturer opens on its own thread, so the host may report the rate before it ran.
    wait_until(|| !opened.lock().unwrap().is_empty()).await;
    // A restart would follow within a loopback round trip; give it ample time to show up.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let opened = opened.lock().unwrap().clone();
    assert_eq!(opened.len(), 1, "capture restarted: {opened:?}");
    assert_eq!(opened[0].max_fps, max_fps);
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
    for _ in 0..400 {
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
            frame_rate: 30,
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
async fn a_refused_sound_permission_reaches_the_viewer() {
    let host = start(TestPlatform {
        deny_audio: true,
        ..TestPlatform::default()
    })
    .await;
    let peak = Arc::new(AtomicUsize::new(0));
    let (_viewer, mut viewer_events) =
        connect_viewer(audio_viewer_config(&host, &peak), &host.password)
            .await
            .unwrap();
    // Not "unavailable": the viewer can tell the host user what to allow.
    wait_for_event(&mut viewer_events, |event| {
        matches!(
            event,
            ViewerEvent::HostStatus(status) if status.audio == Availability::PermissionDenied
        )
    })
    .await;
}

struct StatusChanges {
    field: fn(dari_proto::HostStatus) -> Availability,
    last: Option<Availability>,
}

impl StatusChanges {
    fn of(field: fn(dari_proto::HostStatus) -> Availability) -> Self {
        Self { field, last: None }
    }

    /// Waits for the host to report a different availability, and returns it.
    async fn next_change(
        &mut self,
        events: &mut mpsc::UnboundedReceiver<ViewerEvent>,
    ) -> Availability {
        loop {
            if let ViewerEvent::HostStatus(status) = next_event(events).await {
                let current = (self.field)(status);
                if self.last.replace(current) != Some(current) {
                    return current;
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secure_desktop_on_the_host_is_reported_until_frames_resume() {
    let platform = TestPlatform::default();
    let secure_desktop = platform.secure_desktop.clone();
    let host = start(platform).await;
    let (viewer, mut viewer_events) = connect_viewer(viewer_config(&host), &host.password)
        .await
        .unwrap();
    let mut screen = StatusChanges::of(|status| status.screen);
    assert_eq!(
        screen.next_change(&mut viewer_events).await,
        Availability::Available
    );
    let mut frames = viewer.frames();
    tokio::time::timeout(Duration::from_secs(10), frames.wait_for(Option::is_some))
        .await
        .unwrap()
        .unwrap();

    secure_desktop.store(true, Ordering::SeqCst);
    assert_eq!(
        screen.next_change(&mut viewer_events).await,
        Availability::SecureDesktop
    );
    frames.mark_unchanged();

    secure_desktop.store(false, Ordering::SeqCst);
    assert_eq!(
        screen.next_change(&mut viewer_events).await,
        Availability::Available
    );
    tokio::time::timeout(Duration::from_secs(10), frames.changed())
        .await
        .expect("frames resume once the secure desktop is gone")
        .unwrap();
}

/// A host whose OS asks its user before the first recording, answering after `delay`.
fn asking_platform(refuse: bool) -> TestPlatform {
    TestPlatform {
        audio_unasked: true,
        deny_audio: refuse,
        audio_open_delay: Duration::from_millis(500),
        ..TestPlatform::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_viewer_hears_once_the_host_user_allows_recording() {
    let host = start(asking_platform(false)).await;
    let peak = Arc::new(AtomicUsize::new(0));
    let (_viewer, mut viewer_events) =
        connect_viewer(audio_viewer_config(&host, &peak), &host.password)
            .await
            .unwrap();
    let mut audio = StatusChanges::of(|status| status.audio);
    let mut changes = Vec::new();
    for _ in 0..3 {
        changes.push(audio.next_change(&mut viewer_events).await);
    }
    assert_eq!(
        changes,
        [
            Availability::Available,
            Availability::AwaitingPermission,
            Availability::Available
        ]
    );
    wait_until(|| peak.load(Ordering::SeqCst) > 300).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_viewer_learns_the_host_user_refused_recording() {
    let host = start(asking_platform(true)).await;
    let peak = Arc::new(AtomicUsize::new(0));
    let (_viewer, mut viewer_events) =
        connect_viewer(audio_viewer_config(&host, &peak), &host.password)
            .await
            .unwrap();
    let mut audio = StatusChanges::of(|status| status.audio);
    let mut changes = Vec::new();
    for _ in 0..3 {
        changes.push(audio.next_change(&mut viewer_events).await);
    }
    assert_eq!(
        changes,
        [
            Availability::Available,
            Availability::AwaitingPermission,
            Availability::PermissionDenied
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn muting_during_the_prompt_does_not_leave_the_viewer_waiting() {
    let host = start(asking_platform(false)).await;
    let peak = Arc::new(AtomicUsize::new(0));
    let (viewer, mut viewer_events) =
        connect_viewer(audio_viewer_config(&host, &peak), &host.password)
            .await
            .unwrap();
    let mut audio = StatusChanges::of(|status| status.audio);
    audio.next_change(&mut viewer_events).await;
    assert_eq!(
        audio.next_change(&mut viewer_events).await,
        Availability::AwaitingPermission
    );
    viewer.set_audio(false);
    // The answer still comes, and settles the status for a later unmute.
    assert_eq!(
        audio.next_change(&mut viewer_events).await,
        Availability::Available
    );
    viewer.set_audio(true);
    wait_until(|| peak.load(Ordering::SeqCst) > 300).await;
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

/// Every file below `dir`, as `relative/path` → contents.
fn tree(dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut found = std::collections::BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(dir).unwrap();
                let key = relative
                    .iter()
                    .map(|component| component.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                found.insert(key, std::fs::read(&path).unwrap());
            }
        }
    }
    found
}

fn sample_folder(dir: &Path, name: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join("2026/여름")).unwrap();
    std::fs::write(root.join("readme.txt"), b"hello").unwrap();
    std::fs::write(root.join("empty.bin"), b"").unwrap();
    sample_file(&root.join("2026"), "big.bin", 700_001);
    std::fs::write(root.join("2026/여름/바다.jpg"), vec![9u8; 4321]).unwrap();
    root
}

#[tokio::test(flavor = "multi_thread")]
async fn folders_flow_both_ways_with_their_structure() {
    let mut session = file_session().await;

    // Viewer → host.
    let upload = sample_folder(session.sources.path(), "사진");
    session.viewer.send_file(upload.clone());
    let received = wait_for_transfer(&mut session.host.events, host_transfer, None).await;
    assert_eq!(received.state, TransferState::Completed);
    assert_eq!(received.files, Some(4));
    let saved = received.saved_to.unwrap();
    assert_eq!(saved, session.host_downloads.path().join("사진"));
    assert_eq!(tree(&saved), tree(&upload));
    let sent = wait_for_transfer(&mut session.viewer_events, viewer_transfer, None).await;
    assert_eq!(sent.state, TransferState::Completed);

    // Host → viewer, into a downloads folder that already has one by that name.
    std::fs::create_dir(session.viewer_downloads.path().join("사진")).unwrap();
    session.host.handle.send_file(upload.clone());
    let offer = wait_for_transfer(
        &mut session.viewer_events,
        viewer_transfer,
        Some(TransferState::Offered),
    )
    .await;
    assert_eq!((offer.name.as_str(), offer.files), ("사진", Some(4)));
    session.viewer.accept_transfer(offer.id);
    let received = wait_for_transfer(&mut session.viewer_events, viewer_transfer, None).await;
    assert_eq!(received.state, TransferState::Completed);
    let saved = received.saved_to.unwrap();
    assert_eq!(saved, session.viewer_downloads.path().join("사진 (1)"));
    assert_eq!(tree(&saved), tree(&upload));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_folder_leaves_nothing_behind() {
    let mut session = file_session().await;
    let upload = sample_folder(session.sources.path(), "big");
    sample_file(&upload, "huge.bin", 8_000_000);
    session.host.handle.send_file(upload);
    let offer = wait_for_transfer(
        &mut session.viewer_events,
        viewer_transfer,
        Some(TransferState::Offered),
    )
    .await;
    session.viewer.accept_transfer(offer.id);
    session.viewer.cancel_transfer(offer.id);
    let ended = wait_for_transfer(&mut session.host.events, host_transfer, None).await;
    assert_eq!(ended.state, TransferState::Ended(TransferEnd::Cancelled));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        files_in(session.viewer_downloads.path()),
        Vec::<String>::new()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_audio_device_does_not_stall_the_session() {
    let platform = TestPlatform {
        audio_open_delay: Duration::from_secs(3),
        ..TestPlatform::default()
    };
    let live = platform.audio_live.clone();
    let actions = platform.actions.clone();
    let host = start(platform).await;
    let peak = Arc::new(AtomicUsize::new(0));
    let (viewer, mut viewer_events) =
        connect_viewer(audio_viewer_config(&host, &peak), &host.password)
            .await
            .unwrap();
    wait_for_event(&mut viewer_events, |event| {
        matches!(event, ViewerEvent::HostStatus(_))
    })
    .await;

    // While the capturer is still opening, input keeps flowing.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let opened_at = std::time::Instant::now();
    assert!(viewer.send_input(InputEvent::Text("while audio opens".into())));
    wait_for(&actions, &RecordedAction::Text("while audio opens".into())).await;
    assert!(opened_at.elapsed() < Duration::from_secs(2));
    assert_eq!(live.load(Ordering::SeqCst), 0);

    // Muting before it opened means the capturer is closed as soon as it does.
    viewer.set_audio(false);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(live.load(Ordering::SeqCst), 0);

    // Asking again opens it for real.
    viewer.set_audio(true);
    wait_until(|| peak.load(Ordering::SeqCst) > 300).await;
}
