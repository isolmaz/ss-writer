//! The custom caption buttons: layout, interaction state and painting.
//!
//! The window has no system caption; the app draws minimize / maximize-restore / close in the
//! top-right corner and answers `WM_NCHITTEST` for them (the maximize button reports
//! `HTMAXBUTTON`, which makes the Windows 11 Snap Layouts flyout appear).

use ssw_ui::canvas::{Canvas, IconFont};
use ssw_ui::geometry::{Color, Point, Rect};
use ssw_ui::theme::ThemeTokens;

/// Width of one caption button in DIPs.
pub const BUTTON_WIDTH: f32 = 46.0;
/// Height of the caption strip in DIPs.
pub const CAPTION_HEIGHT: f32 = 32.0;
/// Glyph size of the caption icons in DIPs.
pub const GLYPH_SIZE: f32 = 10.0;

/// Segoe Fluent Icons code points.
pub const GLYPH_MINIMIZE: char = '\u{E921}';
pub const GLYPH_MAXIMIZE: char = '\u{E922}';
pub const GLYPH_RESTORE: char = '\u{E923}';
pub const GLYPH_CLOSE: char = '\u{E8BB}';

/// The close button's hover fill (`#C42B1C`).
const CLOSE_HOVER: Color = Color::from_rgb_u32(0xC42B1C);

/// One of the three caption buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptionButton {
    Minimize,
    Maximize,
    Close,
}

impl CaptionButton {
    /// Left to right.
    pub const ALL: [CaptionButton; 3] = [
        CaptionButton::Minimize,
        CaptionButton::Maximize,
        CaptionButton::Close,
    ];
}

/// Interaction state of the caption buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeState {
    pub hover: Option<CaptionButton>,
    pub pressed: Option<CaptionButton>,
    /// The window is the active window; inactive windows draw dimmed glyphs.
    pub active: bool,
    pub maximized: bool,
}

impl Default for ChromeState {
    fn default() -> Self {
        Self {
            hover: None,
            pressed: None,
            active: true,
            maximized: false,
        }
    }
}

/// The rectangle of `button` in a window that is `width` DIPs wide.
#[must_use]
pub fn button_rect(button: CaptionButton, width: f32) -> Rect {
    let slot = match button {
        CaptionButton::Close => 1.0,
        CaptionButton::Maximize => 2.0,
        CaptionButton::Minimize => 3.0,
    };
    Rect::new(
        width - BUTTON_WIDTH * slot,
        0.0,
        BUTTON_WIDTH,
        CAPTION_HEIGHT,
    )
}

/// The button under `point` (client DIPs), if any.
#[must_use]
pub fn button_at(width: f32, point: Point) -> Option<CaptionButton> {
    CaptionButton::ALL
        .into_iter()
        .find(|&button| button_rect(button, width).contains(point))
}

/// The icon glyph of `button`.
#[must_use]
pub fn glyph(button: CaptionButton, maximized: bool) -> char {
    match (button, maximized) {
        (CaptionButton::Minimize, _) => GLYPH_MINIMIZE,
        (CaptionButton::Maximize, false) => GLYPH_MAXIMIZE,
        (CaptionButton::Maximize, true) => GLYPH_RESTORE,
        (CaptionButton::Close, _) => GLYPH_CLOSE,
    }
}

/// Fill (if any) and glyph color of one button.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ButtonPaint {
    pub background: Option<Color>,
    pub foreground: Color,
}

/// How `button` is painted in `state`.
///
/// Pressed beats hover; a pressed button only shows its pressed look while the pointer is
/// still over it (`hover == pressed`).
#[must_use]
pub fn button_paint(
    button: CaptionButton,
    state: ChromeState,
    tokens: &ThemeTokens,
) -> ButtonPaint {
    let resting = if state.active {
        tokens.text
    } else {
        tokens.text_disabled
    };
    let is_pressed = state.pressed == Some(button) && state.hover == Some(button);
    let is_hover =
        state.hover == Some(button) && state.pressed.is_none_or(|pressed| pressed == button);
    match (button, is_pressed, is_hover) {
        (CaptionButton::Close, true, _) => ButtonPaint {
            background: Some(CLOSE_HOVER.with_alpha(0.8)),
            foreground: Color::WHITE,
        },
        (CaptionButton::Close, false, true) => ButtonPaint {
            background: Some(CLOSE_HOVER),
            foreground: Color::WHITE,
        },
        (_, true, _) => ButtonPaint {
            background: Some(tokens.pressed_bg),
            foreground: tokens.hover_text,
        },
        (_, false, true) => ButtonPaint {
            background: Some(tokens.hover_bg),
            foreground: tokens.hover_text,
        },
        _ => ButtonPaint {
            background: None,
            foreground: resting,
        },
    }
}

