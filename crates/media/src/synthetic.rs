//! A capturer that renders a moving test pattern, for tests and machines without a display.

use std::time::Duration;

use crate::display::CaptureError;
use crate::frame::{CapturedFrame, RgbaFrame};
use crate::stream::ScreenCapturer;

/// Renders a horizontal gradient with a square that moves every frame.
#[derive(Debug, Clone)]
pub struct SyntheticCapturer {
    width: u32,
    height: u32,
    frame_index: u32,
}

impl SyntheticCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
            frame_index: 0,
        }
    }

    /// Changes the size of subsequent frames, like a display resolution change.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
    }

    /// Draws the next frame of the pattern.
    pub fn render(&mut self) -> RgbaFrame {
        let (width, height) = (self.width, self.height);
        let square = (width.min(height) / 4).max(1);
        let offset = (self.frame_index * 4) % width.max(1);
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                let inside = x.wrapping_sub(offset) < square && y.wrapping_sub(height / 3) < square;
                let shade = u8::try_from(x * 255 / width.max(1)).unwrap_or(u8::MAX);
                if inside {
                    pixels.extend_from_slice(&[240, 64, 32, 255]);
                } else {
                    pixels.extend_from_slice(&[shade, 128, 255 - shade, 255]);
                }
            }
        }
        self.frame_index = self.frame_index.wrapping_add(1);
        RgbaFrame::new(width, height, pixels)
            .unwrap_or_else(|| unreachable!("the pattern fills exactly width * height pixels"))
    }
}

impl ScreenCapturer for SyntheticCapturer {
    fn capture(&mut self, _timeout: Duration) -> Result<Option<CapturedFrame>, CaptureError> {
        Ok(Some(self.render().into()))
    }
}
