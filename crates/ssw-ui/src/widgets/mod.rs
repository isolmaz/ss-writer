//! Fluent controls: buttons, toggles, text boxes, lists, trees, tabs, menus, dialogs.
//!
//! Every widget implements [`Widget`](crate::widget::Widget) completely: measure/arrange,
//! paint on a [`Canvas`], input, an [`AccessNode`](crate::access::AccessNode) with the right
//! role, state and patterns, and `perform` for each [`AccessAction`](crate::access::AccessAction)
//! it supports. Strings are passed in already localized; widgets contain no UI text.
//!
//! Widgets report what happened to their owner through small event queues
//! (`take_events`/`take_activated`) that the owner drains after routing an event; nothing is
//! called back from inside a pass.
//!
//! Metrics follow Windows 11 Fluent: control corner radius 4, overlay radius 8, 14 px body text,
//! 16 px Segoe Fluent Icons glyphs, accent fill for the primary state, a 2 px focus ring with a
//! 1 px inner contrast stroke ([`focus_ring`]), and 83 ms / 167 ms eased hover / pressed
//! transitions ([`anim`]) that jump when reduced motion is requested.

pub mod anim;
pub mod banner;
pub mod button;
pub mod checkbox;
pub mod combo;
pub mod dialog;
pub mod focus_ring;
pub mod list;
pub mod menu;
pub mod scroll;
pub mod slider;
pub mod tabs;
pub mod textbox;
pub mod toggle;
pub mod tooltip;
pub mod tree;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use crate::access::AccessState;
use crate::canvas::Canvas;
use crate::geometry::{Color, CornerRadii, Insets, Point, Rect};
use crate::input::{PointerButton, PointerEvent, PointerEventKind};
use crate::services::{Clipboard, TimerId};
use crate::text::{TextLayout, TextLayoutEngine, TextStyle};
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, EventResult, Widget, WidgetId};

use anim::{Easing, HOVER_DURATION, Motion, PRESS_DURATION, Transition};

/// A clipboard shared between the widgets of one window.
pub type SharedClipboard = Rc<RefCell<dyn Clipboard>>;

/// A widget that knows its own id (the id of its access node and focus stop).
pub trait Control: Widget {
    fn id(&self) -> WidgetId;
}

#[cfg(test)]
pub(crate) mod testkit {
    //! Drives widgets with synthetic events, a [`RecordingCanvas`] and [`MonoTextEngine`].

    use super::*;
    use crate::access::{AccessAction, AccessNode, AccessTree};
    use crate::canvas::RecordingCanvas;
    use crate::geometry::Size;
    use crate::input::{Event, FocusEvent, KeyEvent, Modifiers, PointerEventKind, PointerType};
    use crate::text::MonoTextEngine;
    use crate::theme::{DEFAULT_ACCENT, Theme};
    use crate::widget::{AccessCx, FocusManager, LayoutCx, PaintCx, PointerCapture, Requests};

    pub(crate) struct Harness {
        pub theme: Theme,
        pub engine: MonoTextEngine,
        pub focus: FocusManager,
        pub capture: PointerCapture,
        pub requests: Requests,
        pub now: Duration,
    }

    impl Harness {
        pub fn new() -> Self {
            Self {
                theme: Theme::light(DEFAULT_ACCENT),
                engine: MonoTextEngine::default(),
                focus: FocusManager::new(),
                capture: PointerCapture::new(),
                requests: Requests::new(),
                now: Duration::from_secs(1),
            }
        }

        pub fn event_cx(&mut self, id: WidgetId) -> EventCx<'_> {
            EventCx::new(
                id,
                &self.theme,
                1.0,
                &self.engine,
                &mut self.focus,
                &mut self.capture,
                &mut self.requests,
            )
        }

        /// Measures with an unbounded size, then arranges at `origin` with the measured size,
        /// and commits the tab order.
        pub fn place(&mut self, widget: &mut dyn Control, origin: Point) -> Rect {
            let id = widget.id();
            let mut cx = LayoutCx::new(
                id,
                &self.theme,
                1.0,
                &self.engine,
                &mut self.focus,
                &mut self.requests,
            );
            let size = widget.measure(&mut cx, Size::UNBOUNDED);
            let bounds = Rect::from_origin_size(origin, size);
            self.focus.begin_tab_order();
            let mut cx = LayoutCx::new(
                id,
                &self.theme,
                1.0,
                &self.engine,
                &mut self.focus,
                &mut self.requests,
            );
            widget.arrange(&mut cx, bounds);
            self.focus.commit_tab_order();
            bounds
        }

