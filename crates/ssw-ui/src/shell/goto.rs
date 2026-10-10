//! Go to line (`Ctrl+G`, spec §7): a small overlay at the top centre of the editor with one text
//! box. It accepts `line` or `line:column` (1-based; the column counts characters), clamps
//! numbers beyond the end of the document to the last line / line end, and moves the caret of the
//! shared document on `Enter`. `Esc` (or a press outside) closes it.
//!
//! The text box is seeded with the caret's current line number. Like the other overlays the
//! widget asks for focus with [`GoToLine::take_focus_request`] because it cannot be focused
//! before the host has laid it out.

use std::rc::Rc;

use ssw_core::edit::Selection;
use ssw_core::i18n::{Arg, Localizer};

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::Canvas;
use crate::geometry::{CornerRadii, Insets, Rect, Size};
use crate::input::{Event, PointerEventKind, vk};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::textbox::TextBox;
use crate::widgets::{
    CONTROL_HEIGHT, Control, FONT_SIZE, SharedClipboard, draw_left_centered, layout_line,
    paint_overlay,
};

use super::find::SharedDocument;

/// Widest panel.
const MAX_WIDTH: f32 = 360.0;
/// Distance from the top of the editor.
const TOP: f32 = 48.0;
/// Padding inside the panel.
const PAD: f32 = 8.0;
/// Height of the hint line.
const HINT_HEIGHT: f32 = 20.0;

/// What the overlay did that the host must react to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoToEvent {
    /// The caret moved to this offset and the overlay closed: scroll it into view and focus the
    /// editor.
    Went {
        /// The new caret offset (UTF-8 bytes).
        offset: usize,
    },
    /// The overlay closed without moving the caret.
    Closed,
}

/// Parses `line` or `line:column` into 1-based numbers (the column defaults to 1).
fn parse_spec(text: &str) -> Option<(usize, usize)> {
    let (line, column) = match text.trim().split_once(':') {
        Some((line, column)) => (line.trim(), column.trim()),
        None => (text.trim(), "1"),
    };
    let line = line.parse::<usize>().ok().filter(|number| *number >= 1)?;
    let column = column.parse::<usize>().ok().filter(|number| *number >= 1)?;
    Some((line, column))
}

/// The Go to line overlay.
#[derive(Debug)]
pub struct GoToLine {
    id: WidgetId,
    doc: SharedDocument,
    localizer: Rc<Localizer>,
    input: TextBox,
    hint_id: WidgetId,
    hint_rect: Rect,
    panel: Rect,
    visible: bool,
    focus_request: Option<WidgetId>,
    events: Vec<GoToEvent>,
}

impl GoToLine {
    /// A hidden overlay moving the caret of `doc`.
    #[must_use]
    pub fn new(doc: SharedDocument, localizer: Rc<Localizer>, clipboard: &SharedClipboard) -> Self {
        let input = TextBox::new(
            localizer.text("search-goto-label"),
            "",
            Rc::clone(clipboard),
        );
        Self {
            id: WidgetId::allocate(),
            doc,
            localizer,
            input,
            hint_id: WidgetId::allocate(),
            hint_rect: Rect::ZERO,
            panel: Rect::ZERO,
            visible: false,
            focus_request: None,
            events: Vec::new(),
        }
    }

