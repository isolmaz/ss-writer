//! The block-kind extension point and the default renderers.
//!
//! The editor lays out the top-level blocks of the document. How a block looks and behaves is
//! decided by a [`BlockRenderer`] looked up by the block's AST kind in a [`BlockRegistry`]:
//! [`BlockRenderer::measure`] lays the block out and returns a [`BlockLayout`], the retained
//! result that paints the block, maps points to source offsets (hit testing), carets and
//! selections to rectangles, and describes the block for assistive technology. Other kinds
//! (tables, code, callouts, quotes, lists, math, Mermaid, images, front matter, dividers, page
//! breaks, raw HTML) are added by registering a renderer; the editor core does not change.
//!
//! All coordinates of a [`BlockLayout`] are *block-local*: the origin is the top-left corner of
//! the block in the text column; the gutter to the left of the column has negative x.
//!
//! Defaults shipped here: [`ParagraphRenderer`], [`HeadingRenderer`] and [`FallbackRenderer`]
//! (the block's source text, dimmed, for every kind without a renderer).

use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use ssw_core::markdown::ast::{Block, BlockKind, Document as Ast};
use ssw_core::text::{Buffer, TextRange};

use crate::canvas::{Canvas, IconFont};
use crate::geometry::{Color, CornerRadii, Point, Rect};
use crate::text::{FontWeight, TextLayout, TextLayoutEngine, TextStyle};
use crate::theme::Theme;

use super::config::{Composition, EditorConfig, EditorStrings, LinkTarget};
use super::display::{Display, DisplayParams, GapPolicy, Reveal, build, raw};
use super::object::InlineObjectProvider;

/// Heading sizes as multiples of the body size (spec §3).
pub const HEADING_SCALE: [f32; 6] = [1.75, 1.45, 1.25, 1.1, 1.0, 1.0];

/// The kind of an AST block without its data: the key of the [`BlockRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockClass {
    /// Paragraph.
    Paragraph,
    /// ATX heading.
    Heading,
    /// Bullet, numbered or task list.
    List,
    /// Block quote.
    BlockQuote,
    /// `> [!NOTE]` callout.
    Callout,
    /// Fenced or indented code.
    CodeBlock,
    /// `$$ … $$`.
    MathBlock,
    /// GFM table.
    Table,
    /// `---`, `***`, `___`.
    ThematicBreak,
    /// YAML / TOML front matter.
    FrontMatter,
    /// Raw HTML block.
    HtmlBlock,
    /// `[^label]: …`.
    FootnoteDefinition,
    /// `[label]: dest`.
    LinkReferenceDefinition,
    /// `<!-- pagebreak -->`.
    PageBreak,
}

impl BlockClass {
    /// The class of `kind`.
    #[must_use]
    pub fn of(kind: &BlockKind) -> Self {
        match kind {
            BlockKind::Paragraph => Self::Paragraph,
            BlockKind::Heading { .. } => Self::Heading,
            BlockKind::List { .. } | BlockKind::ListItem { .. } => Self::List,
            BlockKind::BlockQuote => Self::BlockQuote,
            BlockKind::Callout { .. } => Self::Callout,
            BlockKind::CodeBlock(_) => Self::CodeBlock,
            BlockKind::MathBlock(_) => Self::MathBlock,
            BlockKind::Table(_) => Self::Table,
            BlockKind::ThematicBreak => Self::ThematicBreak,
            BlockKind::FrontMatter { .. } => Self::FrontMatter,
            BlockKind::HtmlBlock(_) => Self::HtmlBlock,
            BlockKind::FootnoteDefinition { .. } => Self::FootnoteDefinition,
            BlockKind::LinkReferenceDefinition(_) => Self::LinkReferenceDefinition,
            BlockKind::PageBreak => Self::PageBreak,
        }
    }
}

