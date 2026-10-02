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
    HostConfig, HostEvent, HostPlatform, SessionEndReason, ViewerConfig, ViewerEvent,
    connect_viewer, start_host,
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
        Ok(vec![DISPLAY])
    }
    fn open_capturer(&self, display: u32) -> Result<Box<dyn ScreenCapturer>, CaptureError> {
        assert_eq!(display, DISPLAY.id);
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

struct Host {
    handle: open_desk_session::HostHandle,
    events: mpsc::UnboundedReceiver<HostEvent>,
    password: AccessPassword,
}

async fn start(platform: TestPlatform) -> Host {
    let (handle, mut events) = start_host(
        HostConfig {
            bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            host_name: "test-host".into(),
            stream: StreamSettings {
                max_long_edge: 1920,
                max_fps: 30,
                bitrate_bps: 1_000_000,
            },
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
        next_event(&mut viewer_events).await,
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
        next_event(&mut viewer_events).await,
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
        }
    }
    // The session survives the end of the video stream.
    let still_open = tokio::time::timeout(Duration::from_millis(500), viewer_events.recv()).await;
    assert!(still_open.is_err(), "unexpected {still_open:?}");
    assert!(viewer.send_input(InputEvent::Text("still here".into())));
}
