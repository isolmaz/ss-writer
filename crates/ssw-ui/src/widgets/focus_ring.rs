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
        let ops: Vec<&DrawOp> = canvas.ops().collect();
        assert_eq!(ops.len(), 2);
        match ops[0] {
            DrawOp::StrokeRoundedRect {
                rect,
                radii,
                color,
                width,
            } => {
                assert_eq!(*rect, Rect::new(9.0, 9.0, 82.0, 34.0));
                assert_eq!(*radii, CornerRadii::uniform(5.0));
                assert_eq!(*color, theme.tokens.focus_ring);
                assert_eq!(*width, 2.0);
            }
            other => panic!("unexpected op {other:?}"),
        }
        match ops[1] {
            DrawOp::StrokeRoundedRect {
                rect,
                radii,
                color,
                width,
            } => {
                assert_eq!(*rect, Rect::new(10.5, 10.5, 79.0, 31.0));
                assert_eq!(*radii, CornerRadii::uniform(3.5));
                assert_eq!(*color, theme.tokens.surface);
                assert_eq!(*width, 1.0);
            }
            other => panic!("unexpected op {other:?}"),
        }
    }

    #[test]
    fn kit_focus_ring_inner_radius_never_negative() {
        let theme = Theme::dark(DEFAULT_ACCENT);
        let mut canvas = RecordingCanvas::new();
        paint(&mut canvas, Rect::new(0.0, 0.0, 10.0, 10.0), 0.0, &theme);
        let inner = canvas.ops().nth(1).cloned();
        match inner {
            Some(DrawOp::StrokeRoundedRect { radii, .. }) => assert!(radii.is_zero()),
            other => panic!("unexpected op {other:?}"),
        }
    }
}
