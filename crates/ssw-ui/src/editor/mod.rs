//! The editing surface: [`EditorView`] and its extension points.
//!
//! The editor renders a Markdown [`Document`](ssw_core::document::Document) live: a centred
//! text column of top-level blocks laid out lazily (only the visible blocks and a margin are
//! measured; heights of the rest are estimated and cached in a Fenwick tree, so scrolling a
//! 60 000-line document is `O(visible)`). Inline Markdown markers are hidden unless the caret or
//! selection touches the element; the [`display`] map translates between source offsets (caret,
//! selection, document) and display offsets (text layout).
//!
//! # Extension points
//!
//! - [`block::BlockRenderer`] / [`block::BlockLayout`] / [`block::BlockRegistry`]: how a block
//!   kind looks and behaves. Default renderers cover paragraphs and headings; every other kind
//!   shows its source dimmed until a renderer is registered with
//!   [`EditorView::registry_mut`].
//! - [`object::InlineObjectProvider`]: supplies images and math for inline objects
//!   ([`EditorView::set_object_provider`]).
//!
//! # Host contract
//!
//! The host runs the [`Widget`](crate::widget::Widget) passes, routes keyboard, text, IME,
//! pointer and wheel events (the view requests focus on click), and after every event drains
//! [`EditorView::take_events`]. Menu and shortcut commands go through [`EditorView::execute`].
//! `ssw-win` positions the hidden system caret and the IME window at
//! [`EditorView::caret_rect`].

pub mod block;
pub mod blocks_media;
pub mod blocks_table;
pub mod blocks_text;
pub mod config;
pub mod display;
pub mod highlight;
pub mod modes;
pub mod object;
pub mod spell;

mod editing;
mod layout;
mod view;

pub use config::{
    Composition, DocumentKind, EN_FTL, EditorConfig, EditorEvent, EditorStrings, LinkTarget,
    MAX_ZOOM_PERCENT, MIN_ZOOM_PERCENT, ZOOM_STEP_PERCENT,
};
pub use view::EditorView;
