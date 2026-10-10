//! Push buttons: standard, accent, subtle and icon.

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, IconFont};
use crate::geometry::{Color, CornerRadii, Insets, Point, Rect, Size};
use crate::input::{Event, FocusEvent, vk};
use crate::theme::ThemeTokens;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::anim::Motion;
use super::{
    CONTROL_HEIGHT, CONTROL_PADDING, Control, FONT_SIZE, ICON_SIZE, Interaction, PointerOutcome,
    control_state, draw_centered, focus_ring, layout_line, outcome_result, perform_focus,
};

/// Smallest width of a button.
const MIN_WIDTH: f32 = 32.0;
/// Gap between the icon and the label.
const ICON_GAP: f32 = 8.0;

/// The visual style of a [`Button`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Neutral fill with a hairline border.
    Standard,
    /// Accent fill, for the primary action.
    Accent,
    /// No fill until hovered.
    Subtle,
    /// A square subtle button showing only a glyph; the label is the accessible name.
    Icon,
}

/// A push button.
#[derive(Debug, Clone)]
pub struct Button {
    id: WidgetId,
    kind: ButtonKind,
    label: String,
    icon: Option<char>,
    enabled: bool,
    bounds: Rect,
    interaction: Interaction,
    activated: u32,
}

impl Button {
    /// A text button.
    #[must_use]
    pub fn new(kind: ButtonKind, label: impl Into<String>) -> Self {
        Self {
            id: WidgetId::allocate(),
            kind,
            label: label.into(),
            icon: None,
            enabled: true,
            bounds: Rect::ZERO,
            interaction: Interaction::new(Motion::full()),
            activated: 0,
        }
    }

    /// An icon button; `name` is the accessible name (and the tooltip text).
    #[must_use]
    pub fn icon(glyph: char, name: impl Into<String>) -> Self {
        Self {
            icon: Some(glyph),
            ..Self::new(ButtonKind::Icon, name)
        }
    }

