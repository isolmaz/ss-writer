//! The row list shared by Quick Open, the command palette, the wikilink completion and the folder
//! search results.
//!
//! A [`RowList`] is not a [`Widget`](crate::widget::Widget): its owner (an overlay, a popup or a
//! panel) routes keys, wheel and pointer positions to it and paints it, so one focus stop of the
//! owner can drive it, as in a combo box. Rows are fixed-height ([`ROW_HEIGHT`]); only the rows in
//! view are painted and described to accessibility. A row has a muted `prefix` (line number,
//! heading level), a `label` with match highlights, and a muted right-aligned `detail`
//! (shortcut, folder, match count). Header rows group other rows and are only selectable when
//! they are collapsible (`expanded` is set).

use std::ops::Range;

use crate::access::{AccessAction, AccessNode, AccessState, AccessTree, Role};
use crate::canvas::{Canvas, IconFont};
use crate::geometry::{CornerRadii, Insets, Point, Rect};
use crate::input::{KeyEvent, WheelEvent, vk};
use crate::text::FontWeight;
use crate::widget::{AccessCx, PaintCx, WidgetId};
use crate::widgets::{FONT_SIZE, ICON_SIZE, draw_left_centered, focus_ring, glyph, ui_style};

/// Height of one row.
pub const ROW_HEIGHT: f32 = 32.0;
/// Text inset from the row's edges.
const INSET: f32 = 12.0;
/// Gap between the prefix, the label and the detail.
const GAP: f32 = 8.0;
/// Rows scrolled per wheel notch.
const ROWS_PER_NOTCH: f32 = 3.0;
/// Width of the scroll indicator.
const THUMB_WIDTH: f32 = 3.0;
/// Shortest scroll indicator.
const THUMB_MIN: f32 = 16.0;
/// Opacity of the accent behind matched characters.
const MATCH_OPACITY: f32 = 0.3;

/// One row of a [`RowList`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Row {
    /// Muted text before the label.
    pub prefix: String,
    pub label: String,
    /// Byte ranges of `label` shown as matches (char boundaries).
    pub highlights: Vec<Range<usize>>,
    /// Muted text at the right edge.
    pub detail: String,
    /// A group header: semibold, not selectable unless `expanded` is set.
    pub header: bool,
    /// `Some` for a collapsible header: whether its group is shown.
    pub expanded: Option<bool>,
    /// The accessible name when it differs from `prefix label detail`.
    pub name: Option<String>,
    /// Drawn in the disabled text color and described as disabled; its owner refuses to run it.
    pub disabled: bool,
}

