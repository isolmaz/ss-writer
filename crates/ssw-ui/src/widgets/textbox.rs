//! A single-line text box: caret, selection, word movement, clipboard, IME composition with
//! underline, placeholder and horizontal scrolling. Offsets are UTF-8 bytes on grapheme boundaries.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, Dash};
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::{
    Cursor, Event, FocusEvent, ImeAttr, ImeAttrStyle, ImeEvent, KeyEvent, PointerButton,
    PointerEvent, PointerEventKind, vk,
};
use crate::services::ClipboardContent;
use crate::text::TextLayout;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::anim::Motion;
use super::{
    CONTROL_HEIGHT, Control, FONT_SIZE, Interaction, SharedClipboard, control_state, layout_line,
    perform_focus,
};

/// Padding between the border and the text.
const PADDING: f32 = 10.0;
/// Width a text box asks for when its parent does not constrain it.
const DEFAULT_WIDTH: f32 = 200.0;
/// Thickness of the accent line under a focused text box.
const FOCUS_LINE: f32 = 2.0;
/// Key code of the letter A (Ctrl+A).
const KEY_A: u16 = 0x41;
const KEY_C: u16 = 0x43;
const KEY_V: u16 = 0x56;
const KEY_X: u16 = 0x58;

/// An IME composition in progress, displayed at the caret.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Composition {
    text: String,
    /// Caret inside `text`.
    caret: usize,
    attrs: Vec<ImeAttr>,
}

/// The largest char boundary of `text` not above `index`.
fn clamp_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn previous_grapheme(text: &str, from: usize) -> usize {
    text[..from]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_grapheme(text: &str, from: usize) -> usize {
    text[from..]
        .graphemes(true)
        .next()
        .map_or(from, |grapheme| from + grapheme.len())
}

/// Start of the word before `from` (or the start of the text).
fn previous_word(text: &str, from: usize) -> usize {
    text.split_word_bound_indices()
        .rfind(|(index, word)| *index < from && word.chars().any(char::is_alphanumeric))
        .map_or(0, |(index, _)| index)
}

/// Start of the word after `from` (or the end of the text).
fn next_word(text: &str, from: usize) -> usize {
    text.split_word_bound_indices()
        .find(|(index, word)| *index > from && word.chars().any(char::is_alphanumeric))
        .map_or(text.len(), |(index, _)| index)
}

/// The word (or run of blanks / punctuation) around `offset`.
fn word_range(text: &str, offset: usize) -> Range<usize> {
    text.split_word_bound_indices()
        .find(|(index, word)| offset >= *index && offset < index + word.len())
        .map_or(text.len()..text.len(), |(index, word)| {
            index..index + word.len()
        })
}

/// A single-line editable text field.
pub struct TextBox {
    id: WidgetId,
    label: String,
    placeholder: String,
    text: String,
    caret: usize,
    anchor: usize,
    enabled: bool,
    read_only: bool,
    bounds: Rect,
    /// How far the text is scrolled to the left.
    scroll: f32,
    interaction: Interaction,
    clipboard: SharedClipboard,
    composition: Option<Composition>,
    dragging: bool,
    changed: bool,
    submitted: bool,
}

impl std::fmt::Debug for TextBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextBox")
            .field("id", &self.id)
            .field("text", &self.text)
            .field("selection", &self.selection())
            .field("composition", &self.composition)
            .finish_non_exhaustive()
    }
}

