//! Theme tokens (colors and metrics) for light, dark and high-contrast modes.
//!
//! Every color the UI paints comes from [`ThemeTokens`]; no widget hard-codes a color.
//! `Theme::light` / `Theme::dark` derive accent-dependent tokens from the accent so text
//! stays at WCAG AA contrast (4.5:1) for any accent. `Theme::high_contrast` takes every color
//! from the system palette (`GetSysColor`).

use crate::geometry::Color;

/// The default accent `#1874D3` (spec §1).
pub const DEFAULT_ACCENT: Color = Color::from_rgb_u32(0x1874D3);

/// Minimum WCAG contrast for body text.
const TEXT_CONTRAST: f32 = 4.5;
/// Minimum WCAG contrast for non-text UI components (focus rings).
const COMPONENT_CONTRAST: f32 = 3.0;
/// Resolution of the search in [`ensure_contrast`].
const CONTRAST_STEPS: u8 = 20;

const fn rgb(value: u32) -> Color {
    Color::from_rgb_u32(value)
}

/// The mode a [`Theme`] was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeMode {
    Light,
    Dark,
    HighContrast,
}

/// The six highlight colors of `=={color}x==`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightColor {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl HighlightColor {
    pub const ALL: [HighlightColor; 6] = [
        HighlightColor::Red,
        HighlightColor::Orange,
        HighlightColor::Yellow,
        HighlightColor::Green,
        HighlightColor::Blue,
        HighlightColor::Purple,
    ];
}

/// Pill background and the text color that stays readable on it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HighlightStyle {
    pub background: Color,
    pub text: Color,
}

/// One [`HighlightStyle`] per [`HighlightColor`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HighlightPalette {
    pub red: HighlightStyle,
    pub orange: HighlightStyle,
    pub yellow: HighlightStyle,
    pub green: HighlightStyle,
    pub blue: HighlightStyle,
    pub purple: HighlightStyle,
}

impl HighlightPalette {
    #[must_use]
    pub fn get(&self, color: HighlightColor) -> HighlightStyle {
        match color {
            HighlightColor::Red => self.red,
            HighlightColor::Orange => self.orange,
            HighlightColor::Yellow => self.yellow,
            HighlightColor::Green => self.green,
            HighlightColor::Blue => self.blue,
            HighlightColor::Purple => self.purple,
        }
    }

    /// The same style for every color (high contrast).
    fn uniform(style: HighlightStyle) -> Self {
        Self {
            red: style,
            orange: style,
            yellow: style,
            green: style,
            blue: style,
            purple: style,
        }
    }
}

/// The callout kinds of `> [!KIND]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CalloutKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl CalloutKind {
    pub const ALL: [CalloutKind; 5] = [
        CalloutKind::Note,
        CalloutKind::Tip,
        CalloutKind::Important,
        CalloutKind::Warning,
        CalloutKind::Caution,
    ];
}

/// Colors of one callout kind: left bar, tinted background, icon (and title).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalloutStyle {
    pub bar: Color,
    pub background: Color,
    pub icon: Color,
}

/// One [`CalloutStyle`] per [`CalloutKind`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalloutPalette {
    pub note: CalloutStyle,
    pub tip: CalloutStyle,
    pub important: CalloutStyle,
    pub warning: CalloutStyle,
    pub caution: CalloutStyle,
}

impl CalloutPalette {
    #[must_use]
    pub fn get(&self, kind: CalloutKind) -> CalloutStyle {
        match kind {
            CalloutKind::Note => self.note,
            CalloutKind::Tip => self.tip,
            CalloutKind::Important => self.important,
            CalloutKind::Warning => self.warning,
            CalloutKind::Caution => self.caution,
        }
    }

    /// The same style for every kind (high contrast).
    fn uniform(style: CalloutStyle) -> Self {
        Self {
            note: style,
            tip: style,
            important: style,
            warning: style,
            caution: style,
        }
    }
}

