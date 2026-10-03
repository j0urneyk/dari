//! The picture a user can set behind the home window.

use std::path::Path;

use anyhow::Context as _;
use gpui_kit::RenderImage;
use image::{Frame, imageops};

/// The longest side the picture is decoded to. It sits behind translucent panels, softened,
/// so more detail would only cost memory.
const MAX_SIDE: u32 = 960;

/// How far the picture is blurred, in pixels of the downscaled image.
const BLUR_SIGMA: f32 = 10.;

/// Decodes the picture at `path` and softens it so text over it stays readable.
///
/// Slow for large photos; call it off the main thread.
pub(crate) fn prepare(path: &Path) -> anyhow::Result<RenderImage> {
    let picture = image::ImageReader::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("cannot decode {}", path.display()))?
        .thumbnail(MAX_SIDE, MAX_SIDE)
        .into_rgba8();
    let mut softened = imageops::fast_blur(&picture, BLUR_SIGMA);
    // `RenderImage` expects BGRA.
    for pixel in softened.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Ok(RenderImage::new(vec![Frame::new(softened)]))
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
        let prepared = prepare(&path).unwrap();
        let size = prepared.size(0);
        assert_eq!((size.width.0, size.height.0), (960, 480));
    }

    #[test]
    fn a_file_that_is_not_a_picture_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notes.png");
        std::fs::write(&path, "not a picture").unwrap();
        assert!(prepare(&path).is_err());
    }
}
