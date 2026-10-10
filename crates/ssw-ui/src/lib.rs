//! Platform-neutral UI logic: widgets, editor view model, shell view models.
//!
//! This crate never touches the OS. `ssw-win` implements the traits declared here ([`canvas::Canvas`],
//! [`text::TextLayoutEngine`], [`services`]) on top of Direct2D, DirectWrite and Win32, translates
//! window messages into [`input::Event`]s and maps the [`access::AccessTree`] to UI Automation.
//! Tests drive the same code with [`canvas::RecordingCanvas`], [`text::MonoTextEngine`],
//! [`services::FakeClock`] and [`services::MemoryClipboard`].
//!
//! # Modules
//!
//! - [`geometry`]: DIP-based points, sizes, rectangles, insets, transforms and colors.
//! - [`theme`]: light, dark and high-contrast [`theme::Theme`]s; every color the UI paints comes
//!   from [`theme::ThemeTokens`].
//! - [`text`]: the styled text layout contract ([`text::TextLayoutEngine`], [`text::TextLayout`]).
//!   All offsets are UTF-8 byte offsets.
//! - [`canvas`]: the drawing contract ([`canvas::Canvas`]) and the recording test canvas.
//! - [`input`]: keyboard, text, IME, pointer, wheel, focus, timer and animation events, and the
//!   cursor shape.
//! - [`access`]: the accessibility tree ([`access::AccessNode`], [`access::AccessTree`]) and the
//!   actions assistive technology sends back ([`access::AccessAction`]).
//! - [`services`]: OS services as traits (timers, clock, clipboard, dialogs, shell) plus the
//!   portable clocks and in-memory clipboard.
//! - [`widget`]: the [`widget::Widget`] trait, its per-pass contexts, the focus manager, pointer
//!   capture and the host requests (invalidation, timers, cursor).

pub mod access;
pub mod canvas;
pub mod geometry;
pub mod input;
pub mod services;
pub mod text;
pub mod theme;
pub mod widget;

pub mod editor;
pub mod scroll_physics;
pub mod shell;
pub mod shortcuts;
pub mod widgets;
