//! The system theme: dark mode, accent color, high contrast, transparency and animation
//! settings, read through the [`SystemSettings`] seam and turned into a
//! [`ssw_ui::theme::Theme`]. Registry access is read-only.

use ssw_ui::geometry::Color;
use ssw_ui::theme::{DEFAULT_ACCENT, SystemColors, Theme};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, COLOR_BTNTEXT, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT,
    COLOR_HOTLIGHT, COLOR_WINDOW, COLOR_WINDOWTEXT, GetSysColor, SYS_COLOR_INDEX,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SystemParametersInfoW,
};
use windows::core::{BOOL, HSTRING};

/// The `WM_SETTINGCHANGE` string that announces an app theme / accent change.
pub const THEME_CHANGE_MESSAGE: &str = "ImmersiveColorSet";

const PERSONALIZE_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const ACCENT_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent";

/// The inputs the theme is derived from.
pub trait SystemSettings {
    /// `AppsUseLightTheme` (0 = dark).
    fn apps_use_light_theme(&self) -> Option<u32>;
    /// `AccentColorMenu` as stored: `0xAABBGGRR`.
    fn accent_color_menu(&self) -> Option<u32>;
    /// The DWM colorization color: `0xAARRGGBB`.
    fn colorization_color(&self) -> Option<u32>;
    /// `EnableTransparency`.
    fn transparency_enabled(&self) -> Option<u32>;
    /// A high-contrast theme is active.
    fn high_contrast(&self) -> bool;
    /// `GetSysColor(index)` as a `COLORREF` (`0x00BBGGRR`).
    fn sys_color(&self, index: i32) -> u32;
    /// Client-area animations are enabled.
    fn client_animations(&self) -> bool;
}

/// The settings the app reacts to, with the theme built from them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SystemTheme {
    pub theme: Theme,
    /// Apps use the dark theme (also drives `DWMWA_USE_IMMERSIVE_DARK_MODE`).
    pub dark: bool,
    pub high_contrast: bool,
    /// Transparency effects (Mica) are enabled; always false in high contrast.
    pub transparency_effects: bool,
    pub client_animations: bool,
    pub accent: Color,
}

/// A `COLORREF` (`0x00BBGGRR`) as an opaque color.
#[must_use]
pub fn color_from_colorref(colorref: u32) -> Color {
    Color::from_rgba8(
        (colorref & 0xFF) as u8,
        ((colorref >> 8) & 0xFF) as u8,
        ((colorref >> 16) & 0xFF) as u8,
        255,
    )
}

/// An `AccentColorMenu` value (`0xAABBGGRR`) as an opaque color.
#[must_use]
pub fn accent_from_abgr(value: u32) -> Color {
    color_from_colorref(value & 0x00FF_FFFF)
}

/// A DWM colorization value (`0xAARRGGBB`) as an opaque color.
#[must_use]
pub fn accent_from_argb(value: u32) -> Color {
    Color::from_rgb_u32(value & 0x00FF_FFFF)
}

/// True when the `WM_SETTINGCHANGE` string means the theme or accent changed.
#[must_use]
pub fn is_theme_change(setting: &str) -> bool {
    setting == THEME_CHANGE_MESSAGE
}

