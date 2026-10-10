//! The Fluent combo box: a closed box showing the selection and a drop-down popup list.
//!
//! The popup is part of the combo box: [`ComboBox::paint`] draws it above the box (the host
//! paints open combo boxes last so nothing covers it) and [`ComboBox::popup_bounds`] tells the
//! host where it hit-tests and invalidates.

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, IconFont};
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::{
    Event, FocusEvent, KeyEvent, PointerButton, PointerEvent, PointerEventKind, vk,
};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::anim::Motion;
use super::list::{ListView, ROW_HEIGHT, SelectionMode};
use super::{
    CONTROL_HEIGHT, CONTROL_PADDING, Control, FONT_SIZE, Interaction, PointerOutcome, TypeAhead,
    control_state, draw_left_centered, focus_ring, glyph, layout_line, outcome_result,
    paint_overlay, perform_focus,
};

/// Rows the popup shows before it scrolls.
pub const MAX_POPUP_ROWS: usize = 8;
/// Narrowest closed box.
const MIN_WIDTH: f32 = 120.0;
/// Em size of the chevron.
const CHEVRON_SIZE: f32 = 12.0;
/// Gap between the box and its popup.
const POPUP_GAP: f32 = 2.0;
/// Padding above and below the popup's rows.
const POPUP_PADDING: f32 = 4.0;

/// A combo box over a fixed list of items.
#[derive(Debug, Clone)]
pub struct ComboBox {
    id: WidgetId,
    label: String,
    items: Vec<String>,
    selected: Option<usize>,
    enabled: bool,
    bounds: Rect,
    motion: Motion,
    interaction: Interaction,
    popup: Option<ListView>,
    type_ahead: TypeAhead,
    changed: Option<usize>,
}

impl ComboBox {
    /// A combo box over `items`; `label` is the accessible name.
    #[must_use]
    pub fn new(label: impl Into<String>, items: Vec<String>, selected: Option<usize>) -> Self {
        let selected = selected.filter(|index| *index < items.len());
        Self {
            id: WidgetId::allocate(),
            label: label.into(),
            items,
            selected,
            enabled: true,
            bounds: Rect::ZERO,
            motion: Motion::full(),
            interaction: Interaction::new(Motion::full()),
            popup: None,
            type_ahead: TypeAhead::new(),
            changed: None,
        }
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.motion = motion;
        self.interaction.set_motion(motion);
        self
    }

    #[must_use]
    pub fn items(&self) -> &[String] {
        &self.items
    }

    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// The text of the selected item.
    #[must_use]
    pub fn selected_text(&self) -> Option<&str> {
        self.selected.map(|index| self.items[index].as_str())
    }

    /// Selects `index` without notifying the owner; out-of-range indices clear the selection.
    pub fn select(&mut self, index: Option<usize>) {
        self.selected = index.filter(|index| *index < self.items.len());
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.popup.is_some()
    }

