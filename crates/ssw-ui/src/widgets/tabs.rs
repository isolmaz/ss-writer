//! The document tab strip: close on hover, middle-click close, dirty dot, drag to reorder with a
//! tear-off signal, horizontal scroll overflow and a new-tab button.

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, Dash, IconFont};
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::{
    Event, FocusEvent, PointerButton, PointerEvent, PointerEventKind, WheelEvent, vk,
};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::anim::Motion;
use super::button::Button;
use super::scroll::{ScrollModel, WHEEL_STEP};
use super::{Control, FONT_SIZE, focus_ring, glyph, layout_line, perform_focus};

/// Height of the strip.
pub const TAB_HEIGHT: f32 = 40.0;
/// Widest tab.
pub const TAB_MAX_WIDTH: f32 = 200.0;
/// Narrowest tab before the strip scrolls.
pub const TAB_MIN_WIDTH: f32 = 96.0;
/// Side of the new-tab button.
pub const NEW_TAB_SIZE: f32 = 32.0;
/// Side of the close hit zone.
const CLOSE_SIZE: f32 = 20.0;
/// Em size of the close glyph.
const CLOSE_GLYPH: f32 = 12.0;
/// Vertical drag distance that tears a tab off the strip.
pub const TEAR_OFF_DISTANCE: f32 = 32.0;
/// Text inset inside a tab.
const TEXT_INSET: f32 = 12.0;

/// One document tab. `key` identifies it to the owner across reordering.
#[derive(Debug, Clone)]
struct Tab {
    id: WidgetId,
    key: u64,
    title: String,
    dirty: bool,
}

/// What the user did to the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabEvent {
    /// This tab (by key) became the active one.
    Activated(u64),
    /// Close button, middle click or Delete on this tab.
    CloseRequested(u64),
    /// The new-tab button.
    NewTab,
    /// A drag moved the tab with this key to a new position.
    Reordered { key: u64, from: usize, to: usize },
    /// A drag moved the tab with this key far enough off the strip to become its own window.
    TornOff(u64),
    /// Right click on this tab.
    ContextMenu(u64),
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    /// Index the dragged tab currently has.
    index: usize,
    /// Distance from the tab's left edge to the pointer.
    grab: f32,
    start: Point,
    torn: bool,
}

/// A strip of tabs with a new-tab button.
#[derive(Debug, Clone)]
pub struct TabStrip {
    id: WidgetId,
    label: String,
    dirty_description: String,
    tabs: Vec<Tab>,
    active: usize,
    hover: Option<usize>,
    hover_close: bool,
    scroll: ScrollModel,
    new_button: Button,
    drag: Option<Drag>,
    enabled: bool,
    bounds: Rect,
    events: Vec<TabEvent>,
}

