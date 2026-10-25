//! The find bar (spec §7): a floating panel at the top right of the editor with the find box,
//! the Match case / Whole word / Regex toggles, the `n/m` counter, previous / next / close
//! buttons and, when opened with `Ctrl+H`, the replace row.
//!
//! The bar works on the document it was given ([`SharedDocument`], the same handle the editor
//! view uses): typing a query selects the first match at or after the caret (find as you type),
//! `Enter` / `F3` select the next match and `Shift+Enter` / `Shift+F3` the previous one (wrapping),
//! `Tab` / `Shift+Tab` move between the find box and the replace box of the open replace row,
//! *Replace* replaces the current match and moves to the next, *Replace all* applies every
//! replacement as **one** transaction (one undo step; `$1` expands capture groups in regex mode).
//! Every selection it makes is reported as [`FindEvent::Reveal`] so the host can unfold folds,
//! reveal hidden markers and scroll. The editor paints [`FindBar::matches`] and
//! [`FindBar::current_match`] as highlights.
//!
//! An invalid regular expression is shown as a red outline around the find box, the message
//! "Invalid regular expression" in place of the counter, and the engine's message as the box's
//! accessibility description.
//!
//! Focus: opening the bar cannot focus its box before the host has laid it out. The bar
//! therefore asks for focus with [`FindBar::take_focus_request`]; the host focuses the returned
//! widget after its next layout pass. The host also routes focus changes and accessibility actions
//! of the bar's controls to the bar ([`FindBar::owns`], [`FindBar::perform_child`]).

use std::cell::RefCell;
use std::rc::Rc;

use ssw_core::document::Document;
use ssw_core::edit::{Assoc, Selection, Transaction};
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::search::{Query, SearchError, Searcher};
use ssw_core::text::TextRange;

use crate::access::{AccessAction, AccessNode, AccessTree, Role, ToggleState};
use crate::canvas::{Canvas, IconFont};
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::{Event, KeyEvent, vk};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::button::{Button, ButtonKind};
use crate::widgets::textbox::TextBox;
use crate::widgets::{
    CONTROL_HEIGHT, Control, FONT_SIZE, ICON_SIZE, Interaction, PointerOutcome, SharedClipboard,
    control_state, draw_centered, draw_left_centered, focus_ring, glyph, layout_line,
    outcome_result, paint_overlay, perform_focus,
};

/// The document handle shared between the editor view and the search widgets.
pub type SharedDocument = Rc<RefCell<Document>>;

/// Widest bar.
const MAX_WIDTH: f32 = 520.0;
/// Distance from the editor's top and right edges.
const MARGIN: f32 = 12.0;
/// Padding inside the bar.
const PAD: f32 = 4.0;
/// Gap between neighbouring chips.
const GAP: f32 = 2.0;
/// Height of one bar row.
const ROW_HEIGHT: f32 = 40.0;
/// Width reserved for the counter text.
const COUNTER_WIDTH: f32 = 72.0;
/// Largest selection (bytes) that seeds the find box when the bar opens.
const MAX_SEED: usize = 200;
/// Side of a toggle chip.
pub const CHIP_SIZE: f32 = 28.0;

#[derive(Debug, Clone)]
enum Face {
    /// Short text, drawn with an accent fill while on.
    Text(String),
    /// A glyph pair; no fill.
    Icons { off: char, on: char },
}

/// A small toggle button (Match case, Whole word, Regex, the replace expander).
#[derive(Debug, Clone)]
pub struct ToggleChip {
    id: WidgetId,
    face: Face,
    name: String,
    on: bool,
    bounds: Rect,
    interaction: Interaction,
    changed: bool,
}

impl ToggleChip {
    fn with_face(face: Face, name: String, on: bool) -> Self {
        Self {
            id: WidgetId::allocate(),
            face,
            name,
            on,
            bounds: Rect::ZERO,
            interaction: Interaction::new(Motion::full()),
            changed: false,
        }
    }

