//! [`EditorView`]: the editing surface. State, lazy layout, painting and the widget plumbing;
//! input handling lives in `editing.rs`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use ssw_core::document::Document;
use ssw_core::edit::{Edit, Selection, Transaction};
use ssw_core::ops::{Ctx, typing};
use ssw_core::text::TextRange;

use crate::access::{
    AccessAction, AccessNode, AccessState, AccessTree, Patterns, Role, ScrollInfo, TextPatternInfo,
};
use crate::canvas::Canvas;
use crate::geometry::{CornerRadii, Point, Rect, Size};
use crate::input::Event;
use crate::scroll_physics::Extent;
use crate::services::{Clock, TimerId};
use crate::text::{TextLayoutEngine, TextStyle};
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::{Control, SharedClipboard};

use super::block::{BlockCx, BlockDrag, BlockLayout, BlockPaintCx, BlockRegistry, GapLayout};
use super::config::{Composition, DocumentKind, EditorConfig, EditorEvent, EditorStrings};
use super::display::Reveal;
use super::highlight::{Highlighter, Language, Token};
use super::layout::{HeightIndex, Item, ItemKind, build_document_items, document_block, item_at};
use super::modes::{reveal_scroll, typewriter_scroll};
use super::object::{InlineObjectProvider, NoObjects};

/// Space above the first block.
pub(super) const TOP_PAD: f32 = 24.0;
/// Space below the last block.
pub(super) const BOTTOM_PAD: f32 = 48.0;
/// Smallest margin between the window edge and the text column.
pub(super) const MIN_MARGIN: f32 = 24.0;
/// Narrowest text column.
pub(super) const MIN_COLUMN: f32 = 80.0;
/// Caret blink period (half cycle).
pub(super) const BLINK: Duration = Duration::from_millis(530);
/// Period of the auto-scroll timer while a drag holds the pointer near an edge.
pub(super) const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(16);
/// Longest time one auto-scroll tick accounts for (a stalled UI thread must not fling the view).
pub(super) const AUTOSCROLL_MAX_STEP: Duration = Duration::from_millis(100);
/// Travel of the pointer, in DIPs, before a press on the selected text becomes a drag.
pub(super) const TEXT_DRAG_THRESHOLD: f32 = 4.0;
/// Distance kept between the caret and the viewport edge when scrolling it into view.
const REVEAL_MARGIN: f32 = 24.0;

/// Drawing and layout services of the current pass.
#[derive(Clone, Copy)]
pub(super) struct Env<'a> {
    pub text: &'a dyn TextLayoutEngine,
    pub theme: &'a Theme,
    pub scale: f32,
}

/// How a pending caret scroll is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScrollIntent {
    /// Make the caret visible with the least movement.
    Visible,
    /// Keep the caret line at the typewriter position (dead zone applies).
    Typewriter,
    /// Put the caret line at the typewriter position.
    Center,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct LayoutKey {
    version: u64,
    width_bits: u32,
    scale_bits: u32,
    theme: Theme,
    epoch: u64,
}

/// What a drag extends by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DragUnit {
    Char,
    Word,
    Paragraph,
}

/// A selection drag in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Drag {
    pub unit: DragUnit,
    /// The unit under the press (empty at the press offset for [`DragUnit::Char`]).
    pub anchor: TextRange,
}

/// A drag of the selected text in progress (spec §2.4): the text moves (Ctrl: copies) to where
/// the pointer is released.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct TextDrag {
    /// The dragged range: the selection at the press.
    pub source: TextRange,
    /// Where the pointer went down (window DIPs); the drag starts after a few DIPs of travel.
    pub press: Point,
    /// The source offset under the press: where a plain click puts the caret.
    pub click: usize,
    /// The pointer moved far enough for a drag.
    pub started: bool,
    /// Ctrl is held: the text is copied.
    pub copy: bool,
}

/// The auto-scroll timer of a running drag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct AutoScroll {
    pub timer: TimerId,
    /// Clock time of the previous tick.
    pub last_tick: Duration,
    /// The pointer in window DIPs.
    pub pointer: Point,
}

/// Search results to paint ([`EditorView::set_search_highlights`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SearchHighlights {
    pub matches: Vec<TextRange>,
    pub current: Option<usize>,
}

impl SearchHighlights {
    /// The current match.
    pub fn current_range(&self) -> Option<TextRange> {
        self.current
            .and_then(|index| self.matches.get(index))
            .copied()
    }
}

/// A drag owned by a block (an image resize handle) in progress.
pub(super) struct BlockDragState {
    /// The layout item that started it.
    pub item: usize,
    pub drag: Box<dyn BlockDrag>,
}

/// A point mapped into the document.
pub(super) struct Hit {
    pub item: usize,
    pub offset: usize,
    /// Block-local position.
    pub local: Point,
    pub layout: Rc<dyn BlockLayout>,
}

/// Layout state that is refreshed lazily from `&self` (paint has no `&mut`).
#[derive(Default)]
pub(super) struct Cache {
    items_key: Option<(u64, u64)>,
    layout_key: Option<LayoutKey>,
    pub items: Vec<Item>,
    pub heights: HeightIndex,
    pub layouts: HashMap<usize, Rc<dyn BlockLayout>>,
    pub scroll_y: f32,
    selection_ranges: Vec<TextRange>,
    pub pending: Option<ScrollIntent>,
    pub heights_dirty: bool,
    /// Height of one line of body text as the engine lays it out (estimates and blank lines).
    pub line_px: f32,
    line_key: Option<(u64, u32)>,
    /// Tokenizer of a plain-text document as a whole; the code blocks of a Markdown document
    /// have their own. It keeps the tokens of every line: a new document version rescans the
    /// lines between the common head and tail of the old and new text, plus the lines whose
    /// incoming scanner state changed.
    plain_highlighter: Highlighter,
    /// The document version and language `plain_tokens` were computed for.
    plain_key: Option<(u64, Option<Language>)>,
    /// The syntax tokens of the whole plain-text document (offsets into its text); empty unless
    /// [`EditorView::plain_highlight_language`] names a language.
    plain_tokens: Vec<Token>,
}

