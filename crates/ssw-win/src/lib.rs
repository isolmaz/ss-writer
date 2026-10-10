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
//! - [`scene_render`]: [`scene_render::D2dSceneRenderer`], the Direct2D / DirectWrite drawing of
//!   [`ssw_render::scene::Scene`]s (math, Mermaid, SVG, print pages) behind the canvas.
//! - [`chrome`]: custom caption buttons: layout, state and painting.
//! - [`window`]: the window class, the custom title bar, message translation and the loop.
//! - [`theme`]: the system theme (dark mode, accent, high contrast) as [`ssw_ui::theme::Theme`].
//! - [`locale`]: UI language and locale-aware number, date and size formatting.
//! - [`services`]: clipboard, dialogs, shell, timers and clock.
//! - [`dmanip`]: DirectManipulation touch / touchpad panning with inertia and the scroll driver
//!   that hands the offset between it and the animated wheel.
//! - [`print`]: pages as vectors through `ID2D1PrintControl`, the XPS file target and the print
//!   dialog seam.
//! - [`tsf`]: the Text Services Framework text store (IME, dictation, emoji panel, reconversion).
//! - [`caret`]: the hidden system caret that tracks the drawn caret for accessibility tools.
//! - [`instance`], [`jumplist`], [`taskbar`], [`assoc`], [`menus`], [`spell`]: single instance,
//!   Jump List, taskbar progress, file association queries, native popup menus and the spell
//!   checker.
//! - [`dragdrop`]: OLE drop target (files, text, HTML; copy / move / link rules) and the drag
//!   source side for dragging data out.
//! - [`share`]: the Windows share sheet (`DataTransferManager`) for a file.

pub mod assoc;
pub mod canvas;
pub mod caret;
pub mod chrome;
pub mod com;
pub mod dmanip;
pub mod dpi;
pub mod dragdrop;
pub mod dwrite;
pub mod fonts;
pub mod gfx;
pub mod instance;
pub mod jumplist;
pub mod locale;
pub mod menus;
pub mod print;
pub mod scene_render;
pub mod services;
pub mod share;
pub mod spell;
pub mod taskbar;
pub mod theme;
pub mod tsf;
pub mod wic;
pub mod window;

pub mod keyboard;
pub mod uia;
