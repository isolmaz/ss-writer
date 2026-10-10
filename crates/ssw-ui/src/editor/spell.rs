//! Spell check (spec §2.6): red squiggles under misspelled words of paragraphs and headings, and
//! the data of the spelling context menu.
//!
//! The checker itself is behind [`SpellProvider`] (the app implements it with the Windows
//! `ISpellCheckerFactory` of `ssw-win`). [`SpellRenderer`] wraps the renderer of a block class:
//! while the block is measured (that is, lazily, only for the visible blocks) it extracts the
//! checkable text of the block, asks the [`SpellCheck`] service for the misspelled ranges and
//! paints a wavy underline below each of them after the wrapped layout has painted. Results are
//! cached by the checked text, so scrolling, caret moves and edits elsewhere never call the
//! provider again for a paragraph that did not change.
//!
//! What is checked: the text of the block's text nodes, also inside emphasis, links (the link
//! text, not the destination), images (the alt text) and wiki links (the alias, not the target).
//! What is not: inline code, inline math, autolinks and URLs, wiki link targets, footnote
//! references, raw HTML, escapes and entities (a word that touches one is left alone, because
//! the checker cannot see it whole). Code blocks, math blocks and front matter have no inline
//! content and are never checked. Formatting markers do not split words: `un**believ**able`
//! is checked as `unbelievable`.
//!
//! A right click on a misspelled word raises [`EditorEvent::SpellMenu`] with the suggestions;
//! [`SpellCheck::replace`], [`SpellCheck::add_to_dictionary`] and [`SpellCheck::ignore`] carry
//! out the menu entries.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::rc::Rc;

use ssw_core::edit::{Edit, Selection};
use ssw_core::i18n::Localizer;
use ssw_core::markdown::ast::{Block, Inline, InlineKind, LinkKind};
use ssw_core::text::{Buffer, TextRange};

use crate::canvas::{Canvas, Dash, Path};
use crate::geometry::{Color, Point, Rect};
use crate::input::{Cursor, Modifiers};

use super::block::{
    BlockClass, BlockCx, BlockLayout, BlockPaintCx, BlockPress, BlockRegistry, BlockRenderer,
    GutterAction, HeadingRenderer, ParagraphRenderer, TaskBox,
};
use super::config::{EditorEvent, EditorStrings, LinkTarget};
use super::view::EditorView;

/// The English source of the spelling menu strings (`assets/i18n/en/editor-spell.ftl`).
pub const EN_FTL: &str = include_str!("../../../../assets/i18n/en/editor-spell.ftl");

/// Most texts the [`SpellCheck`] cache keeps; it starts over when the limit is reached.
const CACHE_LIMIT: usize = 2048;
/// Horizontal distance between the corners of the wave.
const WAVE_STEP: f32 = 2.0;
/// Distance of the wave's lower edge from the bottom of the line.
const WAVE_FLOOR: f32 = 1.0;
/// Height of the wave.
const WAVE_HEIGHT: f32 = 2.0;
/// Stroke width of the wave.
const WAVE_WIDTH: f32 = 1.0;

/// The spelling checker behind the squiggles.
pub trait SpellProvider {
    /// The misspelled words of `text` as UTF-8 byte ranges on char boundaries, in ascending
    /// order. Called with the checkable text of one block, with the parts that are not checked
    /// (code, URLs, markers) already left out.
    fn check(&self, text: &str) -> Vec<Range<usize>>;

    /// Corrections for `word`, best first; empty when there is none.
    fn suggestions(&self, word: &str) -> Vec<String>;

    /// Adds `word` to the user dictionary: it is no longer reported.
    fn add_to_dictionary(&self, word: &str);

    /// Stops reporting `word` for the rest of the session.
    fn ignore(&self, word: &str);
}

/// The spelling service shared by the renderers of an editor: the provider, the on/off switch
/// (setting "Check spelling") and the cache of results.
pub struct SpellCheck {
    provider: Rc<dyn SpellProvider>,
    enabled: Cell<bool>,
    cache: RefCell<HashMap<String, Rc<[Range<usize>]>>>,
}

