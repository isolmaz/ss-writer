//! The InfoBar: a severity banner with a message, action buttons and an optional close button.

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, IconFont};
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::Event;
use crate::text::FontWeight;
use crate::theme::CalloutKind;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::button::{Button, ButtonKind};
use super::{Control, FONT_SIZE, ICON_SIZE, glyph, ui_style};

/// Height of a one-line banner.
pub const BANNER_HEIGHT: f32 = 48.0;
/// Width a banner asks for when its parent does not constrain it.
const DEFAULT_WIDTH: f32 = 400.0;
/// Inset of the icon from the left edge.
const PADDING: f32 = 16.0;
/// Gap between neighbouring elements.
const GAP: f32 = 8.0;
/// Side of the close button.
const CLOSE_SIZE: f32 = 32.0;

/// How serious the message is; selects the icon and colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Success,
    Warning,
    Error,
}

impl Severity {
    fn glyph(self) -> char {
        match self {
            Severity::Info => glyph::INFO,
            Severity::Success => glyph::SUCCESS,
            Severity::Warning => glyph::WARNING,
            Severity::Error => glyph::ERROR,
        }
    }

    fn callout(self) -> CalloutKind {
        match self {
            Severity::Info => CalloutKind::Note,
            Severity::Success => CalloutKind::Tip,
            Severity::Warning => CalloutKind::Warning,
            Severity::Error => CalloutKind::Caution,
        }
    }
}

/// What the user did with the banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerEvent {
    /// The action button at this index was activated.
    Action(usize),
    /// The close button was activated; the banner hides itself.
    Closed,
}

/// A banner. Strings are passed in already localized.
#[derive(Debug, Clone)]
pub struct Banner {
    id: WidgetId,
    severity: Severity,
    title: String,
    message: String,
    actions: Vec<Button>,
    close: Option<Button>,
    open: bool,
    bounds: Rect,
    events: Vec<BannerEvent>,
}

