//! The Fluent toggle switch: a 40×20 pill with a sliding knob and a label.

use crate::access::{AccessAction, AccessNode, AccessTree, Role, ToggleState};
use crate::canvas::Canvas;
use crate::geometry::{CornerRadii, Insets, Rect, Size};
use crate::input::{Event, FocusEvent, vk};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::anim::{Easing, Motion, PRESS_DURATION, Transition};
use super::{
    CONTROL_HEIGHT, Control, FONT_SIZE, Interaction, PointerOutcome, control_state,
    draw_left_centered, focus_ring, layout_line, outcome_result, perform_focus,
};

/// Track size of the switch.
pub const TRACK: Size = Size::new(40.0, 20.0);
/// Gap between the switch and its label.
const LABEL_GAP: f32 = 12.0;
/// Knob diameter at rest.
const KNOB: f32 = 12.0;
/// Knob diameter while hovered or pressed.
const KNOB_ACTIVE: f32 = 14.0;
/// Space between the track edge and the knob at rest.
const KNOB_MARGIN: f32 = 4.0;

/// A two-state switch with a label.
#[derive(Debug, Clone)]
pub struct ToggleSwitch {
    id: WidgetId,
    label: String,
    on: bool,
    enabled: bool,
    bounds: Rect,
    interaction: Interaction,
    knob: Transition,
    changed: Option<bool>,
}