/// What a renderer sees while measuring a block.
#[derive(Clone, Copy)]
pub struct BlockCx<'a> {
    /// Creates text layouts.
    pub text: &'a dyn TextLayoutEngine,
    /// Colors.
    pub theme: &'a Theme,
    /// Fonts, sizes, spacing (zoom already applicable through its helpers).
    pub config: &'a EditorConfig,
    /// The document text.
    pub buffer: &'a Buffer,
    /// The document tree.
    pub ast: &'a Ast,
    /// Which elements show their markers (selections of the document).
    pub reveal: &'a Reveal,
    /// Supplies images and math.
    pub objects: &'a dyn InlineObjectProvider,
    /// An IME composition to display when it lies inside the block.
    pub composition: Option<&'a Composition>,
    /// Width of the text column in DIPs.
    pub width: f32,
    /// The block is a heading whose section is folded.
    pub folded: bool,
    /// Localized strings.
    pub strings: &'a EditorStrings,
}

impl fmt::Debug for BlockCx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockCx")
            .field("width", &self.width)
            .field("folded", &self.folded)
            .field("reveal", &self.reveal)
            .finish_non_exhaustive()
    }
}

/// What a layout sees while painting.
#[derive(Clone, Copy)]
pub struct BlockPaintCx<'a> {
    /// Colors.
    pub theme: &'a Theme,
    /// Fonts, sizes, spacing.
    pub config: &'a EditorConfig,
    /// Localized strings.
    pub strings: &'a EditorStrings,
    /// Creates text layouts (badges and pills are laid out while painting).
    pub text: &'a dyn TextLayoutEngine,
    /// The pointer is over the block (or its gutter).
    pub hovered: bool,
}

impl fmt::Debug for BlockPaintCx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockPaintCx")
            .field("hovered", &self.hovered)
            .finish_non_exhaustive()
    }
}

/// A click target in the gutter next to a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GutterAction {
    /// Fold or unfold the heading's section.
    ToggleFold,
    /// Open the heading level menu.
    HeadingMenu,
}

/// The retained layout of one measured block.
pub trait BlockLayout {
    /// Total height of the block in DIPs.
    fn height(&self) -> f32;

    /// Draws the block with its top-left corner at the canvas origin. The editor paints the
    /// selection before and the caret after this call.
    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas);

    /// The source offset a caret placed at block-local `point` goes to.
    fn hit_test(&self, point: Point) -> usize;

    /// The caret (zero width, line height) at source `offset`, block-local. Offsets outside the
    /// block clamp to its ends.
    fn caret_rect(&self, offset: usize) -> Rect;

    /// One rectangle per visual line covered by the source `range` (already clipped to the
    /// block), block-local.
    fn selection_rects(&self, range: TextRange) -> Vec<Rect>;

    /// The visual line containing source `offset` as a source range (`Home` / `End`).
    fn line_range(&self, offset: usize) -> TextRange;

    /// The link at block-local `point`.
    fn link_at(&self, _point: Point) -> Option<LinkTarget> {
        None
    }

    /// What a click in the gutter at block-local `point` does.
    fn gutter_hit(&self, _point: Point) -> Option<GutterAction> {
        None
    }

    /// Description of the block for assistive technology.
    fn access_label(&self, strings: &EditorStrings) -> String;
}

/// Measures blocks of one [`BlockClass`].
pub trait BlockRenderer {
    /// Lays `block` out in `cx.width` and returns the retained layout.
    fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout>;
}

/// Renderers by block class, with a fallback for classes without one.
pub struct BlockRegistry {
    renderers: HashMap<BlockClass, Rc<dyn BlockRenderer>>,
    fallback: Rc<dyn BlockRenderer>,
}

impl fmt::Debug for BlockRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut classes: Vec<String> = self
            .renderers
            .keys()
            .map(|class| format!("{class:?}"))
            .collect();
        classes.sort();
        f.debug_struct("BlockRegistry")
            .field("classes", &classes)
            .finish_non_exhaustive()
    }
}

