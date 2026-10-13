//! In-window flyout menus: context menus and menu-bar drop-downs with submenus, check marks,
//! accelerator text, mnemonics and keyboard navigation.
//!
//! An open menu is modal: the host routes every event to it first and paints it last. It reports
//! the chosen command as a [`MenuEvent`]; commands are the owner's numbers, the menu knows no
//! application behavior.

use crate::access::{AccessAction, AccessNode, AccessTree, Role, ToggleState};
use crate::canvas::{Canvas, Dash, IconFont};
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::{Cursor, Event, KeyEvent, PointerButton, PointerEvent, PointerEventKind, vk};
use crate::text::TextLayoutEngine;
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::{Control, FONT_SIZE, ICON_SIZE, draw_left_centered, glyph, layout_line, paint_overlay};

/// Height of an item row.
pub const ITEM_HEIGHT: f32 = 32.0;
/// Height of a separator row.
pub const SEPARATOR_HEIGHT: f32 = 9.0;
/// Padding above and below the rows of a level.
const PADDING: f32 = 4.0;
/// Text inset from the level's edge.
const TEXT_INSET: f32 = 12.0;
/// Width of the check-mark column.
const CHECK_COLUMN: f32 = 28.0;
/// Gap between a label and its accelerator.
const ACCELERATOR_GAP: f32 = 24.0;
/// Width reserved for the submenu chevron.
const CHEVRON_COLUMN: f32 = 20.0;
/// Narrowest level.
const MIN_WIDTH: f32 = 128.0;
/// How far a submenu overlaps its parent.
const SUBMENU_OVERLAP: f32 = 4.0;

/// One row of a menu.
#[derive(Debug, Clone)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
}

/// A menu item. `command` is the owner's number for it.
#[derive(Debug, Clone)]
pub struct MenuItem {
    id: WidgetId,
    menu_id: WidgetId,
    command: u32,
    label: String,
    accelerator: Option<String>,
    mnemonic: Option<char>,
    enabled: bool,
    checked: Option<bool>,
    submenu: Vec<MenuEntry>,
}

impl MenuItem {
    #[must_use]
    pub fn new(command: u32, label: impl Into<String>) -> Self {
        Self {
            id: WidgetId::allocate(),
            menu_id: WidgetId::allocate(),
            command,
            label: label.into(),
            accelerator: None,
            mnemonic: None,
            enabled: true,
            checked: None,
            submenu: Vec::new(),
        }
    }

    /// The accelerator text shown at the right, e.g. `Ctrl+S` (already localized).
    #[must_use]
    pub fn accelerator(mut self, text: impl Into<String>) -> Self {
        self.accelerator = Some(text.into());
        self
    }

    /// The letter that activates the item while the menu is open.
    #[must_use]
    pub fn mnemonic(mut self, letter: char) -> Self {
        self.mnemonic = Some(letter);
        self
    }

    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// A checkable item, currently checked or not.
    #[must_use]
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
        self
    }

    /// Makes the item open `entries` as a submenu.
    #[must_use]
    pub fn submenu(mut self, entries: Vec<MenuEntry>) -> Self {
        self.submenu = entries;
        self
    }

    #[must_use]
    pub fn command(&self) -> u32 {
        self.command
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    fn has_submenu(&self) -> bool {
        !self.submenu.is_empty()
    }
}

/// What the user did with the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuEvent {
    /// An item was activated; the menu closed.
    Command(u32),
    /// Esc, or a press outside the menu; the menu closed.
    Dismissed,
}

/// The geometry of one open level.
#[derive(Debug, Clone)]
struct Level {
    rect: Rect,
    rows: Vec<Rect>,
    check_column: bool,
}

fn entries_at<'a>(root: &'a [MenuEntry], path: &[Option<usize>]) -> &'a [MenuEntry] {
    let mut entries = root;
    for index in path {
        let Some(MenuEntry::Item(item)) = index.and_then(|index| entries.get(index)) else {
            break;
        };
        entries = &item.submenu;
    }
    entries
}

fn first_enabled(entries: &[MenuEntry]) -> Option<usize> {
    entries
        .iter()
        .position(|entry| matches!(entry, MenuEntry::Item(item) if item.enabled))
}

/// A flyout menu with its submenus.
#[derive(Debug, Clone)]
pub struct Menu {
    id: WidgetId,
    label: String,
    entries: Vec<MenuEntry>,
    origin: Point,
    open: bool,
    /// The highlighted row of each open level; a submenu is open for every level after the first.
    levels: Vec<Option<usize>>,
    events: Vec<MenuEvent>,
}

