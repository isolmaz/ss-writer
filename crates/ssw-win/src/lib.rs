//! Win32 platform layer: window, Direct2D, DirectWrite, TSF, UIA.
//!
//! # Modules
//!
//! - [`com`]: the crate error type, COM apartment guards and device-lost classification.
//! - [`dpi`]: DIP / physical pixel conversion and window metrics.
//! - [`fonts`]: bundled font bytes and family fallback rules.
//! - [`wic`]: WIC image decoding and PNG encoding.
//! - [`dwrite`]: [`dwrite::DWriteEngine`], the DirectWrite [`ssw_ui::text::TextLayoutEngine`].
//! - [`gfx`]: the D3D11 / D2D device (created off the UI thread), DirectComposition window
//!   surfaces and WIC render targets.
//! - [`canvas`]: [`canvas::D2dCanvas`], the Direct2D [`ssw_ui::canvas::Canvas`].
//! - [`chrome`]: custom caption buttons: layout, state and painting.
//! - [`window`]: the window class, the custom title bar, message translation and the loop.
//! - [`theme`]: the system theme (dark mode, accent, high contrast) as [`ssw_ui::theme::Theme`].
//! - [`locale`]: UI language and locale-aware number, date and size formatting.
//! - [`services`]: clipboard, dialogs, shell, timers and clock.

pub mod canvas;
pub mod chrome;
pub mod com;
pub mod dpi;
pub mod dwrite;
pub mod fonts;
pub mod gfx;
pub mod locale;
pub mod services;
pub mod theme;
pub mod wic;
pub mod window;

pub mod keyboard;
pub mod uia;
