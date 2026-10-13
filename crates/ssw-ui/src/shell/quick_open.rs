//! Quick Open (`Ctrl+E`) and the command palette (`Ctrl+Shift+P`), spec §7: one overlay at the top
//! centre of the window with a text box and a result list.
//!
//! The first character of the box selects what is searched ([`QuickOpenMode`]):
//!
//! | text | searches | result |
//! |---|---|---|
//! | `query` | workspace files (fuzzy over relative paths, most recently used first) | [`QuickOpenEvent::OpenFile`] |
//! | `#query` | the headings of the current document | [`QuickOpenEvent::GoToHeading`] |
//! | `>query` | every command with its shortcut ([`CommandPalette`]) | [`QuickOpenEvent::RunCommand`] |
//!
//! `Ctrl+Shift+P` opens the overlay with the `>` prefix ([`QuickOpen::open`] with
//! [`QuickOpenMode::Commands`]); deleting the prefix switches back to files. Matching is the fuzzy
//! matcher of `ssw_core::fuzzy` (case-insensitive, Turkish `i`/`İ`/`ı`/`I` equivalence); matched
//! characters are highlighted in the rows. `Up`/`Down`/`PageUp`/`PageDown` move the selection
//! while the box keeps focus, `Enter` (or a click) activates, `Esc` (or a press outside) closes.
//!
//! The overlay asks for focus with [`QuickOpen::take_focus_request`] because it cannot be focused
//! before the host has laid it out.

use std::rc::Rc;

use ssw_core::command::CommandId;
use ssw_core::fuzzy::FuzzyMatcher;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::text::TextRange;
use ssw_core::toc::Toc;

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::Canvas;
use crate::geometry::{Rect, Size};
use crate::input::{Event, PointerButton, PointerEventKind, vk};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::textbox::TextBox;
use crate::widgets::{
    CONTROL_HEIGHT, Control, FONT_SIZE, SharedClipboard, draw_left_centered, layout_line,
    paint_overlay,
};

use super::palette::{CommandPalette, ShortcutLabels};
use super::picker::{ROW_HEIGHT, Row, RowClick, RowList, RowOutcome, highlight_ranges};

/// Widest panel.
const MAX_WIDTH: f32 = 560.0;
/// Distance from the top of the window.
const TOP: f32 = 48.0;
/// Padding inside the panel.
const PAD: f32 = 8.0;
/// Rows in view at most.
const MAX_ROWS: usize = 8;
/// Most files or headings listed.
const MAX_RESULTS: usize = 200;
/// Text inset of the "no results" message.
const MESSAGE_INSET: f32 = 12.0;

/// What the overlay searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickOpenMode {
    /// Workspace files.
    Files,
    /// Headings of the current document (`#`).
    Headings,
    /// Commands (`>`), the command palette.
    Commands,
}

impl QuickOpenMode {
    /// The prefix that selects the mode (empty for files).
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            QuickOpenMode::Files => "",
            QuickOpenMode::Headings => "#",
            QuickOpenMode::Commands => ">",
        }
    }

    /// The mode a box text selects, and the query that follows the prefix.
    #[must_use]
    pub fn of_text(text: &str) -> (QuickOpenMode, &str) {
        if let Some(query) = text.strip_prefix('#') {
            (QuickOpenMode::Headings, query)
        } else if let Some(query) = text.strip_prefix('>') {
            (QuickOpenMode::Commands, query)
        } else {
            (QuickOpenMode::Files, text)
        }
    }
}

/// What the overlay decided; it closes itself before reporting one of the first three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickOpenEvent {
    /// Open this file (the path exactly as given to [`QuickOpen::set_files`]).
    OpenFile {
        /// The chosen path.
        path: String,
    },
    /// Move the caret to this heading.
    GoToHeading {
        /// Index of the heading in the outline given to [`QuickOpen::set_outline`].
        index: usize,
        /// Source range of the heading block.
        range: TextRange,
    },
    /// Run this command.
    RunCommand(CommandId),
    /// The overlay closed without a choice.
    Closed,
}

/// What a row of the list stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    File(usize),
    Heading(usize),
    Command(CommandId),
}

#[derive(Debug, Clone)]
struct HeadingItem {
    level: u8,
    title: String,
    range: TextRange,
}

