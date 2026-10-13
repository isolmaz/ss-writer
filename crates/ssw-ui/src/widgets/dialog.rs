//! The content dialog: a modal panel over a scrim with a title, a wrapped body and up to three
//! buttons. Esc closes, Enter activates the default button, Tab stays inside the dialog.

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::Canvas;
use crate::geometry::{Color, Point, Rect, Size};
use crate::input::{Event, KeyEvent, vk};
use crate::text::{FontWeight, TextLayout, TextLayoutEngine};
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::button::{Button, ButtonKind};
use super::{Control, FONT_SIZE, paint_overlay, ui_style};

/// Narrowest panel.
pub const MIN_WIDTH: f32 = 320.0;
/// Widest panel.
pub const MAX_WIDTH: f32 = 548.0;
/// Padding inside the panel.
const PADDING: f32 = 24.0;
/// Gap between the title and the body.
const TITLE_GAP: f32 = 12.0;
/// Gap between the buttons.
const BUTTON_GAP: f32 = 8.0;
/// Height of a dialog button.
const BUTTON_HEIGHT: f32 = 32.0;
/// Em size of the title.
const TITLE_SIZE: f32 = 20.0;
/// Opacity of the scrim behind the panel.
const SCRIM_OPACITY: f32 = 0.4;

/// A button of the dialog; the result of closing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogButton {
    Primary,
    Secondary,
    /// The close button; also the result of Esc.
    Close,
}

#[derive(Debug, Clone, Copy)]
struct Geometry {
    panel: Rect,
    title: Point,
    body: Point,
    buttons: Rect,
    body_width: f32,
}

/// A modal dialog. The host routes every event to an open dialog first and paints it last.
#[derive(Debug, Clone)]
pub struct ContentDialog {
    id: WidgetId,
    title: String,
    body: String,
    primary: Option<Button>,
    secondary: Option<Button>,
    close: Button,
    default: DialogButton,
    open: bool,
    bounds: Rect,
    result: Option<DialogButton>,
}