impl Menu {
    /// A closed menu; `label` is the accessible name.
    #[must_use]
    pub fn new(label: impl Into<String>, entries: Vec<MenuEntry>) -> Self {
        Self {
            id: WidgetId::allocate(),
            label: label.into(),
            entries,
            origin: Point::ZERO,
            open: false,
            levels: Vec::new(),
            events: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the menu with its top-left corner at `origin` (window DIPs).
    pub fn open_at(&mut self, cx: &mut EventCx, origin: Point) {
        self.open = true;
        self.origin = origin;
        self.levels = vec![None];
        cx.invalidate_all();
    }

    pub fn close(&mut self, cx: &mut EventCx) {
        if self.open {
            self.open = false;
            self.levels.clear();
            cx.invalidate_all();
        }
    }

    /// The highlighted `(level, row)` of the innermost open level.
    #[must_use]
    pub fn highlighted(&self) -> Option<(usize, usize)> {
        let depth = self.levels.len().checked_sub(1)?;
        self.levels[depth].map(|row| (depth, row))
    }

    /// Number of open levels (the menu and its open submenus).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.levels.len()
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<MenuEvent> {
        std::mem::take(&mut self.events)
    }

    fn level_entries(&self, depth: usize) -> &[MenuEntry] {
        entries_at(&self.entries, &self.levels[..depth])
    }

    fn item(&self, depth: usize, row: usize) -> Option<&MenuItem> {
        match self.level_entries(depth).get(row) {
            Some(MenuEntry::Item(item)) => Some(item),
            _ => None,
        }
    }

    fn layout(&self, engine: &dyn TextLayoutEngine, theme: &Theme) -> Vec<Level> {
        let mut levels: Vec<Level> = Vec::with_capacity(self.levels.len());
        for depth in 0..self.levels.len() {
            let entries = self.level_entries(depth);
            let check_column = entries
                .iter()
                .any(|entry| matches!(entry, MenuEntry::Item(item) if item.checked.is_some()));
            let mut width = MIN_WIDTH;
            for entry in entries {
                if let MenuEntry::Item(item) = entry {
                    let label = layout_line(engine, theme, &item.label, FONT_SIZE)
                        .size()
                        .width;
                    let accelerator = item.accelerator.as_ref().map_or(0.0, |text| {
                        ACCELERATOR_GAP + layout_line(engine, theme, text, FONT_SIZE).size().width
                    });
                    let chevron = if item.has_submenu() {
                        CHEVRON_COLUMN
                    } else {
                        0.0
                    };
                    let check = if check_column { CHECK_COLUMN } else { 0.0 };
                    width = width.max(check + 2.0 * TEXT_INSET + label + accelerator + chevron);
                }
            }
            let origin = match levels.last() {
                None => self.origin,
                Some(parent) => {
                    let row = self.levels[depth - 1].and_then(|row| parent.rows.get(row));
                    row.map_or(self.origin, |row| {
                        Point::new(parent.rect.right() - SUBMENU_OVERLAP, row.y - PADDING)
                    })
                }
            };
            let mut y = origin.y + PADDING;
            let mut rows = Vec::with_capacity(entries.len());
            for entry in entries {
                let height = match entry {
                    MenuEntry::Item(_) => ITEM_HEIGHT,
                    MenuEntry::Separator => SEPARATOR_HEIGHT,
                };
                rows.push(Rect::new(origin.x, y, width, height));
                y += height;
            }
            levels.push(Level {
                rect: Rect::new(origin.x, origin.y, width, y + PADDING - origin.y),
                rows,
                check_column,
            });
        }
        levels
    }

    /// The `(level, row)` under `point`, innermost level first.
    fn hit(levels: &[Level], point: Point) -> Option<(usize, Option<usize>)> {
        let depth = levels
            .iter()
            .rposition(|level| level.rect.contains(point))?;
        let row = levels[depth]
            .rows
            .iter()
            .position(|row| row.contains(point));
        Some((depth, row))
    }

    /// Moves the highlight of the innermost level to the next enabled item in `step` direction.
    fn step(&mut self, step: isize) {
        let depth = self.levels.len() - 1;
        let entries = self.level_entries(depth);
        let count = entries.len() as isize;
        if count == 0 {
            return;
        }
        let start =
            self.levels[depth].map_or(if step > 0 { -1 } else { count }, |row| row as isize);
        let enabled = |index: isize| matches!(&entries[index as usize], MenuEntry::Item(item) if item.enabled);
        let next = (1..=count)
            .map(|offset| (start + step * offset).rem_euclid(count))
            .find(|index| enabled(*index));
        if let Some(next) = next {
            self.levels[depth] = Some(next as usize);
        }
    }

    fn jump(&mut self, last: bool) {
        let depth = self.levels.len() - 1;
        let entries = self.level_entries(depth);
        let target = if last {
            entries
                .iter()
                .rposition(|entry| matches!(entry, MenuEntry::Item(item) if item.enabled))
        } else {
            first_enabled(entries)
        };
        self.levels[depth] = target;
    }

    /// Activates the item at `(depth, row)`: opens its submenu or reports its command.
    fn activate(&mut self, cx: &mut EventCx, depth: usize, row: usize) {
        let Some(item) = self.item(depth, row) else {
            return;
        };
        if !item.enabled {
            return;
        }
        let command = item.command;
        let first = first_enabled(&item.submenu).filter(|_| item.has_submenu());
        let opens = item.has_submenu();
        self.levels.truncate(depth + 1);
        self.levels[depth] = Some(row);
        if opens {
            self.levels.push(first);
            cx.invalidate_all();
        } else {
            self.events.push(MenuEvent::Command(command));
            self.close(cx);
        }
    }

    fn dismiss(&mut self, cx: &mut EventCx) {
        self.events.push(MenuEvent::Dismissed);
        self.close(cx);
    }

    fn key(&mut self, cx: &mut EventCx, key: &KeyEvent) {
        let depth = self.levels.len() - 1;
        match key.vk {
            vk::DOWN => self.step(1),
            vk::UP => self.step(-1),
            vk::HOME => self.jump(false),
            vk::END => self.jump(true),
            vk::RIGHT => {
                let opens = self.levels[depth]
                    .filter(|row| self.item(depth, *row).is_some_and(MenuItem::has_submenu));
                if let Some(row) = opens {
                    self.activate(cx, depth, row);
                }
            }
            vk::LEFT if depth > 0 => {
                self.levels.pop();
            }
            vk::RETURN | vk::SPACE => {
                if let Some(row) = self.levels[depth] {
                    self.activate(cx, depth, row);
                }
            }
            vk::ESCAPE if depth > 0 => {
                self.levels.pop();
            }
            vk::ESCAPE => self.dismiss(cx),
            _ => {}
        }
        cx.invalidate_all();
    }

    fn text(&mut self, cx: &mut EventCx, typed: &str) {
        let Some(letter) = typed.chars().next() else {
            return;
        };
        let depth = self.levels.len() - 1;
        let found = self.level_entries(depth).iter().position(|entry| {
            matches!(entry, MenuEntry::Item(item)
                if item.enabled && item.mnemonic.is_some_and(|m| m.to_lowercase().eq(letter.to_lowercase())))
        });
        if let Some(row) = found {
            self.activate(cx, depth, row);
        }
    }

    fn pointer(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        let levels = self.layout(cx.text, cx.theme);
        let hit = Self::hit(&levels, event.position);
        match (event.kind, event.button) {
            (PointerEventKind::Down, Some(PointerButton::Left)) => match hit {
                None => self.dismiss(cx),
                Some((depth, Some(row))) => self.activate(cx, depth, row),
                Some((_, None)) => {}
            },
            (PointerEventKind::Move | PointerEventKind::Enter, _) => {
                if let Some((depth, Some(row))) = hit {
                    let enabled = self.item(depth, row).is_some_and(|item| item.enabled);
                    if enabled && event.kind == PointerEventKind::Move {
                        cx.set_cursor(Cursor::Hand);
                    }
                    if enabled
                        && self.levels.get(depth) == Some(&Some(row))
                        && self.levels.len() == depth + 1
                    {
                        return EventResult::handled();
                    }
                    if enabled {
                        self.levels.truncate(depth + 1);
                        self.levels[depth] = Some(row);
                        if self.item(depth, row).is_some_and(MenuItem::has_submenu) {
                            self.activate(cx, depth, row);
                        }
                    }
                }
            }
            _ => {}
        }
        cx.invalidate_all();
        EventResult::handled()
    }

    /// Carries out `action` on the item whose access node has id `item_id`.
    pub fn perform_item(
        &mut self,
        cx: &mut EventCx,
        item_id: WidgetId,
        action: &AccessAction,
    ) -> bool {
        if !self.open {
            return false;
        }
        let found = (0..self.levels.len()).find_map(|depth| {
            self.level_entries(depth)
                .iter()
                .enumerate()
                .find_map(|(row, entry)| match entry {
                    MenuEntry::Item(item) if item.id == item_id => {
                        Some((depth, row, item.has_submenu()))
                    }
                    _ => None,
                })
        });
        let Some((depth, row, has_submenu)) = found else {
            return false;
        };
        match action {
            AccessAction::Invoke if !has_submenu => {
                self.activate(cx, depth, row);
                true
            }
            AccessAction::Expand if has_submenu => {
                self.activate(cx, depth, row);
                true
            }
            AccessAction::Collapse if has_submenu => {
                self.levels.truncate(depth + 1);
                cx.invalidate_all();
                true
            }
            _ => false,
        }
    }

    fn level_node_id(&self, depth: usize) -> WidgetId {
        if depth == 0 {
            return self.id;
        }
        self.levels[depth - 1]
            .and_then(|row| self.item(depth - 1, row))
            .map_or(self.id, |item| item.menu_id)
    }
}

impl Control for Menu {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for Menu {
    fn measure(&mut self, _cx: &mut LayoutCx, _available: Size) -> Size {
        Size::ZERO
    }

    fn arrange(&mut self, _cx: &mut LayoutCx, _bounds: Rect) {}

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.open {
            return;
        }
        let tokens = &cx.theme.tokens;
        let levels = self.layout(cx.text, cx.theme);
        for (depth, level) in levels.iter().enumerate() {
            paint_overlay(canvas, level.rect, tokens.menu_bg, cx.theme);
            let entries = self.level_entries(depth);
            for (row, (entry, rect)) in entries.iter().zip(&level.rows).enumerate() {
                match entry {
                    MenuEntry::Separator => canvas.stroke_line(
                        Point::new(rect.x + PADDING, rect.center().y),
                        Point::new(rect.right() - PADDING, rect.center().y),
                        tokens.hairline,
                        1.0,
                        Dash::Solid,
                    ),
                    MenuEntry::Item(item) => {
                        let highlighted = self.levels[depth] == Some(row);
                        if highlighted && item.enabled {
                            canvas.fill_rounded_rect(
                                rect.inset(Insets::symmetric(PADDING, 2.0)),
                                CornerRadii::uniform(tokens.metrics.control_radius),
                                tokens.hover_bg,
                            );
                        }
                        let color = if item.enabled {
                            tokens.text
                        } else {
                            tokens.text_disabled
                        };
                        let inset = if level.check_column {
                            CHECK_COLUMN
                        } else {
                            0.0
                        };
                        if item.checked == Some(true) {
                            canvas.draw_icon(
                                glyph::CHECK_MARK,
                                IconFont::SegoeFluent,
                                ICON_SIZE * 0.75,
                                color,
                                Point::new(rect.x + TEXT_INSET + 4.0, rect.center().y),
                            );
                        }
                        let label = layout_line(cx.text, cx.theme, &item.label, FONT_SIZE);
                        draw_left_centered(
                            canvas,
                            label.as_ref(),
                            rect.x + TEXT_INSET + inset,
                            *rect,
                            color,
                        );
                        if let Some(text) = &item.accelerator {
                            let accelerator = layout_line(cx.text, cx.theme, text, FONT_SIZE);
                            let x = rect.right()
                                - TEXT_INSET
                                - accelerator.size().width
                                - if item.has_submenu() {
                                    CHEVRON_COLUMN
                                } else {
                                    0.0
                                };
                            draw_left_centered(
                                canvas,
                                accelerator.as_ref(),
                                x,
                                *rect,
                                tokens.text_muted,
                            );
                        }
                        if item.has_submenu() {
                            canvas.draw_icon(
                                glyph::CHEVRON_RIGHT,
                                IconFont::SegoeFluent,
                                ICON_SIZE * 0.75,
                                color,
                                Point::new(rect.right() - TEXT_INSET - 4.0, rect.center().y),
                            );
                        }
                    }
                }
            }
        }
    }

    /// An open menu is modal: it consumes every key and pointer event.
    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if !self.open {
            return EventResult::ignored();
        }
        match e {
            Event::Key(key) if key.is_down => {
                self.key(cx, key);
                EventResult::handled()
            }
            Event::Text(text) => {
                self.text(cx, &text.0);
                EventResult::handled()
            }
            Event::Pointer(event) => self.pointer(cx, event),
            Event::Wheel(_) => EventResult::handled(),
            _ => EventResult::ignored(),
        }
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        if !self.open {
            let mut node = AccessNode::new(self.id, Role::Menu, self.label.clone(), Rect::ZERO);
            node.state.offscreen = true;
            out.insert(node);
            return;
        }
        let levels = self.layout(cx.text, cx.theme);
        let deepest = self.levels.len() - 1;
        for (depth, level) in levels.iter().enumerate() {
            let mut children = Vec::new();
            for (row, entry) in self.level_entries(depth).iter().enumerate() {
                let MenuEntry::Item(item) = entry else {
                    continue;
                };
                let mut node =
                    AccessNode::new(item.id, Role::MenuItem, item.label.clone(), level.rows[row]);
                if let Some(accelerator) = &item.accelerator {
                    node.description = accelerator.clone();
                }
                node.state.disabled = !item.enabled;
                node.state.focusable = true;
                node.state.focused = depth == deepest && self.levels[depth] == Some(row);
                node.state.toggle = item.checked.map(|checked| {
                    if checked {
                        ToggleState::On
                    } else {
                        ToggleState::Off
                    }
                });
                node.patterns.toggle = item.checked.is_some();
                if item.has_submenu() {
                    let open =
                        self.levels.get(depth + 1).is_some() && self.levels[depth] == Some(row);
                    node.state.expanded = Some(open);
                    node.patterns.expand_collapse = true;
                    if open {
                        node.children.push(item.menu_id);
                    }
                } else {
                    node.patterns.invoke = true;
                }
                children.push(item.id);
                out.insert(node);
            }
            let name = if depth == 0 {
                self.label.clone()
            } else {
                self.levels[depth - 1]
                    .and_then(|row| self.item(depth - 1, row))
                    .map(|item| item.label.clone())
                    .unwrap_or_default()
            };
            out.insert(
                AccessNode::new(self.level_node_id(depth), Role::Menu, name, level.rect)
                    .with_children(children),
            );
        }
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{DrawOp, RecordingCanvas};
    use crate::input::Modifiers;
    use crate::widgets::testkit::Harness;

    fn icon_glyphs(canvas: &RecordingCanvas) -> Vec<char> {
        canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::Icon { glyph, .. } => Some(*glyph),
                _ => None,
            })
            .collect()
    }