/// Builds the theme from `settings`.
#[must_use]
pub fn read_system_theme(settings: &dyn SystemSettings) -> SystemTheme {
    let dark = settings.apps_use_light_theme() == Some(0);
    let accent = settings
        .accent_color_menu()
        .map(accent_from_abgr)
        .or_else(|| settings.colorization_color().map(accent_from_argb))
        .unwrap_or(DEFAULT_ACCENT);
    let high_contrast = settings.high_contrast();
    let theme = if high_contrast {
        let color = |index: SYS_COLOR_INDEX| color_from_colorref(settings.sys_color(index.0));
        Theme::high_contrast(SystemColors {
            window: color(COLOR_WINDOW),
            window_text: color(COLOR_WINDOWTEXT),
            gray_text: color(COLOR_GRAYTEXT),
            highlight: color(COLOR_HIGHLIGHT),
            highlight_text: color(COLOR_HIGHLIGHTTEXT),
            button_face: color(COLOR_BTNFACE),
            button_text: color(COLOR_BTNTEXT),
            hotlight: color(COLOR_HOTLIGHT),
        })
    } else if dark {
        Theme::dark(accent)
    } else {
        Theme::light(accent)
    };
    SystemTheme {
        theme,
        dark,
        high_contrast,
        transparency_effects: !high_contrast && settings.transparency_enabled() != Some(0),
        client_animations: settings.client_animations(),
        accent,
    }
}

/// The real Windows settings.
#[derive(Debug, Clone, Copy, Default)]
pub struct Win32Settings;

fn read_dword(subkey: &str, name: &str) -> Option<u32> {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: valid NUL-terminated strings and an out buffer of `size` bytes; read-only access.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            &HSTRING::from(name),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    (status == ERROR_SUCCESS).then_some(value)
}

impl SystemSettings for Win32Settings {
    fn apps_use_light_theme(&self) -> Option<u32> {
        read_dword(PERSONALIZE_KEY, "AppsUseLightTheme")
    }

    fn accent_color_menu(&self) -> Option<u32> {
        read_dword(ACCENT_KEY, "AccentColorMenu")
    }

    fn colorization_color(&self) -> Option<u32> {
        let mut color = 0u32;
        let mut opaque = BOOL(0);
        // SAFETY: valid out pointers.
        unsafe { DwmGetColorizationColor(&mut color, &mut opaque) }.ok()?;
        Some(color)
    }

    fn transparency_enabled(&self) -> Option<u32> {
        read_dword(PERSONALIZE_KEY, "EnableTransparency")
    }

