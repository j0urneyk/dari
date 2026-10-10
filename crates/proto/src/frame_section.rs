//! The layout of the shared frame section the secure-desktop helper writes and the app reads.

use serde::{Deserialize, Serialize};

use crate::validate::ValidationError;

/// Largest width or height of a frame in a section.
pub const MAX_FRAME_DIMENSION: u32 = 8192;
/// "DRFS", little-endian, at [`FrameLayout::MAGIC_OFFSET`].
pub const FRAME_SECTION_MAGIC: u32 = u32::from_le_bytes(*b"DRFS");
pub const FRAME_SECTION_VERSION: u32 = 1;

const PAGE_LEN: usize = 4096;

/// One of the section's two frame buffers. An enum, not an index, so a message can't name a
/// third buffer: anything else fails to decode.
/// One of the section's two frame buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FrameSlot {
    First,
    Second,
}

impl FrameSlot {
    pub const ALL: [FrameSlot; 2] = [FrameSlot::First, FrameSlot::Second];

    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::First => Self::Second,
            Self::Second => Self::First,
        }
    }
}

/// Where everything sits in a section for frames of `width` x `height`.
///
/// ```text
/// offset 0     header, one 4 KiB page, little-endian:
///               0  u32 magic      4  u32 version
///               8  u32 width     12  u32 height
///              16  u64 sequence of the frame in FrameSlot::First  (0 while it is being written)
///              24  u64 sequence of the frame in FrameSlot::Second (0 while it is being written)
/// offset 4096  FrameSlot::First: width * height RGBA pixels, tightly packed
/// then         FrameSlot::Second, at the next page boundary
/// ```
///
/// Pixels are RGBA, not BGRA: the helper swizzles while it copies rows out of its staging
/// texture anyway, so the app's read is one copy straight into an `RgbaFrame`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    width: u32,
    height: u32,
}

impl FrameLayout {
    pub const HEADER_LEN: usize = PAGE_LEN;
    pub const MAGIC_OFFSET: usize = 0;
    pub const VERSION_OFFSET: usize = 4;
    pub const WIDTH_OFFSET: usize = 8;
    pub const HEIGHT_OFFSET: usize = 12;

    /// Fails unless both dimensions are `1..=MAX_FRAME_DIMENSION`.
    pub fn new(width: u32, height: u32) -> Result<Self, ValidationError> {
        if width == 0 || width > MAX_FRAME_DIMENSION {
            return Err(ValidationError::InvalidValue { field: "width" });
        }
        if height == 0 || height > MAX_FRAME_DIMENSION {
            return Err(ValidationError::InvalidValue { field: "height" });
        }
        Ok(Self { width, height })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// `width * height * 4`. At most 256 MiB, so the whole section fits a 32-bit `usize` too.
    pub fn slot_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    pub fn slot_offset(&self, slot: FrameSlot) -> usize {
        match slot {
            FrameSlot::First => Self::HEADER_LEN,
            FrameSlot::Second => Self::HEADER_LEN + self.slot_len().next_multiple_of(PAGE_LEN),
        }
    }

    /// Offset of the slot's `u64` sequence word in the header, 8-byte aligned.
    pub fn sequence_offset(slot: FrameSlot) -> usize {
        match slot {
            FrameSlot::First => 16,
            FrameSlot::Second => 24,
        }
    }

    pub fn total_len(&self) -> usize {
        self.slot_offset(FrameSlot::Second) + self.slot_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layouts() -> Vec<FrameLayout> {
        [(1, 1), (3, 5), (1920, 1080), (1366, 768), (8192, 8192)]
            .into_iter()
            .map(|(width, height)| FrameLayout::new(width, height).unwrap())
            .collect()
    }

    #[test]
    fn slots_start_on_pages_after_the_header_and_do_not_overlap() {
        for layout in layouts() {
            let first = layout.slot_offset(FrameSlot::First);
            let second = layout.slot_offset(FrameSlot::Second);
            assert_eq!(first, FrameLayout::HEADER_LEN, "{layout:?}");
            assert_eq!(first % PAGE_LEN, 0, "{layout:?}");
            assert_eq!(second % PAGE_LEN, 0, "{layout:?}");
            assert!(first + layout.slot_len() <= second, "{layout:?}");
            assert_eq!(second + layout.slot_len(), layout.total_len(), "{layout:?}");
        }
    }

    #[test]
    fn the_largest_section_has_an_exact_size() {
        let layout = FrameLayout::new(MAX_FRAME_DIMENSION, MAX_FRAME_DIMENSION).unwrap();
        assert_eq!(layout.slot_len(), 268_435_456);
        assert_eq!(layout.total_len(), 4096 + 2 * 268_435_456);
    }

    #[test]
    fn header_fields_fit_the_header_without_overlapping() {
        let mut fields = vec![
            (FrameLayout::MAGIC_OFFSET, 4),
            (FrameLayout::VERSION_OFFSET, 4),
            (FrameLayout::WIDTH_OFFSET, 4),
            (FrameLayout::HEIGHT_OFFSET, 4),
        ];
        for slot in FrameSlot::ALL {
            let offset = FrameLayout::sequence_offset(slot);
            assert_eq!(offset % 8, 0, "{slot:?}");
            fields.push((offset, 8));
        }
        fields.sort_unstable();
        for pair in fields.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{pair:?}");
        }
        let (last, len) = fields[fields.len() - 1];
        assert!(last + len <= FrameLayout::HEADER_LEN);
    }

    #[test]
    fn dimensions_outside_the_bounds_are_rejected() {
        for (width, height) in [
            (0, 1080),
            (1920, 0),
            (MAX_FRAME_DIMENSION + 1, 1080),
            (1920, MAX_FRAME_DIMENSION + 1),
        ] {
            assert!(FrameLayout::new(width, height).is_err(), "{width}x{height}");
        }
    }

    #[test]
    fn each_slot_has_one_other() {
        assert_eq!(FrameSlot::First.other(), FrameSlot::Second);
        assert_eq!(FrameSlot::Second.other(), FrameSlot::First);
    }
}