    fn file_menu() -> Menu {
        Menu::new(
            "File",
            vec![
                MenuEntry::Item(MenuItem::new(1, "New").accelerator("Ctrl+N").mnemonic('N')),
                MenuEntry::Item(MenuItem::new(2, "Open").accelerator("Ctrl+O").mnemonic('O')),
                MenuEntry::Separator,
                MenuEntry::Item(MenuItem::new(3, "Export").mnemonic('E').submenu(vec![
                    MenuEntry::Item(MenuItem::new(31, "PDF").mnemonic('P')),
                    MenuEntry::Item(MenuItem::new(32, "HTML").disabled()),
                    MenuEntry::Item(MenuItem::new(33, "Word").mnemonic('W').checked(true)),
                ])),
                MenuEntry::Item(MenuItem::new(4, "Locked").disabled()),
                MenuEntry::Item(MenuItem::new(5, "Word wrap").checked(false)),
            ],
        )
    }

    fn open_menu() -> (Harness, Menu) {
        let mut harness = Harness::new();
        let mut menu = file_menu();
        let id = menu.id();
        let mut cx = harness.event_cx(id);
        menu.open_at(&mut cx, Point::new(10.0, 20.0));
        (harness, menu)
    }

    fn tap(harness: &mut Harness, menu: &mut Menu, key: u16) {
        assert!(harness.key(menu, key).handled);
    }

