//! The pointer, which Desktop Duplication leaves out of its frames (Phase 0): its shapes as
//! `GetFramePointerShape` returns them, and blending one into an RGBA frame the way Microsoft's
//! `DesktopDuplication` sample does.

const MONOCHROME: u32 = 1;
const COLOR: u32 = 2;
const MASKED_COLOR: u32 = 4;

/// `GetFramePointerShape`'s buffer and the fields of its `DXGI_OUTDUPL_POINTER_SHAPE_INFO`, as
/// DXGI returned them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawPointerShape {
    pub(crate) kind: u32,
    pub(crate) width: u32,
    /// For a monochrome shape, the height of both masks together.
    pub(crate) height: u32,
    pub(crate) pitch: u32,
    pub(crate) buffer: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShapeKind {
    /// 1 bpp: an AND mask, then an XOR mask of the same size.
    Monochrome,
    /// 32 bpp BGRA, blended by its alpha.
    Color,
    /// 32 bpp BGRA: alpha 0 replaces the pixel, any other alpha XORs it.
    MaskedColor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PointerShape {
    kind: ShapeKind,
    width: usize,
    /// Rows of the drawn pointer, so half a monochrome shape's buffer.
    height: usize,
    pitch: usize,
    buffer: Vec<u8>,
}

impl PointerShape {
    /// `None` for an unknown type or a buffer too short for the dimensions.
    pub(crate) fn parse(raw: RawPointerShape) -> Option<Self> {
        let width = raw.width as usize;
        let pitch = raw.pitch as usize;
        let (kind, height, buffer_rows, row_len) = match raw.kind {
            MONOCHROME if raw.height.is_multiple_of(2) => (
                ShapeKind::Monochrome,
                raw.height / 2,
                raw.height,
                width.div_ceil(8),
            ),
            COLOR => (ShapeKind::Color, raw.height, raw.height, width * 4),
            MASKED_COLOR => (ShapeKind::MaskedColor, raw.height, raw.height, width * 4),
            _ => return None,
        };
        let needed = pitch.checked_mul(buffer_rows as usize)?;
        if width == 0 || height == 0 || pitch < row_len || raw.buffer.len() < needed {
            return None;
        }
        Some(Self {
            kind,
            width,
            height: height as usize,
            pitch,
            buffer: raw.buffer,
        })
    }
}

/// `DXGI_OUTDUPL_POINTER_POSITION`: where the shape's top-left sits on the output, as the
/// `DesktopDuplication` sample uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PointerPosition {
    pub(crate) visible: bool,
    pub(crate) x: i32,
    pub(crate) y: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Pointer {
    shape: Option<PointerShape>,
    position: Option<PointerPosition>,
}

impl Pointer {
    pub(crate) fn moved(&mut self, position: PointerPosition) {
        self.position = Some(position);
    }

    pub(crate) fn reshaped(&mut self, shape: PointerShape) {
        self.shape = Some(shape);
    }

    /// Blends the pointer into a tightly packed RGBA `frame` that is `width` pixels wide,
    /// clipped to the frame.
    pub(crate) fn draw(&self, frame: &mut [u8], width: u32) {
        let (Some(shape), Some(position)) = (&self.shape, self.position) else {
            return;
        };
        let width = width as usize;
        if !position.visible || width == 0 {
            return;
        }
        let height = frame.len() / (width * 4);
        let (Some(columns), Some(rows)) = (
            Overlap::of(position.x, shape.width, width),
            Overlap::of(position.y, shape.height, height),
        ) else {
            return;
        };
        for row in 0..rows.count {
            let frame_row = rows.frame + row;
            for column in 0..columns.count {
                let at = (frame_row * width + columns.frame + column) * 4;
                blend(
                    shape,
                    rows.shape + row,
                    columns.shape + column,
                    &mut frame[at..at + 3],
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Overlap {
    shape: usize,
    frame: usize,
    count: usize,
}

impl Overlap {
    fn of(start: i32, len: usize, limit: usize) -> Option<Self> {
        let start = i64::from(start);
        let len = i64::try_from(len).ok()?;
        let limit = i64::try_from(limit).ok()?;
        let first = (-start).clamp(0, len);
        let end = (limit - start).clamp(first, len);
        let count = usize::try_from(end - first)
            .ok()
            .filter(|&count| count > 0)?;
        Some(Self {
            shape: usize::try_from(first).ok()?,
            frame: usize::try_from(start + first).ok()?,
            count,
        })
    }
}

fn blend(shape: &PointerShape, row: usize, column: usize, rgb: &mut [u8]) {
    match shape.kind {
        ShapeKind::Monochrome => {
            let bit = |row: usize| {
                shape.buffer[row * shape.pitch + column / 8] & (0x80 >> (column % 8)) != 0
            };
            let and = if bit(row) { 0xFF } else { 0x00 };
            let xor = if bit(row + shape.height) { 0xFF } else { 0x00 };
            for channel in rgb {
                *channel = (*channel & and) ^ xor;
            }
        }
        ShapeKind::Color => {
            let bgra = bgra(shape, row, column);
            let alpha = u16::from(bgra[3]);
            for (channel, source) in rgb.iter_mut().zip([bgra[2], bgra[1], bgra[0]]) {
                let mixed =
                    (u16::from(source) * alpha + u16::from(*channel) * (255 - alpha) + 127) / 255;
                *channel = u8::try_from(mixed).unwrap_or(u8::MAX);
            }
        }
        ShapeKind::MaskedColor => {
            let bgra = bgra(shape, row, column);
            let xor = bgra[3] != 0;
            for (channel, source) in rgb.iter_mut().zip([bgra[2], bgra[1], bgra[0]]) {
                *channel = if xor { *channel ^ source } else { source };
            }
        }
    }
}

fn bgra(shape: &PointerShape, row: usize, column: usize) -> [u8; 4] {
    let at = row * shape.pitch + column * 4;
    [
        shape.buffer[at],
        shape.buffer[at + 1],
        shape.buffer[at + 2],
        shape.buffer[at + 3],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const BACKGROUND: [u8; 4] = [10, 20, 30, 255];

    fn frame(width: usize, height: usize) -> Vec<u8> {
        BACKGROUND.repeat(width * height)
    }

    fn pixel(frame: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        let at = (y * width + x) * 4;
        frame[at..at + 4].try_into().unwrap()
    }

    fn pointer_at(shape: RawPointerShape, x: i32, y: i32) -> Pointer {
        let mut pointer = Pointer::default();
        pointer.reshaped(PointerShape::parse(shape).unwrap());
        pointer.moved(PointerPosition {
            visible: true,
            x,
            y,
        });
        pointer
    }

    fn solid(kind: u32, width: u32, height: u32, bgra: [u8; 4]) -> RawPointerShape {
        RawPointerShape {
            kind,
            width,
            height,
            pitch: width * 4,
            buffer: bgra.repeat((width * height) as usize),
        }
    }

    #[test]
    fn monochrome_masks_follow_the_and_xor_truth_table() {
        let shape = RawPointerShape {
            kind: MONOCHROME,
            width: 4,
            height: 2,
            pitch: 1,
            buffer: vec![0b1001_0000, 0b0011_0000],
        };
        let mut pixels = frame(4, 1);
        pointer_at(shape, 0, 0).draw(&mut pixels, 4);
        assert_eq!(pixel(&pixels, 4, 0, 0), BACKGROUND, "transparent");
        assert_eq!(pixel(&pixels, 4, 1, 0), [0, 0, 0, 255], "black");
        assert_eq!(pixel(&pixels, 4, 2, 0), [255, 255, 255, 255], "white");
        assert_eq!(pixel(&pixels, 4, 3, 0), [245, 235, 225, 255], "inverted");
    }

    #[test]
    fn color_shapes_blend_by_alpha_and_swap_to_rgba() {
        for (alpha, expected) in [
            (255, [200, 100, 50, 255]),
            (0, BACKGROUND),
            (128, [105, 60, 40, 255]),
        ] {
            let mut pixels = frame(1, 1);
            pointer_at(solid(COLOR, 1, 1, [50, 100, 200, alpha]), 0, 0).draw(&mut pixels, 1);
            assert_eq!(pixel(&pixels, 1, 0, 0), expected, "alpha {alpha}");
        }
    }

    #[test]
    fn masked_color_replaces_at_alpha_zero_and_xors_otherwise() {
        let mut pixels = frame(1, 1);
        pointer_at(solid(MASKED_COLOR, 1, 1, [3, 2, 1, 0]), 0, 0).draw(&mut pixels, 1);
        assert_eq!(pixel(&pixels, 1, 0, 0), [1, 2, 3, 255]);

        let mut pixels = frame(1, 1);
        pointer_at(solid(MASKED_COLOR, 1, 1, [0xFF, 0, 0x0F, 0xFF]), 0, 0).draw(&mut pixels, 1);
        assert_eq!(pixel(&pixels, 1, 0, 0), [5, 20, 225, 255]);
    }

    #[test]
    fn a_pointer_past_any_edge_is_clipped_not_wrapped() {
        let white = [255, 255, 255, 255];
        let drawn = |x: i32, y: i32| {
            let mut pixels = frame(3, 3);
            pointer_at(solid(COLOR, 2, 2, white), x, y).draw(&mut pixels, 3);
            let mut painted = Vec::new();
            for py in 0..3 {
                for px in 0..3 {
                    if pixel(&pixels, 3, px, py) == white {
                        painted.push((px, py));
                    }
                }
            }
            painted
        };
        assert_eq!(drawn(-1, -1), [(0, 0)]);
        assert_eq!(drawn(2, -1), [(2, 0)]);
        assert_eq!(drawn(-1, 2), [(0, 2)]);
        assert_eq!(drawn(2, 2), [(2, 2)]);
        assert_eq!(drawn(1, 0), [(1, 0), (2, 0), (1, 1), (2, 1)]);
        assert_eq!(drawn(3, 0), []);
        assert_eq!(drawn(0, 3), []);
        assert_eq!(drawn(-2, 0), []);
        assert_eq!(drawn(i32::MIN, i32::MAX), []);
    }

    #[test]
    fn an_invisible_or_unplaced_pointer_draws_nothing() {
        let shape = solid(COLOR, 1, 1, [0, 0, 0, 255]);
        let mut pointer = pointer_at(shape.clone(), 0, 0);
        pointer.moved(PointerPosition {
            visible: false,
            x: 0,
            y: 0,
        });
        let mut pixels = frame(1, 1);
        pointer.draw(&mut pixels, 1);
        assert_eq!(pixels, frame(1, 1));

        let mut unplaced = Pointer::default();
        unplaced.reshaped(PointerShape::parse(shape).unwrap());
        unplaced.draw(&mut pixels, 1);
        assert_eq!(pixels, frame(1, 1));
    }

    #[test]
    fn malformed_shapes_are_rejected() {
        let good = solid(COLOR, 2, 2, [0; 4]);
        assert!(PointerShape::parse(good.clone()).is_some());
        for bad in [
            RawPointerShape {
                kind: 3,
                ..good.clone()
            },
            RawPointerShape {
                buffer: vec![0; 15],
                ..good.clone()
            },
            RawPointerShape {
                pitch: 7,
                ..good.clone()
            },
            RawPointerShape {
                width: 0,
                ..good.clone()
            },
            RawPointerShape {
                kind: MONOCHROME,
                width: 8,
                height: 3,
                pitch: 1,
                buffer: vec![0; 3],
            },
            RawPointerShape {
                kind: MONOCHROME,
                width: 9,
                height: 2,
                pitch: 1,
                buffer: vec![0; 2],
            },
        ] {
            assert_eq!(PointerShape::parse(bad.clone()), None, "{bad:?}");
        }
    }
}
