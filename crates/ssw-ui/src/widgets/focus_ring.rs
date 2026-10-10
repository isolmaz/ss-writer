//! The keyboard focus ring: a 2 px ring in the focus color outside the control and a 1 px inner
//! stroke in the surface color so the ring stays visible on any fill (Windows 11 focus visual).

use crate::canvas::Canvas;
use crate::geometry::{CornerRadii, Insets, Rect};
use crate::theme::Theme;

/// Width of the outer focus ring.
pub const RING_WIDTH: f32 = 2.0;
/// Width of the inner contrast stroke.
pub const INNER_WIDTH: f32 = 1.0;

/// Draws the focus ring around `bounds`, whose corners have `radius`. The ring sits outside the
/// bounds (it occupies the two pixels around them) and the contrast stroke inside.
pub fn paint(canvas: &mut dyn Canvas, bounds: Rect, radius: f32, theme: &Theme) {
    let tokens = &theme.tokens;
    let outer = bounds.outset(Insets::uniform(RING_WIDTH / 2.0));
    canvas.stroke_rounded_rect(
        outer,
        CornerRadii::uniform(radius + RING_WIDTH / 2.0),
        tokens.focus_ring,
        RING_WIDTH,
    );
    let inner = bounds.inset(Insets::uniform(INNER_WIDTH / 2.0));
    canvas.stroke_rounded_rect(
        inner,
        CornerRadii::uniform((radius - INNER_WIDTH / 2.0).max(0.0)),
        tokens.surface,
        INNER_WIDTH,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{DrawOp, RecordingCanvas};
    use crate::theme::DEFAULT_ACCENT;

    #[test]
    fn kit_focus_ring_draws_ring_outside_and_contrast_inside() {
        let theme = Theme::light(DEFAULT_ACCENT);
        let bounds = Rect::new(10.0, 10.0, 80.0, 32.0);
        let mut canvas = RecordingCanvas::new();
        paint(&mut canvas, bounds, 4.0, &theme);
        let ops: Vec<DrawOp> = canvas.ops().cloned().collect();
        assert_eq!(
            ops,
            [
                DrawOp::StrokeRoundedRect {
                    rect: Rect::new(9.0, 9.0, 82.0, 34.0),
                    radii: CornerRadii::uniform(5.0),
                    color: theme.tokens.focus_ring,
                    width: 2.0,
                },
                DrawOp::StrokeRoundedRect {
                    rect: Rect::new(10.5, 10.5, 79.0, 31.0),
                    radii: CornerRadii::uniform(3.5),
                    color: theme.tokens.surface,
                    width: 1.0,
                },
            ]
        );
    }

    #[test]
    fn kit_focus_ring_inner_radius_never_negative() {
        let theme = Theme::dark(DEFAULT_ACCENT);
        let mut canvas = RecordingCanvas::new();
        paint(&mut canvas, Rect::new(0.0, 0.0, 10.0, 10.0), 0.0, &theme);
        assert_eq!(
            canvas.ops().nth(1),
            Some(&DrawOp::StrokeRoundedRect {
                rect: Rect::new(0.5, 0.5, 9.0, 9.0),
                radii: CornerRadii::uniform(0.0),
                color: theme.tokens.surface,
                width: 1.0,
            })
        );
    }
}