    /// The row the popup highlights while open.
    #[must_use]
    pub fn highlighted(&self) -> Option<usize> {
        self.popup.as_ref().and_then(ListView::selected_index)
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

    /// Where the popup is while open (below the box).
    #[must_use]
    pub fn popup_bounds(&self) -> Option<Rect> {
        self.popup.as_ref().map(|_| self.popup_rect())
    }

    /// The new selection if the user changed it since the last call.
    pub fn take_changed(&mut self) -> Option<usize> {
        self.changed.take()
    }

    fn popup_rect(&self) -> Rect {
        let rows = self.items.len().min(MAX_POPUP_ROWS);
        Rect::new(
            self.bounds.x,
            self.bounds.bottom() + POPUP_GAP,
            self.bounds.width,
            rows as f32 * ROW_HEIGHT + 2.0 * POPUP_PADDING,
        )
    }

    fn open(&mut self, cx: &mut EventCx) {
        if self.popup.is_some() || self.items.is_empty() {
            return;
        }
        let rect = self.popup_rect();
        let mut list = ListView::new(String::new(), self.items.clone(), SelectionMode::Single)
            .with_motion(self.motion);
        list.place(rect.inset(Insets::symmetric(0.0, POPUP_PADDING)));
        if let Some(index) = self.selected {
            list.select(index);
        }
        self.popup = Some(list);
        cx.invalidate(rect.outset(Insets::uniform(4.0)));
        cx.invalidate(self.bounds);
    }

    fn close(&mut self, cx: &mut EventCx) {
        if self.popup.take().is_some() {
            cx.invalidate(self.popup_rect().outset(Insets::uniform(4.0)));
            cx.invalidate(self.bounds);
        }
    }

    fn commit(&mut self, cx: &mut EventCx, index: usize) {
        if self.selected != Some(index) {
            self.selected = Some(index);
            self.changed = Some(index);
        }
        self.close(cx);
        cx.invalidate(self.bounds);
    }

    /// Moves the selection (closed) or the highlight (open) to `index`.
    fn move_to(&mut self, cx: &mut EventCx, index: usize) {
        let index = index.min(self.items.len().saturating_sub(1));
        match self.popup.as_mut() {
            Some(list) => list.select(index),
            None => {
                if self.selected != Some(index) {
                    self.selected = Some(index);
                    self.changed = Some(index);
                }
            }
        }
        cx.invalidate(self.bounds);
        if let Some(rect) = self.popup_bounds() {
            cx.invalidate(rect);
        }
    }

    fn current(&self) -> Option<usize> {
        self.highlighted().or(self.selected)
    }

    fn key(&mut self, cx: &mut EventCx, key: &KeyEvent) -> EventResult {
        let alt = key.mods.alt();
        if !key.mods.is_empty() && !alt {
            return EventResult::ignored();
        }
        let open = self.is_open();
        let last = self.items.len().saturating_sub(1);
        let current = self.current();
        match key.vk {
            vk::DOWN if alt => {
                if open {
                    self.close(cx);
                } else {
                    self.open(cx);
                }
            }
            vk::UP if alt && open => self.close(cx),
            vk::F4 if key.mods.is_empty() => {
                if open {
                    self.close(cx);
                } else {
                    self.open(cx);
                }
            }
            _ if alt => return EventResult::ignored(),
            vk::DOWN | vk::UP | vk::HOME | vk::END | vk::PAGE_UP | vk::PAGE_DOWN
                if self.items.is_empty() => {}
            vk::DOWN => self.move_to(cx, current.map_or(0, |index| index + 1)),
            vk::UP => self.move_to(cx, current.map_or(last, |index| index.saturating_sub(1))),
            vk::PAGE_DOWN => self.move_to(cx, current.map_or(0, |index| index + MAX_POPUP_ROWS)),
            vk::PAGE_UP => self.move_to(
                cx,
                current.map_or(last, |index| index.saturating_sub(MAX_POPUP_ROWS)),
            ),
            vk::HOME => self.move_to(cx, 0),
            vk::END => self.move_to(cx, last),
            vk::RETURN | vk::SPACE => match self.highlighted() {
                Some(index) => self.commit(cx, index),
                None if open => self.close(cx),
                None => self.open(cx),
            },
            vk::ESCAPE if open => self.close(cx),
            vk::TAB if open => {
                if let Some(index) = self.highlighted() {
                    self.commit(cx, index);
                } else {
                    self.close(cx);
                }
                // Tab still moves focus on.
                return EventResult::ignored();
            }
            _ => return EventResult::ignored(),
        }
        EventResult::handled()
    }

    fn pointer_closed(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        let outcome = self
            .interaction
            .pointer(cx, self.bounds, event, self.enabled);
        if outcome == PointerOutcome::Clicked {
            self.open(cx);
        }
        outcome_result(outcome)
    }

    fn pointer_open(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        let popup_rect = self.popup_rect();
        let on_box = self.bounds.contains(event.position);
        let on_popup = popup_rect.contains(event.position);
        let down =
            event.kind == PointerEventKind::Down && event.button == Some(PointerButton::Left);
        if down && !on_popup {
            // A press on the box toggles the popup, one anywhere else dismisses it.
            self.close(cx);
            return if on_box {
                EventResult::handled_with_focus()
            } else {
                EventResult::handled()
            };
        }
        let Some(list) = self.popup.as_mut() else {
            return EventResult::ignored();
        };
        let list_id = list.id();
        let result = list.event(&mut cx.for_widget(list_id), &Event::Pointer(*event));
        let row = if down && !list.scrollbar_contains(event.position) {
            list.visible_range()
                .find(|&index| list.row_rect(index).contains(event.position))
        } else {
            None
        };
        if let Some(index) = row {
            self.commit(cx, index);
            return EventResult::handled_with_focus();
        }
        if result.handled || on_popup || on_box {
            EventResult::handled()
        } else {
            EventResult::ignored()
        }
    }

    fn text_input(&mut self, cx: &mut EventCx, text: &str) -> EventResult {
        if !self.type_ahead.push(cx, text) {
            return EventResult::ignored();
        }
        let items = &self.items;
        let found = self
            .type_ahead
            .find(items.len(), self.current(), |index, needle| {
                items[index].to_lowercase().starts_with(needle)
            });
        if let Some(index) = found {
            self.move_to(cx, index);
        }
        EventResult::handled()
    }
}

impl Control for ComboBox {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for ComboBox {
    fn measure(&mut self, cx: &mut LayoutCx, _available: Size) -> Size {
        let widest = self
            .items
            .iter()
            .map(|item| layout_line(cx.text, cx.theme, item, FONT_SIZE).size().width)
            .fold(0.0_f32, f32::max);
        let width = widest + CONTROL_PADDING + CHEVRON_SIZE + 2.0 * CONTROL_PADDING;
        Size::new(width.max(MIN_WIDTH), CONTROL_HEIGHT)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        if self.enabled {
            cx.add_tab_stop();
        }
        if self.popup.is_some() {
            let rect = self.popup_rect();
            if let Some(list) = self.popup.as_mut() {
                list.place(rect.inset(Insets::symmetric(0.0, POPUP_PADDING)));
            }
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        if cx.needs_paint(self.bounds) {
            let radii = CornerRadii::uniform(tokens.metrics.control_radius);
            let fill = if self.enabled {
                self.interaction.blend(
                    tokens.surface,
                    tokens.hover_bg.over(tokens.surface),
                    tokens.pressed_bg.over(tokens.surface),
                )
            } else {
                tokens.surface
            };
            canvas.fill_rounded_rect(self.bounds, radii, fill);
            canvas.stroke_rounded_rect(
                self.bounds.inset(Insets::uniform(0.5)),
                radii,
                tokens.hairline,
                1.0,
            );
            let color = if self.enabled {
                tokens.text
            } else {
                tokens.text_disabled
            };
            if let Some(text) = self.selected_text() {
                let layout = layout_line(cx.text, cx.theme, text, FONT_SIZE);
                draw_left_centered(
                    canvas,
                    layout.as_ref(),
                    self.bounds.x + CONTROL_PADDING,
                    self.bounds,
                    color,
                );
            }
            canvas.draw_icon(
                glyph::CHEVRON_DOWN,
                IconFont::SegoeFluent,
                CHEVRON_SIZE,
                color,
                Point::new(
                    self.bounds.right() - CONTROL_PADDING - CHEVRON_SIZE / 2.0,
                    self.bounds.center().y,
                ),
            );
            if cx.is_focused() {
                focus_ring::paint(canvas, self.bounds, tokens.metrics.control_radius, cx.theme);
            }
        }
        if let Some(list) = &self.popup {
            paint_overlay(canvas, self.popup_rect(), tokens.menu_bg, cx.theme);
            list.paint(&cx.for_widget(list.id()), canvas);
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) if self.is_open() && self.enabled => self.pointer_open(cx, event),
            Event::Pointer(event) => self.pointer_closed(cx, event),
            Event::Wheel(event) if self.is_open() => match self.popup.as_mut() {
                Some(list)
                    if self.bounds.contains(event.position)
                        || list.bounds().contains(event.position) =>
                {
                    let list_id = list.id();
                    list.event(&mut cx.for_widget(list_id), e)
                }
                _ => EventResult::ignored(),
            },
            Event::Key(key) if key.is_down && self.enabled && cx.is_focused() => self.key(cx, key),
            Event::Text(text) if self.enabled && cx.is_focused() => self.text_input(cx, &text.0),
            Event::Timer(id) => {
                self.type_ahead.on_timer(*id);
                EventResult::ignored()
            }
            Event::Focus(FocusEvent::Lost) => {
                self.close(cx);
                cx.invalidate(self.bounds.outset(Insets::uniform(2.0)));
                EventResult::ignored()
            }
            Event::Focus(FocusEvent::Gained) => {
                cx.invalidate(self.bounds.outset(Insets::uniform(2.0)));
                EventResult::ignored()
            }
            Event::AnimationFrame(now) => {
                if self.interaction.animate(cx, *now) {
                    cx.invalidate(self.bounds);
                }
                match self.popup.as_mut() {
                    Some(list) => {
                        let list_id = list.id();
                        list.event(&mut cx.for_widget(list_id), e)
                    }
                    None => EventResult::ignored(),
                }
            }
            _ => EventResult::ignored(),
        }
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let mut state = control_state(cx, self.enabled);
        state.expanded = Some(self.is_open());
        state.read_only = true;
        let mut node = AccessNode::new(self.id, Role::ComboBox, self.label.clone(), self.bounds)
            .with_state(state)
            .with_value(self.selected_text().unwrap_or_default());
        node.patterns.expand_collapse = true;
        node.patterns.value = true;
        if let Some(list) = &self.popup {
            list.access(&cx.for_widget(list.id()), out);
            node.children.push(list.id());
        }
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        if !self.enabled {
            return false;
        }
        match a {
            AccessAction::Expand => {
                self.open(cx);
                self.is_open()
            }
            AccessAction::Collapse => {
                self.close(cx);
                true
            }
            AccessAction::Focus => perform_focus(cx),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::input::Modifiers;
    use crate::widgets::testkit::Harness;

    fn fruit() -> Vec<String> {
        ["Apple", "Avocado", "Banana", "Blueberry", "Cherry"]
            .map(String::from)
            .to_vec()
    }

    fn setup(selected: Option<usize>) -> (Harness, ComboBox, Rect) {
        let mut harness = Harness::new();
        let mut combo = ComboBox::new("Fruit", fruit(), selected).with_motion(Motion::reduced());
        let bounds = harness.place(&mut combo, Point::new(10.0, 10.0));
        (harness, combo, bounds)
    }

    #[test]
    fn kit_combo_measures_to_fit_the_widest_item() {
        let (_, combo, bounds) = setup(None);
        // "Blueberry" is nine glyphs; 72 + 11 + 12 + 22 < the 120 minimum.
        assert_eq!(bounds.size(), Size::new(120.0, 32.0));
        let mut harness = Harness::new();
        let mut wide = ComboBox::new("x", vec!["a".repeat(20)], None);
        assert_eq!(
            harness.place(&mut wide, Point::ZERO).width,
            160.0 + 11.0 + 12.0 + 22.0
        );
        assert_eq!(combo.items().len(), 5);
        assert_eq!(ComboBox::new("x", fruit(), Some(9)).selected(), None);
    }

    #[test]
    fn kit_combo_closed_arrow_keys_change_the_selection() {
        let (mut harness, mut combo, _) = setup(None);
        assert!(!harness.key(&mut combo, vk::DOWN).handled, "needs focus");
        harness.focus_widget(&mut combo);
        let table = [
            (vk::DOWN, 0),
            (vk::DOWN, 1),
            (vk::UP, 0),
            (vk::UP, 0),
            (vk::END, 4),
            (vk::DOWN, 4),
            (vk::PAGE_UP, 0),
            (vk::PAGE_DOWN, 4),
            (vk::HOME, 0),
        ];
        for (key, expected) in table {
            assert!(harness.key(&mut combo, key).handled);
            assert_eq!(combo.selected(), Some(expected), "key {key:#x}");
        }
        assert_eq!(combo.take_changed(), Some(0));
        assert_eq!(combo.take_changed(), None);
        assert_eq!(combo.selected_text(), Some("Apple"));
        // Up from nothing goes to the last item.
        let (mut harness, mut fresh, _) = setup(None);
        harness.focus_widget(&mut fresh);
        harness.key(&mut fresh, vk::UP);
        assert_eq!(fresh.selected(), Some(4));
        let (mut harness, mut fresh, _) = setup(None);
        harness.focus_widget(&mut fresh);
        harness.key(&mut fresh, vk::PAGE_UP);
        assert_eq!(fresh.selected(), Some(4));
        assert!(!harness.key(&mut fresh, vk::TAB).handled);
        assert!(
            !harness
                .key_with(&mut fresh, vk::DOWN, Modifiers::CTRL)
                .handled
        );
        assert!(
            !harness
                .key_with(&mut fresh, vk::LEFT, Modifiers::ALT)
                .handled
        );
        assert!(!harness.key(&mut fresh, vk::ESCAPE).handled);
    }

    #[test]
    fn kit_combo_opens_navigates_and_commits_with_the_keyboard() {
        let (mut harness, mut combo, bounds) = setup(Some(1));
        harness.focus_widget(&mut combo);
        assert!(
            harness
                .key_with(&mut combo, vk::DOWN, Modifiers::ALT)
                .handled
        );
        assert!(combo.is_open());
        assert_eq!(combo.highlighted(), Some(1));
        assert_eq!(
            combo.popup_bounds(),
            Some(Rect::new(
                10.0,
                bounds.bottom() + 2.0,
                120.0,
                5.0 * 32.0 + 8.0
            ))
        );
        harness.key(&mut combo, vk::DOWN);
        harness.key(&mut combo, vk::DOWN);
        assert_eq!((combo.highlighted(), combo.selected()), (Some(3), Some(1)));
        harness.key(&mut combo, vk::UP);
        harness.key(&mut combo, vk::END);
        harness.key(&mut combo, vk::HOME);
        harness.key(&mut combo, vk::PAGE_DOWN);
        harness.key(&mut combo, vk::PAGE_UP);
        assert_eq!(combo.highlighted(), Some(0));
        harness.key(&mut combo, vk::DOWN);
        assert!(harness.key(&mut combo, vk::RETURN).handled);
        assert!(!combo.is_open());
        assert_eq!(combo.selected(), Some(1));
        // Escape cancels, F4 and Alt+Up close, Space and Enter open.
        harness.key(&mut combo, vk::SPACE);
        harness.key(&mut combo, vk::DOWN);
        harness.key(&mut combo, vk::ESCAPE);
        assert!(!combo.is_open());
        assert_eq!(combo.selected(), Some(1));
        harness.key(&mut combo, vk::F4);
        assert!(combo.is_open());
        harness.key(&mut combo, vk::F4);
        assert!(!combo.is_open());
        harness.key(&mut combo, vk::RETURN);
        harness.key_with(&mut combo, vk::UP, Modifiers::ALT);
        assert!(!combo.is_open());
        harness.key_with(&mut combo, vk::DOWN, Modifiers::ALT);
        harness.key_with(&mut combo, vk::DOWN, Modifiers::ALT);
        assert!(!combo.is_open());
        // Tab commits the highlight and lets focus move on.
        harness.key(&mut combo, vk::F4);
        harness.key(&mut combo, vk::DOWN);
        assert!(!harness.key(&mut combo, vk::TAB).handled);
        assert_eq!(combo.selected(), Some(2));
        assert!(!combo.is_open());
        assert_eq!(combo.take_changed(), Some(2));
        // Losing focus closes the popup without committing.
        harness.key(&mut combo, vk::F4);
        harness.key(&mut combo, vk::DOWN);
        harness.send(&mut combo, Event::Focus(FocusEvent::Lost));
        assert!(!combo.is_open());
        assert_eq!(combo.selected(), Some(2));
        // Enter on an open popup with nothing highlighted closes it.
        let mut empty = ComboBox::new("e", fruit(), None).with_motion(Motion::reduced());
        harness.arrange(&mut empty, bounds);
        harness.focus_widget(&mut empty);
        harness.key(&mut empty, vk::RETURN);
        assert!(empty.is_open());
        assert_eq!(empty.highlighted(), None);
        harness.key(&mut empty, vk::RETURN);
        assert!(!empty.is_open());
        // Tab with nothing highlighted just closes.
        harness.key(&mut empty, vk::F4);
        assert!(!harness.key(&mut empty, vk::TAB).handled);
        assert!(!empty.is_open());
    }

    #[test]
    fn kit_combo_empty_list_never_opens_or_selects() {
        let mut harness = Harness::new();
        let mut combo = ComboBox::new("e", Vec::new(), None).with_motion(Motion::reduced());
        let bounds = harness.place(&mut combo, Point::ZERO);
        harness.focus_widget(&mut combo);
        for key in [
            vk::DOWN,
            vk::UP,
            vk::HOME,
            vk::END,
            vk::PAGE_UP,
            vk::PAGE_DOWN,
            vk::RETURN,
            vk::F4,
        ] {
            harness.key(&mut combo, key);
        }
        assert!(!combo.is_open());
        assert_eq!(combo.selected(), None);
        harness.click(&mut combo, bounds.center());
        assert!(!combo.is_open());
        assert_eq!(harness.node(&combo).value.as_deref(), Some(""));
    }

    #[test]
    fn kit_combo_pointer_open_pick_and_dismiss() {
        let (mut harness, mut combo, bounds) = setup(Some(0));
        let result = harness.click(&mut combo, bounds.center());
        assert!(result.handled && result.request_focus);
        assert!(combo.is_open());
        let popup = combo.popup_bounds().expect("popup");
        // Hovering a row highlights nothing until clicked; clicking row 3 commits it.
        let row3 = Point::new(popup.x + 30.0, popup.y + 4.0 + 3.0 * 32.0 + 16.0);
        assert!(harness.hover(&mut combo, row3).handled);
        let down = harness.pointer(
            &mut combo,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            row3,
        );
        assert!(down.handled && down.request_focus);
        assert!(!combo.is_open());
        assert_eq!(combo.selected(), Some(3));
        assert_eq!(combo.take_changed(), Some(3));
        // A press elsewhere dismisses without changing anything; a press on the box closes it.
        harness.click(&mut combo, bounds.center());
        assert!(combo.is_open());
        let away = harness.pointer(
            &mut combo,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            Point::new(900.0, 900.0),
        );
        assert!(away.handled && !away.request_focus);
        assert!(!combo.is_open());
        harness.click(&mut combo, bounds.center());
        assert!(
            harness
                .pointer(
                    &mut combo,
                    PointerEventKind::Down,
                    Some(PointerButton::Left),
                    bounds.center()
                )
                .request_focus
        );
        assert!(!combo.is_open());
        assert_eq!(combo.take_changed(), None);
        // Other buttons while open are not presses; moves outside are not handled.
        harness.click(&mut combo, bounds.center());
        assert!(combo.is_open());
        let right = harness.pointer(
            &mut combo,
            PointerEventKind::Down,
            Some(PointerButton::Right),
            Point::new(900.0, 900.0),
        );
        assert!(!right.handled);
        assert!(
            !harness
                .pointer(
                    &mut combo,
                    PointerEventKind::Move,
                    None,
                    Point::new(900.0, 900.0)
                )
                .handled
        );
        assert!(
            harness
                .pointer(&mut combo, PointerEventKind::Move, None, bounds.center())
                .handled
        );
        harness.pointer(
            &mut combo,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::new(900.0, 900.0),
        );
        assert!(combo.is_open());
    }

    #[test]
    fn kit_combo_long_lists_scroll_and_the_scrollbar_does_not_pick() {
        let mut harness = Harness::new();
        let items: Vec<String> = (0..20).map(|i| format!("Item {i}")).collect();
        let mut combo = ComboBox::new("n", items, Some(0)).with_motion(Motion::reduced());
        let bounds = harness.place(&mut combo, Point::ZERO);
        harness.click(&mut combo, bounds.center());
        let popup = combo.popup_bounds().expect("popup");
        assert_eq!(popup.height, 8.0 * 32.0 + 8.0);
        let wheel = Event::Wheel(crate::input::WheelEvent {
            delta: Point::new(0.0, -2.0),
            precise: true,
            position: Point::new(popup.x + 20.0, popup.y + 40.0),
            mods: Modifiers::NONE,
        });
        assert!(harness.send(&mut combo, wheel).handled);
        let far = Event::Wheel(crate::input::WheelEvent {
            delta: Point::new(0.0, -2.0),
            precise: true,
            position: Point::new(900.0, 900.0),
            mods: Modifiers::NONE,
        });
        assert!(!harness.send(&mut combo, far).handled);
        // A press on the scroll bar pages the list instead of picking a row.
        let on_bar = Point::new(popup.right() - 4.0, popup.bottom() - 10.0);
        harness.pointer(
            &mut combo,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            on_bar,
        );
        assert!(combo.is_open());
        assert_eq!(combo.take_changed(), None);
        // Opening on an item further down scrolls it into view.
        harness.focus_widget(&mut combo);
        harness.key(&mut combo, vk::ESCAPE);
        assert!(!combo.is_open());
        combo.select(Some(15));
        harness.key(&mut combo, vk::F4);
        assert_eq!(combo.highlighted(), Some(15));
        // Wheel events with the popup closed are not handled.
        harness.key(&mut combo, vk::ESCAPE);
        let closed = Event::Wheel(crate::input::WheelEvent {
            delta: Point::new(0.0, -2.0),
            precise: true,
            position: Point::new(popup.x + 20.0, popup.y + 40.0),
            mods: Modifiers::NONE,
        });
        assert!(!harness.send(&mut combo, closed).handled);
    }

    #[test]
    fn kit_combo_type_to_select_works_open_and_closed() {
        let (mut harness, mut combo, _) = setup(None);
        assert!(!harness.text(&mut combo, "b").handled, "needs focus");
        harness.focus_widget(&mut combo);
        assert!(harness.text(&mut combo, "b").handled);
        assert_eq!(combo.selected(), Some(2));
        assert!(harness.text(&mut combo, "l").handled);
        assert_eq!(combo.selected(), Some(3));
        let timer = combo.type_ahead.timer;
        harness.send(&mut combo, Event::Timer(timer));
        assert!(!harness.text(&mut combo, " ").handled);
        assert!(
            !harness
                .send(
                    &mut combo,
                    Event::Timer(crate::services::TimerId::allocate())
                )
                .handled
        );
        harness.key(&mut combo, vk::F4);
        assert!(harness.text(&mut combo, "c").handled);
        assert_eq!(combo.highlighted(), Some(4));
        assert_eq!(
            combo.selected(),
            Some(3),
            "open type-ahead only moves the highlight"
        );
        // No match changes nothing.
        harness.send(&mut combo, Event::Timer(timer));
        harness.text(&mut combo, "z");
        assert_eq!(combo.highlighted(), Some(4));
    }

    #[test]
    fn kit_combo_paints_box_text_chevron_and_popup() {
        let (mut harness, mut combo, bounds) = setup(Some(2));
        let tokens = harness.theme.tokens;
        let canvas = harness.paint(&combo);
        assert_eq!(canvas.texts(), vec!["Banana"]);
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::FillRoundedRect { rect, color, .. } if *rect == bounds && *color == tokens.surface)));
        assert!(canvas.ops().any(
            |op| matches!(op, DrawOp::Icon { glyph: g, center, size, .. }
            if *g == glyph::CHEVRON_DOWN && *size == 12.0 && center.x == bounds.right() - 17.0)
        ));
        assert!(
            !canvas
                .ops()
                .any(|op| matches!(op, DrawOp::StrokeRoundedRect { width, .. } if *width == 2.0))
        );
        harness.focus_widget(&mut combo);
        harness.key(&mut combo, vk::F4);
        let canvas = harness.paint(&combo);
        let popup = combo.popup_bounds().expect("popup");
        // Box, overlay and rows; the highlighted row carries the selection fill.
        assert_eq!(
            canvas.texts(),
            vec![
                "Banana",
                "Apple",
                "Avocado",
                "Banana",
                "Blueberry",
                "Cherry"
            ]
        );
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::FillRoundedRect { rect, color, .. } if *rect == popup && *color == tokens.menu_bg)));
        assert!(canvas.ops().any(
            |op| matches!(op, DrawOp::FillRoundedRect { color, .. } if *color == tokens.pressed_bg)
        ));
        // Damage that misses the box still paints the popup; a disabled box is grey.
        let mut partial = crate::canvas::RecordingCanvas::new();
        let cx = PaintCx::new(
            combo.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &harness.focus,
            &harness.capture,
            Some(popup),
        );
        combo.paint(&cx, &mut partial);
        assert_eq!(partial.texts().len(), 5);
        harness.key(&mut combo, vk::ESCAPE);
        combo.set_enabled(false);
        let canvas = harness.paint(&combo);
        assert!(
            canvas.ops().any(
                |op| matches!(op, DrawOp::Text { color, .. } if *color == tokens.text_disabled)
            )
        );
        let (mut harness, empty, _) = setup(None);
        assert!(harness.paint(&empty).texts().is_empty());
    }

    #[test]
    fn kit_combo_access_node_and_actions() {
        let (mut harness, mut combo, bounds) = setup(Some(2));
        let node = harness.node(&combo);
        assert_eq!((node.role, node.name.as_str()), (Role::ComboBox, "Fruit"));
        assert_eq!(node.value.as_deref(), Some("Banana"));
        assert_eq!(node.bounds, bounds);
        assert_eq!(node.state.expanded, Some(false));
        assert!(node.state.read_only && node.patterns.expand_collapse && node.patterns.value);
        assert!(harness.perform(&mut combo, AccessAction::Expand));
        let tree = harness.tree(&combo);
        let root = tree.root().expect("root");
        assert_eq!(root.state.expanded, Some(true));
        assert_eq!(root.children.len(), 1);
        let list = tree.first_child(combo.id()).expect("popup list");
        assert_eq!(list.role, Role::List);
        assert_eq!(tree.children(list.id).count(), 5);
        assert!(
            tree.children(list.id)
                .any(|row| row.name == "Banana" && row.state.selected)
        );
        assert!(harness.perform(&mut combo, AccessAction::Collapse));
        assert!(!combo.is_open());
        assert!(harness.perform(&mut combo, AccessAction::Focus));
        assert!(!harness.perform(&mut combo, AccessAction::Invoke));
        // An empty combo box cannot expand.
        let mut empty = ComboBox::new("e", Vec::new(), None);
        harness.arrange(&mut empty, bounds);
        assert!(!harness.perform(&mut empty, AccessAction::Expand));
        combo.set_enabled(false);
        harness.arrange(&mut combo, bounds);
        assert!(!harness.perform(&mut combo, AccessAction::Expand));
        assert!(!harness.click(&mut combo, bounds.center()).handled);
        assert!(!harness.key(&mut combo, vk::DOWN).handled);
        assert!(harness.node(&combo).state.disabled);
        assert!(!combo.is_enabled());
        assert_eq!(combo.bounds(), bounds);
    }

    #[test]
    fn kit_combo_relayout_moves_an_open_popup() {
        let (mut harness, mut combo, bounds) = setup(Some(0));
        harness.click(&mut combo, bounds.center());
        let moved = Rect::new(50.0, 200.0, 140.0, 32.0);
        harness.arrange(&mut combo, moved);
        assert_eq!(
            combo
                .popup_bounds()
                .map(|rect| (rect.x, rect.y, rect.width)),
            Some((50.0, 234.0, 140.0))
        );
        let canvas = harness.paint(&combo);
        assert!(canvas.texts().contains(&"Cherry"));
    }
}