/// The Quick Open / command palette overlay.
#[derive(Debug)]
pub struct QuickOpen {
    id: WidgetId,
    localizer: Rc<Localizer>,
    input: TextBox,
    list: RowList,
    palette: CommandPalette,
    matcher: FuzzyMatcher,
    files: Vec<String>,
    recent: Vec<usize>,
    headings: Vec<HeadingItem>,
    titles: Vec<String>,
    hits: Vec<Hit>,
    mode: QuickOpenMode,
    visible: bool,
    panel: Rect,
    message_id: WidgetId,
    message_rect: Rect,
    focus_request: Option<WidgetId>,
    events: Vec<QuickOpenEvent>,
}

impl QuickOpen {
    /// A hidden overlay; labels come from `localizer`, shortcut texts from `shortcuts`.
    #[must_use]
    pub fn new(
        localizer: Rc<Localizer>,
        shortcuts: &dyn ShortcutLabels,
        clipboard: &SharedClipboard,
    ) -> Self {
        let input = TextBox::new(
            localizer.text("search-quick-open-label"),
            "",
            Rc::clone(clipboard),
        )
        .with_placeholder(localizer.text("search-quick-open-placeholder"));
        let list = RowList::new(localizer.text("search-quick-open-results"), MAX_ROWS);
        let palette = CommandPalette::new(&localizer, shortcuts);
        Self {
            id: WidgetId::allocate(),
            localizer,
            input,
            list,
            palette,
            matcher: FuzzyMatcher::new(),
            files: Vec::new(),
            recent: Vec::new(),
            headings: Vec::new(),
            titles: Vec::new(),
            hits: Vec::new(),
            mode: QuickOpenMode::Files,
            visible: false,
            panel: Rect::ZERO,
            message_id: WidgetId::allocate(),
            message_rect: Rect::ZERO,
            focus_request: None,
            events: Vec::new(),
        }
    }

