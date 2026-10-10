//! A virtualized list with single or multiple selection, keyboard navigation and type-ahead.

use std::collections::BTreeSet;

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::Canvas;
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::{
    Event, FocusEvent, KeyEvent, PointerButton, PointerEvent, PointerEventKind, vk,
};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::anim::Motion;
use super::scroll::{Axis, BAR_WIDTH, ScrollBar, scroll_info};
use super::{
    CONTROL_HEIGHT, Control, FONT_SIZE, TypeAhead, control_state, draw_left_centered, focus_ring,
    layout_line, perform_focus,
};

/// Height of one row.
pub const ROW_HEIGHT: f32 = CONTROL_HEIGHT;
/// Text inset from the row's left edge (leaves room for the selection indicator).
const TEXT_INSET: f32 = 16.0;
/// Tallest list that asks for its natural height when the parent does not constrain it.
const MAX_NATURAL_HEIGHT: f32 = 320.0;

/// How many rows can be selected at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Single,
    /// Ctrl toggles, Shift extends from the anchor, Ctrl+A selects everything.
    Multiple,
}

/// What happened to the list since the owner last looked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListEvent {
    SelectionChanged,
    /// Enter or a double click on this row.
    Activated(usize),
}

/// A list of text rows. Only the rows in view are painted and described to accessibility.
#[derive(Debug, Clone)]
pub struct ListView {
    id: WidgetId,
    label: String,
    items: Vec<String>,
    item_ids: Vec<WidgetId>,
    mode: SelectionMode,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    current: Option<usize>,
    hover: Option<usize>,
    row_height: f32,
    enabled: bool,
    bounds: Rect,
    bar: ScrollBar,
    type_ahead: TypeAhead,
    events: Vec<ListEvent>,
}

