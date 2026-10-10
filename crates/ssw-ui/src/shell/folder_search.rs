//! The "Search in folder" sidebar panel (`Ctrl+Shift+F`, spec §7): a query box with Match case /
//! Whole word / Regex toggles, a status line and the results grouped by file with line previews
//! and highlighted matches.
//!
//! The panel does not search. It reports what the user asked for ([`FolderSearchEvent::Search`]),
//! the app runs `ssw_io::folder_search` on a worker and hands the matches back with
//! [`FolderSearchPanel::set_results`] (the types mirror `ssw_io::folder_search::{FileMatches,
//! LineMatch}` so the adapter is a field copy). Clicking a match, or `Enter` on it, reports
//! [`FolderSearchEvent::Open`] with the file, line and the byte range of the first match in the
//! line. A file header collapses and expands its matches (click, `Enter`, `Left` / `Right`).
//!
//! Keyboard: `Enter` in the box searches; `Down` moves into the results, `Up` on the first row
//! moves back to the box. A regular expression that does not compile is rejected locally
//! (red outline, "Invalid regular expression") without asking the app to search.
//!
//! Like the other overlays the panel asks for focus with [`FolderSearchPanel::take_focus_request`].

use std::ops::Range;
use std::rc::Rc;

use ssw_core::i18n::{Arg, Localizer};
use ssw_core::search::{Query, SearchError, Searcher};

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::Canvas;
use crate::geometry::{CornerRadii, Insets, Rect, Size};
use crate::input::{Event, PointerButton, PointerEventKind, vk};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::textbox::TextBox;
use crate::widgets::{
    CONTROL_HEIGHT, Control, FONT_SIZE, SharedClipboard, draw_left_centered, layout_line,
};

use super::find::{CHIP_SIZE, ToggleChip};
use super::picker::{Row, RowClick, RowKey, RowList, RowOutcome};

/// Padding around the controls.
const PAD: f32 = 8.0;
/// Gap between the toggle chips.
const GAP: f32 = 2.0;
/// Height of the status line.
const STATUS_HEIGHT: f32 = 20.0;

/// One matching line (mirrors `ssw_io::folder_search::LineMatch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSearchLine {
    /// 1-based line number.
    pub line: u64,
    /// Byte offset of the first match in the line.
    pub column: usize,
    /// Byte ranges of every match inside `preview`.
    pub ranges: Vec<Range<usize>>,
    /// The line without its terminator.
    pub preview: String,
}

/// The matches of one file (mirrors `ssw_io::folder_search::FileMatches`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSearchFile {
    /// The path as shown (relative to the workspace root).
    pub path: String,
    pub matches: Vec<FolderSearchLine>,
}

/// Totals of a finished search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FolderSearchSummary {
    /// The search stopped at its match limit.
    pub truncated: bool,
}

/// What to search for (the app maps it to `ssw_io::folder_search::SearchOptions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSearchRequest {
    pub pattern: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// A match the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSearchHit {
    /// The file's path as given to [`FolderSearchPanel::set_results`].
    pub path: String,
    /// 1-based line number.
    pub line: u64,
    /// Byte range of the first match in the line.
    pub range: Range<usize>,
}

/// What the panel asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderSearchEvent {
    /// Run this search (cancel a running one first).
    Search(FolderSearchRequest),
    /// The query was emptied: cancel a running search; the results are gone.
    Cleared,
    /// Open the file at the match.
    Open(FolderSearchHit),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Idle,
    Searching,
    Done(FolderSearchSummary),
    Invalid(String),
}

/// What a result row stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowRef {
    File(usize),
    Line(usize, usize),
}

/// The sidebar panel.
#[derive(Debug)]
pub struct FolderSearchPanel {
    id: WidgetId,
    localizer: Rc<Localizer>,
    input: TextBox,
    case: ToggleChip,
    word: ToggleChip,
    regex: ToggleChip,
    list: RowList,
    files: Vec<FolderSearchFile>,
    collapsed: Vec<bool>,
    refs: Vec<RowRef>,
    state: State,
    status_id: WidgetId,
    status_rect: Rect,
    bounds: Rect,
    focus_request: Option<WidgetId>,
    events: Vec<FolderSearchEvent>,
}

