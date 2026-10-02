//! Fitting the remote screen into the viewer window and mapping the pointer back.

use gpui_kit::{Bounds, Pixels, Point, Size, point, px, size};
use open_desk_proto::{MAX_SCROLL_LINES, PointerPosition};

/// The largest rectangle with the frame's aspect ratio that fits `area`, centered.
pub(crate) fn letterbox(area: Bounds<Pixels>, frame: Size<u32>) -> Bounds<Pixels> {
    if frame.width == 0 || frame.height == 0 {
        return area;
    }
    let area_width = f32::from(area.size.width);
    let area_height = f32::from(area.size.height);
    #[expect(
        clippy::cast_precision_loss,
        reason = "frame dimensions are at most 8192"
    )]
    let (frame_width, frame_height) = (frame.width as f32, frame.height as f32);
    let scale = (area_width / frame_width).min(area_height / frame_height);
    let fitted = size(px(frame_width * scale), px(frame_height * scale));
    let origin = point(
        area.origin.x + (area.size.width - fitted.width) / 2.,
        area.origin.y + (area.size.height - fitted.height) / 2.,
    );
    Bounds {
        origin,
        size: fitted,
    }
}

/// The normalized remote position under `position`, or `None` outside the picture.
pub(crate) fn pointer_position(
    picture: Bounds<Pixels>,
    position: Point<Pixels>,
) -> Option<PointerPosition> {
    if !picture.contains(&position) || picture.size.width <= px(1.) || picture.size.height <= px(1.)
    {
        return None;
    }
    let normalize = |offset: Pixels, length: Pixels| {
        let ratio = (f32::from(offset) / (f32::from(length) - 1.)).clamp(0., 1.);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped"
        )]
        let value = (ratio * f32::from(u16::MAX)).round() as u16;
        value
    };
    Some(PointerPosition {
        x: normalize(position.x - picture.origin.x, picture.size.width),
        y: normalize(position.y - picture.origin.y, picture.size.height),
    })
}

/// Turns pixel-precise trackpad scrolling into whole wheel lines, carrying the remainder.
#[derive(Debug, Default)]
pub(crate) struct ScrollAccumulator {
    remainder: Point<f32>,
}

impl ScrollAccumulator {
    /// Pixels per wheel line on the host.
    const LINE: f32 = 40.;

    /// Adds a delta in pixels (positive = content moves down/right, GPUI's convention) and
    /// returns whole lines to send (positive = scroll down/right, the protocol's convention).
    pub(crate) fn add(&mut self, delta: Point<Pixels>) -> Option<(i16, i16)> {
        self.remainder.x -= f32::from(delta.x) / Self::LINE;
        self.remainder.y -= f32::from(delta.y) / Self::LINE;
        let limit = f32::from(MAX_SCROLL_LINES);
        let take = |value: &mut f32| {
            let lines = value.trunc().clamp(-limit, limit);
            // Drop anything beyond the per-event limit instead of replaying it later.
            *value = value.fract();
            #[expect(
                clippy::cast_possible_truncation,
                reason = "clamped to MAX_SCROLL_LINES"
            )]
            let lines = lines as i16;
            lines
        };
        let (dx, dy) = (take(&mut self.remainder.x), take(&mut self.remainder.y));
        (dx != 0 || dy != 0).then_some((dx, dy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: Pixels, expected: f32) {
        assert!(
            (f32::from(actual) - expected).abs() < 0.01,
            "{actual:?} != {expected}"
        );
    }

    fn area(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.), px(40.)),
            size: size(px(width), px(height)),
        }
    }

    #[test]
    fn wide_frames_get_bars_above_and_below() {
        let picture = letterbox(area(1000., 1000.), size(1920, 1080));
        assert_close(picture.size.width, 1000.);
        assert_close(picture.size.height, 562.5);
        assert_close(picture.origin.y, 40. + 218.75);
    }

    #[test]
    fn tall_frames_get_bars_on_the_sides() {
        let picture = letterbox(area(1000., 500.), size(1080, 1920));
        assert_close(picture.size.height, 500.);
        assert!(picture.origin.x > px(0.));
    }

    #[test]
    fn pointer_maps_corners_and_rejects_bars() {
        let picture = letterbox(area(1000., 1000.), size(1920, 1080));
        let top_left = pointer_position(picture, picture.origin).unwrap();
        assert_eq!((top_left.x, top_left.y), (0, 0));
        let bottom_right = point(
            picture.origin.x + picture.size.width - px(0.01),
            picture.origin.y + picture.size.height - px(0.01),
        );
        let corner = pointer_position(picture, bottom_right).unwrap();
        assert!(corner.x > 65_000 && corner.y > 65_000);
        assert!(pointer_position(picture, point(px(500.), px(45.))).is_none());
    }

    #[test]
    fn trackpad_scrolling_accumulates_into_lines() {
        let mut scroll = ScrollAccumulator::default();
        assert_eq!(scroll.add(point(px(0.), px(-15.))), None);
        assert_eq!(scroll.add(point(px(0.), px(-30.))), Some((0, 1)));
        assert_eq!(scroll.add(point(px(80.), px(0.))), Some((-2, 0)));
        assert_eq!(
            scroll.add(point(px(0.), px(-1.0e6))),
            Some((0, MAX_SCROLL_LINES))
        );
        assert_eq!(
            scroll.add(point(px(0.), px(-1.))),
            None,
            "the excess is not replayed"
        );
    }
}