impl ListView {
    /// A list of `items`; `label` is the accessible name.
    #[must_use]
    pub fn new(label: impl Into<String>, items: Vec<String>, mode: SelectionMode) -> Self {
        let item_ids = items.iter().map(|_| WidgetId::allocate()).collect();
        Self {
            id: WidgetId::allocate(),
            label: label.into(),
            items,
            item_ids,
            mode,
            selected: BTreeSet::new(),
            anchor: None,
            current: None,
            hover: None,
            row_height: ROW_HEIGHT,
            enabled: true,
            bounds: Rect::ZERO,
            bar: ScrollBar::new(Axis::Vertical, ""),
            type_ahead: TypeAhead::new(),
            events: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.bar = self.bar.with_motion(motion);
        self
    }

    #[must_use]
    pub fn with_row_height(mut self, row_height: f32) -> Self {
        self.row_height = row_height.max(1.0);
        self
    }

    #[must_use]
    pub fn items(&self) -> &[String] {
        &self.items
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Replaces the rows, clearing selection and the current row.
    pub fn set_items(&mut self, items: Vec<String>) {
        self.item_ids = items.iter().map(|_| WidgetId::allocate()).collect();
        self.items = items;
        self.selected.clear();
        self.anchor = None;
        self.current = None;
        self.hover = None;
        self.update_extent();
    }

    /// The selected rows in ascending order.
    #[must_use]
    pub fn selected(&self) -> Vec<usize> {
        self.selected.iter().copied().collect()
    }

    /// The first selected row.
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        self.selected.first().copied()
    }

    /// The row the keyboard cursor is on.
    #[must_use]
    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// Selects only `index` without notifying the owner; out-of-range indices clear the
    /// selection.
    pub fn select(&mut self, index: usize) {
        self.selected.clear();
        if index < self.items.len() {
            self.selected.insert(index);
            self.anchor = Some(index);
            self.current = Some(index);
            let top = self.row_top(index);
            let bottom = top + self.row_height;
            self.bar.model_mut().ensure_visible(top, bottom);
        }
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    #[must_use]
    pub fn scroll_offset(&self) -> f32 {
        self.bar.offset()
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<ListEvent> {
        std::mem::take(&mut self.events)
    }

    /// Places the list without registering a tab stop (a popup list that never takes focus
    /// itself); `arrange` does this and then registers the tab stop.
    pub fn place(&mut self, bounds: Rect) {
        self.bounds = bounds;
        self.update_extent();
        self.bar.set_bounds(Rect::new(
            bounds.right() - BAR_WIDTH,
            bounds.y,
            BAR_WIDTH,
            bounds.height,
        ));
    }

    /// True when `point` is on the scroll bar of a list that scrolls.
    #[must_use]
    pub fn scrollbar_contains(&self, point: Point) -> bool {
        self.bar.model().is_scrollable() && self.bar.bounds().contains(point)
    }

    fn row_top(&self, index: usize) -> f32 {
        index as f32 * self.row_height
    }

    /// Bounds of row `index` in window coordinates (it may be scrolled out of view).
    #[must_use]
    pub fn row_rect(&self, index: usize) -> Rect {
        Rect::new(
            self.bounds.x,
            self.bounds.y + self.row_top(index) - self.bar.offset(),
            self.bounds.width,
            self.row_height,
        )
    }

    /// The rows intersecting the viewport as `first..end`.
    #[must_use]
    pub fn visible_range(&self) -> std::ops::Range<usize> {
        let offset = self.bar.offset();
        let first = (offset / self.row_height).floor() as usize;
        let end = ((offset + self.bounds.height) / self.row_height).ceil() as usize;
        first.min(self.items.len())..end.min(self.items.len())
    }

    fn row_at(&self, point: Point) -> Option<usize> {
        if !self.bounds.contains(point) {
            return None;
        }
        let y = point.y - self.bounds.y + self.bar.offset();
        let index = (y / self.row_height) as usize;
        (index < self.items.len()).then_some(index)
    }

    fn update_extent(&mut self) {
        let content = self.row_top(self.items.len());
        self.bar.model_mut().set_extent(self.bounds.height, content);
    }

    fn page_rows(&self) -> usize {
        ((self.bounds.height / self.row_height) as usize)
            .saturating_sub(1)
            .max(1)
    }

    fn set_selection(&mut self, selection: BTreeSet<usize>) {
        if selection != self.selected {
            self.selected = selection;
            self.events.push(ListEvent::SelectionChanged);
        }
    }

    fn select_range(&mut self, from: usize, to: usize) {
        let (low, high) = if from <= to { (from, to) } else { (to, from) };
        self.set_selection((low..=high).collect());
    }

    /// Moves the cursor to `index` and updates the selection according to `mods`.
    fn go(&mut self, index: usize, mods: crate::input::Modifiers) {
        if self.items.is_empty() {
            return;
        }
        let index = index.min(self.items.len() - 1);
        match self.mode {
            SelectionMode::Multiple if mods.shift() => {
                let anchor = self.anchor.unwrap_or(index);
                self.anchor = Some(anchor);
                self.select_range(anchor, index);
            }
            SelectionMode::Multiple if mods.ctrl() => {}
            _ => {
                self.anchor = Some(index);
                self.set_selection(BTreeSet::from([index]));
            }
        }
        self.current = Some(index);
        let top = self.row_top(index);
        self.bar
            .model_mut()
            .ensure_visible(top, top + self.row_height);
    }

    fn toggle(&mut self, index: usize) {
        let mut selection = self.selected.clone();
        if !selection.remove(&index) {
            selection.insert(index);
        }
        self.anchor = Some(index);
        self.current = Some(index);
        self.set_selection(selection);
    }

    fn key(&mut self, key: &KeyEvent) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let last = self.items.len() - 1;
        let current = self.current;
        let step = |delta: isize| -> usize {
            current.map_or(0, |index| index.saturating_add_signed(delta).min(last))
        };
        let target = match key.vk {
            vk::UP => step(-1),
            vk::DOWN => step(1),
            vk::PAGE_UP => step(-(self.page_rows() as isize)),
            vk::PAGE_DOWN => step(self.page_rows() as isize),
            vk::HOME => 0,
            vk::END => last,
            vk::RETURN if key.mods.is_empty() => {
                if let Some(index) = current {
                    self.events.push(ListEvent::Activated(index));
                }
                return true;
            }
            vk::SPACE => {
                let Some(index) = current else { return true };
                if self.mode == SelectionMode::Multiple && key.mods.ctrl() {
                    self.toggle(index);
                } else {
                    self.go(index, crate::input::Modifiers::NONE);
                }
                return true;
            }
            0x41 if key.mods.ctrl() && self.mode == SelectionMode::Multiple => {
                self.set_selection((0..=last).collect());
                self.anchor = Some(0);
                return true;
            }
            _ => return false,
        };
        self.go(target, key.mods);
        true
    }

    fn pointer(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        if !self.enabled {
            return EventResult::ignored();
        }
        let bar = self.bar.event(cx, &Event::Pointer(*event));
        if self.bar.take_changed() {
            cx.invalidate(self.bounds);
        }
        if bar.handled {
            return bar;
        }
        let row = self.row_at(event.position);
        match event.kind {
            PointerEventKind::Down if event.button == Some(PointerButton::Left) => {
                let Some(index) = row else {
                    return EventResult::ignored();
                };
                if self.mode == SelectionMode::Multiple && event.mods.ctrl() && !event.mods.shift()
                {
                    self.toggle(index);
                } else {
                    self.go(index, event.mods);
                }
                self.current = Some(index);
                if event.click_count == 2 {
                    self.events.push(ListEvent::Activated(index));
                }
                cx.invalidate(self.bounds);
                EventResult::handled_with_focus()
            }
            PointerEventKind::Move | PointerEventKind::Enter | PointerEventKind::Leave => {
                let hover = if event.kind == PointerEventKind::Leave {
                    None
                } else {
                    row
                };
                if hover == self.hover {
                    return EventResult::ignored();
                }
                self.hover = hover;
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            _ => EventResult::ignored(),
        }
    }

    fn row_node(&self, cx: &AccessCx, index: usize) -> AccessNode {
        let mut node = AccessNode::new(
            self.item_ids[index],
            Role::ListItem,
            self.items[index].clone(),
            self.row_rect(index),
        );
        node.state.selected = self.selected.contains(&index);
        node.state.focusable = true;
        node.state.focused = cx.is_focused() && self.current == Some(index);
        node.state.disabled = !self.enabled;
        node.patterns.selection_item = true;
        node
    }

    /// Carries out `action` on the row whose access node has id `item`. Returns whether the
    /// row exists and supports the action.
    pub fn perform_item(
        &mut self,
        cx: &mut EventCx,
        item: WidgetId,
        action: &AccessAction,
    ) -> bool {
        let Some(index) = self.item_ids.iter().position(|id| *id == item) else {
            return false;
        };
        if !self.enabled {
            return false;
        }
        match action {
            AccessAction::Select => {
                self.go(index, crate::input::Modifiers::NONE);
                cx.focus_self();
                cx.invalidate(self.bounds);
                true
            }
            AccessAction::Invoke => {
                self.events.push(ListEvent::Activated(index));
                true
            }
            AccessAction::ScrollIntoView => {
                let top = self.row_top(index);
                self.bar
                    .model_mut()
                    .ensure_visible(top, top + self.row_height);
                cx.invalidate(self.bounds);
                true
            }
            _ => false,
        }
    }
}

impl Control for ListView {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for ListView {
    fn measure(&mut self, cx: &mut LayoutCx, available: Size) -> Size {
        let widest = self
            .items
            .iter()
            .map(|item| layout_line(cx.text, cx.theme, item, FONT_SIZE).size().width)
            .fold(0.0_f32, f32::max);
        let width = if available.width.is_finite() {
            available.width
        } else {
            widest + TEXT_INSET + 12.0 + BAR_WIDTH
        };
        let natural = self.row_top(self.items.len()).min(MAX_NATURAL_HEIGHT);
        let height = if available.height.is_finite() {
            available.height.min(self.row_top(self.items.len()))
        } else {
            natural
        };
        Size::new(width, height)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.place(bounds);
        if self.enabled {
            cx.add_tab_stop();
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        let radius = tokens.metrics.control_radius;
        canvas.push_clip_rect(self.bounds);
        for index in self.visible_range() {
            let row = self.row_rect(index);
            let pill = row.inset(Insets::symmetric(4.0, 2.0));
            let selected = self.selected.contains(&index);
            if selected {
                canvas.fill_rounded_rect(pill, CornerRadii::uniform(radius), tokens.pressed_bg);
                canvas.fill_rounded_rect(
                    Rect::new(row.x + 4.0, row.center().y - 8.0, 3.0, 16.0),
                    CornerRadii::uniform(1.5),
                    tokens.accent,
                );
            } else if self.hover == Some(index) && self.enabled {
                canvas.fill_rounded_rect(pill, CornerRadii::uniform(radius), tokens.hover_bg);
            }
            let color = if self.enabled {
                tokens.text
            } else {
                tokens.text_disabled
            };
            let text = layout_line(cx.text, cx.theme, &self.items[index], FONT_SIZE);
            draw_left_centered(canvas, text.as_ref(), row.x + TEXT_INSET, row, color);
            if cx.is_focused() && self.current == Some(index) {
                focus_ring::paint(canvas, pill, radius, cx.theme);
            }
        }
        canvas.pop_clip();
        self.bar.paint(&cx.for_widget(self.bar.id()), canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => self.pointer(cx, event),
            Event::Wheel(event) if self.enabled && self.bounds.contains(event.position) => {
                if self.bar.wheel(cx, event) {
                    self.bar.take_changed();
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            Event::Key(key) if key.is_down && self.enabled && cx.is_focused() => {
                if self.key(key) {
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            Event::Text(text) if self.enabled && cx.is_focused() => {
                if !self.type_ahead.push(cx, &text.0) {
                    return EventResult::ignored();
                }
                let items = &self.items;
                let found = self
                    .type_ahead
                    .find(items.len(), self.current, |index, needle| {
                        items[index].to_lowercase().starts_with(needle)
                    });
                if let Some(index) = found {
                    self.go(index, crate::input::Modifiers::NONE);
                    cx.invalidate(self.bounds);
                }
                EventResult::handled()
            }
            Event::Timer(id) => {
                self.type_ahead.on_timer(*id);
                EventResult::ignored()
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) => {
                cx.invalidate(self.bounds);
                EventResult::ignored()
            }
            Event::AnimationFrame(now) => {
                if self.bar.animate(cx, *now) {
                    cx.invalidate(self.bounds);
                }
                EventResult::ignored()
            }
            _ => EventResult::ignored(),
        }
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let range = self.visible_range();
        let mut children = Vec::with_capacity(range.len());
        for index in range {
            out.insert(self.row_node(cx, index));
            children.push(self.item_ids[index]);
        }
        let mut node = AccessNode::new(self.id, Role::List, self.label.clone(), self.bounds)
            .with_state(control_state(cx, self.enabled))
            .with_children(children);
        node.state.focused = cx.is_focused() && self.current.is_none();
        node.patterns.scroll = Some(scroll_info(&Default::default(), self.bar.model()));
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::Focus if self.enabled => perform_focus(cx),
            AccessAction::ScrollTo {
                vertical: Some(percent),
                ..
            } if self.enabled => {
                self.bar.model_mut().jump_percent(*percent);
                cx.invalidate(self.bounds);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::input::{Modifiers, PointerType};
    use crate::widgets::testkit::Harness;

    fn names(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("Item {index}")).collect()
    }

    /// Ten rows of 32 DIPs in a 96 DIP viewport at (0, 0), 200 wide.
    fn setup(mode: SelectionMode) -> (Harness, ListView) {
        let mut harness = Harness::new();
        let mut list = ListView::new("Files", names(10), mode).with_motion(Motion::reduced());
        harness.arrange(&mut list, Rect::new(0.0, 0.0, 200.0, 96.0));
        (harness, list)
    }

    fn click_with(
        harness: &mut Harness,
        list: &mut ListView,
        row: usize,
        mods: Modifiers,
        count: u8,
    ) {
        let at = list.row_rect(row).center();
        harness.send(
            list,
            Event::Pointer(PointerEvent {
                kind: PointerEventKind::Down,
                button: Some(PointerButton::Left),
                position: at,
                pointer_type: PointerType::Mouse,
                click_count: count,
                mods,
            }),
        );
    }

    #[test]
    fn kit_list_measures_natural_and_constrained_heights() {
        let mut harness = Harness::new();
        let mut list = ListView::new("x", names(3), SelectionMode::Single);
        let size = harness.place(&mut list, Point::ZERO).size();
        // "Item 0" is six glyphs of 8 DIPs.
        assert_eq!(size, Size::new(48.0 + 16.0 + 12.0 + 12.0, 96.0));
        let mut long = ListView::new("x", names(50), SelectionMode::Single);
        assert_eq!(harness.place(&mut long, Point::ZERO).height, 320.0);
        let id = long.id();
        let mut cx = LayoutCx::new(
            id,
            &harness.theme,
            1.0,
            &harness.engine,
            &mut harness.focus,
            &mut harness.requests,
        );
        assert_eq!(
            long.measure(&mut cx, Size::new(150.0, 200.0)),
            Size::new(150.0, 200.0)
        );
        assert_eq!(
            list.measure(&mut cx, Size::new(150.0, 500.0)),
            Size::new(150.0, 96.0)
        );
        assert_eq!((list.len(), list.is_empty()), (3, false));
    }

    #[test]
    fn kit_list_single_selection_keyboard_navigation() {
        let (mut harness, mut list) = setup(SelectionMode::Single);
        assert!(!harness.key(&mut list, vk::DOWN).handled, "needs focus");
        harness.focus_widget(&mut list);
        harness.key(&mut list, vk::DOWN);
        assert_eq!((list.current(), list.selected()), (Some(0), vec![0]));
        harness.key(&mut list, vk::DOWN);
        harness.key(&mut list, vk::DOWN);
        assert_eq!(list.selected(), vec![2]);
        harness.key(&mut list, vk::UP);
        assert_eq!(list.selected_index(), Some(1));
        harness.key(&mut list, vk::END);
        assert_eq!(list.selected(), vec![9]);
        // The viewport followed the selection to the end.
        assert_eq!(list.scroll_offset(), 320.0 - 96.0);
        assert_eq!(list.visible_range(), 7..10);
        harness.key(&mut list, vk::DOWN);
        assert_eq!(list.selected(), vec![9]);
        harness.key(&mut list, vk::HOME);
        assert_eq!(list.selected(), vec![0]);
        assert_eq!(list.scroll_offset(), 0.0);
        harness.key(&mut list, vk::PAGE_DOWN);
        assert_eq!(list.selected(), vec![2]);
        harness.key(&mut list, vk::PAGE_UP);
        assert_eq!(list.selected(), vec![0]);
        harness.key(&mut list, vk::UP);
        assert_eq!(list.selected(), vec![0]);
        assert!(!harness.key(&mut list, vk::TAB).handled);
        assert_eq!(
            list.take_events()
                .iter()
                .filter(|e| **e == ListEvent::SelectionChanged)
                .count(),
            8
        );
        assert!(list.take_events().is_empty());
    }

    #[test]
    fn kit_list_enter_space_and_double_click_activate_or_select() {
        let (mut harness, mut list) = setup(SelectionMode::Single);
        harness.focus_widget(&mut list);
        // Nothing is current yet.
        assert!(harness.key(&mut list, vk::RETURN).handled);
        assert!(harness.key(&mut list, vk::SPACE).handled);
        assert!(list.take_events().is_empty());
        harness.key(&mut list, vk::DOWN);
        list.take_events();
        assert!(harness.key(&mut list, vk::RETURN).handled);
        assert_eq!(list.take_events(), vec![ListEvent::Activated(0)]);
        assert!(
            !harness
                .key_with(&mut list, vk::RETURN, Modifiers::ALT)
                .handled
        );
        click_with(&mut harness, &mut list, 2, Modifiers::NONE, 2);
        assert_eq!(
            list.take_events(),
            vec![ListEvent::SelectionChanged, ListEvent::Activated(2)]
        );
        // Space re-selects the current row in single mode.
        harness.key(&mut list, vk::SPACE);
        assert!(list.take_events().is_empty());
    }

    #[test]
    fn kit_list_multi_selection_with_pointer_and_keyboard() {
        let (mut harness, mut list) = setup(SelectionMode::Multiple);
        // The default viewport shows three rows; this scenario clicks rows up to 7, so make all
        // ten rows (320 DIPs) visible.
        harness.arrange(&mut list, Rect::new(0.0, 0.0, 200.0, 320.0));
        harness.focus_widget(&mut list);
        click_with(&mut harness, &mut list, 1, Modifiers::NONE, 1);
        assert_eq!(list.selected(), vec![1]);
        click_with(&mut harness, &mut list, 3, Modifiers::CTRL, 1);
        assert_eq!(list.selected(), vec![1, 3]);
        click_with(&mut harness, &mut list, 1, Modifiers::CTRL, 1);
        assert_eq!(list.selected(), vec![3]);
        // Shift extends from the anchor set by the last plain click.
        click_with(&mut harness, &mut list, 3, Modifiers::NONE, 1);
        click_with(&mut harness, &mut list, 0, Modifiers::SHIFT, 1);
        assert_eq!(list.selected(), vec![0, 1, 2, 3]);
        assert!(!harness.key(&mut list, 0x41).handled);
        assert_eq!(list.selected().len(), 4);
        harness.key_with(&mut list, 0x41, Modifiers::CTRL);
        assert_eq!(list.selected().len(), 10);
        // Shift+Down extends from the anchor; Ctrl+Down moves only the cursor.
        click_with(&mut harness, &mut list, 4, Modifiers::NONE, 1);
        harness.key_with(&mut list, vk::DOWN, Modifiers::SHIFT);
        harness.key_with(&mut list, vk::DOWN, Modifiers::SHIFT);
        assert_eq!(list.selected(), vec![4, 5, 6]);
        harness.key_with(&mut list, vk::DOWN, Modifiers::CTRL);
        assert_eq!((list.current(), list.selected()), (Some(7), vec![4, 5, 6]));
        harness.key_with(&mut list, vk::SPACE, Modifiers::CTRL);
        assert_eq!(list.selected(), vec![4, 5, 6, 7]);
        harness.key_with(&mut list, vk::SPACE, Modifiers::CTRL);
        assert_eq!(list.selected(), vec![4, 5, 6]);
        harness.key(&mut list, vk::SPACE);
        assert_eq!(list.selected(), vec![7]);
        // Ctrl+A is only for multiple selection.
        let (mut harness, mut single) = setup(SelectionMode::Single);
        harness.focus_widget(&mut single);
        assert!(!harness.key_with(&mut single, 0x41, Modifiers::CTRL).handled);
    }

    #[test]
    fn kit_list_type_ahead_selects_by_prefix_and_times_out() {
        let mut harness = Harness::new();
        let items = ["Apple", "Avocado", "Banana", "Blueberry", "Cherry"]
            .map(String::from)
            .to_vec();
        let mut list =
            ListView::new("Fruit", items, SelectionMode::Single).with_motion(Motion::reduced());
        harness.arrange(&mut list, Rect::new(0.0, 0.0, 200.0, 160.0));
        assert!(!harness.text(&mut list, "b").handled, "needs focus");
        harness.focus_widget(&mut list);
        assert!(harness.text(&mut list, "b").handled);
        assert_eq!(list.selected(), vec![2]);
        // Typing on within the timeout narrows the prefix to "bl".
        assert!(harness.text(&mut list, "l").handled);
        assert_eq!(list.selected(), vec![3]);
        assert_eq!(list.type_ahead.buffer(), "bl");
        // No match keeps the selection.
        assert!(harness.text(&mut list, "z").handled);
        assert_eq!(list.selected(), vec![3]);
        // The reset timer starts a fresh search.
        let timer = list.type_ahead.timer;
        harness.send(&mut list, Event::Timer(timer));
        assert_eq!(list.type_ahead.buffer(), "");
        assert!(harness.text(&mut list, "c").handled);
        assert_eq!(list.selected(), vec![4]);
        // A leading blank is not a search.
        harness.send(&mut list, Event::Timer(timer));
        assert!(!harness.text(&mut list, " ").handled);
        // Foreign timers are ignored.
        assert!(
            !harness
                .send(
                    &mut list,
                    Event::Timer(crate::services::TimerId::allocate())
                )
                .handled
        );
    }

    #[test]
    fn kit_list_paints_visible_rows_selection_hover_and_focus() {
        let (mut harness, mut list) = setup(SelectionMode::Single);
        let tokens = harness.theme.tokens;
        let canvas = harness.paint(&list);
        assert_eq!(canvas.texts(), vec!["Item 0", "Item 1", "Item 2"]);
        assert!(
            matches!(canvas.ops().next(), Some(DrawOp::PushClipRect(rect)) if *rect == Rect::new(0.0, 0.0, 200.0, 96.0))
        );
        harness.focus_widget(&mut list);
        harness.key(&mut list, vk::DOWN);
        let third = list.row_rect(2).center();
        harness.hover(&mut list, third);
        let canvas = harness.paint(&list);
        let fills: Vec<(Rect, crate::geometry::Color)> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::FillRoundedRect { rect, color, .. } => Some((*rect, *color)),
                _ => None,
            })
            .collect();
        assert_eq!(
            fills[0],
            (Rect::new(4.0, 2.0, 192.0, 28.0), tokens.pressed_bg)
        );
        assert_eq!(
            fills[1],
            (Rect::new(4.0, 16.0 - 8.0, 3.0, 16.0), tokens.accent)
        );
        assert_eq!(
            fills[2],
            (Rect::new(4.0, 66.0, 192.0, 28.0), tokens.hover_bg)
        );
        let rings = canvas
            .ops()
            .filter(|op| matches!(op, DrawOp::StrokeRoundedRect { width, .. } if *width == 2.0))
            .count();
        assert_eq!(rings, 1);
        // Scrolled rows paint from the new range.
        harness.key(&mut list, vk::END);
        let canvas = harness.paint(&list);
        assert_eq!(canvas.texts(), vec!["Item 7", "Item 8", "Item 9"]);
        // A disabled list is grey and shows no hover.
        list.set_enabled(false);
        let ninth = list.row_rect(8).center();
        harness.hover(&mut list, ninth);
        let canvas = harness.paint(&list);
        assert!(
            canvas
                .ops()
                .all(|op| !matches!(op, DrawOp::Text { color, .. } if *color == tokens.text))
        );
        assert!(
            canvas.ops().any(
                |op| matches!(op, DrawOp::Text { color, .. } if *color == tokens.text_disabled)
            )
        );
    }

    #[test]
    fn kit_list_hover_wheel_and_scrollbar() {
        let (mut harness, mut list) = setup(SelectionMode::Single);
        let second = list.row_rect(1).center();
        assert!(harness.hover(&mut list, second).handled);
        assert!(!harness.hover(&mut list, Point::new(10.0, 60.0)).handled);
        assert!(
            harness
                .pointer(&mut list, PointerEventKind::Leave, None, Point::ZERO)
                .handled
        );
        assert!(
            !harness
                .pointer(&mut list, PointerEventKind::Leave, None, Point::ZERO)
                .handled
        );
        let wheel = |dy: f32, x: f32| {
            Event::Wheel(crate::input::WheelEvent {
                delta: Point::new(0.0, dy),
                precise: true,
                position: Point::new(x, 50.0),
                mods: Modifiers::NONE,
            })
        };
        assert!(harness.send(&mut list, wheel(-40.0, 50.0)).handled);
        assert_eq!(list.scroll_offset(), 40.0);
        assert_eq!(list.visible_range(), 1..5);
        assert!(!harness.send(&mut list, wheel(-40.0, 500.0)).handled);
        // Pressing the scroll bar's track pages the list instead of selecting.
        let before = list.selected();
        harness.pointer(
            &mut list,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            Point::new(195.0, 90.0),
        );
        assert!(list.scroll_offset() > 40.0);
        assert_eq!(list.selected(), before);
        harness.pointer(
            &mut list,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::new(195.0, 90.0),
        );
        // Clicks on empty space or with another button do nothing.
        let (mut harness, mut short) = setup(SelectionMode::Single);
        short.set_items(names(1));
        harness.arrange(&mut short, Rect::new(0.0, 0.0, 200.0, 96.0));
        assert!(
            !harness
                .pointer(
                    &mut short,
                    PointerEventKind::Down,
                    Some(PointerButton::Left),
                    Point::new(50.0, 80.0)
                )
                .handled
        );
        assert!(
            !harness
                .pointer(
                    &mut short,
                    PointerEventKind::Down,
                    Some(PointerButton::Right),
                    Point::new(50.0, 10.0)
                )
                .handled
        );
        assert!(
            !harness
                .pointer(
                    &mut short,
                    PointerEventKind::Up,
                    Some(PointerButton::Left),
                    Point::new(50.0, 10.0)
                )
                .handled
        );
    }

    #[test]
    fn kit_list_access_nodes_for_visible_rows_and_actions() {
        let (mut harness, mut list) = setup(SelectionMode::Single);
        harness.focus_widget(&mut list);
        harness.key(&mut list, vk::DOWN);
        harness.key(&mut list, vk::DOWN);
        let tree = harness.tree(&list);
        let root = tree.root().expect("root");
        assert_eq!((root.role, root.name.as_str()), (Role::List, "Files"));
        assert_eq!(root.children.len(), 3);
        assert!(!root.state.focused, "focus sits on the current item");
        let info = root.patterns.scroll.expect("scroll");
        assert!(info.vertically_scrollable && !info.horizontally_scrollable);
        let rows: Vec<_> = tree.children(list.id()).collect();
        assert_eq!(rows[1].role, Role::ListItem);
        assert_eq!(rows[1].name, "Item 1");
        assert!(rows[1].state.selected && rows[1].state.focused);
        assert!(!rows[0].state.selected && !rows[0].state.focused);
        assert!(rows[0].patterns.selection_item);
        assert_eq!(rows[2].bounds, Rect::new(0.0, 64.0, 200.0, 32.0));
        assert_eq!(tree.focused().map(|n| n.name.as_str()), Some("Item 1"));
        // The list itself holds focus when nothing is current.
        let (mut h2, mut fresh) = setup(SelectionMode::Single);
        h2.focus_widget(&mut fresh);
        assert!(h2.node(&fresh).state.focused);
        // Item actions.
        let last = list.item_ids[9];
        let first = list.item_ids[0];
        let id = list.id();
        let mut cx = harness.event_cx(id);
        assert!(list.perform_item(&mut cx, last, &AccessAction::ScrollIntoView));
        assert!(list.perform_item(&mut cx, first, &AccessAction::Select));
        assert!(list.perform_item(&mut cx, first, &AccessAction::Invoke));
        assert!(!list.perform_item(&mut cx, first, &AccessAction::Toggle));
        assert!(!list.perform_item(&mut cx, WidgetId::allocate(), &AccessAction::Select));
        assert_eq!(list.selected(), vec![0]);
        assert_eq!(list.take_events().last(), Some(&ListEvent::Activated(0)));
        list.set_enabled(false);
        assert!(!list.perform_item(&mut cx, first, &AccessAction::Select));
        list.set_enabled(true);
        // The list's own actions.
        assert!(harness.perform(
            &mut list,
            AccessAction::ScrollTo {
                horizontal: None,
                vertical: Some(100.0)
            }
        ));
        assert_eq!(list.scroll_offset(), 224.0);
        assert!(harness.perform(&mut list, AccessAction::Focus));
        assert!(!harness.perform(&mut list, AccessAction::Invoke));
        list.set_enabled(false);
        harness.arrange(&mut list, Rect::new(0.0, 0.0, 200.0, 96.0));
        assert!(!harness.perform(&mut list, AccessAction::Focus));
        assert!(!harness.perform(
            &mut list,
            AccessAction::ScrollTo {
                horizontal: None,
                vertical: Some(0.0)
            }
        ));
        assert!(harness.node(&list).state.disabled);
        assert!(!harness.focus.can_focus(list.id()));
        assert!(!harness.click(&mut list, Point::new(10.0, 10.0)).handled);
        assert!(!list.is_enabled());
    }

    #[test]
    fn kit_list_set_items_and_programmatic_select() {
        let (mut harness, mut list) = setup(SelectionMode::Single);
        list.select(8);
        assert_eq!((list.selected(), list.current()), (vec![8], Some(8)));
        assert!(list.scroll_offset() > 0.0);
        list.select(99);
        assert!(list.selected().is_empty());
        list.set_items(names(2));
        harness.arrange(&mut list, Rect::new(0.0, 0.0, 200.0, 96.0));
        assert_eq!((list.items().len(), list.scroll_offset()), (2, 0.0));
        assert_eq!(list.visible_range(), 0..2);
        let mut empty = ListView::new("e", Vec::new(), SelectionMode::Multiple);
        harness.arrange(&mut empty, Rect::new(0.0, 0.0, 100.0, 50.0));
        harness.focus_widget(&mut empty);
        assert!(!harness.key(&mut empty, vk::DOWN).handled);
        assert!(harness.text(&mut empty, "a").handled);
        assert!(empty.selected().is_empty());
        click_with(&mut harness, &mut empty, 0, Modifiers::NONE, 1);
        assert!(empty.selected().is_empty());
        assert_eq!(harness.node(&empty).children.len(), 0);
    }
}