impl FolderSearchPanel {
    /// An empty panel; every string comes from `localizer`.
    #[must_use]
    pub fn new(localizer: Rc<Localizer>, clipboard: &SharedClipboard) -> Self {
        let l = localizer.as_ref();
        let input = TextBox::new(
            l.text("search-folder-query-label"),
            "",
            Rc::clone(clipboard),
        )
        .with_placeholder(l.text("search-folder-placeholder"));
        let chip = |glyph_id: &str, name_id: &str| {
            ToggleChip::text(l.text(glyph_id), l.text(name_id), false)
        };
        let case = chip("search-find-match-case-glyph", "search-find-match-case");
        let word = chip("search-find-whole-word-glyph", "search-find-whole-word");
        let regex = chip("search-find-regex-glyph", "search-find-regex");
        let list = RowList::new(l.text("search-folder-results"), usize::MAX);
        Self {
            id: WidgetId::allocate(),
            localizer: Rc::clone(&localizer),
            input,
            case,
            word,
            regex,
            list,
            files: Vec::new(),
            collapsed: Vec::new(),
            refs: Vec::new(),
            state: State::Idle,
            status_id: WidgetId::allocate(),
            status_rect: Rect::ZERO,
            bounds: Rect::ZERO,
            focus_request: None,
            events: Vec::new(),
        }
    }

