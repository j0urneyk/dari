//! Measures what a still or scrolling screen looks like after Dari's encoder and the viewer's
//! decoder, frame by frame, to pin down why text looks blurry at a given bitrate.
//!
//! ```sh
//! cargo run --release -p dari-media --example quality_probe -- <image.png|-> <bitrate-bps> <fps> <hardware|software> <still|scroll> [max-long-edge] [out-dir]
//! ```
//!
//! `-` instead of a PNG path renders a deterministic text-like test image (2560×1440; `-WxH`
//! picks another size). The scenarios are `still[:N]` (the image as the keyframe, then N
//! identical frames, as a host would send if it re-sent an unchanged screen; the Mac and Windows
//! hosts do not, so frame 0 is what their viewer keeps seeing) and `scroll[:K:PX:N]` (the image,
//! K frames each scrolled up by another PX pixels, then N identical frames at the final
//! position). Defaults: N = 30, K = 30, PX = 4.
//!
//! The frames go through the same `VideoEncoder` the capture thread builds (`stream.rs`), paced
//! at the frame rate because the hardware encoders stamp frames with the wall clock, and come
//! back through the viewer's `VideoDecoder`. They take the in-memory path: `FrameScaler::fit`
//! to the Quality preset's 2560 px by default, then `rgba_to_i420` (BT.601 limited range) into
//! NV12 for a hardware encoder. The Mac and Windows hosts scale and convert on the GPU instead,
//! which is tested to produce the same colors, so the probe measures the codec, not the
//! platform's scaler. Each frame waits for its output, which the capture thread does not.
//!
//! Each line prints the frame's index, type, encoded size, and PSNR of the picture the viewer
//! shows against the frame's source (a skipped frame leaves the previous picture up). PNGs of
//! the decoded picture at the end of motion (frame 0 for `still`, frame K for `scroll`), at the
//! first identical frame after it, and at the last frame are saved under `out-dir` (default
//! `target/quality_probe`) next to the source.

#![allow(clippy::print_stdout, reason = "measurement output")]

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dari_media::{
    CapturedFrame, DecodedFrame, EncoderSettings, FrameScaler, RgbaFrame, VideoDecoder,
    VideoEncoder,
};

/// PSNR reported for a frame identical to its source.
const IDENTICAL_PSNR: f64 = 99.0;