    /// A chip showing `text`; `name` is the accessible name.
    #[must_use]
    pub fn text(text: impl Into<String>, name: impl Into<String>, on: bool) -> Self {
        Self::with_face(Face::Text(text.into()), name.into(), on)
    }

    /// A chip showing the `off` glyph, or the `on` glyph while on (Segoe Fluent Icons).
    #[must_use]
    pub fn icons(off: char, on: char, name: impl Into<String>, is_on: bool) -> Self {
        Self::with_face(Face::Icons { off, on }, name.into(), is_on)
    }

    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.interaction.set_motion(motion);
        self
    }

    #[must_use]
    pub fn is_on(&self) -> bool {
        self.on
    }

    /// Sets the state without reporting a change.
    pub fn set_on(&mut self, on: bool) {
        self.on = on;
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The new state, once after the user (or assistive technology) flipped the chip.
    pub fn take_changed(&mut self) -> Option<bool> {
        std::mem::take(&mut self.changed).then_some(self.on)
    }

    fn flip(&mut self) {
        self.on = !self.on;
        self.changed = true;
    }
}

impl Control for ToggleChip {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for ToggleChip {
    fn measure(&mut self, _cx: &mut LayoutCx, _available: Size) -> Size {
        Size::new(CHIP_SIZE, CHIP_SIZE)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        cx.add_tab_stop();
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        let accent_fill = self.on && matches!(self.face, Face::Text(_));
        let (base, hover, pressed) = if accent_fill {
            (
                tokens.accent,
                tokens.accent.with_alpha(0.9).over(tokens.surface),
                tokens.accent.with_alpha(0.8).over(tokens.surface),
            )
        } else {
            (
                tokens.hover_bg.with_alpha(0.0),
                tokens.hover_bg,
                tokens.pressed_bg,
            )
        };
        canvas.fill_rounded_rect(
            self.bounds,
            CornerRadii::uniform(tokens.metrics.control_radius),
            self.interaction.blend(base, hover, pressed),
        );
        let color = if accent_fill {
            tokens.accent_text_on
        } else {
            tokens.text
        };
        match &self.face {
            Face::Text(text) => {
                let layout = layout_line(cx.text, cx.theme, text, FONT_SIZE);
                draw_centered(canvas, layout.as_ref(), self.bounds, color);
            }
            Face::Icons { off, on } => canvas.draw_icon(
                if self.on { *on } else { *off },
                IconFont::SegoeFluent,
                ICON_SIZE * 0.75,
                color,
                self.bounds.center(),
            ),
        }
        if cx.is_focused() {
            focus_ring::paint(canvas, self.bounds, tokens.metrics.control_radius, cx.theme);
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => {
                let outcome = self.interaction.pointer(cx, self.bounds, event, true);
                if outcome == PointerOutcome::Clicked {
                    self.flip();
                }
                outcome_result(outcome)
            }
            Event::Key(key)
                if key.is_down
                    && cx.is_focused()
                    && matches!(key.vk, vk::SPACE | vk::RETURN)
                    && key.mods.is_empty() =>
            {
                if !key.repeat {
                    self.flip();
                }
                EventResult::handled()
            }
            Event::Focus(_) => {
                cx.invalidate(self.bounds.outset(Insets::uniform(2.0)));
                EventResult::ignored()
            }
            Event::AnimationFrame(now) => {
                if self.interaction.animate(cx, *now) {
                    cx.invalidate(self.bounds);
                }
                EventResult::ignored()
            }
            _ => EventResult::ignored(),
        }
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let mut state = control_state(cx, true);
        state.toggle = Some(if self.on {
            ToggleState::On
        } else {
            ToggleState::Off
        });
        let mut node = AccessNode::new(self.id, Role::ToggleButton, self.name.clone(), self.bounds)
            .with_state(state);
        node.patterns.toggle = true;
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::Toggle => {
                self.flip();
                true
            }
            AccessAction::Focus => perform_focus(cx),
            _ => false,
        }
    }
}

