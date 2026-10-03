//! Headless GUI tests rendered with GPUI's Metal renderer (macOS only).
//!
//! GPUI's macOS platform must live on the main thread, so this target has its own `main`
//! instead of the test harness, and it needs a Metal device, so it only runs when selected:
//!
//! ```sh
//! cargo test -p dari --test gui
//! ```
//!
//! Each test also writes a PNG of what it rendered to `target/gui-snapshots/` for review.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    reason = "test code"
)]

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
    #[cfg(not(target_os = "macos"))]
    println!("gui: skipped; GPUI only provides a headless renderer on macOS");
}

#[cfg(target_os = "macos")]
mod macos {

    use std::net::{Ipv4Addr, SocketAddr};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use dari_input::{InjectError, InputBackend, RecordedAction};
    use dari_media::{
        CaptureError, DisplayInfo, ScreenCapturer, StreamSettings, SyntheticCapturer,
    };
    use dari_net::{AccessPassword, DeviceIdentity};
    use dari_proto::{KeyCode, MouseButton as RemoteButton, NamedKey};
    use dari_session::{
        HostConfig, HostEvent, HostPlatform, HostPolicy, TransferDirection, TransferState,
        ViewerConfig, ViewerTarget, connect_viewer, start_host,
    };
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::*;

    use dari::test_support::{
        AppState, Home, Settings, TokioRuntime, init_viewer, open_viewer_window,
    };

    const DISPLAY: DisplayInfo = DisplayInfo {
        id: 1,
        name: String::new(),
        x: 0,
        y: 0,
        width: 1600,
        height: 900,
        scale_factor: 1.0,
        is_primary: true,
        refresh_rate: 60,
    };

    struct SyntheticPlatform {
        actions: Arc<Mutex<Vec<RecordedAction>>>,
    }

    struct Recorder(Arc<Mutex<Vec<RecordedAction>>>);

    impl Recorder {
        fn push(&self, action: RecordedAction) -> Result<(), InjectError> {
            self.0
                .lock()
                .map_err(|_| InjectError::Backend("poisoned".into()))?
                .push(action);
            Ok(())
        }
    }

    impl InputBackend for Recorder {
        fn move_pointer(&mut self, x: i32, y: i32) -> Result<(), InjectError> {
            self.push(RecordedAction::Move(x, y))
        }
        fn button(&mut self, button: RemoteButton, pressed: bool) -> Result<(), InjectError> {
            self.push(RecordedAction::Button(button, pressed))
        }
        fn scroll(&mut self, dx: i32, dy: i32) -> Result<(), InjectError> {
            self.push(RecordedAction::Scroll(dx, dy))
        }
        fn key(&mut self, key: KeyCode, pressed: bool) -> Result<(), InjectError> {
            self.push(RecordedAction::Key(key, pressed))
        }
        fn text(&mut self, text: &str) -> Result<(), InjectError> {
            self.push(RecordedAction::Text(text.into()))
        }
    }

    impl HostPlatform for SyntheticPlatform {
        fn displays(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
            Ok(vec![DISPLAY])
        }
        fn open_capturer(
            &self,
            _display: u32,
            _settings: StreamSettings,
        ) -> Result<Box<dyn ScreenCapturer>, CaptureError> {
            Ok(Box::new(SyntheticCapturer::new(800, 450)))
        }
        fn open_input(&self) -> Result<Box<dyn InputBackend>, InjectError> {
            Ok(Box::new(Recorder(self.actions.clone())))
        }
    }