#[derive(Debug, Clone, Copy)]
enum Scenario {
    Still { hold: u32 },
    Scroll { steps: u32, pixels: u32, hold: u32 },
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
            "still" => Ok(Self::Still { hold: number(30)? }),
            "scroll" => Ok(Self::Scroll {
                steps: number(30)?,
                pixels: number(4)?,
                hold: number(30)?,
            }),
            _ => Err(text.to_owned()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Still { .. } => "still",
            Self::Scroll { .. } => "scroll",
        }
    }

    /// The source of every frame the host would send: how far the image has scrolled up, and
    /// whether the screen is still moving.
    fn plan(self) -> Vec<Step> {
        let start = Step {
            scrolled: 0,
            phase: Phase::MotionEnd,
        };
        match self {
            Self::Still { hold } => std::iter::once(start)
                .chain((0..hold).map(|_| Step {
                    scrolled: 0,
                    phase: Phase::Settled,
                }))
                .collect(),
            Self::Scroll {
                steps,
                pixels,
                hold,
            } => {
                let moving = (1..=steps).map(|index| Step {
                    scrolled: index * pixels,
                    phase: if index == steps {
                        Phase::MotionEnd
                    } else {
                        Phase::Moving
                    },
                });
                let held = (0..hold).map(|_| Step {
                    scrolled: steps * pixels,
                    phase: Phase::Settled,
                });
                std::iter::once(Step {
                    scrolled: 0,
                    phase: Phase::Moving,
                })
                .chain(moving)
                .chain(held)
                .collect()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Moving,
    /// The last frame with new content: the keyframe of `still`, the final scroll position of
    /// `scroll`. A host that never re-sends a still screen leaves this picture on the viewer.
    MotionEnd,
    /// An identical frame after motion ended.
    Settled,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Moving => "moving",
            Self::MotionEnd => "motion-end",
            Self::Settled => "settled",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Step {
    scrolled: u32,
    phase: Phase,
}

#[derive(Debug, Clone, Copy)]
enum FrameKind {
    Key,
    Predicted,
    /// The encoder emitted nothing (OpenH264 skips frames to hold its bitrate), so the viewer
    /// keeps showing the previous frame.
    Skipped,
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Key => "I",
            Self::Predicted => "P",
            Self::Skipped => "skip",
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
    index: usize,
    kind: FrameKind,
    bytes: usize,
    /// Of the picture the viewer shows after this frame; `None` while nothing has been decoded.
    psnr: Option<Psnr>,
}

struct Args {
    image: ImageSource,
    bitrate_bps: u32,
    fps: u32,
    hardware: bool,
    scenario: Scenario,
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
    let scenario =
        Scenario::parse(&next("scenario")?).map_err(|text| format!("bad scenario {text:?}"))?;
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
        max_long_edge,
        out_dir,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Shows which encoder was set up and any fallback on the way.
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

    #[expect(clippy::cast_precision_loss, reason = "frame rates are small integers")]
    let mut encoder = VideoEncoder::new(EncoderSettings {
        bitrate_bps: args.bitrate_bps,
        max_fps: args.fps.max(1) as f32,
        hardware: args.hardware,
    })?;
    println!(
        "encoder: {} (requested {})",
        backend_name(encoder.is_hardware()),
        backend_name(args.hardware)
    );
    let stem = format!(
        "{}-{}-{}bps-{}fps",
        args.scenario.name(),
        backend_name(encoder.is_hardware()),
        args.bitrate_bps,
        args.fps
    );
    save_rgba(&args.out_dir.join(format!("{stem}-source.png")), &source)?;
    let mut decoder = VideoDecoder::new()?;
    let plan = args.scenario.plan();
    let measurements = probe(
        &plan,
        &source,
        &mut encoder,
        &mut decoder,
        Duration::from_secs(1) / args.fps.max(1),
        &args.out_dir,
        &stem,
    )?;
    summarize(&measurements, &plan, args.bitrate_bps / 8 / args.fps.max(1));
    Ok(())
}

fn backend_name(hardware: bool) -> &'static str {
    if hardware { "hardware" } else { "software" }
}

/// Encodes every planned frame one frame interval apart, decodes it the way the viewer does,
/// and measures the picture the viewer shows against the frame's source. Saves the decoded
/// picture at the end of motion, at the first settled frame, and at the last frame.
fn probe(
    plan: &[Step],
    source: &RgbaFrame,
    encoder: &mut VideoEncoder,
    decoder: &mut VideoDecoder,
    interval: Duration,
    out_dir: &Path,
    stem: &str,
) -> Result<Vec<Measurement>, Box<dyn std::error::Error>> {
    println!(
        "{:>5} {:>4} {:>8} {:>9} {:>9}  phase",
        "frame", "type", "bytes", "psnr_y", "psnr_rgb"
    );
    let mut measurements = Vec::new();
    let mut shown: Option<DecodedFrame> = None;
    let mut hardware = encoder.is_hardware();
    let mut settled_saved = false;
    let started = Instant::now();
    for (index, step) in plan.iter().enumerate() {
        let frame = scrolled(source, step.scrolled);
        let due = started + interval * u32::try_from(index).unwrap_or(u32::MAX);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        let encoded = encoder.encode(&CapturedFrame::Rgba(frame.clone()))?;
        if encoder.is_hardware() != hardware {
            hardware = encoder.is_hardware();
            println!(
                "encoder fell back to {} at frame {index}",
                backend_name(hardware)
            );
        }
        if let Some(encoded) = &encoded
            && let Some(decoded) = decoder.decode(&encoded.data)?
        {
            shown = Some(decoded);
        }
        let psnr = shown
            .as_ref()
            .map(|shown| psnr_decoded(&frame, shown))
            .transpose()?;
        let measurement = Measurement {
            index,
            kind: encoded.as_ref().map_or(FrameKind::Skipped, |encoded| {
                if encoded.keyframe {
                    FrameKind::Key
                } else {
                    FrameKind::Predicted
                }
            }),
            bytes: encoded.as_ref().map_or(0, |encoded| encoded.data.len()),
            psnr,
        };
        measurements.push(measurement);
        println!(
            "{:>5} {:>4} {:>8} {:>9} {:>9}  {}",
            measurement.index,
            measurement.kind,
            measurement.bytes,
            db(measurement.psnr.map(|psnr| psnr.y)),
            db(measurement.psnr.map(|psnr| psnr.rgb)),
            step.phase.name()
        );
        let Some(shown) = &shown else { continue };
        let first_settled = step.phase == Phase::Settled && !settled_saved;
        if step.phase == Phase::MotionEnd {
            save_bgra(
                &out_dir.join(format!("{stem}-motion-end-frame{index}.png")),
                shown,
            )?;
        }
        if first_settled {
            settled_saved = true;
            save_bgra(
                &out_dir.join(format!("{stem}-settled-frame{index}.png")),
                shown,
            )?;
        }
        if index + 1 == plan.len() {
            save_bgra(
                &out_dir.join(format!("{stem}-last-frame{index}.png")),
                shown,
            )?;
        }
    }
    Ok(measurements)
}

/// A PSNR column: `-` while nothing has been decoded.
fn db(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("{value:.2}"))
}

fn summarize(measurements: &[Measurement], plan: &[Step], budget: u32) {
    let total: usize = measurements.iter().map(|m| m.bytes).sum();
    let keyframes = measurements
        .iter()
        .filter(|m| matches!(m.kind, FrameKind::Key))
        .count();
    let skipped = measurements
        .iter()
        .filter(|m| matches!(m.kind, FrameKind::Skipped))
        .count();
    let psnr_y_at = |phase: Phase| {
        plan.iter()
            .position(|step| step.phase == phase)
            .and_then(|index| measurements.get(index))
            .and_then(|m| m.psnr.map(|psnr| psnr.y))
    };
    let last = measurements.last().and_then(|m| m.psnr.map(|psnr| psnr.y));
    let worst_settled = measurements
        .iter()
        .zip(plan)
        .filter(|(_, step)| step.phase == Phase::Settled)
        .filter_map(|(m, _)| m.psnr.map(|psnr| psnr.y))
        .reduce(f64::min);
    #[expect(clippy::cast_precision_loss, reason = "report only")]
    let mean_bytes = total as f64 / measurements.len().max(1) as f64;
    println!(
        "summary: {} frames ({keyframes} key, {skipped} skipped), {total} bytes, mean {mean_bytes:.0} bytes per frame (budget {budget}); PSNR Y at motion end {} dB, first settled {} dB, last {} dB, worst settled {} dB",
        measurements.len(),
        db(psnr_y_at(Phase::MotionEnd)),
        db(psnr_y_at(Phase::Settled)),
        db(last),
        db(worst_settled),
    );
}

/// The image scrolled up by `rows`, the rows that left at the top coming back in at the bottom
/// so the frame stays full of text.
fn scrolled(source: &RgbaFrame, rows: u32) -> RgbaFrame {
    let stride = source.width() as usize * 4;
    let rows = (rows % source.height()) as usize;
    let pixels = source.pixels();
    let mut shifted = Vec::with_capacity(pixels.len());
    shifted.extend_from_slice(&pixels[rows * stride..]);
    shifted.extend_from_slice(&pixels[..rows * stride]);
    RgbaFrame::new(source.width(), source.height(), shifted)
        .unwrap_or_else(|| unreachable!("the rotated buffer keeps its size"))
}

fn luma(r: u8, g: u8, b: u8) -> f64 {
    0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
}

/// PSNR of luma (Rec. 601 weights on the RGB values) and of the RGB channels, between two
/// frames given as `(r, g, b)` iterators of equal length.
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

/// A deterministic page of small "text": lines of glyph-like 1 px strokes, black on white,
/// with the reddish and bluish fringes subpixel antialiasing leaves on stroke edges, under a
/// light title bar. Nothing in the repository renders fonts, and the codec only cares about the
/// thin high-contrast edges, which this has at the density of a 9 px font.
fn render_text_page(width: u32, height: u32) -> RgbaFrame {
    const CELL_WIDTH: u32 = 7;
    const LINE_HEIGHT: u32 = 13;
    const GLYPH_HEIGHT: u32 = 8;
    const MARGIN: u32 = 24;
    const TITLE_BAR: u32 = 40;
    let (width, height) = (width.max(2) & !1, height.max(2) & !1);
    let mut page = vec![255u8; (width * height * 4) as usize];
    let mut set = |x: u32, y: u32, [r, g, b]: [u8; 3]| {
        if x < width && y < height {
            let at = ((y * width + x) * 4) as usize;
            page[at..at + 3].copy_from_slice(&[r, g, b]);
        }
    };
    for y in 0..TITLE_BAR.min(height) {
        for x in 0..width {
            set(x, y, [236, 236, 238]);
        }
    }
    let mut random = Lcg(0x9E37_79B9);
    let mut line_top = TITLE_BAR + 8;
    let mut line_index = 0u32;
    while line_top + LINE_HEIGHT <= height {
        // Paragraph breaks and ragged right edges, like prose.
        let paragraph_break = line_index % 9 == 8;
        let line_width = if line_index % 9 == 7 {
            width / 2 + random.below(width / 3)
        } else {
            width - MARGIN
        };
        if !paragraph_break {
            let mut x = MARGIN + 4 * u32::from(line_index.is_multiple_of(7)) * CELL_WIDTH;
            let mut word_left = random.below(7) + 2;
            while x + CELL_WIDTH <= line_width {
                if word_left == 0 {
                    word_left = random.below(7) + 2;
                    x += CELL_WIDTH;
                    continue;
                }
                word_left -= 1;
                draw_glyph(&mut set, &mut random, x, line_top, GLYPH_HEIGHT);
                x += CELL_WIDTH;
            }
        }
        line_top += LINE_HEIGHT;
        line_index += 1;
    }
    RgbaFrame::new(width, height, page)
        .unwrap_or_else(|| unreachable!("the page fills exactly width * height pixels"))
}

/// Two to four 1 px strokes in a 5×8 box: verticals, horizontals, and a diagonal, like the
/// stems and bars of Latin letters.
fn draw_glyph(
    set: &mut impl FnMut(u32, u32, [u8; 3]),
    random: &mut Lcg,
    left: u32,
    top: u32,
    glyph_height: u32,
) {
    const INK: [u8; 3] = [16, 16, 16];
    const LEFT_FRINGE: [u8; 3] = [255, 196, 176];
    const RIGHT_FRINGE: [u8; 3] = [176, 196, 255];
    let strokes = random.below(3) + 2;
    for _ in 0..strokes {
        match random.below(4) {
            0 | 1 => {
                let x = left + [0, 2, 4][random.below(3) as usize];
                let (from, to) = if random.below(3) == 0 {
                    (glyph_height / 2, glyph_height)
                } else {
                    (0, glyph_height)
                };
                for y in from..to {
                    set(x.wrapping_sub(1), top + y, LEFT_FRINGE);
                    set(x, top + y, INK);
                    set(x + 1, top + y, RIGHT_FRINGE);
                }
            }
            2 => {
                let y = top + [0, glyph_height / 2, glyph_height - 1][random.below(3) as usize];
                for x in left..left + 5 {
                    set(x, y, INK);
                }
            }
            _ => {
                for step in 0..glyph_height.min(5) {
                    set(left + step, top + glyph_height - 1 - step, INK);
                }
            }
        }
    }
}

/// A tiny deterministic generator, so every run draws the same page.
struct Lcg(u32);

impl Lcg {
    fn below(&mut self, bound: u32) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) % bound.max(1)
    }
}