impl fmt::Debug for SpellCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpellCheck")
            .field("enabled", &self.enabled.get())
            .field("cached_texts", &self.cache.borrow().len())
            .finish_non_exhaustive()
    }
}

impl SpellCheck {
    /// A service over `provider`, switched on.
    #[must_use]
    pub fn new(provider: Rc<dyn SpellProvider>) -> Self {
        Self {
            provider,
            enabled: Cell::new(true),
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Whether squiggles are drawn.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled.get()
    }

    /// Turns the squiggles on or off. The blocks are measured again at their next layout: call
    /// [`EditorView::invalidate_objects`] to have the visible ones redrawn at once.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.set(on);
    }

    /// Forgets every cached result (the spelling languages changed).
    pub fn clear_cache(&self) {
        self.cache.borrow_mut().clear();
    }

    /// The misspelled byte ranges of `text`: from the cache, or from the provider (and cached).
    #[must_use]
    pub fn misspellings(&self, text: &str) -> Rc<[Range<usize>]> {
        if let Some(found) = self.cache.borrow().get(text) {
            return Rc::clone(found);
        }
        let found: Rc<[Range<usize>]> = self.provider.check(text).into();
        let mut cache = self.cache.borrow_mut();
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(text.to_owned(), Rc::clone(&found));
        found
    }

    /// Corrections for `word`, best first.
    #[must_use]
    pub fn suggestions(&self, word: &str) -> Vec<String> {
        self.provider.suggestions(word)
    }

    /// The menu entry "Add to dictionary": `word` is no longer reported. Call
    /// [`EditorView::invalidate_objects`] afterwards to redraw.
    pub fn add_to_dictionary(&self, word: &str) {
        self.provider.add_to_dictionary(word);
        self.clear_cache();
    }

    /// The menu entry "Ignore": `word` is no longer reported in this session. Call
    /// [`EditorView::invalidate_objects`] afterwards to redraw.
    pub fn ignore(&self, word: &str) {
        self.provider.ignore(word);
        self.clear_cache();
    }

    /// A suggestion chosen in the menu: replaces `range` of the document (the `range` of the
    /// [`EditorEvent::SpellMenu`]) with `replacement` as one undo step and puts the caret behind
    /// it. Returns `false` when the document no longer has text at `range` or refuses the edit.
    pub fn replace(view: &mut EditorView, range: TextRange, replacement: &str) -> bool {
        let tx = view.document().borrow().ctx().transaction(
            vec![Edit::replace(range, replacement)],
            Selection::caret(range.start + replacement.len()),
        );
        view.apply(tx, false)
    }

    /// The misspelled words of `block` as source ranges.
    fn find(&self, buffer: &Buffer, block: &Block) -> Vec<Miss> {
        let checkable = Checkable::collect(buffer, &block.inlines);
        if checkable.text.trim().is_empty() {
            return Vec::new();
        }
        self.misspellings(&checkable.text)
            .iter()
            .filter_map(|found| checkable.miss(found))
            .collect()
    }
}

/// The localized labels of the spelling context menu (message ids in `editor-spell.ftl`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpellStrings {
    /// Entry that adds the word to the user dictionary.
    pub add_to_dictionary: String,
    /// Entry that ignores the word for this session.
    pub ignore: String,
    /// Disabled entry shown when there is no suggestion.
    pub no_suggestions: String,
}

impl SpellStrings {
    /// Resolves every string through `localizer`, which must hold `editor-spell.ftl`.
    #[must_use]
    pub fn new(localizer: &Localizer) -> Self {
        Self {
            add_to_dictionary: localizer.text("spell-add-to-dictionary"),
            ignore: localizer.text("spell-ignore"),
            no_suggestions: localizer.text("spell-no-suggestions"),
        }
    }
}

impl Default for SpellStrings {
    /// The English strings.
    fn default() -> Self {
        Self::new(&Localizer::new("en", &[("en", EN_FTL)]))
    }
}

