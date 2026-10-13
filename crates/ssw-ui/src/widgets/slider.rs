//! The Fluent horizontal slider.

use crate::access::{AccessAction, AccessNode, AccessTree, RangeValue, Role};
use crate::canvas::Canvas;
use crate::geometry::{CornerRadii, Insets, Rect, Size};
use crate::input::{Cursor, Event, FocusEvent, PointerButton, PointerEvent, PointerEventKind, vk};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::anim::Motion;
use super::{CONTROL_HEIGHT, Control, Interaction, control_state, focus_ring, perform_focus};

/// Diameter of the thumb.
pub const THUMB: f32 = 20.0;
/// Thickness of the track.
pub const TRACK: f32 = 4.0;
/// Width a slider asks for when its parent does not constrain it.
const DEFAULT_WIDTH: f32 = 200.0;

/// A slider over `min..=max` in multiples of `step`. Arrow keys move by `step`, Page keys by
/// `large_step`, Home and End jump to the ends.
#[derive(Debug, Clone)]
pub struct Slider {
    id: WidgetId,
    label: String,
    min: f64,
    max: f64,
    value: f64,
    step: f64,
    large_step: f64,
    enabled: bool,
    bounds: Rect,
    interaction: Interaction,
    dragging: bool,
    changed: Option<f64>,
}

impl Slider {
    /// A slider at `value`. `label` is the accessible name. `step` must be positive; a
    /// non-positive step is replaced by `(max - min) / 100`.
    #[must_use]
    pub fn new(label: impl Into<String>, min: f64, max: f64, value: f64, step: f64) -> Self {
        let (min, max) = if min <= max { (min, max) } else { (max, min) };
        let step = if step > 0.0 {
            step
        } else {
            (max - min) / 100.0
        };
        let mut slider = Self {
            id: WidgetId::allocate(),
            label: label.into(),
            min,
            max,
            value: min,
            step,
            large_step: step * 10.0,
            enabled: true,
            bounds: Rect::ZERO,
            interaction: Interaction::new(Motion::full()),
            dragging: false,
            changed: None,
        };
        slider.value = slider.snap(value);
        slider
    }

    #[must_use]
    pub fn with_large_step(mut self, large_step: f64) -> Self {
        self.large_step = large_step;
        self
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.interaction.set_motion(motion);
        self
    }

    #[must_use]
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Sets the value (snapped to the step grid and clamped) without notifying the owner.
    pub fn set_value(&mut self, value: f64) {
        self.value = self.snap(value);
    }