        /// Arranges at exactly `bounds` and commits the tab order.
        pub fn arrange(&mut self, widget: &mut dyn Control, bounds: Rect) {
            let id = widget.id();
            self.focus.begin_tab_order();
            let mut cx = LayoutCx::new(
                id,
                &self.theme,
                1.0,
                &self.engine,
                &mut self.focus,
                &mut self.requests,
            );
            widget.arrange(&mut cx, bounds);
            self.focus.commit_tab_order();
        }

        pub fn send(&mut self, widget: &mut dyn Control, event: Event) -> EventResult {
            let id = widget.id();
            let mut cx = self.event_cx(id);
            widget.event(&mut cx, &event)
        }

        pub fn key(&mut self, widget: &mut dyn Control, vk: u16) -> EventResult {
            self.key_with(widget, vk, Modifiers::NONE)
        }

        pub fn key_with(
            &mut self,
            widget: &mut dyn Control,
            vk: u16,
            mods: Modifiers,
        ) -> EventResult {
            self.send(widget, Event::Key(KeyEvent::press(vk, mods)))
        }

        pub fn text(&mut self, widget: &mut dyn Control, text: &str) -> EventResult {
            self.send(
                widget,
                Event::Text(crate::input::TextInput(text.to_owned())),
            )
        }

        pub fn focus_widget(&mut self, widget: &mut dyn Control) {
            self.focus.focus(widget.id());
            self.send(widget, Event::Focus(FocusEvent::Gained));
        }

        pub fn pointer(
            &mut self,
            widget: &mut dyn Control,
            kind: PointerEventKind,
            button: Option<PointerButton>,
            at: Point,
        ) -> EventResult {
            self.send(
                widget,
                Event::Pointer(PointerEvent {
                    kind,
                    button,
                    position: at,
                    pointer_type: PointerType::Mouse,
                    click_count: 1,
                    mods: Modifiers::NONE,
                }),
            )
        }

        pub fn click(&mut self, widget: &mut dyn Control, at: Point) -> EventResult {
            let down = self.pointer(
                widget,
                PointerEventKind::Down,
                Some(PointerButton::Left),
                at,
            );
            let up = self.pointer(widget, PointerEventKind::Up, Some(PointerButton::Left), at);
            down.or(up)
        }

        pub fn hover(&mut self, widget: &mut dyn Control, at: Point) -> EventResult {
            self.pointer(widget, PointerEventKind::Move, None, at)
        }

        /// Advances the animation clock by `advance_ms` and delivers one animation frame.
        pub fn frame(&mut self, widget: &mut dyn Control, advance_ms: u64) {
            self.now += Duration::from_millis(advance_ms);
            let now = self.now;
            self.send(widget, Event::AnimationFrame(now));
        }

        /// Delivers 16 ms animation frames until none is requested; fails when the animation
        /// never settles.
        pub fn run_animations(&mut self, widget: &mut dyn Control) {
            for _ in 0..240 {
                if !self.requests.animation_frame_requested() {
                    return;
                }
                self.requests.flush_timers(&mut NoTimers);
                self.frame(widget, 16);
            }
            panic!("animation did not settle");
        }

        pub fn paint(&mut self, widget: &dyn Control) -> RecordingCanvas {
            let mut canvas = RecordingCanvas::new();
            let cx = PaintCx::new(
                widget.id(),
                &self.theme,
                1.0,
                &self.engine,
                &self.focus,
                &self.capture,
                None,
            );
            widget.paint(&cx, &mut canvas);
            assert!(canvas.is_balanced(), "unbalanced canvas stack");
            canvas
        }

        pub fn tree(&self, widget: &dyn Control) -> AccessTree {
            let mut tree = AccessTree::new(widget.id());
            let cx = AccessCx::new(widget.id(), &self.theme, 1.0, &self.engine, &self.focus);
            widget.access(&cx, &mut tree);
            tree
        }

        pub fn node(&self, widget: &dyn Control) -> AccessNode {
            let tree = self.tree(widget);
            tree.root()
                .cloned()
                .unwrap_or_else(|| panic!("widget inserted no root node"))
        }

