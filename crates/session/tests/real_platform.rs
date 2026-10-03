//! A host and viewer session on the real screen and input devices of this machine.
//!
//! Ignored by default: it needs a display, the OS permissions to capture the screen and inject
//! input (Screen Recording and Accessibility on macOS), and it moves the real pointer. Run it with
//!
//! ```sh
//! cargo test -p dari-session --test real_platform -- --ignored --nocapture
//! ```
//!
//! The decoded frame is saved to `target/real-platform/frame.png` for review.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    reason = "a test that reports what it saw; coordinates and statistics are small, positive values"
)]

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dari_media::{DecodedFrame, StreamSettings};
use dari_net::DeviceIdentity;
use dari_proto::{Availability, HostStatus, InputEvent, PointerPosition};
use dari_session::{
    HostConfig, HostEvent, HostPlatform, HostPolicy, SystemPlatform, ViewerConfig, ViewerEvent,
    ViewerTarget, connect_viewer, start_host,
};
use enigo::{Coordinate, Enigo, Mouse, Settings};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a display and capture/input permissions, and moves the real pointer"]
async fn real_screen_is_streamed_and_real_pointer_is_controlled() {
    dari_input::prepare_process();
    let display = SystemPlatform
        .displays()
        .expect("the host lists its displays")
        .into_iter()
        .next()
        .expect("at least one display");
    println!("primary display: {display:?}");

    let (handle, mut host_events) = start_host(
        HostConfig {
            bind_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            host_name: "real-host".into(),
            stream: StreamSettings::default(),
            policy: HostPolicy {
                require_approval: false,
                clipboard: false,
                file_transfer: false,
                audio: false,
            },
            downloads: None,
            relay: None,
        },
        Arc::new(DeviceIdentity::generate().unwrap()),
        Arc::new(SystemPlatform),
    )
    .expect("hosting starts");
    let password = loop {
        if let Some(HostEvent::PasswordChanged(Some(password))) = host_events.recv().await {
            break password;
        }
    };

    let (viewer, mut viewer_events) = connect_viewer(
        ViewerConfig {
            target: ViewerTarget::Direct(handle.local_address()),
            client_name: "real-viewer".into(),
            map_shortcut_modifier: false,
            clipboard: None,
            frame_rate: 144,
            downloads: None,
            audio: None,
            play_audio: false,
        },
        &password,
    )
    .await
    .expect("the viewer connects");

    // The host must report both capabilities as working, not missing permissions, and stream
    // as fast as asked, up to the display's refresh rate, from the first rate it reports.
    let expected = match display.refresh_rate {
        0 => 144,
        refresh => u16::try_from(refresh.min(144)).unwrap(),
    };
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        let (mut status, mut frame_rate) = (None, None);
        while status.is_none() || frame_rate.is_none() {
            match viewer_events.recv().await {
                Some(ViewerEvent::HostStatus(reported)) => status = Some(reported),
                Some(ViewerEvent::FrameRate(rate)) => frame_rate = Some(rate),
                Some(_) => {}
                None => panic!("the session ended before reporting its status"),
            }
        }
        assert_eq!(
            frame_rate,
            Some(expected),
            "the first rate is the requested one"
        );
        status.unwrap()
    })
    .await
    .expect("the host reports its status and streams at the display's refresh rate");
    println!("streaming at up to {expected} fps");
    assert_eq!(
        status,
        HostStatus {
            screen: Availability::Available,
            input: Availability::Available,
            files: Availability::Unavailable,
            audio: Availability::Unavailable,
        },
        "grant Screen Recording and Accessibility to the process running this test"
    );

    // Real frames arrive, shaped like the display and with actual screen content.
    let mut frames = viewer.frames();
    let frame = wait_for_frame(&mut frames).await;
    let display_aspect = f64::from(display.width) / f64::from(display.height);
    let frame_aspect = f64::from(frame.width) / f64::from(frame.height);
    assert!(
        (display_aspect - frame_aspect).abs() < 0.02,
        "frame {}x{} doesn't match display {}x{}",
        frame.width,
        frame.height,
        display.width,
        display.height
    );
    let spread = luminance_spread(&frame);
    let path = save_png(&frame);
    println!(
        "frame {}x{}, luminance spread {spread:.1}, saved to {}",
        frame.width,
        frame.height,
        path.display()
    );
    assert!(
        spread > 5.0,
        "the frame is a flat color; capture looks blank"
    );

    // Remote pointer moves land where the viewer asked, in the host's own coordinates.
    let mut enigo = Enigo::new(&Settings::default()).expect("enigo opens");
    let original = enigo.location().expect("the pointer location is readable");
    let result = std::panic::AssertUnwindSafe(async {
        for (fx, fy) in [(0.25, 0.25), (0.75, 0.6), (0.5, 0.5)] {
            let target = PointerPosition {
                x: (f64::from(u16::MAX) * fx).round() as u16,
                y: (f64::from(u16::MAX) * fy).round() as u16,
            };
            let expected = (
                display.x + (f64::from(display.width - 1) * fx).round() as i32,
                display.y + (f64::from(display.height - 1) * fy).round() as i32,
            );
            assert!(viewer.send_input(InputEvent::PointerMove(target)));
            let landed = wait_for_pointer(&enigo, expected).await;
            println!("pointer {fx},{fy}: expected {expected:?}, landed {landed:?}");
            assert!(
                (landed.0 - expected.0).abs() <= 2 && (landed.1 - expected.1).abs() <= 2,
                "pointer landed at {landed:?}, expected {expected:?}"
            );
        }
    });
    let outcome = futures_util::FutureExt::catch_unwind(result).await;
    let _restored = enigo.move_mouse(original.0, original.1, Coordinate::Abs);

    viewer.disconnect();
    drop(handle);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn wait_for_frame(
    frames: &mut tokio::sync::watch::Receiver<Option<Arc<DecodedFrame>>>,
) -> Arc<DecodedFrame> {
    tokio::time::timeout(Duration::from_secs(15), frames.wait_for(Option::is_some))
        .await
        .expect("a frame arrives")
        .expect("the viewer is running");
    // Skip the very first frame so the encoder has settled on the full-quality picture.
    let _next = tokio::time::timeout(Duration::from_secs(2), frames.changed()).await;
    frames.borrow().clone().unwrap()
}

async fn wait_for_pointer(enigo: &Enigo, expected: (i32, i32)) -> (i32, i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let location = enigo.location().expect("the pointer location is readable");
        if ((location.0 - expected.0).abs() <= 2 && (location.1 - expected.1).abs() <= 2)
            || Instant::now() > deadline
        {
            return location;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Standard deviation of the frame's luminance; a blank or failed capture is nearly zero.
fn luminance_spread(frame: &DecodedFrame) -> f64 {
    let values: Vec<f64> = frame
        .bgra
        .as_chunks::<4>()
        .0
        .iter()
        .step_by(7)
        .map(|pixel| {
            0.114 * f64::from(pixel[0]) + 0.587 * f64::from(pixel[1]) + 0.299 * f64::from(pixel[2])
        })
        .collect();
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    (values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / values.len() as f64)
        .sqrt()
}

fn save_png(frame: &DecodedFrame) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/real-platform");
    std::fs::create_dir_all(&directory).unwrap();
    let rgba: Vec<u8> = frame
        .bgra
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], 255])
        .collect();
    let path = directory.join("frame.png");
    image::RgbaImage::from_raw(frame.width, frame.height, rgba)
        .expect("the buffer matches the frame size")
        .save(&path)
        .unwrap();
    path
}
