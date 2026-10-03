//! The picture a user can set behind the home window, and the layers made from it.
//!
//! The picture is the window's hero: it shows across the top half of the window and fades out
//! below, so the lower half, where most of the UI sits, keeps the app's own surface. GPUI cannot
//! blur or mask what it draws, so both are baked into the layers here, ahead of time:
//!
//! - `picture`: sharp across the top, its detail softening and its alpha falling to zero
//!   towards the middle. Fading the pixels themselves, rather than covering the picture with
//!   the surface, keeps the lower half free of the picture even when the window is translucent.
//! - `frost`: a small, heavily blurred copy, faded the same way. The sidebar draws it under its
//!   items, aligned with the window, so it reads as frosted glass over the picture.
//! - `average`: the average color of the part of the picture that shows. It tints the veil
//!   over the picture, so the surface belongs to the picture instead of sitting on it, and sets
//!   how much the veil has to cover for text to stay readable.

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

/// The narrowest picture, as width over height. Narrower ones lose rows above and below their
/// middle, so the picture covers the window without spilling far past it, and its fade lands
/// about where the window's middle is.
const NARROWEST: f32 = 1.5;

/// Where the picture softens into its blurred copy, as fractions of its height.
pub(crate) const SOFTEN_FROM: f32 = 0.28;
pub(crate) const SOFTEN_TO: f32 = 0.5;

/// Where the picture fades out, as fractions of its height: whole above `FADE_FROM`, gone below
/// `FADE_TO`.
pub(crate) const FADE_FROM: f32 = 0.36;
pub(crate) const FADE_TO: f32 = 0.62;

/// The layers made from the user's picture, ready to draw.
pub(crate) struct Backdrop {
    pub(crate) picture: RenderImage,
    pub(crate) frost: RenderImage,
    /// The average color of the part of the picture that shows, as sRGB bytes.
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
    let decoded = widen(decoded);

    let mut frost = imageops::fast_blur(&shrink(&decoded, SOFT_SIDE), FROST_SIGMA);
    let average = average(&frost, FADE_TO);
    let sharp = shrink(&decoded, MAX_SIDE);
    let (width, height) = sharp.dimensions();
    let soft = imageops::resize(&frost, width, height, imageops::FilterType::Triangle);
    let mut picture = if blur { soft.clone() } else { sharp };
    for (x, y, pixel) in picture.enumerate_pixels_mut() {
        let row = fraction(y, height);
        pixel.0 = mix(
            pixel.0,
            soft.get_pixel(x, y).0,
            smoothstep(SOFTEN_FROM, SOFTEN_TO, row),
        );
    }
    fade(&mut picture);
    fade(&mut frost);
    Ok(Backdrop {
        picture: render_image(picture),
        frost: render_image(frost),
        average,
    })
}

/// `image` cropped to its middle rows if it is narrower than [`NARROWEST`].
fn widen(image: DynamicImage) -> DynamicImage {
    let (width, height) = (image.width(), image.height());
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "picture sides are small and positive"
    )]
    let rows = (width as f32 / NARROWEST).round() as u32;
    if rows == 0 || rows >= height {
        return image;
    }
    image.crop_imm(0, (height - rows) / 2, width, rows)
}

/// `image` scaled down to fit `side`, never up.
fn shrink(image: &DynamicImage, side: u32) -> RgbaImage {
    if image.width() > side || image.height() > side {
        image.thumbnail(side, side).into_rgba8()
    } else {
        image.to_rgba8()
    }
}

/// Fades `image` out from [`FADE_FROM`] to [`FADE_TO`] of its height.
fn fade(image: &mut RgbaImage) {
    let height = image.height();
    for (_, y, pixel) in image.enumerate_pixels_mut() {
        let keep = 1. - smoothstep(FADE_FROM, FADE_TO, fraction(y, height));
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a byte scaled by a fraction stays a byte"
        )]
        let alpha = (f32::from(pixel.0[3]) * keep).round() as u8;
        pixel.0[3] = alpha;
    }
}

/// How far down `row` is in an image `height` rows tall, from 0 to 1.
fn fraction(row: u32, height: u32) -> f32 {
    #[expect(clippy::cast_precision_loss, reason = "pixel rows are small")]
    let fraction = row as f32 / height.max(1) as f32;
    fraction
}

/// The average color of `image` above `bottom`, a fraction of its height.
fn average(image: &RgbaImage, bottom: f32) -> [u8; 3] {
    let mut sums = [0u64; 3];
    let mut count = 0u64;
    for (_, y, pixel) in image.enumerate_pixels() {
        if fraction(y, image.height()) < bottom {
            for (sum, channel) in sums.iter_mut().zip(pixel.0) {
                *sum += u64::from(channel);
            }
            count += 1;
        }
    }
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
    // `RenderImage` expects BGRA, with straight alpha.
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
    fn a_narrow_picture_keeps_its_middle_rows() {
        let (_directory, path) = save(1200, 1600, [10, 20, 30, 255]);
        assert_eq!(size(&prepare(&path, false).unwrap().picture), (1200, 800));
    }

    #[test]
    fn the_picture_is_sharp_at_the_top_and_gone_at_the_bottom() {
        // Stripes one pixel wide: sharp, neighbors differ; blurred, they all turn gray.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stripes.png");
        RgbaImage::from_fn(300, 200, |x, _| {
            let value = if x % 2 == 0 { 0 } else { 255 };
            image::Rgba([value, value, value, 255])
        })
        .save(&path)
        .unwrap();
        let backdrop = prepare(&path, false).unwrap();
        let bytes = backdrop.picture.as_bytes(0).unwrap();
        let at = |x: usize, row: usize| &bytes[(row * 300 + x) * 4..][..4];
        let contrast = |row: usize| (i32::from(at(150, row)[0]) - i32::from(at(151, row)[0])).abs();
        assert_eq!(contrast(0), 255);
        assert_eq!(at(150, 0)[3], 255);
        assert!(contrast(110) < 20, "{}", contrast(110));
        assert_eq!(at(150, 199)[3], 0);
        let frost = backdrop.frost.as_bytes(0).unwrap();
        assert_eq!(frost[3], 255);
        assert_eq!(frost[frost.len() - 1], 0);
    }

    #[test]
    fn a_file_that_is_not_a_picture_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notes.png");
        std::fs::write(&path, "not a picture").unwrap();
        assert!(prepare(&path, false).is_err());
    }
}