impl Default for BlockRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl BlockRegistry {
    /// Paragraph and heading renderers, and [`FallbackRenderer`] for everything else.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut renderers: HashMap<BlockClass, Rc<dyn BlockRenderer>> = HashMap::new();
        renderers.insert(BlockClass::Paragraph, Rc::new(ParagraphRenderer));
        renderers.insert(BlockClass::Heading, Rc::new(HeadingRenderer));
        Self {
            renderers,
            fallback: Rc::new(FallbackRenderer),
        }
    }

    /// Registers `renderer` for `class`, replacing the previous one.
    pub fn register(&mut self, class: BlockClass, renderer: Rc<dyn BlockRenderer>) {
        self.renderers.insert(class, renderer);
    }

    /// The renderer for `class`, or the fallback.
    #[must_use]
    pub fn renderer(&self, class: BlockClass) -> &dyn BlockRenderer {
        self.renderers
            .get(&class)
            .map_or(&*self.fallback, |renderer| &**renderer)
    }

    /// Measures `block` with the renderer of its class.
    #[must_use]
    pub fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        self.renderer(BlockClass::of(&block.kind))
            .measure(cx, block)
    }
}

/// How a [`TextBlock`] describes itself and what it paints besides text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Paragraph,
    Heading {
        level: u8,
        folded: bool,
        badge: bool,
    },
    Raw,
}

/// A block of text laid out from a [`Display`]: paragraphs, headings and raw fallbacks.
struct TextBlock {
    layout: Box<dyn TextLayout>,
    display: Display,
    kind: TextKind,
    /// Offset of the text inside the block (heading padding).
    origin: Point,
    height: f32,
    /// Where the IME composition is inserted.
    composition_at: Option<usize>,
    /// Color of the text (explicit styles override it).
    color: Color,
    /// Body size, for the gutter widgets.
    size: f32,
}

fn badge_rect(size: f32, pad_top: f32, first_line: f32) -> Rect {
    let height = (size * 0.8).max(14.0);
    Rect::new(-60.0, pad_top + (first_line - height) / 2.0, 36.0, height)
}

fn chevron_rect(size: f32, pad_top: f32, first_line: f32) -> Rect {
    let side = (size * 0.8).max(14.0);
    Rect::new(-22.0, pad_top + (first_line - side) / 2.0, 20.0, side)
}

impl TextBlock {
    fn first_line_height(&self) -> f32 {
        self.layout
            .line_metrics(0)
            .map_or(self.height, |line| line.height)
    }

    fn display_offset(&self, offset: usize) -> usize {
        match (&self.display.composition, self.composition_at) {
            (Some(composition), Some(at)) if at == offset => composition.caret,
            _ => self.display.map.source_to_display(offset),
        }
    }
}