    /// Applies the motion preference to the text box.
    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.input = self.input.with_motion(motion);
        self
    }

    /// Moves the caret of another document (the active tab changed).
    pub fn set_document(&mut self, doc: SharedDocument) {
        self.doc = doc;
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// The panel's bounds (empty while hidden or before layout).
    #[must_use]
    pub fn panel(&self) -> Rect {
        self.panel
    }

    /// The text box.
    #[must_use]
    pub fn input(&self) -> &TextBox {
        &self.input
    }

    /// Replaces the text of the box (as if typed).
    pub fn set_input(&mut self, text: &str) {
        self.input.set_text(text);
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<GoToEvent> {
        std::mem::take(&mut self.events)
    }

    /// The widget the host should focus after its next layout pass, once.
    pub fn take_focus_request(&mut self) -> Option<WidgetId> {
        self.focus_request.take()
    }

    /// True when `id` is the overlay or its text box.
    #[must_use]
    pub fn owns(&self, id: WidgetId) -> bool {
        id == self.id || id == self.hint_id || id == self.input.id()
    }

    /// Number of lines of the document.
    fn line_count(&self) -> usize {
        self.doc.borrow().buffer().line_count()
    }

    /// Shows the overlay with the caret's line number selected in the box.
    pub fn open(&mut self) {
        let line = {
            let doc = self.doc.borrow();
            doc.buffer().line_of(doc.primary_selection().head) + 1
        };
        self.input.set_text(line.to_string());
        self.input.select_all();
        self.visible = true;
        self.focus_request = Some(self.input.id());
    }

    /// Hides the overlay and reports [`GoToEvent::Closed`].
    pub fn close(&mut self) {
        if self.visible {
            self.visible = false;
            self.panel = Rect::ZERO;
            self.events.push(GoToEvent::Closed);
        }
    }

    /// True when the box holds something that is not a line number (`line` or `line:column`).
    #[must_use]
    pub fn is_invalid(&self) -> bool {
        !self.input.text().trim().is_empty() && parse_spec(self.input.text()).is_none()
    }

    /// The hint line: the valid range, or "Not a valid line number".
    #[must_use]
    pub fn hint_text(&self) -> String {
        if self.is_invalid() {
            self.localizer.text("search-goto-invalid")
        } else {
            self.localizer.format(
                "search-goto-hint",
                &[("count", Arg::Int(self.line_count() as i64))],
            )
        }
    }

    /// Moves the caret to the typed line (and column) and closes. Returns `false` (and stays
    /// open) when the box does not hold a valid position.
    pub fn submit(&mut self) -> bool {
        let Some((line, column)) = parse_spec(self.input.text()) else {
            return false;
        };
        let offset = {
            let doc = self.doc.borrow();
            let buffer = doc.buffer();
            // `line_range` clamps lines past the end to the last line.
            let range = buffer.line_range(line - 1);
            let text = buffer.slice(range);
            let within = text
                .char_indices()
                .nth(column - 1)
                .map_or(text.len(), |(byte, _)| byte);
            range.start + within
        };
        self.doc
            .borrow_mut()
            .set_selections(Selection::caret(offset), &[]);
        self.visible = false;
        self.panel = Rect::ZERO;
        self.events.push(GoToEvent::Went { offset });
        true
    }

    /// Carries out an accessibility action on the text box (`child` is its id).
    pub fn perform_child(&mut self, cx: &mut EventCx, child: WidgetId, a: &AccessAction) -> bool {
        if !self.visible || child != self.input.id() {
            return false;
        }
        let performed = self.input.perform(&mut cx.for_widget(child), a);
        if performed {
            cx.invalidate_all();
        }
        performed
    }
}

impl Control for GoToLine {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for GoToLine {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        if !self.visible {
            return Size::ZERO;
        }
        let width = if available.width.is_finite() {
            MAX_WIDTH.min((available.width - 2.0 * PAD).max(0.0))
        } else {
            MAX_WIDTH
        };
        Size::new(width, 3.0 * PAD + CONTROL_HEIGHT + HINT_HEIGHT)
    }

    /// `bounds` is the editor area; the overlay sits at its top centre.
    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.panel = Rect::ZERO;
        if !self.visible {
            return;
        }
        let width = MAX_WIDTH.min((bounds.width - 2.0 * PAD).max(0.0));
        let panel = Rect::new(
            bounds.x + (bounds.width - width) / 2.0,
            bounds.y + TOP,
            width,
            3.0 * PAD + CONTROL_HEIGHT + HINT_HEIGHT,
        );
        self.panel = panel;
        let input = Rect::new(
            panel.x + PAD,
            panel.y + PAD,
            (panel.width - 2.0 * PAD).max(0.0),
            CONTROL_HEIGHT,
        );
        self.hint_rect = Rect::new(input.x, input.bottom() + PAD, input.width, HINT_HEIGHT);
        let id = self.input.id();
        self.input.arrange(&mut cx.for_widget(id), input);
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.visible || !cx.needs_paint(self.panel) {
            return;
        }
        let tokens = &cx.theme.tokens;
        paint_overlay(canvas, self.panel, tokens.surface, cx.theme);
        self.input.paint(&cx.for_widget(self.input.id()), canvas);
        let invalid = self.is_invalid();
        let hint = layout_line(cx.text, cx.theme, &self.hint_text(), FONT_SIZE);
        let color = if invalid {
            tokens.error
        } else {
            tokens.text_muted
        };
        draw_left_centered(
            canvas,
            hint.as_ref(),
            self.hint_rect.x,
            self.hint_rect,
            color,
        );
        if invalid {
            canvas.stroke_rounded_rect(
                self.input.bounds().inset(Insets::uniform(1.0)),
                CornerRadii::uniform(tokens.metrics.control_radius),
                tokens.error,
                2.0,
            );
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if !self.visible {
            return EventResult::ignored();
        }
        let id = self.input.id();
        if let Event::Key(key) = e
            && key.is_down
            && key.vk == vk::ESCAPE
            && key.mods.is_empty()
            && cx.focus.is_focused(id)
        {
            self.close();
            cx.invalidate_all();
            return EventResult::handled();
        }
        if let Event::Pointer(pointer) = e
            && pointer.kind == PointerEventKind::Down
            && !self.panel.contains(pointer.position)
        {
            self.close();
            cx.invalidate_all();
            return EventResult::ignored();
        }
        let mut result = self.input.event(&mut cx.for_widget(id), e);
        if result.request_focus {
            cx.focus.focus(id);
            result.request_focus = false;
        }
        if self.input.take_submitted() {
            self.submit();
        }
        if let Event::Pointer(pointer) = e
            && self.panel.contains(pointer.position)
        {
            result = result.or(EventResult::handled());
        }
        if result.handled {
            cx.invalidate_all();
        }
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let name = self.localizer.text("search-goto-label");
        if !self.visible {
            let mut node = AccessNode::new(self.id, Role::Group, name, Rect::ZERO);
            node.state.offscreen = true;
            out.insert(node);
            return;
        }
        let id = self.input.id();
        self.input.access(&cx.for_widget(id), out);
        // `access` just put the text box's node into the tree.
        out.node(id).cloned().into_iter().for_each(|mut node| {
            node.description = self.hint_text();
            out.insert(node);
        });
        out.insert(AccessNode::new(
            self.hint_id,
            Role::Text,
            self.hint_text(),
            self.hint_rect,
        ));
        out.insert(
            AccessNode::new(self.id, Role::Group, name, self.panel)
                .with_children([id, self.hint_id]),
        );
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}