    #[test]
    fn kit_menu_layout_sizes_levels_and_places_submenus() {
        let (mut harness, mut menu) = open_menu();
        let levels = menu.layout(&harness.engine, &harness.theme);
        assert_eq!(levels.len(), 1);
        // The widest row is "Open  Ctrl+O": 28 check column + 24 padding + 32 + 24 gap + 48.
        assert_eq!(levels[0].rect.width, 156.0);
        assert_eq!(levels[0].rect.y, 20.0);
        assert_eq!(levels[0].rows[0], Rect::new(10.0, 24.0, 156.0, 32.0));
        assert_eq!(levels[0].rows[2].height, 9.0);
        // 5 items * 32 + 1 separator * 9 + 8 padding.
        assert_eq!(levels[0].rect.height, 5.0 * 32.0 + 9.0 + 8.0);
        tap(&mut harness, &mut menu, vk::DOWN);
        tap(&mut harness, &mut menu, vk::DOWN);
        tap(&mut harness, &mut menu, vk::DOWN);
        tap(&mut harness, &mut menu, vk::RIGHT);
        let levels = menu.layout(&harness.engine, &harness.theme);
        assert_eq!(levels.len(), 2);
        let export = levels[0].rows[3];
        assert_eq!(levels[1].rect.x, 10.0 + 156.0 - 4.0);
        assert_eq!(levels[1].rect.y, export.y - 4.0);
        assert!(levels[0].check_column && levels[1].check_column);
        assert_eq!(menu.depth(), 2);
        let mut wide = Menu::new(
            "w",
            vec![MenuEntry::Item(
                MenuItem::new(1, "a very long label here").accelerator("Ctrl+Shift+Alt+L"),
            )],
        );
        let id = wide.id();
        wide.open_at(&mut harness.event_cx(id), Point::ZERO);
        let width = wide.layout(&harness.engine, &harness.theme)[0].rect.width;
        assert_eq!(width, 12.0 * 2.0 + 22.0 * 8.0 + 24.0 + 16.0 * 8.0);
    }