    fn snapshot_directory() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/gui-snapshots")
    }

    /// A headless app with the real Metal renderer and the app's globals installed.
    fn app(data_directory: &std::path::Path) -> HeadlessAppContext {
        let mut cx = HeadlessAppContext::with_platform(
            gpui_kit::platform::current_platform(true).text_system(),
            Arc::new(gpui_kit::assets::Assets),
            gpui_kit::platform::current_headless_renderer,
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let directory = data_directory.to_owned();
        cx.update(|cx| {
            gpui_kit::init(cx);
            init_viewer(cx);
            TokioRuntime::install(runtime, cx);
            AppState::install(directory, cx);
        });
        cx
    }

    fn window_options(width: f32, height: f32) -> WindowOptions {
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(width), px(height)),
            })),
            focus: true,
            show: false,
            ..Default::default()
        }
    }

    /// Lets background threads deliver events and the app process them, until `done` or timeout.
    fn pump(
        cx: &mut HeadlessAppContext,
        timeout: Duration,
        mut done: impl FnMut(&mut HeadlessAppContext) -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("condition not reached within {timeout:?}");
    }

    fn save(cx: &mut HeadlessAppContext, window: AnyWindowHandle, name: &str) {
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        let image = cx
            .capture_screenshot(window)
            .expect("Metal rendering must be available");
        let directory = snapshot_directory();
        std::fs::create_dir_all(&directory).unwrap();
        image.save(directory.join(format!("{name}.png"))).unwrap();
    }

    pub(super) fn home_window_shows_address_and_password() {
        let data = tempfile::tempdir().unwrap();
        // Port 0 picks a free port so tests never collide with a running host.
        Settings {
            port: 0,
            ..Settings::default()
        }
        .save(data.path())
        .unwrap();
        let mut cx = app(data.path());
        let (window, home) = cx
            .update(|cx| {
                gpui_kit::open_window(window_options(960., 640.), cx, |window, cx| {
                    cx.new(|cx| Home::new(window, cx))
                })
            })
            .unwrap();
        pump(&mut cx, Duration::from_secs(5), |cx| {
            cx.update(|cx| home.read(cx).has_password(cx))
        });
        save(&mut cx, window, "home");
    }

    pub(super) fn viewer_window_shows_the_remote_screen_and_forwards_input() {
        let data = tempfile::tempdir().unwrap();
        let mut cx = app(data.path());

        // A host with a synthetic screen and recorded input, served on the shared runtime.
        let actions = Arc::new(Mutex::new(Vec::new()));
        let platform = Arc::new(SyntheticPlatform {
            actions: actions.clone(),
        });
        let identity = DeviceIdentity::generate().unwrap();
        let config = HostConfig {
            bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            host_name: "synthetic-host".into(),
            stream: StreamSettings::default(),
            policy: HostPolicy {
                require_approval: false,
                clipboard: false,
                file_transfer: false,
                audio: false,
            },
            downloads: None,
            relay: None,
        };
        let (host, mut host_events) = cx
            .update(|cx| {
                TokioRuntime::enter(cx, || start_host(config, Arc::new(identity), platform))
            })
            .unwrap();
        let password: AccessPassword = match host_events.blocking_recv() {
            Some(HostEvent::PasswordChanged(Some(password))) => password,
            other => panic!("expected a password, got {other:?}"),
        };
        let viewer_config = ViewerConfig {
            target: ViewerTarget::Direct(host.local_address()),
            client_name: "gui-test".into(),
            map_shortcut_modifier: false,
            clipboard: None,
            frame_rate: 144,
            downloads: None,
            audio: None,
            play_audio: false,
        };
        let attempt = cx.update(|cx| {
            TokioRuntime::spawn(
                cx,
                async move { connect_viewer(viewer_config, &password).await },
            )
        });
        let (viewer, events) = futures_executor_block_on(attempt).unwrap().unwrap();
        let (window, view) = cx
            .update(|cx| open_viewer_window(viewer, events, cx))
            .unwrap();

        // The first decoded frame reaches the window.
        pump(&mut cx, Duration::from_secs(10), |cx| {
            cx.update(|cx| view.read(cx).has_frame())
        });
        // The viewer asked for 144 fps; the host's 60 Hz display caps it, and the toolbar says so.
        pump(&mut cx, Duration::from_secs(10), |cx| {
            cx.update(|cx| view.read(cx).host_frame_rate() == Some(60))
        });

        // Real GPUI input events on the picture reach the host's input backend. The window is
        // 1280x800 with a 44px toolbar; the 16:9 picture is centered in the area below it.
        let center = point(px(640.), px(44. + (800. - 44.) / 2.));
        cx.update_window(window, |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: center,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: center,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position: center,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke: Keystroke {
                        key: "a".into(),
                        ..Keystroke::default()
                    },
                    is_held: false,
                    prefer_character_input: false,
                }),
                cx,
            );
            // Tab is bound by gpui-kit's Root for focus navigation; it must still reach the
            // remote machine while the viewer has focus.
            window.dispatch_event(
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke: Keystroke {
                        key: "tab".into(),
                        ..Keystroke::default()
                    },
                    is_held: false,
                    prefer_character_input: false,
                }),
                cx,
            );
            // So is the platform copy shortcut (⌘C / Ctrl+C), which the remote must receive.
            window.dispatch_event(
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke: Keystroke {
                        key: "c".into(),
                        modifiers: Modifiers {
                            platform: true,
                            ..Modifiers::default()
                        },
                        key_char: None,
                    },
                    is_held: false,
                    prefer_character_input: false,
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::KeyUp(KeyUpEvent {
                    keystroke: Keystroke {
                        key: "a".into(),
                        ..Keystroke::default()
                    },
                }),
                cx,
            );
        })
        .unwrap();

        let expected = [
            RecordedAction::Move(800, 450),
            RecordedAction::Button(RemoteButton::Left, true),
            RecordedAction::Button(RemoteButton::Left, false),
            RecordedAction::Key(KeyCode::Character('a'), true),
            RecordedAction::Key(KeyCode::Character('a'), false),
            RecordedAction::Key(KeyCode::Named(NamedKey::Tab), true),
            RecordedAction::Key(KeyCode::Named(NamedKey::Meta), true),
            RecordedAction::Key(KeyCode::Character('c'), true),
        ];
        pump(&mut cx, Duration::from_secs(5), |_| {
            let recorded = actions.lock().unwrap();
            expected.iter().all(|action| recorded.contains(action))
        });
        save(&mut cx, window, "viewer");

        // Every frame becomes a new GPU texture (~1.4 MB here). If replaced frames were not
        // released, a few seconds of streaming would grow memory by hundreds of megabytes.
        let frames_before = view_frames(&mut cx, &view);
        let rss_before = resident_megabytes();
        let streaming_until = Instant::now() + Duration::from_secs(6);
        while Instant::now() < streaming_until {
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(15));
        }
        let shown = view_frames(&mut cx, &view) - frames_before;
        let growth = resident_megabytes() - rss_before;
        println!("viewer showed {shown} frames; resident memory grew {growth:.1} MB");
        // Slow CI machines decode fewer frames; the leak bound scales with what was shown. A
        // leaked frame here costs about 1.5 MB, a released one nothing.
        assert!(
            shown >= 20,
            "the stream should keep updating the window ({shown} frames)"
        );
        #[expect(clippy::cast_precision_loss, reason = "frame counts are small")]
        let allowed = 20.0 + 0.4 * shown as f64;
        assert!(
            growth < allowed,
            "replaced frames must be released; memory grew {growth:.1} MB over {shown} frames"
        );
        drop(host);
    }

    fn view_frames(
        cx: &mut HeadlessAppContext,
        view: &Entity<dari::test_support::ViewerView>,
    ) -> u64 {
        cx.update(|cx| view.read(cx).frames_shown())
    }

    /// This process's resident memory, from `ps`.
    fn resident_megabytes() -> f64 {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        let kilobytes: f64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap();
        kilobytes / 1024.0
    }

    /// Waits for a Tokio join handle from a thread that is not a runtime worker.
    fn futures_executor_block_on<T>(
        handle: tokio::task::JoinHandle<T>,
    ) -> Result<T, tokio::task::JoinError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(handle)
    }

    /// The whole user flow in one app: fill in the connect form with this device's own address
    /// and password, connect, see the approval request on the host side, allow it.
    pub(super) fn connecting_through_the_form_asks_the_host_user_first() {
        // A local relay, so the form can reach this device by its relay ID.
        let relay_data = tempfile::tempdir().unwrap();
        let relay_runtime = tokio::runtime::Runtime::new().unwrap();
        let relay = {
            let _entered = relay_runtime.enter();
            dari_relay::RelayServer::start(&dari_relay::RelayConfig {
                listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
                data_directory: relay_data.path().to_owned(),
                max_allocations: 4,
            })
            .unwrap()
        };
        let data = tempfile::tempdir().unwrap();
        Settings {
            port: 0,
            relay_address: relay.local_address().unwrap().to_string(),
            // Host and viewer share this machine: real audio would play into its own capture.
            share_audio: false,
            ..Settings::default()
        }
        .save(data.path())
        .unwrap();
        let mut cx = app(data.path());
        let (window, home) = cx
            .update(|cx| {
                gpui_kit::open_window(window_options(960., 900.), cx, |window, cx| {
                    cx.new(|cx| Home::new(window, cx))
                })
            })
            .unwrap();
        pump(&mut cx, Duration::from_secs(10), |cx| {
            cx.update(|cx| home.read(cx).has_password(cx) && home.read(cx).relay_id(cx).is_some())
        });
        let (password, relay_id) = cx.update(|cx| {
            let home = home.read(cx);
            (home.password_text(cx).unwrap(), home.relay_id(cx).unwrap())
        });
        save(&mut cx, window, "home-relay");

        // Connect by the nine-digit relay ID, not by address.
        cx.update_window(window, |_, window, cx| {
            window.click("connect-address", cx);
            window.input(&relay_id, cx);
            window.click("connect-password", cx);
            window.input(&password, cx);
            window.click("connect", cx);
        })
        .unwrap();

        // Approval is on by default: the host side asks before anything is shared.
        pump(&mut cx, Duration::from_secs(10), |cx| {
            cx.update(|cx| home.read(cx).has_pending_approval(cx))
        });
        assert!(
            cx.update(|cx| home.read(cx).admitted_session_status(cx))
                .is_none()
        );
        save(&mut cx, window, "home-approval");

        cx.update_window(window, |_, window, cx| window.click("approval-control", cx))
            .unwrap();
        pump(&mut cx, Duration::from_secs(10), |cx| {
            cx.update(|cx| home.read(cx).admitted_session_status(cx))
                .is_some()
        });
        // A viewer window opened next to the home window.
        assert_eq!(cx.update(|cx| cx.windows().len()), 2);
        drop(relay);
    }

    /// Files dropped on the remote screen go to the host; files from the host wait in the
    /// transfer strip until the viewer user saves them.
    pub(super) fn files_dropped_on_the_viewer_reach_the_host_and_offers_wait_for_save() {
        let data = tempfile::tempdir().unwrap();
        let host_downloads = tempfile::tempdir().unwrap();
        let viewer_downloads = tempfile::tempdir().unwrap();
        let sources = tempfile::tempdir().unwrap();
        let mut cx = app(data.path());

        let platform = Arc::new(SyntheticPlatform {
            actions: Arc::new(Mutex::new(Vec::new())),
        });
        let config = HostConfig {
            bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            host_name: "synthetic-host".into(),
            stream: StreamSettings::default(),
            policy: HostPolicy {
                require_approval: false,
                clipboard: false,
                file_transfer: true,
                audio: false,
            },
            downloads: Some(host_downloads.path().to_owned()),
            relay: None,
        };
        let identity = DeviceIdentity::generate().unwrap();
        let (host, mut host_events) = cx
            .update(|cx| {
                TokioRuntime::enter(cx, || start_host(config, Arc::new(identity), platform))
            })
            .unwrap();
        let password: AccessPassword = match host_events.blocking_recv() {
            Some(HostEvent::PasswordChanged(Some(password))) => password,
            other => panic!("expected a password, got {other:?}"),
        };
        let viewer_config = ViewerConfig {
            target: ViewerTarget::Direct(host.local_address()),
            client_name: "gui-test".into(),
            map_shortcut_modifier: false,
            clipboard: None,
            frame_rate: 30,
            downloads: Some(viewer_downloads.path().to_owned()),
            audio: None,
            play_audio: false,
        };
        let attempt = cx.update(|cx| {
            TokioRuntime::spawn(
                cx,
                async move { connect_viewer(viewer_config, &password).await },
            )
        });
        let (viewer, events) = futures_executor_block_on(attempt).unwrap().unwrap();
        let (window, view) = cx
            .update(|cx| open_viewer_window(viewer, events, cx))
            .unwrap();
        pump(&mut cx, Duration::from_secs(10), |cx| {
            cx.update(|cx| view.read(cx).can_send_files())
        });

        // Drop a file on the picture.
        let dropped = sources.path().join("notes.txt");
        std::fs::write(&dropped, b"dropped on the viewer").unwrap();
        let center = point(px(640.), px(420.));
        cx.update_window(window, |_, window, cx| {
            window.dispatch_event(
                PlatformInput::FileDrop(FileDropEvent::Entered {
                    position: center,
                    paths: ExternalPaths(vec![dropped.clone()].into()),
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::FileDrop(FileDropEvent::Submit { position: center }),
                cx,
            );
        })
        .unwrap();
        let saved = host_downloads.path().join("notes.txt");
        pump(&mut cx, Duration::from_secs(10), |_| {
            while let Ok(event) = host_events.try_recv() {
                drop(event);
            }
            std::fs::read(&saved).is_ok_and(|bytes| bytes == b"dropped on the viewer")
        });

        // The host offers a file; it waits for the viewer user.
        let offered = sources.path().join("from-host.bin");
        std::fs::write(&offered, vec![7u8; 200_000]).unwrap();
        host.send_file(offered);
        pump(&mut cx, Duration::from_secs(10), |cx| {
            cx.update(|cx| {
                view.read(cx).transfers().iter().any(|transfer| {
                    transfer.direction == TransferDirection::Receiving
                        && transfer.state == TransferState::Offered
                })
            })
        });
        save(&mut cx, window, "viewer-file-offer");
        assert!(!viewer_downloads.path().join("from-host.bin").exists());

        cx.update_window(window, |_, window, cx| {
            window.click("transfer-accept-0", cx)
        })
        .unwrap();
        pump(&mut cx, Duration::from_secs(10), |cx| {
            cx.update(|cx| {
                view.read(cx).transfers().iter().any(|transfer| {
                    transfer.direction == TransferDirection::Receiving
                        && transfer.state == TransferState::Completed
                })
            })
        });
        assert_eq!(
            std::fs::read(viewer_downloads.path().join("from-host.bin")).unwrap(),
            vec![7u8; 200_000]
        );
        save(&mut cx, window, "viewer-file-received");
        drop(host);
    }

    pub(super) fn run() {
        let tests: [(&str, fn()); 4] = [
            (
                "files_dropped_on_the_viewer_reach_the_host_and_offers_wait_for_save",
                files_dropped_on_the_viewer_reach_the_host_and_offers_wait_for_save,
            ),
            (
                "connecting_through_the_form_asks_the_host_user_first",
                connecting_through_the_form_asks_the_host_user_first,
            ),
            (
                "home_window_shows_address_and_password",
                home_window_shows_address_and_password,
            ),
            (
                "viewer_window_shows_the_remote_screen_and_forwards_input",
                viewer_window_shows_the_remote_screen_and_forwards_input,
            ),
        ];
        for (name, test) in tests {
            test();
            println!("test {name} ... ok");
        }
    }
}