    /// Applies the motion preference to the animated controls.
    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.input = self.input.with_motion(motion);
        self.case = self.case.with_motion(motion);
        self.word = self.word.with_motion(motion);
        self.regex = self.regex.with_motion(motion);
        self
    }

    /// The panel's bounds (the sidebar area it was arranged in).
    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The query box.
    #[must_use]
    pub fn input(&self) -> &TextBox {
        &self.input
    }

    /// The result list.
    #[must_use]
    pub fn list(&self) -> &RowList {
        &self.list
    }

    /// The files of the current results.
    #[must_use]
    pub fn files(&self) -> &[FolderSearchFile] {
        &self.files
    }

    /// Whether the matches of file `index` are hidden.
    #[must_use]
    pub fn is_collapsed(&self, index: usize) -> bool {
        self.collapsed.get(index).copied().unwrap_or(false)
    }

    /// The query the controls describe.
    #[must_use]
    pub fn request(&self) -> FolderSearchRequest {
        FolderSearchRequest {
            pattern: self.input.text().to_owned(),
            case_sensitive: self.case.is_on(),
            whole_word: self.word.is_on(),
            regex: self.regex.is_on(),
        }
    }

    /// Whether the query is a regular expression that does not compile.
    #[must_use]
    pub fn is_invalid(&self) -> bool {
        matches!(self.state, State::Invalid(_))
    }

    /// The status line: "Searching…", the totals, "No results found" or the invalid-regex message.
    #[must_use]
    pub fn status_text(&self) -> String {
        let l = &self.localizer;
        match &self.state {
            State::Idle => String::new(),
            State::Searching => l.text("search-folder-searching"),
            State::Invalid(_) => l.text("search-folder-invalid-regex"),
            State::Done(_) if self.files.is_empty() => l.text("search-folder-no-results"),
            State::Done(summary) => {
                let total: usize = self.files.iter().map(|file| file.matches.len()).sum();
                let matches = l.format(
                    "search-folder-matches",
                    &[("matches", Arg::Int(total as i64))],
                );
                if summary.truncated {
                    return l.format(
                        "search-folder-truncated",
                        &[("matches", Arg::Str(&matches))],
                    );
                }
                let files = l.format(
                    "search-folder-files",
                    &[("files", Arg::Int(self.files.len() as i64))],
                );
                l.format(
                    "search-folder-summary",
                    &[("matches", Arg::Str(&matches)), ("files", Arg::Str(&files))],
                )
            }
        }
    }

    /// Puts `text` in the query box (a selection seeds it when the panel opens).
    pub fn set_query(&mut self, text: &str) {
        self.input.set_text(text);
    }

    /// Sets the three option toggles.
    pub fn set_options(&mut self, case_sensitive: bool, whole_word: bool, regex: bool) {
        self.case.set_on(case_sensitive);
        self.word.set_on(whole_word);
        self.regex.set_on(regex);
    }

    /// Asks the host to focus the query box and selects its text (`Ctrl+Shift+F`).
    pub fn open(&mut self) {
        self.input.select_all();
        self.focus_request = Some(self.input.id());
    }

    /// The widget the host should focus after its next layout pass, once.
    pub fn take_focus_request(&mut self) -> Option<WidgetId> {
        self.focus_request.take()
    }

    /// True when `id` is the panel or one of its controls (including result rows).
    #[must_use]
    pub fn owns(&self, id: WidgetId) -> bool {
        id == self.id
            || id == self.status_id
            || id == self.input.id()
            || id == self.case.id()
            || id == self.word.id()
            || id == self.regex.id()
            || id == self.list.id()
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<FolderSearchEvent> {
        std::mem::take(&mut self.events)
    }

    /// Starts a search for the current query. An empty query clears the results; a regular
    /// expression that does not compile is shown as invalid. Returns whether a search was
    /// requested.
    pub fn submit(&mut self) -> bool {
        let request = self.request();
        if request.pattern.is_empty() {
            self.clear();
            return false;
        }
        let query = Query {
            pattern: request.pattern.clone(),
            match_case: request.case_sensitive,
            whole_word: request.whole_word,
            regex: request.regex,
        };
        if let Err(SearchError::InvalidRegex(message)) = Searcher::new(&query) {
            self.state = State::Invalid(message);
            self.set_files(Vec::new());
            return false;
        }
        self.state = State::Searching;
        self.events.push(FolderSearchEvent::Search(request));
        true
    }

    /// Empties the results and asks the app to cancel a running search.
    pub fn clear(&mut self) {
        self.state = State::Idle;
        self.set_files(Vec::new());
        self.events.push(FolderSearchEvent::Cleared);
    }

    /// Shows the matches of a finished search.
    pub fn set_results(&mut self, files: Vec<FolderSearchFile>, summary: FolderSearchSummary) {
        self.state = State::Done(summary);
        self.set_files(files);
    }

    fn set_files(&mut self, files: Vec<FolderSearchFile>) {
        self.collapsed = vec![false; files.len()];
        self.files = files;
        self.rebuild(false);
    }

    /// Rebuilds the rows from the files and their collapsed flags.
    fn rebuild(&mut self, keep_position: bool) {
        let l = &self.localizer;
        let mut rows = Vec::new();
        let mut refs = Vec::new();
        for (file_index, file) in self.files.iter().enumerate() {
            let collapsed = self.collapsed[file_index];
            let count = l.format(
                "search-folder-matches",
                &[("matches", Arg::Int(file.matches.len() as i64))],
            );
            rows.push(Row {
                label: file.path.clone(),
                detail: file.matches.len().to_string(),
                header: true,
                expanded: Some(!collapsed),
                name: Some(l.format(
                    "search-folder-file-header",
                    &[
                        ("path", Arg::Str(&file.path)),
                        ("matches", Arg::Str(&count)),
                    ],
                )),
                ..Row::default()
            });
            refs.push(RowRef::File(file_index));
            if collapsed {
                continue;
            }
            for (line_index, line) in file.matches.iter().enumerate() {
                let trimmed = line.preview.trim_start();
                let skipped = line.preview.len() - trimmed.len();
                rows.push(Row {
                    prefix: line.line.to_string(),
                    label: trimmed.to_owned(),
                    highlights: line
                        .ranges
                        .iter()
                        .map(|range| {
                            range.start.saturating_sub(skipped)..range.end.saturating_sub(skipped)
                        })
                        .filter(|range| range.start < range.end)
                        .collect(),
                    name: Some(l.format(
                        "search-folder-line-name",
                        &[
                            ("path", Arg::Str(&file.path)),
                            ("line", Arg::Int(line.line as i64)),
                            ("preview", Arg::Str(trimmed)),
                        ],
                    )),
                    ..Row::default()
                });
                refs.push(RowRef::Line(file_index, line_index));
            }
        }
        self.refs = refs;
        if keep_position {
            self.list.update_rows(rows);
        } else {
            self.list.set_rows(rows);
        }
    }

    /// Collapses or expands the matches of file `index`.
    pub fn toggle_file(&mut self, index: usize) {
        if let Some(flag) = self.collapsed.get_mut(index) {
            *flag = !*flag;
            self.rebuild(true);
        }
    }

    /// Opens the match on row `row`, or toggles the file when it is a header. Returns whether
    /// anything happened.
    pub fn activate_row(&mut self, row: usize) -> bool {
        match self.refs.get(row).copied() {
            Some(RowRef::File(file)) => {
                self.toggle_file(file);
                true
            }
            Some(RowRef::Line(file, line)) => {
                let found = &self.files[file];
                let matched = &found.matches[line];
                self.events.push(FolderSearchEvent::Open(FolderSearchHit {
                    path: found.path.clone(),
                    line: matched.line,
                    range: matched.ranges.first().cloned().unwrap_or(0..0),
                }));
                true
            }
            None => false,
        }
    }

    /// Acts on what the toggles and the box recorded.
    fn collect(&mut self) {
        let mut changed = false;
        for chip in [&mut self.case, &mut self.word, &mut self.regex] {
            changed |= chip.take_changed().is_some();
        }
        if changed && !self.input.text().is_empty() {
            self.submit();
        }
        if self.input.take_submitted() {
            self.submit();
        }
        self.input.take_changed();
    }

    fn list_key(&mut self, cx: &mut EventCx, key: &crate::input::KeyEvent) -> EventResult {
        if !key.mods.is_empty() {
            return EventResult::ignored();
        }
        let selected = self.list.selected();
        match key.vk {
            vk::UP if selected == Some(0) => {
                cx.focus.focus(self.input.id());
            }
            vk::LEFT | vk::RIGHT => {
                let want_collapsed = key.vk == vk::LEFT;
                if let Some(RowRef::File(file)) =
                    selected.and_then(|row| self.refs.get(row).copied())
                    && self.collapsed[file] != want_collapsed
                {
                    self.toggle_file(file);
                }
            }
            _ => match self.list.key(key) {
                RowKey::Activate(row) => {
                    self.activate_row(row);
                }
                RowKey::Moved => {}
                RowKey::Ignored => return EventResult::ignored(),
            },
        }
        cx.invalidate(self.bounds);
        EventResult::handled()
    }

    /// Carries out an accessibility action on a control or a row (`child` is the node id).
    pub fn perform_child(&mut self, cx: &mut EventCx, child: WidgetId, a: &AccessAction) -> bool {
        let id = child;
        let performed = if id == self.input.id() {
            self.input.perform(&mut cx.for_widget(id), a)
        } else if id == self.case.id() {
            self.case.perform(&mut cx.for_widget(id), a)
        } else if id == self.word.id() {
            self.word.perform(&mut cx.for_widget(id), a)
        } else if id == self.regex.id() {
            self.regex.perform(&mut cx.for_widget(id), a)
        } else {
            match self.list.perform_item(id, a) {
                Some(RowOutcome::Click(RowClick::Item(row) | RowClick::Header(row))) => {
                    self.activate_row(row)
                }
                Some(RowOutcome::Selected) => true,
                None => false,
            }
        };
        if performed {
            self.collect();
            cx.invalidate(self.bounds);
        }
        performed
    }
}

