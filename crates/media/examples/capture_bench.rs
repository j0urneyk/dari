//! Captures and encodes the primary display for a few seconds and reports throughput, then
//! decodes what it received the way a viewer does and reports how long that takes.
//!
//! ```sh
//! cargo run --release -p dari-media --example capture_bench -- [seconds] [max-long-edge] [fps] [hardware|software]
//! ```
//!
//! The capture only produces frames while the screen changes, so keep something moving on the
//! primary display (a video, an animation) while it runs. On macOS the terminal needs Screen
//! Recording permission.

#![allow(clippy::print_stdout, reason = "benchmark output")]

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use dari_media::{
    DisplayCapturer, FRAMES_IN_FLIGHT, StreamSettings, VideoDecoder, list_displays,
    spawn_capture_stream,
};

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
    let max_fps: u32 = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(60);
    let hardware_encoder = match args.next().as_deref() {
        None | Some("hardware") => true,
        Some("software") => false,
        Some(other) => return Err(format!("unknown encoder {other:?}").into()),
    };

    for display in list_displays()? {
        println!("display: {display:?}");
    }
    let settings = StreamSettings {
        max_long_edge,
        max_fps,
        hardware_encoder,
        ..StreamSettings::default()
    };
    println!("settings: {settings:?}");
    let (sender, mut receiver) = tokio::sync::mpsc::channel(FRAMES_IN_FLIGHT);
    let stream = spawn_capture_stream(
        move || DisplayCapturer::open(None, &settings),
        settings,
        sender,
    )?;

    let started = Instant::now();
    let (mut frames, mut bytes, mut size) = (0u64, 0u64, (0, 0));
    let mut received = Vec::new();
    while let Some(remaining) = Duration::from_secs(seconds).checked_sub(started.elapsed()) {
        let Ok(Some(frame)) = tokio::time::timeout(remaining, receiver.recv()).await else {
            break;
        };
        let frame = frame?;
        frames += 1;
        bytes += frame.data.len() as u64;
        size = (frame.width, frame.height);
        received.push(frame.data);
    }
    let elapsed = started.elapsed().as_secs_f64();
    let stats = stream.stats();
    let encoded = stats.frames_encoded.load(Ordering::Relaxed).max(1);
    #[expect(clippy::cast_precision_loss, reason = "report only")]
    let (fps, mbps, encode_ms) = (
        frames as f64 / elapsed,
        bytes as f64 * 8.0 / elapsed / 1e6,
        stats.encode_micros.load(Ordering::Relaxed) as f64 / encoded as f64 / 1000.0,
    );
    println!(
        "{}x{}: {frames} frames in {elapsed:.1}s = {fps:.1} fps, {mbps:.2} Mbit/s",
        size.0, size.1
    );
    println!(
        "encoder: {}, {encode_ms:.2} ms from capture to encoded frame, {} skipped",
        if stats.hardware_encoding.load(Ordering::Relaxed) {
            "hardware"
        } else {
            "software"
        },
        stats.frames_skipped.load(Ordering::Relaxed),
    );
    stream.stop();

    let mut decoder = VideoDecoder::new()?;
    let decoding = Instant::now();
    for data in &received {
        decoder.decode(data)?;
    }
    #[expect(clippy::cast_precision_loss, reason = "report only")]
    let decode_ms = decoding.elapsed().as_secs_f64() * 1000.0 / received.len().max(1) as f64;
    println!(
        "viewer decoding to BGRA: {decode_ms:.2} ms per frame (≈{:.0} fps)",
        1000.0 / decode_ms.max(0.001)
    );
    Ok(())
}
