//! What the preview looks like: the colors and font Explorer asks for
//! (`IPreviewHandlerVisuals`) laid over the system theme.

use std::sync::Arc;

use ssw_ui::editor::{DocumentKind, EditorConfig};
use ssw_ui::geometry::Color;
use ssw_ui::theme::Theme;
use ssw_win::com::from_wide;
use ssw_win::theme::{SystemTheme, color_from_colorref};
use windows::Win32::Graphics::Gdi::LOGFONTW;

/// A color is "dark" below this relative luminance.
const DARK_LUMINANCE: f32 = 0.5;

/// What the host asked for through `IPreviewHandlerVisuals`; every part is optional because
/// Explorer may call any subset of the methods, in any order, before or after `DoPreview`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Visuals {
    /// `SetBackgroundColor`: the preview pane's background.
    pub background: Option<Color>,
    /// `SetTextColor`: the pane's text color.
    pub text: Option<Color>,
    /// `SetFont`: the face name of the pane's font.
    pub font_family: Option<String>,
}

impl Visuals {
    /// Records the background color (a `COLORREF`, `0x00BBGGRR`).
    pub fn set_background(&mut self, colorref: u32) {
        self.background = Some(color_from_colorref(colorref));
    }

    /// Records the text color (a `COLORREF`, `0x00BBGGRR`).
    pub fn set_text(&mut self, colorref: u32) {
        self.text = Some(color_from_colorref(colorref));
    }

    /// Records the face name of `font`. A font without a face name changes nothing.
    pub fn set_font(&mut self, font: &LOGFONTW) {
        let family = from_wide(&font.lfFaceName);
        if !family.is_empty() {
            self.font_family = Some(family);
        }
    }

    /// The theme of the preview: the system theme (light, dark or high contrast, accent), but
    /// light or dark by the host's background when it gave one, with the host's background and
    /// text colors in place of the theme's own. High contrast always keeps the system palette.
    #[must_use]
    pub fn theme(&self, system: &SystemTheme) -> Theme {
        if system.high_contrast {
            return system.theme;
        }
        let dark = self.background.map_or(system.dark, |color| {
            color.relative_luminance() < DARK_LUMINANCE
        });
        let mut theme = if dark {
            Theme::dark(system.accent)
        } else {
            Theme::light(system.accent)
        };
        if let Some(background) = self.background {
            theme.tokens.surface = background;
            theme.tokens.window_bg = background;
        }
        if let Some(text) = self.text {
            theme.tokens.text = text;
            theme.tokens.selection_text = text;
            theme.tokens.hover_text = text;
        }
        theme
    }

    /// The editor configuration of the preview of a `document_kind` document: the defaults with
    /// the host's font family as the body font.
    #[must_use]
    pub fn editor_config(&self, document_kind: DocumentKind) -> EditorConfig {
        let mut config = EditorConfig {
            document_kind,
            ..EditorConfig::default()
        };
        if let Some(family) = &self.font_family {
            config.text_font = Arc::from(family.as_str());
        }
        config
    }
}
