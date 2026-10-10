//! DIP / physical pixel conversion and per-monitor DPI window metrics.
//!
//! One DIP is 1/96 inch; the scale factor is `dpi / 96`. Layout and painting stay in DIPs; the
//! conversion to pixels happens only at the window edge (client size, hit testing, swap chain).

use ssw_ui::geometry::{Point, Size};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::WindowsAndMessaging::{SM_CXPADDEDBORDER, SM_CYSIZEFRAME};

/// The DPI of a 100 % scaled display.
pub const DEFAULT_DPI: u32 = 96;

/// Smallest window size the user can resize to, in DIPs.
pub const MIN_WINDOW_SIZE: Size = Size::new(480.0, 320.0);

/// `dpi / 96`; a zero DPI (a destroyed window) counts as 100 %.
#[must_use]
pub fn scale_for_dpi(dpi: u32) -> f32 {
    if dpi == 0 {
        1.0
    } else {
        dpi as f32 / DEFAULT_DPI as f32
    }
}

/// DIPs to the nearest physical pixel.
#[must_use]
pub fn dip_to_px(dip: f32, scale: f32) -> i32 {
    (dip * scale).round() as i32
}

/// Physical pixels to DIPs.
#[must_use]
pub fn px_to_dip(px: i32, scale: f32) -> f32 {
    px as f32 / scale
}

/// A pixel position as DIPs.
#[must_use]
pub fn point_px_to_dip(x: i32, y: i32, scale: f32) -> Point {
    Point::new(px_to_dip(x, scale), px_to_dip(y, scale))
}

/// A pixel size as DIPs.
#[must_use]
pub fn size_px_to_dip(width: i32, height: i32, scale: f32) -> Size {
    Size::new(px_to_dip(width, scale), px_to_dip(height, scale))
}

/// The DPI in the `WM_DPICHANGED` `wParam` (the X and Y values are equal).
#[must_use]
pub fn dpi_from_wparam(wparam: usize) -> u32 {
    (wparam & 0xFFFF) as u32
}

/// Smallest tracking size in pixels at `dpi`.
#[must_use]
pub fn min_track_size_px(dpi: u32) -> (i32, i32) {
    let scale = scale_for_dpi(dpi);
    (
        dip_to_px(MIN_WINDOW_SIZE.width, scale),
        dip_to_px(MIN_WINDOW_SIZE.height, scale),
    )
}

/// Pixel size a window keeps in DIPs when moved from `old_dpi` to `new_dpi`.
#[must_use]
pub fn rescale_px(value: i32, old_dpi: u32, new_dpi: u32) -> i32 {
    dip_to_px(
        px_to_dip(value, scale_for_dpi(old_dpi)),
        scale_for_dpi(new_dpi),
    )
}

/// The DPI of `hwnd`, or [`DEFAULT_DPI`] when the window is gone.
#[must_use]
pub fn window_dpi(hwnd: HWND) -> u32 {
    // SAFETY: `GetDpiForWindow` accepts any handle value and returns 0 for invalid ones.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { DEFAULT_DPI } else { dpi }
}

/// Thickness in pixels of the invisible resize band at the top of the window.
#[must_use]
pub fn top_resize_band_px(dpi: u32) -> i32 {
    // SAFETY: pure metric query without pointer arguments.
    unsafe {
        GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_factor_follows_dpi() {
        for (dpi, scale) in [(96, 1.0), (120, 1.25), (144, 1.5), (192, 2.0), (0, 1.0)] {
            assert_eq!(scale_for_dpi(dpi), scale, "dpi {dpi}");
        }
    }

    #[test]
    fn conversions_round_to_pixels_and_back() {
        assert_eq!(dip_to_px(10.0, 1.25), 13); // 12.5 rounds away from zero
        assert_eq!(dip_to_px(10.0, 1.5), 15);
        assert_eq!(dip_to_px(-10.0, 1.5), -15);
        assert_eq!(px_to_dip(15, 1.5), 10.0);
        assert_eq!(point_px_to_dip(30, 60, 1.5), Point::new(20.0, 40.0));
        assert_eq!(size_px_to_dip(300, 150, 2.0), Size::new(150.0, 75.0));
    }

    #[test]
    fn wparam_dpi_uses_the_low_word() {
        assert_eq!(dpi_from_wparam(0x00C0_00C0), 192);
        assert_eq!(dpi_from_wparam(96), 96);
        assert_eq!(dpi_from_wparam(0x0001_0078), 120);
    }

    #[test]
    fn minimum_size_scales() {
        assert_eq!(min_track_size_px(96), (480, 320));
        assert_eq!(min_track_size_px(144), (720, 480));
        assert_eq!(min_track_size_px(120), (600, 400));
    }

    #[test]
    fn rescaling_keeps_the_dip_size() {
        assert_eq!(rescale_px(960, 96, 192), 1920);
        assert_eq!(rescale_px(1920, 192, 96), 960);
        assert_eq!(rescale_px(1000, 120, 120), 1000);
    }

    #[test]
    fn invalid_window_reports_the_default_dpi() {
        assert_eq!(window_dpi(HWND::default()), DEFAULT_DPI);
        assert!(top_resize_band_px(96) > 0);
        assert!(top_resize_band_px(192) >= top_resize_band_px(96));
    }
}