impl TabStrip {
    /// An empty strip. `label` names the strip, `new_tab_label` the new-tab button and
    /// `dirty_description` is announced for tabs with unsaved changes.
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        new_tab_label: impl Into<String>,
        dirty_description: impl Into<String>,
    ) -> Self {
        Self {
            id: WidgetId::allocate(),
            label: label.into(),
            dirty_description: dirty_description.into(),
            tabs: Vec::new(),
            active: 0,
            hover: None,
            hover_close: false,
            scroll: ScrollModel::default(),
            new_button: Button::icon(glyph::ADD, new_tab_label),
            drag: None,
            enabled: true,
            bounds: Rect::ZERO,
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
        self.scroll.set_motion(motion);
        self.new_button = self.new_button.with_motion(motion);
        self
    }

    /// Appends a tab and returns its index.
    pub fn add_tab(&mut self, key: u64, title: impl Into<String>) -> usize {
        self.tabs.push(Tab {
            id: WidgetId::allocate(),
            key,
            title: title.into(),
            dirty: false,
        });
        self.update_extent();
        self.tabs.len() - 1
    }

    /// Removes tab `index`; the active tab stays the same tab unless it was the removed one.
    pub fn remove_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        if index < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.hover = None;
        self.update_extent();
    }

    pub fn set_title(&mut self, index: usize, title: impl Into<String>) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.title = title.into();
        }
    }

    pub fn set_dirty(&mut self, index: usize, dirty: bool) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.dirty = dirty;
        }
    }

    /// Makes `index` the active tab without notifying the owner.
    pub fn set_active(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
            self.reveal_active();
        }
    }

    #[must_use]
    pub fn active(&self) -> usize {
        self.active
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// The key of tab `index`.
    #[must_use]
    pub fn key_at(&self, index: usize) -> Option<u64> {
        self.tabs.get(index).map(|tab| tab.key)
    }

    #[must_use]
    pub fn title_at(&self, index: usize) -> Option<&str> {
        self.tabs.get(index).map(|tab| tab.title.as_str())
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
        self.scroll.offset()
    }

    /// The tab being dragged.
    #[must_use]
    pub fn dragging(&self) -> Option<usize> {
        self.drag.map(|drag| drag.index)
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<TabEvent> {
        std::mem::take(&mut self.events)
    }

    /// Width available to tabs (the new-tab button takes the rest while the strip scrolls).
    fn viewport(&self) -> f32 {
        (self.bounds.width - NEW_TAB_SIZE).max(0.0)
    }

    /// Width of one tab.
    #[must_use]
    pub fn tab_width(&self) -> f32 {
        if self.tabs.is_empty() {
            return TAB_MAX_WIDTH;
        }
        (self.viewport() / self.tabs.len() as f32).clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH)
    }

    fn update_extent(&mut self) {
        let content = self.tabs.len() as f32 * self.tab_width();
        let viewport = self.viewport();
        self.scroll.set_extent(viewport, content);
    }

    fn reveal_active(&mut self) {
        let width = self.tab_width();
        let left = self.active as f32 * width;
        self.scroll.ensure_visible(left, left + width);
    }

    /// Bounds of tab `index` in window coordinates.
    #[must_use]
    pub fn tab_rect(&self, index: usize) -> Rect {
        let width = self.tab_width();
        Rect::new(
            self.bounds.x + index as f32 * width - self.scroll.offset(),
            self.bounds.y,
            width,
            self.bounds.height,
        )
    }

    /// Where the close button of tab `index` is.
    #[must_use]
    pub fn close_rect(&self, index: usize) -> Rect {
        let tab = self.tab_rect(index);
        Rect::new(
            tab.right() - 8.0 - CLOSE_SIZE,
            tab.y + (tab.height - CLOSE_SIZE) / 2.0,
            CLOSE_SIZE,
            CLOSE_SIZE,
        )
    }

    fn tabs_area(&self) -> Rect {
        Rect::new(
            self.bounds.x,
            self.bounds.y,
            self.viewport(),
            self.bounds.height,
        )
    }

    /// Where the new-tab button sits: after the last tab, or pinned right while scrolling.
    #[must_use]
    pub fn new_tab_rect(&self) -> Rect {
        let content = self.tabs.len() as f32 * self.tab_width();
        let x = self.bounds.x + content.min(self.viewport());
        Rect::new(
            x,
            self.bounds.y + (self.bounds.height - NEW_TAB_SIZE) / 2.0,
            NEW_TAB_SIZE,
            NEW_TAB_SIZE,
        )
    }

    fn tab_at(&self, point: Point) -> Option<usize> {
        if !self.tabs_area().contains(point) {
            return None;
        }
        let x = point.x - self.bounds.x + self.scroll.offset();
        let index = (x / self.tab_width()) as usize;
        (index < self.tabs.len()).then_some(index)
    }

    fn activate(&mut self, index: usize) {
        if index < self.tabs.len() && index != self.active {
            self.active = index;
            self.events.push(TabEvent::Activated(self.tabs[index].key));
        }
        self.reveal_active();
    }

    fn close_visible(&self, index: usize) -> bool {
        index == self.active || self.hover == Some(index)
    }

    fn pointer(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        if !self.enabled {
            return EventResult::ignored();
        }
        let new_id = self.new_button.id();
        let forwarded = self
            .new_button
            .event(&mut cx.for_widget(new_id), &Event::Pointer(*event));
        if self.new_button.take_activated() {
            self.events.push(TabEvent::NewTab);
        }
        if forwarded.handled {
            return forwarded;
        }
        let tab = self.tab_at(event.position);
        match (event.kind, event.button) {
            (PointerEventKind::Down, Some(PointerButton::Left)) => {
                let Some(index) = tab else {
                    return EventResult::ignored();
                };
                if self.close_visible(index) && self.close_rect(index).contains(event.position) {
                    self.events
                        .push(TabEvent::CloseRequested(self.tabs[index].key));
                } else {
                    self.activate(index);
                    self.drag = Some(Drag {
                        index,
                        grab: event.position.x - self.tab_rect(index).x,
                        start: event.position,
                        torn: false,
                    });
                    cx.capture_pointer();
                }
                cx.invalidate(self.bounds);
                EventResult::handled_with_focus()
            }
            (PointerEventKind::Down, Some(PointerButton::Middle)) => match tab {
                Some(index) => {
                    self.events
                        .push(TabEvent::CloseRequested(self.tabs[index].key));
                    EventResult::handled()
                }
                None => EventResult::ignored(),
            },
            (PointerEventKind::Down, Some(PointerButton::Right)) => match tab {
                Some(index) => {
                    self.activate(index);
                    self.events
                        .push(TabEvent::ContextMenu(self.tabs[index].key));
                    cx.invalidate(self.bounds);
                    EventResult::handled_with_focus()
                }
                None => EventResult::ignored(),
            },
            (PointerEventKind::Move, _) if self.drag.is_some() => {
                self.drag_to(cx, event.position);
                EventResult::handled()
            }
            (PointerEventKind::Up, Some(PointerButton::Left)) if self.drag.is_some() => {
                self.drag = None;
                cx.release_pointer();
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            (PointerEventKind::Move | PointerEventKind::Enter, _) => {
                // Hovering a tab reveals its close button, so any tab's button can be hovered.
                let close =
                    tab.is_some_and(|index| self.close_rect(index).contains(event.position));
                self.set_hover(cx, tab, close)
            }
            (PointerEventKind::Leave, _) if self.drag.is_none() => self.set_hover(cx, None, false),
            _ => EventResult::ignored(),
        }
    }

    fn set_hover(&mut self, cx: &mut EventCx, hover: Option<usize>, close: bool) -> EventResult {
        if hover == self.hover && close == self.hover_close {
            return EventResult::ignored();
        }
        self.hover = hover;
        self.hover_close = close;
        cx.invalidate(self.bounds);
        EventResult::handled()
    }

    fn drag_to(&mut self, cx: &mut EventCx, position: Point) {
        let Some(mut drag) = self.drag else { return };
        if drag.index >= self.tabs.len() {
            // The dragged tab was closed meanwhile.
            self.drag = None;
            cx.release_pointer();
            return;
        }
        if !drag.torn && (position.y - drag.start.y).abs() > TEAR_OFF_DISTANCE {
            drag.torn = true;
            self.events
                .push(TabEvent::TornOff(self.tabs[drag.index].key));
        }
        if !drag.torn {
            let width = self.tab_width();
            let left = position.x - drag.grab - self.bounds.x + self.scroll.offset();
            let target = ((left / width + 0.5).floor().max(0.0) as usize).min(self.tabs.len() - 1);
            if target != drag.index {
                let tab = self.tabs.remove(drag.index);
                let key = tab.key;
                self.tabs.insert(target, tab);
                if self.active == drag.index {
                    self.active = target;
                } else if drag.index < self.active && target >= self.active {
                    self.active -= 1;
                } else if drag.index > self.active && target <= self.active {
                    self.active += 1;
                }
                self.events.push(TabEvent::Reordered {
                    key,
                    from: drag.index,
                    to: target,
                });
                drag.index = target;
            }
        }
        self.drag = Some(drag);
        cx.invalidate(self.bounds);
    }

    fn wheel(&mut self, cx: &mut EventCx, event: &WheelEvent) -> EventResult {
        if !self.bounds.contains(event.position) || !self.scroll.is_scrollable() {
            return EventResult::ignored();
        }
        let unit = if event.precise { 1.0 } else { WHEEL_STEP };
        // A vertical wheel scrolls the strip sideways.
        let delta = (event.delta.x - event.delta.y) * unit;
        let before = self.scroll.target();
        if event.precise {
            self.scroll.scroll_by(delta);
        } else if self.scroll.animate_by(delta) {
            cx.request_animation_frame();
        }
        if self.scroll.target() == before {
            return EventResult::ignored();
        }
        cx.invalidate(self.bounds);
        EventResult::handled()
    }

    fn key(&mut self, key: u16) -> bool {
        if self.tabs.is_empty() {
            return false;
        }
        let last = self.tabs.len() - 1;
        match key {
            vk::LEFT => self.activate(self.active.saturating_sub(1)),
            vk::RIGHT => self.activate((self.active + 1).min(last)),
            vk::HOME => self.activate(0),
            vk::END => self.activate(last),
            vk::DELETE => self
                .events
                .push(TabEvent::CloseRequested(self.tabs[self.active].key)),
            _ => return false,
        }
        true
    }

    fn tab_node(&self, cx: &AccessCx, index: usize) -> AccessNode {
        let tab = &self.tabs[index];
        let mut node = AccessNode::new(
            tab.id,
            Role::TabItem,
            tab.title.clone(),
            self.tab_rect(index),
        );
        if tab.dirty {
            node.description = self.dirty_description.clone();
        }
        node.state.selected = index == self.active;
        node.state.disabled = !self.enabled;
        node.state.focusable = false;
        node.state.focused = cx.is_focused() && index == self.active;
        node.state.offscreen = !self.tabs_area().intersects(node.bounds);
        node.patterns.selection_item = true;
        node
    }

    /// Carries out `action` on the tab whose access node has id `item`.
    pub fn perform_item(
        &mut self,
        cx: &mut EventCx,
        item: WidgetId,
        action: &AccessAction,
    ) -> bool {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == item) else {
            return false;
        };
        if !self.enabled {
            return false;
        }
        let done = match action {
            AccessAction::Select => {
                self.activate(index);
                cx.focus_self();
                true
            }
            AccessAction::ScrollIntoView => {
                let width = self.tab_width();
                self.scroll
                    .ensure_visible(index as f32 * width, (index as f32 + 1.0) * width);
                true
            }
            _ => false,
        };
        if done {
            cx.invalidate(self.bounds);
        }
        done
    }
}

