//! Test pictures: a capturer that renders a moving pattern, for tests and machines without a
//! display, and a page of text-like strokes for measuring what the codec does to text.

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

/// A deterministic page of small "text": lines of glyph-like 1 px strokes, black on white,
/// with the reddish and bluish fringes subpixel antialiasing leaves on stroke edges, under a
/// light title bar. Nothing in the repository renders fonts, and the codec only cares about the
/// thin high-contrast edges, which this has at the density of a 9 px font.
pub fn render_text_page(width: u32, height: u32) -> RgbaFrame {
    const CELL_WIDTH: u32 = 7;
    const LINE_HEIGHT: u32 = 13;
    const GLYPH_HEIGHT: u32 = 8;
    const MARGIN: u32 = 24;
    const TITLE_BAR: u32 = 40;
    let (width, height) = (width.max(2) & !1, height.max(2) & !1);
    let mut page = vec![255u8; (width * height * 4) as usize];
    let mut set = |x: u32, y: u32, [r, g, b]: [u8; 3]| {
        if x < width && y < height {
            let at = ((y * width + x) * 4) as usize;
            page[at..at + 3].copy_from_slice(&[r, g, b]);
        }
    };
    for y in 0..TITLE_BAR.min(height) {
        for x in 0..width {
            set(x, y, [236, 236, 238]);
        }
    }
    let mut random = Lcg(0x9E37_79B9);
    let mut line_top = TITLE_BAR + 8;
    let mut line_index = 0u32;
    while line_top + LINE_HEIGHT <= height {
        // Paragraph breaks and ragged right edges, like prose.
        let paragraph_break = line_index % 9 == 8;
        let line_width = if line_index % 9 == 7 {
            width / 2 + random.below(width / 3)
        } else {
            width - MARGIN
        };
        if !paragraph_break {
            let mut x = MARGIN + 4 * u32::from(line_index.is_multiple_of(7)) * CELL_WIDTH;
            let mut word_left = random.below(7) + 2;
            while x + CELL_WIDTH <= line_width {
                if word_left == 0 {
                    word_left = random.below(7) + 2;
                    x += CELL_WIDTH;
                    continue;
                }
                word_left -= 1;
                draw_glyph(&mut set, &mut random, x, line_top, GLYPH_HEIGHT);
                x += CELL_WIDTH;
            }
        }
        line_top += LINE_HEIGHT;
        line_index += 1;
    }
    RgbaFrame::new(width, height, page)
        .unwrap_or_else(|| unreachable!("the page fills exactly width * height pixels"))
}

/// Two to four 1 px strokes in a 5×8 box: verticals, horizontals, and a diagonal, like the
/// stems and bars of Latin letters.
fn draw_glyph(
    set: &mut impl FnMut(u32, u32, [u8; 3]),
    random: &mut Lcg,
    left: u32,
    top: u32,
    glyph_height: u32,
) {
    const INK: [u8; 3] = [16, 16, 16];
    const LEFT_FRINGE: [u8; 3] = [255, 196, 176];
    const RIGHT_FRINGE: [u8; 3] = [176, 196, 255];
    let strokes = random.below(3) + 2;
    for _ in 0..strokes {
        match random.below(4) {
            0 | 1 => {
                let x = left + [0, 2, 4][random.below(3) as usize];
                let (from, to) = if random.below(3) == 0 {
                    (glyph_height / 2, glyph_height)
                } else {
                    (0, glyph_height)
                };
                for y in from..to {
                    set(x.wrapping_sub(1), top + y, LEFT_FRINGE);
                    set(x, top + y, INK);
                    set(x + 1, top + y, RIGHT_FRINGE);
                }
            }
            2 => {
                let y = top + [0, glyph_height / 2, glyph_height - 1][random.below(3) as usize];
                for x in left..left + 5 {
                    set(x, y, INK);
                }
            }
            _ => {
                for step in 0..glyph_height.min(5) {
                    set(left + step, top + glyph_height - 1 - step, INK);
                }
            }
        }
    }
}

/// A tiny deterministic generator, so every run draws the same page.
struct Lcg(u32);

impl Lcg {
    fn below(&mut self, bound: u32) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) % bound.max(1)
    }
}