/// Non-color design tokens (spec §1, §3). Identical in every mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeMetrics {
    /// Corner radius of controls (buttons, inputs, pills), in DIPs.
    pub control_radius: f32,
    /// Corner radius of overlays (menus, flyouts, cards, find bar), in DIPs.
    pub overlay_radius: f32,
    /// Width of the centered editor text column, in DIPs.
    pub editor_column_width: f32,
    /// Base editor font size, in DIPs.
    pub base_font_size: f32,
    /// Editor line height as a multiple of the font size.
    pub line_height: f32,
    /// UI font family.
    pub ui_font: &'static str,
    /// Editor text font family.
    pub text_font: &'static str,
    /// Code font family (bundled).
    pub mono_font: &'static str,
    /// Icon font family.
    pub icon_font: &'static str,
}

impl Default for ThemeMetrics {
    fn default() -> Self {
        Self {
            control_radius: 4.0,
            overlay_radius: 8.0,
            editor_column_width: 680.0,
            base_font_size: 16.0,
            line_height: 1.75,
            ui_font: "Segoe UI Variable",
            text_font: "Segoe UI Variable Text",
            mono_font: "Cascadia Mono",
            icon_font: "Segoe Fluent Icons",
        }
    }
}

/// Every design token the UI paints with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeTokens {
    /// Window/title-row fill when Mica is unavailable.
    pub window_bg: Color,
    /// Editor surface (the solid layer).
    pub surface: Color,
    /// Sidebar and inspector fill when Mica is unavailable.
    pub sidebar_bg: Color,
    /// Body text.
    pub text: Color,
    /// Secondary text (checked tasks, hints, status bar).
    pub text_muted: Color,
    /// Disabled text; exempt from the contrast requirement.
    pub text_disabled: Color,
    /// Dimmed Markdown syntax markers revealed at the caret.
    pub marker: Color,
    /// Accent fill (list dots, primary buttons, toggles, selection chrome).
    pub accent: Color,
    /// Text/glyph color that reads on [`ThemeTokens::accent`].
    pub accent_text_on: Color,
    /// Active text selection fill (opaque, drawn behind text).
    pub selection_bg: Color,
    /// Text color over [`ThemeTokens::selection_bg`] (differs from `text` in high contrast).
    pub selection_text: Color,
    /// Selection fill while the window or control is not focused.
    pub selection_inactive_bg: Color,
    /// Caret color.
    pub caret: Color,
    /// Inline code and code block background.
    pub code_bg: Color,
    /// Table border.
    pub table_border: Color,
    /// Zebra row fill of tables.
    pub table_zebra: Color,
    /// Dividers, hairlines, borders.
    pub hairline: Color,
    /// Control fill while hovered (translucent overlay, or opaque in high contrast).
    pub hover_bg: Color,
    /// Control fill while pressed.
    pub pressed_bg: Color,
    /// Text color over `hover_bg` / `pressed_bg`.
    pub hover_text: Color,
    /// Highlight pill colors.
    pub highlight: HighlightPalette,
    /// Callout colors.
    pub callout: CalloutPalette,
    /// Link text; at least 4.5:1 on `surface`.
    pub link: Color,
    pub error: Color,
    pub success: Color,
    pub warning: Color,
    /// Keyboard focus ring; at least 3:1 on `surface`.
    pub focus_ring: Color,
    /// Scrollbar thumb (translucent).
    pub scrollbar: Color,
    pub tooltip_bg: Color,
    pub tooltip_text: Color,
    pub menu_bg: Color,
    pub metrics: ThemeMetrics,
}

/// The system colors `ssw-win` reads with `GetSysColor` in high-contrast mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SystemColors {
    /// `COLOR_WINDOW`
    pub window: Color,
    /// `COLOR_WINDOWTEXT`
    pub window_text: Color,
    /// `COLOR_GRAYTEXT`
    pub gray_text: Color,
    /// `COLOR_HIGHLIGHT`
    pub highlight: Color,
    /// `COLOR_HIGHLIGHTTEXT`
    pub highlight_text: Color,
    /// `COLOR_BTNFACE`
    pub button_face: Color,
    /// `COLOR_BTNTEXT`
    pub button_text: Color,
    /// `COLOR_HOTLIGHT`
    pub hotlight: Color,
}