impl TextBox {
    /// A text box named `label` holding `text`, copying and pasting through `clipboard`.
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        text: impl Into<String>,
        clipboard: SharedClipboard,
    ) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            id: WidgetId::allocate(),
            label: label.into(),
            placeholder: String::new(),
            text,
            caret: end,
            anchor: end,
            enabled: true,
            read_only: false,
            bounds: Rect::ZERO,
            scroll: 0.0,
            interaction: Interaction::new(Motion::full()),
            clipboard,
            composition: None,
            dragging: false,
            changed: false,
            submitted: false,
        }
    }

    #[must_use]
    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Text can be selected and copied but not changed.
    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
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
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replaces the text (without notifying the owner) and puts the caret at the end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.caret = self.text.len();
        self.anchor = self.caret;
        self.composition = None;
    }

    #[must_use]
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The selected byte range (empty at the caret when nothing is selected).
    #[must_use]
    pub fn selection(&self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }

    #[must_use]
    pub fn selected_text(&self) -> &str {
        &self.text[self.selection()]
    }

    /// Selects from `anchor` to `caret` (clamped to grapheme boundaries).
    pub fn set_selection(&mut self, anchor: usize, caret: usize) {
        self.anchor = clamp_boundary(&self.text, anchor);
        self.caret = clamp_boundary(&self.text, caret);
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// The composition string while an IME composition is in progress.
    #[must_use]
    pub fn composition(&self) -> Option<&str> {
        self.composition
            .as_ref()
            .map(|composition| composition.text.as_str())
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// True once after the user (or assistive technology) changed the text.
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// True once after Enter was pressed.
    pub fn take_submitted(&mut self) -> bool {
        std::mem::take(&mut self.submitted)
    }

    fn inner(&self) -> Rect {
        self.bounds.inset(Insets::symmetric(PADDING, 0.0))
    }

    /// The text as displayed: the composition is spliced in at the caret.
    fn display(&self) -> String {
        match &self.composition {
            Some(composition) => {
                let mut display = String::with_capacity(self.text.len() + composition.text.len());
                display.push_str(&self.text[..self.caret]);
                display.push_str(&composition.text);
                display.push_str(&self.text[self.caret..]);
                display
            }
            None => self.text.clone(),
        }
    }

    /// Display offset of the byte offset `offset` of the text.
    fn to_display(&self, offset: usize) -> usize {
        match &self.composition {
            Some(composition) if offset > self.caret => offset + composition.text.len(),
            _ => offset,
        }
    }

    /// Display range of the composition.
    fn composition_range(&self) -> Option<Range<usize>> {
        self.composition
            .as_ref()
            .map(|composition| self.caret..self.caret + composition.text.len())
    }

    /// The text offset for a display offset (offsets inside the composition map to the caret).
    fn source_offset(&self, display: usize) -> usize {
        match &self.composition {
            Some(composition) if display >= self.caret + composition.text.len() => {
                display - composition.text.len()
            }
            Some(_) if display > self.caret => self.caret,
            _ => display.min(self.text.len()),
        }
    }

    fn replace(&mut self, range: Range<usize>, replacement: &str) {
        self.text.replace_range(range.clone(), replacement);
        self.caret = range.start + replacement.len();
        self.anchor = self.caret;
        self.changed = true;
    }

    /// Replaces the selection by `typed`, minus line breaks.
    fn insert(&mut self, typed: &str) {
        if self.read_only {
            return;
        }
        let cleaned: String = typed.chars().filter(|c| !c.is_control()).collect();
        if cleaned.is_empty() && self.selection().is_empty() {
            return;
        }
        let range = self.selection();
        self.replace(range, &cleaned);
    }

    fn delete(&mut self, forward: bool, by_word: bool) {
        if self.read_only {
            return;
        }
        if self.selection().is_empty() {
            let (from, to) = match (forward, by_word) {
                (false, false) => (previous_grapheme(&self.text, self.caret), self.caret),
                (false, true) => (previous_word(&self.text, self.caret), self.caret),
                (true, false) => (self.caret, next_grapheme(&self.text, self.caret)),
                (true, true) => (self.caret, next_word(&self.text, self.caret)),
            };
            if from == to {
                return;
            }
            self.replace(from..to, "");
        } else {
            let range = self.selection();
            self.replace(range, "");
        }
    }

    fn move_caret(&mut self, to: usize, extend: bool) {
        self.caret = to;
        if !extend {
            self.anchor = to;
        }
    }

    fn copy(&mut self) -> bool {
        let selected = self.selected_text();
        if selected.is_empty() {
            return false;
        }
        let content = ClipboardContent::from_text(selected);
        self.clipboard.borrow_mut().set(content).is_ok()
    }

    fn paste(&mut self) {
        let pasted = self.clipboard.borrow().text();
        if let Ok(Some(text)) = pasted {
            self.insert(&text);
        }
    }

    fn key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.mods.ctrl();
        let shift = key.mods.shift();
        if key.mods.alt() {
            return false;
        }
        match key.vk {
            vk::LEFT => {
                let to = match (self.selection().is_empty() || shift, ctrl) {
                    (false, _) => self.selection().start,
                    (true, false) => previous_grapheme(&self.text, self.caret),
                    (true, true) => previous_word(&self.text, self.caret),
                };
                self.move_caret(to, shift);
            }
            vk::RIGHT => {
                let to = match (self.selection().is_empty() || shift, ctrl) {
                    (false, _) => self.selection().end,
                    (true, false) => next_grapheme(&self.text, self.caret),
                    (true, true) => next_word(&self.text, self.caret),
                };
                self.move_caret(to, shift);
            }
            vk::HOME => self.move_caret(0, shift),
            vk::END => self.move_caret(self.text.len(), shift),
            vk::BACK => self.delete(false, ctrl),
            vk::DELETE if shift && !ctrl => {
                if self.copy() {
                    self.delete(true, false);
                }
            }
            vk::DELETE => self.delete(true, ctrl),
            vk::INSERT if ctrl => {
                self.copy();
            }
            vk::INSERT if shift => self.paste(),
            vk::RETURN if key.mods.is_empty() => self.submitted = true,
            KEY_A if ctrl => self.select_all(),
            KEY_C if ctrl => {
                self.copy();
            }
            KEY_X if ctrl => {
                if self.copy() {
                    self.delete(true, false);
                }
            }
            KEY_V if ctrl => self.paste(),
            _ => return false,
        }
        true
    }

    fn ime(&mut self, event: &ImeEvent) {
        if self.read_only {
            return;
        }
        match event {
            ImeEvent::Start => {
                let range = self.selection();
                if !range.is_empty() {
                    self.replace(range, "");
                }
                self.composition = Some(Composition {
                    text: String::new(),
                    caret: 0,
                    attrs: Vec::new(),
                });
            }
            ImeEvent::Update { text, caret, attrs } => {
                let range = self.selection();
                if !range.is_empty() {
                    self.replace(range, "");
                }
                self.composition = Some(Composition {
                    caret: clamp_boundary(text, *caret),
                    text: text.clone(),
                    attrs: attrs.clone(),
                });
            }
            ImeEvent::Commit(text) => {
                self.composition = None;
                self.insert(text);
            }
            ImeEvent::End => self.composition = None,
        }
    }

    /// Scrolls so that the caret (or the composition caret) is inside the text area.
    fn reveal_caret(&mut self, cx: &EventCx) {
        let display = self.display();
        let layout = layout_line(cx.text, cx.theme, &display, FONT_SIZE);
        let at = self.to_display(self.caret) + self.composition.as_ref().map_or(0, |c| c.caret);
        let x = layout.caret_rect(at, false).x;
        let width = self.inner().width;
        if x < self.scroll {
            self.scroll = x;
        } else if x > self.scroll + width - 2.0 {
            self.scroll = x - width + 2.0;
        }
        self.scroll = self.scroll.max(0.0);
    }

    fn offset_at(&self, cx: &EventCx, position: Point) -> usize {
        let display = self.display();
        let layout = layout_line(cx.text, cx.theme, &display, FONT_SIZE);
        let inner = self.inner();
        let point = Point::new(
            position.x - inner.x + self.scroll,
            layout.size().height / 2.0,
        );
        let offset = clamp_boundary(&display, layout.hit_test_point(point).insertion_offset());
        self.source_offset(offset)
    }

    fn pointer(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        if !self.enabled {
            return EventResult::ignored();
        }
        let inside = self.bounds.contains(event.position);
        if inside {
            cx.set_cursor(Cursor::IBeam);
        }
        match event.kind {
            PointerEventKind::Down if event.button == Some(PointerButton::Left) && inside => {
                let offset = self.offset_at(cx, event.position);
                match event.click_count {
                    2 => {
                        let range = word_range(&self.text, offset);
                        self.set_selection(range.start, range.end);
                    }
                    3.. => self.select_all(),
                    _ => self.move_caret(offset, event.mods.shift()),
                }
                self.dragging = true;
                cx.capture_pointer();
                self.interaction.set_hovered(cx, true);
                self.reveal_caret(cx);
                cx.invalidate(self.bounds);
                EventResult::handled_with_focus()
            }
            PointerEventKind::Move if self.dragging => {
                let offset = self.offset_at(cx, event.position);
                self.move_caret(offset, true);
                self.reveal_caret(cx);
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            PointerEventKind::Up if event.button == Some(PointerButton::Left) && self.dragging => {
                self.dragging = false;
                cx.release_pointer();
                EventResult::handled()
            }
            PointerEventKind::Move | PointerEventKind::Enter | PointerEventKind::Leave => {
                let hovered = inside && event.kind != PointerEventKind::Leave;
                if self.interaction.set_hovered(cx, hovered) {
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            _ => EventResult::ignored(),
        }
    }

    fn colors_underline(style: ImeAttrStyle) -> f32 {
        match style {
            ImeAttrStyle::TargetConverted | ImeAttrStyle::TargetNotConverted => 2.0,
            _ => 1.0,
        }
    }
}

impl Control for TextBox {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for TextBox {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        let width = if available.width.is_finite() {
            available.width
        } else {
            DEFAULT_WIDTH
        };
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
        let focused = cx.is_focused();
        let radii = CornerRadii::uniform(tokens.metrics.control_radius);
        let fill = if self.enabled {
            self.interaction.blend(
                tokens.surface,
                tokens.hover_bg.over(tokens.surface),
                tokens.surface,
            )
        } else {
            tokens.surface
        };
        canvas.fill_rounded_rect(self.bounds, radii, fill);
        canvas.stroke_rounded_rect(
            self.bounds.inset(Insets::uniform(0.5)),
            radii,
            tokens.hairline,
            1.0,
        );
        if focused {
            canvas.fill_rect(
                Rect::new(
                    self.bounds.x + 2.0,
                    self.bounds.bottom() - FOCUS_LINE,
                    self.bounds.width - 4.0,
                    FOCUS_LINE,
                ),
                tokens.accent,
            );
        }
        let inner = self.inner();
        canvas.push_clip_rect(inner);
        let display = self.display();
        let color = if self.enabled {
            tokens.text
        } else {
            tokens.text_disabled
        };
        if display.is_empty() {
            if !self.placeholder.is_empty() {
                let placeholder = layout_line(cx.text, cx.theme, &self.placeholder, FONT_SIZE);
                let origin = Point::new(
                    inner.x,
                    inner.y + (inner.height - placeholder.size().height) / 2.0,
                );
                canvas.draw_text_layout(placeholder.as_ref(), origin, tokens.text_muted);
            }
        } else {
            let layout = layout_line(cx.text, cx.theme, &display, FONT_SIZE);
            let origin = Point::new(
                inner.x - self.scroll,
                inner.y + (inner.height - layout.size().height) / 2.0,
            );
            let selection = self.selection();
            if !selection.is_empty() {
                let color = if focused {
                    tokens.selection_bg
                } else {
                    tokens.selection_inactive_bg
                };
                for rect in layout.range_rects(selection) {
                    canvas.fill_rect(rect.translate(origin.x, origin.y), color);
                }
            }
            canvas.draw_text_layout(layout.as_ref(), origin, color);
            self.paint_composition(canvas, layout.as_ref(), origin, color);
        }
        if focused && self.selection().is_empty() {
            let display = self.display();
            let layout = layout_line(cx.text, cx.theme, &display, FONT_SIZE);
            let at = self.to_display(self.caret) + self.composition.as_ref().map_or(0, |c| c.caret);
            let caret = layout.caret_rect(at, false);
            let top = inner.y + (inner.height - layout.size().height) / 2.0 + caret.y;
            canvas.fill_rect(
                Rect::new(inner.x - self.scroll + caret.x, top, 1.0, caret.height),
                tokens.caret,
            );
        }
        canvas.pop_clip();
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => self.pointer(cx, event),
            Event::Key(key) if key.is_down && self.enabled && cx.is_focused() => {
                if self.key(key) {
                    self.reveal_caret(cx);
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            Event::Text(text) if self.enabled && cx.is_focused() => {
                self.insert(&text.0);
                self.reveal_caret(cx);
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            Event::Ime(event) if self.enabled && cx.is_focused() => {
                self.ime(event);
                self.reveal_caret(cx);
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            Event::Focus(FocusEvent::Gained) => {
                cx.invalidate(self.bounds);
                EventResult::ignored()
            }
            Event::Focus(FocusEvent::Lost) => {
                self.composition = None;
                self.dragging = false;
                cx.invalidate(self.bounds);
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
        state.read_only = self.read_only;
        let mut node = AccessNode::new(self.id, Role::Edit, self.label.clone(), self.bounds)
            .with_state(state)
            .with_value(self.text.clone());
        node.patterns.value = true;
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::SetValue(value) if self.enabled && !self.read_only => {
                self.set_text(value.clone());
                self.changed = true;
                self.reveal_caret(cx);
                cx.invalidate(self.bounds);
                true
            }
            AccessAction::Focus if self.enabled => perform_focus(cx),
            _ => false,
        }
    }
}

impl TextBox {
    /// Underlines the composition: thick for the converted clause being edited, thin otherwise.
    fn paint_composition(
        &self,
        canvas: &mut dyn Canvas,
        layout: &dyn TextLayout,
        origin: Point,
        color: crate::geometry::Color,
    ) {
        let (Some(range), Some(composition)) = (self.composition_range(), &self.composition) else {
            return;
        };
        let segments: Vec<(Range<usize>, f32)> = if composition.attrs.is_empty() {
            vec![(0..composition.text.len(), 1.0)]
        } else {
            composition
                .attrs
                .iter()
                .map(|attr| (attr.range.clone(), Self::colors_underline(attr.style)))
                .collect()
        };
        for (segment, width) in segments {
            let start = range.start + segment.start.min(composition.text.len());
            let end = range.start + segment.end.min(composition.text.len());
            for rect in layout.range_rects(start..end) {
                let y = origin.y + rect.bottom() - 1.0;
                canvas.stroke_line(
                    Point::new(origin.x + rect.x, y),
                    Point::new(origin.x + rect.right(), y),
                    color,
                    width,
                    Dash::Solid,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::input::{Modifiers, PointerType};
    use crate::services::MemoryClipboard;
    use crate::widgets::testkit::Harness;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn clipboard() -> SharedClipboard {
        Rc::new(RefCell::new(MemoryClipboard::default()))
    }

    fn setup(text: &str) -> (Harness, TextBox, SharedClipboard) {
        let mut harness = Harness::new();
        let shared = clipboard();
        let mut textbox =
            TextBox::new("Name", text, Rc::clone(&shared)).with_motion(Motion::reduced());
        harness.arrange(&mut textbox, Rect::new(10.0, 10.0, 200.0, 32.0));
        harness.focus_widget(&mut textbox);
        (harness, textbox, shared)
    }

    #[test]
    fn kit_textbox_typing_replaces_selection_and_strips_line_breaks() {
        let (mut harness, mut textbox, _) = setup("");
        harness.text(&mut textbox, "hello");
        harness.text(&mut textbox, " wor\nld\t");
        assert_eq!(textbox.text(), "hello world");
        assert!(textbox.take_changed());
        assert!(!textbox.take_changed());
        textbox.set_selection(0, 5);
        harness.text(&mut textbox, "J");
        assert_eq!((textbox.text(), textbox.caret()), ("J world", 1));
        // Empty or control-only input changes nothing.
        textbox.take_changed();
        harness.text(&mut textbox, "\n");
        assert!(!textbox.take_changed());
        // Without focus nothing is typed.
        let (mut harness, mut idle, _) = setup("a");
        harness.focus.blur();
        assert!(!harness.text(&mut idle, "b").handled);
        assert_eq!(idle.text(), "a");
    }

    #[test]
    fn kit_textbox_caret_movement_by_grapheme_and_word_with_selection() {
        let (mut harness, mut textbox, _) = setup("ağış  and ünı-x");
        // "ğ" and "ı" are two bytes each; moves never split them.
        harness.key(&mut textbox, vk::HOME);
        harness.key(&mut textbox, vk::RIGHT);
        assert_eq!(textbox.caret(), 1);
        harness.key(&mut textbox, vk::RIGHT);
        assert_eq!(textbox.caret(), 3);
        harness.key(&mut textbox, vk::LEFT);
        assert_eq!(textbox.caret(), 1);
        harness.key_with(&mut textbox, vk::RIGHT, Modifiers::CTRL);
        assert_eq!(textbox.caret(), "ağış  ".len(), "next word start");
        harness.key_with(&mut textbox, vk::RIGHT, Modifiers::CTRL);
        assert_eq!(textbox.caret(), "ağış  and ".len());
        harness.key_with(&mut textbox, vk::LEFT, Modifiers::CTRL);
        assert_eq!(textbox.caret(), "ağış  ".len(), "previous word start");
        harness.key_with(&mut textbox, vk::LEFT, Modifiers::CTRL);
        harness.key_with(&mut textbox, vk::LEFT, Modifiers::CTRL);
        assert_eq!(textbox.caret(), 0);
        // Shift extends; plain arrows collapse to the selection's edge.
        harness.key_with(&mut textbox, vk::RIGHT, Modifiers::SHIFT);
        harness.key_with(&mut textbox, vk::RIGHT, Modifiers::SHIFT);
        assert_eq!(textbox.selection(), 0..3);
        assert_eq!(textbox.selected_text(), "ağ");
        harness.key(&mut textbox, vk::RIGHT);
        assert_eq!((textbox.caret(), textbox.selection().is_empty()), (3, true));
        textbox.set_selection(0, 3);
        harness.key(&mut textbox, vk::LEFT);
        assert_eq!(textbox.caret(), 0);
        harness.key_with(&mut textbox, vk::END, Modifiers::SHIFT);
        assert_eq!(textbox.selection(), 0..textbox.text().len());
        harness.key(&mut textbox, vk::END);
        harness.key_with(&mut textbox, vk::RIGHT, Modifiers::CTRL);
        assert_eq!(textbox.caret(), textbox.text().len());
        harness.key(&mut textbox, vk::RIGHT);
        assert_eq!(textbox.caret(), textbox.text().len());
        assert!(!harness.key(&mut textbox, vk::UP).handled);
        assert!(
            !harness
                .key_with(&mut textbox, vk::LEFT, Modifiers::ALT)
                .handled
        );
    }

    #[test]
    fn kit_textbox_deletion_removes_whole_graphemes_and_words() {
        let (mut harness, mut textbox, _) = setup("a👍🏽 bc dé");
        harness.key(&mut textbox, vk::BACK);
        harness.key(&mut textbox, vk::BACK);
        assert_eq!(textbox.text(), "a👍🏽 bc ");
        harness.key_with(&mut textbox, vk::BACK, Modifiers::CTRL);
        assert_eq!(textbox.text(), "a👍🏽 ");
        harness.key(&mut textbox, vk::HOME);
        harness.key(&mut textbox, vk::RIGHT);
        harness.key(&mut textbox, vk::DELETE);
        assert_eq!(
            textbox.text(),
            "a ",
            "the emoji and its skin tone go together"
        );
        harness.key_with(&mut textbox, vk::DELETE, Modifiers::CTRL);
        assert_eq!(textbox.text(), "a");
        harness.key(&mut textbox, vk::DELETE);
        harness.key(&mut textbox, vk::END);
        harness.key(&mut textbox, vk::BACK);
        harness.key(&mut textbox, vk::BACK);
        assert_eq!(textbox.text(), "");
        assert!(textbox.take_changed());
        // A selection is deleted as a whole by either key.
        textbox.set_text("abcdef");
        textbox.set_selection(1, 4);
        harness.key(&mut textbox, vk::DELETE);
        assert_eq!(textbox.text(), "aef");
        textbox.set_selection(0, 1);
        harness.key(&mut textbox, vk::BACK);
        assert_eq!(textbox.text(), "ef");
    }

    #[test]
    fn kit_textbox_clipboard_copy_cut_paste_through_the_service() {
        let (mut harness, mut textbox, shared) = setup("hello world");
        textbox.set_selection(0, 5);
        harness.key_with(&mut textbox, KEY_C, Modifiers::CTRL);
        assert_eq!(
            shared.borrow().text().expect("clipboard"),
            Some("hello".to_owned())
        );
        textbox.set_selection(6, 11);
        harness.key_with(&mut textbox, KEY_X, Modifiers::CTRL);
        assert_eq!(textbox.text(), "hello ");
        assert_eq!(
            shared.borrow().text().expect("clipboard"),
            Some("world".to_owned())
        );
        harness.key_with(&mut textbox, KEY_V, Modifiers::CTRL);
        assert_eq!(textbox.text(), "hello world");
        // Shift+Delete cuts, Ctrl+Insert copies, Shift+Insert pastes.
        textbox.set_selection(0, 6);
        harness.key_with(&mut textbox, vk::DELETE, Modifiers::SHIFT);
        assert_eq!(textbox.text(), "world");
        textbox.set_selection(0, 5);
        harness.key_with(&mut textbox, vk::INSERT, Modifiers::CTRL);
        textbox.set_selection(5, 5);
        harness.key_with(&mut textbox, vk::INSERT, Modifiers::SHIFT);
        assert_eq!(textbox.text(), "worldworld");
        // Copying nothing leaves the clipboard alone; pasting from an empty clipboard does nothing.
        textbox.set_selection(0, 0);
        harness.key_with(&mut textbox, KEY_C, Modifiers::CTRL);
        harness.key_with(&mut textbox, KEY_X, Modifiers::CTRL);
        assert_eq!(textbox.text(), "worldworld");
        shared.borrow_mut().clear().expect("clear");
        harness.key_with(&mut textbox, KEY_V, Modifiers::CTRL);
        assert_eq!(textbox.text(), "worldworld");
        // Pasted line breaks are dropped (single line).
        shared
            .borrow_mut()
            .set(ClipboardContent::from_text("a\r\nb"))
            .expect("set");
        textbox.set_text("");
        harness.key_with(&mut textbox, KEY_V, Modifiers::CTRL);
        assert_eq!(textbox.text(), "ab");
        harness.key_with(&mut textbox, KEY_A, Modifiers::CTRL);
        assert_eq!(textbox.selection(), 0..2);
    }

    #[test]
    fn kit_textbox_read_only_selects_and_copies_but_never_edits() {
        let mut harness = Harness::new();
        let shared = clipboard();
        let mut textbox = TextBox::new("Path", "C:/docs", Rc::clone(&shared)).read_only();
        harness.arrange(&mut textbox, Rect::new(0.0, 0.0, 200.0, 32.0));
        harness.focus_widget(&mut textbox);
        harness.text(&mut textbox, "x");
        harness.key(&mut textbox, vk::BACK);
        harness.key_with(&mut textbox, KEY_V, Modifiers::CTRL);
        harness.send(&mut textbox, Event::Ime(ImeEvent::Start));
        harness.send(&mut textbox, Event::Ime(ImeEvent::Commit("y".into())));
        assert_eq!(textbox.text(), "C:/docs");
        harness.key_with(&mut textbox, KEY_A, Modifiers::CTRL);
        harness.key_with(&mut textbox, KEY_C, Modifiers::CTRL);
        harness.key_with(&mut textbox, KEY_X, Modifiers::CTRL);
        assert_eq!(
            shared.borrow().text().expect("clipboard"),
            Some("C:/docs".to_owned())
        );
        assert_eq!(textbox.text(), "C:/docs");
        assert!(textbox.is_read_only());
        assert!(harness.node(&textbox).state.read_only);
        assert!(!harness.perform(&mut textbox, AccessAction::SetValue("z".into())));
        assert!(!textbox.take_changed());
    }

    #[test]
    fn kit_textbox_enter_submits() {
        let (mut harness, mut textbox, _) = setup("go");
        assert!(harness.key(&mut textbox, vk::RETURN).handled);
        assert!(textbox.take_submitted());
        assert!(!textbox.take_submitted());
        assert!(
            !harness
                .key_with(&mut textbox, vk::RETURN, Modifiers::CTRL)
                .handled
        );
    }

    #[test]
    fn kit_textbox_ime_composition_is_displayed_underlined_and_committed() {
        let (mut harness, mut textbox, _) = setup("ab");
        textbox.set_selection(1, 1);
        harness.send(&mut textbox, Event::Ime(ImeEvent::Start));
        assert_eq!(textbox.composition(), Some(""));
        harness.send(
            &mut textbox,
            Event::Ime(ImeEvent::Update {
                text: "かな".into(),
                caret: 3,
                attrs: vec![
                    ImeAttr {
                        range: 0..3,
                        style: ImeAttrStyle::TargetConverted,
                    },
                    ImeAttr {
                        range: 3..6,
                        style: ImeAttrStyle::Input,
                    },
                ],
            }),
        );
        assert_eq!(textbox.composition(), Some("かな"));
        assert_eq!(textbox.text(), "ab", "the text only changes on commit");
        let canvas = harness.paint(&textbox);
        assert_eq!(canvas.texts(), vec!["aかなb"]);
        // Two underlines: a thick one under the converted clause and a thin one under the rest.
        let lines: Vec<(f32, f32, f32)> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::StrokeLine {
                    from, to, width, ..
                } => Some((from.x, to.x, *width)),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].2, 2.0);
        assert_eq!(lines[1].2, 1.0);
        assert_eq!(lines[0].1 - lines[0].0, 8.0, "one cluster wide");
        assert_eq!(lines[1].0, lines[0].1);
        harness.send(&mut textbox, Event::Ime(ImeEvent::Commit("か".into())));
        harness.send(&mut textbox, Event::Ime(ImeEvent::End));
        assert_eq!((textbox.text(), textbox.composition()), ("aかb", None));
        assert_eq!(textbox.caret(), 1 + "か".len());
        assert!(textbox.take_changed());
        // Starting a composition over a selection replaces it; End cancels; without attributes the
        // whole composition has the thin underline.
        textbox.set_selection(0, 4);
        harness.send(&mut textbox, Event::Ime(ImeEvent::Start));
        assert_eq!(textbox.text(), "b");
        harness.send(
            &mut textbox,
            Event::Ime(ImeEvent::Update {
                text: "x".into(),
                caret: 99,
                attrs: vec![],
            }),
        );
        let canvas = harness.paint(&textbox);
        assert_eq!(
            canvas
                .ops()
                .filter(|op| matches!(op, DrawOp::StrokeLine { width, .. } if *width == 1.0))
                .count(),
            1
        );
        harness.send(&mut textbox, Event::Ime(ImeEvent::End));
        assert_eq!(textbox.composition(), None);
        // Updates over a selection replace it too.
        textbox.set_selection(0, 1);
        harness.send(
            &mut textbox,
            Event::Ime(ImeEvent::Update {
                text: "q".into(),
                caret: 1,
                attrs: vec![],
            }),
        );
        assert_eq!(textbox.text(), "");
        // Losing focus cancels a composition.
        harness.send(&mut textbox, Event::Focus(FocusEvent::Lost));
        assert_eq!(textbox.composition(), None);
        // The IME is ignored without focus.
        harness.focus.blur();
        assert!(
            !harness
                .send(&mut textbox, Event::Ime(ImeEvent::Start))
                .handled
        );
    }

    #[test]
    fn kit_textbox_pointer_places_caret_drags_selection_and_selects_words() {
        let (mut harness, mut textbox, _) = setup("hello big world");
        // Text starts at x = 20 (10 + 10 padding), 8 DIPs per glyph.
        let y = 26.0;
        let result = harness.pointer(
            &mut textbox,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            Point::new(20.0 + 3.0 * 8.0 + 2.0, y),
        );
        assert!(result.handled && result.request_focus);
        assert_eq!(textbox.caret(), 3);
        assert!(harness.capture.is_captured_by(textbox.id()));
        assert_eq!(harness.requests.cursor(), Some(Cursor::IBeam));
        harness.pointer(
            &mut textbox,
            PointerEventKind::Move,
            None,
            Point::new(20.0 + 9.0 * 8.0 + 1.0, y),
        );
        assert_eq!(textbox.selection(), 3..9);
        harness.pointer(
            &mut textbox,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::new(100.0, y),
        );
        assert!(!harness.capture.is_captured_by(textbox.id()));
        // Shift+click extends from the caret.
        let shift_click = PointerEvent {
            mods: Modifiers::SHIFT,
            click_count: 1,
            ..PointerEvent::mouse(
                PointerEventKind::Down,
                Some(PointerButton::Left),
                Point::new(20.0 + 15.0 * 8.0, y),
            )
        };
        harness.send(&mut textbox, Event::Pointer(shift_click));
        assert_eq!(textbox.selection(), 3..15);
        harness.pointer(
            &mut textbox,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::ZERO,
        );
        // Double click selects the word, triple click everything.
        let double = PointerEvent {
            click_count: 2,
            pointer_type: PointerType::Mouse,
            ..PointerEvent::mouse(
                PointerEventKind::Down,
                Some(PointerButton::Left),
                Point::new(20.0 + 7.0 * 8.0, y),
            )
        };
        harness.send(&mut textbox, Event::Pointer(double));
        assert_eq!(textbox.selected_text(), "big");
        harness.pointer(
            &mut textbox,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::ZERO,
        );
        let triple = PointerEvent {
            click_count: 3,
            ..double
        };
        harness.send(&mut textbox, Event::Pointer(triple));
        assert_eq!(textbox.selection(), 0..15);
        harness.pointer(
            &mut textbox,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::ZERO,
        );
        // Presses outside, with another button and releases without a press are ignored.
        assert!(
            !harness
                .pointer(
                    &mut textbox,
                    PointerEventKind::Down,
                    Some(PointerButton::Left),
                    Point::new(500.0, y)
                )
                .handled
        );
        assert!(
            !harness
                .pointer(
                    &mut textbox,
                    PointerEventKind::Down,
                    Some(PointerButton::Right),
                    Point::new(50.0, y)
                )
                .handled
        );
        assert!(
            !harness
                .pointer(
                    &mut textbox,
                    PointerEventKind::Up,
                    Some(PointerButton::Left),
                    Point::new(50.0, y)
                )
                .handled
        );
        // Hover tracking: the presses above already left the box hovered, so leaving comes
        // first; moving within an already hovered box changes nothing.
        assert!(
            harness
                .pointer(&mut textbox, PointerEventKind::Leave, None, Point::ZERO)
                .handled
        );
        assert!(harness.hover(&mut textbox, Point::new(50.0, y)).handled);
        assert!(!harness.hover(&mut textbox, Point::new(50.0, y)).handled);
        assert!(
            harness
                .pointer(&mut textbox, PointerEventKind::Leave, None, Point::ZERO)
                .handled
        );
        assert!(
            !harness
                .pointer(&mut textbox, PointerEventKind::Leave, None, Point::ZERO)
                .handled
        );
        textbox.set_enabled(false);
        assert!(!harness.hover(&mut textbox, Point::new(50.0, y)).handled);
    }

    #[test]
    fn kit_textbox_scrolls_to_keep_the_caret_visible() {
        let (mut harness, mut textbox, _) = setup("");
        // 180 DIPs of text area hold 22 glyphs.
        harness.text(&mut textbox, &"x".repeat(40));
        assert_eq!(textbox.scroll, 40.0 * 8.0 - 180.0 + 2.0);
        harness.key(&mut textbox, vk::HOME);
        assert_eq!(textbox.scroll, 0.0);
        // Clicking in the scrolled text maps back through the scroll offset.
        harness.key(&mut textbox, vk::END);
        harness.pointer(
            &mut textbox,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            Point::new(20.0, 26.0),
        );
        assert_eq!(textbox.caret(), (textbox.scroll / 8.0).round() as usize);
    }

    #[test]
    fn kit_textbox_paints_placeholder_selection_caret_and_focus_line() {
        let mut harness = Harness::new();
        let mut textbox = TextBox::new("Name", "", clipboard())
            .with_placeholder("Type a name")
            .with_motion(Motion::reduced());
        harness.arrange(&mut textbox, Rect::new(10.0, 10.0, 200.0, 32.0));
        let tokens = harness.theme.tokens;
        let canvas = harness.paint(&textbox);
        assert_eq!(canvas.texts(), vec!["Type a name"]);
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::Text { color, .. } if *color == tokens.text_muted))
        );
        assert!(
            !canvas
                .ops()
                .any(|op| matches!(op, DrawOp::FillRect { color, .. } if *color == tokens.caret))
        );
        harness.focus_widget(&mut textbox);
        let canvas = harness.paint(&textbox);
        // The focused empty box shows the accent line under it, and the placeholder stays.
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::FillRect { rect, color } if *color == tokens.accent && rect.height == 2.0 && rect.y == 40.0)));
        harness.text(&mut textbox, "abc");
        let canvas = harness.paint(&textbox);
        assert_eq!(canvas.texts(), vec!["abc"]);
        // The caret is a 1 DIP wide bar after the text.
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::FillRect { rect, color }
            if *color == tokens.caret && rect.width == 1.0 && rect.x == 20.0 + 24.0))
        );
        // A selection is drawn behind the text and the caret hides.
        textbox.set_selection(1, 3);
        let canvas = harness.paint(&textbox);
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::FillRect { rect, color }
            if *color == tokens.selection_bg && rect.x == 28.0 && rect.width == 16.0))
        );
        assert!(
            !canvas
                .ops()
                .any(|op| matches!(op, DrawOp::FillRect { color, .. } if *color == tokens.caret))
        );
        // Without focus the selection uses the inactive color and there is no accent line.
        harness.focus.blur();
        let canvas = harness.paint(&textbox);
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::FillRect { color, .. } if *color == tokens.selection_inactive_bg)));
        assert!(
            !canvas
                .ops()
                .any(|op| matches!(op, DrawOp::FillRect { color, .. } if *color == tokens.accent))
        );
        // Disabled text is grey; damage elsewhere skips painting.
        textbox.set_enabled(false);
        let canvas = harness.paint(&textbox);
        assert!(
            canvas.ops().any(
                |op| matches!(op, DrawOp::Text { color, .. } if *color == tokens.text_disabled)
            )
        );
        let mut skipped = crate::canvas::RecordingCanvas::new();
        let cx = PaintCx::new(
            textbox.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &harness.focus,
            &harness.capture,
            Some(Rect::new(0.0, 500.0, 5.0, 5.0)),
        );
        textbox.paint(&cx, &mut skipped);
        assert!(skipped.recorded().is_empty());
    }

    #[test]
    fn kit_textbox_access_node_value_and_actions() {
        let (mut harness, mut textbox, _) = setup("abc");
        let node = harness.node(&textbox);
        assert_eq!((node.role, node.name.as_str()), (Role::Edit, "Name"));
        assert_eq!(node.value.as_deref(), Some("abc"));
        assert!(node.patterns.value && !node.state.read_only && !node.state.password);
        assert!(node.state.focused && node.state.focusable);
        assert!(harness.perform(&mut textbox, AccessAction::SetValue("xyz!".into())));
        assert_eq!((textbox.text(), textbox.caret()), ("xyz!", 4));
        assert!(textbox.take_changed());
        assert!(harness.perform(&mut textbox, AccessAction::Focus));
        assert!(!harness.perform(&mut textbox, AccessAction::Invoke));
        textbox.set_enabled(false);
        let bounds = textbox.bounds();
        harness.arrange(&mut textbox, bounds);
        assert!(!harness.perform(&mut textbox, AccessAction::SetValue("no".into())));
        assert!(!harness.perform(&mut textbox, AccessAction::Focus));
        assert!(harness.node(&textbox).state.disabled);
        assert!(!textbox.is_enabled());
        assert!(!harness.key(&mut textbox, vk::BACK).handled);
        assert!(format!("{textbox:?}").starts_with("TextBox"));
        let mut measuring = TextBox::new("m", "", clipboard());
        assert_eq!(
            harness.place(&mut measuring, Point::ZERO).size(),
            Size::new(200.0, 32.0)
        );
        harness.send(
            &mut measuring,
            Event::AnimationFrame(std::time::Duration::from_millis(1)),
        );
    }
}
