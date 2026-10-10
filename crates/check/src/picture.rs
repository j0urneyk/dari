use std::time::{Duration, Instant};

const COLUMNS: usize = 32;
const ROWS: usize = 18;
const SAMPLES: usize = 8;
const CELL_DELTA: f32 = 12.0;

/// Share of cells that must change for the picture to differ clearly from another: a UAC prompt
/// dims the whole desktop, and the lock screen replaces it.
pub(crate) const CLEARLY_DIFFERENT: f64 = 0.3;
/// Share of cells that may still differ from the baseline once the screen is back, for a clock or
/// a closing window.
pub(crate) const NEAR: f64 = 0.1;
const STILL: f64 = 0.02;

/// A frame reduced to the mean brightness of each cell of a grid.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Thumbnail {
    width: u32,
    height: u32,
    cells: Vec<f32>,
}

impl Thumbnail {
    /// Reduces a tightly packed BGRA frame.
    pub(crate) fn of(width: u32, height: u32, bgra: &[u8]) -> Self {
        let mut cells = vec![0.0; COLUMNS * ROWS];
        let (w, h) = (width as usize, height as usize);
        if w > 0 && h > 0 && bgra.len() >= w * h * 4 {
            for row in 0..ROWS * SAMPLES {
                let y = (2 * row + 1) * h / (2 * ROWS * SAMPLES);
                for column in 0..COLUMNS * SAMPLES {
                    let x = (2 * column + 1) * w / (2 * COLUMNS * SAMPLES);
                    let pixel = &bgra[(y * w + x) * 4..][..3];
                    cells[row / SAMPLES * COLUMNS + column / SAMPLES] += 0.114
                        * f32::from(pixel[0])
                        + 0.587 * f32::from(pixel[1])
                        + 0.299 * f32::from(pixel[2]);
                }
            }
        }
        #[expect(clippy::cast_precision_loss, reason = "SAMPLES is small")]
        let per_cell = (SAMPLES * SAMPLES) as f32;
        for cell in &mut cells {
            *cell /= per_cell;
        }
        Self {
            width,
            height,
            cells,
        }
    }

    /// Share of cells, 0..=1, that differ between the two; 1 when their sizes differ.
    #[expect(clippy::cast_precision_loss, reason = "cell counts are small")]
    pub(crate) fn difference(&self, other: &Self) -> f64 {
        if (self.width, self.height) != (other.width, other.height) {
            return 1.0;
        }
        let changed = self
            .cells
            .iter()
            .zip(&other.cells)
            .filter(|(a, b)| (*a - *b).abs() > CELL_DELTA)
            .count();
        changed as f64 / self.cells.len() as f64
    }
}

/// Decides when the picture has stopped changing. A host sends a still screen again only when
/// asked, so the caller feeds it frames at least every second or so.
#[derive(Debug)]
pub(crate) struct Settle {
    quiet: Duration,
    last_change: Option<(Thumbnail, Instant)>,
}

impl Settle {
    pub(crate) fn new(quiet: Duration) -> Self {
        Self {
            quiet,
            last_change: None,
        }
    }

    /// Takes a frame seen at `now`; returns whether the picture has been still for `quiet`.
    pub(crate) fn feed(&mut self, picture: &Thumbnail, now: Instant) -> bool {
        match &self.last_change {
            Some((reference, since)) if reference.difference(picture) <= STILL => {
                now.duration_since(*since) >= self.quiet
            }
            _ => {
                self.last_change = Some((picture.clone(), now));
                self.quiet.is_zero()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 360;
    const PROMPT: (f64, f64, f64, f64) = (0.35, 0.3, 0.65, 0.7);

    type Patch = ((f64, f64, f64, f64), u8);

    fn frame(level: u8, patches: &[Patch]) -> Thumbnail {
        let mut bgra = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let (fx, fy) = (
                    f64::from(x) / f64::from(WIDTH),
                    f64::from(y) / f64::from(HEIGHT),
                );
                let value = patches
                    .iter()
                    .rev()
                    .find(|((left, top, right, bottom), _)| {
                        (*left..*right).contains(&fx) && (*top..*bottom).contains(&fy)
                    })
                    .map_or(level, |(_, value)| *value);
                bgra.extend([value, value, value, 255]);
            }
        }
        Thumbnail::of(WIDTH, HEIGHT, &bgra)
    }

    #[test]
    fn a_dimmed_desktop_differs_clearly_and_a_caret_or_noise_does_not_change_it() {
        let desktop = frame(120, &[]);
        assert!(desktop.difference(&frame(60, &[])) >= CLEARLY_DIFFERENT);

        let caret = frame(120, &[((0.5, 0.5, 0.505, 0.53), 0)]);
        assert!(desktop.difference(&caret) <= STILL);
        assert!(desktop.difference(&frame(126, &[])) <= STILL);
    }

    #[test]
    fn a_leftover_window_is_near_the_baseline_but_not_still() {
        let desktop = frame(120, &[]);
        let window = frame(120, &[((0.0, 0.0, 0.25, 0.3), 250)]);
        let difference = desktop.difference(&window);
        assert!(difference <= NEAR, "{difference}");
        assert!(difference > STILL, "{difference}");
        let lock_screen = frame(30, &[((0.4, 0.4, 0.6, 0.5), 200)]);
        assert!(desktop.difference(&lock_screen) > NEAR);
    }

    #[test]
    fn a_resized_screen_differs_entirely() {
        let small = Thumbnail::of(2, 2, &[100; 16]);
        assert!((frame(100, &[]).difference(&small) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn settles_only_once_the_last_visible_change_has_been_still_for_the_quiet_time() {
        let start = Instant::now();
        let at = |seconds: f64| start + Duration::from_secs_f64(seconds);
        let dimmed = frame(60, &[]);
        let prompt = frame(60, &[(PROMPT, 230)]);
        let mut settle = Settle::new(Duration::from_secs(2));

        assert!(!settle.feed(&dimmed, at(0.0)));
        assert!(!settle.feed(&dimmed, at(1.5)));
        assert!(
            !settle.feed(&prompt, at(1.9)),
            "the prompt appearing restarts the wait"
        );
        assert!(!settle.feed(&prompt, at(3.0)));
        assert!(settle.feed(&prompt, at(3.9)));
    }

    #[test]
    fn a_blinking_caret_does_not_restart_the_wait() {
        let start = Instant::now();
        let prompt = frame(60, &[(PROMPT, 230)]);
        let blinked = frame(60, &[(PROMPT, 230), ((0.45, 0.5, 0.452, 0.53), 0)]);
        let mut settle = Settle::new(Duration::from_secs(1));
        assert!(!settle.feed(&prompt, start));
        assert!(settle.feed(&blinked, start + Duration::from_secs(1)));
    }
}