        pub fn perform(&mut self, widget: &mut dyn Control, action: AccessAction) -> bool {
            let id = widget.id();
            let mut cx = self.event_cx(id);
            widget.perform(&mut cx, &action)
        }
    }

    struct NoTimers;

    impl crate::services::Timers for NoTimers {
        fn set(&mut self, _: crate::services::TimerId, _: Duration, _: bool) {}
        fn kill(&mut self, _: crate::services::TimerId) {}
        fn request_animation_frame(&mut self) {}
    }
}

/// Height of a standard control.
pub const CONTROL_HEIGHT: f32 = 32.0;
/// Horizontal padding inside a standard control.
pub const CONTROL_PADDING: f32 = 11.0;
/// Body text size of controls.
pub const FONT_SIZE: f32 = 14.0;
/// Em size of icon glyphs.
pub const ICON_SIZE: f32 = 16.0;

/// Segoe Fluent Icons code points used by the controls.
pub mod glyph {
    pub const CHEVRON_DOWN: char = '\u{E70D}';
    pub const CHEVRON_UP: char = '\u{E70E}';
    pub const CHEVRON_RIGHT: char = '\u{E76C}';
    pub const CHECK_MARK: char = '\u{E73E}';
    pub const CLOSE: char = '\u{E711}';
    pub const ADD: char = '\u{E710}';
    pub const INFO: char = '\u{E946}';
    pub const WARNING: char = '\u{E7BA}';
    pub const ERROR: char = '\u{EA39}';
    pub const SUCCESS: char = '\u{E930}';
    /// The indeterminate dash of a tri-state check box.
    pub const REMOVE: char = '\u{E738}';
}

/// The UI font at `size` DIPs.
pub(crate) fn ui_style(theme: &Theme, size: f32) -> TextStyle {
    TextStyle::new(theme.tokens.metrics.ui_font, size)
}

/// Lays out one unwrapped line of UI text.
pub(crate) fn layout_line(
    engine: &dyn TextLayoutEngine,
    theme: &Theme,
    text: &str,
    size: f32,
) -> Box<dyn TextLayout> {
    engine.layout(text, &ui_style(theme, size), &[], &[], f32::INFINITY)
}

/// What a pointer event did to an [`Interaction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerOutcome {
    /// The event is not about this control.
    Ignored,
    /// The state changed (hover, press) but nothing was activated.
    Handled,
    /// Pressed and released inside the control.
    Clicked,
}

/// Hover and press tracking with eased fractions, shared by the button-like controls.
#[derive(Debug, Clone, Copy)]
pub struct Interaction {
    hover: Transition,
    press: Transition,
    hovered: bool,
    pressed: bool,
}

impl Interaction {
    #[must_use]
    pub fn new(motion: Motion) -> Self {
        Self {
            hover: Transition::new(0.0, motion),
            press: Transition::new(0.0, motion),
            hovered: false,
            pressed: false,
        }
    }

    #[must_use]
    pub fn hovered(&self) -> bool {
        self.hovered
    }

    #[must_use]
    pub fn pressed(&self) -> bool {
        self.pressed
    }

    /// Eased hover fraction `0..=1`.
    #[must_use]
    pub fn hover_amount(&self) -> f32 {
        self.hover.value()
    }

    /// Eased pressed fraction `0..=1`.
    #[must_use]
    pub fn press_amount(&self) -> f32 {
        self.press.value()
    }

    pub fn set_motion(&mut self, motion: Motion) {
        self.hover.set_motion(motion);
        self.press.set_motion(motion);
    }

    /// Sets the hover state, returning whether it changed.
    pub fn set_hovered(&mut self, cx: &mut EventCx, hovered: bool) -> bool {
        if hovered == self.hovered {
            return false;
        }
        self.hovered = hovered;
        let target = if hovered { 1.0 } else { 0.0 };
        if self
            .hover
            .set_target(target, HOVER_DURATION, Easing::Standard)
        {
            cx.request_animation_frame();
        }
        true
    }

    /// Sets the pressed state, returning whether it changed.
    pub fn set_pressed(&mut self, cx: &mut EventCx, pressed: bool) -> bool {
        if pressed == self.pressed {
            return false;
        }
        self.pressed = pressed;
        let target = if pressed { 1.0 } else { 0.0 };
        if self
            .press
            .set_target(target, PRESS_DURATION, Easing::Decelerate)
        {
            cx.request_animation_frame();
        }
        true
    }

