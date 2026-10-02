//! Full host ↔ viewer sessions over loopback QUIC with a synthetic screen and recorded input.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers may panic"
)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use open_desk_input::{InjectError, InputBackend, RecordedAction};
use open_desk_media::{
    CaptureError, DisplayInfo, ScreenCapturer, StreamSettings, SyntheticCapturer,
};
use open_desk_net::{AccessPassword, DeviceIdentity};
use open_desk_proto::{Availability, InputEvent, KeyCode, MouseButton, NamedKey, PointerPosition};
use open_desk_session::{
    ApprovalDecision, ClipboardAccess, ClipboardFactory, HostConfig, HostEvent, HostPlatform,
    SessionEndReason, ViewerConfig, ViewerEvent, connect_viewer, start_host,
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
    handle: open_desk_session::HostHandle,
    events: mpsc::UnboundedReceiver<HostEvent>,
    password: AccessPassword,
}

async fn start(platform: TestPlatform) -> Host {
    start_with(platform, false).await
}

async fn start_with(platform: TestPlatform, require_approval: bool) -> Host {
    let (handle, mut events) = start_host(
        HostConfig {
            bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            host_name: "test-host".into(),
            stream: StreamSettings {
                max_long_edge: 1920,
                max_fps: 30,
                bitrate_bps: 1_000_000,
            },
            require_approval,
            clipboard: true,
        },
        &DeviceIdentity::generate().unwrap(),
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
        address: host.handle.local_address(),
        client_name: "test-viewer".into(),
        map_shortcut_modifier: false,
        clipboard: None,
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
        ViewerEvent::HostStatus(open_desk_proto::HostStatus {
            screen: Availability::Available,
            input: Availability::Available,
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
        ViewerEvent::HostStatus(open_desk_proto::HostStatus {
            screen: Availability::Available,
            input: Availability::PermissionDenied,
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
            ViewerEvent::AwaitingApproval | ViewerEvent::Displays { .. } => {}
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
        ViewerEvent::HostStatus(open_desk_proto::HostStatus {
            screen: Availability::Available,
            input: Availability::Available,
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
        ViewerEvent::HostStatus(open_desk_proto::HostStatus {
            screen: Availability::Available,
            input: Availability::NotAllowed,
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
