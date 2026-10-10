//! What Explorer's visuals do to the preview's theme and editor configuration.

use ssw_preview::appearance::Visuals;
use ssw_ui::editor::{DocumentKind, EditorConfig};
use ssw_ui::theme::{DEFAULT_ACCENT, Theme, ThemeMode};
use ssw_win::theme::{SystemTheme, color_from_colorref};
use windows::Win32::Graphics::Gdi::LOGFONTW;

fn system(dark: bool, high_contrast: bool) -> SystemTheme {
    SystemTheme {
        theme: if dark {
            Theme::dark(DEFAULT_ACCENT)
        } else {
            Theme::light(DEFAULT_ACCENT)
        },
        dark,
        high_contrast,
        transparency_effects: true,
        client_animations: true,
        accent: DEFAULT_ACCENT,
    }
}

fn font(face: &str) -> LOGFONTW {
    let mut font = LOGFONTW::default();
    for (slot, unit) in font.lfFaceName.iter_mut().zip(face.encode_utf16()) {
        *slot = unit;
    }
    font
}

#[test]
fn colorrefs_are_blue_green_red() {
    let mut visuals = Visuals::default();
    assert_eq!(visuals, Visuals::default());
    visuals.set_background(0x0030_2010);
    visuals.set_text(0x0000_00FF);
    assert_eq!(visuals.background, Some(color_from_colorref(0x0030_2010)));
    let red = visuals.text.unwrap();
    assert!(
        (red.r - 1.0).abs() < 1e-6 && red.g == 0.0 && red.b == 0.0,
        "{red:?}"
    );
    let bg = visuals.background.unwrap();
    assert!((bg.r - 16.0 / 255.0).abs() < 1e-6 && (bg.b - 48.0 / 255.0).abs() < 1e-6);
}

#[test]
fn without_visuals_the_system_theme_decides() {
    let none = Visuals::default();
    assert_eq!(
        none.theme(&system(false, false)),
        Theme::light(DEFAULT_ACCENT)
    );
    assert_eq!(
        none.theme(&system(true, false)),
        Theme::dark(DEFAULT_ACCENT)
    );
}

#[test]
fn the_hosts_background_decides_light_or_dark_and_replaces_the_surface() {
    let mut dark_pane = Visuals::default();
    dark_pane.set_background(0x0010_1010);
    let theme = dark_pane.theme(&system(false, false));
    assert_eq!(theme.mode, ThemeMode::Dark);
    assert_eq!(theme.tokens.surface, dark_pane.background.unwrap());
    assert_eq!(theme.tokens.window_bg, dark_pane.background.unwrap());
    let mut light_pane = Visuals::default();
    light_pane.set_background(0x00F0_F0F0);
    let theme = light_pane.theme(&system(true, false));
    assert_eq!(theme.mode, ThemeMode::Light);
    assert_eq!(theme.tokens.surface, light_pane.background.unwrap());
}

#[test]
fn the_hosts_text_color_replaces_the_text_tokens() {
    let mut visuals = Visuals::default();
    visuals.set_text(0x0000_80FF);
    let theme = visuals.theme(&system(false, false));
    let text = visuals.text.unwrap();
    assert_eq!(theme.tokens.text, text);
    assert_eq!(theme.tokens.selection_text, text);
    assert_eq!(theme.tokens.hover_text, text);
    assert_eq!(
        theme.tokens.surface,
        Theme::light(DEFAULT_ACCENT).tokens.surface
    );
}

#[test]
fn high_contrast_keeps_the_system_palette() {
    let mut visuals = Visuals::default();
    visuals.set_background(0x0000_0000);
    visuals.set_text(0x00FF_FFFF);
    let high_contrast = system(true, true);
    assert_eq!(visuals.theme(&high_contrast), high_contrast.theme);
}

#[test]
fn the_hosts_font_becomes_the_body_font() {
    let mut visuals = Visuals::default();
    visuals.set_font(&font("Segoe UI"));
    assert_eq!(visuals.font_family.as_deref(), Some("Segoe UI"));
    let config = visuals.editor_config(DocumentKind::Markdown);
    assert_eq!(&*config.text_font, "Segoe UI");
    // A font without a face name changes nothing.
    visuals.set_font(&font(""));
    assert_eq!(visuals.font_family.as_deref(), Some("Segoe UI"));
}

#[test]
fn the_document_kind_reaches_the_configuration_and_nothing_else_changes() {
    let config = Visuals::default().editor_config(DocumentKind::Mermaid);
    assert_eq!(config.document_kind, DocumentKind::Mermaid);
    let plain = Visuals::default().editor_config(DocumentKind::Markdown);
    assert_eq!(plain, EditorConfig::default());
}