impl Control for FolderSearchPanel {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for FolderSearchPanel {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        Size::new(
            if available.width.is_finite() {
                available.width
            } else {
                260.0
            },
            if available.height.is_finite() {
                available.height
            } else {
                400.0
            },
        )
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        let inner = (bounds.width - 2.0 * PAD).max(0.0);
        let input = Rect::new(bounds.x + PAD, bounds.y + PAD, inner, CONTROL_HEIGHT);
        let chips_y = input.bottom() + PAD;
        let case = Rect::new(input.x, chips_y, CHIP_SIZE, CHIP_SIZE);
        let word = case.translate(CHIP_SIZE + GAP, 0.0);
        let regex = word.translate(CHIP_SIZE + GAP, 0.0);
        self.status_rect = Rect::new(input.x, case.bottom() + PAD, inner, STATUS_HEIGHT);
        let list_top = self.status_rect.bottom() + PAD;
        let list = Rect::new(
            bounds.x,
            list_top,
            bounds.width,
            (bounds.bottom() - list_top).max(0.0),
        );
        for (id, rect, control) in [
            (self.input.id(), input, &mut self.input as &mut dyn Control),
            (self.case.id(), case, &mut self.case),
            (self.word.id(), word, &mut self.word),
            (self.regex.id(), regex, &mut self.regex),
        ] {
            control.arrange(&mut cx.for_widget(id), rect);
        }
        self.list.arrange(list);
        cx.for_widget(self.list.id()).add_tab_stop();
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        for control in [
            &self.input as &dyn Control,
            &self.case,
            &self.word,
            &self.regex,
        ] {
            control.paint(&cx.for_widget(control.id()), canvas);
        }
        let status = self.status_text();
        if !status.is_empty() {
            let layout = layout_line(cx.text, cx.theme, &status, FONT_SIZE);
            let color = if self.is_invalid() {
                tokens.error
            } else {
                tokens.text_muted
            };
            draw_left_centered(
                canvas,
                layout.as_ref(),
                self.status_rect.x,
                self.status_rect,
                color,
            );
        }
        if self.is_invalid() {
            canvas.stroke_rounded_rect(
                self.input.bounds().inset(Insets::uniform(1.0)),
                CornerRadii::uniform(tokens.metrics.control_radius),
                tokens.error,
                2.0,
            );
        }
        self.list.paint(&cx.for_widget(self.list.id()), canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let input_id = self.input.id();
        let list_id = self.list.id();
        if let Event::Key(key) = e
            && key.is_down
        {
            if cx.focus.is_focused(list_id) {
                return self.list_key(cx, key);
            }
            if cx.focus.is_focused(input_id)
                && key.mods.is_empty()
                && key.vk == vk::DOWN
                && !self.list.is_empty()
            {
                cx.focus.focus(list_id);
                cx.invalidate(self.bounds);
                return EventResult::handled();
            }
        }
        if let Event::Wheel(wheel) = e
            && self.list.wheel(wheel)
        {
            cx.invalidate(self.bounds);
            return EventResult::handled();
        }
        let mut result = EventResult::ignored();
        for control in [
            &mut self.input as &mut dyn Control,
            &mut self.case,
            &mut self.word,
            &mut self.regex,
        ] {
            let id = control.id();
            let handled = control.event(&mut cx.for_widget(id), e);
            if handled.request_focus {
                cx.focus.focus(id);
            }
            result = result.or(EventResult {
                request_focus: false,
                ..handled
            });
        }
        if let Event::Pointer(pointer) = e {
            let in_list = self.list.bounds().contains(pointer.position);
            match pointer.kind {
                PointerEventKind::Move => {
                    let changed = if in_list {
                        self.list.hover_at(pointer.position)
                    } else {
                        self.list.clear_hover()
                    };
                    if changed {
                        cx.invalidate(self.bounds);
                    }
                }
                PointerEventKind::Leave => {
                    if self.list.clear_hover() {
                        cx.invalidate(self.bounds);
                    }
                }
                PointerEventKind::Down
                    if pointer.button == Some(PointerButton::Left) && in_list =>
                {
                    cx.focus.focus(list_id);
                    if let Some(click) = self.list.click(pointer.position) {
                        let (RowClick::Item(row) | RowClick::Header(row)) = click;
                        self.activate_row(row);
                    }
                    result = result.or(EventResult::handled());
                }
                _ => {}
            }
        }
        self.collect();
        if result.handled {
            cx.invalidate(self.bounds);
        }
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let mut children = Vec::new();
        for control in [
            &self.input as &dyn Control,
            &self.case,
            &self.word,
            &self.regex,
        ] {
            let id = control.id();
            control.access(&cx.for_widget(id), out);
            children.push(id);
        }
        if let State::Invalid(message) = &self.state
            && let Some(mut node) = out.node(self.input.id()).cloned()
        {
            node.description.clone_from(message);
            out.insert(node);
        }
        out.insert(AccessNode::new(
            self.status_id,
            Role::Text,
            self.status_text(),
            self.status_rect,
        ));
        children.push(self.status_id);
        self.list.access(&cx.for_widget(self.list.id()), out);
        children.push(self.list.id());
        out.insert(
            AccessNode::new(
                self.id,
                Role::Group,
                self.localizer.text("search-folder-title"),
                self.bounds,
            )
            .with_children(children),
        );
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}