impl Control for TabStrip {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for TabStrip {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        let width = if available.width.is_finite() {
            available.width
        } else {
            self.tabs.len() as f32 * TAB_MAX_WIDTH + NEW_TAB_SIZE
        };
        Size::new(width, TAB_HEIGHT)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        self.update_extent();
        if self.enabled && !self.tabs.is_empty() {
            cx.add_tab_stop();
        }
        let id = self.new_button.id();
        let rect = self.new_tab_rect();
        self.new_button.arrange(&mut cx.for_widget(id), rect);
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        canvas.push_clip_rect(self.tabs_area());
        for (index, tab) in self.tabs.iter().enumerate() {
            let rect = self.tab_rect(index);
            if !rect.intersects(self.tabs_area()) {
                continue;
            }
            let active = index == self.active;
            let hovered = self.hover == Some(index);
            let body = Rect::new(
                rect.x + 2.0,
                rect.y + 4.0,
                rect.width - 4.0,
                rect.height - 4.0,
            );
            if active {
                canvas.fill_rounded_rect(
                    body,
                    CornerRadii::new(
                        tokens.metrics.overlay_radius,
                        tokens.metrics.overlay_radius,
                        0.0,
                        0.0,
                    ),
                    tokens.surface,
                );
            } else if hovered {
                canvas.fill_rounded_rect(
                    body.inset(Insets::symmetric(0.0, 2.0)),
                    CornerRadii::uniform(tokens.metrics.control_radius),
                    tokens.hover_bg,
                );
            } else if index > 0 && index != self.active + 1 {
                canvas.stroke_line(
                    Point::new(rect.x, rect.y + 12.0),
                    Point::new(rect.x, rect.bottom() - 8.0),
                    tokens.hairline,
                    1.0,
                    Dash::Solid,
                );
            }
            let color = if self.enabled {
                tokens.text
            } else {
                tokens.text_disabled
            };
            let title = layout_line(cx.text, cx.theme, &tab.title, FONT_SIZE);
            canvas.push_clip_rect(Rect::new(
                rect.x + TEXT_INSET,
                rect.y,
                (rect.width - TEXT_INSET - 8.0 - CLOSE_SIZE).max(0.0),
                rect.height,
            ));
            super::draw_left_centered(canvas, title.as_ref(), rect.x + TEXT_INSET, rect, color);
            canvas.pop_clip();
            let close = self.close_rect(index);
            if self.close_visible(index) {
                if self.hover_close && hovered {
                    canvas.fill_rounded_rect(
                        close,
                        CornerRadii::uniform(tokens.metrics.control_radius),
                        tokens.hover_bg,
                    );
                }
                if !(tab.dirty && !hovered) {
                    canvas.draw_icon(
                        glyph::CLOSE,
                        IconFont::SegoeFluent,
                        CLOSE_GLYPH,
                        color,
                        close.center(),
                    );
                }
            }
            if tab.dirty && !hovered {
                canvas.fill_rounded_rect(
                    Rect::new(close.center().x - 4.0, close.center().y - 4.0, 8.0, 8.0),
                    CornerRadii::uniform(4.0),
                    tokens.accent,
                );
            }
            if active && cx.is_focused() {
                focus_ring::paint(canvas, body, tokens.metrics.control_radius, cx.theme);
            }
        }
        canvas.pop_clip();
        self.new_button
            .paint(&cx.for_widget(self.new_button.id()), canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => self.pointer(cx, event),
            Event::Wheel(event) if self.enabled => self.wheel(cx, event),
            Event::Key(key)
                if key.is_down && self.enabled && cx.is_focused() && key.mods.is_empty() =>
            {
                if self.key(key.vk) {
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) => {
                cx.invalidate(self.bounds);
                let id = self.new_button.id();
                self.new_button.event(&mut cx.for_widget(id), e);
                EventResult::ignored()
            }
            Event::AnimationFrame(now) => {
                let (moved, more) = self.scroll.tick(*now);
                if more {
                    cx.request_animation_frame();
                }
                if moved {
                    cx.invalidate(self.bounds);
                }
                let id = self.new_button.id();
                self.new_button.event(&mut cx.for_widget(id), e);
                EventResult::ignored()
            }
            _ => EventResult::ignored(),
        }
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let mut children = Vec::with_capacity(self.tabs.len() + 1);
        for index in 0..self.tabs.len() {
            out.insert(self.tab_node(cx, index));
            children.push(self.tabs[index].id);
        }
        self.new_button
            .access(&cx.for_widget(self.new_button.id()), out);
        children.push(self.new_button.id());
        let mut state = super::control_state(cx, self.enabled);
        state.focused = cx.is_focused() && self.tabs.is_empty();
        let node = AccessNode::new(self.id, Role::Tab, self.label.clone(), self.bounds)
            .with_state(state)
            .with_children(children);
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::Focus if self.enabled => perform_focus(cx),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::widgets::testkit::Harness;

    fn strip(count: u64, width: f32) -> (Harness, TabStrip) {
        let mut harness = Harness::new();
        let mut strip =
            TabStrip::new("Documents", "New tab", "Unsaved changes").with_motion(Motion::reduced());
        for key in 0..count {
            strip.add_tab(key + 10, format!("doc{key}.md"));
        }
        harness.arrange(&mut strip, Rect::new(0.0, 0.0, width, TAB_HEIGHT));
        (harness, strip)
    }

    fn press(
        harness: &mut Harness,
        strip: &mut TabStrip,
        button: PointerButton,
        at: Point,
    ) -> EventResult {
        harness.pointer(strip, PointerEventKind::Down, Some(button), at)
    }

    #[test]
    fn kit_tabs_layout_widths_and_new_tab_button() {
        let (harness, strip) = strip(3, 700.0);
        assert_eq!(strip.tab_width(), 200.0);
        assert_eq!(strip.tab_rect(1), Rect::new(200.0, 0.0, 200.0, 40.0));
        // The new-tab button follows the last tab while everything fits.
        assert_eq!(strip.new_tab_rect(), Rect::new(600.0, 4.0, 32.0, 32.0));
        assert_eq!(
            harness.focus.tab_order().len(),
            2,
            "the strip and its new-tab button"
        );
        // More tabs than fit shrink down to 96 DIPs, then scroll.
        let (_, crowded) = self::strip(10, 700.0);
        assert_eq!(crowded.tab_width(), 96.0);
        assert_eq!(crowded.new_tab_rect().x, 700.0 - 32.0);
        assert!(crowded.scroll.is_scrollable());
        let (_, few) = self::strip(5, 700.0);
        assert_eq!(few.tab_width(), 668.0 / 5.0);
        let (_, empty) = self::strip(0, 700.0);
        assert_eq!(empty.tab_width(), 200.0);
        assert!(empty.is_empty() && !few.is_empty());
        assert_eq!(few.len(), 5);
    }

    #[test]
    fn kit_tabs_click_activates_and_close_button_requests_close() {
        let (mut harness, mut strip) = strip(3, 700.0);
        let second = strip.tab_rect(1).center();
        let result = press(&mut harness, &mut strip, PointerButton::Left, second);
        assert!(result.handled && result.request_focus);
        assert_eq!(strip.active(), 1);
        assert_eq!(strip.take_events(), vec![TabEvent::Activated(11)]);
        harness.pointer(
            &mut strip,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            second,
        );
        // The active tab shows its close button; inactive ones only while hovered.
        let close = strip.close_rect(1).center();
        press(&mut harness, &mut strip, PointerButton::Left, close);
        assert_eq!(strip.take_events(), vec![TabEvent::CloseRequested(11)]);
        assert_eq!(strip.dragging(), None);
        let other = strip.close_rect(2).center();
        press(&mut harness, &mut strip, PointerButton::Left, other);
        assert_eq!(
            strip.take_events(),
            vec![TabEvent::Activated(12)],
            "hidden close button is a plain click"
        );
        harness.pointer(
            &mut strip,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            other,
        );
        // Hovering an inactive tab reveals its close button.
        let first_close = strip.close_rect(0).center();
        assert!(harness.hover(&mut strip, first_close).handled);
        press(&mut harness, &mut strip, PointerButton::Left, first_close);
        assert_eq!(strip.take_events(), vec![TabEvent::CloseRequested(10)]);
    }

    #[test]
    fn kit_tabs_middle_click_right_click_and_new_tab() {
        let (mut harness, mut strip) = strip(3, 700.0);
        let third = strip.tab_rect(2).center();
        assert!(press(&mut harness, &mut strip, PointerButton::Middle, third).handled);
        assert_eq!(strip.take_events(), vec![TabEvent::CloseRequested(12)]);
        assert_eq!(strip.active(), 0, "middle click does not activate");
        let result = press(&mut harness, &mut strip, PointerButton::Right, third);
        assert!(result.handled && result.request_focus);
        assert_eq!(
            strip.take_events(),
            vec![TabEvent::Activated(12), TabEvent::ContextMenu(12)]
        );
        // Empty strip space ignores presses.
        let empty_space = Point::new(690.0, 20.0);
        assert!(!press(&mut harness, &mut strip, PointerButton::Left, empty_space).handled);
        assert!(!press(&mut harness, &mut strip, PointerButton::Middle, empty_space).handled);
        assert!(!press(&mut harness, &mut strip, PointerButton::Right, empty_space).handled);
        // The new-tab button.
        let plus = strip.new_tab_rect().center();
        assert!(harness.click(&mut strip, plus).handled);
        assert_eq!(strip.take_events(), vec![TabEvent::NewTab]);
    }

    #[test]
    fn kit_tabs_drag_reorders_and_tears_off() {
        let (mut harness, mut strip) = strip(3, 700.0);
        let first = strip.tab_rect(0).center();
        press(&mut harness, &mut strip, PointerButton::Left, first);
        assert_eq!(strip.dragging(), Some(0));
        strip.take_events();
        // Dragging right past the neighbour's middle swaps them; the active tab follows.
        harness.pointer(
            &mut strip,
            PointerEventKind::Move,
            None,
            Point::new(first.x + 120.0, first.y),
        );
        assert_eq!(
            strip.take_events(),
            vec![TabEvent::Reordered {
                key: 10,
                from: 0,
                to: 1
            }]
        );
        assert_eq!(
            (strip.key_at(0), strip.key_at(1), strip.active()),
            (Some(11), Some(10), 1)
        );
        assert_eq!(strip.dragging(), Some(1));
        harness.pointer(
            &mut strip,
            PointerEventKind::Move,
            None,
            Point::new(first.x + 1000.0, first.y),
        );
        assert_eq!(strip.key_at(2), Some(10));
        assert_eq!(strip.active(), 2);
        // Moving too far off the strip tears the tab off exactly once.
        harness.pointer(
            &mut strip,
            PointerEventKind::Move,
            None,
            Point::new(first.x + 1000.0, first.y + 80.0),
        );
        harness.pointer(
            &mut strip,
            PointerEventKind::Move,
            None,
            Point::new(first.x + 50.0, first.y + 90.0),
        );
        let events = strip.take_events();
        assert_eq!(
            events
                .iter()
                .filter(|e| **e == TabEvent::TornOff(10))
                .count(),
            1
        );
        assert_eq!(strip.key_at(2), Some(10), "no reordering once torn off");
        harness.pointer(
            &mut strip,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            first,
        );
        assert_eq!(strip.dragging(), None);
        assert!(!harness.capture.is_captured_by(strip.id()));
        // Dragging a tab left past an earlier active tab keeps the active tab selected.
        let (mut harness, mut strip) = self::strip(4, 800.0);
        strip.set_active(1);
        let third = strip.tab_rect(3).center();
        press(&mut harness, &mut strip, PointerButton::Left, third);
        harness.pointer(
            &mut strip,
            PointerEventKind::Move,
            None,
            Point::new(10.0, third.y),
        );
        assert_eq!(strip.key_at(0), Some(13));
        assert_eq!(
            strip.active(),
            0,
            "the dragged (and newly activated) tab is active"
        );
        // Dragging an inactive tab across the active one shifts the active index.
        let (mut harness, mut strip) = self::strip(3, 700.0);
        strip.set_active(1);
        strip.drag = Some(Drag {
            index: 0,
            grab: 100.0,
            start: Point::new(100.0, 20.0),
            torn: false,
        });
        let mut cx = harness.event_cx(strip.id());
        strip.drag_to(&mut cx, Point::new(450.0, 20.0));
        assert_eq!(strip.active(), 0);
        strip.drag = Some(Drag {
            index: 2,
            grab: 100.0,
            start: Point::new(100.0, 20.0),
            torn: false,
        });
        strip.drag_to(&mut cx, Point::new(150.0, 20.0));
        assert_eq!(strip.active(), 1);
    }

    #[test]
    fn kit_tabs_keyboard_navigation_and_delete() {
        let (mut harness, mut strip) = strip(3, 700.0);
        assert!(!harness.key(&mut strip, vk::RIGHT).handled, "needs focus");
        harness.focus_widget(&mut strip);
        let table = [
            (vk::RIGHT, 1),
            (vk::RIGHT, 2),
            (vk::RIGHT, 2),
            (vk::LEFT, 1),
            (vk::HOME, 0),
            (vk::LEFT, 0),
            (vk::END, 2),
        ];
        for (key, expected) in table {
            assert!(harness.key(&mut strip, key).handled);
            assert_eq!(strip.active(), expected, "key {key:#x}");
        }
        strip.take_events();
        harness.key(&mut strip, vk::DELETE);
        assert_eq!(strip.take_events(), vec![TabEvent::CloseRequested(12)]);
        assert!(!harness.key(&mut strip, vk::TAB).handled);
        assert!(
            !harness
                .key_with(&mut strip, vk::LEFT, crate::input::Modifiers::CTRL)
                .handled
        );
        let (mut harness, mut empty) = self::strip(0, 700.0);
        harness.arrange(&mut empty, Rect::new(0.0, 0.0, 700.0, 40.0));
        assert!(!harness.key(&mut empty, vk::LEFT).handled);
    }

    #[test]
    fn kit_tabs_wheel_scrolls_overflowing_strips_and_keeps_the_active_tab_visible() {
        let (mut harness, mut strip) = strip(10, 700.0);
        let over = Point::new(300.0, 20.0);
        let wheel = |dy: f32, precise: bool, position: Point| {
            Event::Wheel(WheelEvent {
                delta: Point::new(0.0, dy),
                precise,
                position,
                mods: crate::input::Modifiers::NONE,
            })
        };
        assert!(harness.send(&mut strip, wheel(-30.0, true, over)).handled);
        assert_eq!(strip.scroll_offset(), 30.0);
        assert!(
            !harness
                .send(&mut strip, wheel(-30.0, true, Point::new(900.0, 20.0)))
                .handled
        );
        strip.scroll.jump(0.0);
        assert!(
            !harness.send(&mut strip, wheel(1.0, true, over)).handled,
            "already at the start"
        );
        // Notched wheels ease.
        let mut smooth = TabStrip::new("d", "n", "u");
        for key in 0..10 {
            smooth.add_tab(key, "t");
        }
        harness.arrange(&mut smooth, Rect::new(0.0, 0.0, 700.0, 40.0));
        assert!(harness.send(&mut smooth, wheel(-1.0, false, over)).handled);
        assert!(harness.requests.animation_frame_requested());
        harness.run_animations(&mut smooth);
        assert_eq!(smooth.scroll_offset(), 48.0);
        // Activating the last tab scrolls it into view.
        strip.set_active(9);
        assert_eq!(strip.tab_rect(9).right(), 668.0);
        strip.set_active(0);
        assert_eq!(strip.tab_rect(0).x, 0.0);
        strip.set_active(99);
        assert_eq!(strip.active(), 0);
        // Fitting strips do not scroll.
        let (mut harness, mut fits) = self::strip(2, 700.0);
        assert!(!harness.send(&mut fits, wheel(-30.0, true, over)).handled);
    }

    #[test]
    fn kit_tabs_add_remove_and_edit() {
        let (_, mut strip) = strip(3, 700.0);
        strip.set_active(2);
        strip.remove_tab(0);
        assert_eq!(
            (strip.len(), strip.active(), strip.key_at(0)),
            (2, 1, Some(11))
        );
        strip.remove_tab(1);
        assert_eq!((strip.len(), strip.active()), (1, 0));
        strip.remove_tab(0);
        assert!(strip.is_empty());
        strip.remove_tab(0);
        assert_eq!(strip.add_tab(7, "a"), 0);
        strip.set_title(0, "b.md");
        assert_eq!(strip.title_at(0), Some("b.md"));
        strip.set_title(9, "x");
        strip.set_dirty(9, true);
        assert_eq!(strip.title_at(9), None);
        assert_eq!(strip.key_at(9), None);
        strip.set_active(0);
        assert!(strip.is_enabled());
        assert_eq!(strip.bounds(), Rect::new(0.0, 0.0, 700.0, 40.0));
    }

    #[test]
    fn kit_tabs_paint_active_hover_dirty_dot_and_close() {
        let (mut harness, mut strip) = strip(3, 700.0);
        let tokens = harness.theme.tokens;
        strip.set_dirty(1, true);
        strip.set_dirty(2, true);
        let canvas = harness.paint(&strip);
        assert_eq!(canvas.texts(), vec!["doc0.md", "doc1.md", "doc2.md"]);
        // The active tab has the surface fill with rounded top corners only.
        assert!(canvas.ops().any(
            |op| matches!(op, DrawOp::FillRoundedRect { rect, radii, color }
            if *rect == Rect::new(2.0, 4.0, 196.0, 36.0) && *color == tokens.surface
                && radii.top_left == 8.0 && radii.bottom_left == 0.0)
        ));
        // Dirty inactive tabs show the dot instead of the close glyph.
        let dots = canvas
            .ops()
            .filter(|op| {
                matches!(op, DrawOp::FillRoundedRect { rect, color, .. }
            if rect.width == 8.0 && *color == tokens.accent)
            })
            .count();
        assert_eq!(dots, 2);
        let closes = canvas
            .ops()
            .filter(|op| matches!(op, DrawOp::Icon { glyph: g, .. } if *g == glyph::CLOSE))
            .count();
        assert_eq!(closes, 1, "only the clean active tab shows a close glyph");
        // Hovering a dirty tab turns the dot into a close button with a hover fill.
        let close = strip.close_rect(1).center();
        harness.hover(&mut strip, close);
        let canvas = harness.paint(&strip);
        let closes = canvas
            .ops()
            .filter(|op| matches!(op, DrawOp::Icon { glyph: g, .. } if *g == glyph::CLOSE))
            .count();
        assert_eq!(closes, 2);
        assert!(canvas.ops().filter(|op| matches!(op, DrawOp::FillRoundedRect { color, .. } if *color == tokens.hover_bg)).count() >= 2);
        // Hover without being over the close glyph has the tab fill only.
        let middle = strip.tab_rect(1).center();
        harness.hover(&mut strip, middle);
        assert!(!strip.hover_close);
        // Separators between inactive tabs, focus ring on the active tab, damage skipping.
        harness.focus_widget(&mut strip);
        let canvas = harness.paint(&strip);
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::StrokeLine { .. }))
        );
        assert_eq!(
            canvas
                .ops()
                .filter(|op| matches!(op, DrawOp::StrokeRoundedRect { width, .. } if *width == 2.0))
                .count(),
            1
        );
        let mut skipped = crate::canvas::RecordingCanvas::new();
        let cx = PaintCx::new(
            strip.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &harness.focus,
            &harness.capture,
            Some(Rect::new(0.0, 500.0, 5.0, 5.0)),
        );
        strip.paint(&cx, &mut skipped);
        assert!(skipped.recorded().is_empty());
        harness.pointer(&mut strip, PointerEventKind::Leave, None, Point::ZERO);
        assert!(
            !harness
                .pointer(&mut strip, PointerEventKind::Leave, None, Point::ZERO)
                .handled
        );
    }

    #[test]
    fn kit_tabs_access_tree_and_item_actions() {
        let (mut harness, mut strip) = strip(3, 700.0);
        strip.set_dirty(1, true);
        harness.focus_widget(&mut strip);
        let tree = harness.tree(&strip);
        let root = tree.root().expect("root");
        assert_eq!((root.role, root.name.as_str()), (Role::Tab, "Documents"));
        assert_eq!(root.children.len(), 4);
        let items: Vec<&AccessNode> = tree.children(strip.id()).collect();
        assert_eq!(items[0].role, Role::TabItem);
        assert!(
            items[0].state.selected && items[0].state.focused && items[0].patterns.selection_item
        );
        assert_eq!(items[1].description, "Unsaved changes");
        assert_eq!(items[0].description, "");
        assert_eq!(items[3].role, Role::Button);
        assert_eq!(items[3].name, "New tab");
        let id1 = items[1].id;
        let id = strip.id();
        {
            let mut cx = harness.event_cx(id);
            assert!(strip.perform_item(&mut cx, id1, &AccessAction::Select));
            assert!(strip.perform_item(&mut cx, id1, &AccessAction::ScrollIntoView));
            assert!(!strip.perform_item(&mut cx, id1, &AccessAction::Toggle));
            assert!(!strip.perform_item(&mut cx, WidgetId::allocate(), &AccessAction::Select));
        }
        assert_eq!(strip.active(), 1);
        assert!(harness.perform(&mut strip, AccessAction::Focus));
        assert!(!harness.perform(&mut strip, AccessAction::Invoke));
        // Offscreen tabs are flagged; a disabled strip refuses actions.
        let (mut harness, mut crowded) = self::strip(10, 700.0);
        let tree = harness.tree(&crowded);
        assert!(
            tree.children(crowded.id())
                .filter(|n| n.role == Role::TabItem)
                .any(|n| n.state.offscreen)
        );
        crowded.set_enabled(false);
        harness.arrange(&mut crowded, Rect::new(0.0, 0.0, 700.0, 40.0));
        let tab_id = crowded.tabs[0].id;
        let crowded_id = crowded.id();
        let mut cx = harness.event_cx(crowded_id);
        assert!(!crowded.perform_item(&mut cx, tab_id, &AccessAction::Select));
        assert!(!harness.click(&mut crowded, Point::new(10.0, 10.0)).handled);
        assert!(!harness.perform(&mut crowded, AccessAction::Focus));
        assert!(harness.node(&crowded).state.disabled);
    }

    #[test]
    fn kit_tabs_measure_and_empty_strip_focus() {
        let mut harness = Harness::new();
        let mut strip = TabStrip::new("d", "n", "u");
        assert_eq!(
            harness.place(&mut strip, Point::ZERO).size(),
            Size::new(32.0, 40.0)
        );
        harness.focus_widget(&mut strip);
        assert!(
            !harness.focus.can_focus(strip.id()),
            "an empty strip is not a tab stop"
        );
        strip.add_tab(1, "a");
        assert_eq!(
            harness.place(&mut strip, Point::ZERO).size(),
            Size::new(232.0, 40.0)
        );
        harness.focus_widget(&mut strip);
        assert!(harness.node(&strip).children.len() == 2);
        harness.send(
            &mut strip,
            Event::AnimationFrame(std::time::Duration::from_millis(5)),
        );
    }
}
