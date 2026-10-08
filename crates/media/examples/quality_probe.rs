//! Measures what a still or scrolling screen looks like after Dari's capture stream and the
//! viewer's decoder, frame by frame, to pin down why text looks blurry at a given bitrate.
//!
//! ```sh
//! cargo run --release -p dari-media --example quality_probe -- <image.png|-> <bitrate-bps> <fps> <hardware|software> <scenario> [max-long-edge] [out-dir]
//! ```
//!
//! `-` instead of a PNG path renders a deterministic text-like test image (2560×1440; `-WxH`
//! picks another size). The scenarios are `still` (the image once, then a still screen) and
//! `scroll[:K:PX]` (the image, then K frames each scrolled up by another PX pixels, then a still
//! screen; defaults K = 30, PX = 4). A `@DELAY:INTERVAL:FRAMES` suffix on the scenario
//! (milliseconds, milliseconds, count) tries another refinement policy than the stream's
//! default: `still@100:33:12`.
//!
//! Each line prints one delivered frame: when it arrived, its type and encoded size, and the
//! PSNR of the decoded picture against the frame's source. A marker drawn into the top-left
//! corner of every frame names its source, so a frame the encoder skipped or the stream dropped
//! shows up as a gap, not as a mismatch. PNGs of the decoded picture at the end of motion (the
//! last frame with new content), at the first refinement frame after it, and at the last frame
//! are saved under `out-dir` (default `target/quality_probe`) next to the source.

#![allow(clippy::print_stdout, reason = "measurement output")]

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use dari_media::{
    CaptureError, CapturedFrame, DecodedFrame, EncodedFrame, FRAMES_IN_FLIGHT, FrameScaler,
    RgbaFrame, ScreenCapturer, StillRefinement, StreamSettings, VideoDecoder, render_text_page,
    spawn_capture_stream,
};
use tokio::sync::mpsc;

const IDENTICAL_PSNR: f64 = 99.0;
const MARKER_BLOCK: u32 = 16;
const MARKER_BITS: u32 = 12;

#[derive(Debug, Clone, Copy)]
enum Scenario {
    Still,
    Scroll { steps: u32, pixels: u32 },
}

