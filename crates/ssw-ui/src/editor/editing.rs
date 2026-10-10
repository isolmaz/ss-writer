//! Input handling and commands of [`EditorView`]: keyboard, text, IME, pointer, wheel and
//! [`EditorView::execute`]. Text changes only through `ssw_core::ops` and `Document::apply`.

use ssw_core::command::CommandId as C;
use ssw_core::edit::{Selection, Transaction};
use ssw_core::markdown::ast::{Alignment, BlockKind, CalloutKind, HighlightColor};
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

use super::block::GutterAction;
use super::config::{Composition, EditorEvent, LinkTarget};
use super::layout::ItemKind;
use super::modes::step_zoom;
use super::view::{BLINK, Drag, DragUnit, EditorView, Env, Hit, ScrollIntent, TOP_PAD};

const KEY_A: u16 = 0x41;
const KEY_C: u16 = 0x43;
const KEY_V: u16 = 0x56;
const KEY_X: u16 = 0x58;
const KEY_Y: u16 = 0x59;
const KEY_Z: u16 = 0x5A;

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
            Event::Timer(id) => self.on_timer(*id),
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
                if let Some(id) = self.blink_timer.take() {
                    cx.kill_timer(id);
                }
                self.doc.borrow_mut().break_undo_group();
            }
        }
        EventResult::handled()
    }

    fn on_timer(&mut self, id: TimerId) -> EventResult {
        if self.blink_timer != Some(id) {
            return EventResult::ignored();
        }
        if self.focused {
            self.caret_on = !self.caret_on;
        }
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
        let tx = {
            let doc = self.doc.borrow();
            typing::insert_text(doc.ctx(), text)
        };
        self.apply(tx, true);
        EventResult::handled()
    }

    fn on_ime(&mut self, ime: &ImeEvent) -> EventResult {
        match ime {
            ImeEvent::Start => {}
            ImeEvent::Update { text, caret, attrs } => {
                let replaces = !self.doc.borrow().primary_selection().is_caret();
                if replaces {
                    let tx = {
                        let doc = self.doc.borrow();
                        typing::backspace(doc.ctx())
                    };
                    if let Some(tx) = tx {
                        self.apply(tx, true);
                    }
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
                    let tx = {
                        let doc = self.doc.borrow();
                        typing::insert_text(doc.ctx(), text)
                    };
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
        if probe_y >= total {
            return self.doc.borrow().buffer().len();
        }
        let (left, _) = self.column();
        let scroll = self.cache.borrow().scroll_y;
        let point = Point::new(left + goal, self.bounds.y + probe_y - scroll);
        self.hit_window(env, point)
            .map_or_else(|| self.doc.borrow().buffer().len(), |hit| hit.offset)
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
            if soft {
                typing::shift_enter(ctx)
            } else {
                table::move_down(ctx).unwrap_or_else(|| typing::enter(ctx))
            }
        };
        self.apply(tx, true);
    }

    fn tab(&mut self, back: bool) {
        let indent = self.config.list_indent;
        self.run(|ctx| {
            if back {
                table::prev_cell(ctx).or_else(|| lists::outdent_items(ctx))
            } else {
                table::next_cell(ctx)
                    .or_else(|| lists::indent_items(ctx, indent))
                    .or_else(|| Some(typing::insert_text(ctx, "\t")))
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
            PointerEventKind::Move => self.pointer_move(cx, env, pointer),
            PointerEventKind::Up if pointer.button == Some(PointerButton::Left) => {
                self.pointer_up(cx)
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

    fn gutter_click(&mut self, hit: &Hit, action: GutterAction) {
        let (range, level) = {
            let cache = self.cache.borrow();
            let doc = self.doc.borrow();
            let item = cache.items[hit.item];
            let level = match item.kind {
                ItemKind::Block(index) => match &doc.ast().blocks[index].kind {
                    BlockKind::Heading { level, .. } => *level,
                    _ => 1,
                },
                ItemKind::Gap { .. } => 1,
            };
            (item.range, level)
        };
        match action {
            GutterAction::ToggleFold => self.events.push(EditorEvent::ToggleFold {
                heading_start: range.start,
            }),
            GutterAction::HeadingMenu => {
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
        if let Some(drag) = self.drag
            && cx.has_pointer_capture()
        {
            if let Some(hit) = self.hit_window(env, pointer.position) {
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
        let cursor = match &hit {
            Some(hit) if pointer.position.x < left => {
                if hit.layout.gutter_hit(hit.local).is_some() {
                    Cursor::Hand
                } else {
                    Cursor::Arrow
                }
            }
            Some(hit) if pointer.mods.ctrl() && hit.layout.link_at(hit.local).is_some() => {
                Cursor::Hand
            }
            _ => Cursor::IBeam,
        };
        cx.set_cursor(cursor);
        EventResult::handled()
    }

    fn pointer_up(&mut self, cx: &mut EventCx) -> EventResult {
        if self.drag.take().is_none() {
            return EventResult::ignored();
        }
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
        let delta = if wheel.precise {
            wheel.delta.y
        } else {
            wheel.delta.y * 3.0 * self.config.line_px()
        };
        let cache = self.cache.get_mut();
        cache.scroll_y = (cache.scroll_y - delta).max(0.0);
        EventResult::handled()
    }

    // ----- commands -------------------------------------------------------------------------

    /// Runs `command` if it is an editing, formatting, table or view command of the editor.
    /// Returns whether the editor owns the command (the host should not route it elsewhere);
    /// commands that need the app (links, images, tables insertion, find, files, copy as plain
    /// text / HTML / RTF) return `false`.
    pub fn execute(&mut self, command: C) -> bool {
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
            C::EditPaste => self.paste(false),
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

    /// Inserts `formatted` (the localized long date, `F5`) at the caret; escapes a date that
    /// would start an ordered list.
    pub fn insert_date(&mut self, formatted: &str) {
        self.run(|ctx| Some(format::insert_date(ctx, formatted)));
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