impl ToggleSwitch {
    #[must_use]
    pub fn new(label: impl Into<String>, on: bool) -> Self {
        let motion = Motion::full();
        Self {
            id: WidgetId::allocate(),
            label: label.into(),
            on,
            enabled: true,
            bounds: Rect::ZERO,
            interaction: Interaction::new(motion),
            knob: Transition::new(if on { 1.0 } else { 0.0 }, motion),
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
        self.interaction.set_motion(motion);
        self.knob.set_motion(motion);
        self
    }

    #[must_use]
    pub fn is_on(&self) -> bool {
        self.on
    }

    /// Sets the state without notifying the owner and without animating.
    pub fn set_on(&mut self, on: bool) {
        self.on = on;
        self.knob.set_target(
            f32::from(u8::from(on)),
            std::time::Duration::ZERO,
            Easing::Linear,
        );
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The new state if the user changed the switch since the last call.
    pub fn take_changed(&mut self) -> Option<bool> {
        self.changed.take()
    }

    fn track_rect(&self) -> Rect {
        Rect::new(
            self.bounds.x,
            self.bounds.y + (self.bounds.height - TRACK.height) / 2.0,
            TRACK.width,
            TRACK.height,
        )
    }

    fn toggle(&mut self, cx: &mut EventCx) {
        self.on = !self.on;
        let target = f32::from(u8::from(self.on));
        if self
            .knob
            .set_target(target, PRESS_DURATION, Easing::Decelerate)
        {
            cx.request_animation_frame();
        }
        self.changed = Some(self.on);
        cx.invalidate(self.bounds);
    }
}

impl Control for ToggleSwitch {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for ToggleSwitch {
    fn measure(&mut self, cx: &mut LayoutCx, _available: Size) -> Size {
        let label = layout_line(cx.text, cx.theme, &self.label, FONT_SIZE)
            .size()
            .width;
        let gap = if self.label.is_empty() {
            0.0
        } else {
            LABEL_GAP
        };
        Size::new(TRACK.width + gap + label, CONTROL_HEIGHT)
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
        let track = self.track_rect();
        let radii = CornerRadii::uniform(TRACK.height / 2.0);
        let progress = self.knob.value();
        let (fill, knob_color) = if !self.enabled {
            (tokens.hairline, tokens.text_disabled)
        } else {
            (
                tokens.accent.with_alpha(progress),
                tokens.text_muted.lerp(tokens.accent_text_on, progress),
            )
        };
        canvas.fill_rounded_rect(track, radii, fill);
        if progress < 1.0 {
            canvas.stroke_rounded_rect(
                track.inset(Insets::uniform(0.5)),
                radii,
                if self.enabled {
                    tokens.text_muted
                } else {
                    tokens.text_disabled
                },
                1.0,
            );
        }
        let diameter = KNOB
            + (KNOB_ACTIVE - KNOB)
                * if self.enabled {
                    self.interaction
                        .hover_amount()
                        .max(self.interaction.press_amount())
                } else {
                    0.0
                };
        let travel = TRACK.width - 2.0 * KNOB_MARGIN - KNOB;
        let center_x = track.x + KNOB_MARGIN + KNOB / 2.0 + travel * progress;
        let knob = Rect::new(
            center_x - diameter / 2.0,
            track.y + (track.height - diameter) / 2.0,
            diameter,
            diameter,
        );
        canvas.fill_rounded_rect(knob, CornerRadii::uniform(diameter / 2.0), knob_color);
        if !self.label.is_empty() {
            let text = layout_line(cx.text, cx.theme, &self.label, FONT_SIZE);
            let color = if self.enabled {
                tokens.text
            } else {
                tokens.text_disabled
            };
            draw_left_centered(
                canvas,
                text.as_ref(),
                track.right() + LABEL_GAP,
                self.bounds,
                color,
            );
        }
        if cx.is_focused() {
            focus_ring::paint(canvas, self.bounds, tokens.metrics.control_radius, cx.theme);
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => {
                let outcome = self
                    .interaction
                    .pointer(cx, self.bounds, event, self.enabled);
                if outcome == PointerOutcome::Clicked {
                    self.toggle(cx);
                }
                outcome_result(outcome)
            }
            Event::Key(key)
                if key.is_down
                    && self.enabled
                    && cx.is_focused()
                    && key.vk == vk::SPACE
                    && key.mods.is_empty() =>
            {
                if !key.repeat {
                    self.toggle(cx);
                }
                EventResult::handled()
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) => {
                cx.invalidate(self.bounds.outset(Insets::uniform(2.0)));
                EventResult::ignored()
            }
            Event::AnimationFrame(now) => {
                let interacting = self.interaction.animate(cx, *now);
                let moving = self.knob.tick(*now);
                if moving {
                    cx.request_animation_frame();
                }
                if interacting || moving {
                    cx.invalidate(self.bounds);
                }
                EventResult::ignored()
            }
            _ => EventResult::ignored(),
        }
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let mut state = control_state(cx, self.enabled);
        state.toggle = Some(if self.on {
            ToggleState::On
        } else {
            ToggleState::Off
        });
        let mut node =
            AccessNode::new(self.id, Role::ToggleButton, self.label.clone(), self.bounds)
                .with_state(state);
        node.patterns.toggle = true;
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::Toggle if self.enabled => {
                self.toggle(cx);
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
    use crate::widgets::testkit::Harness;

    fn setup(on: bool) -> (Harness, ToggleSwitch, Rect) {
        let mut harness = Harness::new();
        let mut toggle = ToggleSwitch::new("Wrap", on).with_motion(Motion::reduced());
        let bounds = harness.place(&mut toggle, Point::new(10.0, 10.0));
        (harness, toggle, bounds)
    }

    fn knob_rect(canvas: &crate::canvas::RecordingCanvas) -> Rect {
        canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::FillRoundedRect { rect, .. } => Some(*rect),
                _ => None,
            })
            .nth(1)
            .expect("no knob")
    }

    #[test]
    fn kit_toggle_measure_and_click() {
        let (mut harness, mut toggle, bounds) = setup(false);
        assert_eq!(bounds.size(), Size::new(40.0 + 12.0 + 32.0, 32.0));
        assert!(!toggle.is_on());
        assert!(harness.click(&mut toggle, bounds.center()).request_focus);
        assert!(toggle.is_on());
        assert_eq!(toggle.take_changed(), Some(true));
        assert_eq!(toggle.take_changed(), None);
        harness.click(&mut toggle, bounds.center());
        assert_eq!(toggle.take_changed(), Some(false));
        assert_eq!(toggle.label(), "Wrap");
        assert_eq!(toggle.bounds(), bounds);
        let mut bare = ToggleSwitch::new("", false);
        assert_eq!(harness.place(&mut bare, Point::ZERO).width, 40.0);
    }

    #[test]
    fn kit_toggle_space_toggles_when_focused() {
        let (mut harness, mut toggle, _) = setup(false);
        assert!(!harness.key(&mut toggle, vk::SPACE).handled);
        harness.focus_widget(&mut toggle);
        assert!(harness.key(&mut toggle, vk::SPACE).handled);
        assert_eq!(toggle.take_changed(), Some(true));
        let repeat = crate::input::KeyEvent {
            repeat: true,
            ..crate::input::KeyEvent::press(vk::SPACE, crate::input::Modifiers::NONE)
        };
        assert!(harness.send(&mut toggle, Event::Key(repeat)).handled);
        assert_eq!(toggle.take_changed(), None);
        assert!(!harness.key(&mut toggle, vk::RETURN).handled);
    }

    #[test]
    fn kit_toggle_access_and_perform() {
        let (mut harness, mut toggle, _) = setup(true);
        let node = harness.node(&toggle);
        assert_eq!(node.role, Role::ToggleButton);
        assert_eq!(node.state.toggle, Some(ToggleState::On));
        assert!(node.patterns.toggle);
        assert!(harness.perform(&mut toggle, AccessAction::Toggle));
        assert_eq!(harness.node(&toggle).state.toggle, Some(ToggleState::Off));
        assert_eq!(toggle.take_changed(), Some(false));
        assert!(harness.perform(&mut toggle, AccessAction::Focus));
        assert!(!harness.perform(&mut toggle, AccessAction::Invoke));
        toggle.set_enabled(false);
        assert!(!toggle.is_enabled());
        assert!(!harness.perform(&mut toggle, AccessAction::Toggle));
        assert!(!harness.perform(&mut toggle, AccessAction::Focus));
        assert!(harness.node(&toggle).state.disabled);
    }

    #[test]
    fn kit_toggle_knob_position_follows_state() {
        let (mut harness, mut toggle, bounds) = setup(false);
        let off = knob_rect(&harness.paint(&toggle));
        assert_eq!(off, Rect::new(bounds.x + 4.0, bounds.y + 10.0, 12.0, 12.0));
        toggle.set_on(true);
        let on = knob_rect(&harness.paint(&toggle));
        // Travel is 40 - 2*4 - 12 = 20.
        assert_eq!(on.x, off.x + 20.0);
        let canvas = harness.paint(&toggle);
        let tokens = harness.theme.tokens;
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::FillRoundedRect { color, .. } if *color == tokens.accent
        )));
        // The border is only drawn while the track is not fully on.
        let stroke_widths = |canvas: &crate::canvas::RecordingCanvas| -> Vec<f32> {
            canvas
                .ops()
                .filter_map(|op| match op {
                    DrawOp::StrokeRoundedRect { width, .. } => Some(*width),
                    _ => None,
                })
                .collect()
        };
        assert!(stroke_widths(&canvas).is_empty());
        assert_eq!(canvas.texts(), vec!["Wrap"]);
        toggle.set_on(false);
        let canvas = harness.paint(&toggle);
        assert_eq!(stroke_widths(&canvas), vec![1.0]);
    }

