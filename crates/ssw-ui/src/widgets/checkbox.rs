//! The Fluent check box, optionally tri-state.

use crate::access::{AccessAction, AccessNode, AccessTree, Role, ToggleState};
use crate::canvas::{Canvas, IconFont};
use crate::geometry::{CornerRadii, Insets, Rect, Size};
use crate::input::{Event, FocusEvent, vk};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::anim::Motion;
use super::{
    CONTROL_HEIGHT, Control, FONT_SIZE, Interaction, PointerOutcome, control_state,
    draw_left_centered, focus_ring, glyph, layout_line, outcome_result, perform_focus,
};

/// Side of the box.
pub const BOX: f32 = 20.0;
/// Gap between the box and its label.
const LABEL_GAP: f32 = 8.0;
/// Em size of the check glyph.
const GLYPH: f32 = 12.0;

/// A check box with a label. With `tri_state`, activating cycles
/// unchecked → checked → indeterminate → unchecked; otherwise it toggles between the first two.
#[derive(Debug, Clone)]
pub struct CheckBox {
    id: WidgetId,
    label: String,
    state: ToggleState,
    tri_state: bool,
    enabled: bool,
    bounds: Rect,
    interaction: Interaction,
    changed: Option<ToggleState>,
}

impl CheckBox {
    #[must_use]
    pub fn new(label: impl Into<String>, state: ToggleState) -> Self {
        Self {
            id: WidgetId::allocate(),
            label: label.into(),
            state,
            tri_state: false,
            enabled: true,
            bounds: Rect::ZERO,
            interaction: Interaction::new(Motion::full()),
            changed: None,
        }
    }

