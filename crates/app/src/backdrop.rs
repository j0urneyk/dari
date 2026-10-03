//! The picture a user can set behind the home window, and the layers made from it.
//!
//! The picture is the window's hero: it shows sharp across the top, where there is little UI,
//! and softens into a calm surface below, where the UI sits. GPUI cannot blur what is behind an
//! element, so the blurred layers are made here, ahead of time:
//!
//! - `picture`: sharp across the top, crossfading into a heavily blurred copy further down, so
//!   the lower part of the window shows only the picture's colors, not its detail. It is one
//!   image because GPUI fades each layer separately: a sharp layer under a blurred one would
//!   show through as soon as the picture is drawn translucent.
//! - `frost`: a small, heavily blurred copy. Panels draw it under their tint, aligned with the
//!   window, so they read as frosted glass over the picture.
//! - `average`: the picture's average color, which tints the veil over it so the surface
//!   belongs to the picture instead of sitting on it.

use std::path::Path;

use anyhow::Context as _;
use gpui_kit::RenderImage;
use image::{DynamicImage, Frame, RgbaImage, imageops};

/// The longest side the picture is decoded to: enough for a large window on a Retina display.
const MAX_SIDE: u32 = 2560;

/// The longest side of the blurred layers. Blurring hides detail, and a small picture blurs fast.
const SOFT_SIDE: u32 = 480;

/// How far the blurred layers are blurred, in pixels of the small picture.
const FROST_SIGMA: f32 = 14.;

/// Where `picture` starts and finishes crossfading into its blurred copy, as fractions of its
/// height.
const SOFTEN_FROM: f32 = 0.08;
const SOFTEN_TO: f32 = 0.28;

/// The layers made from the user's picture, ready to draw.
pub(crate) struct Backdrop {
    pub(crate) picture: RenderImage,
    pub(crate) frost: RenderImage,
    /// The picture's average color, as sRGB bytes.
    pub(crate) average: [u8; 3],
}

/// Decodes the picture at `path` and makes its layers; `blur` blurs the top of the picture too.
///
/// Slow for large photos; call it off the main thread.
pub(crate) fn prepare(path: &Path, blur: bool) -> anyhow::Result<Backdrop> {
    let decoded = image::ImageReader::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("cannot decode {}", path.display()))?;

    let frost = imageops::fast_blur(&shrink(&decoded, SOFT_SIDE), FROST_SIGMA);
    let average = average(&frost);
    let sharp = shrink(&decoded, MAX_SIDE);
    let (width, height) = sharp.dimensions();
    let soft = imageops::resize(&frost, width, height, imageops::FilterType::Triangle);
    let picture = if blur {
        soft
    } else {
        let mut picture = sharp;
        let rows = height.max(1);
        for (x, y, pixel) in picture.enumerate_pixels_mut() {
            #[expect(clippy::cast_precision_loss, reason = "pixel rows are small")]
            let blend = smoothstep(SOFTEN_FROM, SOFTEN_TO, y as f32 / rows as f32);
            pixel.0 = mix(pixel.0, soft.get_pixel(x, y).0, blend);
        }
        picture
    };
    Ok(Backdrop {
        picture: render_image(picture),
        frost: render_image(frost),
        average,
    })
}

/// `image` scaled down to fit `side`, never up.
fn shrink(image: &DynamicImage, side: u32) -> RgbaImage {
    if image.width() > side || image.height() > side {
        image.thumbnail(side, side).into_rgba8()
    } else {
        image.to_rgba8()
    }
}

fn average(image: &RgbaImage) -> [u8; 3] {
    let mut sums = [0u64; 3];
    for pixel in image.pixels() {
        for (sum, channel) in sums.iter_mut().zip(pixel.0) {
            *sum += u64::from(channel);
        }
    }
    let count = u64::from(image.width()) * u64::from(image.height());
    sums.map(|sum| u8::try_from(sum / count.max(1)).unwrap_or(u8::MAX))
}

fn smoothstep(from: f32, to: f32, value: f32) -> f32 {
    let t = ((value - from) / (to - from)).clamp(0., 1.);
    t * t * (3. - 2. * t)
}

/// `from` moved `amount` of the way toward `to`, channel by channel.
fn mix(from: [u8; 4], to: [u8; 4], amount: f32) -> [u8; 4] {
    let mut mixed = from;
    for (channel, (start, end)) in mixed.iter_mut().zip(from.into_iter().zip(to)) {
        let value = f32::from(start) + (f32::from(end) - f32::from(start)) * amount;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "between two bytes, so within 0..=255"
        )]
        let byte = value.round() as u8;
        *channel = byte;
    }
    mixed
}

fn render_image(mut image: RgbaImage) -> RenderImage {
    // `RenderImage` expects BGRA.
    for pixel in image.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    RenderImage::new(vec![Frame::new(image)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(width: u32, height: u32, color: [u8; 4]) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("picture.png");
        RgbaImage::from_pixel(width, height, image::Rgba(color))
            .save(&path)
            .unwrap();
        (directory, path)
    }

    fn size(image: &RenderImage) -> (i32, i32) {
        let size = image.size(0);
        (size.width.0, size.height.0)
    }

    #[test]
    fn a_large_photo_is_downscaled_and_its_frost_is_small() {
        let (_directory, path) = save(3000, 1500, [10, 20, 30, 255]);
        let sharp = prepare(&path, false).unwrap();
        assert_eq!(size(&sharp.picture), (2560, 1280));
        assert_eq!(size(&sharp.frost), (480, 240));
        let blurred = prepare(&path, true).unwrap();
        assert_eq!(size(&blurred.picture), (2560, 1280));
    }

    #[test]
    fn a_small_picture_is_not_scaled_up() {
        let (_directory, path) = save(300, 200, [10, 20, 30, 255]);
        assert_eq!(size(&prepare(&path, false).unwrap().picture), (300, 200));
    }

    #[test]
    fn the_average_color_is_the_pictures_color() {
        let (_directory, path) = save(64, 64, [200, 100, 50, 255]);
        assert_eq!(prepare(&path, false).unwrap().average, [200, 100, 50]);
    }

    #[test]
    fn the_picture_is_sharp_at_the_top_and_soft_at_the_bottom() {
        // Stripes one pixel wide: sharp, neighbors differ; blurred, they all turn gray.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stripes.png");
        RgbaImage::from_fn(200, 200, |x, _| {
            let value = if x % 2 == 0 { 0 } else { 255 };
            image::Rgba([value, value, value, 255])
        })
        .save(&path)
        .unwrap();
        let picture = prepare(&path, false).unwrap().picture;
        let bytes = picture.as_bytes(0).unwrap();
        let contrast = |row: usize| {
            let at = |x: usize| i32::from(bytes[(row * 200 + x) * 4]);
            (at(100) - at(101)).abs()
        };
        assert_eq!(contrast(0), 255);
        assert!(contrast(199) < 20, "{}", contrast(199));
    }

    #[test]
    fn a_file_that_is_not_a_picture_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notes.png");
        std::fs::write(&path, "not a picture").unwrap();
        assert!(prepare(&path, false).is_err());
    }
}