    /// Shows `glyph` before the label.
    #[must_use]
    pub fn with_icon(mut self, glyph: char) -> Self {
        self.icon = Some(glyph);
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
    pub fn kind(&self) -> ButtonKind {
        self.kind
    }

    /// Changes the visual style (a dialog moves the accent to its default button).
    pub fn set_kind(&mut self, kind: ButtonKind) {
        self.kind = kind;
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
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
    pub fn is_hovered(&self) -> bool {
        self.interaction.hovered()
    }

    #[must_use]
    pub fn is_pressed(&self) -> bool {
        self.interaction.pressed()
    }

    /// True once per activation (click, Space, Enter, UIA Invoke) since the last call.
    pub fn take_activated(&mut self) -> bool {
        std::mem::take(&mut self.activated) > 0
    }

    fn shows_label(&self) -> bool {
        self.kind != ButtonKind::Icon
    }

    /// Base, hover and pressed fills.
    fn fills(&self, tokens: &ThemeTokens) -> (Color, Color, Color) {
        match self.kind {
            ButtonKind::Standard => (
                tokens.surface,
                tokens.hover_bg.over(tokens.surface),
                tokens.pressed_bg.over(tokens.surface),
            ),
            ButtonKind::Accent => (
                tokens.accent,
                tokens.accent.with_alpha(0.9).over(tokens.surface),
                tokens.accent.with_alpha(0.8).over(tokens.surface),
            ),
            ButtonKind::Subtle | ButtonKind::Icon => (
                tokens.hover_bg.with_alpha(0.0),
                tokens.hover_bg,
                tokens.pressed_bg,
            ),
        }
    }

    fn text_color(&self, tokens: &ThemeTokens) -> Color {
        if !self.enabled {
            tokens.text_disabled
        } else if self.kind == ButtonKind::Accent {
            tokens.accent_text_on
        } else if self.interaction.pressed() || self.interaction.hover_amount() > 0.5 {
            tokens.hover_text
        } else {
            tokens.text
        }
    }

    fn activate(&mut self) {
        self.activated += 1;
    }
}

impl Control for Button {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for Button {
    fn measure(&mut self, cx: &mut LayoutCx, _available: Size) -> Size {
        if !self.shows_label() {
            return Size::new(CONTROL_HEIGHT, CONTROL_HEIGHT);
        }
        let text = layout_line(cx.text, cx.theme, &self.label, FONT_SIZE);
        let icon = if self.icon.is_some() {
            ICON_SIZE + ICON_GAP
        } else {
            0.0
        };
        let width = (text.size().width + icon + 2.0 * CONTROL_PADDING).max(MIN_WIDTH);
        Size::new(width, CONTROL_HEIGHT)
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
        let radii = CornerRadii::uniform(tokens.metrics.control_radius);
        let (base, hover, pressed) = self.fills(tokens);
        let fill = if self.enabled {
            self.interaction.blend(base, hover, pressed)
        } else if self.kind == ButtonKind::Accent {
            tokens.hairline
        } else {
            base
        };
        canvas.fill_rounded_rect(self.bounds, radii, fill);
        if self.kind == ButtonKind::Standard {
            canvas.stroke_rounded_rect(
                self.bounds.inset(Insets::uniform(0.5)),
                radii,
                tokens.hairline,
                1.0,
            );
        }
        let color = self.text_color(tokens);
        match (self.icon, self.shows_label()) {
            (Some(glyph), false) => {
                canvas.draw_icon(
                    glyph,
                    IconFont::SegoeFluent,
                    ICON_SIZE,
                    color,
                    self.bounds.center(),
                );
            }
            (Some(glyph), true) => {
                let text = layout_line(cx.text, cx.theme, &self.label, FONT_SIZE);
                let total = ICON_SIZE + ICON_GAP + text.size().width;
                let left = self.bounds.x + (self.bounds.width - total) / 2.0;
                canvas.draw_icon(
                    glyph,
                    IconFont::SegoeFluent,
                    ICON_SIZE,
                    color,
                    Point::new(left + ICON_SIZE / 2.0, self.bounds.center().y),
                );
                let rest = Rect::new(
                    left + ICON_SIZE + ICON_GAP,
                    self.bounds.y,
                    text.size().width,
                    self.bounds.height,
                );
                draw_centered(canvas, text.as_ref(), rest, color);
            }
            (None, _) => {
                let text = layout_line(cx.text, cx.theme, &self.label, FONT_SIZE);
                draw_centered(canvas, text.as_ref(), self.bounds, color);
            }
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
                    self.activate();
                }
                outcome_result(outcome)
            }
            Event::Key(key)
                if key.is_down
                    && self.enabled
                    && cx.is_focused()
                    && matches!(key.vk, vk::SPACE | vk::RETURN)
                    && key.mods.is_empty() =>
            {
                if !key.repeat {
                    self.activate();
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
        let mut node = AccessNode::new(self.id, Role::Button, self.label.clone(), self.bounds)
            .with_state(control_state(cx, self.enabled));
        node.patterns.invoke = true;
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::Invoke if self.enabled => {
                self.activate();
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
    use crate::input::{KeyEvent, Modifiers, PointerButton, PointerEventKind};
    use crate::widgets::testkit::Harness;

    fn placed(kind: ButtonKind, label: &str) -> (Harness, Button, Rect) {
        let mut harness = Harness::new();
        let mut button = Button::new(kind, label).with_motion(Motion::reduced());
        let bounds = harness.place(&mut button, Point::new(20.0, 20.0));
        (harness, button, bounds)
    }

    #[test]
    fn kit_button_measures_text_and_padding() {
        let (_, _, bounds) = placed(ButtonKind::Standard, "Save");
        // 4 glyphs * 8 DIPs + 2 * 11 padding.
        assert_eq!(bounds.size(), Size::new(54.0, 32.0));
        let (_, _, short) = placed(ButtonKind::Standard, "x");
        assert_eq!(short.width, 32.0);
        let mut harness = Harness::new();
        let mut icon = Button::icon('\u{E713}', "Settings");
        assert_eq!(
            harness.place(&mut icon, Point::ZERO).size(),
            Size::new(32.0, 32.0)
        );
        let mut both = Button::new(ButtonKind::Subtle, "Add").with_icon('\u{E710}');
        assert_eq!(
            harness.place(&mut both, Point::ZERO).width,
            24.0 + 16.0 + 8.0 + 22.0
        );
    }

    #[test]
    fn kit_button_click_activates_once() {
        let (mut harness, mut button, bounds) = placed(ButtonKind::Accent, "OK");
        let at = bounds.center();
        let result = harness.click(&mut button, at);
        assert!(result.handled && result.request_focus);
        assert!(button.take_activated());
        assert!(!button.take_activated());
        // Releasing elsewhere cancels.
        harness.pointer(
            &mut button,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            at,
        );
        assert!(button.is_pressed());
        harness.pointer(
            &mut button,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::new(900.0, 900.0),
        );
        assert!(!button.take_activated());
    }

    #[test]
    fn kit_button_keyboard_requires_focus_and_ignores_repeat() {
        let (mut harness, mut button, _) = placed(ButtonKind::Standard, "Go");
        assert!(!harness.key(&mut button, vk::RETURN).handled);
        assert!(!button.take_activated());
        harness.focus_widget(&mut button);
        assert!(harness.key(&mut button, vk::SPACE).handled);
        assert!(button.take_activated());
        assert!(harness.key(&mut button, vk::RETURN).handled);
        assert!(button.take_activated());
        // Auto-repeat is consumed without re-activating.
        let repeat = KeyEvent {
            repeat: true,
            ..KeyEvent::press(vk::SPACE, Modifiers::NONE)
        };
        assert!(harness.send(&mut button, Event::Key(repeat)).handled);
        assert!(!button.take_activated());
        // Other keys and modified keys are ignored.
        assert!(!harness.key(&mut button, vk::TAB).handled);
        assert!(
            !harness
                .key_with(&mut button, vk::SPACE, Modifiers::CTRL)
                .handled
        );
        assert!(
            !harness
                .send(
                    &mut button,
                    Event::Text(crate::input::TextInput("a".into()))
                )
                .handled
        );
    }

    #[test]
    fn kit_button_disabled_ignores_input_and_leaves_tab_order() {
        let (mut harness, mut button, bounds) = placed(ButtonKind::Standard, "Go");
        assert!(harness.focus.can_focus(button.id()));
        button.set_enabled(false);
        harness.arrange(&mut button, bounds);
        assert!(!harness.focus.can_focus(button.id()));
        assert!(!harness.click(&mut button, bounds.center()).handled);
        assert!(!harness.perform(&mut button, AccessAction::Invoke));
        assert!(!harness.perform(&mut button, AccessAction::Focus));
        assert!(!button.take_activated());
        assert!(harness.node(&button).state.disabled);
        assert!(!button.is_enabled());
    }

    #[test]
    fn kit_button_access_node_and_perform() {
        let (mut harness, mut button, bounds) = placed(ButtonKind::Standard, "Save");
        let node = harness.node(&button);
        assert_eq!(node.role, Role::Button);
        assert_eq!(node.name, "Save");
        assert_eq!(node.bounds, bounds);
        assert!(node.patterns.invoke);
        assert!(node.state.focusable && !node.state.focused && !node.state.disabled);
        assert!(harness.perform(&mut button, AccessAction::Invoke));
        assert!(button.take_activated());
        assert!(harness.perform(&mut button, AccessAction::Focus));
        assert!(harness.node(&button).state.focused);
        assert!(!harness.perform(&mut button, AccessAction::Toggle));
        button.set_label("Save as");
        assert_eq!(harness.node(&button).name, "Save as");
        assert_eq!(button.label(), "Save as");
        assert_eq!(button.kind(), ButtonKind::Standard);
        assert_eq!(button.bounds(), bounds);
    }

    #[test]
    fn kit_button_paints_kind_specific_fills() {
        for (kind, accent_fill) in [
            (ButtonKind::Standard, false),
            (ButtonKind::Accent, true),
            (ButtonKind::Subtle, false),
        ] {
            let (mut harness, button, bounds) = placed(kind, "Run");
            let canvas = harness.paint(&button);
            let tokens = harness.theme.tokens;
            let fills: Vec<&DrawOp> = canvas
                .ops()
                .filter(|op| matches!(op, DrawOp::FillRoundedRect { .. }))
                .collect();
            assert_eq!(fills.len(), 1);
            assert!(
                matches!(fills[0], DrawOp::FillRoundedRect { rect, radii, color }
                    if *rect == bounds
                        && *radii == CornerRadii::uniform(4.0)
                        && (!accent_fill || *color == tokens.accent)
                        && (kind != ButtonKind::Subtle || color.a == 0.0))
            );
            let border = canvas
                .ops()
                .filter(|op| matches!(op, DrawOp::StrokeRoundedRect { .. }))
                .count();
            assert_eq!(border, usize::from(kind == ButtonKind::Standard));
            assert_eq!(canvas.texts(), vec!["Run"]);
        }
    }

    #[test]
    fn kit_button_hover_press_colors_and_text() {
        let (mut harness, mut button, bounds) = placed(ButtonKind::Subtle, "Run");
        let tokens = harness.theme.tokens;
        harness.hover(&mut button, bounds.center());
        assert!(button.is_hovered());
        let color_of = |harness: &mut Harness, button: &Button| {
            let canvas = harness.paint(button);
            // The label is drawn after the fill: look from the end.
            canvas
                .ops()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .find_map(|op| match op {
                    DrawOp::FillRoundedRect { color, .. } => Some(*color),
                    _ => None,
                })
                .expect("the button is filled")
        };
        assert_eq!(color_of(&mut harness, &button), tokens.hover_bg);
        let text_color = |harness: &mut Harness, button: &Button| {
            let canvas = harness.paint(button);
            canvas
                .ops()
                .find_map(|op| match op {
                    DrawOp::Text { color, .. } => Some(*color),
                    _ => None,
                })
                .expect("the button draws its label")
        };
        assert_eq!(text_color(&mut harness, &button), tokens.hover_text);
        harness.pointer(
            &mut button,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            bounds.center(),
        );
        let pressed = color_of(&mut harness, &button);
        for (got, want) in [
            (pressed.r, tokens.pressed_bg.r),
            (pressed.g, tokens.pressed_bg.g),
            (pressed.b, tokens.pressed_bg.b),
            (pressed.a, tokens.pressed_bg.a),
        ] {
            assert!((got - want).abs() < 1e-5);
        }
        harness.pointer(&mut button, PointerEventKind::Leave, None, Point::ZERO);
        harness.pointer(
            &mut button,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            bounds.center(),
        );
        harness.pointer(&mut button, PointerEventKind::Leave, None, Point::ZERO);
        assert!(!button.is_hovered());
        assert_eq!(text_color(&mut harness, &button), tokens.text);
    }

    #[test]
    fn kit_button_disabled_paint_uses_disabled_text() {
        let (mut harness, mut button, bounds) = placed(ButtonKind::Accent, "Run");
        button.set_enabled(false);
        let canvas = harness.paint(&button);
        let tokens = harness.theme.tokens;
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::FillRoundedRect { color, rect, .. } if *color == tokens.hairline && *rect == bounds
        )));
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::Text { color, .. } if *color == tokens.text_disabled
        )));
        let mut standard = Button::new(ButtonKind::Standard, "Run");
        standard.set_enabled(false);
        harness.arrange(&mut standard, bounds);
        let canvas = harness.paint(&standard);
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::FillRoundedRect { color, .. } if *color == tokens.surface
        )));
    }

    #[test]
    fn kit_button_icon_variants_paint_glyph() {
        let mut harness = Harness::new();
        let mut icon = Button::icon('\u{E713}', "Settings").with_motion(Motion::reduced());
        let bounds = harness.place(&mut icon, Point::new(4.0, 4.0));
        let canvas = harness.paint(&icon);
        assert!(canvas.texts().is_empty());
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::Icon { glyph: '\u{E713}', font: IconFont::SegoeFluent, center, size, .. }
                if *center == bounds.center() && *size == 16.0
        )));
        let mut both = Button::new(ButtonKind::Standard, "Add")
            .with_icon('\u{E710}')
            .with_motion(Motion::reduced());
        harness.place(&mut both, Point::ZERO);
        let canvas = harness.paint(&both);
        assert_eq!(canvas.texts(), vec!["Add"]);
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::Icon {
                glyph: '\u{E710}',
                ..
            }
        )));
    }

    #[test]
    fn kit_button_focus_ring_and_damage_clipping() {
        let (mut harness, mut button, bounds) = placed(ButtonKind::Standard, "Go");
        harness.focus_widget(&mut button);
        let canvas = harness.paint(&button);
        let strokes = canvas
            .ops()
            .filter(|op| matches!(op, DrawOp::StrokeRoundedRect { width, .. } if *width == 2.0))
            .count();
        assert_eq!(strokes, 1);
        // Painting outside the damage rectangle draws nothing.
        let mut canvas = crate::canvas::RecordingCanvas::new();
        let cx = PaintCx::new(
            button.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &harness.focus,
            &harness.capture,
            Some(Rect::new(bounds.right() + 100.0, 0.0, 10.0, 10.0)),
        );
        button.paint(&cx, &mut canvas);
        assert!(canvas.recorded().is_empty());
    }

    #[test]
    fn kit_button_animates_hover_with_full_motion() {
        let mut harness = Harness::new();
        let mut button = Button::new(ButtonKind::Subtle, "Go");
        let bounds = harness.place(&mut button, Point::ZERO);
        harness.hover(&mut button, bounds.center());
        assert!(harness.requests.animation_frame_requested());
        harness.run_animations(&mut button);
        let canvas = harness.paint(&button);
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::FillRoundedRect { color, .. } if *color == harness.theme.tokens.hover_bg
        )));
        assert!(!harness.requests.invalidation().is_clean());
        // Focus changes invalidate the ring area around the button.
        harness.requests.take_invalidation();
        harness.send(&mut button, Event::Focus(FocusEvent::Lost));
        assert!(!harness.requests.invalidation().is_clean());
    }

    #[test]
    fn kit_button_with_id_names_the_control() {
        let id = WidgetId::allocate();
        let button = Button::new(ButtonKind::Standard, "Run").with_id(id);
        assert_eq!(button.id(), id);
        assert_eq!(Harness::new().node(&button).id, id);
    }
}