/// A complete theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub mode: ThemeMode,
    pub tokens: ThemeTokens,
}

impl Theme {
    /// The light theme with the given accent.
    #[must_use]
    pub fn light(accent: Color) -> Theme {
        let surface = rgb(0xFFFFFF);
        let callout = |bar: u32| {
            let bar = rgb(bar);
            CalloutStyle {
                bar,
                background: surface.lerp(bar, 0.10),
                icon: bar,
            }
        };
        let highlight = |background: u32, text: u32| HighlightStyle {
            background: rgb(background),
            text: rgb(text),
        };
        Theme {
            mode: ThemeMode::Light,
            tokens: ThemeTokens {
                window_bg: rgb(0xF3F3F3),
                surface,
                sidebar_bg: rgb(0xF7F7F7),
                text: rgb(0x444444),
                text_muted: rgb(0x888888),
                text_disabled: rgb(0xA6A6A6),
                marker: rgb(0xB0B0B0),
                accent,
                accent_text_on: readable_on(accent),
                selection_bg: surface.lerp(accent, 0.30),
                selection_text: rgb(0x444444),
                selection_inactive_bg: rgb(0xD6E5FA),
                caret: rgb(0x222222),
                code_bg: rgb(0xF3F4F6),
                table_border: rgb(0xDCDFE3),
                table_zebra: rgb(0xF8F9FA),
                hairline: rgb(0xE3E3E3),
                hover_bg: Color::BLACK.with_alpha(0.06),
                pressed_bg: Color::BLACK.with_alpha(0.10),
                hover_text: rgb(0x444444),
                highlight: HighlightPalette {
                    red: highlight(0xFDDCDC, 0x8C1D18),
                    orange: highlight(0xFEE7C7, 0x7A4100),
                    yellow: highlight(0xFEF3A8, 0x5C4B00),
                    green: highlight(0xDBFDAE, 0x1E5A0C),
                    blue: highlight(0xD3E8FD, 0x0B4A8F),
                    purple: highlight(0xF8DCFD, 0x6B1F7A),
                },
                callout: CalloutPalette {
                    note: callout(0x0969DA),
                    tip: callout(0x1A7F37),
                    important: callout(0x8250DF),
                    warning: callout(0x9A6700),
                    caution: callout(0xCF222E),
                },
                link: ensure_contrast(accent, surface, TEXT_CONTRAST, Color::BLACK),
                error: rgb(0xC42B1C),
                success: rgb(0x0F7B0F),
                warning: rgb(0x9D5D00),
                focus_ring: ensure_contrast(accent, surface, COMPONENT_CONTRAST, Color::BLACK),
                scrollbar: Color::BLACK.with_alpha(0.38),
                tooltip_bg: rgb(0xF9F9F9),
                tooltip_text: rgb(0x1B1B1B),
                menu_bg: rgb(0xFCFCFC),
                metrics: ThemeMetrics::default(),
            },
        }
    }

