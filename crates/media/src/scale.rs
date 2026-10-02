//! Fitting captured frames into the encoder's size limits.

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

use crate::frame::RgbaFrame;

/// OpenH264 cannot encode beyond 3840x2160 (level 5.2).
pub const MAX_ENCODED_LONG_EDGE: u32 = 3840;
const MAX_ENCODED_SHORT_EDGE: u32 = 2160;

/// Largest size with the same aspect ratio that fits `max_long_edge` and the encoder's limits,
/// rounded down to even dimensions as YUV 4:2:0 requires. Never upscales.
pub fn fit_within(width: u32, height: u32, max_long_edge: u32) -> (u32, u32) {
    let long_limit = max_long_edge.clamp(2, MAX_ENCODED_LONG_EDGE);
    let (long, short) = (width.max(height), width.min(height));
    let scale = [
        1.0,
        f64::from(long_limit) / f64::from(long.max(1)),
        f64::from(MAX_ENCODED_SHORT_EDGE) / f64::from(short.max(1)),
    ]
    .into_iter()
    .fold(f64::INFINITY, f64::min);
    let scaled = |value: u32| {
        // Truncation is intended: the result must not exceed the limits.
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let scaled = (f64::from(value) * scale).floor() as u32;
        (scaled & !1).max(2)
    };
    (scaled(width), scaled(height))
}

/// Resizes frames to the stream size, reusing its working buffers.
pub struct FrameScaler {
    resizer: Resizer,
    options: ResizeOptions,
}

impl std::fmt::Debug for FrameScaler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameScaler").finish_non_exhaustive()
    }
}

impl Default for FrameScaler {
    fn default() -> Self {
        Self {
            resizer: Resizer::new(),
            // Bilinear keeps text legible when downscaling a desktop and is cheap.
            options: ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear)),
        }
    }
}

impl FrameScaler {
    /// Returns `frame` resized to fit `max_long_edge` with even dimensions, or unchanged if it
    /// already fits.
    pub fn fit(&mut self, frame: RgbaFrame, max_long_edge: u32) -> RgbaFrame {
        let (width, height) = fit_within(frame.width(), frame.height(), max_long_edge);
        if (width, height) == (frame.width(), frame.height()) {
            return frame;
        }
        let Ok(source) = ImageRef::new(
            frame.width(),
            frame.height(),
            frame.pixels(),
            PixelType::U8x4,
        ) else {
            return frame;
        };
        let mut target = Image::new(width, height, PixelType::U8x4);
        if self
            .resizer
            .resize(&source, &mut target, &self.options)
            .is_err()
        {
            return frame;
        }
        RgbaFrame::new(width, height, target.into_vec()).unwrap_or(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_even_frames_are_unchanged() {
        assert_eq!(fit_within(1280, 720, 1920), (1280, 720));
    }

    #[test]
    fn large_frames_keep_their_aspect_ratio() {
        assert_eq!(fit_within(5120, 2880, 1920), (1920, 1080));
        assert_eq!(fit_within(2880, 5120, 1920), (1080, 1920));
    }

    #[test]
    fn odd_dimensions_become_even() {
        assert_eq!(fit_within(1365, 767, 1920), (1364, 766));
    }

    #[test]
    fn encoder_limits_apply_even_with_a_high_limit() {
        let (width, height) = fit_within(7680, 4320, 10_000);
        assert!(width <= MAX_ENCODED_LONG_EDGE && height <= MAX_ENCODED_SHORT_EDGE);
        let (width, height) = fit_within(3000, 3000, 10_000);
        assert_eq!((width, height), (2160, 2160));
    }

    #[test]
    fn scaler_produces_the_fitted_size() {
        let frame = RgbaFrame::new(101, 51, vec![200; 101 * 51 * 4]).unwrap();
        let scaled = FrameScaler::default().fit(frame, 1920);
        assert_eq!((scaled.width(), scaled.height()), (100, 50));
        assert!(
            scaled
                .pixels()
                .iter()
                .all(|value| (199..=201).contains(value))
        );
    }
}