    /// Clears hover and press (the control became disabled or hidden).
    pub fn reset(&mut self, cx: &mut EventCx) {
        self.set_hovered(cx, false);
        self.set_pressed(cx, false);
        cx.release_pointer();
    }

    /// Advances the transitions to the frame time. Requests another frame while one is running.
    /// Returns whether anything changed on screen (a transition was running).
    pub fn animate(&mut self, cx: &mut EventCx, now: Duration) -> bool {
        let was_running = self.hover.is_running() || self.press.is_running();
        let hover = self.hover.tick(now);
        let press = self.press.tick(now);
        if hover || press {
            cx.request_animation_frame();
        }
        was_running
    }

    /// Updates hover/press from a pointer event in `bounds`. A left press captures the pointer;
    /// releasing it inside `bounds` is a click. Disabled controls ignore everything.
    pub fn pointer(
        &mut self,
        cx: &mut EventCx,
        bounds: Rect,
        event: &PointerEvent,
        enabled: bool,
    ) -> PointerOutcome {
        if !enabled {
            return PointerOutcome::Ignored;
        }
        let inside = bounds.contains(event.position);
        match event.kind {
            PointerEventKind::Down => {
                if event.button != Some(PointerButton::Left) || !inside {
                    return PointerOutcome::Ignored;
                }
                self.set_hovered(cx, true);
                self.set_pressed(cx, true);
                cx.capture_pointer();
                cx.invalidate(bounds);
                PointerOutcome::Handled
            }
            PointerEventKind::Up => {
                if event.button != Some(PointerButton::Left) || !self.pressed {
                    return PointerOutcome::Ignored;
                }
                self.set_pressed(cx, false);
                self.set_hovered(cx, inside);
                cx.release_pointer();
                cx.invalidate(bounds);
                if inside {
                    PointerOutcome::Clicked
                } else {
                    PointerOutcome::Handled
                }
            }
            PointerEventKind::Move | PointerEventKind::Enter => {
                if self.set_hovered(cx, inside) {
                    cx.invalidate(bounds);
                    PointerOutcome::Handled
                } else {
                    PointerOutcome::Ignored
                }
            }
            PointerEventKind::Leave => {
                if self.set_hovered(cx, false) {
                    cx.invalidate(bounds);
                    PointerOutcome::Handled
                } else {
                    PointerOutcome::Ignored
                }
            }
        }
    }

    /// `base` blended toward `hover` and `pressed` by the eased fractions. Colors are
    /// translucent overlays, so blending is plain interpolation of the premultiplied result.
    #[must_use]
    pub fn blend(&self, base: Color, hover: Color, pressed: Color) -> Color {
        let hovered = base.lerp(hover, self.hover.value());
        hovered.lerp(pressed, self.press.value())
    }
}

/// The result of routing a pointer outcome: clicks are handled and ask for focus.
pub(crate) fn outcome_result(outcome: PointerOutcome) -> EventResult {
    match outcome {
        PointerOutcome::Ignored => EventResult::ignored(),
        PointerOutcome::Handled => EventResult::handled(),
        PointerOutcome::Clicked => EventResult::handled_with_focus(),
    }
}

/// Handles `AccessAction::Focus`: moves focus to the widget and reports whether it has it now.
pub(crate) fn perform_focus(cx: &mut EventCx) -> bool {
    cx.focus_self();
    cx.is_focused()
}

/// The common state flags of a focusable control.
pub(crate) fn control_state(cx: &AccessCx, enabled: bool) -> AccessState {
    AccessState {
        focusable: cx.is_focusable(),
        focused: cx.is_focused(),
        disabled: !enabled,
        ..AccessState::default()
    }
}

/// Draws `layout` centered on `rect`.
pub(crate) fn draw_centered(
    canvas: &mut dyn Canvas,
    layout: &dyn TextLayout,
    rect: Rect,
    color: Color,
) {
    let size = layout.size();
    let origin = Point::new(
        rect.x + (rect.width - size.width) / 2.0,
        rect.y + (rect.height - size.height) / 2.0,
    );
    canvas.draw_text_layout(layout, origin, color);
}