impl BlockLayout for TextBlock {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        canvas.save();
        canvas.translate(self.origin.x, self.origin.y);
        self.display.paint_behind(self.layout.as_ref(), canvas);
        canvas.draw_text_layout(self.layout.as_ref(), Point::ZERO, self.color);
        self.display
            .paint_front(self.layout.as_ref(), canvas, tokens.text);
        canvas.restore();
        let TextKind::Heading {
            level,
            folded,
            badge,
            ..
        } = self.kind
        else {
            return;
        };
        let first_line = self.first_line_height();
        if badge {
            let rect = badge_rect(self.size, self.origin.y, first_line);
            canvas.fill_rounded_rect(rect, CornerRadii::uniform(4.0), tokens.hover_bg);
            let label = &cx.strings.heading_badges[usize::from(level - 1)];
            let style = TextStyle::new(cx.config.text_font.clone(), rect.height * 0.7)
                .with_weight(FontWeight::SEMIBOLD);
            let text = cx.text.layout(label, &style, &[], &[], f32::INFINITY);
            let size = text.size();
            canvas.draw_text_layout(
                text.as_ref(),
                Point::new(
                    rect.x + (rect.width - size.width) / 2.0,
                    rect.y + (rect.height - size.height) / 2.0,
                ),
                tokens.accent,
            );
        }
        if cx.hovered || folded {
            let rect = chevron_rect(self.size, self.origin.y, first_line);
            let glyph = if folded { '\u{E76C}' } else { '\u{E70D}' };
            canvas.draw_icon(
                glyph,
                IconFont::SegoeFluent,
                rect.height * 0.7,
                tokens.text_muted,
                rect.center(),
            );
        }
        if folded {
            let end = self.layout.caret_rect(self.display.text.len(), false);
            let style = TextStyle::new(cx.config.text_font.clone(), self.size * 0.8);
            let label = cx
                .text
                .layout(&cx.strings.fold_pill, &style, &[], &[], f32::INFINITY);
            let size = label.size();
            let pill = Rect::new(
                self.origin.x + end.x + 8.0,
                self.origin.y + end.y + (end.height - size.height - 4.0) / 2.0,
                size.width + 12.0,
                size.height + 4.0,
            );
            canvas.fill_rounded_rect(
                pill,
                CornerRadii::uniform(pill.height / 2.0),
                tokens.hover_bg,
            );
            canvas.draw_text_layout(
                label.as_ref(),
                Point::new(pill.x + 6.0, pill.y + 2.0),
                tokens.text_muted,
            );
        }
    }

    fn hit_test(&self, point: Point) -> usize {
        let local = Point::new(point.x - self.origin.x, point.y - self.origin.y);
        let hit = self.layout.hit_test_point(local);
        self.display.map.display_to_source(hit.insertion_offset())
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        let rect = self.layout.caret_rect(self.display_offset(offset), false);
        rect.translate(self.origin.x, self.origin.y)
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        let map = &self.display.map;
        let start = map.source_to_display(range.start);
        let end = map.source_to_display(range.end);
        self.layout
            .range_rects(start..end)
            .into_iter()
            .map(|rect| rect.translate(self.origin.x, self.origin.y))
            .collect()
    }

    fn line_range(&self, offset: usize) -> TextRange {
        let map = &self.display.map;
        let display = self.display_offset(offset);
        let Some(line) = self
            .layout
            .line_metrics(self.layout.line_for_offset(display))
        else {
            return TextRange::at(offset);
        };
        let start = map.display_to_source(line.start);
        let end = map.display_to_source(line.content_end());
        TextRange::new(start, end.max(start))
    }

    fn link_at(&self, point: Point) -> Option<LinkTarget> {
        let local = Point::new(point.x - self.origin.x, point.y - self.origin.y);
        let hit = self.layout.hit_test_point(local);
        if !hit.inside {
            return None;
        }
        self.display
            .link_at(hit.offset)
            .map(|link| link.target.clone())
    }

    fn gutter_hit(&self, point: Point) -> Option<GutterAction> {
        let TextKind::Heading { badge, .. } = self.kind else {
            return None;
        };
        let first_line = self.first_line_height();
        if chevron_rect(self.size, self.origin.y, first_line).contains(point) {
            return Some(GutterAction::ToggleFold);
        }
        (badge && badge_rect(self.size, self.origin.y, first_line).contains(point))
            .then_some(GutterAction::HeadingMenu)
    }

    fn access_label(&self, strings: &EditorStrings) -> String {
        match self.kind {
            TextKind::Paragraph => strings.paragraph.clone(),
            TextKind::Heading { level, .. } => {
                strings.heading_names[usize::from(level - 1)].clone()
            }
            TextKind::Raw => strings.block.clone(),
        }
    }
}

/// The composition when it lies inside `range`.
fn composition_in<'a>(cx: &BlockCx<'a>, range: TextRange) -> Option<&'a Composition> {
    cx.composition.filter(|comp| range.touches(comp.at))
}