impl Row {
    /// A plain row showing `label`.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ..Self::default()
        }
    }

    fn selectable(&self) -> bool {
        !self.header || self.expanded.is_some()
    }

    fn access_name(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        [&self.prefix, &self.label, &self.detail]
            .iter()
            .filter(|part| !part.is_empty())
            .map(|part| part.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Converts the matched `char` indices of the fuzzy matcher (sorted, deduplicated) to merged
/// byte ranges of `text`.
#[must_use]
pub fn highlight_ranges(text: &str, indices: &[usize]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut wanted = indices.iter().copied().peekable();
    for (position, (start, c)) in text.char_indices().enumerate() {
        if wanted.peek() != Some(&position) {
            continue;
        }
        wanted.next();
        let end = start + c.len_utf8();
        match ranges.last_mut() {
            Some(last) if last.end == start => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    ranges
}

/// What a pointer click on the list hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowClick {
    /// A normal row was clicked.
    Item(usize),
    /// A collapsible header was clicked.
    Header(usize),
}

/// What a key did to the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKey {
    /// Not a list key.
    Ignored,
    /// The selection moved (or stayed at an end).
    Moved,
    /// Enter on the selected row.
    Activate(usize),
}

/// What assistive technology did to a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOutcome {
    /// The row was selected.
    Selected,
    /// The row was invoked (or expanded / collapsed).
    Click(RowClick),
}

/// A scrolling list of [`Row`]s with one selected row.
#[derive(Debug, Clone)]
pub struct RowList {
    id: WidgetId,
    label: String,
    rows: Vec<Row>,
    ids: Vec<WidgetId>,
    selected: Option<usize>,
    hover: Option<usize>,
    first: usize,
    max_rows: usize,
    bounds: Rect,
}

impl RowList {
    /// An empty list named `label` that wants at most `max_rows` rows of height.
    #[must_use]
    pub fn new(label: impl Into<String>, max_rows: usize) -> Self {
        Self {
            id: WidgetId::allocate(),
            label: label.into(),
            rows: Vec::new(),
            ids: Vec::new(),
            selected: None,
            hover: None,
            first: 0,
            max_rows,
            bounds: Rect::ZERO,
        }
    }

    #[must_use]
    pub fn id(&self) -> WidgetId {
        self.id
    }

    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The selected row's index.
    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Index of the first row in view.
    #[must_use]
    pub fn first_visible(&self) -> usize {
        self.first
    }

    /// Replaces the rows, scrolls to the top and selects the first selectable row.
    pub fn set_rows(&mut self, rows: Vec<Row>) {
        self.ids = rows.iter().map(|_| WidgetId::allocate()).collect();
        self.rows = rows;
        self.first = 0;
        self.hover = None;
        self.selected = self.rows.iter().position(Row::selectable);
    }

    /// Replaces the rows keeping the scroll position and the selected index where they still
    /// exist (a collapsible group changed).
    pub fn update_rows(&mut self, rows: Vec<Row>) {
        let selected = self.selected;
        let first = self.first;
        self.set_rows(rows);
        let last = self.rows.len().saturating_sub(1);
        if let Some(index) =
            selected.filter(|index| self.rows.get(*index).is_some_and(Row::selectable))
        {
            self.selected = Some(index);
        }
        self.first = first.min(last);
        self.fit();
    }

    /// The height the list wants: its rows up to the limit given to [`RowList::new`].
    #[must_use]
    pub fn desired_height(&self) -> f32 {
        self.rows.len().min(self.max_rows) as f32 * ROW_HEIGHT
    }

    /// Assigns the final bounds and keeps the first row in view valid.
    pub fn arrange(&mut self, bounds: Rect) {
        self.bounds = bounds;
        self.fit();
    }

    /// Number of rows that fit in the bounds (at least one).
    #[must_use]
    pub fn capacity(&self) -> usize {
        ((self.bounds.height / ROW_HEIGHT).floor() as usize).max(1)
    }

    /// The rows in view.
    #[must_use]
    pub fn visible(&self) -> Range<usize> {
        self.first..(self.first + self.capacity()).min(self.rows.len())
    }

    /// Bounds of row `index` when it is in view.
    #[must_use]
    pub fn row_rect(&self, index: usize) -> Option<Rect> {
        self.visible()
            .contains(&index)
            .then(|| self.slot_rect(index - self.first))
    }

    fn slot_rect(&self, slot: usize) -> Rect {
        Rect::new(
            self.bounds.x,
            self.bounds.y + slot as f32 * ROW_HEIGHT,
            self.bounds.width,
            ROW_HEIGHT,
        )
    }

    /// Clamps the scroll position and brings the selection into view.
    fn fit(&mut self) {
        let capacity = self.capacity();
        self.first = self.first.min(self.rows.len().saturating_sub(capacity));
        if let Some(index) = self.selected {
            if index < self.first {
                self.first = index;
            } else if index >= self.first + capacity {
                self.first = index + 1 - capacity;
            }
        }
    }

    /// Selects row `index` when it is selectable. Returns whether it is selected afterwards.
    pub fn select(&mut self, index: usize) -> bool {
        if !self.rows.get(index).is_some_and(Row::selectable) {
            return false;
        }
        self.selected = Some(index);
        self.fit();
        true
    }

    /// Moves the selection one selectable row up or down, wrapping around.
    pub fn step(&mut self, delta: isize) -> bool {
        let count = self.rows.len() as isize;
        if count == 0 {
            return false;
        }
        let direction = delta.signum();
        let mut at = self
            .selected
            .map_or(if direction > 0 { -1 } else { count }, |index| {
                index as isize
            });
        for _ in 0..count {
            at = (at + direction).rem_euclid(count);
            if self.select(at as usize) {
                return true;
            }
        }
        false
    }

    /// Selects the selectable row nearest to `target`, searching forward first when `forward`.
    fn select_nearest(&mut self, target: usize, forward: bool) -> bool {
        let count = self.rows.len();
        if count == 0 {
            return false;
        }
        let target = target.min(count - 1);
        let ahead = target..count;
        let behind = (0..target).rev();
        let found = if forward {
            ahead
                .chain(behind)
                .find(|index| self.rows[*index].selectable())
        } else {
            (0..=target)
                .rev()
                .chain(target + 1..count)
                .find(|index| self.rows[*index].selectable())
        };
        found.is_some_and(|index| self.select(index))
    }

    /// Scrolls by whole rows without moving the selection.
    pub fn scroll_by(&mut self, rows: isize) -> bool {
        let last = self.rows.len().saturating_sub(self.capacity());
        let first = self.first.saturating_add_signed(rows).min(last);
        let moved = first != self.first;
        self.first = first;
        moved
    }

    /// Handles a wheel event over the list. Returns whether the list scrolled.
    pub fn wheel(&mut self, event: &WheelEvent) -> bool {
        if !self.bounds.contains(event.position) {
            return false;
        }
        let rows = if event.precise {
            -event.delta.y / ROW_HEIGHT
        } else {
            -event.delta.y * ROWS_PER_NOTCH
        };
        self.scroll_by(rows.round() as isize)
    }

    /// Handles a navigation key: Up/Down move (wrapping), Page Up/Down move by a page, Home/End
    /// jump, Enter activates. Keys with modifiers are ignored.
    pub fn key(&mut self, key: &KeyEvent) -> RowKey {
        if !key.mods.is_empty() {
            return RowKey::Ignored;
        }
        let page = self.capacity();
        let current = self.selected.unwrap_or(0);
        match key.vk {
            vk::UP => self.step(-1),
            vk::DOWN => self.step(1),
            vk::PAGE_UP => self.select_nearest(current.saturating_sub(page), false),
            vk::PAGE_DOWN => self.select_nearest(current + page, true),
            vk::HOME => self.select_nearest(0, true),
            vk::END => self.select_nearest(usize::MAX, false),
            vk::RETURN => {
                return self.selected.map_or(RowKey::Ignored, RowKey::Activate);
            }
            _ => return RowKey::Ignored,
        };
        RowKey::Moved
    }

    /// The row under `point`, when in view.
    #[must_use]
    pub fn hit(&self, point: Point) -> Option<usize> {
        if !self.bounds.contains(point) {
            return None;
        }
        let slot = ((point.y - self.bounds.y) / ROW_HEIGHT) as usize;
        let index = self.first + slot;
        self.visible().contains(&index).then_some(index)
    }

    /// Tracks the hovered row. Returns whether it changed.
    pub fn hover_at(&mut self, point: Point) -> bool {
        let hover = self.hit(point);
        std::mem::replace(&mut self.hover, hover) != hover
    }

    /// Clears the hover (the pointer left).
    pub fn clear_hover(&mut self) -> bool {
        self.hover.take().is_some()
    }

    /// A left click at `point`: selects a selectable row and reports what was hit.
    pub fn click(&mut self, point: Point) -> Option<RowClick> {
        let index = self.hit(point)?;
        self.select(index);
        Some(self.click_of(index))
    }

    fn click_of(&self, index: usize) -> RowClick {
        if self.rows[index].header {
            RowClick::Header(index)
        } else {
            RowClick::Item(index)
        }
    }

    /// Draws the rows in view. `cx` is the list's own context (`cx.for_widget(list.id())`).
    pub fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        let radius = tokens.metrics.control_radius;
        canvas.push_clip_rect(self.bounds);
        for index in self.visible() {
            let rect = self.slot_rect(index - self.first);
            let row = &self.rows[index];
            let pill = rect.inset(Insets::symmetric(4.0, 2.0));
            if self.selected == Some(index) {
                canvas.fill_rounded_rect(pill, CornerRadii::uniform(radius), tokens.pressed_bg);
                canvas.fill_rounded_rect(
                    Rect::new(rect.x + 4.0, rect.center().y - 8.0, THUMB_WIDTH, 16.0),
                    CornerRadii::uniform(THUMB_WIDTH / 2.0),
                    tokens.accent,
                );
            } else if self.hover == Some(index) && row.selectable() {
                canvas.fill_rounded_rect(pill, CornerRadii::uniform(radius), tokens.hover_bg);
            }
            self.paint_row(cx, canvas, row, rect);
            if cx.is_focused() && self.selected == Some(index) {
                focus_ring::paint(canvas, pill, radius, cx.theme);
            }
        }
        canvas.pop_clip();
        let capacity = self.capacity();
        if self.rows.len() > capacity {
            let track = self.bounds.height;
            let thumb = (track * capacity as f32 / self.rows.len() as f32).max(THUMB_MIN);
            let range = (self.rows.len() - capacity) as f32;
            let y = self.bounds.y + (track - thumb) * self.first as f32 / range;
            canvas.fill_rounded_rect(
                Rect::new(
                    self.bounds.right() - THUMB_WIDTH - 2.0,
                    y,
                    THUMB_WIDTH,
                    thumb,
                ),
                CornerRadii::uniform(THUMB_WIDTH / 2.0),
                tokens.scrollbar,
            );
        }
    }

    fn paint_row(&self, cx: &PaintCx, canvas: &mut dyn Canvas, row: &Row, rect: Rect) {
        let tokens = &cx.theme.tokens;
        let base = ui_style(cx.theme, FONT_SIZE);
        let layout = |text: &str, weight: FontWeight| {
            cx.text.layout(
                text,
                &base.clone().with_weight(weight),
                &[],
                &[],
                f32::INFINITY,
            )
        };
        let weight = if row.header {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        };
        let (text_color, muted_color) = if row.disabled {
            (tokens.text_disabled, tokens.text_disabled)
        } else {
            (tokens.text, tokens.text_muted)
        };
        let mut left = rect.x + INSET;
        if let Some(expanded) = row.expanded {
            let glyph = if expanded {
                glyph::CHEVRON_DOWN
            } else {
                glyph::CHEVRON_RIGHT
            };
            canvas.draw_icon(
                glyph,
                IconFont::SegoeFluent,
                ICON_SIZE * 0.75,
                tokens.text_muted,
                Point::new(left + 6.0, rect.center().y),
            );
            left += ICON_SIZE;
        }
        if !row.prefix.is_empty() {
            let prefix = layout(&row.prefix, FontWeight::NORMAL);
            draw_left_centered(canvas, prefix.as_ref(), left, rect, muted_color);
            left += prefix.size().width + GAP;
        }
        let mut right = rect.right() - INSET;
        if !row.detail.is_empty() {
            let detail = layout(&row.detail, FontWeight::NORMAL);
            let width = detail.size().width;
            draw_left_centered(canvas, detail.as_ref(), right - width, rect, muted_color);
            right -= width + GAP;
        }
        let label = layout(&row.label, weight);
        let area = Rect::new(left, rect.y, (right - left).max(0.0), rect.height);
        let top = rect.y + (rect.height - label.size().height) / 2.0;
        canvas.push_clip_rect(area);
        for range in &row.highlights {
            for part in label.range_rects(range.clone()) {
                canvas.fill_rounded_rect(
                    part.translate(left, top),
                    CornerRadii::uniform(2.0),
                    tokens.accent.with_alpha(MATCH_OPACITY),
                );
            }
        }
        canvas.draw_text_layout(label.as_ref(), Point::new(left, top), text_color);
        canvas.pop_clip();
    }

    /// Describes the list and the rows in view. `cx` is the list's own context.
    pub fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let visible = self.visible();
        for index in visible.clone() {
            let row = &self.rows[index];
            let mut node = AccessNode::new(
                self.ids[index],
                Role::ListItem,
                row.access_name(),
                self.slot_rect(index - self.first),
            );
            node.state.selected = self.selected == Some(index);
            node.state.expanded = row.expanded;
            node.state.disabled = row.disabled;
            node.patterns.selection_item = row.selectable();
            node.patterns.expand_collapse = row.expanded.is_some();
            node.patterns.invoke = !row.header && !row.disabled;
            out.insert(node);
        }
        let state = AccessState {
            focusable: cx.is_focusable(),
            focused: cx.is_focused(),
            ..AccessState::default()
        };
        out.insert(
            AccessNode::new(self.id, Role::List, self.label.clone(), self.bounds)
                .with_state(state)
                .with_children(visible.map(|index| self.ids[index])),
        );
    }

    /// Carries out `action` on the row whose access node has id `item`. `None` when the row does
    /// not exist or does not support the action.
    pub fn perform_item(&mut self, item: WidgetId, action: &AccessAction) -> Option<RowOutcome> {
        let index = self.ids.iter().position(|id| *id == item)?;
        let expanded = self.rows[index].expanded;
        match action {
            AccessAction::Select => self.select(index).then_some(RowOutcome::Selected),
            AccessAction::Invoke => Some(RowOutcome::Click(self.click_of(index))),
            AccessAction::Expand if expanded == Some(false) => {
                Some(RowOutcome::Click(RowClick::Header(index)))
            }
            AccessAction::Collapse if expanded == Some(true) => {
                Some(RowOutcome::Click(RowClick::Header(index)))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{DrawOp, RecordingCanvas};
    use crate::input::Modifiers;
    use crate::widgets::testkit::Harness;

    fn list_of(rows: Vec<Row>, height: f32) -> RowList {
        let mut list = RowList::new("Results", 10);
        list.set_rows(rows);
        list.arrange(Rect::new(10.0, 20.0, 200.0, height));
        list
    }

    fn plain(count: usize) -> Vec<Row> {
        (0..count).map(|n| Row::new(format!("row {n}"))).collect()
    }

    fn header(label: &str) -> Row {
        Row {
            header: true,
            ..Row::new(label)
        }
    }

    fn press(list: &mut RowList, key: u16) -> RowKey {
        list.key(&KeyEvent::press(key, Modifiers::NONE))
    }

    fn wheel(list: &mut RowList, at: Point, dy: f32, precise: bool) -> bool {
        list.wheel(&WheelEvent {
            delta: Point::new(0.0, dy),
            precise,
            position: at,
            mods: Modifiers::NONE,
        })
    }

    fn paint(harness: &Harness, list: &RowList, damage: Option<Rect>) -> RecordingCanvas {
        let mut canvas = RecordingCanvas::new();
        let cx = PaintCx::new(
            list.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &harness.focus,
            &harness.capture,
            damage,
        );
        list.paint(&cx, &mut canvas);
        canvas
    }

    fn count_fills(canvas: &RecordingCanvas, color: crate::geometry::Color) -> usize {
        canvas
            .ops()
            .filter(|op| matches!(op, DrawOp::FillRoundedRect { color: c, .. } if *c == color))
            .count()
    }

    #[test]
    fn a_new_list_selects_its_first_row_and_reports_its_size() {
        let list = list_of(plain(3), 96.0);
        assert_eq!(list.len(), 3);
        assert!(!list.is_empty());
        assert_eq!(list.rows()[1].label, "row 1");
        assert_eq!(list.selected(), Some(0));
        assert_eq!(RowList::new("Empty", 4).len(), 0);
    }

    #[test]
    fn headers_that_cannot_collapse_are_skipped_by_selection() {
        let rows = vec![header("One"), Row::new("a"), header("Two"), Row::new("b")];
        let mut list = list_of(rows, 128.0);
        assert_eq!(list.selected(), Some(1));
        assert!(!list.select(0) && !list.select(2) && !list.select(9));
        assert_eq!(list.selected(), Some(1));
        // Stepping passes the header and wraps around in both directions.
        assert!(list.step(1));
        assert_eq!(list.selected(), Some(3));
        assert!(list.step(1));
        assert_eq!(list.selected(), Some(1));
        assert!(list.step(-1));
        assert_eq!(list.selected(), Some(3));
    }

    #[test]
    fn a_list_without_selectable_rows_never_selects() {
        let mut list = list_of(vec![header("One"), header("Two")], 64.0);
        assert_eq!(list.selected(), None);
        assert!(!list.step(1) && !list.step(-1));
        assert_eq!(press(&mut list, vk::DOWN), RowKey::Moved);
        assert_eq!(press(&mut list, vk::END), RowKey::Moved);
        assert_eq!(list.selected(), None);
        assert_eq!(press(&mut list, vk::RETURN), RowKey::Ignored);
        let mut empty = list_of(Vec::new(), 64.0);
        assert!(!empty.step(1));
        assert_eq!(press(&mut empty, vk::HOME), RowKey::Moved);
        assert_eq!(empty.selected(), None);
    }

    #[test]
    fn updating_rows_keeps_the_scroll_and_the_selection_only_where_they_still_exist() {
        let mut list = list_of(plain(8), 96.0);
        press(&mut list, vk::END);
        assert_eq!((list.selected(), list.first_visible()), (Some(7), 5));
        // A longer list keeps both where they were.
        list.update_rows(plain(10));
        assert_eq!((list.selected(), list.first_visible()), (Some(7), 5));
        // A shorter list has no row 7: the selection falls back to the first selectable row.
        list.update_rows(plain(4));
        assert_eq!((list.selected(), list.first_visible()), (Some(0), 0));
        // A row that is no longer selectable cannot keep the selection either.
        assert!(list.select(2));
        list.update_rows(vec![
            Row::new("a"),
            Row::new("b"),
            header("Two"),
            Row::new("c"),
        ]);
        assert_eq!(list.selected(), Some(0));
    }

    #[test]
    fn navigation_keys_move_by_row_page_and_to_the_ends() {
        let mut list = list_of(plain(8), 96.0);
        assert_eq!(list.capacity(), 3);
        assert_eq!(press(&mut list, vk::END), RowKey::Moved);
        assert_eq!((list.selected(), list.first_visible()), (Some(7), 5));
        assert_eq!(press(&mut list, vk::HOME), RowKey::Moved);
        assert_eq!((list.selected(), list.first_visible()), (Some(0), 0));
        press(&mut list, vk::PAGE_DOWN);
        assert_eq!(list.selected(), Some(3));
        press(&mut list, vk::PAGE_UP);
        assert_eq!(list.selected(), Some(0));
        assert_eq!(press(&mut list, vk::RETURN), RowKey::Activate(0));
        assert_eq!(press(&mut list, vk::F3), RowKey::Ignored);
        // Modified keys belong to the owner.
        let ctrl_down = KeyEvent::press(vk::DOWN, Modifiers::CTRL);
        assert_eq!(list.key(&ctrl_down), RowKey::Ignored);
        assert_eq!(list.selected(), Some(0));
    }

    #[test]
    fn the_wheel_scrolls_whole_rows_only_over_the_list() {
        let mut list = list_of(plain(8), 96.0);
        let inside = Point::new(50.0, 50.0);
        assert!(!wheel(&mut list, Point::new(500.0, 500.0), -64.0, true));
        assert_eq!(list.first_visible(), 0);
        // A precise wheel scrolls by the distance, a notched one three rows per notch.
        assert!(wheel(&mut list, inside, -64.0, true));
        assert_eq!(list.first_visible(), 2);
        assert!(wheel(&mut list, inside, -1.0, false));
        assert_eq!(list.first_visible(), 5);
        assert!(!wheel(&mut list, inside, -1.0, false));
        assert_eq!(list.first_visible(), 5);
        assert_eq!(list.selected(), Some(0));
    }

    #[test]
    fn only_rows_in_view_can_be_hit_or_clicked() {
        let mut list = list_of(plain(1), 96.0);
        assert_eq!(list.hit(Point::new(20.0, 30.0)), Some(0));
        assert_eq!(list.hit(Point::new(500.0, 500.0)), None);
        // Inside the bounds but below the last row.
        let blank = Point::new(20.0, 20.0 + 2.0 * ROW_HEIGHT);
        assert_eq!(list.hit(blank), None);
        assert_eq!(list.click(blank), None);
        assert_eq!(list.click(Point::new(500.0, 500.0)), None);
        assert_eq!(list.click(Point::new(20.0, 30.0)), Some(RowClick::Item(0)));
    }

    #[test]
    fn hovering_highlights_selectable_rows_only() {
        let harness = Harness::new();
        let hover = harness.theme.tokens.hover_bg;
        let mut list = list_of(plain(3), 96.0);
        assert_eq!(count_fills(&paint(&harness, &list, None), hover), 0);
        assert!(list.hover_at(Point::new(20.0, 20.0 + 1.5 * ROW_HEIGHT)));
        assert!(!list.hover_at(Point::new(20.0, 20.0 + 1.6 * ROW_HEIGHT)));
        assert_eq!(count_fills(&paint(&harness, &list, None), hover), 1);
        assert!(list.clear_hover());
        assert!(!list.clear_hover());
        assert_eq!(count_fills(&paint(&harness, &list, None), hover), 0);
        // A header that cannot collapse is not a target.
        let mut grouped = list_of(vec![header("One"), Row::new("a")], 64.0);
        assert!(grouped.hover_at(Point::new(20.0, 30.0)));
        assert_eq!(count_fills(&paint(&harness, &grouped, None), hover), 0);
    }

    #[test]
    fn painting_follows_the_damage_and_the_focus() {
        let mut harness = Harness::new();
        let list = list_of(plain(3), 96.0);
        let away = Rect::new(500.0, 500.0, 10.0, 10.0);
        assert_eq!(paint(&harness, &list, Some(away)).ops().count(), 0);
        assert!(paint(&harness, &list, Some(list.bounds())).ops().count() > 0);
        let ring = harness.theme.tokens.focus_ring;
        let rings = |canvas: &RecordingCanvas| {
            canvas
                .ops()
                .filter(
                    |op| matches!(op, DrawOp::StrokeRoundedRect { color, .. } if *color == ring),
                )
                .count()
        };
        assert_eq!(rings(&paint(&harness, &list, None)), 0);
        harness.focus.begin_tab_order();
        harness.focus.add_tab_stop(list.id());
        harness.focus.commit_tab_order();
        assert!(harness.focus.focus(list.id()));
        assert_eq!(rings(&paint(&harness, &list, None)), 1);
    }

    #[test]
    fn collapsible_headers_show_a_chevron_for_their_state() {
        let harness = Harness::new();
        let group = |label: &str, expanded: bool| Row {
            expanded: Some(expanded),
            ..header(label)
        };
        let list = list_of(vec![group("Closed", false), group("Open", true)], 64.0);
        let canvas = paint(&harness, &list, None);
        let glyphs: Vec<char> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::Icon { glyph, .. } => Some(*glyph),
                _ => None,
            })
            .collect();
        assert_eq!(glyphs, [glyph::CHEVRON_RIGHT, glyph::CHEVRON_DOWN]);
    }

    #[test]
    fn highlight_ranges_merge_adjacent_characters_in_bytes() {
        // "İstanbul/ğ.md": the dotted capital I is two bytes.
        let text = "İstanbul/ğ.md";
        assert_eq!(highlight_ranges(text, &[0, 1, 2, 4]), vec![0..4, 5..6]);
        assert_eq!(highlight_ranges(text, &[9]), vec![10..12]);
        assert!(highlight_ranges(text, &[]).is_empty());
    }
}