/// Draws `layout` left-aligned at `x`, vertically centered on `rect`.
pub(crate) fn draw_left_centered(
    canvas: &mut dyn Canvas,
    layout: &dyn TextLayout,
    x: f32,
    rect: Rect,
    color: Color,
) {
    let size = layout.size();
    canvas.draw_text_layout(
        layout,
        Point::new(x, rect.y + (rect.height - size.height) / 2.0),
        color,
    );
}

/// Paints a flyout surface (menu, tooltip, dialog): a soft shadow, the fill and a hairline.
pub(crate) fn paint_overlay(canvas: &mut dyn Canvas, rect: Rect, fill: Color, theme: &Theme) {
    let tokens = &theme.tokens;
    let radii = CornerRadii::uniform(tokens.metrics.overlay_radius);
    canvas.fill_rounded_rect(
        rect.translate(0.0, 2.0).outset(Insets::uniform(1.0)),
        radii,
        Color::BLACK.with_alpha(0.1),
    );
    canvas.fill_rounded_rect(rect, radii, fill);
    canvas.stroke_rounded_rect(
        rect.inset(Insets::uniform(0.5)),
        radii,
        tokens.hairline,
        1.0,
    );
}

/// How long typed characters keep extending a type-ahead search.
pub(crate) const TYPE_AHEAD_RESET: Duration = Duration::from_millis(1000);

/// Type-to-select: the characters typed in quick succession form a prefix that selects the
/// first matching item. Typing one character repeatedly cycles through the items starting with
/// it. A timer clears the buffer after [`TYPE_AHEAD_RESET`].
#[derive(Debug, Clone)]
pub(crate) struct TypeAhead {
    buffer: String,
    timer: TimerId,
}

impl TypeAhead {
    pub(crate) fn new() -> Self {
        Self {
            buffer: String::new(),
            timer: TimerId::allocate(),
        }
    }

    #[cfg(test)]
    pub(crate) fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Appends typed `text` (control characters and a leading blank are ignored) and restarts
    /// the reset timer. Returns whether the buffer changed.
    pub(crate) fn push(&mut self, cx: &mut EventCx, text: &str) -> bool {
        let typed: String = text.chars().filter(|c| !c.is_control()).collect();
        if typed.is_empty() || (self.buffer.is_empty() && typed.trim().is_empty()) {
            return false;
        }
        self.buffer.push_str(&typed);
        cx.set_timer(self.timer, TYPE_AHEAD_RESET, false);
        true
    }

    /// Clears the buffer when `id` is the reset timer. Returns whether it was.
    pub(crate) fn on_timer(&mut self, id: TimerId) -> bool {
        if id == self.timer {
            self.buffer.clear();
            true
        } else {
            false
        }
    }