    /// The dark theme with the given accent. Accent-colored text (links, focus ring) is
    /// lightened until it reaches the required contrast on the dark surface.
    #[must_use]
    pub fn dark(accent: Color) -> Theme {
        let surface = rgb(0x272727);
        let callout = |bar: u32| {
            let bar = rgb(bar);
            CalloutStyle {
                bar,
                background: surface.lerp(bar, 0.16),
                icon: bar,
            }
        };
        let highlight = |background: u32, text: u32| HighlightStyle {
            background: rgb(background),
            text: rgb(text),
        };
        Theme {
            mode: ThemeMode::Dark,
            tokens: ThemeTokens {
                window_bg: rgb(0x1C1C1C),
                surface,
                sidebar_bg: rgb(0x202020),
                text: rgb(0xE4E4E4),
                text_muted: rgb(0x9E9E9E),
                text_disabled: rgb(0x6B6B6B),
                marker: rgb(0x707070),
                accent,
                accent_text_on: readable_on(accent),
                selection_bg: surface.lerp(accent, 0.45),
                selection_text: rgb(0xE4E4E4),
                selection_inactive_bg: rgb(0x3A4656),
                caret: rgb(0xFFFFFF),
                code_bg: rgb(0x303134),
                table_border: rgb(0x3D4045),
                table_zebra: rgb(0x2C2C2C),
                hairline: rgb(0x3A3A3A),
                hover_bg: Color::WHITE.with_alpha(0.08),
                pressed_bg: Color::WHITE.with_alpha(0.12),
                hover_text: rgb(0xE4E4E4),
                highlight: HighlightPalette {
                    red: highlight(0x6B2A2A, 0xFFD6D6),
                    orange: highlight(0x6B4620, 0xFFE2BF),
                    yellow: highlight(0x5E5420, 0xFFF3A3),
                    green: highlight(0x2F5A25, 0xD6F8C5),
                    blue: highlight(0x254A75, 0xCFE4FF),
                    purple: highlight(0x5A2F66, 0xF3D5FA),
                },
                callout: CalloutPalette {
                    note: callout(0x4493F8),
                    tip: callout(0x3FB950),
                    important: callout(0xA371F7),
                    warning: callout(0xD29922),
                    caution: callout(0xF85149),
                },
                link: ensure_contrast(accent, surface, TEXT_CONTRAST, Color::WHITE),
                error: rgb(0xFF99A4),
                success: rgb(0x6CCB5F),
                warning: rgb(0xFCE100),
                focus_ring: ensure_contrast(accent, surface, COMPONENT_CONTRAST, Color::WHITE),
                scrollbar: Color::WHITE.with_alpha(0.42),
                tooltip_bg: rgb(0x2C2C2C),
                tooltip_text: rgb(0xFFFFFF),
                menu_bg: rgb(0x2C2C2C),
                metrics: ThemeMetrics::default(),
            },
        }
    }

    /// The high-contrast theme: every color is taken from the system palette. Hover and press
    /// use the system highlight with its text color; status colors collapse to the text color
    /// (state is conveyed by icons and labels).
    #[must_use]
    pub fn high_contrast(system: SystemColors) -> Theme {
        let highlight = HighlightStyle {
            background: system.highlight,
            text: system.highlight_text,
        };
        let callout = CalloutStyle {
            bar: system.window_text,
            background: system.window,
            icon: system.window_text,
        };
        Theme {
            mode: ThemeMode::HighContrast,
            tokens: ThemeTokens {
                window_bg: system.window,
                surface: system.window,
                sidebar_bg: system.window,
                text: system.window_text,
                text_muted: system.gray_text,
                text_disabled: system.gray_text,
                marker: system.gray_text,
                accent: system.highlight,
                accent_text_on: system.highlight_text,
                selection_bg: system.highlight,
                selection_text: system.highlight_text,
                selection_inactive_bg: system.button_face,
                caret: system.window_text,
                code_bg: system.window,
                table_border: system.window_text,
                table_zebra: system.window,
                hairline: system.window_text,
                hover_bg: system.highlight,
                pressed_bg: system.highlight,
                hover_text: system.highlight_text,
                highlight: HighlightPalette::uniform(highlight),
                callout: CalloutPalette::uniform(callout),
                link: system.hotlight,
                error: system.window_text,
                success: system.window_text,
                warning: system.window_text,
                focus_ring: system.window_text,
                scrollbar: system.window_text,
                tooltip_bg: system.button_face,
                tooltip_text: system.button_text,
                menu_bg: system.window,
                metrics: ThemeMetrics::default(),
            },
        }
    }
}

