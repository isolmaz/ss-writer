//! Theme derivation from injected system settings (every combination), the real system
//! settings against independent OS queries, and locale-aware formatting.

use ssw_ui::geometry::Color;
use ssw_ui::theme::{DEFAULT_ACCENT, ThemeMode};
use ssw_win::locale::{Locale, grouping_value, tag_from_langid, ui_language};
use ssw_win::theme::{
    SystemSettings, Win32Settings, accent_from_abgr, accent_from_argb, color_from_colorref,
    is_theme_change, read_system_theme,
};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, COLOR_WINDOWTEXT, GetSysColor};

#[derive(Clone, Copy)]
struct Injected {
    light: Option<u32>,
    menu: Option<u32>,
    colorization: Option<u32>,
    transparency: Option<u32>,
    high_contrast: bool,
    animations: bool,
}

impl SystemSettings for Injected {
    fn apps_use_light_theme(&self) -> Option<u32> {
        self.light
    }
    fn accent_color_menu(&self) -> Option<u32> {
        self.menu
    }
    fn colorization_color(&self) -> Option<u32> {
        self.colorization
    }
    fn transparency_enabled(&self) -> Option<u32> {
        self.transparency
    }
    fn high_contrast(&self) -> bool {
        self.high_contrast
    }
    fn sys_color(&self, index: i32) -> u32 {
        0x0010_2030 + index as u32
    }
    fn client_animations(&self) -> bool {
        self.animations
    }
}

