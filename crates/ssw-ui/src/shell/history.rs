//! The version history panel (spec §4.5): the saved versions of a document, the changes between
//! the selected version and the current text, and Restore.
//!
//! The panel owns no data. The app lists the snapshots with [`HistoryPanel::set_entries`]; when
//! the user selects one the panel emits [`HistoryEvent::Selected`], the app diffs that version
//! against the document (`ssw_io::diff::diff_lines`, re-expressed as [`DiffHunk`]s) and answers
//! with [`HistoryPanel::set_diff`]. Restore emits [`HistoryEvent::Restore`]; replacing the
//! document (and snapshotting it first, spec §4.5) is the app's job.

use std::fmt;
use std::rc::Rc;

use ssw_core::i18n::{Arg, Localizer};

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::Canvas;
use crate::geometry::{Point, Rect, Size};
use crate::input::{Event, vk};
use crate::text::{FontWeight, TextStyle};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::button::{Button, ButtonKind};
use crate::widgets::list::{ListEvent, ListView, SelectionMode};
use crate::widgets::scroll::{Axis, BAR_WIDTH, ScrollBar};
use crate::widgets::{CONTROL_HEIGHT, Control, FONT_SIZE, ui_style};

use super::settings::form::forward;

/// Height of the title row.
const HEADER_HEIGHT: f32 = 48.0;
/// Height of the row with the Restore button.
const FOOTER_HEIGHT: f32 = 56.0;
/// Height of the caption above the changes.
const CAPTION_HEIGHT: f32 = 28.0;
/// Padding at the panel's edges.
const PAD: f32 = 16.0;
/// Gap between the stacked parts.
const GAP: f32 = 8.0;
/// Em size of the title.
const TITLE_SIZE: f32 = 16.0;
/// Em size of the diff text.
const DIFF_SIZE: f32 = 13.0;
/// Vertical padding of a diff line.
const LINE_PAD: f32 = 2.0;
/// Height of a hunk header row.
const HUNK_HEIGHT: f32 = 24.0;
/// Gap between the diff gutters and the text.
const GUTTER_GAP: f32 = 6.0;
/// Width of the `+` / `-` marker column.
const MARKER_WIDTH: f32 = 14.0;
/// Opacity of the added / removed line backgrounds.
const LINE_TINT: f32 = 0.16;
/// Shortest and tallest version list.
const LIST_HEIGHT: (f32, f32) = (96.0, 240.0);

/// One saved version of the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// The snapshot id the app uses in [`HistoryEvent`]s.
    pub id: i64,
    /// When the version was saved, already formatted for the user's locale.
    pub time: String,
    /// Size of the text in bytes.
    pub size: usize,
}

/// What a line of a diff did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// Present in both versions.
    Context,
    /// Only in the current document.
    Added,
    /// Only in the selected version.
    Removed,
}

/// One line of a [`DiffHunk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// What the line did.
    pub kind: DiffLineKind,
    /// 0-based line in the selected version (`Context`, `Removed`).
    pub old_line: Option<usize>,
    /// 0-based line in the current document (`Context`, `Added`).
    pub new_line: Option<usize>,
    /// The line without its line break.
    pub text: String,
}

/// A group of changes with their context lines (the shape of `ssw_io::diff::Hunk`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// Index of the first old line covered.
    pub old_start: usize,
    /// Number of old lines covered.
    pub old_count: usize,
    /// Index of the first new line covered.
    pub new_start: usize,
    /// Number of new lines covered.
    pub new_count: usize,
    /// The lines in order.
    pub lines: Vec<DiffLine>,
}

/// Something the app has to react to; drained with [`HistoryPanel::take_events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryEvent {
    /// The user selected this version: answer with [`HistoryPanel::set_diff`].
    Selected(i64),
    /// The user asked to restore this version.
    Restore(i64),
    /// The user closed the panel.
    Close,
}

/// What the changes area shows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Diff {
    /// No version is selected.
    None,
    /// A version is selected and its comparison has been requested.
    Loading,
    Ready(Vec<DiffHunk>),
}

/// One row of the flattened diff: a hunk header or one of its lines.
#[derive(Debug, Clone, Copy)]
struct DiffRow {
    hunk: usize,
    line: Option<usize>,
}