/// White or black, whichever contrasts more with `background`.
fn readable_on(background: Color) -> Color {
    if Color::WHITE.contrast_ratio(background) >= Color::BLACK.contrast_ratio(background) {
        Color::WHITE
    } else {
        Color::BLACK
    }
}

/// Moves `foreground` towards `toward` in `CONTRAST_STEPS` steps until its contrast on
/// `background` reaches `minimum`; returns `toward` when no step suffices.
fn ensure_contrast(foreground: Color, background: Color, minimum: f32, toward: Color) -> Color {
    (0..=CONTRAST_STEPS)
        .map(|step| foreground.lerp(toward, f32::from(step) / f32::from(CONTRAST_STEPS)))
        .find(|candidate| candidate.contrast_ratio(background) >= minimum)
        .unwrap_or(toward)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Color {
        Color::from_hex(text).expect("valid test color")
    }

    fn hc_black() -> SystemColors {
        SystemColors {
            window: hex("#000000"),
            window_text: hex("#FFFFFF"),
            gray_text: hex("#3FF23F"),
            highlight: hex("#1AEBFF"),
            highlight_text: hex("#000000"),
            button_face: hex("#202020"),
            button_text: hex("#FFFFFF"),
            hotlight: hex("#FFFF00"),
        }
    }

    #[test]
    fn spec_colors_of_the_light_theme() {
        let t = Theme::light(DEFAULT_ACCENT);
        assert_eq!(t.mode, ThemeMode::Light);
        let tokens = t.tokens;
        assert_eq!(tokens.text, hex("#444444"));
        assert_eq!(tokens.text_muted, hex("#888888"));
        assert_eq!(tokens.accent, hex("#1874D3"));
        assert_eq!(tokens.selection_inactive_bg, hex("#D6E5FA"));
        assert_eq!(tokens.code_bg, hex("#F3F4F6"));
        assert_eq!(tokens.table_border, hex("#DCDFE3"));
        assert_eq!(tokens.hairline, hex("#E3E3E3"));
        assert_eq!(tokens.highlight.green.background, hex("#DBFDAE"));
        assert_eq!(tokens.highlight.purple.background, hex("#F8DCFD"));
        assert_eq!(DEFAULT_ACCENT, hex("#1874D3"));
    }

    #[test]
    fn metrics_follow_the_spec() {
        for theme in [
            Theme::light(DEFAULT_ACCENT),
            Theme::dark(DEFAULT_ACCENT),
            Theme::high_contrast(hc_black()),
        ] {
            let m = theme.tokens.metrics;
            assert_eq!(m.control_radius, 4.0);
            assert_eq!(m.overlay_radius, 8.0);
            assert_eq!(m.editor_column_width, 680.0);
            assert_eq!(m.base_font_size, 16.0);
            assert_eq!(m.line_height, 1.75);
            assert_eq!(m.ui_font, "Segoe UI Variable");
            assert_eq!(m.text_font, "Segoe UI Variable Text");
            assert_eq!(m.mono_font, "Cascadia Mono");
            assert_eq!(m.icon_font, "Segoe Fluent Icons");
        }
    }

    #[test]
    fn text_meets_wcag_aa_on_every_surface() {
        for theme in [Theme::light(DEFAULT_ACCENT), Theme::dark(DEFAULT_ACCENT)] {
            let t = theme.tokens;
            let mode = theme.mode;
            let on = |fg: Color, bg: Color| fg.contrast_ratio(bg);
            assert!(on(t.text, t.surface) >= 4.5, "{mode:?} text/surface");
            assert!(on(t.text, t.window_bg) >= 4.5, "{mode:?} text/window");
            assert!(on(t.text, t.sidebar_bg) >= 4.5, "{mode:?} text/sidebar");
            assert!(on(t.text, t.code_bg) >= 4.5, "{mode:?} text/code");
            assert!(on(t.text, t.table_zebra) >= 4.5, "{mode:?} text/zebra");
            assert!(
                on(t.selection_text, t.selection_bg) >= 4.5,
                "{mode:?} selection"
            );
            assert!(
                on(t.selection_text, t.selection_inactive_bg) >= 4.5,
                "{mode:?} inactive"
            );
            assert!(on(t.link, t.surface) >= 4.5, "{mode:?} link");
            assert!(
                on(t.accent_text_on, t.accent) >= 4.5,
                "{mode:?} accent text"
            );
            assert!(on(t.focus_ring, t.surface) >= 3.0, "{mode:?} focus ring");
            assert!(on(t.error, t.surface) >= 4.5, "{mode:?} error");
            assert!(on(t.success, t.surface) >= 4.5, "{mode:?} success");
            assert!(on(t.warning, t.surface) >= 4.5, "{mode:?} warning");
            assert!(on(t.tooltip_text, t.tooltip_bg) >= 4.5, "{mode:?} tooltip");
            assert!(on(t.text, t.menu_bg) >= 4.5, "{mode:?} menu");
            assert!(on(t.text_muted, t.surface) >= 3.0, "{mode:?} muted");
            for kind in HighlightColor::ALL {
                let style = t.highlight.get(kind);
                assert!(on(style.text, style.background) >= 4.5, "{mode:?} {kind:?}");
            }
        }
    }

    #[test]
    fn dark_surface_is_dark_and_light_surface_is_light() {
        let light = Theme::light(DEFAULT_ACCENT).tokens;
        let dark = Theme::dark(DEFAULT_ACCENT).tokens;
        assert!(light.surface.relative_luminance() > 0.9);
        assert!(dark.surface.relative_luminance() < 0.05);
        assert!(light.text.relative_luminance() < light.surface.relative_luminance());
        assert!(dark.text.relative_luminance() > dark.surface.relative_luminance());
        assert_eq!(Theme::dark(DEFAULT_ACCENT).mode, ThemeMode::Dark);
    }

    #[test]
    fn accent_override_flows_into_derived_tokens() {
        let green = hex("#2E7D32");
        let light = Theme::light(green).tokens;
        assert_eq!(light.accent, green);
        // Selection is tinted by the accent.
        assert_ne!(
            light.selection_bg,
            Theme::light(DEFAULT_ACCENT).tokens.selection_bg
        );

        // A pale accent still yields readable link text in the light theme (darkened) ...
        let pale = hex("#F5E050");
        let light_pale = Theme::light(pale).tokens;
        assert_ne!(light_pale.link, pale);
        assert!(light_pale.link.contrast_ratio(light_pale.surface) >= 4.5);
        // ... and dark accent text on the pale accent.
        assert_eq!(light_pale.accent_text_on, Color::BLACK);

        // A dark accent is lightened for links in the dark theme but stays the fill.
        let navy = hex("#103070");
        let dark_navy = Theme::dark(navy).tokens;
        assert_eq!(dark_navy.accent, navy);
        assert_ne!(dark_navy.link, navy);
        assert!(dark_navy.link.contrast_ratio(dark_navy.surface) >= 4.5);
        assert_eq!(dark_navy.accent_text_on, Color::WHITE);
    }

    #[test]
    fn highlight_and_callout_lookup_cover_every_kind() {
        let t = Theme::light(DEFAULT_ACCENT).tokens;
        assert_eq!(t.highlight.get(HighlightColor::Red), t.highlight.red);
        assert_eq!(t.highlight.get(HighlightColor::Orange), t.highlight.orange);
        assert_eq!(t.highlight.get(HighlightColor::Yellow), t.highlight.yellow);
        assert_eq!(t.highlight.get(HighlightColor::Green), t.highlight.green);
        assert_eq!(t.highlight.get(HighlightColor::Blue), t.highlight.blue);
        assert_eq!(t.highlight.get(HighlightColor::Purple), t.highlight.purple);
        assert_eq!(t.callout.get(CalloutKind::Note), t.callout.note);
        assert_eq!(t.callout.get(CalloutKind::Tip), t.callout.tip);
        assert_eq!(t.callout.get(CalloutKind::Important), t.callout.important);
        assert_eq!(t.callout.get(CalloutKind::Warning), t.callout.warning);
        assert_eq!(t.callout.get(CalloutKind::Caution), t.callout.caution);
        // Kinds are visually distinct.
        for (i, a) in CalloutKind::ALL.iter().enumerate() {
            for b in &CalloutKind::ALL[i + 1..] {
                assert_ne!(
                    t.callout.get(*a).bar,
                    t.callout.get(*b).bar,
                    "{a:?} vs {b:?}"
                );
            }
        }
        for (i, a) in HighlightColor::ALL.iter().enumerate() {
            for b in &HighlightColor::ALL[i + 1..] {
                assert_ne!(
                    t.highlight.get(*a).background,
                    t.highlight.get(*b).background
                );
            }
        }
        // Callout backgrounds are a faint tint of the bar over the surface.
        let note = t.callout.note;
        assert!(note.background.relative_luminance() > note.bar.relative_luminance());
        assert_eq!(note.icon, note.bar);
    }

    #[test]
    fn high_contrast_takes_every_color_from_the_system_palette() {
        let system = hc_black();
        let theme = Theme::high_contrast(system);
        assert_eq!(theme.mode, ThemeMode::HighContrast);
        let t = theme.tokens;
        assert_eq!(t.window_bg, system.window);
        assert_eq!(t.surface, system.window);
        assert_eq!(t.text, system.window_text);
        assert_eq!(t.text_muted, system.gray_text);
        assert_eq!(t.accent, system.highlight);
        assert_eq!(t.accent_text_on, system.highlight_text);
        assert_eq!(t.selection_bg, system.highlight);
        assert_eq!(t.selection_text, system.highlight_text);
        assert_eq!(t.link, system.hotlight);
        assert_eq!(t.tooltip_bg, system.button_face);
        assert_eq!(t.tooltip_text, system.button_text);
        assert_eq!(t.hover_bg, system.highlight);
        assert_eq!(t.hover_text, system.highlight_text);
        assert_eq!(
            t.highlight.get(HighlightColor::Green).background,
            system.highlight
        );
        assert_eq!(
            t.highlight.get(HighlightColor::Purple).text,
            system.highlight_text
        );
        assert_eq!(t.callout.get(CalloutKind::Caution).bar, system.window_text);
        assert_eq!(t.callout.get(CalloutKind::Tip).background, system.window);
        assert_eq!(t.focus_ring, system.window_text);
        // Text stays maximally readable on the hover fill and on the surface.
        assert!(t.hover_text.contrast_ratio(t.hover_bg) >= 4.5);
        assert!(t.text.contrast_ratio(t.surface) >= 4.5);
    }

    #[test]
    fn readable_on_picks_the_higher_contrast_color() {
        assert_eq!(readable_on(Color::BLACK), Color::WHITE);
        assert_eq!(readable_on(Color::WHITE), Color::BLACK);
        assert_eq!(readable_on(hex("#1874D3")), Color::WHITE);
        assert_eq!(readable_on(hex("#FFEB3B")), Color::BLACK);
    }

    #[test]
    fn ensure_contrast_stops_at_the_first_sufficient_step_or_the_target() {
        let background = Color::WHITE;
        // Already sufficient: unchanged.
        assert_eq!(
            ensure_contrast(Color::BLACK, background, 4.5, Color::BLACK),
            Color::BLACK
        );
        // Needs darkening, and the result meets the minimum.
        let grey = hex("#999999");
        let fixed = ensure_contrast(grey, background, 4.5, Color::BLACK);
        assert!(fixed.contrast_ratio(background) >= 4.5);
        assert!(fixed.relative_luminance() < grey.relative_luminance());
        // Unreachable minimum falls back to the target.
        assert_eq!(
            ensure_contrast(grey, background, 50.0, Color::BLACK),
            Color::BLACK
        );
    }
}