    #[test]
    fn kit_menu_keyboard_skips_separators_and_disabled_items_and_wraps() {
        let (mut harness, mut menu) = open_menu();
        assert_eq!(menu.highlighted(), None);
        tap(&mut harness, &mut menu, vk::DOWN);
        assert_eq!(menu.highlighted(), Some((0, 0)));
        tap(&mut harness, &mut menu, vk::DOWN);
        tap(&mut harness, &mut menu, vk::DOWN);
        assert_eq!(menu.highlighted(), Some((0, 3)), "the separator is skipped");
        tap(&mut harness, &mut menu, vk::DOWN);
        assert_eq!(
            menu.highlighted(),
            Some((0, 5)),
            "the disabled item is skipped"
        );
        tap(&mut harness, &mut menu, vk::DOWN);
        assert_eq!(menu.highlighted(), Some((0, 0)), "wraps to the top");
        tap(&mut harness, &mut menu, vk::UP);
        assert_eq!(menu.highlighted(), Some((0, 5)), "and back to the bottom");
        tap(&mut harness, &mut menu, vk::HOME);
        assert_eq!(menu.highlighted(), Some((0, 0)));
        tap(&mut harness, &mut menu, vk::END);
        assert_eq!(menu.highlighted(), Some((0, 5)));
        // Up from nothing starts at the bottom.
        let (mut harness, mut fresh) = open_menu();
        tap(&mut harness, &mut fresh, vk::UP);
        assert_eq!(fresh.highlighted(), Some((0, 5)));
        // Left in the top level and unknown keys change nothing.
        tap(&mut harness, &mut fresh, vk::LEFT);
        tap(&mut harness, &mut fresh, vk::F5);
        assert_eq!(fresh.highlighted(), Some((0, 5)));
        // Right on an item without a submenu does nothing.
        tap(&mut harness, &mut fresh, vk::RIGHT);
        assert_eq!(fresh.depth(), 1);
    }

    #[test]
    fn kit_menu_submenus_open_close_and_report_commands() {
        let (mut harness, mut menu) = open_menu();
        for _ in 0..3 {
            tap(&mut harness, &mut menu, vk::DOWN);
        }
        tap(&mut harness, &mut menu, vk::RETURN);
        assert_eq!(menu.depth(), 2);
        assert_eq!(
            menu.highlighted(),
            Some((1, 0)),
            "the first enabled submenu item"
        );
        tap(&mut harness, &mut menu, vk::DOWN);
        assert_eq!(menu.highlighted(), Some((1, 2)), "HTML is disabled");
        tap(&mut harness, &mut menu, vk::LEFT);
        assert_eq!(menu.depth(), 1);
        tap(&mut harness, &mut menu, vk::SPACE);
        tap(&mut harness, &mut menu, vk::ESCAPE);
        assert_eq!(menu.depth(), 1, "Esc closes the submenu first");
        assert!(menu.is_open());
        tap(&mut harness, &mut menu, vk::RIGHT);
        tap(&mut harness, &mut menu, vk::RETURN);
        assert_eq!(menu.take_events(), vec![MenuEvent::Command(31)]);
        assert!(!menu.is_open());
        // Esc on the top level dismisses.
        let id = menu.id();
        menu.open_at(&mut harness.event_cx(id), Point::ZERO);
        tap(&mut harness, &mut menu, vk::ESCAPE);
        assert_eq!(menu.take_events(), vec![MenuEvent::Dismissed]);
        // Enter on disabled or no highlight does nothing; a closed menu ignores events.
        menu.open_at(&mut harness.event_cx(id), Point::ZERO);
        tap(&mut harness, &mut menu, vk::RETURN);
        assert!(menu.take_events().is_empty());
        menu.close(&mut harness.event_cx(id));
        menu.close(&mut harness.event_cx(id));
        assert!(!harness.key(&mut menu, vk::DOWN).handled);
        assert!(!harness.text(&mut menu, "n").handled);
    }