/// Geometry of the rows, rebuilt when the diff or the width changes.
#[derive(Debug, Default)]
struct RowMetrics {
    /// Top of every row and, last, the total height.
    tops: Vec<f32>,
    /// Width of one line-number gutter.
    number_width: f32,
    /// Width available to the text of a line.
    text_width: f32,
    /// The width the metrics were computed for; `None` when stale.
    for_width: Option<f32>,
}

/// The version history panel.
pub struct HistoryPanel {
    id: WidgetId,
    loc: Rc<Localizer>,
    motion: Motion,
    entries: Vec<HistoryEntry>,
    selected: Option<i64>,
    diff: Diff,
    rows: Vec<DiffRow>,
    row_ids: Vec<WidgetId>,
    metrics: RowMetrics,
    list: ListView,
    restore: Button,
    close: Button,
    bar: ScrollBar,
    diff_id: WidgetId,
    message_id: WidgetId,
    bounds: Rect,
    header_rect: Rect,
    list_rect: Rect,
    caption_rect: Rect,
    diff_rect: Rect,
    events: Vec<HistoryEvent>,
}

impl fmt::Debug for HistoryPanel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HistoryPanel")
            .field("id", &self.id)
            .field("entries", &self.entries.len())
            .field("selected", &self.selected)
            .field("diff", &self.diff)
            .finish_non_exhaustive()
    }
}

/// `1.5 KB`-style size text.
fn size_text(loc: &Localizer, bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value < KB {
        loc.format(
            "history-size-bytes",
            &[("value", Arg::Str(&bytes.to_string()))],
        )
    } else if value < MB {
        loc.format(
            "history-size-kb",
            &[("value", Arg::Str(&format!("{:.1}", value / KB)))],
        )
    } else {
        loc.format(
            "history-size-mb",
            &[("value", Arg::Str(&format!("{:.1}", value / MB)))],
        )
    }
}

fn mono_style(font: &'static str) -> TextStyle {
    TextStyle::new(font, DIFF_SIZE)
}

impl HistoryPanel {
    /// An empty panel.
    #[must_use]
    pub fn new(loc: Rc<Localizer>, motion: Motion) -> Self {
        let list = ListView::new(loc.text("history-list"), Vec::new(), SelectionMode::Single)
            .with_motion(motion);
        let restore =
            Button::new(ButtonKind::Accent, loc.text("history-restore")).with_motion(motion);
        let close =
            Button::new(ButtonKind::Standard, loc.text("history-close")).with_motion(motion);
        let bar = ScrollBar::new(Axis::Vertical, loc.text("history-diff")).with_motion(motion);
        let mut panel = Self {
            id: WidgetId::allocate(),
            loc,
            motion,
            entries: Vec::new(),
            selected: None,
            diff: Diff::None,
            rows: Vec::new(),
            row_ids: Vec::new(),
            metrics: RowMetrics::default(),
            list,
            restore,
            close,
            bar,
            diff_id: WidgetId::allocate(),
            message_id: WidgetId::allocate(),
            bounds: Rect::ZERO,
            header_rect: Rect::ZERO,
            list_rect: Rect::ZERO,
            caption_rect: Rect::ZERO,
            diff_rect: Rect::ZERO,
            events: Vec::new(),
        };
        panel.restore.set_enabled(false);
        panel
    }

    /// The motion preference the panel was built with.
    #[must_use]
    pub fn motion(&self) -> Motion {
        self.motion
    }

    /// The control that takes the keyboard focus when the panel opens (the list of versions).
    #[must_use]
    pub fn focus_id(&self) -> WidgetId {
        self.list.id()
    }

    /// Whether `id` is the panel or one of its controls (for focus and pointer capture checks).
    #[must_use]
    pub fn owns(&self, id: WidgetId) -> bool {
        [
            self.id,
            self.list.id(),
            self.restore.id(),
            self.close.id(),
            self.bar.id(),
        ]
        .contains(&id)
    }

    /// Replaces the list of versions, newest first. The selection stays when its version is
    /// still listed and is cleared otherwise.
    pub fn set_entries(&mut self, entries: Vec<HistoryEntry>) {
        let items = entries
            .iter()
            .map(|entry| {
                self.loc.format(
                    "history-entry",
                    &[
                        ("time", Arg::Str(&entry.time)),
                        ("size", Arg::Str(&size_text(&self.loc, entry.size))),
                    ],
                )
            })
            .collect();
        self.list.set_items(items);
        self.entries = entries;
        let position = self
            .selected
            .and_then(|id| self.entries.iter().position(|entry| entry.id == id));
        match position {
            Some(index) => self.list.select(index),
            None => self.clear_selection(),
        }
        self.restore.set_enabled(self.selected.is_some());
    }

