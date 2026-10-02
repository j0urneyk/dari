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