/// The editing surface of spec §2: a centred text column over a shared [`Document`], rendered
/// live (inline markers hidden unless touched), virtualized (only the visible blocks are laid
/// out), with caret, selection, IME composition, typewriter and focus modes, zoom and folding
/// hooks. Text changes only through `ssw_core::ops` and [`Document::apply`].
///
/// The host calls the [`Widget`] passes as for any widget, routes keyboard events to it while
/// it is focused, drains [`EditorView::take_events`] after every event, runs commands through
/// [`EditorView::execute`] and positions the hidden system caret at
/// [`EditorView::caret_rect`].
pub struct EditorView {
    pub(super) id: WidgetId,
    pub(super) doc: Rc<RefCell<Document>>,
    pub(super) clock: Rc<dyn Clock>,
    pub(super) clipboard: Option<SharedClipboard>,
    pub(super) registry: BlockRegistry,
    pub(super) objects: Rc<dyn InlineObjectProvider>,
    pub(super) config: EditorConfig,
    pub(super) strings: EditorStrings,
    pub(super) bounds: Rect,
    pub(super) focused: bool,
    pub(super) caret_on: bool,
    pub(super) blink_enabled: bool,
    /// Text never changes (Explorer preview): every edit path is refused in `apply`/`undo_redo`.
    pub(super) read_only: bool,
    /// Horizontal scroll of an unwrapped plain-text document, in DIPs; shifts the column.
    pub(super) scroll_x: std::cell::Cell<f32>,
    pub(super) blink_timer: Option<TimerId>,
    pub(super) goal_x: Option<f32>,
    pub(super) drag: Option<Drag>,
    pub(super) block_drag: Option<BlockDragState>,
    pub(super) text_drag: Option<TextDrag>,
    pub(super) autoscroll: Option<AutoScroll>,
    /// Where dropped text would land (source offset), drawn as a caret.
    pub(super) drop_caret: Option<usize>,
    pub(super) search: SearchHighlights,
    pub(super) composition: Option<Composition>,
    pub(super) hovered_item: Option<usize>,
    pub(super) typewriter: bool,
    pub(super) focus_mode: bool,
    pub(super) hidden: Vec<TextRange>,
    pub(super) folded_headings: Vec<usize>,
    pub(super) events: Vec<EditorEvent>,
    pub(super) last_selection: Selection,
    /// Bumped by everything that changes how blocks look.
    pub(super) epoch: u64,
    /// Bumped when the hidden (folded) ranges change.
    pub(super) folds_epoch: u64,
    pub(super) cache: RefCell<Cache>,
}

impl fmt::Debug for EditorView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EditorView")
            .field("id", &self.id)
            .field("bounds", &self.bounds)
            .field("focused", &self.focused)
            .field("typewriter", &self.typewriter)
            .field("focus_mode", &self.focus_mode)
            .field("zoom_percent", &self.config.zoom_percent)
            .finish_non_exhaustive()
    }
}

impl EditorView {
    /// An editor over `doc`. `clock` times typing groups for undo.
    #[must_use]
    pub fn new(doc: Rc<RefCell<Document>>, clock: Rc<dyn Clock>) -> Self {
        let last_selection = doc.borrow().primary_selection();
        Self {
            id: WidgetId::allocate(),
            doc,
            clock,
            clipboard: None,
            registry: BlockRegistry::with_defaults(),
            objects: Rc::new(NoObjects),
            config: EditorConfig::default(),
            strings: EditorStrings::default(),
            bounds: Rect::ZERO,
            focused: false,
            caret_on: true,
            blink_enabled: true,
            read_only: false,
            scroll_x: std::cell::Cell::new(0.0),
            blink_timer: None,
            goal_x: None,
            drag: None,
            block_drag: None,
            text_drag: None,
            autoscroll: None,
            drop_caret: None,
            search: SearchHighlights::default(),
            composition: None,
            hovered_item: None,
            typewriter: false,
            focus_mode: false,
            hidden: Vec::new(),
            folded_headings: Vec::new(),
            events: Vec::new(),
            last_selection,
            epoch: 0,
            folds_epoch: 0,
            cache: RefCell::new(Cache::default()),
        }
    }

    /// The shared document.
    #[must_use]
    pub fn document(&self) -> &Rc<RefCell<Document>> {
        &self.doc
    }

    /// Replaces the configuration (fonts, sizes, spacing, modes' parameters, zoom).
    pub fn set_config(&mut self, config: EditorConfig) {
        if config != self.config {
            if config.document_kind != self.config.document_kind {
                self.folds_epoch += 1;
            }
            if config.document_kind != self.config.document_kind || config.wrap != self.config.wrap
            {
                self.scroll_x.set(0.0);
            }
            self.config = config;
            self.epoch += 1;
            self.cache.get_mut().heights_dirty = true;
        }
    }

    /// The current configuration.
    #[must_use]
    pub fn config(&self) -> &EditorConfig {
        &self.config
    }

    /// Replaces the localized strings.
    pub fn set_strings(&mut self, strings: EditorStrings) {
        self.strings = strings;
        self.epoch += 1;
    }

    /// Sets the source of images and math. Call [`EditorView::invalidate_objects`] when it has
    /// new answers.
    pub fn set_object_provider(&mut self, provider: Rc<dyn InlineObjectProvider>) {
        self.objects = provider;
        self.epoch += 1;
    }