    #[test]
    fn kit_menu_mnemonics_activate_items_case_insensitively() {
        let (mut harness, mut menu) = open_menu();
        assert!(harness.text(&mut menu, "o").handled);
        assert_eq!(menu.take_events(), vec![MenuEvent::Command(2)]);
        let id = menu.id();
        menu.open_at(&mut harness.event_cx(id), Point::ZERO);
        harness.text(&mut menu, "E");
        assert_eq!(menu.depth(), 2);
        harness.text(&mut menu, "w");
        assert_eq!(menu.take_events(), vec![MenuEvent::Command(33)]);
        // Unknown letters, disabled items and empty input do nothing.
        menu.open_at(&mut harness.event_cx(id), Point::ZERO);
        harness.text(&mut menu, "z");
        harness.text(&mut menu, "");
        assert!(menu.take_events().is_empty());
        assert!(menu.is_open());
    }

    #[test]
    fn kit_menu_pointer_hover_click_and_dismiss() {
        let (mut harness, mut menu) = open_menu();
        let levels = menu.layout(&harness.engine, &harness.theme);
        let row = |r: usize| levels[0].rows[r].center();
        assert!(harness.hover(&mut menu, row(1)).handled);
        assert_eq!(menu.highlighted(), Some((0, 1)));
        // Hovering the disabled item or a separator keeps the highlight.
        harness.hover(&mut menu, row(4));
        harness.hover(&mut menu, row(2));
        assert_eq!(menu.highlighted(), Some((0, 1)));
        // Hovering a submenu item opens it; hovering back closes it.
        harness.hover(&mut menu, row(3));
        assert_eq!(menu.depth(), 2);
        harness.hover(&mut menu, row(3));
        assert_eq!(menu.depth(), 2);
        harness.hover(&mut menu, row(0));
        assert_eq!(menu.depth(), 1);
        harness.pointer(
            &mut menu,
            PointerEventKind::Down,
            Some(PointerButton::Right),
            row(0),
        );
        assert!(menu.take_events().is_empty());
        // Clicking an item reports it; clicking a disabled item or separator does not.
        harness.pointer(
            &mut menu,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            row(4),
        );
        harness.pointer(
            &mut menu,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            row(2),
        );
        assert!(menu.is_open());
        harness.pointer(
            &mut menu,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            row(1),
        );
        assert_eq!(menu.take_events(), vec![MenuEvent::Command(2)]);
        // Clicking a submenu item opens the submenu; clicking inside it picks the item.
        let id = menu.id();
        menu.open_at(&mut harness.event_cx(id), Point::new(10.0, 20.0));
        harness.pointer(
            &mut menu,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            row(3),
        );
        let levels = menu.layout(&harness.engine, &harness.theme);
        harness.pointer(
            &mut menu,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            levels[1].rows[0].center(),
        );
        assert_eq!(menu.take_events(), vec![MenuEvent::Command(31)]);
        // A press outside every level dismisses; other events are swallowed or ignored.
        menu.open_at(&mut harness.event_cx(id), Point::new(10.0, 20.0));
        assert!(
            harness
                .pointer(
                    &mut menu,
                    PointerEventKind::Down,
                    Some(PointerButton::Left),
                    Point::new(900.0, 900.0)
                )
                .handled
        );
        assert_eq!(menu.take_events(), vec![MenuEvent::Dismissed]);
        menu.open_at(&mut harness.event_cx(id), Point::new(10.0, 20.0));
        harness.send(
            &mut menu,
            Event::Wheel(crate::input::WheelEvent {
                delta: Point::new(0.0, 1.0),
                precise: false,
                position: Point::ZERO,
                mods: Modifiers::NONE,
            }),
        );
        assert!(
            !harness
                .send(&mut menu, Event::Focus(crate::input::FocusEvent::Lost))
                .handled
        );
        assert!(menu.is_open());
    }

