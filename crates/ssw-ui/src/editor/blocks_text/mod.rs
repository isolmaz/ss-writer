//! Renderers of the text blocks of Markdown: lists and tasks, quotes, callouts, dividers, page
//! breaks, front matter, raw HTML and footnote definitions (spec §3).
//!
//! [`register`] adds them to a [`BlockRegistry`]. Containers (lists, quotes, callouts, footnote
//! definitions) lay their child blocks out one below the other: paragraphs and headings with
//! the display map of the editor core (so inline markers hide and reveal as in a top-level
//! paragraph), nested lists, quotes and callouts recursively, and every other kind (code, math,
//! tables, images, …) through the registry, so renderers registered by other modules also work
//! inside containers.
//!
//! # Markers
//!
//! The syntax markers of a container (`>`, list markers, `[^id]:`, the callout tag) are replaced
//! by their rendering (bar, bullet, number, checkbox, icon and title) while the caret is
//! elsewhere and shown dimmed as source text where they belong while the caret touches the
//! block, so the caret never rests inside a hidden range. With *Auto-hide Markdown* off they
//! are always shown. The `>` of continuation lines show with their quote, list-item markers
//! with their item.
//!
//! # Interaction
//!
//! A task checkbox is reported through [`BlockLayout::task_box_at`]; the editor toggles the
//! item. The icon of a callout raises [`EditorEvent::CalloutMenu`] through
//! [`BlockLayout::press`].

mod container;
mod item;
mod list;
mod simple;
mod stack;
mod text;

use std::rc::Rc;
use std::sync::Arc;

use ssw_core::i18n::Localizer;
use ssw_core::markdown::ast::{Block, BlockKind, CalloutKind};

use crate::geometry::Color;
use crate::text::TextStyle;

use super::block::{BlockClass, BlockCx, BlockLayout, BlockRegistry, BlockRenderer};

/// The English source of the strings of this module (`assets/i18n/en/editor-blocks.ftl`).
pub const EN_FTL: &str = include_str!("../../../../../assets/i18n/en/editor-blocks.ftl");

/// The narrowest width a child block is laid out in.
const MIN_WIDTH: f32 = 24.0;

/// The localized strings of the text block renderers (message ids `blocks-…`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockStrings {
    /// Default callout titles, in the order of `CalloutKind::ALL` (Note, Tip, Important,
    /// Warning, Caution).
    pub callout_titles: [String; 5],
    /// Accessible descriptions of callouts, in the order of `CalloutKind::ALL`.
    pub callout_access: [String; 5],
    /// Accessible description of a bullet list.
    pub bullet_list: String,
    /// Accessible description of a numbered list.
    pub numbered_list: String,
    /// Accessible description of a task list.
    pub task_list: String,
    /// Accessible description of a block quote.
    pub quote: String,
    /// Accessible description of a divider.
    pub divider: String,
    /// Accessible description of the front matter.
    pub front_matter: String,
    /// Accessible description of a raw HTML block.
    pub html: String,
    /// Accessible description of a footnote definition.
    pub footnote: String,
    /// Label of a page break.
    pub page_break: String,
}

impl Default for BlockStrings {
    /// The English strings.
    fn default() -> Self {
        Self::new(&Localizer::new("en", &[("en", EN_FTL)]))
    }
}

impl BlockStrings {
    /// Resolves every string through `localizer`.
    #[must_use]
    pub fn new(localizer: &Localizer) -> Self {
        Self {
            callout_titles: [
                localizer.text("blocks-callout-note"),
                localizer.text("blocks-callout-tip"),
                localizer.text("blocks-callout-important"),
                localizer.text("blocks-callout-warning"),
                localizer.text("blocks-callout-caution"),
            ],
            callout_access: [
                localizer.text("blocks-access-callout-note"),
                localizer.text("blocks-access-callout-tip"),
                localizer.text("blocks-access-callout-important"),
                localizer.text("blocks-access-callout-warning"),
                localizer.text("blocks-access-callout-caution"),
            ],
            bullet_list: localizer.text("blocks-access-bullet-list"),
            numbered_list: localizer.text("blocks-access-numbered-list"),
            task_list: localizer.text("blocks-access-task-list"),
            quote: localizer.text("blocks-access-quote"),
            divider: localizer.text("blocks-access-divider"),
            front_matter: localizer.text("blocks-access-front-matter"),
            html: localizer.text("blocks-access-html"),
            footnote: localizer.text("blocks-access-footnote"),
            page_break: localizer.text("blocks-page-break"),
        }
    }
}

