//! A tooltip: appears 500 ms after the pointer settles on its target, hides when the pointer
//! leaves, a button is pressed or a key is typed.

use std::time::Duration;

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::Canvas;
use crate::geometry::{Point, Rect, Size};
use crate::input::{Event, PointerEventKind};
use crate::services::TimerId;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::{Control, FONT_SIZE, draw_left_centered, layout_line, paint_overlay};

/// How long the pointer must rest on the target before the tooltip shows.
pub const TOOLTIP_DELAY: Duration = Duration::from_millis(500);
/// Horizontal text padding.
const PADDING_X: f32 = 8.0;
/// Vertical text padding.
const PADDING_Y: f32 = 6.0;
/// Distance below the pointer where the tooltip appears.
const POINTER_OFFSET: f32 = 20.0;

/// A tooltip for a rectangular target. The owner paints it last (above everything).
#[derive(Debug, Clone)]
pub struct Tooltip {
    id: WidgetId,
    text: String,
    target: Rect,
    /// Where tooltips may appear (the window); `None` places them without clamping.
    area: Option<Rect>,
    timer: Option<TimerId>,
    pointer: Point,
    /// Bounds of the visible tooltip.
    shown: Option<Rect>,
}

impl Tooltip {
    /// A tooltip showing `text` over `target`.
    #[must_use]
    pub fn new(text: impl Into<String>, target: Rect) -> Self {
        Self {
            id: WidgetId::allocate(),
            text: text.into(),
            target,
            area: None,
            timer: None,
            pointer: Point::ZERO,
            shown: None,
        }
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    /// Keeps the tooltip inside `area` (the client area of the window).
    #[must_use]
    pub fn with_area(mut self, area: Rect) -> Self {
        self.area = Some(area);
        self
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }

    pub fn set_target(&mut self, target: Rect) {
        self.target = target;
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.shown.is_some()
    }

    /// Bounds of the tooltip while visible.
    #[must_use]
    pub fn bounds(&self) -> Option<Rect> {
        self.shown
    }

    fn hide(&mut self, cx: &mut EventCx) {
        if let Some(timer) = self.timer.take() {
            cx.kill_timer(timer);
        }
        if let Some(rect) = self.shown.take() {
            cx.invalidate(rect);
        }
    }

    fn show(&mut self, cx: &mut EventCx) {
        let text = layout_line(cx.text, cx.theme, &self.text, FONT_SIZE);
        let size = Size::new(
            text.size().width + 2.0 * PADDING_X,
            text.size().height + 2.0 * PADDING_Y,
        );
        let mut rect = Rect::from_origin_size(
            Point::new(self.pointer.x, self.pointer.y + POINTER_OFFSET),
            size,
        );
        if let Some(area) = self.area {
            if rect.right() > area.right() {
                rect.x = area.right() - rect.width;
            }
            if rect.bottom() > area.bottom() {
                // Flip above the pointer.
                rect.y = self.pointer.y - POINTER_OFFSET / 2.0 - rect.height;
            }
            rect.x = rect.x.max(area.x);
            rect.y = rect.y.max(area.y);
        }
        cx.invalidate(rect);
        self.shown = Some(rect);
    }
}

impl Control for Tooltip {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for Tooltip {
    fn measure(&mut self, _cx: &mut LayoutCx, _available: Size) -> Size {
        Size::ZERO
    }

    fn arrange(&mut self, _cx: &mut LayoutCx, _bounds: Rect) {}

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        let Some(rect) = self.shown else { return };
        if !cx.needs_paint(rect) {
            return;
        }
        let tokens = &cx.theme.tokens;
        paint_overlay(canvas, rect, tokens.tooltip_bg, cx.theme);
        let text = layout_line(cx.text, cx.theme, &self.text, FONT_SIZE);
        draw_left_centered(
            canvas,
            text.as_ref(),
            rect.x + PADDING_X,
            rect,
            tokens.tooltip_text,
        );
    }