    /// The listed versions.
    #[must_use]
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// The selected version.
    #[must_use]
    pub fn selected(&self) -> Option<i64> {
        self.selected
    }

    /// Selects the version `id` as if the user had clicked it (so [`HistoryEvent::Selected`]
    /// follows). Unknown ids are ignored.
    pub fn select(&mut self, id: i64) {
        if let Some(index) = self.entries.iter().position(|entry| entry.id == id) {
            self.list.select(index);
            self.choose(id);
        }
    }

    /// Shows the changes between version `id` and the current document. Answers for a version
    /// that is no longer selected are dropped; returns whether the hunks were taken.
    pub fn set_diff(&mut self, id: i64, hunks: Vec<DiffHunk>) -> bool {
        if self.selected != Some(id) {
            return false;
        }
        self.rows = hunks
            .iter()
            .enumerate()
            .flat_map(|(hunk, h)| {
                std::iter::once(DiffRow { hunk, line: None }).chain((0..h.lines.len()).map(
                    move |line| DiffRow {
                        hunk,
                        line: Some(line),
                    },
                ))
            })
            .collect();
        self.row_ids = self.rows.iter().map(|_| WidgetId::allocate()).collect();
        self.diff = Diff::Ready(hunks);
        self.metrics.for_width = None;
        self.bar.model_mut().jump(0.0);
        true
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<HistoryEvent> {
        std::mem::take(&mut self.events)
    }

    fn clear_selection(&mut self) {
        self.selected = None;
        self.diff = Diff::None;
        self.rows.clear();
        self.row_ids.clear();
        self.metrics.for_width = None;
    }

    fn choose(&mut self, id: i64) {
        if self.selected == Some(id) {
            return;
        }
        self.selected = Some(id);
        self.diff = Diff::Loading;
        self.rows.clear();
        self.row_ids.clear();
        self.metrics.for_width = None;
        self.bar.model_mut().jump(0.0);
        self.restore.set_enabled(true);
        self.events.push(HistoryEvent::Selected(id));
    }

    /// The message shown instead of the changes, if any.
    fn message(&self) -> Option<String> {
        let id = if self.entries.is_empty() {
            "history-empty"
        } else {
            match &self.diff {
                Diff::None => "history-select",
                Diff::Loading => "history-loading",
                Diff::Ready(hunks) if hunks.is_empty() => "history-identical",
                Diff::Ready(_) => return None,
            }
        };
        Some(self.loc.text(id))
    }

    fn hunks(&self) -> &[DiffHunk] {
        match &self.diff {
            Diff::Ready(hunks) => hunks,
            _ => &[],
        }
    }

    fn line(&self, row: DiffRow) -> Option<&DiffLine> {
        self.hunks()
            .get(row.hunk)
            .and_then(|hunk| row.line.and_then(|line| hunk.lines.get(line)))
    }

    /// The text of a row: the hunk header or the line.
    fn row_text(&self, row: DiffRow) -> String {
        match self.line(row) {
            Some(line) => line.text.clone(),
            None => self.hunks().get(row.hunk).map_or_else(String::new, |hunk| {
                let old_first = hunk.old_start + 1;
                let new_first = hunk.new_start + 1;
                self.loc.format(
                    "history-hunk",
                    &[
                        ("oldStart", Arg::Int(old_first as i64)),
                        (
                            "oldEnd",
                            Arg::Int((hunk.old_start + hunk.old_count).max(old_first) as i64),
                        ),
                        ("newStart", Arg::Int(new_first as i64)),
                        (
                            "newEnd",
                            Arg::Int((hunk.new_start + hunk.new_count).max(new_first) as i64),
                        ),
                    ],
                )
            }),
        }
    }

    /// The accessible name of a row.
    fn row_name(&self, row: DiffRow) -> String {
        let text = self.row_text(row);
        match self.line(row).map(|line| line.kind) {
            Some(DiffLineKind::Added) => self
                .loc
                .format("history-line-added", &[("text", Arg::Str(&text))]),
            Some(DiffLineKind::Removed) => self
                .loc
                .format("history-line-removed", &[("text", Arg::Str(&text))]),
            Some(DiffLineKind::Context) => self
                .loc
                .format("history-line-context", &[("text", Arg::Str(&text))]),
            None => text,
        }
    }

    /// Recomputes the row heights for the current diff and width.
    fn measure_rows(&mut self, cx: &LayoutCx) {
        let width = self.diff_rect.width;
        if self.metrics.for_width == Some(width) {
            return;
        }
        let font = cx.theme.tokens.metrics.mono_font;
        let digits = self
            .hunks()
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .flat_map(|line| [line.old_line, line.new_line])
            .flatten()
            .max()
            .map_or(1, |last| (last + 1).to_string().len());
        let number_width = cx
            .text
            .layout(
                &"0".repeat(digits),
                &mono_style(font),
                &[],
                &[],
                f32::INFINITY,
            )
            .size()
            .width;
        let text_x = 2.0 * number_width + 3.0 * GUTTER_GAP + MARKER_WIDTH;
        let text_width = (width - BAR_WIDTH - text_x - GUTTER_GAP).max(0.0);
        let mut tops = Vec::with_capacity(self.rows.len() + 1);
        let mut top = 0.0;
        for row in &self.rows {
            tops.push(top);
            top += if row.line.is_some() {
                let text = self.row_text(*row);
                cx.text
                    .layout(&text, &mono_style(font), &[], &[], text_width)
                    .size()
                    .height
                    + 2.0 * LINE_PAD
            } else {
                HUNK_HEIGHT
            };
        }
        tops.push(top);
        self.metrics = RowMetrics {
            tops,
            number_width,
            text_width,
            for_width: Some(width),
        };
    }

    /// The rows intersecting the viewport as `first..end`.
    fn visible_rows(&self) -> std::ops::Range<usize> {
        let tops = &self.metrics.tops;
        if tops.len() < 2 {
            return 0..0;
        }
        let offset = self.bar.offset();
        let first = tops.partition_point(|top| *top <= offset).saturating_sub(1);
        let end = tops
            .partition_point(|top| *top < offset + self.diff_rect.height)
            .min(self.rows.len());
        first.min(self.rows.len())..end
    }

    fn row_rect(&self, index: usize) -> Rect {
        let top = self.metrics.tops[index];
        let bottom = self.metrics.tops[index + 1];
        Rect::new(
            self.diff_rect.x,
            self.diff_rect.y + top - self.bar.offset(),
            self.diff_rect.width - BAR_WIDTH,
            bottom - top,
        )
    }
}

impl Control for HistoryPanel {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for HistoryPanel {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        Size::new(
            if available.width.is_finite() {
                available.width
            } else {
                420.0
            },
            if available.height.is_finite() {
                available.height
            } else {
                560.0
            },
        )
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        self.header_rect = Rect::new(bounds.x, bounds.y, bounds.width, HEADER_HEIGHT);
        let footer_y = bounds.bottom() - FOOTER_HEIGHT;
        let inner_x = bounds.x + PAD;
        let inner_width = (bounds.width - 2.0 * PAD).max(0.0);
        let body_top = self.header_rect.bottom();
        let body_height = (footer_y - body_top).max(0.0);
        let list_height = (body_height * 0.35)
            .clamp(LIST_HEIGHT.0.min(body_height / 2.0), LIST_HEIGHT.1.max(0.0));
        self.list_rect = Rect::new(inner_x, body_top, inner_width, list_height);
        self.caption_rect = Rect::new(
            inner_x,
            self.list_rect.bottom() + GAP,
            inner_width,
            CAPTION_HEIGHT,
        );
        let diff_top = self.caption_rect.bottom();
        self.diff_rect = Rect::new(
            inner_x,
            diff_top,
            inner_width,
            (footer_y - GAP - diff_top).max(0.0),
        );
        let close_id = self.close.id();
        let close_size = self
            .close
            .measure(&mut cx.for_widget(close_id), Size::UNBOUNDED);
        self.close.arrange(
            &mut cx.for_widget(close_id),
            Rect::new(
                bounds.right() - PAD - close_size.width,
                bounds.y + (HEADER_HEIGHT - CONTROL_HEIGHT) / 2.0,
                close_size.width,
                CONTROL_HEIGHT,
            ),
        );
        let list_id = self.list.id();
        self.list
            .arrange(&mut cx.for_widget(list_id), self.list_rect);
        let restore_id = self.restore.id();
        let restore_size = self
            .restore
            .measure(&mut cx.for_widget(restore_id), Size::UNBOUNDED);
        self.restore.arrange(
            &mut cx.for_widget(restore_id),
            Rect::new(
                bounds.right() - PAD - restore_size.width,
                footer_y + (FOOTER_HEIGHT - CONTROL_HEIGHT) / 2.0,
                restore_size.width,
                CONTROL_HEIGHT,
            ),
        );
        self.measure_rows(cx);
        let total = self.metrics.tops.last().copied().unwrap_or(0.0);
        self.bar
            .model_mut()
            .set_extent(self.diff_rect.height, total);
        let bar_id = self.bar.id();
        self.bar.arrange(
            &mut cx.for_widget(bar_id),
            Rect::new(
                self.diff_rect.right() - BAR_WIDTH,
                self.diff_rect.y,
                BAR_WIDTH,
                self.diff_rect.height,
            ),
        );
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        canvas.fill_rect(self.bounds, tokens.surface);
        let title = cx.text.layout(
            &self.loc.text("history-title"),
            &ui_style(cx.theme, TITLE_SIZE).with_weight(FontWeight::SEMIBOLD),
            &[],
            &[],
            f32::INFINITY,
        );
        canvas.draw_text_layout(
            title.as_ref(),
            Point::new(
                self.bounds.x + PAD,
                self.header_rect.y + (HEADER_HEIGHT - title.size().height) / 2.0,
            ),
            tokens.text,
        );
        self.close.paint(&cx.for_widget(self.close.id()), canvas);
        self.list.paint(&cx.for_widget(self.list.id()), canvas);
        let caption = cx.text.layout(
            &self.loc.text("history-diff"),
            &ui_style(cx.theme, FONT_SIZE).with_weight(FontWeight::SEMIBOLD),
            &[],
            &[],
            f32::INFINITY,
        );
        canvas.draw_text_layout(
            caption.as_ref(),
            Point::new(
                self.caption_rect.x,
                self.caption_rect.y + (CAPTION_HEIGHT - caption.size().height) / 2.0,
            ),
            tokens.text_muted,
        );
        match self.message() {
            Some(message) => {
                let layout = cx.text.layout(
                    &message,
                    &ui_style(cx.theme, FONT_SIZE),
                    &[],
                    &[],
                    self.diff_rect.width,
                );
                canvas.draw_text_layout(
                    layout.as_ref(),
                    Point::new(self.diff_rect.x, self.diff_rect.y + GAP),
                    tokens.text_muted,
                );
            }
            None => self.paint_rows(cx, canvas),
        }
        self.restore
            .paint(&cx.for_widget(self.restore.id()), canvas);
        self.bar.paint(&cx.for_widget(self.bar.id()), canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let mut result = forward(&mut self.close, cx, e);
        result = result.or(forward(&mut self.list, cx, e));
        result = result.or(forward(&mut self.restore, cx, e));
        let bar_id = self.bar.id();
        match e {
            Event::Wheel(wheel) if self.diff_rect.contains(wheel.position) => {
                if self.bar.wheel(&mut cx.for_widget(bar_id), wheel) {
                    self.bar.take_changed();
                    cx.invalidate(self.diff_rect);
                    result = result.or(EventResult::handled());
                }
            }
            Event::Pointer(_) => {
                result = result.or(self.bar.event(&mut cx.for_widget(bar_id), e));
                if self.bar.take_changed() {
                    cx.invalidate(self.diff_rect);
                }
            }
            Event::AnimationFrame(now) => {
                if self.bar.animate(&mut cx.for_widget(bar_id), *now) {
                    cx.invalidate(self.diff_rect);
                }
            }
            Event::Key(key)
                if key.is_down
                    && key.vk == vk::ESCAPE
                    && key.mods.is_empty()
                    && !result.handled =>
            {
                self.events.push(HistoryEvent::Close);
                result = result.or(EventResult::handled());
            }
            _ => {}
        }
        self.harvest();
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        self.close.access(&cx.for_widget(self.close.id()), out);
        self.list.access(&cx.for_widget(self.list.id()), out);
        self.restore.access(&cx.for_widget(self.restore.id()), out);
        let mut diff_children = Vec::new();
        match self.message() {
            Some(message) => {
                out.insert(AccessNode::new(
                    self.message_id,
                    Role::Text,
                    message,
                    self.diff_rect,
                ));
                diff_children.push(self.message_id);
            }
            None => {
                for index in self.visible_rows() {
                    let row = self.rows[index];
                    let id = self.row_ids[index];
                    out.insert(AccessNode::new(
                        id,
                        Role::ListItem,
                        self.row_name(row),
                        self.row_rect(index),
                    ));
                    diff_children.push(id);
                }
            }
        }
        if self.bar.model().is_scrollable() {
            self.bar.access(&cx.for_widget(self.bar.id()), out);
            diff_children.push(self.bar.id());
        }
        out.insert(
            AccessNode::new(
                self.diff_id,
                Role::List,
                self.loc.text("history-diff"),
                self.diff_rect,
            )
            .with_children(diff_children),
        );
        out.insert(
            AccessNode::new(
                self.id,
                Role::Pane,
                self.loc.text("history-title"),
                self.bounds,
            )
            .with_children([
                self.close.id(),
                self.list.id(),
                self.diff_id,
                self.restore.id(),
            ]),
        );
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        let target = cx.id;
        let handled = if target == self.close.id() {
            self.close.perform(&mut cx.for_widget(target), a)
        } else if target == self.restore.id() {
            self.restore.perform(&mut cx.for_widget(target), a)
        } else if target == self.bar.id() {
            self.bar.perform(&mut cx.for_widget(target), a)
        } else if target == self.list.id() {
            self.list.perform(&mut cx.for_widget(target), a)
        } else {
            self.list
                .perform_item(&mut cx.for_widget(target), target, a)
        };
        self.harvest();
        handled
    }
}

impl HistoryPanel {
    fn harvest(&mut self) {
        let selection = self
            .list
            .take_events()
            .contains(&ListEvent::SelectionChanged);
        if selection
            && let Some(entry) = self
                .list
                .selected_index()
                .and_then(|index| self.entries.get(index))
        {
            let id = entry.id;
            self.choose(id);
        }
        if self.restore.take_activated()
            && let Some(id) = self.selected
        {
            self.events.push(HistoryEvent::Restore(id));
        }
        if self.close.take_activated() {
            self.events.push(HistoryEvent::Close);
        }
    }

