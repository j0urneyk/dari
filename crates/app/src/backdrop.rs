//! The picture a user can set behind the home window.

use std::path::Path;

use anyhow::Context as _;
use gpui_kit::RenderImage;
use image::{Frame, imageops};

/// The longest side the picture is decoded to: enough for a large window on a Retina display.
const MAX_SIDE: u32 = 2560;

/// The longest side a blurred picture is decoded to. Blurring hides the detail anyway, and a
/// small picture blurs fast.
const BLURRED_MAX_SIDE: u32 = 960;

/// How far a blurred picture is blurred, in pixels of the downscaled image.
const BLUR_SIGMA: f32 = 10.;

/// Decodes the picture at `path`, blurring it when `blur` is set so text over it reads more
/// easily.
///
/// Slow for large photos; call it off the main thread.
pub(crate) fn prepare(path: &Path, blur: bool) -> anyhow::Result<RenderImage> {
    let picture = image::ImageReader::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("cannot decode {}", path.display()))?;
    let side = if blur { BLURRED_MAX_SIDE } else { MAX_SIDE };
    let picture = if picture.width() > side || picture.height() > side {
        picture.thumbnail(side, side)
    } else {
        picture
    }
    .into_rgba8();
    let mut picture = if blur {
        imageops::fast_blur(&picture, BLUR_SIGMA)
    } else {
        picture
    };
    // `RenderImage` expects BGRA.
    for pixel in picture.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(RenderImage::new(vec![Frame::new(picture)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_photo_is_downscaled() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wide.png");
        image::RgbaImage::from_pixel(3000, 1500, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .unwrap();
        let sharp = prepare(&path, false).unwrap().size(0);
        assert_eq!((sharp.width.0, sharp.height.0), (2560, 1280));
        let blurred = prepare(&path, true).unwrap().size(0);
        assert_eq!((blurred.width.0, blurred.height.0), (960, 480));
    }

    #[test]
    fn a_file_that_is_not_a_picture_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notes.png");
        std::fs::write(&path, "not a picture").unwrap();
        assert!(prepare(&path, false).is_err());
    }
}
