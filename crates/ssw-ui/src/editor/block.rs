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
use crate::input::{Cursor, Modifiers};
use crate::text::{FontWeight, TextLayout, TextLayoutEngine, TextStyle};
use crate::theme::Theme;

use super::config::{
    Composition, DocumentKind, EditorConfig, EditorEvent, EditorStrings, LinkTarget,
};
use super::display::{Display, DisplayParams, GapPolicy, Reveal, SegmentKind, build, raw};
use super::highlight::{SyntaxColors, Token};
use super::object::InlineObjectProvider;

/// Heading sizes as multiples of the body size (spec §3).
pub const HEADING_SCALE: [f32; 6] = [1.75, 1.45, 1.25, 1.1, 1.0, 1.0];

/// Smallest line height of a heading as a multiple of its own size. Headings otherwise take
/// the body line height (so the vertical rhythm of the page holds), but a large heading must
/// not be cramped: 28 DIP text on a 28 DIP line would touch the lines around it.
pub const HEADING_LINE_HEIGHT: f32 = 1.3;

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
    /// Renderers of container blocks measure their children through it.
    pub registry: &'a BlockRegistry,
    /// The syntax tokens of a plain-text line: those of the document that touch the line, in
    /// order, with their offsets into the document text (not the line). Empty for a line that
    /// is not coloured and for every block of a Markdown or Mermaid document.
    pub plain_tokens: &'a [Token],
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
    /// Open the level menu of a heading of this `level` (1-6).
    HeadingMenu {
        /// The level of the heading the badge belongs to.
        level: u8,
    },
}

/// A task checkbox reported by [`BlockLayout::task_box_at`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TaskBox {
    /// The checkbox, block-local.
    pub rect: Rect,
    /// Source offset of the `[ ]` / `[x]` marker of the task item (any offset on the item's
    /// first line selects the item; the start of the marker is conventional).
    pub offset: usize,
}

/// A text replacement a block asks the editor to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockEdit {
    /// The source range to replace.
    pub range: TextRange,
    /// The replacement.
    pub text: String,
}

/// A pointer drag owned by a block (an image resize handle). The editor captures the pointer
/// while it runs and keeps the block's layout; the block previews the drag itself.
pub trait BlockDrag: fmt::Debug {
    /// The pointer moved to block-local `point`.
    fn moved(&mut self, point: Point);

    /// The button was released at block-local `point`; the edit to apply as one undo step.
    fn finish(self: Box<Self>, point: Point) -> Option<BlockEdit>;

    /// The cursor shown while the drag runs.
    fn cursor(&self) -> Cursor;
}

/// What a left press on an interactive part of a block (a button, a handle, a placeholder) does
/// instead of placing the caret.
#[derive(Debug)]
pub enum BlockPress {
    /// Report an event to the host; the selection does not change.
    Event(EditorEvent),
    /// Replace a source range as one undo step; the selection is mapped through the edit.
    Edit(BlockEdit),
    /// Start a drag owned by the block.
    Drag(Box<dyn BlockDrag>),
}

/// The retained layout of one measured block.
pub trait BlockLayout {
    /// Total height of the block in DIPs.
    fn height(&self) -> f32;

    /// Width of the widest line of the block in DIPs (0 when the layout does not know): bounds
    /// the horizontal scroll of an unwrapped plain-text document.
    fn content_width(&self) -> f32 {
        0.0
    }