    /// Never consumes events: the tooltip only watches them.
    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => {
                let inside = self.target.contains(event.position);
                match event.kind {
                    PointerEventKind::Move | PointerEventKind::Enter if inside => {
                        let moved = event.position != self.pointer;
                        self.pointer = event.position;
                        if self.shown.is_none() && (self.timer.is_none() || moved) {
                            // Every movement restarts the countdown: the pointer must rest.
                            let timer = *self.timer.get_or_insert_with(TimerId::allocate);
                            cx.set_timer(timer, TOOLTIP_DELAY, false);
                        }
                    }
                    PointerEventKind::Move | PointerEventKind::Enter => self.hide(cx),
                    PointerEventKind::Down | PointerEventKind::Leave => self.hide(cx),
                    PointerEventKind::Up => {}
                }
            }
            Event::Key(_) | Event::Text(_) | Event::Wheel(_) => self.hide(cx),
            Event::Timer(id) if self.timer == Some(*id) => {
                self.timer = None;
                self.show(cx);
            }
            _ => {}
        }
        EventResult::ignored()
    }

    fn access(&self, _cx: &AccessCx, out: &mut AccessTree) {
        let bounds = self.shown.unwrap_or(Rect::ZERO);
        let mut node = AccessNode::new(self.id, Role::ToolTip, self.text.clone(), bounds);
        node.state.offscreen = self.shown.is_none();
        out.insert(node);
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::input::PointerButton;
    use crate::widget::TimerRequest;
    use crate::widgets::testkit::Harness;

    const TARGET: Rect = Rect::new(100.0, 100.0, 40.0, 32.0);

    /// Swallows the timer requests the host would forward to the OS.
    struct Discard;

    impl crate::services::Timers for Discard {
        fn set(&mut self, _: TimerId, _: Duration, _: bool) {}
        fn kill(&mut self, _: TimerId) {}
        fn request_animation_frame(&mut self) {}
    }

    fn timer_of(harness: &Harness) -> Option<(TimerId, Duration)> {
        harness
            .requests
            .timers()
            .iter()
            .rev()
            .find_map(|request| match request {
                TimerRequest::Set { id, interval, .. } => Some((*id, *interval)),
                TimerRequest::Kill(_) => None,
            })
    }

    fn move_to(harness: &mut Harness, tip: &mut Tooltip, x: f32, y: f32) {
        harness.hover(tip, Point::new(x, y));
    }

    #[test]
    fn kit_tooltip_shows_after_the_delay_below_the_pointer() {
        let mut harness = Harness::new();
        let mut tip = Tooltip::new("Bold", TARGET);
        move_to(&mut harness, &mut tip, 110.0, 110.0);
        let (timer, interval) = timer_of(&harness).expect("timer set");
        assert_eq!(interval, Duration::from_millis(500));
        assert!(!tip.is_visible());
        assert!(
            !harness
                .send(&mut tip, Event::Timer(TimerId::allocate()))
                .handled
        );
        assert!(!tip.is_visible());
        assert!(!harness.send(&mut tip, Event::Timer(timer)).handled);
        // "Bold" is four glyphs of 8 DIPs plus 8 DIPs of padding each side; one 16.8 DIP line.
        let rect = tip.bounds().expect("visible");
        assert_eq!((rect.x, rect.y, rect.width), (110.0, 130.0, 48.0));
        assert!((rect.height - 28.8).abs() < 1e-3);
        assert_eq!(tip.text(), "Bold");
        let canvas = harness.paint(&tip);
        let tokens = harness.theme.tokens;
        assert_eq!(canvas.texts(), vec!["Bold"]);
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::FillRoundedRect { rect: r, color, .. } if *r == rect && *color == tokens.tooltip_bg)));
        assert!(
            canvas.ops().any(
                |op| matches!(op, DrawOp::Text { color, .. } if *color == tokens.tooltip_text)
            )
        );
        // The same timer firing again does nothing.
        harness.send(&mut tip, Event::Timer(timer));
        assert!(tip.is_visible());
    }

    #[test]
    fn kit_tooltip_hides_on_leave_press_key_and_exit_from_target() {
        let mut harness = Harness::new();
        let mut tip = Tooltip::new("Bold", TARGET);
        let show = |harness: &mut Harness, tip: &mut Tooltip| {
            move_to(harness, tip, 110.0, 110.0);
            let (timer, _) = timer_of(harness).expect("timer");
            harness.send(tip, Event::Timer(timer));
            assert!(tip.is_visible());
        };
        show(&mut harness, &mut tip);
        harness.pointer(&mut tip, PointerEventKind::Leave, None, Point::ZERO);
        assert!(!tip.is_visible());
        show(&mut harness, &mut tip);
        harness.pointer(
            &mut tip,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            Point::new(110.0, 110.0),
        );
        assert!(!tip.is_visible());
        show(&mut harness, &mut tip);
        harness.key(&mut tip, 0x41);
        assert!(!tip.is_visible());
        show(&mut harness, &mut tip);
        harness.text(&mut tip, "a");
        assert!(!tip.is_visible());
        show(&mut harness, &mut tip);
        move_to(&mut harness, &mut tip, 500.0, 500.0);
        assert!(!tip.is_visible());
        show(&mut harness, &mut tip);
        harness.send(
            &mut tip,
            Event::Wheel(crate::input::WheelEvent {
                delta: Point::new(0.0, 1.0),
                precise: false,
                position: Point::new(110.0, 110.0),
                mods: crate::input::Modifiers::NONE,
            }),
        );
        assert!(!tip.is_visible());
        // Releases and other events leave a visible tooltip alone.
        show(&mut harness, &mut tip);
        harness.pointer(
            &mut tip,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::ZERO,
        );
        harness.send(&mut tip, Event::Focus(crate::input::FocusEvent::Gained));
        assert!(tip.is_visible());
    }

    #[test]
    fn kit_tooltip_pointer_movement_restarts_the_countdown_and_leaving_cancels_it() {
        let mut harness = Harness::new();
        let mut tip = Tooltip::new("Bold", TARGET);
        move_to(&mut harness, &mut tip, 110.0, 110.0);
        let (first, _) = timer_of(&harness).expect("timer");
        harness.requests.flush_timers(&mut Discard);
        move_to(&mut harness, &mut tip, 112.0, 110.0);
        let (second, _) = timer_of(&harness).expect("timer restarted");
        assert_eq!(first, second, "one timer, restarted");
        // Staying still does not restart it.
        harness.requests.flush_timers(&mut Discard);
        move_to(&mut harness, &mut tip, 112.0, 110.0);
        assert!(timer_of(&harness).is_none());
        // Leaving cancels the countdown.
        harness.pointer(&mut tip, PointerEventKind::Leave, None, Point::ZERO);
        assert!(
            harness
                .requests
                .timers()
                .iter()
                .any(|r| matches!(r, TimerRequest::Kill(id) if *id == first))
        );
        harness.send(&mut tip, Event::Timer(first));
        assert!(!tip.is_visible());
    }

    #[test]
    fn kit_tooltip_stays_inside_the_window() {
        let mut harness = Harness::new();
        let area = Rect::new(0.0, 0.0, 200.0, 150.0);
        let mut tip =
            Tooltip::new("A long tooltip text", Rect::new(0.0, 0.0, 200.0, 150.0)).with_area(area);
        move_to(&mut harness, &mut tip, 190.0, 140.0);
        let (timer, _) = timer_of(&harness).expect("timer");
        harness.send(&mut tip, Event::Timer(timer));
        let rect = tip.bounds().expect("visible");
        assert_eq!(rect.right(), 200.0, "shifted left to fit");
        assert!(
            (rect.bottom() - 130.0).abs() < 1e-3,
            "flipped above the pointer"
        );
        // Near the top-left corner nothing is clamped past the origin.
        tip.set_target(Rect::new(-50.0, -50.0, 300.0, 300.0));
        tip.set_text("x");
        harness.pointer(&mut tip, PointerEventKind::Leave, None, Point::ZERO);
        move_to(&mut harness, &mut tip, -10.0, -30.0);
        let (timer, _) = timer_of(&harness).expect("timer");
        harness.send(&mut tip, Event::Timer(timer));
        let rect = tip.bounds().expect("visible");
        assert_eq!((rect.x, rect.y), (0.0, 0.0));
    }

    #[test]
    fn kit_tooltip_access_node_reflects_visibility() {
        let mut harness = Harness::new();
        let mut tip = Tooltip::new("Bold", TARGET);
        let hidden = harness.node(&tip);
        assert_eq!((hidden.role, hidden.name.as_str()), (Role::ToolTip, "Bold"));
        assert!(hidden.state.offscreen);
        move_to(&mut harness, &mut tip, 110.0, 110.0);
        let (timer, _) = timer_of(&harness).expect("timer");
        harness.send(&mut tip, Event::Timer(timer));
        let shown = harness.node(&tip);
        assert!(!shown.state.offscreen);
        assert_eq!(Some(shown.bounds), tip.bounds());
        assert!(!harness.perform(&mut tip, crate::access::AccessAction::Invoke));
        // Measure and arrange take no space; a hidden tooltip paints nothing.
        let placed = harness.place(&mut tip, Point::ZERO);
        assert_eq!(placed.size(), Size::ZERO);
        tip.set_text("Italic");
        assert_eq!(tip.text(), "Italic");
    }

    #[test]
    fn kit_tooltip_paint_respects_damage() {
        let mut harness = Harness::new();
        let mut tip = Tooltip::new("Bold", TARGET);
        assert!(harness.paint(&tip).recorded().is_empty());
        move_to(&mut harness, &mut tip, 110.0, 110.0);
        let (timer, _) = timer_of(&harness).expect("timer");
        harness.send(&mut tip, Event::Timer(timer));
        let mut canvas = crate::canvas::RecordingCanvas::new();
        let cx = PaintCx::new(
            tip.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &harness.focus,
            &harness.capture,
            Some(Rect::new(0.0, 0.0, 10.0, 10.0)),
        );
        tip.paint(&cx, &mut canvas);
        assert!(canvas.recorded().is_empty());
    }

    #[test]
    fn kit_tooltip_with_id_names_the_control() {
        let id = WidgetId::allocate();
        let tip = Tooltip::new("Hint", TARGET).with_id(id);
        assert_eq!(tip.id(), id);
        assert_eq!(Harness::new().node(&tip).id, id);
    }

    #[test]
    fn kit_tooltip_test_host_forwards_timer_requests() {
        let mut harness = Harness::new();
        let first = TimerId::allocate();
        harness
            .requests
            .set_timer(first, Duration::from_millis(500), false);
        harness.requests.kill_timer(TimerId::allocate());
        harness.requests.request_animation_frame();
        // The latest start request is found behind a later stop request.
        assert_eq!(
            timer_of(&harness),
            Some((first, Duration::from_millis(500)))
        );
        harness.requests.flush_timers(&mut Discard);
        assert!(harness.requests.timers().is_empty());
        assert!(!harness.requests.animation_frame_requested());
    }
}