/// What the find bar did that the host must react to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindEvent {
    /// The bar selected this match in the document: reveal hidden markers, unfold, scroll.
    Reveal(TextRange),
    /// Replace or Replace all changed the document.
    Replaced {
        /// Number of replaced matches.
        count: usize,
    },
    /// The bar closed (Esc or the close button); focus returns to the editor.
    Closed,
}

/// The find / replace bar.
#[derive(Debug)]
pub struct FindBar {
    id: WidgetId,
    doc: SharedDocument,
    localizer: Rc<Localizer>,
    visible: bool,
    replace_open: bool,
    expand: ToggleChip,
    find: TextBox,
    case: ToggleChip,
    word: ToggleChip,
    regex: ToggleChip,
    previous: Button,
    next: Button,
    close: Button,
    replace: TextBox,
    replace_one: Button,
    replace_all: Button,
    counter_id: WidgetId,
    counter_rect: Rect,
    panel: Rect,
    searcher: Option<Searcher>,
    matches: Vec<TextRange>,
    current: Option<usize>,
    error: Option<String>,
    version: u64,
    generation: u64,
    focus_request: Option<WidgetId>,
    events: Vec<FindEvent>,
}

impl FindBar {
    /// A hidden bar searching `doc`; every string comes from `localizer`.
    #[must_use]
    pub fn new(doc: SharedDocument, localizer: Rc<Localizer>, clipboard: &SharedClipboard) -> Self {
        let l = localizer.as_ref();
        let chip = |glyph_id: &str, name_id: &str| {
            ToggleChip::text(l.text(glyph_id), l.text(name_id), false)
        };
        let expand = ToggleChip::icons(
            glyph::CHEVRON_RIGHT,
            glyph::CHEVRON_DOWN,
            l.text("search-find-toggle-replace"),
            false,
        );
        let case = chip("search-find-match-case-glyph", "search-find-match-case");
        let word = chip("search-find-whole-word-glyph", "search-find-whole-word");
        let regex = chip("search-find-regex-glyph", "search-find-regex");
        let find = TextBox::new(l.text("search-find-label"), "", Rc::clone(clipboard))
            .with_placeholder(l.text("search-find-placeholder"));
        let replace = TextBox::new(l.text("search-replace-label"), "", Rc::clone(clipboard))
            .with_placeholder(l.text("search-replace-placeholder"));
        let previous = Button::icon(glyph::CHEVRON_UP, l.text("search-find-previous"));
        let next = Button::icon(glyph::CHEVRON_DOWN, l.text("search-find-next"));
        let close = Button::icon(glyph::CLOSE, l.text("search-find-close"));
        let replace_one = Button::new(ButtonKind::Standard, l.text("search-replace-one"));
        let replace_all = Button::new(ButtonKind::Standard, l.text("search-replace-all"));
        let version = doc.borrow().version();
        Self {
            id: WidgetId::allocate(),
            doc,
            localizer: Rc::clone(&localizer),
            visible: false,
            replace_open: false,
            expand,
            find,
            case,
            word,
            regex,
            previous,
            next,
            close,
            replace,
            replace_one,
            replace_all,
            counter_id: WidgetId::allocate(),
            counter_rect: Rect::ZERO,
            panel: Rect::ZERO,
            searcher: None,
            matches: Vec::new(),
            current: None,
            error: None,
            version,
            generation: 0,
            focus_request: None,
            events: Vec::new(),
        }
    }