    /// Relayouts the blocks that contain objects (a scene or image became ready).
    pub fn invalidate_objects(&mut self) {
        self.epoch += 1;
    }

    /// The clipboard used by copy, cut and paste commands.
    pub fn set_clipboard(&mut self, clipboard: SharedClipboard) {
        self.clipboard = Some(clipboard);
    }

    /// The block renderers; register renderers for more block classes here.
    pub fn registry_mut(&mut self) -> &mut BlockRegistry {
        self.epoch += 1;
        &mut self.registry
    }

    /// Folding hook: `hidden` are the source ranges inside folded sections (sorted, disjoint;
    /// `ssw_core::folding::FoldSet::hidden_ranges`) and `folded_headings` the source offsets of
    /// the heading blocks that are folded (they show the `…` pill).
    pub fn set_folds(&mut self, hidden: Vec<TextRange>, mut folded_headings: Vec<usize>) {
        folded_headings.sort_unstable();
        self.hidden = hidden;
        self.folded_headings = folded_headings;
        self.folds_epoch += 1;
        self.epoch += 1;
    }

    /// True when `offset` lies inside a folded section.
    #[must_use]
    pub fn is_offset_hidden(&self, offset: usize) -> bool {
        self.hidden
            .iter()
            .any(|range| range.start < offset && offset <= range.end)
    }

    /// Turns the caret blink on or off (system animation setting).
    pub fn set_caret_blink(&mut self, enabled: bool) {
        self.blink_enabled = enabled;
        self.caret_on = true;
    }

    /// Read-only mode (the Explorer preview): selection, navigation, copy, zoom and scrolling
    /// work, but no command, key or drop changes the document, the caret is not drawn and
    /// Markdown markers stay hidden whatever the selection.
    pub fn set_read_only(&mut self, read_only: bool) {
        if self.read_only != read_only {
            self.read_only = read_only;
            self.epoch += 1;
        }
    }

    /// Whether the view refuses to change the document.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Typewriter mode: keeps the caret line at the configured position while typing and
    /// navigating, with overscroll past the end.
    pub fn set_typewriter(&mut self, on: bool) {
        if self.typewriter != on {
            self.typewriter = on;
            if on {
                self.cache.get_mut().pending = Some(ScrollIntent::Typewriter);
            }
        }
    }

    /// Whether typewriter mode is on.
    #[must_use]
    pub fn typewriter(&self) -> bool {
        self.typewriter
    }

    /// Focus mode: blocks other than the current one are dimmed.
    pub fn set_focus_mode(&mut self, on: bool) {
        self.focus_mode = on;
    }

    /// Whether focus mode is on.
    #[must_use]
    pub fn focus_mode(&self) -> bool {
        self.focus_mode
    }

    /// Sets the zoom in percent (clamped to 50–400).
    pub fn set_zoom_percent(&mut self, percent: u32) {
        let mut config = self.config.clone();
        config.zoom_percent = percent.clamp(
            super::config::MIN_ZOOM_PERCENT,
            super::config::MAX_ZOOM_PERCENT,
        );
        self.set_config(config);
    }

    /// The zoom in percent.
    #[must_use]
    pub fn zoom_percent(&self) -> u32 {
        self.config.zoom_percent
    }

    /// The scroll offset (content y at the top of the viewport).
    #[must_use]
    pub fn scroll_y(&self) -> f32 {
        self.cache.borrow().scroll_y
    }

    /// Scrolls to `y` (clamped at the next layout pass).
    pub fn set_scroll_y(&mut self, y: f32) {
        self.cache.get_mut().scroll_y = y.max(0.0);
    }

    /// The vertical scrollable range for a scroll driver (DirectManipulation): the viewport
    /// height and the content height, with the typewriter overscroll past the end counted as
    /// content, so [`Extent::max_offset`] is exactly the largest offset the editor accepts.
    /// Blocks that are not laid out yet count with their estimated height; the range is
    /// refined as they are measured.
    #[must_use]
    pub fn scroll_extent(&self) -> Extent {
        let content = self.content_height(&self.cache.borrow());
        Extent::new(f64::from(self.bounds.height), f64::from(content))
    }

    /// The primary selection.
    #[must_use]
    pub fn selection(&self) -> Selection {
        self.doc.borrow().primary_selection()
    }

    /// Takes the events accumulated since the last call.
    pub fn take_events(&mut self) -> Vec<EditorEvent> {
        std::mem::take(&mut self.events)
    }

    /// The number of blocks and blank lines currently laid out (the virtualization window).
    #[must_use]
    pub fn laid_out_items(&self) -> usize {
        self.cache.borrow().layouts.len()
    }