    /// Lets the user cycle through the indeterminate state.
    #[must_use]
    pub fn tri_state(mut self) -> Self {
        self.tri_state = true;
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
    pub fn state(&self) -> ToggleState {
        self.state
    }

    /// Sets the state without notifying the owner.
    pub fn set_state(&mut self, state: ToggleState) {
        self.state = state;
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

    /// The new state if the user changed the check box since the last call.
    pub fn take_changed(&mut self) -> Option<ToggleState> {
        self.changed.take()
    }

    fn box_rect(&self) -> Rect {
        Rect::new(
            self.bounds.x,
            self.bounds.y + (self.bounds.height - BOX) / 2.0,
            BOX,
            BOX,
        )
    }

    fn cycle(&mut self, cx: &mut EventCx) {
        self.state = match (self.state, self.tri_state) {
            (ToggleState::Off, _) => ToggleState::On,
            (ToggleState::On, true) => ToggleState::Indeterminate,
            (ToggleState::On | ToggleState::Indeterminate, false)
            | (ToggleState::Indeterminate, true) => ToggleState::Off,
        };
        self.changed = Some(self.state);
        cx.invalidate(self.bounds);
    }
}

impl Control for CheckBox {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for CheckBox {
    fn measure(&mut self, cx: &mut LayoutCx, _available: Size) -> Size {
        let label = layout_line(cx.text, cx.theme, &self.label, FONT_SIZE)
            .size()
            .width;
        let gap = if self.label.is_empty() {
            0.0
        } else {
            LABEL_GAP
        };
        Size::new(BOX + gap + label, CONTROL_HEIGHT)
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
        let rect = self.box_rect();
        let radii = CornerRadii::uniform(tokens.metrics.control_radius);
        let checked = self.state != ToggleState::Off;
        if checked {
            let fill = if self.enabled {
                tokens.accent
            } else {
                tokens.text_disabled
            };
            let fill = if self.enabled {
                self.interaction
                    .blend(fill, fill.with_alpha(0.9), fill.with_alpha(0.8))
            } else {
                fill
            };
            canvas.fill_rounded_rect(rect, radii, fill);
            let mark = match self.state {
                ToggleState::Indeterminate => glyph::REMOVE,
                _ => glyph::CHECK_MARK,
            };
            canvas.draw_icon(
                mark,
                IconFont::SegoeFluent,
                GLYPH,
                tokens.accent_text_on,
                rect.center(),
            );
        } else {
            let fill = if self.enabled {
                self.interaction.blend(
                    tokens.hover_bg.with_alpha(0.0),
                    tokens.hover_bg,
                    tokens.pressed_bg,
                )
            } else {
                tokens.hover_bg.with_alpha(0.0)
            };
            canvas.fill_rounded_rect(rect, radii, fill);
            canvas.stroke_rounded_rect(
                rect.inset(Insets::uniform(0.5)),
                radii,
                if self.enabled {
                    tokens.text_muted
                } else {
                    tokens.text_disabled
                },
                1.0,
            );
        }
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
                rect.right() + LABEL_GAP,
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
                    self.cycle(cx);
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
                    self.cycle(cx);
                }
                EventResult::handled()
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
        let mut state = control_state(cx, self.enabled);
        state.toggle = Some(self.state);
        let mut node = AccessNode::new(self.id, Role::CheckBox, self.label.clone(), self.bounds)
            .with_state(state);
        node.patterns.toggle = true;
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::Toggle if self.enabled => {
                self.cycle(cx);
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

    fn setup(state: ToggleState, tri: bool) -> (Harness, CheckBox, Rect) {
        let mut harness = Harness::new();
        let mut check = CheckBox::new("Done", state).with_motion(Motion::reduced());
        if tri {
            check = check.tri_state();
        }
        let bounds = harness.place(&mut check, Point::new(5.0, 5.0));
        (harness, check, bounds)
    }

    #[test]
    fn kit_checkbox_two_state_cycle() {
        let (mut harness, mut check, bounds) = setup(ToggleState::Off, false);
        assert_eq!(bounds.size(), Size::new(20.0 + 8.0 + 32.0, 32.0));
        for (expected, label) in [(ToggleState::On, "check"), (ToggleState::Off, "uncheck")] {
            assert!(
                harness.click(&mut check, bounds.center()).handled,
                "{label}"
            );
            assert_eq!(check.state(), expected, "{label}");
            assert_eq!(check.take_changed(), Some(expected), "{label}");
        }
        assert_eq!(check.take_changed(), None);
        // An externally set indeterminate state collapses to unchecked in a two-state box.
        check.set_state(ToggleState::Indeterminate);
        harness.focus_widget(&mut check);
        harness.key(&mut check, vk::SPACE);
        assert_eq!(check.state(), ToggleState::Off);
        assert_eq!(check.label(), "Done");
        assert_eq!(check.bounds(), bounds);
    }

    #[test]
    fn kit_checkbox_tri_state_cycle_via_keyboard_and_uia() {
        let (mut harness, mut check, _) = setup(ToggleState::Off, true);
        assert!(!harness.key(&mut check, vk::SPACE).handled);
        harness.focus_widget(&mut check);
        let mut seen = Vec::new();
        for _ in 0..4 {
            assert!(harness.key(&mut check, vk::SPACE).handled);
            seen.push(check.take_changed());
        }
        assert_eq!(
            seen,
            vec![
                Some(ToggleState::On),
                Some(ToggleState::Indeterminate),
                Some(ToggleState::Off),
                Some(ToggleState::On)
            ]
        );
        assert!(harness.perform(&mut check, AccessAction::Toggle));
        assert_eq!(
            harness.node(&check).state.toggle,
            Some(ToggleState::Indeterminate)
        );
        assert!(!harness.key(&mut check, vk::RETURN).handled);
        let repeat = crate::input::KeyEvent {
            repeat: true,
            ..crate::input::KeyEvent::press(vk::SPACE, crate::input::Modifiers::NONE)
        };
        check.take_changed();
        assert!(harness.send(&mut check, Event::Key(repeat)).handled);
        assert_eq!(check.take_changed(), None);
    }

    #[test]
    fn kit_checkbox_access_node() {
        let (mut harness, mut check, bounds) = setup(ToggleState::On, false);
        let node = harness.node(&check);
        assert_eq!(node.role, Role::CheckBox);
        assert_eq!(node.name, "Done");
        assert_eq!(node.bounds, bounds);
        assert_eq!(node.state.toggle, Some(ToggleState::On));
        assert!(node.patterns.toggle && !node.patterns.invoke);
        assert!(harness.perform(&mut check, AccessAction::Focus));
        assert!(harness.node(&check).state.focused);
        assert!(!harness.perform(&mut check, AccessAction::Invoke));
        check.set_enabled(false);
        assert!(!check.is_enabled());
        assert!(!harness.perform(&mut check, AccessAction::Toggle));
        assert!(!harness.perform(&mut check, AccessAction::Focus));
        harness.arrange(&mut check, bounds);
        assert!(!harness.focus.can_focus(check.id()));
        assert!(!harness.click(&mut check, bounds.center()).handled);
        assert!(harness.node(&check).state.disabled);
    }

    #[test]
    fn kit_checkbox_paints_each_state() {
        for (state, glyph_expected) in [
            (ToggleState::Off, None),
            (ToggleState::On, Some(glyph::CHECK_MARK)),
            (ToggleState::Indeterminate, Some(glyph::REMOVE)),
        ] {
            let (mut harness, check, bounds) = setup(state, true);
            let canvas = harness.paint(&check);
            let tokens = harness.theme.tokens;
            let icon = canvas.ops().find_map(|op| match op {
                DrawOp::Icon { glyph, color, .. } => Some((*glyph, *color)),
                _ => None,
            });
            assert_eq!(icon.map(|(glyph, _)| glyph), glyph_expected);
            if let Some((_, color)) = icon {
                assert_eq!(color, tokens.accent_text_on);
            }
            // The mark is drawn after the box: look from the end.
            let fill = canvas
                .ops()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .find_map(|op| match op {
                    DrawOp::FillRoundedRect { rect, color, .. } => Some((*rect, *color)),
                    _ => None,
                });
            let (rect, color) = fill.expect("the box is filled");
            assert_eq!(rect, Rect::new(bounds.x, bounds.y + 6.0, 20.0, 20.0));
            if state == ToggleState::Off {
                assert_eq!(color.a, 0.0);
                assert!(canvas.ops().any(
                    |op| matches!(op, DrawOp::StrokeRoundedRect { width, .. } if *width == 1.0)
                ));
            } else {
                assert_eq!(color, tokens.accent);
            }
            assert_eq!(canvas.texts(), vec!["Done"]);
        }
    }

    #[test]
    fn kit_checkbox_hover_and_disabled_paint() {
        let (mut harness, mut check, bounds) = setup(ToggleState::Off, false);
        harness.hover(&mut check, bounds.center());
        let tokens = harness.theme.tokens;
        let canvas = harness.paint(&check);
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::FillRoundedRect { color, .. } if *color == tokens.hover_bg
        )));
        harness.hover(&mut check, Point::new(900.0, 900.0));
        // Checked and hovered: the accent fill turns slightly translucent.
        check.set_state(ToggleState::On);
        harness.hover(&mut check, bounds.center());
        let canvas = harness.paint(&check);
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::FillRoundedRect { color, .. } if (color.a - 0.9).abs() < 1e-6
        )));
        // Disabled boxes use the disabled colors, checked or not.
        check.set_enabled(false);
        let canvas = harness.paint(&check);
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::FillRoundedRect { color, .. } if *color == tokens.text_disabled
        )));
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::Text { color, .. } if *color == tokens.text_disabled
        )));
        check.set_state(ToggleState::Off);
        let canvas = harness.paint(&check);
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::StrokeRoundedRect { color, .. } if *color == tokens.text_disabled
        )));
    }

    #[test]
    fn kit_checkbox_focus_ring_labelless_and_animation() {
        let mut harness = Harness::new();
        let mut check = CheckBox::new("", ToggleState::Off);
        let bounds = harness.place(&mut check, Point::ZERO);
        assert_eq!(bounds.width, 20.0);
        harness.focus_widget(&mut check);
        let canvas = harness.paint(&check);
        assert!(canvas.texts().is_empty());
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::StrokeRoundedRect { width, .. } if *width == 2.0))
        );
        harness.hover(&mut check, bounds.center());
        harness.run_animations(&mut check);
        harness.requests.take_invalidation();
        harness.send(&mut check, Event::Focus(FocusEvent::Lost));
        assert!(!harness.requests.invalidation().is_clean());
        harness.requests.take_invalidation();
        harness.frame(&mut check, 16);
        assert!(harness.requests.invalidation().is_clean());
    }

    #[test]
    fn kit_checkbox_with_id_names_the_control_and_paints_only_where_damaged() {
        let id = WidgetId::allocate();
        let mut checkbox = CheckBox::new("Agree", ToggleState::Off).with_id(id);
        assert_eq!(checkbox.id(), id);
        let mut harness = crate::widgets::testkit::Harness::new();
        assert_eq!(harness.node(&checkbox).id, id);
        let bounds = harness.place(&mut checkbox, Point::new(10.0, 10.0));
        let away = Rect::new(500.0, 500.0, 5.0, 5.0);
        assert_eq!(
            harness.paint_damaged(&checkbox, Some(away)).ops().count(),
            0
        );
        assert!(harness.paint_damaged(&checkbox, Some(bounds)).ops().count() > 0);
    }
}