    /// Applies the motion preference to every animated control of the bar.
    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.expand = self.expand.with_motion(motion);
        self.find = self.find.with_motion(motion);
        self.case = self.case.with_motion(motion);
        self.word = self.word.with_motion(motion);
        self.regex = self.regex.with_motion(motion);
        self.previous = self.previous.with_motion(motion);
        self.next = self.next.with_motion(motion);
        self.close = self.close.with_motion(motion);
        self.replace = self.replace.with_motion(motion);
        self.replace_one = self.replace_one.with_motion(motion);
        self.replace_all = self.replace_all.with_motion(motion);
        self
    }

    /// Searches another document (the active tab changed). A hidden bar searches it when it is
    /// shown.
    pub fn set_document(&mut self, doc: SharedDocument) {
        self.doc = doc;
        if self.visible {
            self.recompute();
        }
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    #[must_use]
    pub fn is_replace_open(&self) -> bool {
        self.replace_open
    }

    /// The icon-only control under `point` and its name, the text of its tooltip: the replace
    /// expander, the three search toggles, previous, next and close.
    #[must_use]
    pub fn tooltip_at(&self, point: Point) -> Option<(Rect, &str)> {
        if !self.visible {
            return None;
        }
        let chips = [&self.expand, &self.case, &self.word, &self.regex]
            .map(|chip| (chip.bounds(), chip.name()));
        let buttons = [&self.previous, &self.next, &self.close]
            .map(|button| (button.bounds(), button.label()));
        chips
            .into_iter()
            .chain(buttons)
            .find(|(bounds, _)| bounds.contains(point))
    }

    /// The bar's bounds within the window (empty while hidden or before layout).
    #[must_use]
    pub fn panel(&self) -> Rect {
        self.panel
    }

    /// The query the controls currently describe.
    #[must_use]
    pub fn query(&self) -> Query {
        Query {
            pattern: self.find.text().to_owned(),
            match_case: self.case.is_on(),
            whole_word: self.word.is_on(),
            regex: self.regex.is_on(),
        }
    }

    /// Every match of the query in the document, left to right (what the editor highlights).
    #[must_use]
    pub fn matches(&self) -> &[TextRange] {
        &self.matches
    }

    /// How many searches the bar has run: [`matches`](Self::matches) only changes when this does.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Index into [`matches`](Self::matches) of the match the counter shows.
    #[must_use]
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// The match the counter shows (drawn stronger than the others).
    #[must_use]
    pub fn current_match(&self) -> Option<TextRange> {
        self.current.map(|index| self.matches[index])
    }

    /// The engine's message when the regular expression is invalid.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The text of the counter: `n/m`, "No results", or the invalid-regex message; empty for an
    /// empty query.
    #[must_use]
    pub fn counter_text(&self) -> String {
        if self.error.is_some() {
            return self.localizer.text("search-find-invalid-regex");
        }
        if self.find.text().is_empty() {
            return String::new();
        }
        match self.current {
            None => self.localizer.text("search-find-no-results"),
            Some(current) => self.localizer.format(
                "search-find-counter",
                &[
                    ("current", Arg::Int(current as i64 + 1)),
                    ("total", Arg::Int(self.matches.len() as i64)),
                ],
            ),
        }
    }

    /// The find box (its text is the pattern).
    #[must_use]
    pub fn find_box(&self) -> &TextBox {
        &self.find
    }

    /// The replace box.
    #[must_use]
    pub fn replace_box(&self) -> &TextBox {
        &self.replace
    }

    /// Types `text` as the pattern (as if typed) and searches. For hosts and tests.
    pub fn set_pattern(&mut self, text: &str) {
        self.find.set_text(text);
        self.query_changed();
    }

    /// Sets the replacement text.
    pub fn set_replacement(&mut self, text: &str) {
        self.replace.set_text(text);
    }

    /// Sets the three option toggles and searches again.
    pub fn set_options(&mut self, match_case: bool, whole_word: bool, regex: bool) {
        self.case.set_on(match_case);
        self.word.set_on(whole_word);
        self.regex.set_on(regex);
        self.query_changed();
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<FindEvent> {
        std::mem::take(&mut self.events)
    }

    /// The widget the host should focus after its next layout pass (the find box), once.
    pub fn take_focus_request(&mut self) -> Option<WidgetId> {
        self.focus_request.take()
    }

    /// True when `id` is the bar or one of its controls.
    #[must_use]
    pub fn owns(&self, id: WidgetId) -> bool {
        id == self.id || id == self.counter_id || self.children().iter().any(|c| c.id() == id)
    }

    /// Shows the bar (`Ctrl+F`, or `Ctrl+H` with `replace`). A single-line selection seeds the
    /// find box; the box's text is selected so typing replaces it.
    pub fn open(&mut self, replace: bool) {
        self.visible = true;
        if replace {
            self.replace_open = true;
            self.expand.set_on(true);
        }
        let seed = {
            let doc = self.doc.borrow();
            let range = doc.primary_selection().range();
            (!range.is_empty() && range.len() <= MAX_SEED)
                .then(|| doc.buffer().slice(range).into_owned())
        };
        if let Some(seed) = seed.filter(|text| !text.contains(['\n', '\r'])) {
            self.find.set_text(seed);
        }
        self.find.select_all();
        self.recompute();
        self.focus_request = Some(self.find.id());
    }

    /// Hides the bar and reports [`FindEvent::Closed`].
    pub fn close(&mut self) {
        if self.visible {
            self.visible = false;
            self.panel = Rect::ZERO;
            self.events.push(FindEvent::Closed);
        }
    }

    /// Recomputes the matches when the document changed and keeps the counter at the caret.
    /// The host calls it before painting.
    pub fn refresh(&mut self) {
        if !self.visible {
            return;
        }
        if self.doc.borrow().version() == self.version {
            let from = self.doc.borrow().primary_selection().start();
            self.current = self.nearest(from);
        } else {
            self.recompute();
        }
    }

    /// Selects the next match after the selection (wrapping). Returns whether there is one.
    pub fn find_next(&mut self) -> bool {
        self.refresh();
        let from = self.doc.borrow().primary_selection().end();
        match self.nearest(from) {
            Some(index) => {
                self.goto(index);
                true
            }
            None => false,
        }
    }

    /// Selects the previous match before the selection (wrapping). Returns whether there is one.
    pub fn find_previous(&mut self) -> bool {
        self.refresh();
        let from = self.doc.borrow().primary_selection().start();
        let before = self.matches.partition_point(|found| found.start < from);
        let index = before.checked_sub(1).or(self.matches.len().checked_sub(1));
        match index {
            Some(index) => {
                self.goto(index);
                true
            }
            None => false,
        }
    }

    /// Replaces the current match with the replacement text and selects the next match. Returns
    /// whether a replacement was made.
    pub fn replace_current(&mut self) -> bool {
        self.refresh();
        let (text, before) = {
            let doc = self.doc.borrow();
            (doc.text(), doc.primary_selection())
        };
        let edit = self
            .searcher
            .as_ref()
            .zip(self.current)
            .and_then(|(searcher, index)| {
                searcher.replace_one(&text, self.replace.text(), self.matches[index].start)
            });
        let Some(edit) = edit else {
            return false;
        };
        let after = edit.new_range().end;
        let tx = Transaction::single(edit, before, Selection::caret(after));
        // The edit was made from the text just searched: the document takes it.
        let applied = self.doc.borrow_mut().apply(tx);
        applied.is_ok_and(|()| {
            self.events.push(FindEvent::Replaced { count: 1 });
            self.query_changed();
            true
        })
    }

    /// Replaces every match in one undo step. Returns the number of replacements.
    pub fn replace_all(&mut self) -> usize {
        self.refresh();
        let (text, before) = {
            let doc = self.doc.borrow();
            (doc.text(), doc.primary_selection())
        };
        let edits = self
            .searcher
            .as_ref()
            .map(|searcher| searcher.replace_all(&text, self.replace.text()))
            .unwrap_or_default();
        if edits.is_empty() {
            return 0;
        }
        let count = edits.len();
        let mut tx = Transaction::new(edits, before, before);
        tx.selection_after = Selection::caret(tx.map_offset(before.head, Assoc::After));
        let replaced = self.doc.borrow_mut().apply(tx).map_or(0, |()| count);
        self.events.push(FindEvent::Replaced { count: replaced });
        self.recompute();
        replaced
    }

    /// The first match starting at or after `from`, wrapping to the first.
    fn nearest(&self, from: usize) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        let index = self.matches.partition_point(|found| found.start < from);
        Some(if index == self.matches.len() {
            0
        } else {
            index
        })
    }

    fn goto(&mut self, index: usize) {
        let range = self.matches[index];
        self.current = Some(index);
        self.doc
            .borrow_mut()
            .set_selections(Selection::from_range(range), &[]);
        self.events.push(FindEvent::Reveal(range));
    }

    /// Searches the document again with the current query.
    fn recompute(&mut self) {
        let query = self.query();
        let (text, version, caret) = {
            let doc = self.doc.borrow();
            (doc.text(), doc.version(), doc.primary_selection().start())
        };
        self.version = version;
        self.generation += 1;
        self.matches.clear();
        self.error = None;
        self.searcher = None;
        match Searcher::new(&query) {
            Ok(searcher) => {
                self.matches = searcher.find_all(&text);
                self.searcher = Some(searcher);
            }
            Err(SearchError::InvalidRegex(message)) => self.error = Some(message),
        }
        self.current = self.nearest(caret);
    }

    /// The query changed: search again and select the first match at or after the caret.
    fn query_changed(&mut self) {
        self.recompute();
        if let Some(index) = self.current {
            self.goto(index);
        }
    }

    fn children(&self) -> Vec<&dyn Control> {
        let mut children: Vec<&dyn Control> = vec![
            &self.expand as &dyn Control,
            &self.find,
            &self.case,
            &self.word,
            &self.regex,
            &self.previous,
            &self.next,
            &self.close,
        ];
        if self.replace_open {
            children.extend([
                &self.replace as &dyn Control,
                &self.replace_one,
                &self.replace_all,
            ]);
        }
        children
    }

    fn children_mut(&mut self) -> Vec<&mut dyn Control> {
        let mut children: Vec<&mut dyn Control> = vec![
            &mut self.expand as &mut dyn Control,
            &mut self.find,
            &mut self.case,
            &mut self.word,
            &mut self.regex,
            &mut self.previous,
            &mut self.next,
            &mut self.close,
        ];
        if self.replace_open {
            children.extend([
                &mut self.replace as &mut dyn Control,
                &mut self.replace_one,
                &mut self.replace_all,
            ]);
        }
        children
    }

    fn forward(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let mut result = EventResult::ignored();
        for child in self.children_mut() {
            let id = child.id();
            let handled = child.event(&mut cx.for_widget(id), e);
            if handled.request_focus {
                cx.focus.focus(id);
            }
            result = result.or(EventResult {
                request_focus: false,
                ..handled
            });
        }
        result
    }

    /// Acts on what the controls recorded while handling an event.
    fn collect(&mut self) {
        let mut changed = self.find.take_changed();
        for chip in [&mut self.case, &mut self.word, &mut self.regex] {
            changed |= chip.take_changed().is_some();
        }
        if changed {
            self.query_changed();
        }
        if let Some(open) = self.expand.take_changed() {
            self.replace_open = open;
        }
        if self.find.take_submitted() {
            self.find_next();
        }
        if self.previous.take_activated() {
            self.find_previous();
        }
        if self.next.take_activated() {
            self.find_next();
        }
        if self.close.take_activated() {
            self.close();
        }
        if self.replace.take_submitted() || self.replace_one.take_activated() {
            self.replace_current();
        }
        if self.replace_all.take_activated() {
            self.replace_all();
        }
    }

    /// Keys of the bar while one of its controls has focus. Returns whether the key was used.
    fn on_key(&mut self, cx: &mut EventCx, key: &KeyEvent) -> bool {
        let current = cx.focus.focused();
        let focused = |id: WidgetId| current == Some(id);
        if !self.children().iter().any(|child| focused(child.id())) {
            return false;
        }
        let plain = key.mods.is_empty();
        let shift_only = key.mods.shift() && !key.mods.ctrl() && !key.mods.alt();
        let ctrl_only = key.mods.ctrl() && !key.mods.shift() && !key.mods.alt();
        match key.vk {
            vk::ESCAPE if plain => self.close(),
            vk::F3 if plain => {
                self.find_next();
            }
            vk::F3 if shift_only => {
                self.find_previous();
            }
            vk::RETURN if shift_only && focused(self.find.id()) => {
                self.find_previous();
            }
            vk::RETURN if ctrl_only && self.replace_open && focused(self.replace.id()) => {
                self.replace_all();
            }
            // Tab and Shift+Tab swap the two fields of the open replace row; the field entered
            // is selected so typing replaces its text.
            vk::TAB if (plain || shift_only) && self.replace_open => {
                let to = if focused(self.find.id()) {
                    &mut self.replace
                } else if focused(self.replace.id()) {
                    &mut self.find
                } else {
                    return false;
                };
                to.select_all();
                cx.focus.focus(to.id());
            }
            _ => return false,
        }
        true
    }

    /// Carries out an accessibility action on the control with id `child`. Returns whether the
    /// control exists and supports it.
    pub fn perform_child(&mut self, cx: &mut EventCx, child: WidgetId, a: &AccessAction) -> bool {
        if !self.visible {
            return false;
        }
        let performed = self
            .children_mut()
            .into_iter()
            .find(|control| control.id() == child)
            .is_some_and(|control| control.perform(&mut cx.for_widget(child), a));
        if performed {
            self.collect();
            cx.invalidate_all();
        }
        performed
    }
}

