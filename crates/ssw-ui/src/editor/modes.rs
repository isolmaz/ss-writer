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

/// Width of the band inside each viewport edge where a drag already scrolls, in DIPs.
pub const AUTOSCROLL_ZONE: f32 = 12.0;
/// Auto-scroll speed in DIPs per second for each DIP the pointer is past the inner edge of the
/// zone.
pub const AUTOSCROLL_GAIN: f32 = 20.0;
/// Fastest auto-scroll, in DIPs per second.
pub const AUTOSCROLL_MAX_SPEED: f32 = 4_000.0;

/// How fast, and in which direction, the view scrolls while a drag holds the pointer at window
/// height `pointer_y` over a viewport spanning `top..bottom`: negative scrolls up, positive
/// down, in DIPs per second; `0.0` in the middle of the viewport.
///
/// The speed grows linearly with the distance beyond the inner edge of the [`AUTOSCROLL_ZONE`]
/// band (so it is slow at the edge itself and fast far outside the window) and is capped at
/// [`AUTOSCROLL_MAX_SPEED`]. The band shrinks to a quarter of the viewport height for tiny
/// viewports.
#[must_use]
pub fn autoscroll_speed(pointer_y: f32, top: f32, bottom: f32) -> f32 {
    let zone = AUTOSCROLL_ZONE.min((bottom - top) / 4.0);
    let above = top + zone - pointer_y;
    let below = pointer_y - (bottom - zone);
    if above > 0.0 {
        -(above * AUTOSCROLL_GAIN).min(AUTOSCROLL_MAX_SPEED)
    } else if below > 0.0 {
        (below * AUTOSCROLL_GAIN).min(AUTOSCROLL_MAX_SPEED)
    } else {
        0.0
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
    fn autoscroll_speed_grows_with_the_distance_beyond_the_zone() {
        // Viewport 100..500, zone 12: scrolling starts at 112 (up) and 488 (down).
        assert_eq!(autoscroll_speed(300.0, 100.0, 500.0), 0.0);
        assert_eq!(autoscroll_speed(112.0, 100.0, 500.0), 0.0);
        assert_eq!(autoscroll_speed(488.0, 100.0, 500.0), 0.0);
        // At the very edge: 12 DIPs into the zone × 20.
        assert_eq!(autoscroll_speed(100.0, 100.0, 500.0), -240.0);
        assert_eq!(autoscroll_speed(500.0, 100.0, 500.0), 240.0);
        // 50 DIPs beyond the edge: 62 × 20.
        assert_eq!(autoscroll_speed(50.0, 100.0, 500.0), -1_240.0);
        assert_eq!(autoscroll_speed(550.0, 100.0, 500.0), 1_240.0);
        // Capped.
        assert_eq!(
            autoscroll_speed(-10_000.0, 100.0, 500.0),
            -AUTOSCROLL_MAX_SPEED
        );
        assert_eq!(
            autoscroll_speed(10_000.0, 100.0, 500.0),
            AUTOSCROLL_MAX_SPEED
        );
        // A tiny viewport shrinks the zone to a quarter of its height (4 DIPs of 16).
        assert_eq!(autoscroll_speed(8.0, 0.0, 16.0), 0.0);
        assert_eq!(autoscroll_speed(0.0, 0.0, 16.0), -80.0);
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