/// Registers the renderers of this module (English strings) for lists, block quotes, callouts,
/// footnote definitions, dividers, page breaks, front matter and raw HTML.
pub fn register(registry: &mut BlockRegistry) {
    register_with(registry, BlockStrings::default());
}

/// Like [`register`], with localized `strings`.
pub fn register_with(registry: &mut BlockRegistry, strings: BlockStrings) {
    let strings = Rc::new(strings);
    for class in [
        BlockClass::List,
        BlockClass::BlockQuote,
        BlockClass::Callout,
        BlockClass::FootnoteDefinition,
        BlockClass::ThematicBreak,
        BlockClass::PageBreak,
        BlockClass::FrontMatter,
        BlockClass::HtmlBlock,
    ] {
        registry.register(
            class,
            Rc::new(Renderer {
                class,
                strings: Rc::clone(&strings),
            }),
        );
    }
}

/// The renderer registered for one class.
struct Renderer {
    class: BlockClass,
    strings: Rc<BlockStrings>,
}

impl BlockRenderer for Renderer {
    fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        let ctx = Ctx {
            cx,
            strings: &self.strings,
            depth: 0,
            color: cx.theme.tokens.text,
            shown: false,
        };
        match self.class {
            BlockClass::ThematicBreak => simple::divider(&ctx, block),
            BlockClass::PageBreak => simple::page_break(&ctx, block),
            BlockClass::FrontMatter => simple::front_matter(&ctx, block),
            BlockClass::HtmlBlock => simple::html(&ctx, block),
            _ => measure_block(&ctx, block),
        }
    }
}

/// What nested measurements share.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    /// The context of the block being measured; `cx.width` is the width available to it.
    cx: &'a BlockCx<'a>,
    strings: &'a BlockStrings,
    /// How many list items enclose the block (selects the bullet shape).
    depth: usize,
    /// Color of the text of paragraphs and headings.
    color: Color,
    /// An enclosing container shows its line markers (`>` of continuation lines).
    shown: bool,
}

/// Measures `block` inside a container (or at top level for the kinds this module owns):
/// paragraphs and headings as text, containers recursively, everything else through the
/// registry.
fn measure_block(ctx: &Ctx<'_>, block: &Block) -> Box<dyn BlockLayout> {
    match &block.kind {
        BlockKind::Paragraph => Box::new(text::paragraph(ctx, block)),
        BlockKind::Heading { level, .. } => {
            Box::new(text::heading(ctx, block, (*level).clamp(1, 6)))
        }
        BlockKind::List { .. } | BlockKind::ListItem { .. } => list::measure(ctx, block),
        BlockKind::BlockQuote => container::quote(ctx, block),
        BlockKind::Callout { kind, tag, title } => {
            container::callout(ctx, block, *kind, *tag, *title)
        }
        BlockKind::FootnoteDefinition { label } => container::footnote(ctx, block, label),
        _ => ctx.cx.registry.measure(ctx.cx, block),
    }
}

/// The style of text in the block context: `family` at `size`, every line as tall as a line of
/// body text.
fn style(cx: &BlockCx<'_>, family: &Arc<str>, size: f32) -> TextStyle {
    TextStyle::new(Arc::clone(family), size).with_line_height(cx.config.line_height_for(size, 1.0))
}

/// Index of `kind` in `CalloutKind::ALL` (the order of the string arrays).
fn kind_index(kind: CalloutKind) -> usize {
    CalloutKind::ALL
        .iter()
        .position(|candidate| *candidate == kind)
        .unwrap_or(0)
}