/// Wraps the paragraph and heading renderers of `registry` with spell checking by `spell`.
/// Renderers that other modules register for these classes afterwards replace the wrapper; wrap
/// them with [`SpellRenderer::new`] instead.
pub fn register(registry: &mut BlockRegistry, spell: Rc<SpellCheck>) {
    registry.register(
        BlockClass::Paragraph,
        Rc::new(SpellRenderer::new(
            Rc::new(ParagraphRenderer),
            Rc::clone(&spell),
        )),
    );
    registry.register(
        BlockClass::Heading,
        Rc::new(SpellRenderer::new(Rc::new(HeadingRenderer), spell)),
    );
}

/// Measures blocks with `inner` and underlines their misspelled words.
pub struct SpellRenderer {
    inner: Rc<dyn BlockRenderer>,
    spell: Rc<SpellCheck>,
}

impl fmt::Debug for SpellRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpellRenderer")
            .field("spell", &self.spell)
            .finish_non_exhaustive()
    }
}

impl SpellRenderer {
    /// A renderer that lays blocks out with `inner` and checks their text with `spell`.
    #[must_use]
    pub fn new(inner: Rc<dyn BlockRenderer>, spell: Rc<SpellCheck>) -> Self {
        Self { inner, spell }
    }
}

impl BlockRenderer for SpellRenderer {
    fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        let layout = self.inner.measure(cx, block);
        if !self.spell.enabled() {
            return layout;
        }
        let misses = self.spell.find(cx.buffer, block);
        if misses.is_empty() {
            return layout;
        }
        Box::new(SpellLayout {
            inner: layout,
            misses,
            spell: Rc::clone(&self.spell),
        })
    }
}

/// A misspelled word: where it is in the document and what it says.
struct Miss {
    range: TextRange,
    word: String,
}

/// A stretch of the checked text and where it comes from in the document.
struct Piece {
    /// Offset in the checked text.
    start: usize,
    /// Offset in the document.
    source: usize,
    len: usize,
}

/// The text of a block that is worth checking, with the map back to the document.
struct Checkable {
    text: String,
    pieces: Vec<Piece>,
    /// Escapes and entities: words touching them are not judged.
    glue: Vec<TextRange>,
    /// Something that is not checked came since the last piece: a space goes between them.
    gap: bool,
}

impl Checkable {
    fn collect(buffer: &Buffer, inlines: &[Inline]) -> Self {
        let mut checkable = Self {
            text: String::new(),
            pieces: Vec::new(),
            glue: Vec::new(),
            gap: false,
        };
        checkable.walk(buffer, inlines);
        checkable
    }

    fn walk(&mut self, buffer: &Buffer, inlines: &[Inline]) {
        for inline in inlines {
            match &inline.kind {
                InlineKind::Text => self.push(buffer, inline.range),
                InlineKind::Emphasis
                | InlineKind::Strong
                | InlineKind::Underline
                | InlineKind::Strikethrough
                | InlineKind::Highlight { .. } => self.walk(buffer, &inline.children),
                InlineKind::Link(link) => {
                    self.gap = true;
                    if !matches!(link.kind, LinkKind::Autolink { .. }) {
                        self.walk(buffer, &inline.children);
                    }
                    self.gap = true;
                }
                InlineKind::Image(_) => {
                    self.gap = true;
                    self.walk(buffer, &inline.children);
                    self.gap = true;
                }
                InlineKind::WikiLink(link) => {
                    self.gap = true;
                    if let Some(alias) = link.alias_range {
                        self.push(buffer, alias);
                    }
                    self.gap = true;
                }
                InlineKind::Escape | InlineKind::Entity { .. } => {
                    self.glue.push(inline.range);
                    self.gap = true;
                }
                InlineKind::Code { .. }
                | InlineKind::InlineMath
                | InlineKind::FootnoteRef { .. }
                | InlineKind::HardBreak
                | InlineKind::SoftBreak
                | InlineKind::RawHtml => self.gap = true,
            }
        }
    }