/// Paints the three buttons into a window `width` DIPs wide.
pub fn paint_caption_buttons(
    canvas: &mut dyn Canvas,
    width: f32,
    state: ChromeState,
    tokens: &ThemeTokens,
) {
    for button in CaptionButton::ALL {
        let rect = button_rect(button, width);
        let paint = button_paint(button, state, tokens);
        if let Some(background) = paint.background {
            canvas.fill_rect(rect, background);
        }
        canvas.draw_icon(
            glyph(button, state.maximized),
            IconFont::SegoeFluent,
            GLYPH_SIZE,
            paint.foreground,
            rect.center(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssw_ui::canvas::{DrawOp, RecordingCanvas};
    use ssw_ui::theme::Theme;

    fn tokens() -> ThemeTokens {
        Theme::light(ssw_ui::theme::DEFAULT_ACCENT).tokens
    }

    #[test]
    fn buttons_sit_in_the_top_right_corner() {
        assert_eq!(
            button_rect(CaptionButton::Close, 800.0),
            Rect::new(754.0, 0.0, 46.0, 32.0)
        );
        assert_eq!(button_rect(CaptionButton::Maximize, 800.0).x, 708.0);
        assert_eq!(button_rect(CaptionButton::Minimize, 800.0).x, 662.0);
    }

    #[test]
    fn hit_testing_picks_the_button_under_the_point() {
        let cases = [
            (Point::new(790.0, 10.0), Some(CaptionButton::Close)),
            (Point::new(754.0, 0.0), Some(CaptionButton::Close)),
            (Point::new(730.0, 31.9), Some(CaptionButton::Maximize)),
            (Point::new(662.0, 5.0), Some(CaptionButton::Minimize)),
            (Point::new(661.9, 5.0), None),
            (Point::new(790.0, 32.0), None),
            (Point::new(10.0, 10.0), None),
        ];
        for (point, expected) in cases {
            assert_eq!(button_at(800.0, point), expected, "{point:?}");
        }
    }

    #[test]
    fn glyphs_follow_the_maximized_state() {
        assert_eq!(glyph(CaptionButton::Minimize, true), '\u{E921}');
        assert_eq!(glyph(CaptionButton::Maximize, false), '\u{E922}');
        assert_eq!(glyph(CaptionButton::Maximize, true), '\u{E923}');
        assert_eq!(glyph(CaptionButton::Close, false), '\u{E8BB}');
    }

    #[test]
    fn paint_depends_on_hover_press_and_activation() {
        let tokens = tokens();
        let idle = ChromeState::default();
        let min_idle = button_paint(CaptionButton::Minimize, idle, &tokens);
        assert_eq!(
            (min_idle.background, min_idle.foreground),
            (None, tokens.text)
        );

        let inactive = ChromeState {
            active: false,
            ..idle
        };
        assert_eq!(
            button_paint(CaptionButton::Minimize, inactive, &tokens).foreground,
            tokens.text_disabled
        );

        let hover = ChromeState {
            hover: Some(CaptionButton::Maximize),
            ..idle
        };
        let max_hover = button_paint(CaptionButton::Maximize, hover, &tokens);
        assert_eq!(max_hover.background, Some(tokens.hover_bg));
        assert_eq!(max_hover.foreground, tokens.hover_text);
        assert_eq!(
            button_paint(CaptionButton::Minimize, hover, &tokens).background,
            None
        );

        let pressed = ChromeState {
            hover: Some(CaptionButton::Maximize),
            pressed: Some(CaptionButton::Maximize),
            ..idle
        };
        assert_eq!(
            button_paint(CaptionButton::Maximize, pressed, &tokens).background,
            Some(tokens.pressed_bg)
        );
        // Pressed, but the pointer moved off the button: no pressed look anywhere.
        let dragged_off = ChromeState {
            hover: Some(CaptionButton::Close),
            pressed: Some(CaptionButton::Maximize),
            ..idle
        };
        assert_eq!(
            button_paint(CaptionButton::Maximize, dragged_off, &tokens).background,
            None
        );
        assert_eq!(
            button_paint(CaptionButton::Close, dragged_off, &tokens).background,
            None
        );

        let close_hover = ChromeState {
            hover: Some(CaptionButton::Close),
            ..idle
        };
        let close = button_paint(CaptionButton::Close, close_hover, &tokens);
        assert_eq!(close.background, Some(Color::from_rgb_u32(0xC42B1C)));
        assert_eq!(close.foreground, Color::WHITE);
        let close_pressed = ChromeState {
            pressed: Some(CaptionButton::Close),
            ..close_hover
        };
        let close = button_paint(CaptionButton::Close, close_pressed, &tokens);
        assert_eq!(
            close.background,
            Some(Color::from_rgb_u32(0xC42B1C).with_alpha(0.8))
        );
    }

    #[test]
    fn painting_fills_hovered_buttons_and_draws_every_glyph() {
        let tokens = tokens();
        let mut canvas = RecordingCanvas::new();
        let state = ChromeState {
            hover: Some(CaptionButton::Close),
            maximized: true,
            ..ChromeState::default()
        };
        paint_caption_buttons(&mut canvas, 400.0, state, &tokens);
        let fills: Vec<_> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::FillRect { rect, color } => Some((*rect, *color)),
                _ => None,
            })
            .collect();
        assert_eq!(
            fills,
            vec![(
                button_rect(CaptionButton::Close, 400.0),
                Color::from_rgb_u32(0xC42B1C)
            )]
        );
        let glyphs: Vec<char> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::Icon {
                    glyph, size, font, ..
                } => {
                    assert_eq!((*size, *font), (GLYPH_SIZE, IconFont::SegoeFluent));
                    Some(*glyph)
                }
                _ => None,
            })
            .collect();
        assert_eq!(glyphs, vec!['\u{E921}', '\u{E923}', '\u{E8BB}']);
        let centers: Vec<Point> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::Icon { center, .. } => Some(*center),
                _ => None,
            })
            .collect();
        assert_eq!(centers[2], Point::new(377.0, 16.0));
    }
}