    #[test]
    fn kit_menu_paints_rows_accelerators_checks_separators_and_chevrons() {
        let (mut harness, mut menu) = open_menu();
        let tokens = harness.theme.tokens;
        tap(&mut harness, &mut menu, vk::DOWN);
        let canvas = harness.paint(&menu);
        assert_eq!(
            canvas.texts(),
            vec![
                "New",
                "Ctrl+N",
                "Open",
                "Ctrl+O",
                "Export",
                "Locked",
                "Word wrap"
            ]
        );
        let level = menu.layout(&harness.engine, &harness.theme).remove(0);
        assert!(canvas.ops().any(
            |op| matches!(op, DrawOp::FillRoundedRect { rect, color, .. }
            if *rect == level.rect && *color == tokens.menu_bg)
        ));
        // The highlighted row has the hover fill.
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::FillRoundedRect { rect, color, .. }
            if *rect == level.rows[0].inset(Insets::symmetric(4.0, 2.0)) && *color == tokens.hover_bg)));
        assert_eq!(
            canvas
                .ops()
                .filter(|op| matches!(op, DrawOp::StrokeLine { .. }))
                .count(),
            1
        );
        // Disabled items are grey; unchecked items draw no check mark.
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::Text { text, color, .. } if text == "Locked" && *color == tokens.text_disabled)));
        assert_eq!(icon_glyphs(&canvas), [glyph::CHEVRON_RIGHT]);
        // The submenu paints beside it, with the checked item marked.
        for _ in 0..2 {
            tap(&mut harness, &mut menu, vk::DOWN);
        }
        tap(&mut harness, &mut menu, vk::RIGHT);
        let canvas = harness.paint(&menu);
        assert!(canvas.texts().contains(&"HTML"));
        assert!(icon_glyphs(&canvas).contains(&glyph::CHECK_MARK));
        // A closed menu paints nothing.
        let id = menu.id();
        menu.close(&mut harness.event_cx(id));
        assert!(harness.paint(&menu).recorded().is_empty());
    }

    #[test]
    fn kit_menu_access_nodes_and_item_actions() {
        let (mut harness, mut menu) = open_menu();
        assert!(harness.node(&file_menu()).state.offscreen);
        tap(&mut harness, &mut menu, vk::DOWN);
        let tree = harness.tree(&menu);
        let root = tree.root().expect("root");
        assert_eq!((root.role, root.name.as_str()), (Role::Menu, "File"));
        assert_eq!(root.children.len(), 5, "separators have no node");
        let items: Vec<&AccessNode> = tree.children(menu.id()).collect();
        assert_eq!(items[0].name, "New");
        assert_eq!(items[0].description, "Ctrl+N");
        assert!(items[0].state.focused && items[0].patterns.invoke);
        assert!(items[2].patterns.expand_collapse && !items[2].patterns.invoke);
        assert_eq!(items[2].state.expanded, Some(false));
        assert!(items[2].children.is_empty());
        assert!(items[3].state.disabled);
        assert_eq!(items[3].description, "");
        assert_eq!(items[3].state.toggle, None);
        assert_eq!(
            (items[4].state.toggle, items[4].patterns.toggle),
            (Some(ToggleState::Off), true)
        );
        let export_id = items[2].id;
        let new_id = items[0].id;
        let locked_id = items[3].id;
        let id = menu.id();
        {
            let mut cx = harness.event_cx(id);
            assert!(menu.perform_item(&mut cx, export_id, &AccessAction::Expand));
        }
        assert_eq!(menu.depth(), 2);
        let tree = harness.tree(&menu);
        let export = tree.node(export_id).expect("export");
        assert_eq!(export.state.expanded, Some(true));
        assert_eq!(export.children.len(), 1);
        let submenu = tree.node(export.children[0]).expect("submenu");
        assert_eq!(
            (submenu.role, submenu.name.as_str()),
            (Role::Menu, "Export")
        );
        assert_eq!(tree.children(submenu.id).count(), 3);
        assert!(
            tree.children(submenu.id)
                .any(|n| n.name == "Word" && n.state.toggle == Some(ToggleState::On))
        );
        {
            let mut cx = harness.event_cx(id);
            assert!(menu.perform_item(&mut cx, export_id, &AccessAction::Collapse));
            assert_eq!(menu.depth(), 1);
            assert!(!menu.perform_item(&mut cx, new_id, &AccessAction::Expand));
            assert!(!menu.perform_item(&mut cx, new_id, &AccessAction::Collapse));
            assert!(!menu.perform_item(&mut cx, export_id, &AccessAction::Invoke));
            assert!(!menu.perform_item(&mut cx, locked_id, &AccessAction::Select));
            assert!(!menu.perform_item(&mut cx, WidgetId::allocate(), &AccessAction::Invoke));
            assert!(menu.perform_item(&mut cx, new_id, &AccessAction::Invoke));
        }
        assert_eq!(menu.take_events(), vec![MenuEvent::Command(1)]);
        assert!(!harness.perform(&mut menu, AccessAction::Invoke));
        // A closed menu refuses item actions.
        let mut cx = harness.event_cx(id);
        assert!(!menu.perform_item(&mut cx, new_id, &AccessAction::Invoke));
        assert_eq!(harness.place(&mut menu, Point::ZERO).size(), Size::ZERO);
    }

    #[test]
    fn kit_menu_item_accessors_report_the_command_and_label() {
        let item = MenuItem::new(7, "Seven");
        assert_eq!(item.command(), 7);
        assert_eq!(item.label(), "Seven");
    }

    #[test]
    fn kit_menu_entry_paths_stop_at_the_last_level_they_lead_to() {
        let menu = file_menu();
        let root = menu.entries.len();
        assert_eq!(entries_at(&menu.entries, &[]).len(), root);
        assert_eq!(entries_at(&menu.entries, &[Some(3)]).len(), 3);
        // No highlight, a separator or a missing row cannot be descended into.
        for path in [[None], [Some(2)], [Some(99)]] {
            assert_eq!(entries_at(&menu.entries, &path).len(), root, "{path:?}");
        }
        assert_eq!(
            entries_at(&menu.entries, &[Some(3), None, Some(0)]).len(),
            3
        );
    }

    #[test]
    fn kit_menu_requests_the_hand_over_enabled_items_only() {
        let (mut harness, mut menu) = open_menu();
        let levels = menu.layout(&harness.engine, &harness.theme);
        let row = |r: usize| levels[0].rows[r].center();
        harness.hover(&mut menu, row(1));
        assert_eq!(harness.requests.take_cursor(), Some(Cursor::Hand));
        harness.hover(&mut menu, row(4));
        assert_eq!(harness.requests.take_cursor(), None, "disabled item");
        harness.hover(&mut menu, row(2));
        assert_eq!(harness.requests.take_cursor(), None, "separator");
        harness.hover(&mut menu, Point::new(row(1).x, levels[0].rect.y + 1.0));
        assert_eq!(harness.requests.take_cursor(), None, "padding");
        harness.hover(&mut menu, row(3));
        assert_eq!(harness.requests.take_cursor(), Some(Cursor::Hand));
    }

    #[test]
    fn kit_menu_pointer_details_hover_twice_padding_and_outside() {
        let (mut harness, mut menu) = open_menu();
        let levels = menu.layout(&harness.engine, &harness.theme);
        let row = levels[0].rows[1].center();
        assert!(harness.hover(&mut menu, row).handled);
        assert_eq!(menu.highlighted(), Some((0, 1)));
        // Hovering the highlighted item again changes nothing.
        assert!(harness.hover(&mut menu, row).handled);
        assert_eq!(menu.highlighted(), Some((0, 1)));
        // Moving over the padding or away from the menu keeps the highlight.
        let padding = Point::new(row.x, levels[0].rect.y + 1.0);
        assert!(harness.hover(&mut menu, padding).handled);
        assert!(harness.hover(&mut menu, Point::new(900.0, 900.0)).handled);
        assert_eq!(menu.highlighted(), Some((0, 1)));
        // Pressing the padding neither activates an item nor dismisses the menu.
        let left = Some(PointerButton::Left);
        let down = harness.pointer(&mut menu, PointerEventKind::Down, left, padding);
        assert!(down.handled);
        assert!(menu.is_open());
        assert!(menu.take_events().is_empty());
    }

    #[test]
    fn kit_menu_without_entries_has_nothing_to_highlight() {
        let mut harness = Harness::new();
        let mut menu = Menu::new("Empty", Vec::new());
        let id = menu.id();
        menu.open_at(&mut harness.event_cx(id), Point::ZERO);
        tap(&mut harness, &mut menu, vk::DOWN);
        tap(&mut harness, &mut menu, vk::UP);
        assert_eq!(menu.highlighted(), None);
        assert!(menu.is_open());
    }

    #[test]
    fn kit_menu_without_enabled_items_highlights_nothing_even_when_closed() {
        let mut harness = Harness::new();
        let mut menu = Menu::new(
            "Locked",
            vec![
                MenuEntry::Item(MenuItem::new(1, "Read only").disabled()),
                MenuEntry::Separator,
            ],
        );
        assert_eq!(menu.highlighted(), None, "a closed menu has no levels");
        assert_eq!(menu.depth(), 0);
        let id = menu.id();
        menu.open_at(&mut harness.event_cx(id), Point::ZERO);
        tap(&mut harness, &mut menu, vk::DOWN);
        assert_eq!(menu.highlighted(), None);
        tap(&mut harness, &mut menu, vk::UP);
        assert_eq!(menu.highlighted(), None);
        assert!(menu.is_open());
    }

    #[test]
    fn kit_menu_accelerators_make_room_for_the_submenu_chevron() {
        let mut harness = Harness::new();
        let inner = vec![MenuEntry::Item(MenuItem::new(21, "Inner"))];
        let mut menu = Menu::new(
            "Share",
            vec![
                MenuEntry::Item(MenuItem::new(1, "Plain").accelerator("Ctrl+L")),
                MenuEntry::Item(
                    MenuItem::new(2, "More")
                        .accelerator("Ctrl+M")
                        .submenu(inner),
                ),
            ],
        );
        let id = menu.id();
        menu.open_at(&mut harness.event_cx(id), Point::ZERO);
        let right = menu.layout(&harness.engine, &harness.theme)[0].rows[0].right();
        let canvas = harness.paint(&menu);
        let end_of = |wanted: &str| {
            canvas.ops().find_map(|op| match op {
                DrawOp::Text {
                    text, origin, size, ..
                } if text == wanted => Some(origin.x + size.width),
                _ => None,
            })
        };
        let plain = end_of("Ctrl+L").expect("accelerator of the plain item");
        let with_submenu = end_of("Ctrl+M").expect("accelerator of the submenu item");
        assert!((plain - (right - TEXT_INSET)).abs() < 1e-3, "{plain}");
        let expected = right - TEXT_INSET - CHEVRON_COLUMN;
        assert!((with_submenu - expected).abs() < 1e-3, "{with_submenu}");
    }
}