    #[must_use]
    pub fn range(&self) -> (f64, f64) {
        (self.min, self.max)
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
    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// The new value if the user changed it since the last call.
    pub fn take_changed(&mut self) -> Option<f64> {
        self.changed.take()
    }

    fn snap(&self, value: f64) -> f64 {
        let steps = ((value - self.min) / self.step).round();
        (self.min + steps * self.step).clamp(self.min, self.max)
    }

    /// Changes the value and queues the notification. Returns whether it changed.
    fn change(&mut self, cx: &mut EventCx, value: f64) -> bool {
        let value = self.snap(value);
        if value == self.value {
            return false;
        }
        self.value = value;
        self.changed = Some(value);
        cx.invalidate(self.bounds.outset(Insets::uniform(2.0)));
        true
    }

    /// The part of the track the thumb centre travels on.
    fn travel(&self) -> (f32, f32) {
        (
            self.bounds.x + THUMB / 2.0,
            (self.bounds.right() - THUMB / 2.0).max(self.bounds.x + THUMB / 2.0),
        )
    }

    fn fraction(&self) -> f32 {
        if self.max <= self.min {
            0.0
        } else {
            ((self.value - self.min) / (self.max - self.min)) as f32
        }
    }

    /// The thumb's bounds.
    #[must_use]
    pub fn thumb_rect(&self) -> Rect {
        let (left, right) = self.travel();
        let x = left + (right - left) * self.fraction();
        let center = self.bounds.center();
        Rect::new(x - THUMB / 2.0, center.y - THUMB / 2.0, THUMB, THUMB)
    }

    fn value_at(&self, x: f32) -> f64 {
        let (left, right) = self.travel();
        if right <= left {
            return self.min;
        }
        let fraction = f64::from(((x - left) / (right - left)).clamp(0.0, 1.0));
        self.min + fraction * (self.max - self.min)
    }

    fn pointer(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        if !self.enabled {
            return EventResult::ignored();
        }
        let left = event.button == Some(PointerButton::Left);
        let inside = self.bounds.contains(event.position);
        if event.kind == PointerEventKind::Move && (inside || self.dragging) {
            cx.set_cursor(Cursor::Hand);
        }
        match event.kind {
            PointerEventKind::Down if left && inside => {
                self.dragging = true;
                self.interaction.set_hovered(cx, true);
                self.interaction.set_pressed(cx, true);
                cx.capture_pointer();
                self.change(cx, self.value_at(event.position.x));
                cx.invalidate(self.bounds);
                EventResult::handled_with_focus()
            }
            PointerEventKind::Move if self.dragging => {
                self.change(cx, self.value_at(event.position.x));
                EventResult::handled()
            }
            PointerEventKind::Up if left && self.dragging => {
                self.dragging = false;
                self.interaction.set_pressed(cx, false);
                self.interaction.set_hovered(cx, inside);
                cx.release_pointer();
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            PointerEventKind::Move | PointerEventKind::Enter => {
                let over = self.thumb_rect().contains(event.position);
                if self.interaction.set_hovered(cx, over) {
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            PointerEventKind::Leave if !self.dragging => {
                if self.interaction.set_hovered(cx, false) {
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            _ => EventResult::ignored(),
        }
    }

    fn key(&mut self, cx: &mut EventCx, key: u16) -> bool {
        let target = match key {
            vk::LEFT | vk::DOWN => self.value - self.step,
            vk::RIGHT | vk::UP => self.value + self.step,
            vk::PAGE_DOWN => self.value - self.large_step,
            vk::PAGE_UP => self.value + self.large_step,
            vk::HOME => self.min,
            vk::END => self.max,
            _ => return false,
        };
        self.change(cx, target);
        true
    }
}

impl Control for Slider {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for Slider {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        let width = if available.width.is_finite() {
            available.width
        } else {
            DEFAULT_WIDTH
        };
        Size::new(width.max(THUMB), CONTROL_HEIGHT)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        if self.enabled {
            cx.add_tab_stop();
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        let (left, right) = self.travel();
        let thumb = self.thumb_rect();
        let center_y = self.bounds.center().y;
        let radii = CornerRadii::uniform(TRACK / 2.0);
        let (rail, fill) = if self.enabled {
            (tokens.text_muted.with_alpha(0.5), tokens.accent)
        } else {
            (tokens.text_disabled.with_alpha(0.5), tokens.text_disabled)
        };
        canvas.fill_rounded_rect(
            Rect::new(left, center_y - TRACK / 2.0, right - left, TRACK),
            radii,
            rail,
        );
        let filled = thumb.center().x - left;
        if filled > 0.0 {
            canvas.fill_rounded_rect(
                Rect::new(left, center_y - TRACK / 2.0, filled, TRACK),
                radii,
                fill,
            );
        }
        let round = CornerRadii::uniform(THUMB / 2.0);
        canvas.fill_rounded_rect(thumb, round, tokens.surface);
        canvas.stroke_rounded_rect(
            thumb.inset(Insets::uniform(0.5)),
            round,
            tokens.hairline,
            1.0,
        );
        // The inner dot grows on hover and shrinks while pressed.
        let dot =
            10.0 + 2.0 * self.interaction.hover_amount() - 4.0 * self.interaction.press_amount();
        let dot_rect = Rect::new(
            thumb.center().x - dot / 2.0,
            thumb.center().y - dot / 2.0,
            dot,
            dot,
        );
        canvas.fill_rounded_rect(dot_rect, CornerRadii::uniform(dot / 2.0), fill);
        if cx.is_focused() {
            focus_ring::paint(canvas, thumb, THUMB / 2.0, cx.theme);
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => self.pointer(cx, event),
            Event::Key(key)
                if key.is_down && self.enabled && cx.is_focused() && key.mods.is_empty() =>
            {
                if self.key(cx, key.vk) {
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) => {
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
        let mut node = AccessNode::new(self.id, Role::Slider, self.label.clone(), self.bounds)
            .with_state(control_state(cx, self.enabled))
            .with_value(self.value.to_string());
        node.patterns.range_value = Some(RangeValue {
            value: self.value,
            min: self.min,
            max: self.max,
            step: self.step,
        });
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::SetRangeValue(value) if self.enabled => {
                self.change(cx, *value);
                true
            }
            AccessAction::Focus if self.enabled => perform_focus(cx),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::geometry::Point;
    use crate::input::Modifiers;
    use crate::widgets::testkit::Harness;

    fn placed(value: f64) -> (Harness, Slider, Rect) {
        let mut harness = Harness::new();
        let mut slider = Slider::new("Volume", 0.0, 100.0, value, 1.0)
            .with_large_step(10.0)
            .with_motion(Motion::reduced());
        let bounds = harness.place(&mut slider, Point::new(10.0, 10.0));
        (harness, slider, bounds)
    }

    #[test]
    fn kit_slider_measures_and_snaps_initial_value() {
        let (harness, slider, bounds) = placed(250.0);
        assert_eq!(bounds.size(), Size::new(200.0, 32.0));
        assert_eq!(slider.value(), 100.0);
        assert!(harness.focus.can_focus(slider.id()));
        assert_eq!(Slider::new("x", 0.0, 10.0, 3.4, 0.5).value(), 3.5);
        // Reversed bounds are normalised and a bad step falls back to a hundredth of the range.
        let odd = Slider::new("x", 10.0, 0.0, 5.0, 0.0);
        assert_eq!(odd.range(), (0.0, 10.0));
        assert_eq!(odd.value(), 5.0);
        let mut harness = Harness::new();
        let mut narrow = Slider::new("x", 0.0, 1.0, 0.0, 0.1);
        let cx_size = {
            let id = narrow.id();
            let mut cx = LayoutCx::new(
                id,
                &harness.theme,
                1.0,
                &harness.engine,
                &mut harness.focus,
                &mut harness.requests,
            );
            narrow.measure(&mut cx, Size::new(5.0, 40.0))
        };
        assert_eq!(cx_size.width, 20.0);
    }

    #[test]
    fn kit_slider_keyboard_steps_pages_and_ends() {
        let (mut harness, mut slider, _) = placed(50.0);
        // Not focused: ignored.
        assert!(!harness.key(&mut slider, vk::RIGHT).handled);
        harness.focus_widget(&mut slider);
        let table = [
            (vk::RIGHT, 51.0),
            (vk::UP, 52.0),
            (vk::LEFT, 51.0),
            (vk::DOWN, 50.0),
            (vk::PAGE_UP, 60.0),
            (vk::PAGE_DOWN, 50.0),
            (vk::HOME, 0.0),
            (vk::LEFT, 0.0),
            (vk::END, 100.0),
            (vk::RIGHT, 100.0),
        ];
        for (key, expected) in table {
            assert!(harness.key(&mut slider, key).handled, "key {key:#x}");
            assert_eq!(slider.value(), expected, "key {key:#x}");
        }
        assert!(!harness.key(&mut slider, vk::TAB).handled);
        assert!(
            !harness
                .key_with(&mut slider, vk::LEFT, Modifiers::CTRL)
                .handled
        );
        assert_eq!(slider.take_changed(), Some(100.0));
        assert_eq!(slider.take_changed(), None);
        // Moving by a key that changes nothing queues nothing.
        harness.key(&mut slider, vk::END);
        assert_eq!(slider.take_changed(), None);
    }

    #[test]
    fn kit_slider_requests_the_hand_over_the_track_and_while_dragging() {
        let (mut harness, mut slider, bounds) = placed(0.0);
        let y = bounds.center().y;
        harness.hover(&mut slider, Point::new(110.0, y));
        assert_eq!(harness.requests.take_cursor(), Some(Cursor::Hand));
        harness.hover(&mut slider, Point::new(1000.0, y));
        assert_eq!(harness.requests.take_cursor(), None);
        harness.pointer(
            &mut slider,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            Point::new(110.0, y),
        );
        harness.requests.take_cursor();
        harness.hover(&mut slider, Point::new(1000.0, y));
        assert_eq!(harness.requests.take_cursor(), Some(Cursor::Hand));
        slider.set_enabled(false);
        harness.pointer(
            &mut slider,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::new(1000.0, y),
        );
        harness.hover(&mut slider, Point::new(110.0, y));
        assert_eq!(harness.requests.take_cursor(), None);
    }

    #[test]
    fn kit_slider_pointer_press_drag_and_release() {
        let (mut harness, mut slider, bounds) = placed(0.0);
        let y = bounds.center().y;
        // Travel runs from x = 20 to x = 200 (thumb centre).
        let result = harness.pointer(
            &mut slider,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            Point::new(110.0, y),
        );
        assert!(result.handled && result.request_focus);
        assert!(slider.is_dragging() && harness.capture.is_captured_by(slider.id()));
        assert_eq!(slider.value(), 50.0);
        assert_eq!(slider.take_changed(), Some(50.0));
        harness.pointer(
            &mut slider,
            PointerEventKind::Move,
            None,
            Point::new(1000.0, y),
        );
        assert_eq!(slider.value(), 100.0);
        harness.pointer(
            &mut slider,
            PointerEventKind::Move,
            None,
            Point::new(-1000.0, y),
        );
        assert_eq!(slider.value(), 0.0);
        assert!(
            harness
                .pointer(
                    &mut slider,
                    PointerEventKind::Up,
                    Some(PointerButton::Left),
                    Point::new(20.0, y)
                )
                .handled
        );
        assert!(!slider.is_dragging());
        assert!(!harness.capture.is_captured_by(slider.id()));
        assert_eq!(slider.take_changed(), Some(0.0));
        // Stray releases, right clicks and presses outside are ignored.
        assert!(
            !harness
                .pointer(
                    &mut slider,
                    PointerEventKind::Up,
                    Some(PointerButton::Left),
                    Point::new(20.0, y)
                )
                .handled
        );
        assert!(
            !harness
                .pointer(
                    &mut slider,
                    PointerEventKind::Down,
                    Some(PointerButton::Right),
                    Point::new(110.0, y)
                )
                .handled
        );
        assert!(
            !harness
                .pointer(
                    &mut slider,
                    PointerEventKind::Down,
                    Some(PointerButton::Left),
                    Point::new(900.0, y)
                )
                .handled
        );
    }

    #[test]
    fn kit_slider_hover_follows_the_thumb_and_dot_animates() {
        let (mut harness, mut slider, bounds) = placed(0.0);
        let y = bounds.center().y;
        let thumb = slider.thumb_rect();
        assert_eq!(thumb, Rect::new(10.0, y - 10.0, 20.0, 20.0));
        assert!(!harness.hover(&mut slider, Point::new(150.0, y)).handled);
        assert!(harness.hover(&mut slider, thumb.center()).handled);
        let dot_width = |harness: &mut Harness, slider: &Slider| {
            let canvas = harness.paint(slider);
            let widths: Vec<f32> = canvas
                .ops()
                .filter_map(|op| match op {
                    DrawOp::FillRoundedRect { rect, .. }
                        if rect.width < 14.0 && rect.width > 1.0 && rect.height == rect.width =>
                    {
                        Some(rect.width)
                    }
                    _ => None,
                })
                .collect();
            widths[0]
        };
        assert_eq!(dot_width(&mut harness, &slider), 12.0);
        harness.pointer(
            &mut slider,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            thumb.center(),
        );
        assert_eq!(dot_width(&mut harness, &slider), 8.0);
        harness.pointer(&mut slider, PointerEventKind::Leave, None, Point::ZERO);
        harness.pointer(
            &mut slider,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::new(500.0, 500.0),
        );
        assert_eq!(dot_width(&mut harness, &slider), 10.0);
        assert!(
            !harness
                .pointer(&mut slider, PointerEventKind::Leave, None, Point::ZERO)
                .handled
        );
        harness.hover(&mut slider, thumb.center());
        assert!(
            harness
                .pointer(&mut slider, PointerEventKind::Leave, None, Point::ZERO)
                .handled
        );
    }

    #[test]
    fn kit_slider_paints_rail_fill_thumb_and_focus() {
        let (mut harness, mut slider, bounds) = placed(50.0);
        let tokens = harness.theme.tokens;
        let canvas = harness.paint(&slider);
        let center_y = bounds.center().y;
        let rects: Vec<(Rect, crate::geometry::Color)> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::FillRoundedRect { rect, color, .. } => Some((*rect, *color)),
                _ => None,
            })
            .collect();
        assert_eq!(rects[0].0, Rect::new(20.0, center_y - 2.0, 180.0, 4.0));
        assert_eq!(
            rects[1],
            (Rect::new(20.0, center_y - 2.0, 90.0, 4.0), tokens.accent)
        );
        assert_eq!(
            rects[2],
            (
                Rect::new(100.0, center_y - 10.0, 20.0, 20.0),
                tokens.surface
            )
        );
        assert_eq!(rects[3].1, tokens.accent);
        let stroke_widths: Vec<f32> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::StrokeRoundedRect { width, .. } => Some(*width),
                _ => None,
            })
            .collect();
        // The thumb's 1 DIP border is drawn; the 2 DIP focus ring is not.
        assert_eq!(
            stroke_widths,
            [1.0],
            "no focus ring while unfocused: {stroke_widths:?}"
        );
        harness.focus_widget(&mut slider);
        let canvas = harness.paint(&slider);
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::StrokeRoundedRect { width, .. } if *width == 2.0))
        );
        // At the minimum there is no filled part; a disabled slider is grey.
        slider.set_value(0.0);
        slider.set_enabled(false);
        let canvas = harness.paint(&slider);
        let fills = canvas
            .ops()
            .filter(|op| matches!(op, DrawOp::FillRoundedRect { .. }))
            .count();
        assert_eq!(fills, 3);
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::FillRoundedRect { color, .. } if *color == tokens.text_disabled)));
    }

    #[test]
    fn kit_slider_access_node_and_actions() {
        let (mut harness, mut slider, bounds) = placed(25.0);
        let node = harness.node(&slider);
        assert_eq!(node.role, Role::Slider);
        assert_eq!(node.name, "Volume");
        assert_eq!(node.value.as_deref(), Some("25"));
        assert_eq!(node.bounds, bounds);
        let range = node.patterns.range_value.expect("range");
        assert_eq!(
            (range.value, range.min, range.max, range.step),
            (25.0, 0.0, 100.0, 1.0)
        );
        assert!(harness.perform(&mut slider, AccessAction::SetRangeValue(40.4)));
        assert_eq!(slider.value(), 40.0);
        assert_eq!(slider.take_changed(), Some(40.0));
        assert!(harness.perform(&mut slider, AccessAction::Focus));
        assert!(!harness.perform(&mut slider, AccessAction::Invoke));
        slider.set_enabled(false);
        harness.arrange(&mut slider, bounds);
        assert!(!harness.perform(&mut slider, AccessAction::SetRangeValue(1.0)));
        assert!(!harness.perform(&mut slider, AccessAction::Focus));
        assert!(harness.node(&slider).state.disabled);
        assert!(!slider.is_enabled());
        assert!(!harness.key(&mut slider, vk::RIGHT).handled);
        assert!(!harness.click(&mut slider, bounds.center()).handled);
        assert!(!harness.focus.can_focus(slider.id()));
        assert_eq!(slider.bounds(), bounds);
    }

    #[test]
    fn kit_slider_focus_and_animation_frames_invalidate() {
        let mut harness = Harness::new();
        let mut slider = Slider::new("x", 0.0, 10.0, 5.0, 1.0);
        let bounds = harness.place(&mut slider, Point::ZERO);
        let thumb = slider.thumb_rect();
        harness.hover(&mut slider, thumb.center());
        assert!(harness.requests.animation_frame_requested());
        harness.requests.take_invalidation();
        harness.run_animations(&mut slider);
        assert!(!harness.requests.invalidation().is_clean());
        harness.requests.take_invalidation();
        harness.send(&mut slider, Event::Focus(FocusEvent::Gained));
        assert!(!harness.requests.invalidation().is_clean());
        // A zero-width range keeps the thumb at the start.
        let mut flat = Slider::new("x", 5.0, 5.0, 5.0, 1.0);
        harness.arrange(&mut flat, bounds);
        assert_eq!(flat.thumb_rect().x, bounds.x);
        harness.pointer(
            &mut flat,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            Point::new(100.0, 16.0),
        );
        assert_eq!(flat.value(), 5.0);
        assert_eq!(flat.take_changed(), None);
    }

    #[test]
    fn kit_slider_idle_animation_frame_repaints_nothing() {
        let (mut harness, mut slider, _) = placed(50.0);
        harness.requests.take_invalidation();
        harness.frame(&mut slider, 16);
        assert!(harness.requests.invalidation().is_clean());
        assert!(!harness.requests.animation_frame_requested());
        assert_eq!(slider.value(), 50.0);
    }

    #[test]
    fn kit_slider_with_id_and_paint_damage() {
        let id = WidgetId::allocate();
        let mut slider = Slider::new("Volume", 0.0, 100.0, 50.0, 1.0).with_id(id);
        assert_eq!(slider.id(), id);
        let mut harness = crate::widgets::testkit::Harness::new();
        assert_eq!(harness.node(&slider).id, id);
        let bounds = harness.place(&mut slider, Point::new(10.0, 10.0));
        let away = Rect::new(500.0, 500.0, 5.0, 5.0);
        assert_eq!(harness.paint_damaged(&slider, Some(away)).ops().count(), 0);
        assert!(harness.paint_damaged(&slider, Some(bounds)).ops().count() > 0);
    }

    #[test]
    fn kit_slider_without_travel_has_nothing_to_drag_to() {
        let mut harness = crate::widgets::testkit::Harness::new();
        let mut slider = Slider::new("Volume", 10.0, 20.0, 15.0, 1.0);
        // A track too short for the thumb has no travel: every position reads as the minimum.
        harness.arrange(&mut slider, Rect::new(0.0, 0.0, 4.0, 32.0));
        harness.focus.focus(slider.id());
        harness.click(&mut slider, Point::new(2.0, 16.0));
        assert_eq!(slider.value(), 10.0);
    }
}