    /// Applies the motion preference to the text box.
    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.input = self.input.with_motion(motion);
        self
    }

    /// The workspace files, as paths relative to the root with `/` separators, and the indices
    /// (into `files`) of the most recently used ones, most recent first.
    pub fn set_files(&mut self, files: Vec<String>, recent: Vec<usize>) {
        self.files = files;
        self.recent = recent;
        self.rebuild();
    }

    /// The headings of the current document.
    pub fn set_outline(&mut self, toc: &Toc) {
        self.headings = toc
            .entries()
            .iter()
            .map(|entry| HeadingItem {
                level: entry.level,
                title: entry.title.clone(),
                range: entry.range,
            })
            .collect();
        self.titles = self
            .headings
            .iter()
            .map(|item| item.title.clone())
            .collect();
        self.rebuild();
    }

    /// Re-reads the shortcut texts (the user changed the keymap).
    pub fn refresh_shortcuts(&mut self, shortcuts: &dyn ShortcutLabels) {
        self.palette.refresh_shortcuts(shortcuts);
        self.rebuild();
    }

    /// Tells the command mode which commands apply now (`enabled` is asked for every command).
    /// The others stay listed, drawn disabled, and choosing one does nothing.
    pub fn set_command_availability(&mut self, enabled: &dyn Fn(CommandId) -> bool) {
        self.palette.set_availability(enabled);
        self.rebuild();
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// What the current box text searches.
    #[must_use]
    pub fn mode(&self) -> QuickOpenMode {
        self.mode
    }

    /// The box text after the mode prefix.
    #[must_use]
    pub fn query(&self) -> &str {
        QuickOpenMode::of_text(self.input.text()).1
    }

    /// The text box.
    #[must_use]
    pub fn input(&self) -> &TextBox {
        &self.input
    }

    /// The result list.
    #[must_use]
    pub fn list(&self) -> &RowList {
        &self.list
    }

    /// The panel's bounds (empty while hidden or before layout).
    #[must_use]
    pub fn panel(&self) -> Rect {
        self.panel
    }

    /// Replaces the box text (as if typed) and searches.
    pub fn set_input(&mut self, text: &str) {
        self.input.set_text(text);
        self.rebuild();
    }

    /// The message shown instead of the list when nothing matches.
    #[must_use]
    pub fn empty_text(&self) -> String {
        let id = match self.mode {
            QuickOpenMode::Files => "search-quick-open-no-files",
            QuickOpenMode::Headings => "search-quick-open-no-headings",
            QuickOpenMode::Commands => "search-quick-open-no-commands",
        };
        self.localizer.text(id)
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<QuickOpenEvent> {
        std::mem::take(&mut self.events)
    }

    /// The widget the host should focus after its next layout pass (the text box), once.
    pub fn take_focus_request(&mut self) -> Option<WidgetId> {
        self.focus_request.take()
    }

    /// True when `id` is the overlay, its box or its list (or one of its rows).
    #[must_use]
    pub fn owns(&self, id: WidgetId) -> bool {
        id == self.id || id == self.message_id || id == self.input.id() || id == self.list.id()
    }

    /// Shows the overlay in `mode` with an empty query (`Ctrl+E`, `Ctrl+Shift+P`).
    pub fn open(&mut self, mode: QuickOpenMode) {
        self.input.set_text(mode.prefix());
        self.visible = true;
        self.rebuild();
        self.focus_request = Some(self.input.id());
    }

    /// Hides the overlay and reports [`QuickOpenEvent::Closed`].
    pub fn close(&mut self) {
        if self.visible {
            self.hide();
            self.events.push(QuickOpenEvent::Closed);
        }
    }

    fn hide(&mut self) {
        self.visible = false;
        self.panel = Rect::ZERO;
    }

    /// Chooses the selected row: the overlay closes and reports the choice. Returns whether a
    /// row was chosen; a disabled command is not (the overlay stays open).
    pub fn activate(&mut self) -> bool {
        let Some(selected) = self.list.selected() else {
            return false;
        };
        let event = match self.hits[selected] {
            Hit::Command(command) if !self.palette.is_enabled(command) => return false,
            Hit::File(index) => QuickOpenEvent::OpenFile {
                path: self.files[index].clone(),
            },
            Hit::Heading(index) => QuickOpenEvent::GoToHeading {
                index,
                range: self.headings[index].range,
            },
            Hit::Command(command) => QuickOpenEvent::RunCommand(command),
        };
        self.hide();
        self.events.push(event);
        true
    }

    /// Searches again for the box text and replaces the list.
    fn rebuild(&mut self) {
        let text = self.input.text().to_owned();
        let (mode, query) = QuickOpenMode::of_text(&text);
        self.mode = mode;
        let found: Vec<(Row, Hit)> = match mode {
            QuickOpenMode::Files => self
                .matcher
                .rank_paths(query, &self.files, &self.recent, MAX_RESULTS)
                .into_iter()
                .map(|ranked| {
                    let path = &self.files[ranked.index];
                    let row = Row {
                        label: path.clone(),
                        highlights: highlight_ranges(path, &ranked.indices),
                        ..Row::default()
                    };
                    (row, Hit::File(ranked.index))
                })
                .collect(),
            QuickOpenMode::Headings => self
                .matcher
                .rank_texts(query, &self.titles, MAX_RESULTS)
                .into_iter()
                .map(|ranked| {
                    let item = &self.headings[ranked.index];
                    let row = Row {
                        prefix: self.localizer.format(
                            "search-quick-open-heading-level",
                            &[("level", Arg::Int(i64::from(item.level)))],
                        ),
                        label: item.title.clone(),
                        highlights: highlight_ranges(&item.title, &ranked.indices),
                        ..Row::default()
                    };
                    (row, Hit::Heading(ranked.index))
                })
                .collect(),
            QuickOpenMode::Commands => self
                .palette
                .rank(query, usize::MAX)
                .into_iter()
                .map(|entry| (entry.to_row(), Hit::Command(entry.command)))
                .collect(),
        };
        let (rows, hits): (Vec<Row>, Vec<Hit>) = found.into_iter().unzip();
        self.hits = hits;
        self.list.set_rows(rows);
    }

    fn panel_height(&self) -> f32 {
        let list = if self.list.is_empty() {
            ROW_HEIGHT
        } else {
            self.list.desired_height()
        };
        3.0 * PAD + CONTROL_HEIGHT + list
    }

    /// Carries out an accessibility action on the box or a row (`child` is the node id).
    pub fn perform_child(&mut self, cx: &mut EventCx, child: WidgetId, a: &AccessAction) -> bool {
        if !self.visible {
            return false;
        }
        let performed = if child == self.input.id() {
            self.input.perform(&mut cx.for_widget(child), a)
        } else {
            match self.list.perform_item(child, a) {
                Some(RowOutcome::Click(RowClick::Item(_))) => self.activate(),
                Some(_) => true,
                None => false,
            }
        };
        if self.input.take_changed() {
            self.rebuild();
        }
        if performed {
            cx.invalidate_all();
        }
        performed
    }

    fn pointer(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let Event::Pointer(pointer) = e else {
            return EventResult::ignored();
        };
        if pointer.kind == PointerEventKind::Down && !self.panel.contains(pointer.position) {
            self.close();
            cx.invalidate_all();
            return EventResult::ignored();
        }
        let in_list = self.list.bounds().contains(pointer.position);
        match pointer.kind {
            PointerEventKind::Move => {
                self.list.request_cursor(cx, pointer.position);
                let changed = if in_list {
                    self.list.hover_at(pointer.position)
                } else {
                    self.list.clear_hover()
                };
                if changed {
                    cx.invalidate(self.panel);
                }
            }
            PointerEventKind::Leave => {
                if self.list.clear_hover() {
                    cx.invalidate(self.panel);
                }
            }
            PointerEventKind::Down if pointer.button == Some(PointerButton::Left) && in_list => {
                if let Some(RowClick::Item(_)) = self.list.click(pointer.position) {
                    self.activate();
                }
                cx.invalidate_all();
                return EventResult::handled();
            }
            _ => {}
        }
        EventResult::ignored()
    }
}

impl Control for QuickOpen {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for QuickOpen {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        if !self.visible {
            return Size::ZERO;
        }
        let width = if available.width.is_finite() {
            MAX_WIDTH.min((available.width - 2.0 * PAD).max(0.0))
        } else {
            MAX_WIDTH
        };
        Size::new(width, self.panel_height())
    }

    /// `bounds` is the window area; the overlay sits at its top centre.
    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.panel = Rect::ZERO;
        if !self.visible {
            return;
        }
        let width = MAX_WIDTH.min((bounds.width - 2.0 * PAD).max(0.0));
        let panel = Rect::new(
            bounds.x + (bounds.width - width) / 2.0,
            bounds.y + TOP,
            width,
            self.panel_height(),
        );
        self.panel = panel;
        let inner = (panel.width - 2.0 * PAD).max(0.0);
        let input = Rect::new(panel.x + PAD, panel.y + PAD, inner, CONTROL_HEIGHT);
        let list_height = panel.bottom() - PAD - (input.bottom() + PAD);
        let list = Rect::new(input.x, input.bottom() + PAD, inner, list_height);
        self.message_rect = list;
        self.list.arrange(list);
        let id = self.input.id();
        self.input.arrange(&mut cx.for_widget(id), input);
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.visible || !cx.needs_paint(self.panel) {
            return;
        }
        let tokens = &cx.theme.tokens;
        paint_overlay(canvas, self.panel, tokens.surface, cx.theme);
        self.input.paint(&cx.for_widget(self.input.id()), canvas);
        if self.list.is_empty() {
            let message = layout_line(cx.text, cx.theme, &self.empty_text(), FONT_SIZE);
            draw_left_centered(
                canvas,
                message.as_ref(),
                self.message_rect.x + MESSAGE_INSET,
                self.message_rect,
                tokens.text_muted,
            );
        } else {
            self.list.paint(&cx.for_widget(self.list.id()), canvas);
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if !self.visible {
            return EventResult::ignored();
        }
        let id = self.input.id();
        if let Event::Key(key) = e
            && key.is_down
            && key.mods.is_empty()
            && cx.focus.is_focused(id)
        {
            match key.vk {
                vk::ESCAPE => {
                    self.close();
                    cx.invalidate_all();
                    return EventResult::handled();
                }
                vk::RETURN => {
                    self.activate();
                    cx.invalidate_all();
                    return EventResult::handled();
                }
                vk::UP | vk::DOWN | vk::PAGE_UP | vk::PAGE_DOWN => {
                    // The guard admits unmodified keys only, and those always answer `Moved`.
                    self.list.key(key);
                    cx.invalidate(self.panel);
                    return EventResult::handled();
                }
                _ => {}
            }
        }
        if let Event::Wheel(wheel) = e
            && self.list.wheel(wheel)
        {
            cx.invalidate(self.panel);
            return EventResult::handled();
        }
        let routed = self.pointer(cx, e);
        if routed.handled || !self.visible {
            return routed;
        }
        let mut result = self.input.event(&mut cx.for_widget(id), e);
        if result.request_focus {
            cx.focus.focus(id);
            result.request_focus = false;
        }
        if self.input.take_changed() {
            self.rebuild();
        }
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
        let name = self.localizer.text("search-quick-open-label");
        if !self.visible {
            let mut node = AccessNode::new(self.id, Role::Group, name, Rect::ZERO);
            node.state.offscreen = true;
            out.insert(node);
            return;
        }
        let input_id = self.input.id();
        self.input.access(&cx.for_widget(input_id), out);
        let mut children = vec![input_id];
        if self.list.is_empty() {
            out.insert(AccessNode::new(
                self.message_id,
                Role::Text,
                self.empty_text(),
                self.message_rect,
            ));
            children.push(self.message_id);
        } else {
            self.list.access(&cx.for_widget(self.list.id()), out);
            children.push(self.list.id());
        }
        out.insert(AccessNode::new(self.id, Role::Group, name, self.panel).with_children(children));
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}