    /// Draws what lies behind the text of the block (card and code backgrounds, tints) with the
    /// block's top-left corner at the canvas origin. The editor calls it before it paints search
    /// matches and the selection, so those stay visible over the background.
    fn paint_backdrop(&self, _cx: &BlockPaintCx<'_>, _canvas: &mut dyn Canvas) {}

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

    /// The task checkbox drawn at block-local `point`, if any. Layouts that draw task items
    /// (lists, also nested in quotes) return the box under the point; a click on it toggles the
    /// item through `ssw_core::ops::tasks` as one undo step without moving the selection.
    fn task_box_at(&self, _point: Point) -> Option<TaskBox> {
        None
    }

    /// The event a right click at block-local `point` raises (table cell menu, spelling
    /// suggestions). `origin` is the top-left corner of the block in window DIPs, for anchors.
    fn context_menu(&self, _point: Point, _origin: Point) -> Option<EditorEvent> {
        None
    }

    /// What a left press at block-local `point` does when it hits an interactive part of the
    /// block (copy button, resize handle, image placeholder). `origin` is the top-left corner of
    /// the block in window DIPs, for event anchors. `None` lets the editor place the caret.
    fn press(&self, _point: Point, _origin: Point, _mods: Modifiers) -> Option<BlockPress> {
        None
    }

    /// The cursor to show over block-local `point` when it is not the text cursor (buttons,
    /// handles); `None` keeps the editor's default.
    fn cursor_at(&self, _point: Point) -> Option<Cursor> {
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

/// A renderer that takes over only some blocks of a class (a paragraph that is a lone image, an
/// HTML block that is a lone `<img>`). It is tried before the renderer registered for the class
/// with [`BlockRegistry::register`], whatever the registration order.
pub trait SpecialRenderer {
    /// The layout of `block`, or `None` when this renderer does not handle it.
    fn try_measure(&self, cx: &BlockCx<'_>, block: &Block) -> Option<Box<dyn BlockLayout>>;
}

/// Renderers by block class, with a fallback for classes without one.
pub struct BlockRegistry {
    renderers: HashMap<BlockClass, Rc<dyn BlockRenderer>>,
    specials: HashMap<BlockClass, Vec<Rc<dyn SpecialRenderer>>>,
    documents: HashMap<DocumentKind, Rc<dyn BlockRenderer>>,
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
        let mut documents: HashMap<DocumentKind, Rc<dyn BlockRenderer>> = HashMap::new();
        documents.insert(DocumentKind::PlainText, Rc::new(PlainTextRenderer));
        Self {
            renderers,
            specials: HashMap::new(),
            documents,
            fallback: Rc::new(FallbackRenderer),
        }
    }

    /// Registers the renderer of the blocks of a document presented as `kind` (see
    /// [`DocumentKind`]), replacing the previous one.
    pub fn register_document(&mut self, kind: DocumentKind, renderer: Rc<dyn BlockRenderer>) {
        self.documents.insert(kind, renderer);
    }

    /// Measures `block` of a document presented as `kind` with its document renderer, or the
    /// fallback.
    #[must_use]
    pub fn measure_document(
        &self,
        kind: DocumentKind,
        cx: &BlockCx<'_>,
        block: &Block,
    ) -> Box<dyn BlockLayout> {
        self.documents
            .get(&kind)
            .map_or(&*self.fallback, |renderer| &**renderer)
            .measure(cx, block)
    }

    /// Registers `renderer` for `class`, replacing the previous one.
    pub fn register(&mut self, class: BlockClass, renderer: Rc<dyn BlockRenderer>) {
        self.renderers.insert(class, renderer);
    }

    /// Adds `renderer` to the special renderers of `class`; they are tried in registration
    /// order before the class renderer.
    pub fn register_special(&mut self, class: BlockClass, renderer: Rc<dyn SpecialRenderer>) {
        self.specials.entry(class).or_default().push(renderer);
    }

    /// The renderer for `class`, or the fallback.
    #[must_use]
    pub fn renderer(&self, class: BlockClass) -> &dyn BlockRenderer {
        self.renderers
            .get(&class)
            .map_or(&*self.fallback, |renderer| &**renderer)
    }

    /// Measures `block` with the first special renderer of its class that accepts it, else with
    /// the renderer of its class.
    #[must_use]
    pub fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        let class = BlockClass::of(&block.kind);
        if let Some(layout) = self
            .specials
            .get(&class)
            .and_then(|specials| specials.iter().find_map(|s| s.try_measure(cx, block)))
        {
            return layout;
        }
        self.renderer(class).measure(cx, block)
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

    fn content_width(&self) -> f32 {
        self.layout.size().width + self.origin.x
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
        // `line_for_offset` always names an existing line; the default is the empty line.
        self.layout
            .line_metrics(self.layout.line_for_offset(display))
            .map_or(TextRange::at(offset), |line| {
                let start = map.display_to_source(line.start);
                let end = map.display_to_source(line.content_end());
                TextRange::new(start, end.max(start))
            })
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
        let TextKind::Heading { level, badge, .. } = self.kind else {
            return None;
        };
        let first_line = self.first_line_height();
        if chevron_rect(self.size, self.origin.y, first_line).contains(point) {
            return Some(GutterAction::ToggleFold);
        }
        (badge && badge_rect(self.size, self.origin.y, first_line).contains(point))
            .then_some(GutterAction::HeadingMenu { level })
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
pub(super) fn composition_in<'a>(cx: &BlockCx<'a>, range: TextRange) -> Option<&'a Composition> {
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
        let base = TextStyle::new(cx.config.text_font.clone(), cx.config.body_size())
            .with_line_height(cx.config.line_height());
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
        let base = TextStyle::new(cx.config.heading_font.clone(), size)
            .with_weight(FontWeight::SEMIBOLD)
            .with_line_height(cx.config.line_height_for(size, HEADING_LINE_HEIGHT));
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
        let size = cx.config.body_size() * 0.95;
        let base = TextStyle::new(cx.config.code_font.clone(), size)
            .with_line_height(cx.config.line_height_for(size, 1.2));
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

/// Shows the source text of a plain-text document line as written (monospace, wrapped, text
/// color; the tokens of [`BlockCx::plain_tokens`] in the calm syntax palette): the renderer of
/// [`DocumentKind::PlainText`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PlainTextRenderer;

/// Colors the stretches of `display` that `tokens` (offsets into the document) cover. The
/// display text is the source of one line, so a token is clipped to it and mapped to display
/// offsets through the shown segments: an IME composition inserted inside a token splits it
/// and is itself left in the base style. Tokens the palette would style as plain text anyway
/// are skipped, so a calm document adds a range only for its comments, strings and keywords.
fn color_tokens(display: &mut Display, tokens: &[Token], colors: &SyntaxColors, base: &TextStyle) {
    let line = display.map.source_range();
    for token in tokens.iter().filter(|token| colors.colors(token.kind)) {
        let (start, end) = (token.start.max(line.start), token.end.min(line.end));
        for segment in display.map.segments() {
            if segment.kind != SegmentKind::Shown {
                continue;
            }
            let (from, to) = (start.max(segment.src.start), end.min(segment.src.end));
            if from < to {
                let at = |offset: usize| segment.disp.start + (offset - segment.src.start);
                let piece = Token {
                    start: at(from),
                    end: at(to),
                    kind: token.kind,
                };
                display.styles.push(colors.styled(&piece, base, 0));
            }
        }
    }
}

impl BlockRenderer for PlainTextRenderer {
    fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        let size = cx.config.body_size() * 0.95;
        let base = TextStyle::new(cx.config.code_font.clone(), size)
            .with_line_height(cx.config.line_height_for(size, 1.2));
        let source = cx.buffer.slice(block.range);
        let composition = composition_in(cx, block.range);
        let mut display = raw(&source, block.range, composition);
        if !cx.plain_tokens.is_empty() {
            let colors = SyntaxColors::calm(cx.theme);
            color_tokens(&mut display, cx.plain_tokens, &colors, &base);
        }
        let width = if cx.config.wrap {
            cx.width
        } else {
            f32::INFINITY
        };
        let layout = display.layout(cx.text, &base, width);
        let height = layout.size().height;
        Box::new(TextBlock {
            layout,
            display,
            kind: TextKind::Raw,
            origin: Point::ZERO,
            height,
            composition_at: composition.map(|comp| comp.at),
            color: cx.theme.tokens.text,
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
