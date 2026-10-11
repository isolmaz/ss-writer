//! Input handling and commands of [`EditorView`]: keyboard, text, IME, pointer, wheel and
//! [`EditorView::execute`]. Text changes only through `ssw_core::ops` and `Document::apply`.

use ssw_core::command::CommandId as C;
use ssw_core::edit::{Edit, Selection, Transaction};
use ssw_core::markdown::ast::{Alignment, CalloutKind, HighlightColor};
use ssw_core::ops::format::{InlineStyle, LinkInfo};
use ssw_core::ops::lists::ListKind;
use ssw_core::ops::movement::{self, Movement};
use ssw_core::ops::{Ctx, blocks, format, lists, paste, table, tasks, typing};
use ssw_core::text::TextRange;
use ssw_core::undo::Timestamp;

use crate::geometry::{Point, Rect};
use crate::input::{
    Cursor, Event, FocusEvent, ImeEvent, KeyEvent, PointerButton, PointerEvent, PointerEventKind,
    WheelEvent, vk,
};
use crate::services::{ClipboardContent, TimerId};
use crate::widget::{EventCx, EventResult};

use super::block::{BlockEdit, BlockPress, GutterAction};
use ssw_core::settings::ListIndent;

use super::config::{Composition, DocumentKind, EditorEvent, LinkTarget};
use super::modes::{autoscroll_speed, step_zoom};
use super::view::{
    AUTOSCROLL_INTERVAL, AUTOSCROLL_MAX_STEP, AutoScroll, BLINK, BlockDragState, Drag, DragUnit,
    EditorView, Env, Hit, ScrollIntent, TEXT_DRAG_THRESHOLD, TOP_PAD, TextDrag,
};

const KEY_A: u16 = 0x41;
const KEY_C: u16 = 0x43;
const KEY_V: u16 = 0x56;
const KEY_X: u16 = 0x58;
const KEY_Y: u16 = 0x59;
const KEY_Z: u16 = 0x5A;

/// Replaces the selection with `text` and puts the caret after it: typing without Markdown
/// behaviors (plain-text and Mermaid documents).
fn plain_insert(ctx: Ctx<'_>, text: &str) -> Transaction {
    let range = ctx.selection.range();
    ctx.transaction(
        vec![Edit::replace(range, text)],
        Selection::caret(range.start + text.len()),
    )
}

/// Backspace / Delete (by character or word) that only deletes text.
fn plain_delete(ctx: Ctx<'_>, word: bool, forward: bool) -> Option<Transaction> {
    let selection = ctx.selection;
    let range = if selection.is_caret() {
        let movement = match (word, forward) {
            (true, true) => Movement::WordForward,
            (true, false) => Movement::WordBackward,
            (false, true) => Movement::CharForward,
            (false, false) => Movement::CharBackward,
        };
        let target = movement::target(ctx.buffer, selection.head, movement);
        TextRange::new(selection.head.min(target), selection.head.max(target))
    } else {
        selection.range()
    };
    (!range.is_empty())
        .then(|| ctx.transaction(vec![Edit::delete(range)], Selection::caret(range.start)))
}

/// Tab inserts `unit`; Shift+Tab removes one leading tab (or up to `unit` spaces) from the
/// caret line.
fn plain_tab(ctx: Ctx<'_>, back: bool, unit: &str) -> Option<Transaction> {
    if !back {
        return Some(plain_insert(ctx, unit));
    }
    let line = ctx.line_of(ctx.selection.head);
    let start = ctx.line_range(line).start;
    let text = ctx.line_text(line);
    let strip = if text.starts_with('\t') {
        1
    } else {
        text.bytes()
            .take(unit.len().min(4))
            .take_while(|&b| b == b' ')
            .count()
    };
    (strip > 0)
        .then(|| ctx.transaction_mapped(vec![Edit::delete(TextRange::new(start, start + strip))]))
}

impl EditorView {
    // ----- event routing --------------------------------------------------------------------