    /// The first of `count` items after `current` (or at it, for a multi-character prefix)
    /// accepted by `matches(index, lowercase_prefix)`, wrapping around.
    pub(crate) fn find(
        &self,
        count: usize,
        current: Option<usize>,
        matches: impl Fn(usize, &str) -> bool,
    ) -> Option<usize> {
        if count == 0 || self.buffer.is_empty() {
            return None;
        }
        let lower = self.buffer.to_lowercase();
        let first = lower.chars().next()?;
        let repeated = lower.chars().all(|c| c == first);
        let (needle, start) = if repeated {
            (first.to_string(), current.map_or(0, |index| index + 1))
        } else {
            (lower, current.unwrap_or(0))
        };
        (0..count)
            .map(|step| (start + step) % count)
            .find(|&index| matches(index, &needle))
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::Harness;
    use super::*;
    use crate::input::PointerType;

    #[test]
    fn kit_type_ahead_prefix_cycle_and_timeout() {
        let mut harness = Harness::new();
        let id = crate::widget::WidgetId::allocate();
        let names = ["Apple", "avocado", "Banana", "Blueberry", "Cherry"];
        let matches = |index: usize, needle: &str| names[index].to_lowercase().starts_with(needle);
        let mut ahead = TypeAhead::new();
        let mut cx = harness.event_cx(id);
        // A leading blank and control characters are not part of a search.
        assert!(!ahead.push(&mut cx, " "));
        assert!(!ahead.push(&mut cx, "\u{8}"));
        assert!(ahead.buffer().is_empty());
        assert_eq!(ahead.find(5, None, matches), None);
        assert!(ahead.push(&mut cx, "b"));
        assert_eq!(ahead.find(5, None, matches), Some(2));
        // The same single letter again cycles to the next match.
        assert!(ahead.push(&mut cx, "b"));
        assert_eq!(ahead.find(5, Some(2), matches), Some(3));
        assert_eq!(ahead.find(5, Some(3), matches), Some(2));
        // A longer prefix is searched from the current item.
        ahead.on_timer(ahead.timer);
        assert!(ahead.push(&mut cx, "bl"));
        assert_eq!(ahead.find(5, Some(2), matches), Some(3));
        assert!(ahead.push(&mut cx, "x"));
        assert_eq!(ahead.find(5, Some(3), matches), None);
        assert_eq!(ahead.find(0, None, matches), None);
        // The timer resets the buffer; foreign timers do nothing.
        assert!(!ahead.on_timer(TimerId::allocate()));
        assert_eq!(ahead.buffer(), "blx");
        assert!(ahead.on_timer(ahead.timer));
        assert!(ahead.buffer().is_empty());
        assert!(harness
            .requests
            .timers()
            .iter()
            .any(|request| matches!(request, crate::widget::TimerRequest::Set { interval, .. } if *interval == TYPE_AHEAD_RESET)));
    }

    #[test]
    fn kit_overlay_paints_shadow_fill_and_hairline() {
        let harness = Harness::new();
        let mut canvas = crate::canvas::RecordingCanvas::new();
        let rect = Rect::new(10.0, 10.0, 100.0, 50.0);
        paint_overlay(
            &mut canvas,
            rect,
            harness.theme.tokens.menu_bg,
            &harness.theme,
        );
        let ops: Vec<_> = canvas.ops().cloned().collect();
        assert_eq!(ops.len(), 3);
        assert!(
            matches!(&ops[0], crate::canvas::DrawOp::FillRoundedRect { rect: shadow, .. } if shadow.y > rect.y - 1.0 && shadow.width == 102.0)
        );
        assert!(
            matches!(&ops[1], crate::canvas::DrawOp::FillRoundedRect { rect: body, radii, color }
            if *body == rect && *radii == CornerRadii::uniform(8.0) && *color == harness.theme.tokens.menu_bg)
        );
        assert!(
            matches!(&ops[2], crate::canvas::DrawOp::StrokeRoundedRect { color, width, .. }
            if *color == harness.theme.tokens.hairline && *width == 1.0)
        );
    }

    fn event(
        kind: PointerEventKind,
        button: Option<PointerButton>,
        x: f32,
        y: f32,
    ) -> PointerEvent {
        PointerEvent {
            kind,
            button,
            position: Point::new(x, y),
            pointer_type: PointerType::Mouse,
            click_count: 1,
            mods: crate::input::Modifiers::NONE,
        }
    }

    const BOUNDS: Rect = Rect::new(0.0, 0.0, 100.0, 32.0);

    #[test]
    fn kit_interaction_click_cycle() {
        let mut harness = Harness::new();
        let mut interaction = Interaction::new(Motion::reduced());
        let id = crate::widget::WidgetId::allocate();
        let mut cx = harness.event_cx(id);
        let down = event(
            PointerEventKind::Down,
            Some(PointerButton::Left),
            10.0,
            10.0,
        );
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &down, true),
            PointerOutcome::Handled
        );
        assert!(interaction.pressed() && interaction.hovered());
        assert_eq!(interaction.press_amount(), 1.0);
        assert!(cx.has_pointer_capture());
        let up = event(PointerEventKind::Up, Some(PointerButton::Left), 10.0, 10.0);
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &up, true),
            PointerOutcome::Clicked
        );
        assert!(!interaction.pressed());
        assert!(!cx.has_pointer_capture());
        assert_eq!(interaction.press_amount(), 0.0);
        // A second release without a press is ignored.
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &up, true),
            PointerOutcome::Ignored
        );
    }

    #[test]
    fn kit_interaction_release_outside_is_not_a_click() {
        let mut harness = Harness::new();
        let mut interaction = Interaction::new(Motion::reduced());
        let id = crate::widget::WidgetId::allocate();
        let mut cx = harness.event_cx(id);
        interaction.pointer(
            &mut cx,
            BOUNDS,
            &event(
                PointerEventKind::Down,
                Some(PointerButton::Left),
                10.0,
                10.0,
            ),
            true,
        );
        let outcome = interaction.pointer(
            &mut cx,
            BOUNDS,
            &event(PointerEventKind::Up, Some(PointerButton::Left), 500.0, 10.0),
            true,
        );
        assert_eq!(outcome, PointerOutcome::Handled);
        assert!(!interaction.hovered());
    }

    #[test]
    fn kit_interaction_hover_and_ignored_events() {
        let mut harness = Harness::new();
        let mut interaction = Interaction::new(Motion::reduced());
        let id = crate::widget::WidgetId::allocate();
        let mut cx = harness.event_cx(id);
        let enter = event(PointerEventKind::Enter, None, 5.0, 5.0);
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &enter, true),
            PointerOutcome::Handled
        );
        assert!(interaction.hovered());
        assert_eq!(interaction.hover_amount(), 1.0);
        // Moving inside does not change anything.
        let moved = event(PointerEventKind::Move, None, 6.0, 6.0);
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &moved, true),
            PointerOutcome::Ignored
        );
        // Moving out clears hover.
        let away = event(PointerEventKind::Move, None, 600.0, 6.0);
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &away, true),
            PointerOutcome::Handled
        );
        assert!(!interaction.hovered());
        // Leave while not hovered is ignored; leave while hovered clears.
        let leave = event(PointerEventKind::Leave, None, 0.0, 0.0);
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &leave, true),
            PointerOutcome::Ignored
        );
        interaction.pointer(&mut cx, BOUNDS, &enter, true);
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &leave, true),
            PointerOutcome::Handled
        );
        assert!(!interaction.hovered());
        // Right button presses, presses outside, and disabled controls are ignored.
        let right = event(PointerEventKind::Down, Some(PointerButton::Right), 5.0, 5.0);
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &right, true),
            PointerOutcome::Ignored
        );
        let outside = event(
            PointerEventKind::Down,
            Some(PointerButton::Left),
            500.0,
            5.0,
        );
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &outside, true),
            PointerOutcome::Ignored
        );
        let left = event(PointerEventKind::Down, Some(PointerButton::Left), 5.0, 5.0);
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &left, false),
            PointerOutcome::Ignored
        );
        let right_up = event(PointerEventKind::Up, Some(PointerButton::Right), 5.0, 5.0);
        assert_eq!(
            interaction.pointer(&mut cx, BOUNDS, &right_up, true),
            PointerOutcome::Ignored
        );
    }

    #[test]
    fn kit_interaction_animates_and_blends() {
        let mut harness = Harness::new();
        let mut interaction = Interaction::new(Motion::full());
        let id = crate::widget::WidgetId::allocate();
        {
            let mut cx = harness.event_cx(id);
            interaction.set_hovered(&mut cx, true);
            // The first frame only starts the clock: a transition was running and the eased
            // progress at 0 is (numerically) 0, not exactly 0.
            assert!(interaction.animate(&mut cx, Duration::ZERO));
        }
        assert!(harness.requests.animation_frame_requested());
        assert!(interaction.hover_amount() < 1e-3);
        let base = Color::BLACK;
        let hover = Color::WHITE;
        let pressed = Color::new(1.0, 0.0, 0.0, 1.0);
        assert!(interaction.blend(base, hover, pressed).r < 1e-3);
        {
            let mut cx = harness.event_cx(id);
            interaction.animate(&mut cx, Duration::from_millis(500));
        }
        assert_eq!(interaction.hover_amount(), 1.0);
        assert_eq!(interaction.blend(base, hover, pressed), hover);
        {
            let mut cx = harness.event_cx(id);
            interaction.reset(&mut cx);
            interaction.set_motion(Motion::reduced());
            interaction.set_pressed(&mut cx, true);
        }
        assert_eq!(interaction.press_amount(), 1.0);
        assert_eq!(interaction.blend(base, hover, pressed), pressed);
    }

    #[test]
    fn kit_outcome_results() {
        assert_eq!(
            outcome_result(PointerOutcome::Ignored),
            EventResult::ignored()
        );
        assert_eq!(
            outcome_result(PointerOutcome::Handled),
            EventResult::handled()
        );
        assert_eq!(
            outcome_result(PointerOutcome::Clicked),
            EventResult::handled_with_focus()
        );
    }
}
