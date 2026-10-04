/// An uncompressed frame in RGBA byte order, tightly packed (`width * 4` bytes per row).
#[derive(Clone, PartialEq, Eq)]
pub struct RgbaFrame {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl RgbaFrame {
    /// Wraps `pixels`. Returns `None` if the buffer size does not match the dimensions.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Option<Self> {
        let expected = usize::try_from(width)
            .ok()?
            .checked_mul(usize::try_from(height).ok()?)?
            .checked_mul(4)?;
        (width > 0 && height > 0 && pixels.len() == expected).then_some(Self {
            width,
            height,
            pixels,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

/// A captured screen image on its way to the encoder.
#[derive(Debug, Clone)]
pub enum CapturedFrame {
    /// Pixels in memory at the display's size; the stream scales them to its size limit.
    Rgba(RgbaFrame),
    /// A frame in GPU memory, already at the stream's size and in NV12: a ScreenCaptureKit
    /// pixel buffer on macOS, a Direct3D 11 texture on Windows.
    #[cfg(any(target_os = "macos", windows))]
    Native(crate::native::NativeFrame),
}

impl CapturedFrame {
    pub fn width(&self) -> u32 {
        match self {
            CapturedFrame::Rgba(frame) => frame.width(),
            #[cfg(any(target_os = "macos", windows))]
            CapturedFrame::Native(frame) => frame.width(),
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            CapturedFrame::Rgba(frame) => frame.height(),
            #[cfg(any(target_os = "macos", windows))]
            CapturedFrame::Native(frame) => frame.height(),
        }
    }
}

impl From<RgbaFrame> for CapturedFrame {
    fn from(frame: RgbaFrame) -> Self {
        CapturedFrame::Rgba(frame)
    }
}

impl std::fmt::Debug for RgbaFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RgbaFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_mismatched_buffers() {
        assert!(RgbaFrame::new(2, 2, vec![0; 16]).is_some());
        assert!(RgbaFrame::new(2, 2, vec![0; 15]).is_none());
        assert!(RgbaFrame::new(0, 2, Vec::new()).is_none());
        assert!(RgbaFrame::new(u32::MAX, u32::MAX, Vec::new()).is_none());
    }
}