    pub(super) fn handle_event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let env = Env {
            text: cx.text,
            theme: cx.theme,
            scale: cx.scale,
        };
        let result = match e {
            Event::Focus(focus) => self.on_focus(cx, *focus),
            Event::Timer(id) => self.on_timer(&env, *id),
            Event::Key(key) if cx.is_focused() && key.is_down => self.on_key(&env, key),
            Event::Text(text) if cx.is_focused() => self.on_text(&text.0),
            Event::Ime(ime) if cx.is_focused() => self.on_ime(ime),
            Event::Pointer(pointer) => self.on_pointer(cx, &env, pointer),
            Event::Wheel(wheel) => self.on_wheel(wheel),
            _ => EventResult::ignored(),
        };
        if result.handled {
            let resets_blink = matches!(
                e,
                Event::Key(_) | Event::Text(_) | Event::Ime(_) | Event::Pointer(_)
            );
            self.after_input(cx, resets_blink);
        }
        result
    }

    fn after_input(&mut self, cx: &mut EventCx, resets_blink: bool) {
        let selection = self.doc.borrow().primary_selection();
        if selection != self.last_selection {
            self.last_selection = selection;
            self.events.push(EditorEvent::SelectionChanged(selection));
        }
        if resets_blink && self.focused {
            self.caret_on = true;
            if let Some(id) = self.blink_timer {
                cx.set_timer(id, BLINK, true);
            }
        }
        cx.invalidate(self.bounds);
    }

    fn on_focus(&mut self, cx: &mut EventCx, focus: FocusEvent) -> EventResult {
        match focus {
            FocusEvent::Gained => {
                self.focused = true;
                self.caret_on = true;
                if self.blink_enabled && self.blink_timer.is_none() {
                    self.blink_timer = Some(cx.start_timer(BLINK, true));
                }
            }
            FocusEvent::Lost => {
                self.focused = false;
                self.drag = None;
                self.text_drag = None;
                self.drop_caret = None;
                self.stop_autoscroll(cx);
                self.block_drag = None;
                if let Some(id) = self.blink_timer.take() {
                    cx.kill_timer(id);
                }
                self.doc.borrow_mut().break_undo_group();
            }
        }
        EventResult::handled()
    }

    fn on_timer(&mut self, env: &Env<'_>, id: TimerId) -> EventResult {
        if let Some(state) = self.autoscroll
            && state.timer == id
        {
            return self.autoscroll_tick(env, state);
        }
        // The blink timer runs exactly while the view is focused (`on_focus` starts and kills it).
        if self.blink_timer != Some(id) {
            return EventResult::ignored();
        }
        self.caret_on = !self.caret_on;
        EventResult::handled()
    }

    // ----- text input -----------------------------------------------------------------------

    pub(super) fn caret_intent(&self) -> ScrollIntent {
        if self.typewriter {
            ScrollIntent::Typewriter
        } else {
            ScrollIntent::Visible
        }
    }

    /// Applies `tx` to the document; `typing` joins it to the current undo group when it
    /// continues a typing run. Returns whether the document accepted it.
    pub(super) fn apply(&mut self, tx: Transaction, typing: bool) -> bool {
        if self.read_only {
            return false;
        }
        let millis = u64::try_from(self.clock.now().as_millis()).unwrap_or(u64::MAX);
        let result = {
            let mut doc = self.doc.borrow_mut();
            if typing {
                doc.apply_typing(tx, Timestamp::from_millis(millis))
            } else {
                doc.apply(tx)
            }
        };
        if result.is_err() {
            return false;
        }
        self.edited();
        true
    }

    fn edited(&mut self) {
        self.goal_x = None;
        self.events.push(EditorEvent::Edited);
        let intent = self.caret_intent();
        self.request_scroll(intent);
    }

    /// Runs an editing operation over the current context and applies its transaction as one
    /// undo step.
    fn run(&mut self, op: impl FnOnce(Ctx<'_>) -> Option<Transaction>) {
        let tx = {
            let doc = self.doc.borrow();
            op(doc.ctx())
        };
        if let Some(tx) = tx {
            self.apply(tx, false);
        }
    }

    fn on_text(&mut self, text: &str) -> EventResult {
        if text.is_empty()
            || text
                .chars()
                .any(|c| c.is_control() && c != '\t' && c != '\n')
        {
            return EventResult::ignored();
        }
        let tx = self.insert_tx(text);
        self.apply(tx, true);
        EventResult::handled()
    }

    fn is_markdown(&self) -> bool {
        self.config.document_kind == DocumentKind::Markdown
    }

    /// The transaction that types `text`: Markdown typing behaviors in a Markdown document,
    /// a plain replacement of the selection otherwise.
    fn insert_tx(&self, text: &str) -> Transaction {
        let doc = self.doc.borrow();
        if self.is_markdown() {
            typing::insert_text(doc.ctx(), text)
        } else {
            plain_insert(doc.ctx(), text)
        }
    }

    fn on_ime(&mut self, ime: &ImeEvent) -> EventResult {
        match ime {
            ImeEvent::Start => {}
            ImeEvent::Update { text, caret, attrs } => {
                let replaces = !self.doc.borrow().primary_selection().is_caret();
                if replaces {
                    let tx = {
                        let doc = self.doc.borrow();
                        if self.is_markdown() {
                            typing::backspace(doc.ctx())
                                .expect("backspace over a selection always deletes it")
                        } else {
                            plain_insert(doc.ctx(), "")
                        }
                    };
                    self.apply(tx, true);
                }
                let at = self.doc.borrow().primary_selection().head;
                self.composition = Some(Composition {
                    at,
                    text: text.clone(),
                    caret: *caret,
                    attrs: attrs.clone(),
                });
                self.epoch += 1;
                let intent = self.caret_intent();
                self.request_scroll(intent);
            }
            ImeEvent::Commit(text) => {
                self.composition = None;
                self.epoch += 1;
                if !text.is_empty() {
                    let tx = self.insert_tx(text);
                    self.apply(tx, true);
                }
            }
            ImeEvent::End => {
                if self.composition.take().is_some() {
                    self.epoch += 1;
                }
            }
        }
        EventResult::handled()
    }

    // ----- keyboard -------------------------------------------------------------------------

    fn on_key(&mut self, env: &Env<'_>, key: &KeyEvent) -> EventResult {
        let ctrl = key.mods.ctrl() && !key.mods.alt();
        let shift = key.mods.shift();
        let handled = match key.vk {
            vk::LEFT => {
                let movement = if ctrl {
                    Movement::WordBackward
                } else {
                    Movement::CharBackward
                };
                self.move_by(movement, shift);
                true
            }
            vk::RIGHT => {
                let movement = if ctrl {
                    Movement::WordForward
                } else {
                    Movement::CharForward
                };
                self.move_by(movement, shift);
                true
            }
            vk::UP | vk::DOWN => {
                let down = key.vk == vk::DOWN;
                if ctrl {
                    let movement = if down {
                        Movement::ParagraphForward
                    } else {
                        Movement::ParagraphBackward
                    };
                    self.move_by(movement, shift);
                } else {
                    self.move_vertical(env, down, false, shift);
                }
                true
            }
            vk::HOME | vk::END => {
                let end = key.vk == vk::END;
                if ctrl {
                    let movement = if end {
                        Movement::DocumentEnd
                    } else {
                        Movement::DocumentStart
                    };
                    self.move_by(movement, shift);
                } else {
                    self.move_to_line_edge(env, end, shift);
                }
                true
            }
            vk::PAGE_UP | vk::PAGE_DOWN => {
                self.move_vertical(env, key.vk == vk::PAGE_DOWN, true, shift);
                true
            }
            vk::BACK | vk::DELETE => {
                self.delete_key(ctrl, key.vk == vk::DELETE);
                true
            }
            vk::RETURN => {
                self.enter(shift);
                true
            }
            vk::TAB => {
                self.tab(shift);
                true
            }
            vk::ESCAPE => {
                let selection = self.doc.borrow().primary_selection();
                if selection.is_caret() {
                    false
                } else {
                    self.select(selection.collapsed());
                    true
                }
            }
            KEY_A if ctrl && !shift => self.execute(C::EditSelectAll),
            KEY_C if ctrl && !shift => self.execute(C::EditCopy),
            KEY_X if ctrl && !shift => self.execute(C::EditCut),
            KEY_V if ctrl => self.execute(if shift {
                C::EditPastePlain
            } else {
                C::EditPaste
            }),
            KEY_Z if ctrl => self.execute(if shift { C::EditRedo } else { C::EditUndo }),
            KEY_Y if ctrl && !shift => self.execute(C::EditRedo),
            _ => false,
        };
        if handled {
            EventResult::handled()
        } else {
            EventResult::ignored()
        }
    }

    /// Moves the selection to `selection` (a user navigation: ends the typing group and
    /// re-aligns a table the caret left).
    pub(super) fn select(&mut self, selection: Selection) {
        let previous = self.doc.borrow().primary_selection();
        {
            let mut doc = self.doc.borrow_mut();
            doc.set_selections(selection, &[]);
            doc.break_undo_group();
        }
        if self.config.format_tables_on_exit {
            let tx = {
                let doc = self.doc.borrow();
                table::format_after_leaving(doc.ctx(), previous.head)
            };
            if let Some(tx) = tx {
                self.apply(tx, false);
            }
        }
    }

    fn move_by(&mut self, movement: Movement, extend: bool) {
        let next = {
            let doc = self.doc.borrow();
            movement::move_selection(doc.buffer(), doc.primary_selection(), movement, extend)
        };
        self.select(next);
        self.goal_x = None;
        let intent = self.caret_intent();
        self.request_scroll(intent);
    }

    /// The source offset at content x/y (the caret column `goal` and a probe line), clamped to
    /// the document at both ends.
    fn offset_at_content(&self, env: &Env<'_>, goal: f32, probe_y: f32) -> usize {
        let total = TOP_PAD + self.cache.borrow().heights.total() as f32;
        if probe_y < TOP_PAD {
            return 0;
        }
        let end = self.doc.borrow().buffer().len();
        if probe_y >= total {
            return end;
        }
        let (left, _) = self.column();
        let scroll = self.cache.borrow().scroll_y;
        let point = Point::new(left + goal, self.bounds.y + probe_y - scroll);
        // Only a point that rounds past the last block misses a layout.
        self.hit_window(env, point).map_or(end, |hit| hit.offset)
    }

    /// Up/Down by one visual line, or by a page.
    fn move_vertical(&mut self, env: &Env<'_>, down: bool, page: bool, extend: bool) {
        self.settle(env);
        let Some(caret) = self.caret_content_rect(env) else {
            return;
        };
        let goal = *self.goal_x.get_or_insert(caret.x);
        let line = caret.height.max(1.0);
        let step = if page {
            (self.bounds.height - line).max(line)
        } else {
            line
        };
        let probe_y = match (page, down) {
            (true, true) => caret.y + step + line / 2.0,
            (true, false) => caret.y - step + line / 2.0,
            (false, true) => caret.bottom() + 1.0,
            (false, false) => caret.y - 1.0,
        };
        if page {
            let cache = self.cache.get_mut();
            cache.scroll_y = (cache.scroll_y + if down { step } else { -step }).max(0.0);
        }
        let target = self.offset_at_content(env, goal, probe_y);
        let selection = {
            let current = self.doc.borrow().primary_selection();
            if extend {
                current.with_head(target)
            } else {
                Selection::caret(target)
            }
        };
        self.select(selection);
        if !page {
            let intent = self.caret_intent();
            self.request_scroll(intent);
        }
    }

    /// Home/End: the start/end of the visual line.
    fn move_to_line_edge(&mut self, env: &Env<'_>, end: bool, extend: bool) {
        self.settle(env);
        let Some((_, layout)) = self.caret_layout(env) else {
            return;
        };
        let (current, head) = {
            let doc = self.doc.borrow();
            let current = doc.primary_selection();
            (current, current.head)
        };
        let line = layout.line_range(head);
        let target = if end { line.end } else { line.start };
        let selection = if extend {
            current.with_head(target)
        } else {
            Selection::caret(target)
        };
        self.select(selection);
        self.goal_x = None;
        let intent = self.caret_intent();
        self.request_scroll(intent);
    }

    fn delete_key(&mut self, word: bool, forward: bool) {
        if !self.is_markdown() {
            let tx = plain_delete(self.doc.borrow().ctx(), word, forward);
            if let Some(tx) = tx {
                self.apply(tx, true);
            }
            return;
        }
        let tx = {
            let doc = self.doc.borrow();
            let ctx = doc.ctx();
            if word && ctx.selection.is_caret() {
                let movement = if forward {
                    Movement::WordForward
                } else {
                    Movement::WordBackward
                };
                let target = movement::target(ctx.buffer, ctx.selection.head, movement);
                let span = Ctx::new(
                    ctx.buffer,
                    ctx.ast,
                    Selection::new(ctx.selection.head, target),
                );
                if forward {
                    typing::delete_forward(span)
                } else {
                    typing::backspace(span)
                }
            } else if forward {
                typing::delete_forward(ctx)
            } else {
                typing::backspace(ctx)
            }
        };
        if let Some(tx) = tx {
            self.apply(tx, true);
        }
    }

    fn enter(&mut self, soft: bool) {
        let tx = {
            let doc = self.doc.borrow();
            let ctx = doc.ctx();
            if !self.is_markdown() {
                plain_insert(ctx, "\n")
            } else if soft {
                typing::shift_enter(ctx)
            } else {
                table::move_down(ctx).unwrap_or_else(|| typing::enter(ctx))
            }
        };
        self.apply(tx, true);
    }

    fn tab(&mut self, back: bool) {
        let indent = self.config.list_indent;
        if !self.is_markdown() {
            let unit = match indent {
                ListIndent::Two => "  ",
                ListIndent::Four => "    ",
                ListIndent::Align | ListIndent::Tab => "\t",
            };
            self.run(|ctx| plain_tab(ctx, back, unit));
            return;
        }
        self.run(|ctx| {
            if back {
                table::prev_cell(ctx).or_else(|| lists::outdent_items(ctx))
            } else {
                table::next_cell(ctx)
                    .or_else(|| lists::indent_items(ctx, indent))
                    .or_else(|| (!lists::in_item(ctx)).then(|| typing::insert_text(ctx, "\t")))
            }
        });
    }

    // ----- pointer --------------------------------------------------------------------------

    fn on_pointer(
        &mut self,
        cx: &mut EventCx,
        env: &Env<'_>,
        pointer: &PointerEvent,
    ) -> EventResult {
        match pointer.kind {
            PointerEventKind::Down if pointer.button == Some(PointerButton::Left) => {
                self.pointer_down(cx, env, pointer)
            }
            PointerEventKind::Down if pointer.button == Some(PointerButton::Right) => {
                self.context_click(cx, env, pointer)
            }
            PointerEventKind::Move => self.pointer_move(cx, env, pointer),
            PointerEventKind::Up if pointer.button == Some(PointerButton::Left) => {
                match self.block_drag.take() {
                    Some(state) => self.block_drag_up(cx, state, pointer),
                    None => self.pointer_up(cx, env, pointer),
                }
            }
            PointerEventKind::Leave => {
                if self.hovered_item.take().is_some() {
                    cx.invalidate(self.bounds);
                }
                EventResult::ignored()
            }
            _ => EventResult::ignored(),
        }
    }

    fn pointer_down(
        &mut self,
        cx: &mut EventCx,
        env: &Env<'_>,
        pointer: &PointerEvent,
    ) -> EventResult {
        if !self.bounds.contains(pointer.position) {
            return EventResult::ignored();
        }
        let Some(hit) = self.hit_window(env, pointer.position) else {
            return EventResult::handled_with_focus();
        };
        let (left, _) = self.column();
        let origin = Point::new(
            left,
            self.bounds.y + self.item_top(hit.item) - self.cache.borrow().scroll_y,
        );
        if let Some(press) = hit.layout.press(hit.local, origin, pointer.mods) {
            self.block_press(cx, hit.item, press);
            return EventResult::handled_with_focus();
        }
        if let Some(task) = hit.layout.task_box_at(hit.local) {
            self.toggle_task_at(task.offset);
            return EventResult::handled_with_focus();
        }
        if pointer.position.x < left
            && let Some(action) = hit.layout.gutter_hit(hit.local)
        {
            self.gutter_click(&hit, action);
            return EventResult::handled_with_focus();
        }
        match hit.layout.link_at(hit.local) {
            Some(target) if pointer.mods.ctrl() => {
                self.events.push(EditorEvent::LinkActivated(target));
                return EventResult::handled_with_focus();
            }
            Some(LinkTarget::Footnote(label)) if !pointer.mods.shift() => {
                let definition = self.doc.borrow().ast().definitions.footnote(&label);
                if let Some(definition) = definition {
                    self.select(Selection::caret(definition.start));
                    self.request_scroll(ScrollIntent::Visible);
                    return EventResult::handled_with_focus();
                }
            }
            _ => {}
        }
        if pointer.click_count.max(1) == 1
            && !pointer.mods.shift()
            && self.begin_text_drag(cx, &hit, pointer.position)
        {
            return EventResult::handled_with_focus();
        }
        let current = self.doc.borrow().primary_selection();
        let (selection, drag) = match pointer.click_count.max(1) {
            1 => {
                if pointer.mods.shift() {
                    (
                        current.with_head(hit.offset),
                        Drag {
                            unit: DragUnit::Char,
                            anchor: TextRange::at(current.anchor),
                        },
                    )
                } else {
                    (
                        Selection::caret(hit.offset),
                        Drag {
                            unit: DragUnit::Char,
                            anchor: TextRange::at(hit.offset),
                        },
                    )
                }
            }
            2 => {
                let word = movement::select_word(self.doc.borrow().buffer(), hit.offset);
                (
                    Selection::from_range(word),
                    Drag {
                        unit: DragUnit::Word,
                        anchor: word,
                    },
                )
            }
            _ => {
                let paragraph = movement::select_paragraph(self.doc.borrow().buffer(), hit.offset);
                (
                    Selection::from_range(paragraph),
                    Drag {
                        unit: DragUnit::Paragraph,
                        anchor: paragraph,
                    },
                )
            }
        };
        self.select(selection);
        self.goal_x = None;
        self.drag = Some(drag);
        cx.capture_pointer();
        EventResult::handled_with_focus()
    }

    /// Carries out what a block asked for in answer to a press on it.
    fn block_press(&mut self, cx: &mut EventCx, item: usize, press: BlockPress) {
        match press {
            BlockPress::Event(event) => self.events.push(event),
            BlockPress::Edit(edit) => self.block_edit(edit),
            BlockPress::Drag(drag) => {
                self.block_drag = Some(BlockDragState { item, drag });
                cx.capture_pointer();
            }
        }
    }

    /// Applies a block's replacement as one undo step; the selection is mapped through it.
    fn block_edit(&mut self, edit: BlockEdit) {
        let tx = self
            .doc
            .borrow()
            .ctx()
            .transaction_mapped(vec![Edit::replace(edit.range, edit.text)]);
        self.apply(tx, false);
    }

    /// `position` (window DIPs) relative to the top-left corner of item `item`.
    fn block_local(&self, item: usize, position: Point) -> Point {
        let (left, _) = self.column();
        let top = self.item_top(item) - self.cache.borrow().scroll_y + self.bounds.y;
        Point::new(position.x - left, position.y - top)
    }

    /// The pointer was released while a block drag was running: finish it and apply its edit.
    fn block_drag_up(
        &mut self,
        cx: &mut EventCx,
        state: BlockDragState,
        pointer: &PointerEvent,
    ) -> EventResult {
        cx.release_pointer();
        let local = self.block_local(state.item, pointer.position);
        if let Some(edit) = state.drag.finish(local) {
            self.block_edit(edit);
        }
        EventResult::handled()
    }

    fn gutter_click(&mut self, hit: &Hit, action: GutterAction) {
        let range = self.cache.borrow().items[hit.item].range;
        match action {
            GutterAction::ToggleFold => self.events.push(EditorEvent::ToggleFold {
                heading_start: range.start,
            }),
            GutterAction::HeadingMenu { level } => {
                let (left, _) = self.column();
                let top = self.bounds.y + self.item_top(hit.item) - self.cache.borrow().scroll_y;
                self.events.push(EditorEvent::HeadingMenu {
                    range,
                    level,
                    anchor: Rect::new(left - 60.0, top, 36.0, 24.0),
                });
            }
        }
    }

    fn pointer_move(
        &mut self,
        cx: &mut EventCx,
        env: &Env<'_>,
        pointer: &PointerEvent,
    ) -> EventResult {
        if let Some(mut state) = self.block_drag.take() {
            let local = self.block_local(state.item, pointer.position);
            state.drag.moved(local);
            cx.set_cursor(state.drag.cursor());
            self.block_drag = Some(state);
            cx.invalidate(self.bounds);
            return EventResult::handled();
        }
        if let Some(drag) = self.text_drag
            && cx.has_pointer_capture()
        {
            return self.text_drag_move(cx, env, pointer, drag);
        }
        if let Some(drag) = self.drag
            && cx.has_pointer_capture()
        {
            self.extend_drag(env, drag, pointer.position);
            self.update_autoscroll(cx, pointer.position);
            return EventResult::handled();
        }
        if !self.bounds.contains(pointer.position) {
            if self.hovered_item.take().is_some() {
                cx.invalidate(self.bounds);
            }
            return EventResult::ignored();
        }
        let hit = self.hit_window(env, pointer.position);
        let hovered = hit.as_ref().map(|hit| hit.item);
        if hovered != self.hovered_item {
            self.hovered_item = hovered;
            cx.invalidate(self.bounds);
        }
        let (left, _) = self.column();
        let block_cursor = hit.as_ref().and_then(|hit| hit.layout.cursor_at(hit.local));
        let cursor = block_cursor.unwrap_or_else(|| match &hit {
            Some(hit) if pointer.position.x < left => {
                if hit.layout.gutter_hit(hit.local).is_some() {
                    Cursor::Hand
                } else {
                    Cursor::Arrow
                }
            }
            Some(hit) if hit.layout.task_box_at(hit.local).is_some() => Cursor::Hand,
            Some(hit) if pointer.mods.ctrl() && hit.layout.link_at(hit.local).is_some() => {
                Cursor::Hand
            }
            _ => Cursor::IBeam,
        });
        cx.set_cursor(cursor);
        EventResult::handled()
    }

    /// Extends the selection drag `drag` to the pointer at `position` (window DIPs). The probe
    /// stays inside the viewport, so a pointer beyond an edge selects line by line as the
    /// auto-scroll moves the content under it.
    fn extend_drag(&mut self, env: &Env<'_>, drag: Drag, position: Point) {
        let bottom = (self.bounds.bottom() - 1.0).max(self.bounds.y);
        let probe = Point::new(position.x, position.y.clamp(self.bounds.y, bottom));
        let Some(hit) = self.hit_window(env, probe) else {
            return;
        };
        let unit = {
            let doc = self.doc.borrow();
            match drag.unit {
                DragUnit::Char => TextRange::at(hit.offset),
                DragUnit::Word => movement::select_word(doc.buffer(), hit.offset),
                DragUnit::Paragraph => movement::select_paragraph(doc.buffer(), hit.offset),
            }
        };
        let selection = if unit.start < drag.anchor.start {
            Selection::new(drag.anchor.end, unit.start)
        } else {
            Selection::new(drag.anchor.start, unit.end)
        };
        if selection != self.doc.borrow().primary_selection() {
            self.select(selection);
        }
    }

    // ----- edge auto-scroll -----------------------------------------------------------------

    /// Starts, steers or stops the auto-scroll timer of a running drag: it runs while the pointer
    /// is within [`autoscroll_speed`]'s edge zones (or beyond the edges) and records the
    /// pointer for the next tick.
    fn update_autoscroll(&mut self, cx: &mut EventCx, position: Point) {
        if autoscroll_speed(position.y, self.bounds.y, self.bounds.bottom()) == 0.0 {
            self.stop_autoscroll(cx);
            return;
        }
        match &mut self.autoscroll {
            Some(state) => state.pointer = position,
            None => {
                let timer = cx.start_timer(AUTOSCROLL_INTERVAL, true);
                self.autoscroll = Some(AutoScroll {
                    timer,
                    last_tick: self.clock.now(),
                    pointer: position,
                });
            }
        }
    }

    fn stop_autoscroll(&mut self, cx: &mut EventCx) {
        if let Some(state) = self.autoscroll.take() {
            cx.kill_timer(state.timer);
        }
    }

    /// One auto-scroll step: scrolls by speed × elapsed clock time and extends the drag to the
    /// pointer's last position.
    fn autoscroll_tick(&mut self, env: &Env<'_>, state: AutoScroll) -> EventResult {
        let now = self.clock.now();
        let elapsed = now.saturating_sub(state.last_tick).min(AUTOSCROLL_MAX_STEP);
        self.autoscroll = Some(AutoScroll {
            last_tick: now,
            ..state
        });
        let speed = autoscroll_speed(state.pointer.y, self.bounds.y, self.bounds.bottom());
        self.settle(env);
        let cache = self.cache.get_mut();
        cache.scroll_y = (cache.scroll_y + speed * elapsed.as_secs_f32()).max(0.0);
        if let Some(drag) = self.drag {
            self.extend_drag(env, drag, state.pointer);
        } else {
            // The timer starts with a started text drag and stops with every drag.
            let drag = self
                .text_drag
                .expect("an auto-scroll runs only while a drag does");
            self.retarget_text_drag(env, drag, state.pointer, drag.copy);
        }
        EventResult::handled()
    }

    // ----- drag and drop of the selected text -----------------------------------------------

    /// Starts dragging the selected text when the press lands on it (spec §2.4). The drag
    /// begins once the pointer moves past a few DIPs; a release before that is a plain click
    /// that puts the caret there.
    fn begin_text_drag(&mut self, cx: &mut EventCx, hit: &Hit, position: Point) -> bool {
        if self.read_only {
            return false;
        }
        let (selection, item_range) = {
            let doc = self.doc.borrow();
            let cache = self.cache.borrow();
            (doc.primary_selection(), cache.items[hit.item].range)
        };
        let on_selection = selection
            .range()
            .intersection(item_range)
            .is_some_and(|shared| {
                hit.layout
                    .selection_rects(shared)
                    .iter()
                    .any(|rect| rect.contains(hit.local))
            });
        if selection.is_caret() || !on_selection {
            return false;
        }
        self.text_drag = Some(TextDrag {
            source: selection.range(),
            press: position,
            click: hit.offset,
            started: false,
            copy: false,
        });
        cx.capture_pointer();
        true
    }

    /// Where dropping `source` at the window point `position` would put it: the source offset
    /// there, or `None` outside the editor or inside the dragged text itself (a move onto its
    /// own edges changes nothing; a copy may be dropped on them).
    fn drop_target_at(
        &self,
        env: &Env<'_>,
        position: Point,
        source: TextRange,
        copy: bool,
    ) -> Option<usize> {
        if !self.bounds.contains(position) {
            return None;
        }
        let offset = self.hit_window(env, position)?.offset;
        let outside = if copy {
            offset <= source.start || offset >= source.end
        } else {
            offset < source.start || offset > source.end
        };
        outside.then_some(offset)
    }

    fn retarget_text_drag(&mut self, env: &Env<'_>, drag: TextDrag, position: Point, copy: bool) {
        self.drop_caret = self.drop_target_at(env, position, drag.source, copy);
        self.text_drag = Some(TextDrag { copy, ..drag });
    }

    fn text_drag_move(
        &mut self,
        cx: &mut EventCx,
        env: &Env<'_>,
        pointer: &PointerEvent,
        drag: TextDrag,
    ) -> EventResult {
        let (dx, dy) = (
            pointer.position.x - drag.press.x,
            pointer.position.y - drag.press.y,
        );
        if !drag.started && dx * dx + dy * dy < TEXT_DRAG_THRESHOLD * TEXT_DRAG_THRESHOLD {
            return EventResult::handled();
        }
        let copy = pointer.mods.ctrl();
        self.retarget_text_drag(
            env,
            TextDrag {
                started: true,
                ..drag
            },
            pointer.position,
            copy,
        );
        if self.bounds.contains(pointer.position) {
            self.update_autoscroll(cx, pointer.position);
        } else {
            self.stop_autoscroll(cx);
        }
        let inside = self.bounds.contains(pointer.position);
        cx.set_cursor(if self.drop_caret.is_some() || !inside {
            Cursor::Arrow
        } else {
            Cursor::NotAllowed
        });
        cx.invalidate(self.bounds);
        EventResult::handled()
    }

    /// The pointer was released while the selected text was being dragged: moves or copies it
    /// (one undo step), reports a drop outside the editor, or places the caret for a click.
    fn finish_text_drag(
        &mut self,
        cx: &mut EventCx,
        env: &Env<'_>,
        pointer: &PointerEvent,
        drag: TextDrag,
    ) -> EventResult {
        self.drop_caret = None;
        self.stop_autoscroll(cx);
        cx.release_pointer();
        if !drag.started {
            self.select(Selection::caret(drag.click));
            self.goal_x = None;
            return EventResult::handled();
        }
        let copy = pointer.mods.ctrl();
        if self.bounds.contains(pointer.position) {
            if let Some(target) = self.drop_target_at(env, pointer.position, drag.source, copy) {
                self.move_text(drag.source, target, copy);
            }
        } else {
            let text = self.doc.borrow().buffer().slice(drag.source).into_owned();
            self.events.push(EditorEvent::TextDroppedOutside {
                text,
                source: drag.source,
                copy,
                position: pointer.position,
            });
        }
        EventResult::handled()
    }

    /// Moves (or with `copy` duplicates) the text of `source` to `target` as one undo step and
    /// selects it at its new place. Returns whether the document accepted the edit.
    fn move_text(&mut self, source: TextRange, target: usize, copy: bool) -> bool {
        let (text, before) = {
            let doc = self.doc.borrow();
            let len = doc.buffer().len();
            if source.end > len || target > len {
                return false;
            }
            (
                doc.buffer().slice(source).into_owned(),
                doc.primary_selection(),
            )
        };
        let size = text.len();
        let (edits, start) = if copy {
            (vec![Edit::insert(target, text)], target)
        } else if target < source.start {
            (
                vec![Edit::insert(target, text), Edit::delete(source)],
                target,
            )
        } else {
            (
                vec![Edit::delete(source), Edit::insert(target, text)],
                target - size,
            )
        };
        let after = Selection::new(start, start + size);
        self.apply(Transaction::new(edits, before, after), false)
    }

    // ----- task checkboxes and context menus ------------------------------------------------

    /// Toggles the task item whose checkbox starts at `offset` as one undo step. The user's
    /// selection stays where it is (mapped through the edit) and the view does not scroll.
    fn toggle_task_at(&mut self, offset: usize) {
        let sort = self.config.sort_completed_tasks;
        let tx = {
            let doc = self.doc.borrow();
            let keep = doc.primary_selection();
            let ctx = Ctx::new(doc.buffer(), doc.ast(), Selection::caret(offset));
            tasks::toggle_task(ctx, sort).map(|mut tx| {
                tx.selection_before = keep;
                tx.selection_after = keep.map_through(&tx);
                tx
            })
        };
        if let Some(tx) = tx
            && self.apply(tx, false)
        {
            self.cache.get_mut().pending = None;
        }
    }

    /// A right click: the caret moves to the clicked place unless it is inside the selection,
    /// and the block under the pointer may answer with a context menu event.
    fn context_click(
        &mut self,
        cx: &mut EventCx,
        env: &Env<'_>,
        pointer: &PointerEvent,
    ) -> EventResult {
        if !self.bounds.contains(pointer.position) {
            return EventResult::ignored();
        }
        let Some(hit) = self.hit_window(env, pointer.position) else {
            return EventResult::ignored();
        };
        if !self
            .doc
            .borrow()
            .primary_selection()
            .range()
            .touches(hit.offset)
        {
            self.select(Selection::caret(hit.offset));
            self.goal_x = None;
        }
        let (left, _) = self.column();
        let origin = Point::new(
            left,
            self.bounds.y + self.item_top(hit.item) - self.cache.borrow().scroll_y,
        );
        match hit.layout.context_menu(hit.local, origin) {
            Some(event) => {
                self.events.push(event);
                EventResult::handled_with_focus()
            }
            None => {
                // Not handled, so the selection change is reported here.
                self.after_input(cx, false);
                EventResult::ignored()
            }
        }
    }

    fn pointer_up(
        &mut self,
        cx: &mut EventCx,
        env: &Env<'_>,
        pointer: &PointerEvent,
    ) -> EventResult {
        if let Some(drag) = self.text_drag.take() {
            return self.finish_text_drag(cx, env, pointer, drag);
        }
        if self.drag.take().is_none() {
            return EventResult::ignored();
        }
        self.stop_autoscroll(cx);
        cx.release_pointer();
        if !self.doc.borrow().primary_selection().is_caret()
            && let Some(anchor) = self.caret_rect()
        {
            self.events.push(EditorEvent::SelectionToolbar { anchor });
        }
        EventResult::handled()
    }

    fn on_wheel(&mut self, wheel: &WheelEvent) -> EventResult {
        if !self.bounds.contains(wheel.position) {
            return EventResult::ignored();
        }
        if wheel.mods.ctrl() {
            if wheel.delta.y != 0.0 {
                let direction = if wheel.delta.y > 0.0 { 1 } else { -1 };
                self.set_zoom_percent(step_zoom(self.config.zoom_percent, direction));
                self.events
                    .push(EditorEvent::ZoomChanged(self.config.zoom_percent));
            }
            return EventResult::handled();
        }
        let notch = if wheel.precise {
            1.0
        } else {
            3.0 * self.config.line_px()
        };
        if self.is_unwrapped() && (wheel.mods.shift() || wheel.delta.x != 0.0) {
            let amount = if wheel.delta.x != 0.0 {
                -wheel.delta.x
            } else {
                wheel.delta.y
            };
            let wanted = self.scroll_x.get() - amount * notch;
            self.scroll_x.set(wanted.clamp(0.0, self.max_scroll_x()));
            return EventResult::handled();
        }
        let delta = wheel.delta.y * notch;
        let cache = self.cache.get_mut();
        cache.scroll_y = (cache.scroll_y - delta).max(0.0);
        EventResult::handled()
    }

    // ----- commands -------------------------------------------------------------------------

    /// Runs `command` if it is an editing, formatting, table or view command of the editor.
    /// Returns whether the editor owns the command (the host should not route it elsewhere);
    /// commands that need the app (links, images, the date, find, files, copy as plain text /
    /// HTML / RTF) return `false`.
    pub fn execute(&mut self, command: C) -> bool {
        if !self.is_markdown()
            && !matches!(
                command,
                C::EditUndo
                    | C::EditRedo
                    | C::EditSelectAll
                    | C::EditCopy
                    | C::EditCopyAsMarkdown
                    | C::EditCut
                    | C::EditPaste
                    | C::EditPastePlain
                    | C::ViewTypewriterMode
                    | C::ViewFocusMode
                    | C::ViewZoomIn
                    | C::ViewZoomOut
                    | C::ViewZoomReset
                    | C::ViewRecenterCaret
            )
        {
            return false;
        }
        if let Some(level) = command.heading_level() {
            self.run(|ctx| blocks::toggle_heading(ctx, level));
            return true;
        }
        match command {
            C::EditUndo => self.undo_redo(false),
            C::EditRedo => self.undo_redo(true),
            C::EditSelectAll => {
                let all = movement::select_all(self.doc.borrow().buffer());
                self.select(all);
                true
            }
            C::EditCopy | C::EditCopyAsMarkdown => self.copy(false),
            C::EditCut => self.copy(true),
            C::EditPaste => self.paste(!self.is_markdown()),
            C::EditPastePlain => self.paste(true),
            C::FormatBold => self.toggle(InlineStyle::Bold),
            C::FormatItalic => self.toggle(InlineStyle::Italic),
            C::FormatUnderline => self.toggle(InlineStyle::Underline),
            C::FormatStrikethrough => self.toggle(InlineStyle::Strikethrough),
            C::FormatHighlight => self.toggle(InlineStyle::Highlight(None)),
            C::FormatHighlightRed => self.toggle(InlineStyle::Highlight(Some(HighlightColor::Red))),
            C::FormatHighlightOrange => {
                self.toggle(InlineStyle::Highlight(Some(HighlightColor::Orange)))
            }
            C::FormatHighlightYellow => {
                self.toggle(InlineStyle::Highlight(Some(HighlightColor::Yellow)))
            }
            C::FormatHighlightGreen => {
                self.toggle(InlineStyle::Highlight(Some(HighlightColor::Green)))
            }
            C::FormatHighlightBlue => {
                self.toggle(InlineStyle::Highlight(Some(HighlightColor::Blue)))
            }
            C::FormatHighlightPurple => {
                self.toggle(InlineStyle::Highlight(Some(HighlightColor::Purple)))
            }
            C::FormatInlineCode => self.toggle(InlineStyle::Code),
            C::FormatMath => self.toggle(InlineStyle::Math),
            C::FormatCodeBlock => self.op(blocks::toggle_code_block),
            C::FormatQuote => self.op(blocks::toggle_quote),
            C::FormatCallout => self.op(|ctx| blocks::set_callout(ctx, CalloutKind::Note)),
            C::FormatMathBlock => self.op(blocks::toggle_math_block),
            C::FormatDivider => self.op(|ctx| Some(blocks::insert_divider(ctx))),
            C::FormatFootnote => self.op(|ctx| Some(blocks::insert_footnote(ctx))),
            C::FormatPageBreak => self.op(|ctx| Some(blocks::insert_page_break(ctx))),
            C::FormatTable => self.op(|ctx| Some(blocks::insert_table(ctx))),
            C::FormatParagraph => self.op(blocks::clear_heading),
            C::FormatBulletList => self.op(|ctx| lists::toggle_list(ctx, ListKind::Bullet)),
            C::FormatNumberedList => self.op(|ctx| lists::toggle_list(ctx, ListKind::Numbered)),
            C::FormatTaskList => self.op(|ctx| lists::toggle_list(ctx, ListKind::Task)),
            C::FormatTaskToggle => {
                let sort = self.config.sort_completed_tasks;
                self.op(|ctx| tasks::toggle_task(ctx, sort))
            }
            C::TableInsertRowAbove => self.op(table::insert_row_above),
            C::TableInsertRowBelow => self.op(table::insert_row_below),
            C::TableInsertColumnLeft => self.op(table::insert_column_left),
            C::TableInsertColumnRight => self.op(table::insert_column_right),
            C::TableDeleteRow => self.op(table::delete_row),
            C::TableDeleteColumn => self.op(table::delete_column),
            C::TableAlignLeft => self.op(|ctx| table::set_alignment(ctx, Alignment::Left)),
            C::TableAlignCenter => self.op(|ctx| table::set_alignment(ctx, Alignment::Center)),
            C::TableAlignRight => self.op(|ctx| table::set_alignment(ctx, Alignment::Right)),
            C::ViewTypewriterMode => {
                self.set_typewriter(!self.typewriter);
                self.modes_changed();
                true
            }
            C::ViewFocusMode => {
                self.focus_mode = !self.focus_mode;
                self.modes_changed();
                true
            }
            C::ViewZoomIn | C::ViewZoomOut | C::ViewZoomReset => {
                let percent = match command {
                    C::ViewZoomIn => step_zoom(self.config.zoom_percent, 1),
                    C::ViewZoomOut => step_zoom(self.config.zoom_percent, -1),
                    _ => 100,
                };
                self.set_zoom_percent(percent);
                self.events
                    .push(EditorEvent::ZoomChanged(self.config.zoom_percent));
                true
            }
            C::ViewRecenterCaret => {
                self.request_scroll(ScrollIntent::Center);
                true
            }
            _ => false,
        }
    }

    fn modes_changed(&mut self) {
        self.events.push(EditorEvent::ModesChanged {
            typewriter: self.typewriter,
            focus: self.focus_mode,
        });
    }

    fn op(&mut self, op: impl FnOnce(Ctx<'_>) -> Option<Transaction>) -> bool {
        self.run(op);
        true
    }

    fn toggle(&mut self, style: InlineStyle) -> bool {
        self.op(|ctx| format::toggle_inline(ctx, style))
    }

    fn undo_redo(&mut self, redo: bool) -> bool {
        if self.read_only {
            return true;
        }
        let changed = {
            let mut doc = self.doc.borrow_mut();
            if redo { doc.redo() } else { doc.undo() }
        };
        if matches!(changed, Ok(true)) {
            self.edited();
        }
        true
    }

    fn copy(&mut self, cut: bool) -> bool {
        let Some(clipboard) = self.clipboard.clone() else {
            return false;
        };
        let (empty, text) = {
            let doc = self.doc.borrow();
            let range = doc.primary_selection().range();
            (range.is_empty(), doc.buffer().slice(range).into_owned())
        };
        if empty
            || clipboard
                .borrow_mut()
                .set(ClipboardContent::from_text(text))
                .is_err()
        {
            return false;
        }
        if cut {
            self.run(typing::backspace);
        }
        true
    }

    fn paste(&mut self, plain: bool) -> bool {
        let Some(clipboard) = self.clipboard.clone() else {
            return false;
        };
        let text = match clipboard.borrow().text() {
            Ok(Some(text)) => text,
            _ => return false,
        };
        self.run(|ctx| {
            Some(if plain {
                paste::paste_plain(ctx, &text)
            } else {
                paste::paste_text(ctx, &text)
            })
        });
        true
    }

    // ----- commands that need text from the app ---------------------------------------------

    /// Inserts `formatted` (the localized long date, `F5`) at the caret; in a Markdown document
    /// a date that would start an ordered list is escaped.
    pub fn insert_date(&mut self, formatted: &str) {
        if self.is_markdown() {
            self.run(|ctx| Some(format::insert_date(ctx, formatted)));
        } else {
            self.run(|ctx| Some(typing::insert_text(ctx, formatted)));
        }
    }

    /// Creates or updates the link at the caret/selection (`Ctrl+K` popup result).
    pub fn set_link(&mut self, text: &str, url: &str) {
        self.run(|ctx| Some(format::set_link(ctx, text, url)));
    }

    /// Replaces the selection with `![alt](dest)`.
    pub fn insert_image(&mut self, alt: &str, dest: &str) {
        self.run(|ctx| Some(format::insert_image(ctx, alt, dest)));
    }

    /// Pastes Markdown (converted HTML) with list renumbering and verbatim-block handling.
    pub fn paste_markdown(&mut self, markdown: &str) {
        self.run(|ctx| Some(paste::paste_markdown(ctx, markdown)));
    }

    /// Pastes code as a fenced block (VS Code / monospace HTML) unless the caret is already
    /// inside one.
    pub fn paste_code(&mut self, code: &str) {
        self.run(|ctx| Some(paste::paste_code(ctx, code)));
    }

    /// Whether the caret (or the whole selection) is inside an element of `style`; drives the
    /// state of the B I U pill.
    #[must_use]
    pub fn style_active(&self, style: InlineStyle) -> bool {
        format::is_style_active(self.doc.borrow().ctx(), style)
    }

    /// The link under the caret, for the link edit popup.
    #[must_use]
    pub fn link_at_caret(&self) -> Option<LinkInfo> {
        format::link_at(self.doc.borrow().ctx())
    }

    /// The selected source text.
    #[must_use]
    pub fn selected_text(&self) -> String {
        let doc = self.doc.borrow();
        doc.buffer()
            .slice(doc.primary_selection().range())
            .into_owned()
    }

    /// Replaces the selection (a programmatic move: find results, outline clicks, session
    /// restore). Scrolls the caret into view.
    pub fn go_to(&mut self, selection: Selection) {
        self.select(selection);
        self.goal_x = None;
        self.request_scroll(ScrollIntent::Visible);
    }

    /// Sets the selection without scrolling.
    pub fn set_selection(&mut self, selection: Selection) {
        self.select(selection);
        self.goal_x = None;
    }
}