impl Scenario {
    fn parse(text: &str) -> Result<Self, String> {
        let mut parts = text.split(':');
        let name = parts.next().unwrap_or_default();
        let mut number = |default: u32| -> Result<u32, String> {
            parts.next().map_or(Ok(default), |value| {
                value.parse().map_err(|_| text.to_owned())
            })
        };
        match name {
            "still" => Ok(Self::Still),
            "scroll" => Ok(Self::Scroll {
                steps: number(30)?,
                pixels: number(4)?,
            }),
            _ => Err(text.to_owned()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Still => "still",
            Self::Scroll { .. } => "scroll",
        }
    }

    fn scroll_offsets(self) -> Vec<u32> {
        match self {
            Self::Still => vec![0],
            Self::Scroll { steps, pixels } => (0..=steps).map(|index| index * pixels).collect(),
        }
    }
}

fn parse_refinement(text: &str) -> Result<StillRefinement, String> {
    let mut parts = text.split(':');
    let mut number = || -> Result<u64, String> {
        parts
            .next()
            .ok_or_else(|| text.to_owned())?
            .parse()
            .map_err(|_| text.to_owned())
    };
    Ok(StillRefinement {
        delay: Duration::from_millis(number()?),
        interval: Duration::from_millis(number()?),
        frames: u32::try_from(number()?).map_err(|_| text.to_owned())?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Moving,
    MotionEnd,
    Refinement,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Moving => "moving",
            Self::MotionEnd => "motion-end",
            Self::Refinement => "refinement",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FrameKind {
    Key,
    Predicted,
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Key => "I",
            Self::Predicted => "P",
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct Psnr {
    y: f64,
    rgb: f64,
}

#[derive(Debug, Clone, Copy)]
struct Measurement {
    plan_index: usize,
    arrived: Duration,
    kind: FrameKind,
    bytes: usize,
    psnr: Psnr,
    phase: Phase,
}

struct Args {
    image: ImageSource,
    bitrate_bps: u32,
    fps: u32,
    hardware: bool,
    scenario: Scenario,
    refinement: Option<StillRefinement>,
    max_long_edge: u32,
    out_dir: PathBuf,
}

enum ImageSource {
    File(PathBuf),
    Generated { width: u32, height: u32 },
}

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut next = |name: &str| {
        args.next()
            .ok_or_else(|| format!("missing argument: {name}"))
    };
    let image = match next("image")?.as_str() {
        "-" => ImageSource::Generated {
            width: 2560,
            height: 1440,
        },
        generated if generated.starts_with('-') => {
            let (width, height) = generated[1..]
                .split_once('x')
                .ok_or_else(|| format!("expected -WxH, got {generated:?}"))?;
            ImageSource::Generated {
                width: width.parse()?,
                height: height.parse()?,
            }
        }
        path => ImageSource::File(path.into()),
    };
    let bitrate_bps = next("bitrate")?.parse()?;
    let fps = next("fps")?.parse()?;
    let hardware = match next("encoder")?.as_str() {
        "hardware" => true,
        "software" => false,
        other => return Err(format!("unknown encoder {other:?}").into()),
    };
    let scenario = next("scenario")?;
    let (scenario, refinement) = match scenario.split_once('@') {
        Some((scenario, refinement)) => (scenario, Some(refinement)),
        None => (scenario.as_str(), None),
    };
    let refinement = refinement
        .map(parse_refinement)
        .transpose()
        .map_err(|text| format!("bad refinement policy {text:?}"))?;
    let scenario = Scenario::parse(scenario).map_err(|text| format!("bad scenario {text:?}"))?;
    let max_long_edge = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(2560);
    let out_dir = args
        .next()
        .map_or_else(|| PathBuf::from("target/quality_probe"), PathBuf::from);
    Ok(Args {
        image,
        bitrate_bps,
        fps,
        hardware,
        scenario,
        refinement,
        max_long_edge,
        out_dir,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dari_media=debug".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = parse_args()?;
    let original = match &args.image {
        ImageSource::File(path) => load_png(path)?,
        ImageSource::Generated { width, height } => render_text_page(*width, *height),
    };
    let source = FrameScaler::default().fit(original.clone(), args.max_long_edge);
    std::fs::create_dir_all(&args.out_dir)?;

    println!(
        "source {}x{} -> encoded {}x{}; {} bps at {} fps = {} bytes per frame budget",
        original.width(),
        original.height(),
        source.width(),
        source.height(),
        args.bitrate_bps,
        args.fps,
        args.bitrate_bps / 8 / args.fps.max(1)
    );
    if (source.width(), source.height()) != (original.width(), original.height()) {
        let restored = resize_rgba(&source, original.width(), original.height());
        let Psnr { y, rgb } = psnr(&original, &restored);
        println!(
            "scaling alone (no codec), {}x{} down to {}x{} and bilinear back: PSNR Y {y:.2} dB, RGB {rgb:.2} dB",
            original.width(),
            original.height(),
            source.width(),
            source.height()
        );
    }

    let settings = StreamSettings {
        max_long_edge: args.max_long_edge,
        max_fps: args.fps.max(1),
        bitrate_bps: args.bitrate_bps,
        hardware_encoder: args.hardware,
        still_refinement: args.refinement.unwrap_or_default(),
    };
    let refinement = settings.still_refinement;
    println!(
        "refinement: after {:?} still, {} frames {:?} apart{}",
        refinement.delay,
        refinement.frames,
        refinement.interval,
        if args.refinement.is_some() {
            " (overridden)"
        } else {
            " (the stream's default)"
        }
    );
    let plan = args.scenario.scroll_offsets();
    let stem = format!(
        "{}-{}-{}bps-{}fps",
        args.scenario.name(),
        backend_name(args.hardware),
        args.bitrate_bps,
        args.fps
    );
    save_rgba(&args.out_dir.join(format!("{stem}-source.png")), &source)?;

    let (sender, mut receiver) = mpsc::channel(FRAMES_IN_FLIGHT);
    let planned = PlannedSource {
        source: source.clone(),
        plan: plan.clone(),
        interval: Duration::from_secs(1) / args.fps.max(1),
        started: None,
        next: 0,
    };
    let stream = spawn_capture_stream(move || Ok(planned), settings, sender)?;
    let quiet = refinement.delay + 2 * refinement.interval + Duration::from_secs(1);
    let mut arrivals: Vec<(Instant, EncodedFrame)> = Vec::new();
    while let Ok(Some(frame)) = tokio::time::timeout(quiet, receiver.recv()).await {
        arrivals.push((Instant::now(), frame?));
    }
    let stats = stream.stats();
    let hardware = stats.hardware_encoding.load(Ordering::Relaxed);
    let skipped = stats.frames_skipped.load(Ordering::Relaxed);
    let refinements = stats.still_refinements.load(Ordering::Relaxed);
    stream.stop();
    println!(
        "encoder: {} (requested {}); {} frames planned, {} delivered, {skipped} dropped before encoding, {refinements} refinement frames encoded",
        backend_name(hardware),
        backend_name(args.hardware),
        plan.len(),
        arrivals.len()
    );

    let measurements = measure(&arrivals, &source, &plan, &args.out_dir, &stem)?;
    summarize(&measurements, args.bitrate_bps / 8 / args.fps.max(1));
    Ok(())
}

fn backend_name(hardware: bool) -> &'static str {
    if hardware { "hardware" } else { "software" }
}

struct PlannedSource {
    source: RgbaFrame,
    plan: Vec<u32>,
    interval: Duration,
    started: Option<Instant>,
    next: usize,
}

impl ScreenCapturer for PlannedSource {
    fn capture(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        if let Some(&scrolled) = self.plan.get(self.next) {
            let started = *self.started.get_or_insert_with(Instant::now);
            let due = started + self.interval * u32::try_from(self.next).unwrap_or(u32::MAX);
            if due <= Instant::now() + timeout {
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
                let frame = frame_for(&self.source, self.next, scrolled);
                self.next += 1;
                return Ok(Some(frame.into()));
            }
        }
        std::thread::sleep(timeout);
        Ok(None)
    }

    fn paces_itself(&self) -> bool {
        true
    }
}

fn measure(
    arrivals: &[(Instant, EncodedFrame)],
    source: &RgbaFrame,
    plan: &[u32],
    out_dir: &Path,
    stem: &str,
) -> Result<Vec<Measurement>, Box<dyn std::error::Error>> {
    println!(
        "{:>5} {:>7} {:>4} {:>8} {:>9} {:>9}  phase",
        "frame", "t_ms", "type", "bytes", "psnr_y", "psnr_rgb"
    );
    let mut decoder = VideoDecoder::new()?;
    let mut measurements = Vec::new();
    let mut motion_ended = false;
    let mut refinement_saved = false;
    let first_arrival = arrivals.first().map(|(at, _)| *at);
    for (index, (arrived, encoded)) in arrivals.iter().enumerate() {
        let Some(decoded) = decoder.decode(&encoded.data)? else {
            println!("{index:>5} produced no picture");
            continue;
        };
        let source_index = read_marker(&decoded);
        let Some(&scrolled) = plan.get(source_index) else {
            return Err(
                format!("frame {index} names source {source_index}, not in the plan").into(),
            );
        };
        let phase = if source_index + 1 < plan.len() {
            Phase::Moving
        } else if std::mem::replace(&mut motion_ended, true) {
            Phase::Refinement
        } else {
            Phase::MotionEnd
        };
        let psnr = psnr_decoded(&frame_for(source, source_index, scrolled), &decoded)?;
        let measurement = Measurement {
            plan_index: source_index,
            arrived: arrived.saturating_duration_since(first_arrival.unwrap_or(*arrived)),
            kind: if encoded.keyframe {
                FrameKind::Key
            } else {
                FrameKind::Predicted
            },
            bytes: encoded.data.len(),
            psnr,
            phase,
        };
        measurements.push(measurement);
        println!(
            "{:>5} {:>7} {:>4} {:>8} {:>9.2} {:>9.2}  {}",
            measurement.plan_index,
            measurement.arrived.as_millis(),
            measurement.kind,
            measurement.bytes,
            measurement.psnr.y,
            measurement.psnr.rgb,
            phase.name()
        );
        let snapshot = match phase {
            Phase::MotionEnd => Some("motion-end"),
            Phase::Refinement if !refinement_saved => {
                refinement_saved = true;
                Some("refinement")
            }
            _ => None,
        };
        if let Some(name) = snapshot {
            save_bgra(
                &out_dir.join(format!("{stem}-{name}-frame{index}.png")),
                &decoded,
            )?;
        }
        if index + 1 == arrivals.len() {
            save_bgra(
                &out_dir.join(format!("{stem}-last-frame{index}.png")),
                &decoded,
            )?;
        }
    }
    Ok(measurements)
}

fn db(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("{value:.2}"))
}

fn summarize(measurements: &[Measurement], budget: u32) {
    let total: usize = measurements.iter().map(|m| m.bytes).sum();
    let keyframes = measurements
        .iter()
        .filter(|m| matches!(m.kind, FrameKind::Key))
        .count();
    let at = |phase: Phase| measurements.iter().find(|m| m.phase == phase);
    let refinements: Vec<&Measurement> = measurements
        .iter()
        .filter(|m| m.phase == Phase::Refinement)
        .collect();
    let refinement_bytes: usize = refinements.iter().map(|m| m.bytes).sum();
    let refinement_span = match (at(Phase::MotionEnd), refinements.last()) {
        (Some(end), Some(last)) => Some(last.arrived.saturating_sub(end.arrived)),
        _ => None,
    };
    #[expect(clippy::cast_precision_loss, reason = "report only")]
    let mean_bytes = total as f64 / measurements.len().max(1) as f64;
    println!(
        "summary: {} frames ({keyframes} key), {total} bytes, mean {mean_bytes:.0} bytes per frame (budget {budget}); PSNR Y at motion end {} dB, first refinement {} dB, last {} dB, worst refinement {} dB; refinement {} frames, {refinement_bytes} bytes, over {}",
        measurements.len(),
        db(at(Phase::MotionEnd).map(|m| m.psnr.y)),
        db(at(Phase::Refinement).map(|m| m.psnr.y)),
        db(measurements.last().map(|m| m.psnr.y)),
        db(refinements.iter().map(|m| m.psnr.y).reduce(f64::min)),
        refinements.len(),
        refinement_span.map_or_else(|| "-".to_owned(), |span| format!("{} ms", span.as_millis())),
    );
}

fn frame_for(source: &RgbaFrame, index: usize, scrolled: u32) -> RgbaFrame {
    let width = source.width();
    let stride = width as usize * 4;
    let rows = (scrolled % source.height()) as usize;
    let pixels = source.pixels();
    let mut shifted = Vec::with_capacity(pixels.len());
    shifted.extend_from_slice(&pixels[rows * stride..]);
    shifted.extend_from_slice(&pixels[..rows * stride]);
    for bit in 0..MARKER_BITS {
        let value = if (index >> bit) & 1 == 1 { 255 } else { 0 };
        for y in 0..MARKER_BLOCK {
            for x in bit * MARKER_BLOCK..(bit + 1) * MARKER_BLOCK {
                let at = ((y * width + x) * 4) as usize;
                shifted[at..at + 3].copy_from_slice(&[value, value, value]);
            }
        }
    }
    RgbaFrame::new(width, source.height(), shifted)
        .unwrap_or_else(|| unreachable!("the rotated buffer keeps its size"))
}

fn read_marker(frame: &DecodedFrame) -> usize {
    let half = MARKER_BLOCK / 2;
    (0..MARKER_BITS)
        .filter(|bit| {
            let mut sum = 0u32;
            for y in half / 2..half + half / 2 {
                for x in bit * MARKER_BLOCK + half / 2..bit * MARKER_BLOCK + half + half / 2 {
                    let at = ((y * frame.width + x) * 4) as usize;
                    sum += u32::from(frame.bgra[at + 1]);
                }
            }
            sum > 128 * half * half
        })
        .fold(0usize, |index, bit| index | (1 << bit))
}

fn luma(r: u8, g: u8, b: u8) -> f64 {
    0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
}

fn psnr_pixels(pairs: impl Iterator<Item = ([u8; 3], [u8; 3])>) -> Psnr {
    let (mut y_error, mut rgb_error, mut count) = (0.0f64, 0.0f64, 0usize);
    for ([r1, g1, b1], [r2, g2, b2]) in pairs {
        y_error += (luma(r1, g1, b1) - luma(r2, g2, b2)).powi(2);
        rgb_error += [(r1, r2), (g1, g2), (b1, b2)]
            .into_iter()
            .map(|(a, b)| f64::from(a.abs_diff(b)).powi(2))
            .sum::<f64>();
        count += 1;
    }
    #[expect(clippy::cast_precision_loss, reason = "pixel counts")]
    let count = count.max(1) as f64;
    let to_db = |mse: f64| {
        if mse <= 0.0 {
            IDENTICAL_PSNR
        } else {
            (10.0 * (255.0f64.powi(2) / mse).log10()).min(IDENTICAL_PSNR)
        }
    };
    Psnr {
        y: to_db(y_error / count),
        rgb: to_db(rgb_error / (count * 3.0)),
    }
}

fn psnr(a: &RgbaFrame, b: &RgbaFrame) -> Psnr {
    psnr_pixels(
        a.pixels()
            .as_chunks::<4>()
            .0
            .iter()
            .zip(b.pixels().as_chunks::<4>().0)
            .map(|(a, b)| ([a[0], a[1], a[2]], [b[0], b[1], b[2]])),
    )
}

fn psnr_decoded(source: &RgbaFrame, decoded: &DecodedFrame) -> Result<Psnr, String> {
    if (source.width(), source.height()) != (decoded.width, decoded.height) {
        return Err(format!(
            "decoded {}x{} does not match the {}x{} source",
            decoded.width,
            decoded.height,
            source.width(),
            source.height()
        ));
    }
    Ok(psnr_pixels(
        source
            .pixels()
            .as_chunks::<4>()
            .0
            .iter()
            .zip(decoded.bgra.as_chunks::<4>().0)
            .map(|(rgba, bgra)| ([rgba[0], rgba[1], rgba[2]], [bgra[2], bgra[1], bgra[0]])),
    ))
}

fn load_png(path: &Path) -> Result<RgbaFrame, Box<dyn std::error::Error>> {
    let image = image::open(path)?.to_rgba8();
    let (width, height) = image.dimensions();
    RgbaFrame::new(width, height, image.into_raw())
        .ok_or_else(|| format!("{} is empty", path.display()).into())
}

fn resize_rgba(frame: &RgbaFrame, width: u32, height: u32) -> RgbaFrame {
    let image = image::RgbaImage::from_raw(frame.width(), frame.height(), frame.pixels().to_vec())
        .unwrap_or_else(|| unreachable!("an RgbaFrame is exactly width * height * 4 bytes"));
    let resized = image::imageops::resize(&image, width, height, image::imageops::Triangle);
    RgbaFrame::new(width, height, resized.into_raw())
        .unwrap_or_else(|| unreachable!("the resized image has the requested size"))
}

fn save_rgba(path: &Path, frame: &RgbaFrame) -> Result<(), Box<dyn std::error::Error>> {
    let image = image::RgbaImage::from_raw(frame.width(), frame.height(), frame.pixels().to_vec())
        .ok_or("frame size mismatch")?;
    image.save(path)?;
    Ok(())
}

fn save_bgra(path: &Path, frame: &DecodedFrame) -> Result<(), Box<dyn std::error::Error>> {
    let rgb: Vec<u8> = frame
        .bgra
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|bgra| [bgra[2], bgra[1], bgra[0]])
        .collect();
    let image =
        image::RgbImage::from_raw(frame.width, frame.height, rgb).ok_or("frame size mismatch")?;
    image.save(path)?;
    Ok(())
}