    fn push(&mut self, buffer: &Buffer, range: TextRange) {
        if self.gap && !self.text.is_empty() {
            self.text.push(' ');
        }
        self.gap = false;
        let text = buffer.slice(range);
        self.pieces.push(Piece {
            start: self.text.len(),
            source: range.start,
            len: text.len(),
        });
        self.text.push_str(&text);
    }

    /// The misspelling the provider reported at `found` (a range of the checked text), or
    /// `None` when the range is no word of the text, starts or ends between the pieces, or
    /// touches an escape or entity.
    fn miss(&self, found: &Range<usize>) -> Option<Miss> {
        let word = self
            .text
            .get(found.clone())
            .filter(|word| !word.is_empty())?;
        let first = self
            .pieces
            .iter()
            .find(|piece| piece.start <= found.start && found.start < piece.start + piece.len)?;
        let last = self
            .pieces
            .iter()
            .find(|piece| piece.start < found.end && found.end <= piece.start + piece.len)?;
        let range = TextRange::new(
            first.source + found.start - first.start,
            last.source + found.end - last.start,
        );
        let touches_glue = self
            .glue
            .iter()
            .any(|glue| glue.start <= range.end && range.start <= glue.end);
        (!touches_glue).then(|| Miss {
            range,
            word: word.to_owned(),
        })
    }
}

/// The wrapped layout plus the squiggles.
struct SpellLayout {
    inner: Box<dyn BlockLayout>,
    misses: Vec<Miss>,
    spell: Rc<SpellCheck>,
}

/// A wavy line along the bottom of `rect`.
fn squiggle(canvas: &mut dyn Canvas, rect: Rect, color: Color) {
    let floor = rect.bottom() - WAVE_FLOOR;
    let mut path = Path::new().move_to(Point::new(rect.x, floor));
    let corners = (rect.width / WAVE_STEP).ceil().max(1.0) as usize;
    for corner in 1..=corners {
        let x = (rect.x + corner as f32 * WAVE_STEP).min(rect.right());
        let y = if corner % 2 == 1 {
            floor - WAVE_HEIGHT
        } else {
            floor
        };
        path = path.line_to(Point::new(x, y));
    }
    canvas.stroke_path(&path, color, WAVE_WIDTH, Dash::Solid);
}

impl BlockLayout for SpellLayout {
    fn height(&self) -> f32 {
        self.inner.height()
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        self.inner.paint(cx, canvas);
        for miss in &self.misses {
            for rect in self.inner.selection_rects(miss.range) {
                squiggle(canvas, rect, cx.theme.tokens.error);
            }
        }
    }

    fn hit_test(&self, point: Point) -> usize {
        self.inner.hit_test(point)
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        self.inner.caret_rect(offset)
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        self.inner.selection_rects(range)
    }

    fn line_range(&self, offset: usize) -> TextRange {
        self.inner.line_range(offset)
    }

    fn link_at(&self, point: Point) -> Option<LinkTarget> {
        self.inner.link_at(point)
    }

    fn gutter_hit(&self, point: Point) -> Option<GutterAction> {
        self.inner.gutter_hit(point)
    }

    fn task_box_at(&self, point: Point) -> Option<TaskBox> {
        self.inner.task_box_at(point)
    }

    fn context_menu(&self, point: Point, origin: Point) -> Option<EditorEvent> {
        for miss in &self.misses {
            let hit = self
                .inner
                .selection_rects(miss.range)
                .into_iter()
                .find(|rect| rect.contains(point));
            if let Some(rect) = hit {
                return Some(EditorEvent::SpellMenu {
                    range: miss.range,
                    word: miss.word.clone(),
                    suggestions: self.spell.suggestions(&miss.word),
                    anchor: rect.translate(origin.x, origin.y),
                });
            }
        }
        self.inner.context_menu(point, origin)
    }

    fn press(&self, point: Point, origin: Point, mods: Modifiers) -> Option<BlockPress> {
        self.inner.press(point, origin, mods)
    }

    fn cursor_at(&self, point: Point) -> Option<Cursor> {
        self.inner.cursor_at(point)
    }

    fn access_label(&self, strings: &EditorStrings) -> String {
        self.inner.access_label(strings)
    }
}