/// Moves `child` to `rect` (registering its tab stop).
fn place(cx: &mut LayoutCx, child: &mut dyn Control, rect: Rect) {
    let id = child.id();
    child.arrange(&mut cx.for_widget(id), rect);
}

fn button_width(cx: &mut LayoutCx, button: &mut Button) -> f32 {
    let id = button.id();
    button
        .measure(&mut cx.for_widget(id), Size::UNBOUNDED)
        .width
}

impl Control for FindBar {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for FindBar {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        if !self.visible {
            return Size::ZERO;
        }
        let width = if available.width.is_finite() {
            MAX_WIDTH.min((available.width - 2.0 * MARGIN).max(0.0))
        } else {
            MAX_WIDTH
        };
        let rows = if self.replace_open { 2.0 } else { 1.0 };
        Size::new(width, rows * ROW_HEIGHT)
    }

    /// `bounds` is the editor area; the bar floats at its top right.
    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.panel = Rect::ZERO;
        if !self.visible {
            return;
        }
        let width = MAX_WIDTH.min((bounds.width - 2.0 * MARGIN).max(0.0));
        let rows = if self.replace_open { 2.0 } else { 1.0 };
        let panel = Rect::new(
            bounds.right() - MARGIN - width,
            bounds.y + MARGIN,
            width,
            rows * ROW_HEIGHT,
        );
        self.panel = panel;
        let top = panel.y + PAD;
        let chip_y = top + (CONTROL_HEIGHT - CHIP_SIZE) / 2.0;
        let close = Rect::new(
            panel.right() - PAD - CONTROL_HEIGHT,
            top,
            CONTROL_HEIGHT,
            CONTROL_HEIGHT,
        );
        let next = close.translate(-CONTROL_HEIGHT, 0.0);
        let previous = next.translate(-CONTROL_HEIGHT, 0.0);
        self.counter_rect = Rect::new(
            previous.x - COUNTER_WIDTH,
            top,
            COUNTER_WIDTH,
            CONTROL_HEIGHT,
        );
        let regex = Rect::new(
            self.counter_rect.x - CHIP_SIZE,
            chip_y,
            CHIP_SIZE,
            CHIP_SIZE,
        );
        let word = regex.translate(-(CHIP_SIZE + GAP), 0.0);
        let case = word.translate(-(CHIP_SIZE + GAP), 0.0);
        let expand = Rect::new(panel.x + PAD, chip_y, CHIP_SIZE, CHIP_SIZE);
        let find_x = expand.right() + GAP;
        let find = Rect::new(
            find_x,
            top,
            (case.x - GAP - find_x).max(0.0),
            CONTROL_HEIGHT,
        );
        place(cx, &mut self.expand, expand);
        place(cx, &mut self.find, find);
        place(cx, &mut self.case, case);
        place(cx, &mut self.word, word);
        place(cx, &mut self.regex, regex);
        place(cx, &mut self.previous, previous);
        place(cx, &mut self.next, next);
        place(cx, &mut self.close, close);
        if self.replace_open {
            let row = top + ROW_HEIGHT;
            let all_width = button_width(cx, &mut self.replace_all);
            let one_width = button_width(cx, &mut self.replace_one);
            let all = Rect::new(
                panel.right() - PAD - all_width,
                row,
                all_width,
                CONTROL_HEIGHT,
            );
            let one = Rect::new(all.x - GAP - one_width, row, one_width, CONTROL_HEIGHT);
            let replace = Rect::new(find_x, row, (one.x - GAP - find_x).max(0.0), CONTROL_HEIGHT);
            place(cx, &mut self.replace, replace);
            place(cx, &mut self.replace_one, one);
            place(cx, &mut self.replace_all, all);
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.visible || !cx.needs_paint(self.panel) {
            return;
        }
        let tokens = &cx.theme.tokens;
        paint_overlay(canvas, self.panel, tokens.surface, cx.theme);
        for child in self.children() {
            child.paint(&cx.for_widget(child.id()), canvas);
        }
        let counter = self.counter_text();
        if !counter.is_empty() {
            let layout = layout_line(cx.text, cx.theme, &counter, FONT_SIZE);
            let color = if self.error.is_some() || self.current.is_none() {
                tokens.error
            } else {
                tokens.text_muted
            };
            draw_left_centered(
                canvas,
                layout.as_ref(),
                self.counter_rect.x,
                self.counter_rect,
                color,
            );
        }
        if self.error.is_some() {
            canvas.stroke_rounded_rect(
                self.find.bounds().inset(Insets::uniform(1.0)),
                CornerRadii::uniform(tokens.metrics.control_radius),
                tokens.error,
                2.0,
            );
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if !self.visible {
            return EventResult::ignored();
        }
        if let Event::Key(key) = e
            && key.is_down
            && self.on_key(cx, key)
        {
            cx.invalidate_all();
            return EventResult::handled();
        }
        let mut result = self.forward(cx, e);
        self.collect();
        if let Event::Pointer(pointer) = e
            && self.panel.contains(pointer.position)
        {
            result = result.or(EventResult::handled());
        }
        if result.handled {
            cx.invalidate_all();
        }
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let name = self.localizer.text("search-find-bar");
        if !self.visible {
            let mut node = AccessNode::new(self.id, Role::Group, name, Rect::ZERO);
            node.state.offscreen = true;
            out.insert(node);
            return;
        }
        let mut children = Vec::new();
        for child in self.children() {
            let id = child.id();
            child.access(&cx.for_widget(id), out);
            children.push(id);
        }
        if let Some(message) = &self.error
            && let Some(mut node) = out.node(self.find.id()).cloned()
        {
            node.description.clone_from(message);
            out.insert(node);
        }
        // The counter reads right after the option toggles.
        children.insert(5, self.counter_id);
        out.insert(AccessNode::new(
            self.counter_id,
            Role::Text,
            self.counter_text(),
            self.counter_rect,
        ));
        out.insert(AccessNode::new(self.id, Role::Group, name, self.panel).with_children(children));
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}