/// Shared measurement of paragraphs and headings.
fn measure_text(
    cx: &BlockCx<'_>,
    block: &Block,
    base: &TextStyle,
    kind: TextKind,
    pad_top: f32,
    pad_bottom: f32,
) -> Box<dyn BlockLayout> {
    let source = cx.buffer.slice(block.range);
    let gap = match kind {
        TextKind::Heading { .. } => GapPolicy::Marker {
            reveal: cx.reveal.touches(block.range),
        },
        _ => GapPolicy::Raw,
    };
    let composition = composition_in(cx, block.range);
    let display = build(&DisplayParams {
        src: &source,
        range: block.range,
        inlines: &block.inlines,
        tokens: &cx.theme.tokens,
        base,
        mono_family: &cx.config.code_font,
        reveal: cx.reveal,
        gap,
        objects: cx.objects,
        max_width: cx.width,
        composition,
    });
    let layout = display.layout(cx.text, base, cx.width);
    let height = layout.size().height + pad_top + pad_bottom;
    Box::new(TextBlock {
        layout,
        display,
        kind,
        origin: Point::new(0.0, pad_top),
        height,
        composition_at: composition.map(|comp| comp.at),
        color: cx.theme.tokens.text,
        size: base.size,
    })
}

/// Renders paragraphs: inline elements with their markers hidden unless touched.
#[derive(Debug, Clone, Copy, Default)]
pub struct ParagraphRenderer;

impl BlockRenderer for ParagraphRenderer {
    fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        let base = TextStyle::new(cx.config.text_font.clone(), cx.config.body_size());
        measure_text(cx, block, &base, TextKind::Paragraph, 0.0, 0.0)
    }
}

/// Renders ATX headings at 1.75 / 1.45 / 1.25 / 1.1 / 1 / 1 × the body size, semibold, with the
/// `#` markers visible while the caret is on the heading, the level badge and the fold chevron
/// in the gutter and the `…` pill when folded.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeadingRenderer;

impl BlockRenderer for HeadingRenderer {
    fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        let level = match &block.kind {
            BlockKind::Heading { level, .. } => (*level).clamp(1, 6),
            _ => 1,
        };
        let size = cx.config.body_size() * HEADING_SCALE[usize::from(level - 1)];
        let base =
            TextStyle::new(cx.config.heading_font.clone(), size).with_weight(FontWeight::SEMIBOLD);
        let kind = TextKind::Heading {
            level,
            folded: cx.folded,
            badge: cx.reveal.selection_touches(block.range),
        };
        measure_text(cx, block, &base, kind, size * 0.35, size * 0.15)
    }
}

/// Shows the source text of any block dimmed (monospace, wrapped); used for every kind that has
/// no renderer, so every document renders.
#[derive(Debug, Clone, Copy, Default)]
pub struct FallbackRenderer;

impl BlockRenderer for FallbackRenderer {
    fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        let tokens = &cx.theme.tokens;
        let base = TextStyle::new(cx.config.code_font.clone(), cx.config.body_size() * 0.95);
        let source = cx.buffer.slice(block.range);
        let composition = composition_in(cx, block.range);
        let display = raw(&source, block.range, composition);
        let layout = display.layout(cx.text, &base, cx.width);
        let height = layout.size().height;
        Box::new(TextBlock {
            layout,
            display,
            kind: TextKind::Raw,
            origin: Point::ZERO,
            height,
            composition_at: composition.map(|comp| comp.at),
            color: tokens.text_muted,
            size: base.size,
        })
    }
}

/// A blank source line between blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GapLayout {
    range: TextRange,
    height: f32,
}

impl GapLayout {
    /// A blank line whose content (without its line break) is `range`, `height` DIPs tall.
    #[must_use]
    pub fn new(range: TextRange, height: f32) -> Self {
        Self { range, height }
    }
}

impl BlockLayout for GapLayout {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint(&self, _cx: &BlockPaintCx<'_>, _canvas: &mut dyn Canvas) {}

    fn hit_test(&self, _point: Point) -> usize {
        self.range.end
    }

    fn caret_rect(&self, _offset: usize) -> Rect {
        Rect::new(0.0, 0.0, 0.0, self.height)
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        if range.is_empty() {
            Vec::new()
        } else {
            vec![Rect::new(0.0, 0.0, 6.0, self.height)]
        }
    }

    fn line_range(&self, _offset: usize) -> TextRange {
        self.range
    }

    fn access_label(&self, strings: &EditorStrings) -> String {
        strings.blank_line.clone()
    }
}