impl Banner {
    #[must_use]
    pub fn new(severity: Severity, title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: WidgetId::allocate(),
            severity,
            title: title.into(),
            message: message.into(),
            actions: Vec::new(),
            close: None,
            open: true,
            bounds: Rect::ZERO,
            events: Vec::new(),
        }
    }

    /// Adds an action button labelled `label`.
    #[must_use]
    pub fn with_action(mut self, label: impl Into<String>) -> Self {
        self.actions.push(Button::new(ButtonKind::Standard, label));
        self
    }

    /// Adds the close button; `label` is its accessible name.
    #[must_use]
    pub fn closable(mut self, label: impl Into<String>) -> Self {
        self.close = Some(Button::icon(glyph::CLOSE, label));
        self
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    #[must_use]
    pub fn severity(&self) -> Severity {
        self.severity
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Shows or hides the banner (a hidden banner takes no space).
    pub fn set_open(&mut self, open: bool) {
        self.open = open;
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<BannerEvent> {
        std::mem::take(&mut self.events)
    }

    fn buttons(&self) -> impl Iterator<Item = &Button> {
        self.actions.iter().chain(self.close.iter())
    }

    fn collect(&mut self, cx: &mut EventCx) {
        for (index, action) in self.actions.iter_mut().enumerate() {
            if action.take_activated() {
                self.events.push(BannerEvent::Action(index));
            }
        }
        if self.close.as_mut().is_some_and(Button::take_activated) {
            self.events.push(BannerEvent::Closed);
            self.open = false;
            cx.invalidate(self.bounds);
        }
    }
}

impl Control for Banner {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for Banner {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        if !self.open {
            return Size::ZERO;
        }
        let width = if available.width.is_finite() {
            available.width
        } else {
            DEFAULT_WIDTH
        };
        Size::new(width, BANNER_HEIGHT)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        if !self.open {
            return;
        }
        let mut widths = Vec::with_capacity(self.actions.len());
        for action in &mut self.actions {
            let id = action.id();
            widths.push(
                action
                    .measure(&mut cx.for_widget(id), Size::UNBOUNDED)
                    .width,
            );
        }
        let close_width = if self.close.is_some() {
            CLOSE_SIZE + GAP
        } else {
            0.0
        };
        let total: f32 = widths.iter().map(|width| width + GAP).sum();
        let mut x = bounds.right() - GAP - close_width - total + GAP;
        for (action, width) in self.actions.iter_mut().zip(widths) {
            let id = action.id();
            let top = bounds.y + (bounds.height - 32.0) / 2.0;
            action.arrange(&mut cx.for_widget(id), Rect::new(x, top, width, 32.0));
            x += width + GAP;
        }
        if let Some(close) = self.close.as_mut() {
            let id = close.id();
            close.arrange(
                &mut cx.for_widget(id),
                Rect::new(
                    bounds.right() - GAP - CLOSE_SIZE,
                    bounds.y + (bounds.height - CLOSE_SIZE) / 2.0,
                    CLOSE_SIZE,
                    CLOSE_SIZE,
                ),
            );
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.open || !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        let style = tokens.callout.get(self.severity.callout());
        let radii = CornerRadii::uniform(tokens.metrics.control_radius);
        canvas.fill_rounded_rect(self.bounds, radii, style.background);
        canvas.stroke_rounded_rect(
            self.bounds.inset(Insets::uniform(0.5)),
            radii,
            tokens.hairline,
            1.0,
        );
        canvas.draw_icon(
            self.severity.glyph(),
            IconFont::SegoeFluent,
            ICON_SIZE,
            style.icon,
            Point::new(
                self.bounds.x + PADDING + ICON_SIZE / 2.0,
                self.bounds.center().y,
            ),
        );
        let title = cx.text.layout(
            &self.title,
            &ui_style(cx.theme, FONT_SIZE).with_weight(FontWeight::SEMIBOLD),
            &[],
            &[],
            f32::INFINITY,
        );
        let x = self.bounds.x + PADDING + ICON_SIZE + GAP + 4.0;
        super::draw_left_centered(canvas, title.as_ref(), x, self.bounds, tokens.text);
        if !self.message.is_empty() {
            let message = super::layout_line(cx.text, cx.theme, &self.message, FONT_SIZE);
            super::draw_left_centered(
                canvas,
                message.as_ref(),
                x + title.size().width + GAP,
                self.bounds,
                tokens.text_muted,
            );
        }
        for button in self.buttons() {
            button.paint(&cx.for_widget(button.id()), canvas);
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if !self.open {
            return EventResult::ignored();
        }
        let mut result = EventResult::ignored();
        for action in &mut self.actions {
            let id = action.id();
            result = result.or(action.event(&mut cx.for_widget(id), e));
        }
        if let Some(close) = self.close.as_mut() {
            let id = close.id();
            result = result.or(close.event(&mut cx.for_widget(id), e));
        }
        self.collect(cx);
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let mut children = Vec::new();
        if self.open {
            for button in self.buttons() {
                button.access(&cx.for_widget(button.id()), out);
                children.push(button.id());
            }
        }
        let bounds = if self.open { self.bounds } else { Rect::ZERO };
        let mut node = AccessNode::new(self.id, Role::Group, self.title.clone(), bounds)
            .with_description(self.message.clone())
            .with_children(children);
        node.state.offscreen = !self.open;
        out.insert(node);
    }

    /// Carries out `a` on the action or close button whose access node has id `cx.id`: Invoke
    /// has the effect of a click (the same [`BannerEvent`]), Focus moves keyboard focus to it.
    /// The banner's own node supports no action.
    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        if !self.open {
            return false;
        }
        let target = cx.id;
        let Some(button) = self
            .actions
            .iter_mut()
            .chain(self.close.as_mut())
            .find(|button| button.id() == target)
        else {
            return false;
        };
        let performed = button.perform(cx, a);
        self.collect(cx);
        performed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::input::{FocusEvent, PointerButton, PointerEventKind, vk};
    use crate::widgets::testkit::Harness;

    fn setup() -> (Harness, Banner, Rect) {
        let mut harness = Harness::new();
        let mut banner = Banner::new(Severity::Warning, "Disk full", "Free some space.")
            .with_action("Clean up")
            .with_action("Ignore")
            .closable("Close");
        let bounds = harness.place(&mut banner, Point::new(0.0, 0.0));
        harness.arrange(&mut banner, Rect::new(0.0, 0.0, 600.0, 48.0));
        (harness, banner, bounds)
    }

    #[test]
    fn kit_banner_lays_out_actions_and_close_from_the_right() {
        let (harness, banner, placed) = setup();
        assert_eq!(placed.size(), Size::new(400.0, 48.0));
        let tree = harness.tree(&banner);
        let buttons: Vec<_> = tree.children(banner.id()).collect();
        assert_eq!(buttons.len(), 3);
        // Close is a 32 DIP square 8 DIPs from the right edge; actions sit left of it.
        assert_eq!(buttons[2].bounds, Rect::new(560.0, 8.0, 32.0, 32.0));
        // "Ignore" is 6 glyphs: 48 + 22 wide, ending 8 DIPs before the close button.
        assert_eq!(
            buttons[1].bounds,
            Rect::new(560.0 - 8.0 - 70.0, 8.0, 70.0, 32.0)
        );
        assert_eq!(buttons[0].bounds.right() + 8.0, buttons[1].bounds.x);
        assert_eq!(harness.focus.tab_order().len(), 3);
        assert!(!harness.focus.tab_order().is_empty());
    }

    #[test]
    fn kit_banner_paints_severity_colors_icon_and_text() {
        let (mut harness, banner, _) = setup();
        let tokens = harness.theme.tokens;
        let style = tokens.callout.warning;
        let canvas = harness.paint(&banner);
        assert!(canvas.ops().any(
            |op| matches!(op, DrawOp::FillRoundedRect { rect, color, .. }
            if *rect == Rect::new(0.0, 0.0, 600.0, 48.0) && *color == style.background)
        ));
        assert!(canvas.ops().any(
            |op| matches!(op, DrawOp::Icon { glyph: g, color, center, .. }
            if *g == glyph::WARNING && *color == style.icon && center.x == 24.0 && center.y == 24.0)
        ));
        assert_eq!(
            canvas.texts(),
            vec!["Disk full", "Free some space.", "Clean up", "Ignore"]
        );
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::Icon { glyph: g, .. } if *g == glyph::CLOSE))
        );
        for (severity, expected) in [
            (Severity::Info, glyph::INFO),
            (Severity::Success, glyph::SUCCESS),
            (Severity::Error, glyph::ERROR),
        ] {
            let mut other = Banner::new(severity, "t", "");
            harness.arrange(&mut other, Rect::new(0.0, 0.0, 300.0, 48.0));
            let canvas = harness.paint(&other);
            assert!(
                canvas
                    .ops()
                    .any(|op| matches!(op, DrawOp::Icon { glyph: g, .. } if *g == expected)),
                "{severity:?}"
            );
            // An empty message draws only the title.
            assert_eq!(canvas.texts(), vec!["t"]);
            assert_eq!(other.severity(), severity);
        }
    }

    #[test]
    fn kit_banner_action_and_close_clicks_become_events() {
        let (mut harness, mut banner, _) = setup();
        let tree = harness.tree(&banner);
        let buttons: Vec<Point> = tree
            .children(banner.id())
            .map(|n| n.bounds.center())
            .collect();
        let result = harness.click(&mut banner, buttons[1]);
        assert!(result.handled);
        assert_eq!(banner.take_events(), vec![BannerEvent::Action(1)]);
        harness.click(&mut banner, buttons[0]);
        assert_eq!(banner.take_events(), vec![BannerEvent::Action(0)]);
        assert!(banner.is_open());
        harness.click(&mut banner, buttons[2]);
        assert_eq!(banner.take_events(), vec![BannerEvent::Closed]);
        assert!(!banner.is_open());
        // A closed banner takes no space, draws nothing and ignores input.
        assert!(harness.paint(&banner).recorded().is_empty());
        assert!(!harness.click(&mut banner, buttons[1]).handled);
        let size = harness.place(&mut banner, Point::ZERO).size();
        assert_eq!(size, Size::ZERO);
        let node = harness.node(&banner);
        assert!(node.state.offscreen && node.children.is_empty());
        banner.set_open(true);
        assert!(banner.is_open());
    }

    #[test]
    fn kit_banner_keyboard_activates_the_focused_button() {
        let (mut harness, mut banner, _) = setup();
        let first = harness.focus.tab_order()[0];
        harness.focus.focus(first);
        harness.send(&mut banner, Event::Focus(FocusEvent::Gained));
        assert!(harness.key(&mut banner, vk::RETURN).handled);
        assert_eq!(banner.take_events(), vec![BannerEvent::Action(0)]);
        let last = harness.focus.tab_order()[2];
        harness.focus.focus(last);
        harness.key(&mut banner, vk::SPACE);
        assert_eq!(banner.take_events(), vec![BannerEvent::Closed]);
    }

    #[test]
    fn kit_banner_access_node_describes_message_and_children() {
        let (mut harness, mut banner, bounds) = setup();
        let tree = harness.tree(&banner);
        let root = tree.root().expect("root");
        assert_eq!((root.role, root.name.as_str()), (Role::Group, "Disk full"));
        assert_eq!(root.description, "Free some space.");
        assert_eq!(root.children.len(), 3);
        let names: Vec<&str> = tree
            .children(banner.id())
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(names, vec!["Clean up", "Ignore", "Close"]);
        assert!(
            tree.children(banner.id())
                .all(|n| n.role == Role::Button && n.patterns.invoke)
        );
        assert!(!harness.perform(&mut banner, AccessAction::Invoke));
        assert_eq!(banner.title(), "Disk full");
        assert_eq!(banner.message(), "Free some space.");
        assert_eq!(bounds.height, 48.0);
        assert_eq!(banner.bounds().width, 600.0);
        // Pointer movement is forwarded without being consumed outside the buttons.
        assert!(
            !harness
                .pointer(
                    &mut banner,
                    PointerEventKind::Down,
                    Some(PointerButton::Left),
                    Point::new(5.0, 5.0)
                )
                .handled
        );
    }

    #[test]
    fn kit_banner_access_invoke_on_a_button_is_a_click() {
        let (mut harness, mut banner, _) = setup();
        let ids: Vec<WidgetId> = harness
            .tree(&banner)
            .children(banner.id())
            .map(|node| node.id)
            .collect();
        for index in [1, 0] {
            let mut cx = harness.event_cx(ids[index]);
            assert!(banner.perform(&mut cx, &AccessAction::Invoke));
            assert_eq!(banner.take_events(), vec![BannerEvent::Action(index)]);
        }
        assert!(banner.is_open());
        // Focus moves keyboard focus to the button without activating it.
        let mut cx = harness.event_cx(ids[1]);
        assert!(banner.perform(&mut cx, &AccessAction::Focus));
        assert!(harness.focus.is_focused(ids[1]));
        assert!(banner.take_events().is_empty());
        // Actions a button does not support, and nodes that are not buttons, are refused.
        let mut cx = harness.event_cx(ids[0]);
        assert!(!banner.perform(&mut cx, &AccessAction::Toggle));
        let mut cx = harness.event_cx(WidgetId::allocate());
        assert!(!banner.perform(&mut cx, &AccessAction::Invoke));
        assert!(banner.take_events().is_empty());
        // Invoke on the close button closes the banner like clicking it.
        let mut cx = harness.event_cx(ids[2]);
        assert!(banner.perform(&mut cx, &AccessAction::Invoke));
        assert_eq!(banner.take_events(), vec![BannerEvent::Closed]);
        assert!(!banner.is_open());
        // A closed banner has no buttons to invoke.
        let mut cx = harness.event_cx(ids[0]);
        assert!(!banner.perform(&mut cx, &AccessAction::Invoke));
        assert!(banner.take_events().is_empty());
    }

    #[test]
    fn kit_banner_without_buttons_has_no_children() {
        let mut harness = Harness::new();
        let mut plain = Banner::new(Severity::Info, "Saved", "All changes are saved.");
        harness.arrange(&mut plain, Rect::new(0.0, 0.0, 300.0, 48.0));
        assert!(harness.node(&plain).children.is_empty());
        assert!(harness.focus.tab_order().is_empty());
    }
}
