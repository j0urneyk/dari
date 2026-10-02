//! Captures and encodes the primary display for a few seconds and reports throughput.
//!
//! ```sh
//! cargo run --release -p open-desk-media --example capture_bench -- [seconds] [max-long-edge]
//! ```

#![allow(clippy::print_stdout, reason = "benchmark output")]

use std::time::{Duration, Instant};

use open_desk_media::{
    CaptureError, PermissionState, RgbaFrame, ScreenCapturer, StreamSettings, list_displays,
    screen_capture_access, spawn_capture_stream,
};

/// Captures the primary display through xcap without the permission preflight. Without
/// permission macOS still returns full-size frames (desktop background only), which is enough
/// to measure capture and encoding cost.
struct UncheckedCapturer(xcap::Monitor);

impl ScreenCapturer for UncheckedCapturer {
    fn capture(&mut self) -> Result<RgbaFrame, CaptureError> {
        let image = self.0.capture_image()?;
        let (width, height) = image.dimensions();
        RgbaFrame::new(width, height, image.into_raw())
            .ok_or_else(|| CaptureError::Backend("bad frame".into()))
    }
}

fn open_unchecked() -> Result<UncheckedCapturer, CaptureError> {
    let monitors = xcap::Monitor::all()?;
    let primary = monitors
        .iter()
        .position(|monitor| monitor.is_primary().unwrap_or(false))
        .unwrap_or(0);
    monitors
        .into_iter()
        .nth(primary)
        .map(UncheckedCapturer)
        .ok_or(CaptureError::NoDisplay)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(5);
    let max_long_edge: u32 = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(1920);

    let permission = screen_capture_access();
    println!("screen capture permission: {permission:?}");
    if permission == PermissionState::Denied {
        println!("measuring without permission: frames show only the desktop background");
    }
    for display in list_displays()? {
        println!("display: {display:?}");
    }

    let settings = StreamSettings {
        max_long_edge,
        max_fps: 60,
        ..StreamSettings::default()
    };
    let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
    let stream = spawn_capture_stream(open_unchecked, settings, sender)?;

    let started = Instant::now();
    let (mut frames, mut bytes, mut size) = (0u64, 0u64, (0, 0));
    while started.elapsed() < Duration::from_secs(seconds) {
        let Some(frame) = receiver.recv().await else {
            break;
        };
        let frame = frame?;
        frames += 1;
        bytes += frame.data.len() as u64;
        size = (frame.width, frame.height);
    }
    let elapsed = started.elapsed().as_secs_f64();
    #[expect(clippy::cast_precision_loss, reason = "report only")]
    let (fps, mbps) = (frames as f64 / elapsed, bytes as f64 * 8.0 / elapsed / 1e6);
    println!(
        "{}x{}: {frames} frames in {elapsed:.1}s = {fps:.1} fps, {mbps:.2} Mbit/s",
        size.0, size.1
    );
    stream.stop();
    Ok(())
}