impl ContentDialog {
    /// A dialog with only a close button labelled `close`.
    #[must_use]
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        close: impl Into<String>,
    ) -> Self {
        let mut dialog = Self {
            id: WidgetId::allocate(),
            title: title.into(),
            body: body.into(),
            primary: None,
            secondary: None,
            close: Button::new(ButtonKind::Standard, close),
            default: DialogButton::Close,
            open: true,
            bounds: Rect::ZERO,
            result: None,
        };
        dialog.restyle();
        dialog
    }

    /// Adds the primary button; it becomes the default.
    #[must_use]
    pub fn with_primary(mut self, label: impl Into<String>) -> Self {
        self.primary = Some(Button::new(ButtonKind::Standard, label));
        self.default = DialogButton::Primary;
        self.restyle();
        self
    }

    #[must_use]
    pub fn with_secondary(mut self, label: impl Into<String>) -> Self {
        self.secondary = Some(Button::new(ButtonKind::Standard, label));
        self.restyle();
        self
    }

    /// Chooses the button Enter activates. A button the dialog does not have is ignored.
    #[must_use]
    pub fn with_default(mut self, default: DialogButton) -> Self {
        let exists = match default {
            DialogButton::Primary => self.primary.is_some(),
            DialogButton::Secondary => self.secondary.is_some(),
            DialogButton::Close => true,
        };
        if exists {
            self.default = default;
            self.restyle();
        }
        self
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    /// The accent goes to the default button, but only when the dialog has an action button.
    fn restyle(&mut self) {
        let accent = self.primary.is_some() || self.secondary.is_some();
        let default = self.default;
        for (which, button) in self.buttons_mut() {
            let kind = if accent && which == default {
                ButtonKind::Accent
            } else {
                ButtonKind::Standard
            };
            button.set_kind(kind);
        }
    }

    fn buttons_mut(&mut self) -> Vec<(DialogButton, &mut Button)> {
        let mut buttons = Vec::with_capacity(3);
        if let Some(primary) = self.primary.as_mut() {
            buttons.push((DialogButton::Primary, primary));
        }
        if let Some(secondary) = self.secondary.as_mut() {
            buttons.push((DialogButton::Secondary, secondary));
        }
        buttons.push((DialogButton::Close, &mut self.close));
        buttons
    }

    fn buttons(&self) -> Vec<(DialogButton, &Button)> {
        let mut buttons = Vec::with_capacity(3);
        if let Some(primary) = self.primary.as_ref() {
            buttons.push((DialogButton::Primary, primary));
        }
        if let Some(secondary) = self.secondary.as_ref() {
            buttons.push((DialogButton::Secondary, secondary));
        }
        buttons.push((DialogButton::Close, &self.close));
        buttons
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the dialog again (the result of the previous run is kept until taken).
    pub fn open(&mut self) {
        self.open = true;
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The panel's bounds within the window, once arranged.
    #[must_use]
    pub fn panel_bounds(&self, engine: &dyn TextLayoutEngine, theme: &Theme) -> Rect {
        self.geometry(engine, theme).panel
    }

    /// The button Enter activates; the host focuses it when the dialog opens.
    #[must_use]
    pub fn default_button_id(&self) -> WidgetId {
        self.buttons()
            .into_iter()
            .find(|(which, _)| *which == self.default)
            .map_or(self.close.id(), |(_, button)| button.id())
    }

    #[must_use]
    pub fn default_button(&self) -> DialogButton {
        self.default
    }

    /// The button that closed the dialog, once.
    pub fn take_result(&mut self) -> Option<DialogButton> {
        self.result.take()
    }

    fn title_layout(&self, engine: &dyn TextLayoutEngine, theme: &Theme) -> Box<dyn TextLayout> {
        engine.layout(
            &self.title,
            &ui_style(theme, TITLE_SIZE).with_weight(FontWeight::SEMIBOLD),
            &[],
            &[],
            f32::INFINITY,
        )
    }

    fn geometry(&self, engine: &dyn TextLayoutEngine, theme: &Theme) -> Geometry {
        let width = (self.bounds.width - 2.0 * PADDING).clamp(MIN_WIDTH, MAX_WIDTH);
        let body_width = width - 2.0 * PADDING;
        let title = self.title_layout(engine, theme);
        let body = engine.layout(
            &self.body,
            &ui_style(theme, FONT_SIZE),
            &[],
            &[],
            body_width,
        );
        let height = PADDING
            + title.size().height
            + TITLE_GAP
            + body.size().height
            + PADDING
            + BUTTON_HEIGHT
            + PADDING;
        let panel = Rect::new(
            self.bounds.x + (self.bounds.width - width) / 2.0,
            self.bounds.y + (self.bounds.height - height) / 2.0,
            width,
            height,
        );
        Geometry {
            panel,
            title: Point::new(panel.x + PADDING, panel.y + PADDING),
            body: Point::new(
                panel.x + PADDING,
                panel.y + PADDING + title.size().height + TITLE_GAP,
            ),
            buttons: Rect::new(
                panel.x + PADDING,
                panel.bottom() - PADDING - BUTTON_HEIGHT,
                body_width,
                BUTTON_HEIGHT,
            ),
            body_width,
        }
    }

    fn finish(&mut self, which: DialogButton, cx: &mut EventCx) {
        self.result = Some(which);
        self.open = false;
        cx.invalidate(self.bounds);
    }

    fn collect(&mut self, cx: &mut EventCx) {
        let finished = self
            .buttons_mut()
            .into_iter()
            .find_map(|(which, button)| button.take_activated().then_some(which));
        if let Some(which) = finished {
            self.finish(which, cx);
        }
    }

    fn forward(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let mut result = EventResult::ignored();
        for (_, button) in self.buttons_mut() {
            let id = button.id();
            result = result.or(button.event(&mut cx.for_widget(id), e));
        }
        result
    }

    fn key(&mut self, cx: &mut EventCx, key: &KeyEvent, e: &Event) -> EventResult {
        match key.vk {
            vk::ESCAPE if key.mods.is_empty() => {
                self.finish(DialogButton::Close, cx);
            }
            vk::TAB => {
                let ids: Vec<WidgetId> = self
                    .buttons()
                    .iter()
                    .map(|(_, button)| button.id())
                    .collect();
                let current = ids.iter().position(|id| cx.focus.is_focused(*id));
                let next = match (current, key.mods.shift()) {
                    (Some(index), false) => (index + 1) % ids.len(),
                    (Some(index), true) => (index + ids.len() - 1) % ids.len(),
                    (None, false) => 0,
                    (None, true) => ids.len() - 1,
                };
                cx.focus.focus(ids[next]);
                cx.invalidate(self.bounds);
            }
            vk::RETURN if key.mods.is_empty() => {
                let focused = self
                    .buttons()
                    .iter()
                    .any(|(_, button)| cx.focus.is_focused(button.id()));
                if focused {
                    // The focused button reacts to Enter itself.
                    self.forward(cx, e);
                    self.collect(cx);
                } else {
                    self.finish(self.default, cx);
                }
            }
            _ => {
                self.forward(cx, e);
                self.collect(cx);
            }
        }
        EventResult::handled()
    }
}

impl Control for ContentDialog {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for ContentDialog {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        // The dialog covers the window it is shown in.
        Size::new(
            if available.width.is_finite() {
                available.width
            } else {
                MAX_WIDTH + 2.0 * PADDING
            },
            if available.height.is_finite() {
                available.height
            } else {
                240.0
            },
        )
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        if !self.open {
            return;
        }
        let geometry = self.geometry(cx.text, cx.theme);
        let count = self.buttons().len();
        let each =
            ((geometry.buttons.width - BUTTON_GAP * (count as f32 - 1.0)) / count as f32).max(0.0);
        let mut x = geometry.buttons.x;
        for (_, button) in self.buttons_mut() {
            let id = button.id();
            button.arrange(
                &mut cx.for_widget(id),
                Rect::new(x, geometry.buttons.y, each, BUTTON_HEIGHT),
            );
            x += each + BUTTON_GAP;
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.open || !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        let geometry = self.geometry(cx.text, cx.theme);
        canvas.fill_rect(self.bounds, Color::BLACK.with_alpha(SCRIM_OPACITY));
        paint_overlay(canvas, geometry.panel, tokens.surface, cx.theme);
        let title = self.title_layout(cx.text, cx.theme);
        canvas.draw_text_layout(title.as_ref(), geometry.title, tokens.text);
        let body = cx.text.layout(
            &self.body,
            &ui_style(cx.theme, FONT_SIZE),
            &[],
            &[],
            geometry.body_width,
        );
        canvas.draw_text_layout(body.as_ref(), geometry.body, tokens.text);
        canvas.stroke_line(
            Point::new(geometry.panel.x, geometry.buttons.y - PADDING / 2.0),
            Point::new(geometry.panel.right(), geometry.buttons.y - PADDING / 2.0),
            tokens.hairline,
            1.0,
            crate::canvas::Dash::Solid,
        );
        for (_, button) in self.buttons() {
            button.paint(&cx.for_widget(button.id()), canvas);
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if !self.open {
            return EventResult::ignored();
        }
        match e {
            Event::Key(key) if key.is_down => self.key(cx, key, e),
            Event::Pointer(_) => {
                // Modal: nothing behind the dialog sees the pointer.
                self.forward(cx, e);
                self.collect(cx);
                EventResult::handled()
            }
            Event::Wheel(_) | Event::Text(_) | Event::Ime(_) => EventResult::handled(),
            _ => {
                self.forward(cx, e);
                self.collect(cx);
                EventResult::ignored()
            }
        }
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let mut children = Vec::new();
        if self.open {
            for (_, button) in self.buttons() {
                button.access(&cx.for_widget(button.id()), out);
                children.push(button.id());
            }
        }
        let bounds = if self.open { self.bounds } else { Rect::ZERO };
        let mut node = AccessNode::new(self.id, Role::Window, self.title.clone(), bounds)
            .with_description(self.body.clone())
            .with_children(children);
        node.state.offscreen = !self.open;
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
    use crate::input::{Cursor, FocusEvent, Modifiers, PointerButton, PointerEventKind};
    use crate::widgets::testkit::Harness;

    fn setup() -> (Harness, ContentDialog) {
        let mut harness = Harness::new();
        let mut dialog =
            ContentDialog::new("Save changes?", "Your changes will be lost.", "Cancel")
                .with_primary("Save")
                .with_secondary("Don't save");
        harness.arrange(&mut dialog, Rect::new(0.0, 0.0, 800.0, 600.0));
        (harness, dialog)
    }

    fn center(harness: &Harness, dialog: &ContentDialog, which: usize) -> Point {
        let tree = harness.tree(dialog);
        tree.children(dialog.id())
            .nth(which)
            .map(|node| node.bounds.center())
            .expect("button")
    }

    #[test]
    fn kit_dialog_requests_the_hand_over_its_buttons_only() {
        let (mut harness, mut dialog) = setup();
        let primary = center(&harness, &dialog, 0);
        harness.hover(&mut dialog, primary);
        assert_eq!(harness.requests.take_cursor(), Some(Cursor::Hand));
        harness.hover(&mut dialog, Point::new(5.0, 5.0));
        assert_eq!(harness.requests.take_cursor(), None, "scrim");
        let panel = dialog.panel_bounds(&harness.engine, &harness.theme);
        harness.hover(&mut dialog, Point::new(panel.x + 4.0, panel.y + 4.0));
        assert_eq!(harness.requests.take_cursor(), None, "panel padding");
    }

    #[test]
    fn kit_dialog_centers_the_panel_and_shares_the_button_row() {
        let (harness, dialog) = setup();
        let panel = dialog.panel_bounds(&harness.engine, &harness.theme);
        assert_eq!(panel.width, 548.0);
        let close = |a: f32, b: f32| (a - b).abs() < 1e-3;
        assert_eq!(panel.x, (800.0 - 548.0) / 2.0);
        assert!(close(panel.y + panel.height / 2.0, 300.0));
        let tree = harness.tree(&dialog);
        let buttons: Vec<Rect> = tree.children(dialog.id()).map(|n| n.bounds).collect();
        assert_eq!(buttons.len(), 3);
        // Three equal buttons 8 DIPs apart across 548 - 48 DIPs, 24 DIPs above the bottom.
        let each = (500.0 - 16.0) / 3.0;
        assert!(close(buttons[0].width, each));
        assert!(close(buttons[1].x, buttons[0].right() + 8.0));
        assert!(close(buttons[0].bottom(), panel.bottom() - 24.0));
        assert_eq!(harness.focus.tab_order().len(), 3);
        // Narrow windows shrink the panel down to the minimum.
        let mut narrow = ContentDialog::new("t", "b", "ok");
        let mut harness = Harness::new();
        harness.arrange(&mut narrow, Rect::new(0.0, 0.0, 300.0, 300.0));
        assert_eq!(
            narrow.panel_bounds(&harness.engine, &harness.theme).width,
            320.0
        );
    }

    #[test]
    fn kit_dialog_default_button_gets_the_accent() {
        let (mut harness, dialog) = setup();
        let canvas = harness.paint(&dialog);
        let accent = harness.theme.tokens.accent;
        let accent_fills = canvas
            .ops()
            .filter(|op| matches!(op, DrawOp::FillRoundedRect { color, .. } if *color == accent))
            .count();
        assert_eq!(accent_fills, 1);
        assert_eq!(dialog.default_button(), DialogButton::Primary);
        assert_eq!(
            dialog.primary.as_ref().map(Button::kind),
            Some(ButtonKind::Accent)
        );
        let moved = ContentDialog::new("t", "b", "ok")
            .with_primary("yes")
            .with_secondary("no")
            .with_default(DialogButton::Secondary);
        assert_eq!(
            moved.secondary.as_ref().map(Button::kind),
            Some(ButtonKind::Accent)
        );
        assert_eq!(
            moved.primary.as_ref().map(Button::kind),
            Some(ButtonKind::Standard)
        );
        // A missing button cannot be the default; a bare dialog has no accent.
        let bare = ContentDialog::new("t", "b", "ok").with_default(DialogButton::Primary);
        assert_eq!(bare.default_button(), DialogButton::Close);
        assert_eq!(bare.close.kind(), ButtonKind::Standard);
        let close_default = ContentDialog::new("t", "b", "ok")
            .with_secondary("no")
            .with_default(DialogButton::Close);
        assert_eq!(close_default.close.kind(), ButtonKind::Accent);
        assert_eq!(close_default.default_button_id(), close_default.close.id());
        assert_eq!(
            dialog.default_button_id(),
            dialog.primary.as_ref().map(Button::id).expect("primary")
        );
    }

    #[test]
    fn kit_dialog_paints_scrim_panel_title_body_and_buttons() {
        let (mut harness, dialog) = setup();
        let tokens = harness.theme.tokens;
        let panel = dialog.panel_bounds(&harness.engine, &harness.theme);
        let canvas = harness.paint(&dialog);
        assert!(
            matches!(canvas.ops().next(), Some(DrawOp::FillRect { rect, color })
            if *rect == Rect::new(0.0, 0.0, 800.0, 600.0) && color.a == 0.4)
        );
        assert!(canvas.ops().any(
            |op| matches!(op, DrawOp::FillRoundedRect { rect, color, radii }
            if *rect == panel && *color == tokens.surface && radii.top_left == 8.0)
        ));
        assert_eq!(
            canvas.texts(),
            vec![
                "Save changes?",
                "Your changes will be lost.",
                "Save",
                "Don't save",
                "Cancel"
            ]
        );
        // The title is 20 DIPs tall text at the panel padding; the body wraps to the panel width.
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::Text { text, origin, .. }
            if text == "Save changes?" && origin.x == panel.x + 24.0 && origin.y == panel.y + 24.0)));
        // Closed or outside the damage: nothing.
        let mut closed = dialog.clone();
        closed.finish(DialogButton::Close, &mut harness.event_cx(closed.id()));
        assert!(harness.paint(&closed).recorded().is_empty());
        let mut empty = crate::canvas::RecordingCanvas::new();
        let cx = PaintCx::new(
            dialog.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &harness.focus,
            &harness.capture,
            Some(Rect::new(900.0, 900.0, 1.0, 1.0)),
        );
        dialog.paint(&cx, &mut empty);
        assert!(empty.recorded().is_empty());
    }

    #[test]
    fn kit_dialog_escape_enter_and_tab() {
        let (mut harness, mut dialog) = setup();
        // Enter with nothing focused activates the default button.
        assert!(harness.key(&mut dialog, vk::RETURN).handled);
        assert_eq!(dialog.take_result(), Some(DialogButton::Primary));
        assert!(!dialog.is_open());
        // A closed dialog ignores everything until reopened.
        assert!(!harness.key(&mut dialog, vk::ESCAPE).handled);
        dialog.open();
        assert!(harness.key(&mut dialog, vk::ESCAPE).handled);
        assert_eq!(dialog.take_result(), Some(DialogButton::Close));
        assert_eq!(dialog.take_result(), None);
        dialog.open();
        harness.arrange(&mut dialog, Rect::new(0.0, 0.0, 800.0, 600.0));
        // Tab cycles inside the dialog in both directions.
        let order = harness.focus.tab_order().to_vec();
        harness.key(&mut dialog, vk::TAB);
        assert_eq!(harness.focus.focused(), Some(order[0]));
        harness.key(&mut dialog, vk::TAB);
        harness.key(&mut dialog, vk::TAB);
        assert_eq!(harness.focus.focused(), Some(order[2]));
        harness.key(&mut dialog, vk::TAB);
        assert_eq!(harness.focus.focused(), Some(order[0]));
        harness.key_with(&mut dialog, vk::TAB, Modifiers::SHIFT);
        assert_eq!(harness.focus.focused(), Some(order[2]));
        harness.key_with(&mut dialog, vk::TAB, Modifiers::SHIFT);
        assert_eq!(harness.focus.focused(), Some(order[1]));
        // Enter on a focused button activates that button, not the default.
        harness.send(&mut dialog, Event::Focus(FocusEvent::Gained));
        assert!(harness.key(&mut dialog, vk::RETURN).handled);
        assert_eq!(dialog.take_result(), Some(DialogButton::Secondary));
        dialog.open();
        harness.key(&mut dialog, vk::SPACE);
        assert_eq!(dialog.take_result(), Some(DialogButton::Secondary));
        dialog.open();
        // Shift+Tab with nothing focused goes to the last button; other keys are swallowed.
        harness.focus.blur();
        harness.key_with(&mut dialog, vk::TAB, Modifiers::SHIFT);
        assert_eq!(harness.focus.focused(), Some(order[2]));
        assert!(harness.key(&mut dialog, vk::UP).handled);
        assert!(
            harness
                .key_with(&mut dialog, vk::ESCAPE, Modifiers::CTRL)
                .handled
        );
        assert!(dialog.is_open());
        assert!(harness.text(&mut dialog, "x").handled);
    }

    #[test]
    fn kit_dialog_is_modal_for_the_pointer() {
        let (mut harness, mut dialog) = setup();
        let secondary = center(&harness, &dialog, 1);
        assert!(
            harness
                .pointer(
                    &mut dialog,
                    PointerEventKind::Down,
                    Some(PointerButton::Left),
                    Point::new(5.0, 5.0)
                )
                .handled
        );
        assert!(harness.hover(&mut dialog, Point::new(5.0, 5.0)).handled);
        assert_eq!(dialog.take_result(), None);
        assert!(harness.click(&mut dialog, secondary).handled);
        assert_eq!(dialog.take_result(), Some(DialogButton::Secondary));
        assert!(!dialog.is_open());
        assert!(!harness.click(&mut dialog, secondary).handled);
        dialog.open();
        let wheel = Event::Wheel(crate::input::WheelEvent {
            delta: Point::new(0.0, 1.0),
            precise: false,
            position: Point::ZERO,
            mods: Modifiers::NONE,
        });
        assert!(harness.send(&mut dialog, wheel).handled);
        // Frames and other events reach the buttons without being consumed.
        assert!(
            !harness
                .send(
                    &mut dialog,
                    Event::AnimationFrame(std::time::Duration::from_millis(5))
                )
                .handled
        );
    }

    #[test]
    fn kit_dialog_access_node_and_measure() {
        let (mut harness, mut dialog) = setup();
        let tree = harness.tree(&dialog);
        let root = tree.root().expect("root");
        assert_eq!(
            (root.role, root.name.as_str()),
            (Role::Window, "Save changes?")
        );
        assert_eq!(root.description, "Your changes will be lost.");
        let names: Vec<&str> = tree
            .children(dialog.id())
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(names, vec!["Save", "Don't save", "Cancel"]);
        assert!(!harness.perform(&mut dialog, AccessAction::Invoke));
        assert_eq!(dialog.title(), "Save changes?");
        assert_eq!(dialog.body(), "Your changes will be lost.");
        assert_eq!(dialog.bounds(), Rect::new(0.0, 0.0, 800.0, 600.0));
        let size = harness.place(&mut dialog, Point::ZERO).size();
        assert_eq!(size, Size::new(596.0, 240.0));
        harness.send(
            &mut dialog,
            Event::Key(KeyEvent::press(vk::ESCAPE, Modifiers::NONE)),
        );
        let closed = harness.node(&dialog);
        assert!(closed.state.offscreen && closed.children.is_empty());
        // A closed dialog is not laid out.
        harness.arrange(&mut dialog, Rect::new(0.0, 0.0, 800.0, 600.0));
        assert!(harness.focus.tab_order().is_empty());
    }

    #[test]
    fn kit_dialog_with_id_names_the_control_and_measure_covers_the_window() {
        let id = WidgetId::allocate();
        let mut dialog = ContentDialog::new("Title", "Body", "Close").with_id(id);
        assert_eq!(dialog.id(), id);
        let mut harness = crate::widgets::testkit::Harness::new();
        assert_eq!(harness.node(&dialog).id, id);
        let window = Size::new(900.0, 700.0);
        assert_eq!(harness.measure(&mut dialog, window), window);
        let unbounded = harness.measure(&mut dialog, Size::UNBOUNDED);
        assert_eq!(
            harness.measure(&mut dialog, Size::new(f32::INFINITY, 700.0)),
            Size::new(unbounded.width, 700.0)
        );
        assert_eq!(
            harness.measure(&mut dialog, Size::new(900.0, f32::INFINITY)),
            Size::new(900.0, unbounded.height)
        );
    }
}