    fn paint_rows(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        let font = tokens.metrics.mono_font;
        let style = mono_style(font);
        canvas.push_clip_rect(self.diff_rect);
        for index in self.visible_rows() {
            let row = self.rows[index];
            let rect = self.row_rect(index);
            let Some(line) = self.line(row) else {
                canvas.fill_rect(rect, tokens.code_bg);
                let layout = cx
                    .text
                    .layout(&self.row_text(row), &style, &[], &[], rect.width);
                canvas.draw_text_layout(
                    layout.as_ref(),
                    Point::new(
                        rect.x + GUTTER_GAP,
                        rect.y + (rect.height - layout.size().height) / 2.0,
                    ),
                    tokens.text_muted,
                );
                continue;
            };
            let (tint, marker) = match line.kind {
                DiffLineKind::Added => (Some(tokens.success), "+"),
                DiffLineKind::Removed => (Some(tokens.error), "-"),
                DiffLineKind::Context => (None, " "),
            };
            if let Some(tint) = tint {
                canvas.fill_rect(rect, tint.with_alpha(LINE_TINT));
            }
            let number_width = self.metrics.number_width;
            let columns = [
                (line.old_line, rect.x + GUTTER_GAP),
                (line.new_line, rect.x + 2.0 * GUTTER_GAP + number_width),
            ];
            for (number, x) in columns {
                if let Some(number) = number {
                    let layout =
                        cx.text
                            .layout(&(number + 1).to_string(), &style, &[], &[], f32::INFINITY);
                    canvas.draw_text_layout(
                        layout.as_ref(),
                        Point::new(x + number_width - layout.size().width, rect.y + LINE_PAD),
                        tokens.text_muted,
                    );
                }
            }
            let marker_x = rect.x + 3.0 * GUTTER_GAP + 2.0 * number_width;
            let marker_layout = cx.text.layout(marker, &style, &[], &[], f32::INFINITY);
            canvas.draw_text_layout(
                marker_layout.as_ref(),
                Point::new(marker_x, rect.y + LINE_PAD),
                tokens.text_muted,
            );
            let text_layout = cx
                .text
                .layout(&line.text, &style, &[], &[], self.metrics.text_width);
            canvas.draw_text_layout(
                text_layout.as_ref(),
                Point::new(marker_x + MARKER_WIDTH, rect.y + LINE_PAD),
                tokens.text,
            );
        }
        canvas.pop_clip();
    }
}