    #[test]
    fn kit_toggle_knob_grows_on_hover_and_animates() {
        let mut harness = Harness::new();
        let mut toggle = ToggleSwitch::new("Wrap", false);
        let bounds = harness.place(&mut toggle, Point::ZERO);
        harness.hover(&mut toggle, bounds.center());
        harness.run_animations(&mut toggle);
        assert_eq!(knob_rect(&harness.paint(&toggle)).width, 14.0);
        harness.click(&mut toggle, bounds.center());
        // Mid-animation the knob is between its two resting positions: the knob center travels
        // from 4 + 6 = 10 (off) to 10 + 20 = 30 (on) with a decelerating curve.
        harness.frame(&mut toggle, 0);
        harness.frame(&mut toggle, 20);
        let mid = knob_rect(&harness.paint(&toggle));
        let center = mid.x + mid.width / 2.0;
        assert!(
            center > 10.0 + 1.0 && center < 30.0 - 1.0,
            "mid center {center}"
        );
        harness.run_animations(&mut toggle);
        let end = knob_rect(&harness.paint(&toggle));
        assert_eq!(end.x + end.width / 2.0, 30.0);
    }

    #[test]
    fn kit_toggle_disabled_paint_and_focus_ring() {
        let (mut harness, mut toggle, bounds) = setup(true);
        harness.focus_widget(&mut toggle);
        let canvas = harness.paint(&toggle);
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::StrokeRoundedRect { width, .. } if *width == 2.0))
        );
        toggle.set_enabled(false);
        harness.arrange(&mut toggle, bounds);
        assert!(!harness.focus.can_focus(toggle.id()));
        let canvas = harness.paint(&toggle);
        let tokens = harness.theme.tokens;
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::Text { color, .. } if *color == tokens.text_disabled
        )));
        assert!(!harness.click(&mut toggle, bounds.center()).handled);
        assert_eq!(toggle.take_changed(), None);
        assert!(toggle.is_on());
        harness.send(&mut toggle, Event::Focus(FocusEvent::Lost));
    }

    #[test]
    fn kit_toggle_with_id_and_disabled_paint_and_damage() {
        let id = WidgetId::allocate();
        let mut toggle = ToggleSwitch::new("Wrap", true).with_id(id);
        assert_eq!(toggle.id(), id);
        let mut harness = crate::widgets::testkit::Harness::new();
        assert_eq!(harness.node(&toggle).id, id);
        let bounds = harness.place(&mut toggle, Point::new(10.0, 10.0));
        let away = Rect::new(500.0, 500.0, 5.0, 5.0);
        assert_eq!(harness.paint_damaged(&toggle, Some(away)).ops().count(), 0);
        assert!(harness.paint_damaged(&toggle, Some(bounds)).ops().count() > 0);
        // A disabled switch and its label are drawn dimmed.
        let tokens = harness.theme.tokens;
        toggle.set_enabled(false);
        let canvas = harness.paint(&toggle);
        assert!(canvas.ops().any(|op| matches!(
            op,
            crate::canvas::DrawOp::Text { color, .. } if *color == tokens.text_disabled
        )));
        // An off switch outlines its track in the disabled color too.
        let mut off = ToggleSwitch::new("Wrap", false);
        off.set_enabled(false);
        harness.place(&mut off, Point::new(10.0, 60.0));
        assert!(harness.paint(&off).ops().any(|op| matches!(
            op,
            crate::canvas::DrawOp::StrokeRoundedRect { color, .. } if *color == tokens.text_disabled
        )));
    }

    #[test]
    fn kit_toggle_without_a_label_draws_no_text() {
        let mut toggle = ToggleSwitch::new("", true);
        let mut harness = crate::widgets::testkit::Harness::new();
        harness.place(&mut toggle, Point::new(10.0, 10.0));
        assert!(harness.paint(&toggle).texts().is_empty());
    }
}