    /// The number of layout items of the document.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.cache.borrow().items.len()
    }

    /// The caret in window DIPs, for the hidden system caret and the IME candidate window; `None`
    /// when it is scrolled out of view or the layout is stale.
    #[must_use]
    pub fn caret_rect(&self) -> Option<Rect> {
        let head = self.doc.borrow().primary_selection().head;
        self.rect_at(head)
    }

    /// The point the source offset `offset` is at, as a zero-width rectangle in window DIPs
    /// ([`EditorView::caret_rect`] for any offset); `None` when it is scrolled out of view or the
    /// layout is stale.
    #[must_use]
    pub fn rect_at(&self, offset: usize) -> Option<Rect> {
        let doc = self.doc.borrow();
        let cache = self.cache.borrow();
        if cache
            .layout_key
            .as_ref()
            .is_none_or(|key| key.version != doc.version())
        {
            return None;
        }
        let index = item_at(&cache.items, offset);
        let local = cache.layouts.get(&index)?.caret_rect(offset);
        let (left, _) = self.column();
        let top = self.bounds.y + TOP_PAD + cache.heights.prefix(index) as f32 - cache.scroll_y;
        let rect = Rect::new(
            left + local.x,
            top + local.y,
            local.width.max(1.0),
            local.height,
        );
        self.bounds.intersects(rect).then_some(rect)
    }

    /// The source offset under the window point `position`, from the blocks that are laid out
    /// (the visible ones); `None` outside the editor, over a block that is not laid out, or when
    /// the layout is stale.
    #[must_use]
    pub fn offset_at(&self, position: Point) -> Option<usize> {
        let doc = self.doc.borrow();
        let cache = self.cache.borrow();
        if !self.bounds.contains(position)
            || cache
                .layout_key
                .as_ref()
                .is_none_or(|key| key.version != doc.version())
        {
            return None;
        }
        let (left, _) = self.column();
        let content_y = position.y - self.bounds.y + cache.scroll_y;
        let index = cache.heights.find(f64::from(content_y - TOP_PAD));
        let local = Point::new(
            position.x - left,
            content_y - TOP_PAD - cache.heights.prefix(index) as f32,
        );
        Some(cache.layouts.get(&index)?.hit_test(local))
    }

    /// Shows (or with `None` hides) a drop caret where text dropped at the window point
    /// `position` would land, for drags that arrive from outside the editor (another tab, the
    /// file tree, Explorer). Returns whether a drop is possible there (the view is editable and
    /// the point is over text).
    pub fn set_drop_caret(&mut self, position: Option<Point>) -> bool {
        self.drop_caret = position
            .and_then(|point| self.offset_at(point))
            .filter(|_| !self.read_only);
        self.drop_caret.is_some()
    }

    /// Inserts `text` at the window point `position` as one undo step and selects it: the drop
    /// of text that comes from outside the editor. Returns whether the text was inserted
    /// (`false` when the point is not over text, `text` is empty or the view is read-only).
    pub fn drop_text(&mut self, position: Point, text: &str) -> bool {
        self.drop_caret = None;
        let Some(target) = self.offset_at(position) else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        let before = self.selection();
        let after = Selection::new(target, target + text.len());
        self.apply(
            Transaction::single(Edit::insert(target, text), before, after),
            false,
        )
    }

    /// Deletes `range` as one undo step: the source side of a move to another tab once the
    /// target accepted the text of an [`EditorEvent::TextDroppedOutside`]. Returns whether the
    /// range was removed (`false` when it is empty, outside the text or the view is read-only).
    pub fn remove_range(&mut self, range: TextRange) -> bool {
        if range.is_empty() || range.end > self.doc.borrow().buffer().len() {
            return false;
        }
        let tx = {
            let doc = self.doc.borrow();
            let ctx = doc.ctx();
            // The deletion of a selection: ordered lists it touches are renumbered.
            typing::delete_forward(Ctx::new(
                ctx.buffer,
                ctx.ast,
                Selection::new(range.start, range.end),
            ))
        };
        tx.is_some_and(|tx| self.apply(tx, false))
    }

    /// The source range currently visible in the viewport.
    #[must_use]
    pub fn visible_range(&self) -> Option<Range<usize>> {
        let cache = self.cache.borrow();
        if cache.items.is_empty() {
            return None;
        }
        let first = cache.heights.find(f64::from(cache.scroll_y - TOP_PAD));
        let last = cache
            .heights
            .find(f64::from(cache.scroll_y + self.bounds.height - TOP_PAD));
        Some(cache.items[first].range.start..cache.items[last].range.end)
    }

    // ----- geometry -------------------------------------------------------------------------

    /// Left edge and width of the text column in window DIPs.
    pub(super) fn column(&self) -> (f32, f32) {
        let available = (self.bounds.width - 2.0 * MIN_MARGIN).max(MIN_COLUMN);
        let width = self.config.column_width().min(available).max(MIN_COLUMN);
        (
            self.bounds.x + (self.bounds.width - width) / 2.0 - self.scroll_x.get(),
            width,
        )
    }

    /// Plain-text lines are not wrapped and the view scrolls horizontally.
    pub(super) fn is_unwrapped(&self) -> bool {
        self.config.document_kind == DocumentKind::PlainText && !self.config.wrap
    }

    /// The largest horizontal scroll: the widest laid-out line minus the visible text width.
    pub(super) fn max_scroll_x(&self) -> f32 {
        let widest = self
            .cache
            .borrow()
            .layouts
            .values()
            .map(|layout| layout.content_width())
            .fold(0.0, f32::max);
        (widest - (self.bounds.width - 2.0 * MIN_MARGIN)).max(0.0)
    }

    /// How far the view can scroll sideways: [`EditorView::max_scroll_x`] while the lines are
    /// not wrapped, nothing otherwise.
    fn horizontal_extent(&self) -> f32 {
        if self.is_unwrapped() {
            self.max_scroll_x()
        } else {
            0.0
        }
    }

    /// The width of text visible at once, at least 1 DIP so that ratios stay finite.
    fn shown_width(&self) -> f32 {
        (self.bounds.width - 2.0 * MIN_MARGIN).max(1.0)
    }

    /// The scrollable content height: the blocks with their padding and, in typewriter mode, the
    /// room past the end that lets the last line reach the caret position.
    fn content_height(&self, cache: &Cache) -> f32 {
        let content = TOP_PAD + cache.heights.total() as f32 + BOTTOM_PAD;
        let overscroll = if self.typewriter {
            self.bounds.height * (1.0 - self.config.typewriter_position)
        } else {
            0.0
        };
        content + overscroll
    }

    fn max_scroll(&self, cache: &Cache) -> f32 {
        (self.content_height(cache) - self.bounds.height).max(0.0)
    }

    fn estimate(&self, item: &Item, line_px: f32) -> f32 {
        if item.hidden {
            return 0.0;
        }
        match item.kind {
            ItemKind::Gap { separator: true } => self.config.spacing_px().max(1.0),
            ItemKind::Gap { separator: false } => line_px,
            ItemKind::Block(_) => item.lines as f32 * line_px,
        }
    }

    pub(super) fn reveal(&self, doc: &Document) -> Reveal {
        Reveal::new(!self.config.auto_hide_markdown, self.reveal_ranges(doc))
    }

    /// The selections and the current search match: elements they touch show their markers. A
    /// read-only view shows the rendered document only: nothing is ever revealed, so selecting
    /// text to copy does not shift the layout.
    fn reveal_ranges(&self, doc: &Document) -> Vec<TextRange> {
        if self.read_only {
            return Vec::new();
        }
        let mut ranges: Vec<TextRange> = doc.selections().iter().map(|s| s.range()).collect();
        ranges.extend(self.search.current_range());
        ranges
    }

    /// Sets the search results to paint: every match in the match color and the `current` one
    /// (an index into `matches`, which are sorted and do not overlap) in the current-match color
    /// with an outline. The current match also reveals the hidden Markdown markers it touches
    /// (spec §7), and when it lies inside a folded section an
    /// [`EditorEvent::UnfoldRequested`] asks the app to unfold it. Scrolling to the match is the
    /// caller's job ([`EditorView::go_to`]).
    pub fn set_search_highlights(&mut self, matches: Vec<TextRange>, current: Option<usize>) {
        let current = current.filter(|&index| index < matches.len());
        let highlights = SearchHighlights { matches, current };
        if highlights == self.search {
            return;
        }
        self.search = highlights;
        if let Some(range) = self.search.current_range()
            && self.hidden.iter().any(|hidden| hidden.intersects(range))
        {
            self.events.push(EditorEvent::UnfoldRequested { range });
        }
    }

    /// Removes the search highlights.
    pub fn clear_search_highlights(&mut self) {
        self.set_search_highlights(Vec::new(), None);
    }

    /// The rectangles of the search matches inside `range` (the source range of one layout
    /// item), block-local, each flagged when it belongs to the current match.
    fn search_rects(&self, layout: &dyn BlockLayout, range: TextRange) -> Vec<(Rect, bool)> {
        let search = &self.search;
        let first = search
            .matches
            .partition_point(|found| found.end <= range.start);
        let mut rects = Vec::new();
        for (index, found) in search.matches.iter().enumerate().skip(first) {
            if found.start >= range.end {
                break;
            }
            let current = search.current == Some(index);
            rects.extend(
                found
                    .intersection(range)
                    .into_iter()
                    .flat_map(|shared| layout.selection_rects(shared))
                    .map(|rect| (rect, current)),
            );
        }
        rects
    }

    /// Asks the next layout pass to scroll the caret into view.
    pub(super) fn request_scroll(&mut self, intent: ScrollIntent) {
        self.cache.get_mut().pending = Some(intent);
    }

    // ----- lazy layout ----------------------------------------------------------------------

    /// The language the lines of a plain-text document are coloured as: none for any other
    /// document kind, with `syntax_highlighting` off and for a file type without a language.
    fn plain_highlight_language(&self) -> Option<Language> {
        if self.config.document_kind == DocumentKind::PlainText && self.config.syntax_highlighting {
            self.config.plain_language
        } else {
            None
        }
    }

    /// Lays `item` out in `width`; `cache` supplies the height of a blank line and the syntax
    /// tokens of a plain-text document.
    fn make_layout(
        &self,
        doc: &Document,
        env: &Env<'_>,
        reveal: &Reveal,
        width: f32,
        cache: &Cache,
        item: &Item,
    ) -> Rc<dyn BlockLayout> {
        let composing = self
            .composition
            .as_ref()
            .is_some_and(|comp| item.content.touches(comp.at));
        match item.kind {
            ItemKind::Gap { separator } if !composing => {
                let touched = reveal.selection_touches(item.content);
                let height = if separator && !touched {
                    self.config.spacing_px().max(1.0)
                } else {
                    cache.line_px
                };
                Rc::new(GapLayout::new(item.content, height))
            }
            // A blank line holding an IME composition is laid out as the paragraph it becomes.
            ItemKind::Block(_) | ItemKind::Gap { .. } => {
                let kind = self.config.document_kind;
                let synthesized;
                let block = match item.kind {
                    ItemKind::Block(index) if kind == DocumentKind::Markdown => {
                        &doc.ast().blocks[index]
                    }
                    _ => {
                        synthesized = document_block(doc, item, kind);
                        &synthesized
                    }
                };
                // The tokens of the document that touch this line (sorted, disjoint).
                let line_tokens: &[Token] = if kind == DocumentKind::PlainText {
                    let all = &cache.plain_tokens;
                    let first = all.partition_point(|token| token.end <= item.content.start);
                    let last = all.partition_point(|token| token.start < item.content.end);
                    all.get(first..last).unwrap_or_default()
                } else {
                    &[]
                };
                let cx = BlockCx {
                    text: env.text,
                    theme: env.theme,
                    config: &self.config,
                    buffer: doc.buffer(),
                    ast: doc.ast(),
                    reveal,
                    objects: &*self.objects,
                    composition: self.composition.as_ref(),
                    width,
                    folded: self
                        .folded_headings
                        .binary_search(&block.range.start)
                        .is_ok(),
                    strings: &self.strings,
                    registry: &self.registry,
                    plain_tokens: line_tokens,
                };
                if kind == DocumentKind::Markdown {
                    Rc::from(self.registry.measure(&cx, block))
                } else {
                    Rc::from(self.registry.measure_document(kind, &cx, block))
                }
            }
        }
    }

    /// The layout of item `index`, measuring it (and recording its height) when needed; `None`
    /// for an item inside a folded section.
    fn ensure(
        &self,
        doc: &Document,
        env: &Env<'_>,
        reveal: &Reveal,
        width: f32,
        cache: &mut Cache,
        index: usize,
    ) -> Option<Rc<dyn BlockLayout>> {
        // Every caller derives `index` from the item list (a document always has an item).
        let item = cache.items[index];
        if item.hidden {
            return None;
        }
        if let Some(layout) = cache.layouts.get(&index) {
            return Some(Rc::clone(layout));
        }
        let layout = self.make_layout(doc, env, reveal, width, cache, &item);
        cache.heights.set(index, layout.height());
        cache.layouts.insert(index, Rc::clone(&layout));
        Some(layout)
    }

    /// Brings the item list and the cached layouts in line with the document, the selection and
    /// the current pass parameters.
    fn prepare(&self, env: &Env<'_>) {
        let doc = self.doc.borrow();
        let mut cache = self.cache.borrow_mut();
        let c = &mut *cache;
        let width = self.column().1;
        let line_key = (self.epoch, env.scale.to_bits());
        if c.line_key != Some(line_key) {
            let style = TextStyle::new(self.config.text_font.clone(), self.config.body_size())
                .with_line_height(self.config.line_height());
            let measured = env
                .text
                .layout("A", &style, &[], &[], f32::INFINITY)
                .size()
                .height
                .max(1.0);
            c.line_key = Some(line_key);
            if (measured - c.line_px).abs() > f32::EPSILON {
                c.line_px = measured;
                c.heights_dirty = true;
            }
        }
        let line_px = c.line_px;
        // Rebuilding the heights replaces measured heights with estimates: the item at the top
        // and the distance into it are restored below so the same text stays in view.
        let anchor = (c.heights_dirty && !c.items.is_empty()).then(|| {
            let y = f64::from(c.scroll_y - TOP_PAD);
            let index = c.heights.find(y);
            (index, y - c.heights.prefix(index))
        });
        // One tokenizer run over the whole plain-text document per version (or language
        // change), never one per line: the highlighter rescans only the lines that changed.
        let language = self.plain_highlight_language();
        let plain_key = (doc.version(), language);
        if c.plain_key != Some(plain_key) {
            c.plain_tokens = match language {
                Some(language) => c.plain_highlighter.highlight(0, language, &doc.text()),
                None => {
                    c.plain_highlighter = Highlighter::new();
                    Vec::new()
                }
            };
            c.plain_key = Some(plain_key);
        }
        let items_key = (doc.version(), self.folds_epoch);
        if c.items_key != Some(items_key) {
            let items = build_document_items(&doc, &self.hidden, self.config.document_kind);
            let estimate = |item: &Item| self.estimate(item, line_px);
            c.heights = if c.items.is_empty() || c.heights_dirty {
                HeightIndex::new(items.iter().map(estimate).collect())
            } else {
                c.heights.remapped(&c.items, &items, &estimate)
            };
            c.items = items;
            c.items_key = Some(items_key);
            c.heights_dirty = false;
            c.layouts.clear();
        } else if c.heights_dirty {
            c.heights = HeightIndex::new(
                c.items
                    .iter()
                    .map(|item| self.estimate(item, line_px))
                    .collect(),
            );
            c.heights_dirty = false;
            c.layouts.clear();
        }
        let key = LayoutKey {
            version: doc.version(),
            width_bits: width.to_bits(),
            scale_bits: env.scale.to_bits(),
            theme: *env.theme,
            epoch: self.epoch,
        };
        if c.layout_key.as_ref() != Some(&key) {
            c.layouts.clear();
            c.layout_key = Some(key);
        }
        let ranges = self.reveal_ranges(&doc);
        if c.selection_ranges != ranges {
            let old = std::mem::replace(&mut c.selection_ranges, ranges.clone());
            let touched = |range: TextRange| {
                old.iter()
                    .chain(ranges.iter())
                    .any(|sel| sel.start <= range.end && range.start <= sel.end)
            };
            let items = &c.items;
            c.layouts.retain(|index, _| !touched(items[*index].range));
        }
        if let Some((index, offset)) = anchor {
            let index = index.min(c.items.len() - 1);
            self.ensure(&doc, env, &self.reveal(&doc), width, c, index);
            let offset = offset.min(0.99 * f64::from(c.heights.height(index)));
            c.scroll_y = scroll_to_content(c.heights.prefix(index) + offset);
        }
        let max = self.max_scroll(c);
        c.scroll_y = c.scroll_y.clamp(0.0, max);
    }

    /// Lays out the items in and around the viewport; keeps the first visible item in place
    /// when measuring changes the heights above it.
    fn layout_window(&self, env: &Env<'_>) {
        let doc = self.doc.borrow();
        let mut cache = self.cache.borrow_mut();
        let c = &mut *cache;
        let reveal = self.reveal(&doc);
        let width = self.column().1;
        let viewport = self.bounds.height.max(1.0);
        for _ in 0..4 {
            let margin = viewport * 0.5;
            let first = c.heights.find(f64::from(c.scroll_y - margin - TOP_PAD));
            let last = c
                .heights
                .find(f64::from(c.scroll_y + viewport + margin - TOP_PAD));
            let anchor = c.heights.find(f64::from(c.scroll_y - TOP_PAD));
            let anchor_offset = f64::from(c.scroll_y - TOP_PAD) - c.heights.prefix(anchor);
            let mut changed = false;
            for index in first..=last {
                let before = c.heights.height(index);
                self.ensure(&doc, env, &reveal, width, c, index);
                changed |= (c.heights.height(index) - before).abs() > f32::EPSILON;
            }
            if !changed {
                break;
            }
            let max = self.max_scroll(c);
            c.scroll_y =
                scroll_to_content(c.heights.prefix(anchor) + anchor_offset).clamp(0.0, max);
        }
        // Keep the layouts a screen or so around the viewport; the rest is re-measured on demand.
        let near = c.heights.find(f64::from(c.scroll_y - viewport - TOP_PAD))
            ..=c.heights
                .find(f64::from(c.scroll_y + 2.0 * viewport - TOP_PAD));
        let (keep_from, keep_to) = (near.start().saturating_sub(32), near.end() + 32);
        c.layouts
            .retain(|index, _| (keep_from..=keep_to).contains(index));
    }

    /// The caret in content coordinates (x from the column's left edge, y from the top of the
    /// content).
    pub(super) fn caret_content_rect(&self, env: &Env<'_>) -> Option<Rect> {
        let (index, layout) = self.caret_layout(env)?;
        let head = self.doc.borrow().primary_selection().head;
        Some(layout.caret_rect(head).translate(0.0, self.item_top(index)))
    }

    /// The item holding the primary caret and its layout.
    pub(super) fn caret_layout(&self, env: &Env<'_>) -> Option<(usize, Rc<dyn BlockLayout>)> {
        let doc = self.doc.borrow();
        let mut cache = self.cache.borrow_mut();
        let c = &mut *cache;
        let reveal = self.reveal(&doc);
        let width = self.column().1;
        let index = item_at(&c.items, doc.primary_selection().head);
        let layout = self.ensure(&doc, env, &reveal, width, c, index)?;
        Some((index, layout))
    }

    /// Content y of the top of item `index`.
    pub(super) fn item_top(&self, index: usize) -> f32 {
        TOP_PAD + self.cache.borrow().heights.prefix(index) as f32
    }

    /// Resolves a pending caret scroll. Returns whether the scroll offset changed.
    fn apply_scroll_intent(&self, env: &Env<'_>) -> bool {
        let intent = self.cache.borrow_mut().pending.take();
        let Some(intent) = intent else {
            return false;
        };
        let Some(caret) = self.caret_content_rect(env) else {
            return false;
        };
        if self.is_unwrapped() {
            let shown = self.bounds.width - 2.0 * MIN_MARGIN;
            let scroll = self.scroll_x.get();
            if caret.x < scroll {
                self.scroll_x.set((caret.x - MIN_MARGIN).max(0.0));
            } else if caret.x + 2.0 > scroll + shown {
                self.scroll_x.set(caret.x + 2.0 - shown + MIN_MARGIN);
            }
        }
        let mut cache = self.cache.borrow_mut();
        let viewport = self.bounds.height.max(1.0);
        let old = cache.scroll_y;
        let wanted = match intent {
            ScrollIntent::Visible => reveal_scroll(caret, old, viewport, REVEAL_MARGIN),
            ScrollIntent::Typewriter => typewriter_scroll(
                caret,
                old,
                viewport,
                self.config.typewriter_position,
                self.config.typewriter_dead_zone,
                false,
            ),
            ScrollIntent::Center => typewriter_scroll(
                caret,
                old,
                viewport,
                self.config.typewriter_position,
                self.config.typewriter_dead_zone,
                true,
            ),
        };
        let max = self.max_scroll(&cache);
        cache.scroll_y = wanted.clamp(0.0, max);
        (cache.scroll_y - old).abs() > f32::EPSILON
    }

    /// Brings every cached structure up to date; call at the start of every pass and handler.
    pub(super) fn settle(&self, env: &Env<'_>) {
        self.prepare(env);
        for _ in 0..3 {
            self.layout_window(env);
            if !self.apply_scroll_intent(env) {
                break;
            }
        }
        let max_x = self.horizontal_extent();
        self.scroll_x.set(self.scroll_x.get().clamp(0.0, max_x));
    }

    /// The document position under window point `point`.
    pub(super) fn hit_window(&self, env: &Env<'_>, point: Point) -> Option<Hit> {
        self.settle(env);
        let doc = self.doc.borrow();
        let mut cache = self.cache.borrow_mut();
        let c = &mut *cache;
        let reveal = self.reveal(&doc);
        let (left, width) = self.column();
        let content_y = point.y - self.bounds.y + c.scroll_y;
        let index = c.heights.find(f64::from(content_y - TOP_PAD));
        let layout = self.ensure(&doc, env, &reveal, width, c, index)?;
        let local = Point::new(
            point.x - left,
            content_y - TOP_PAD - c.heights.prefix(index) as f32,
        );
        Some(Hit {
            item: index,
            offset: layout.hit_test(local),
            local,
            layout,
        })
    }
}

