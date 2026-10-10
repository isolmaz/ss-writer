//! Writing modes: typewriter scrolling and zoom steps (spec §2.7).

use crate::geometry::Rect;

use super::config::{MAX_ZOOM_PERCENT, MIN_ZOOM_PERCENT, ZOOM_STEP_PERCENT};

/// The scroll offset that keeps the caret line at `position` (fraction of the viewport height)
/// in typewriter mode.
///
/// `caret` is the caret rectangle in content coordinates (y from the top of the content),
/// `scroll_y` the current scroll offset and `viewport_height` the visible height. Inside the
/// dead zone (`dead_zone` is a fraction of the viewport height on each side of `position`) the
/// scroll offset stays so clicks and small moves do not shift the view; outside it, or when
/// `force` is set (`Ctrl+L`), the caret line is moved exactly to `position`. The result may be
/// negative or beyond the content; the view clamps it.
#[must_use]
pub fn typewriter_scroll(
    caret: Rect,
    scroll_y: f32,
    viewport_height: f32,
    position: f32,
    dead_zone: f32,
    force: bool,
) -> f32 {
    let on_screen = caret.y + caret.height / 2.0 - scroll_y;
    let delta = on_screen - viewport_height * position;
    if !force && delta.abs() <= viewport_height * dead_zone {
        scroll_y
    } else {
        scroll_y + delta
    }
}

/// The scroll offset that makes the caret visible with the least movement, keeping `margin`
/// DIPs between the caret and the viewport edges.
#[must_use]
pub fn reveal_scroll(caret: Rect, scroll_y: f32, viewport_height: f32, margin: f32) -> f32 {
    let margin = margin.min(viewport_height / 2.0);
    if caret.y - margin < scroll_y {
        caret.y - margin
    } else if caret.bottom() + margin > scroll_y + viewport_height {
        caret.bottom() + margin - viewport_height
    } else {
        scroll_y
    }
}

/// The zoom after one step up (`direction > 0`) or down, clamped to the supported range.
#[must_use]
pub fn step_zoom(percent: u32, direction: i32) -> u32 {
    let stepped = if direction > 0 {
        percent.saturating_add(ZOOM_STEP_PERCENT)
    } else {
        percent.saturating_sub(ZOOM_STEP_PERCENT)
    };
    stepped.clamp(MIN_ZOOM_PERCENT, MAX_ZOOM_PERCENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typewriter_keeps_the_view_inside_the_dead_zone() {
        // Viewport 400 tall, caret line centred at 50 %, dead zone ±10 % = ±40.
        let caret = |y: f32| Rect::new(0.0, y, 0.0, 20.0);
        // Caret center at screen 190: 10 above the target line (200): inside.
        assert_eq!(
            typewriter_scroll(caret(520.0), 340.0, 400.0, 0.5, 0.1, false),
            340.0
        );
        // Center at screen 241: 41 below the target, outside the zone: scroll by the
        // difference so the caret center lands on 200.
        let scrolled = typewriter_scroll(caret(571.0), 340.0, 400.0, 0.5, 0.1, false);
        assert_eq!(scrolled, 340.0 + 41.0);
        // Exactly on the edge (40 below) counts as inside.
        assert_eq!(
            typewriter_scroll(caret(570.0), 340.0, 400.0, 0.5, 0.1, false),
            340.0
        );
        // Above the zone (center at 70) scrolls up.
        assert_eq!(
            typewriter_scroll(caret(400.0), 340.0, 400.0, 0.5, 0.1, false),
            340.0 - 130.0
        );
        // Ctrl+L recenters regardless.
        assert_eq!(
            typewriter_scroll(caret(520.0), 340.0, 400.0, 0.5, 0.1, true),
            340.0 - 10.0
        );
    }

    #[test]
    fn reveal_scrolls_minimally() {
        let caret = Rect::new(0.0, 100.0, 0.0, 20.0);
        assert_eq!(reveal_scroll(caret, 0.0, 400.0, 10.0), 0.0);
        assert_eq!(reveal_scroll(caret, 200.0, 400.0, 10.0), 90.0);
        assert_eq!(reveal_scroll(caret, 0.0, 100.0, 10.0), 30.0);
        assert_eq!(
            reveal_scroll(caret, 0.0, 100.0, 80.0),
            70.0,
            "margin is capped at half"
        );
    }

    #[test]
    fn zoom_steps_clamp() {
        assert_eq!(step_zoom(100, 1), 110);
        assert_eq!(step_zoom(100, -1), 90);
        assert_eq!(step_zoom(MAX_ZOOM_PERCENT, 1), MAX_ZOOM_PERCENT);
        assert_eq!(step_zoom(MIN_ZOOM_PERCENT, -1), MIN_ZOOM_PERCENT);
        assert_eq!(step_zoom(0, -1), MIN_ZOOM_PERCENT);
    }
}