    fn high_contrast(&self) -> bool {
        let mut info = HIGHCONTRASTW {
            cbSize: size_of::<HIGHCONTRASTW>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a valid HIGHCONTRASTW with its size set.
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                info.cbSize,
                Some((&mut info as *mut HIGHCONTRASTW).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        ok.is_ok() && info.dwFlags.0 & HCF_HIGHCONTRASTON.0 != 0
    }

    fn sys_color(&self, index: i32) -> u32 {
        // SAFETY: pure query.
        unsafe { GetSysColor(SYS_COLOR_INDEX(index)) }
    }

    fn client_animations(&self) -> bool {
        let mut enabled = BOOL(1);
        // SAFETY: `enabled` is a valid BOOL out buffer.
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                Some((&mut enabled as *mut BOOL).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        ok.is_err() || enabled.as_bool()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssw_ui::theme::ThemeMode;

    #[derive(Default)]
    struct Fake {
        light: Option<u32>,
        menu: Option<u32>,
        colorization: Option<u32>,
        transparency: Option<u32>,
        high_contrast: bool,
        animations: bool,
    }

    impl SystemSettings for Fake {
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
            // A distinct COLORREF per index so each mapping is checked.
            0x0001_0203 + (index as u32) * 0x10
        }
        fn client_animations(&self) -> bool {
            self.animations
        }
    }

    #[test]
    fn dark_and_light_follow_apps_use_light_theme() {
        let dark = read_system_theme(&Fake {
            light: Some(0),
            ..Fake::default()
        });
        assert!(dark.dark);
        assert_eq!(dark.theme.mode, ThemeMode::Dark);
        for light in [Some(1), None] {
            let theme = read_system_theme(&Fake {
                light,
                ..Fake::default()
            });
            assert!(!theme.dark);
            assert_eq!(theme.theme.mode, ThemeMode::Light);
        }
    }

    #[test]
    fn accent_prefers_the_menu_value_then_colorization_then_the_default() {
        // AccentColorMenu is ABGR: 0xFF_B0_80_20 -> r=0x20 g=0x80 b=0xB0.
        let from_menu = read_system_theme(&Fake {
            menu: Some(0xFFB0_8020),
            colorization: Some(0xC400_FF00),
            ..Fake::default()
        });
        assert_eq!(from_menu.accent, Color::from_rgba8(0x20, 0x80, 0xB0, 255));
        assert_eq!(from_menu.theme.tokens.accent, from_menu.accent);
        // Colorization is ARGB.
        let from_dwm = read_system_theme(&Fake {
            colorization: Some(0xC412_3456),
            ..Fake::default()
        });
        assert_eq!(from_dwm.accent, Color::from_rgba8(0x12, 0x34, 0x56, 255));
        let default = read_system_theme(&Fake::default());
        assert_eq!(default.accent, DEFAULT_ACCENT);
    }

    #[test]
    fn high_contrast_uses_system_colors_and_disables_effects() {
        let theme = read_system_theme(&Fake {
            high_contrast: true,
            light: Some(0),
            ..Fake::default()
        });
        assert!(theme.high_contrast);
        assert_eq!(theme.theme.mode, ThemeMode::HighContrast);
        let expected =
            |index: SYS_COLOR_INDEX| color_from_colorref(0x0001_0203 + (index.0 as u32) * 0x10);
        assert_eq!(theme.theme.tokens.window_bg, expected(COLOR_WINDOW));
        assert_eq!(theme.theme.tokens.text, expected(COLOR_WINDOWTEXT));
        assert_eq!(theme.theme.tokens.text_muted, expected(COLOR_GRAYTEXT));
        assert_eq!(theme.theme.tokens.selection_bg, expected(COLOR_HIGHLIGHT));
        assert_eq!(
            theme.theme.tokens.selection_text,
            expected(COLOR_HIGHLIGHTTEXT)
        );
        assert_eq!(
            theme.theme.tokens.selection_inactive_bg,
            expected(COLOR_BTNFACE)
        );
        assert_eq!(theme.theme.tokens.link, expected(COLOR_HOTLIGHT));
        assert!(!theme.transparency_effects);
    }

    #[test]
    fn transparency_and_animation_flags() {
        let on = read_system_theme(&Fake {
            transparency: Some(1),
            animations: true,
            ..Fake::default()
        });
        assert!(on.transparency_effects && on.client_animations);
        let unset = read_system_theme(&Fake::default());
        assert!(unset.transparency_effects && !unset.client_animations);
        let off = read_system_theme(&Fake {
            transparency: Some(0),
            ..Fake::default()
        });
        assert!(!off.transparency_effects);
    }

    #[test]
    fn color_conversions() {
        assert_eq!(
            color_from_colorref(0x00CC_BBAA),
            Color::from_rgba8(0xAA, 0xBB, 0xCC, 255)
        );
        assert_eq!(
            accent_from_abgr(0x11CC_BBAA),
            Color::from_rgba8(0xAA, 0xBB, 0xCC, 255)
        );
        assert_eq!(
            accent_from_argb(0xFF11_2233),
            Color::from_rgba8(0x11, 0x22, 0x33, 255)
        );
        assert!(is_theme_change("ImmersiveColorSet"));
        assert!(!is_theme_change("intl"));
    }

    #[test]
    fn the_real_system_reports_a_consistent_theme() {
        let system = read_system_theme(&Win32Settings);
        if system.high_contrast {
            assert_eq!(system.theme.mode, ThemeMode::HighContrast);
        } else {
            assert_eq!(system.theme.mode == ThemeMode::Dark, system.dark);
        }
        assert_eq!(Win32Settings.sys_color(COLOR_WINDOW.0) & 0xFF00_0000, 0);
        let _ = (
            Win32Settings.client_animations(),
            Win32Settings.colorization_color(),
        );
        assert!(Win32Settings.accent_color_menu().is_none() || system.accent.a == 1.0);
    }
}