impl Widget for EditorView {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        let width = if available.width.is_finite() {
            available.width
        } else {
            self.config.column_width() + 2.0 * MIN_MARGIN
        };
        let height = if available.height.is_finite() {
            available.height
        } else {
            600.0
        };
        Size::new(width, height)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        cx.add_tab_stop();
        let env = Env {
            text: cx.text,
            theme: cx.theme,
            scale: cx.scale,
        };
        self.settle(&env);
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        let env = Env {
            text: cx.text,
            theme: cx.theme,
            scale: cx.scale,
        };
        self.settle(&env);
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let doc = self.doc.borrow();
        let cache = self.cache.borrow();
        let tokens = &cx.theme.tokens;
        let (left, width) = self.column();
        canvas.push_clip_rect(self.bounds);
        canvas.fill_rect(self.bounds, tokens.surface);
        let viewport = self.bounds.height;
        let first = cache.heights.find(f64::from(cache.scroll_y - TOP_PAD));
        let last = cache
            .heights
            .find(f64::from(cache.scroll_y + viewport - TOP_PAD));
        let selections: Vec<TextRange> = doc.selections().iter().map(|s| s.range()).collect();
        let current = item_at(&cache.items, doc.primary_selection().head);
        let selection_color = if self.focused {
            tokens.selection_bg
        } else {
            tokens.selection_inactive_bg
        };
        for index in first..=last {
            let Some(layout) = cache.layouts.get(&index) else {
                continue;
            };
            let item = &cache.items[index];
            let top = self.bounds.y + TOP_PAD + cache.heights.prefix(index) as f32 - cache.scroll_y;
            let extent = Rect::new(left - 64.0, top, width + 64.0, layout.height());
            if !cx.needs_paint(extent) {
                continue;
            }
            canvas.save();
            canvas.translate(left, top);
            let paint_cx = BlockPaintCx {
                theme: cx.theme,
                config: &self.config,
                strings: &self.strings,
                text: cx.text,
                hovered: self.hovered_item == Some(index),
            };
            layout.paint_backdrop(&paint_cx, canvas);
            let found = self.search_rects(&**layout, item.range);
            for &(rect, is_current) in &found {
                let fill = if is_current {
                    tokens.highlight.orange.background
                } else {
                    tokens.highlight.yellow.background
                };
                canvas.fill_rounded_rect(rect, CornerRadii::uniform(2.0), fill);
            }
            for selection in &selections {
                let Some(shared) = selection.intersection(item.range) else {
                    continue;
                };
                for rect in layout.selection_rects(shared) {
                    canvas.fill_rect(rect, selection_color);
                }
            }
            for &(rect, _) in found.iter().filter(|(_, is_current)| *is_current) {
                canvas.stroke_rounded_rect(rect, CornerRadii::uniform(2.0), tokens.accent, 1.5);
            }
            let dim =
                self.focus_mode && index != current && matches!(item.kind, ItemKind::Block(_));
            if dim {
                canvas.push_layer(0.35);
            }
            layout.paint(&paint_cx, canvas);
            if dim {
                canvas.pop_layer();
            }
            canvas.restore();
        }
        if self.focused
            && self.caret_on
            && !self.read_only
            && let Some(rect) = self.caret_rect()
        {
            canvas.fill_rect(Rect::new(rect.x, rect.y, 1.5, rect.height), tokens.caret);
        }
        if let Some(rect) = self.drop_caret.and_then(|offset| self.rect_at(offset)) {
            canvas.fill_rect(Rect::new(rect.x, rect.y, 2.0, rect.height), tokens.accent);
        }
        canvas.pop_clip();
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        self.handle_event(cx, e)
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let env = Env {
            text: cx.text,
            theme: cx.theme,
            scale: cx.scale,
        };
        self.settle(&env);
        let doc = self.doc.borrow();
        let cache = self.cache.borrow();
        let selection = doc.primary_selection();
        let current = item_at(&cache.items, selection.head);
        let description = cache
            .layouts
            .get(&current)
            .map(|layout| layout.access_label(&self.strings))
            .unwrap_or_default();
        let max = self.max_scroll(&cache);
        let content = TOP_PAD + cache.heights.total() as f32 + BOTTOM_PAD;
        let max_x = self.horizontal_extent();
        let shown = self.shown_width();
        let scroll = ScrollInfo {
            horizontal_percent: if max_x > 0.0 {
                self.scroll_x.get() / max_x * 100.0
            } else {
                0.0
            },
            vertical_percent: if max > 0.0 {
                cache.scroll_y / max * 100.0
            } else {
                0.0
            },
            horizontal_view_percent: shown / (shown + max_x) * 100.0,
            vertical_view_percent: (self.bounds.height / content * 100.0).min(100.0),
            horizontally_scrollable: max_x > 0.0,
            vertically_scrollable: max > 0.0,
        };
        drop(cache);
        let patterns = Patterns {
            text: Some(TextPatternInfo {
                len: doc.buffer().len(),
                selection: vec![std::ops::Range {
                    start: selection.start(),
                    end: selection.end(),
                }],
                caret: selection.head,
                visible: self.visible_range().unwrap_or(0..0),
            }),
            scroll: Some(scroll),
            ..Patterns::default()
        };
        let state = AccessState {
            focusable: cx.is_focusable(),
            focused: cx.is_focused(),
            multiline: true,
            ..AccessState::default()
        };
        out.insert(
            AccessNode::new(
                self.id,
                Role::Document,
                self.strings.name.clone(),
                self.bounds,
            )
            .with_description(description)
            .with_state(state)
            .with_patterns(patterns),
        );
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::Focus => {
                cx.focus_self();
                true
            }
            AccessAction::ScrollTo {
                horizontal,
                vertical,
            } => {
                let sideways = horizontal.filter(|_| self.is_unwrapped());
                if let Some(percent) = vertical {
                    let max = self.max_scroll(&self.cache.borrow());
                    self.cache.get_mut().scroll_y = (percent / 100.0).clamp(0.0, 1.0) * max;
                }
                if let Some(percent) = sideways {
                    self.scroll_x
                        .set((percent / 100.0).clamp(0.0, 1.0) * self.max_scroll_x());
                }
                let scrolled = vertical.is_some() || sideways.is_some();
                if scrolled {
                    cx.invalidate(self.bounds);
                }
                scrolled
            }
            _ => false,
        }
    }
}

impl Control for EditorView {
    fn id(&self) -> WidgetId {
        self.id
    }
}

/// The scroll offset that puts content y `y` at the top of the viewport. Rounded up to the
/// next `f32` when rounding to nearest falls short: the item starting at `y` then stays the
/// item under the top edge instead of the one above it.
fn scroll_to_content(y: f64) -> f32 {
    let scroll = (y + f64::from(TOP_PAD)) as f32;
    if f64::from(scroll - TOP_PAD) < y {
        scroll.next_up()
    } else {
        scroll
    }
}