#[test]
fn every_settings_combination_maps_to_a_consistent_theme() {
    let options = |value: u32| [None, Some(0), Some(1), Some(value)];
    let mut checked = 0;
    for light in options(7) {
        for menu in [None, Some(0xFF80_4020)] {
            for colorization in [None, Some(0xC411_2233)] {
                for transparency in options(5) {
                    for high_contrast in [false, true] {
                        for animations in [false, true] {
                            let settings = Injected {
                                light,
                                menu,
                                colorization,
                                transparency,
                                high_contrast,
                                animations,
                            };
                            let system = read_system_theme(&settings);
                            checked += 1;
                            // Dark only for an explicit 0.
                            assert_eq!(system.dark, light == Some(0));
                            // Accent: menu value, then DWM colorization, then the default.
                            let accent = match (menu, colorization) {
                                (Some(_), _) => Color::from_rgba8(0x20, 0x40, 0x80, 255),
                                (None, Some(_)) => Color::from_rgba8(0x11, 0x22, 0x33, 255),
                                (None, None) => DEFAULT_ACCENT,
                            };
                            assert_eq!(system.accent, accent);
                            assert_eq!(system.high_contrast, high_contrast);
                            assert_eq!(system.client_animations, animations);
                            let mode = match (high_contrast, system.dark) {
                                (true, _) => ThemeMode::HighContrast,
                                (false, true) => ThemeMode::Dark,
                                (false, false) => ThemeMode::Light,
                            };
                            assert_eq!(system.theme.mode, mode);
                            // High contrast never uses transparency; otherwise only an
                            // explicit 0 turns it off.
                            assert_eq!(
                                system.transparency_effects,
                                !high_contrast && transparency != Some(0)
                            );
                            if high_contrast {
                                let window =
                                    color_from_colorref(settings.sys_color(COLOR_WINDOW.0));
                                let text =
                                    color_from_colorref(settings.sys_color(COLOR_WINDOWTEXT.0));
                                assert_eq!(system.theme.tokens.window_bg, window);
                                assert_eq!(system.theme.tokens.text, text);
                            } else if !system.dark {
                                assert_eq!(system.theme.tokens.accent, accent);
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(checked, 4 * 2 * 2 * 4 * 2 * 2);
}

#[test]
fn color_encodings_follow_their_byte_orders() {
    // COLORREF is 0x00BBGGRR, AccentColorMenu 0xAABBGGRR, DWM colorization 0xAARRGGBB.
    let expected = Color::from_rgba8(0x11, 0x22, 0x33, 255);
    assert_eq!(color_from_colorref(0x0033_2211), expected);
    assert_eq!(accent_from_abgr(0x7F33_2211), expected);
    assert_eq!(accent_from_argb(0x7F11_2233), expected);
    assert!(is_theme_change("ImmersiveColorSet"));
    assert!(!is_theme_change("immersivecolorset"));
    assert!(!is_theme_change(""));
}

#[test]
fn the_real_settings_agree_with_direct_os_queries() {
    let settings = Win32Settings;
    // SAFETY: pure queries.
    let (window, text) = unsafe { (GetSysColor(COLOR_WINDOW), GetSysColor(COLOR_WINDOWTEXT)) };
    assert_eq!(settings.sys_color(COLOR_WINDOW.0), window);
    assert_eq!(settings.sys_color(COLOR_WINDOWTEXT.0), text);
    let system = read_system_theme(&settings);
    assert_eq!(system.dark, settings.apps_use_light_theme() == Some(0));
    assert_eq!(system.high_contrast, settings.high_contrast());
    assert_eq!(system.client_animations, settings.client_animations());
    if system.high_contrast {
        assert_eq!(system.theme.tokens.window_bg, color_from_colorref(window));
        assert_eq!(system.theme.tokens.text, color_from_colorref(text));
    }
    if let Some(argb) = settings.colorization_color() {
        assert_eq!(accent_from_argb(argb).a, 1.0);
    }
}

#[test]
fn locales_format_numbers_with_their_own_separators() {
    let en = Locale::new("en-US");
    let tr = Locale::new("tr-TR");
    let de = Locale::new("de-DE");
    let cases: [(&Locale, i64, &str); 8] = [
        (&en, 0, "0"),
        (&en, 999, "999"),
        (&en, 1_000, "1,000"),
        (&en, -1_234_567, "-1,234,567"),
        (&tr, 1_234_567, "1.234.567"),
        (&tr, -1_000, "-1.000"),
        (&de, 1_234_567, "1.234.567"),
        (&de, 12, "12"),
    ];
    for (locale, value, expected) in cases {
        assert_eq!(
            locale.format_integer(value),
            expected,
            "{} {value}",
            locale.tag()
        );
    }
    assert_eq!(en.format_decimal(1234.5, 2), "1,234.50");
    assert_eq!(tr.format_decimal(1234.5, 2), "1.234,50");
    assert_eq!(de.format_decimal(0.5, 1), "0,5");
    assert_eq!(tr.format_decimal(1.23456, 3), "1,235");
}

#[test]
fn dates_and_file_sizes_follow_the_locale() {
    let en = Locale::new("en-US");
    let tr = Locale::new("tr-TR");
    assert_eq!(
        en.format_date_short(2026, 10, 10).as_deref(),
        Some("10/10/2026")
    );
    assert_eq!(
        tr.format_date_short(2026, 10, 10).as_deref(),
        Some("10.10.2026")
    );
    assert_eq!(
        en.format_date_long(2026, 10, 10).as_deref(),
        Some("Saturday, October 10, 2026")
    );
    let long = tr.format_date_long(2026, 10, 10).unwrap();
    assert!(
        long.contains("Ekim") && long.contains("Cumartesi") && long.contains("2026"),
        "{long}"
    );
    assert_eq!(en.format_date_short(2026, 2, 30), None);
    assert_eq!(en.format_date_long(2026, 0, 1), None);
    let sizes = [
        (0u64, "0 B", "0 B"),
        (1023, "1,023 B", "1.023 B"),
        (1024, "1.0 KB", "1,0 KB"),
        (1536, "1.5 KB", "1,5 KB"),
        (10 * 1024 * 1024, "10.0 MB", "10,0 MB"),
        (3 * 1024 * 1024 * 1024, "3.0 GB", "3,0 GB"),
    ];
    for (bytes, english, turkish) in sizes {
        assert_eq!(en.format_file_size(bytes), english, "{bytes}");
        assert_eq!(tr.format_file_size(bytes), turkish, "{bytes}");
    }
}

#[test]
fn language_ids_and_grouping_strings_convert() {
    assert_eq!(tag_from_langid(0x041F).as_deref(), Some("tr-TR"));
    assert_eq!(tag_from_langid(0x0409).as_deref(), Some("en-US"));
    assert_eq!(tag_from_langid(0xFFFF), None);
    let ui = ui_language();
    assert!(
        ui.len() >= 2 && ui.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "{ui}"
    );
    for (grouping, expected) in [("3;0", 3), ("3;2;0", 32), ("3;2", 320), ("0", 0), ("", 0)] {
        assert_eq!(grouping_value(grouping), expected, "{grouping:?}");
    }
    let user = Locale::user_default();
    assert!(!user.tag().is_empty());
    assert_eq!(
        Locale::new("zz-not-a-locale-at-all-123456").format_integer(1234),
        "1234"
    );
}
