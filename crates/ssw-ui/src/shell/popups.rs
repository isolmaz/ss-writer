//! Editor popups (spec §2.9 and §3.2): the B I U pill, the selection toolbar, the heading level
//! menu, the link edit popup and the wikilink completion list.
//!
//! None of them touches the document: each reports what the user chose as an event
//! ([`FormatPillEvent`], [`ToolbarEvent`], [`CommandMenuEvent`], [`LinkEditEvent`],
//! [`WikilinkEvent`]) and the editor / app applies it through `ssw_core::ops`. Commands are
//! reported as [`CommandId`]s, so the host runs them like a shortcut. Labels come from Fluent
//! (the command labels of `main.ftl` and `search.ftl`), accelerators from [`ShortcutLabels`].
//!
//! The host routes every event to a visible popup first (like a menu) and paints it last.
//! Popups placed next to the text ([`SelectionToolbar`], [`LinkEditPopup`],
//! [`WikilinkCompletion`]) take the anchor rectangle (the selection, the link, the caret) and
//! the window rectangle to keep themselves inside; they flip above the anchor when there is no
//! room below.

use std::ops::Range;
use std::rc::Rc;

use ssw_core::command::CommandId;
use ssw_core::document::Document;
use ssw_core::fuzzy::FuzzyMatcher;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::links::{RelPath, complete_heading, complete_wikilink};
use ssw_core::markdown::ast::{Block, Inline, InlineKind};
use ssw_core::text::TextRange;
use ssw_core::toc::Toc;

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, IconFont};
use crate::geometry::{CornerRadii, Point, Rect, Size};
use crate::input::{Event, PointerButton, PointerEventKind, vk};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::button::{Button, ButtonKind};
use crate::widgets::menu::{Menu, MenuEntry, MenuEvent, MenuItem};
use crate::widgets::textbox::TextBox;
use crate::widgets::{CONTROL_HEIGHT, Control, ICON_SIZE, SharedClipboard, glyph, paint_overlay};

use super::find::{CHIP_SIZE, ToggleChip};
use super::palette::ShortcutLabels;
use super::picker::{Row, RowClick, RowKey, RowList, RowOutcome, highlight_ranges};

/// Padding inside a popup.
const PAD: f32 = 4.0;
/// Gap between an anchor and its popup.
const ANCHOR_GAP: f32 = 6.0;
/// Distance kept from the window edges.
const EDGE: f32 = 8.0;
/// Gap between neighbouring chips.
const CHIP_GAP: f32 = 2.0;

// ---------------------------------------------------------------------------- shared helpers

/// The shortcut text of every command, resolved once.
#[derive(Debug, Clone)]
struct Accelerators(Vec<Option<String>>);

impl Accelerators {
    fn new(shortcuts: &dyn ShortcutLabels) -> Self {
        Self(
            CommandId::all()
                .map(|command| shortcuts.shortcut_label(command))
                .collect(),
        )
    }

    fn get(&self, command: CommandId) -> Option<&String> {
        // `CommandId` discriminants are the indices of `CommandId::ALL`.
        self.0[command as usize].as_ref()
    }
}

/// A menu item running `command`: its Fluent label and shortcut text.
fn command_item(localizer: &Localizer, accel: &Accelerators, command: CommandId) -> MenuItem {
    let item = MenuItem::new(command as u32, localizer.text(command.label_id()));
    match accel.get(command) {
        Some(text) => item.accelerator(text.clone()),
        None => item,
    }
}

fn command_entries(
    localizer: &Localizer,
    accel: &Accelerators,
    commands: &[CommandId],
) -> Vec<MenuEntry> {
    commands
        .iter()
        .map(|command| MenuEntry::Item(command_item(localizer, accel, *command)))
        .collect()
}

/// Paragraph and Heading 1 to 6, in this order.
const HEADING_COMMANDS: [CommandId; 7] = [
    CommandId::FormatParagraph,
    CommandId::FormatHeading1,
    CommandId::FormatHeading2,
    CommandId::FormatHeading3,
    CommandId::FormatHeading4,
    CommandId::FormatHeading5,
    CommandId::FormatHeading6,
];

/// The heading menu entries; with `current` (`Some(None)` is a paragraph) the matching entry is
/// checked.
fn heading_entries(
    localizer: &Localizer,
    accel: &Accelerators,
    current: Option<Option<u8>>,
) -> Vec<MenuEntry> {
    HEADING_COMMANDS
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let item = command_item(localizer, accel, *command);
            MenuEntry::Item(match current {
                Some(level) => item.checked(usize::from(level.unwrap_or(0)) == index),
                None => item,
            })
        })
        .collect()
}

/// Sends `e` to every child; a child that asks for focus gets it right away.
fn route(cx: &mut EventCx, children: Vec<&mut dyn Control>, e: &Event) -> EventResult {
    let mut result = EventResult::ignored();
    for child in children {
        let id = child.id();
        let handled = child.event(&mut cx.for_widget(id), e);
        if handled.request_focus {
            cx.focus.focus(id);
        }
        result = result.or(EventResult {
            request_focus: false,
            ..handled
        });
    }
    result
}

/// Where a popup of `size` goes next to `anchor` inside `window`: centred (`centered`) or left
/// aligned with the anchor, below it, or above it when it does not fit below; kept inside the
/// window.
fn place_near(anchor: Rect, window: Rect, size: Size, centered: bool) -> Rect {
    let wanted = if centered {
        anchor.center().x - size.width / 2.0
    } else {
        anchor.x
    };
    let x = wanted
        .min(window.right() - EDGE - size.width)
        .max(window.x + EDGE);
    let below = anchor.bottom() + ANCHOR_GAP;
    let above = anchor.y - ANCHOR_GAP - size.height;
    let y = if below + size.height <= window.bottom() - EDGE || above < window.y {
        below
    } else {
        above
    };
    Rect::new(x, y, size.width, size.height)
}

// ---------------------------------------------------------------------------- command menu

/// What a [`CommandMenu`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMenuEvent {
    /// The user chose an item; the menu closed.
    Command(CommandId),
    /// Esc or a press outside; the menu closed.
    Dismissed,
}

/// A flyout menu whose items are commands: wraps [`Menu`] and reports [`CommandId`]s instead of
/// the menu's numbers.
#[derive(Debug)]
pub struct CommandMenu {
    id: WidgetId,
    name: String,
    menu: Menu,
    events: Vec<CommandMenuEvent>,
}

impl CommandMenu {
    /// A closed menu named `name` (the accessible name).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        let id = WidgetId::allocate();
        let name = name.into();
        Self {
            id,
            menu: Menu::new(name.clone(), Vec::new()).with_id(id),
            name,
            events: Vec::new(),
        }
    }

    /// Opens the menu with `entries` (items built from command numbers, see
    /// [`CommandMenu::command_number`]) at `origin` (window DIPs).
    pub fn open_at(&mut self, cx: &mut EventCx, origin: Point, entries: Vec<MenuEntry>) {
        self.menu = Menu::new(self.name.clone(), entries).with_id(self.id);
        self.menu.open_at(cx, origin);
    }

    /// The number a [`MenuItem`] carries for `command`.
    #[must_use]
    pub fn command_number(command: CommandId) -> u32 {
        command as u32
    }

    pub fn close(&mut self, cx: &mut EventCx) {
        self.menu.close(cx);
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.menu.is_open()
    }

    /// The highlighted `(level, row)` of the innermost open level.
    #[must_use]
    pub fn highlighted(&self) -> Option<(usize, usize)> {
        self.menu.highlighted()
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<CommandMenuEvent> {
        std::mem::take(&mut self.events)
    }

    fn drain(&mut self) {
        for event in self.menu.take_events() {
            self.events.push(match event {
                MenuEvent::Command(number) => CommandId::ALL
                    .get(number as usize)
                    .map_or(CommandMenuEvent::Dismissed, |command| {
                        CommandMenuEvent::Command(*command)
                    }),
                MenuEvent::Dismissed => CommandMenuEvent::Dismissed,
            });
        }
    }

    /// Carries out `action` on the item whose access node has id `item`.
    pub fn perform_item(
        &mut self,
        cx: &mut EventCx,
        item: WidgetId,
        action: &AccessAction,
    ) -> bool {
        let performed = self.menu.perform_item(cx, item, action);
        self.drain();
        performed
    }
}

impl Control for CommandMenu {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for CommandMenu {
    fn measure(&mut self, cx: &mut LayoutCx, available: Size) -> Size {
        self.menu.measure(cx, available)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.menu.arrange(cx, bounds);
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        self.menu.paint(cx, canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let result = self.menu.event(cx, e);
        self.drain();
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        self.menu.access(cx, out);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        let performed = self.menu.perform(cx, a);
        self.drain();
        performed
    }
}

// ---------------------------------------------------------------------------- heading menu

/// The menu opened by clicking a heading's gutter badge: Paragraph and Heading 1 to 6 with the
/// current level checked. Reports [`CommandMenuEvent`]s (`FormatHeading1` … `FormatParagraph`).
#[derive(Debug)]
pub struct HeadingLevelMenu {
    localizer: Rc<Localizer>,
    accel: Accelerators,
    menu: CommandMenu,
}

impl HeadingLevelMenu {
    #[must_use]
    pub fn new(localizer: Rc<Localizer>, shortcuts: &dyn ShortcutLabels) -> Self {
        let menu = CommandMenu::new(localizer.text("search-heading-menu"));
        Self {
            accel: Accelerators::new(shortcuts),
            localizer,
            menu,
        }
    }

    /// Opens the menu at `origin`; `level` is the heading level of the block (`None` for a
    /// paragraph).
    pub fn open_at(&mut self, cx: &mut EventCx, origin: Point, level: Option<u8>) {
        let entries = heading_entries(&self.localizer, &self.accel, Some(level));
        self.menu.open_at(cx, origin, entries);
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.menu.is_open()
    }

    pub fn close(&mut self, cx: &mut EventCx) {
        self.menu.close(cx);
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<CommandMenuEvent> {
        self.menu.take_events()
    }

    /// Carries out `action` on the item whose access node has id `item`.
    pub fn perform_item(
        &mut self,
        cx: &mut EventCx,
        item: WidgetId,
        action: &AccessAction,
    ) -> bool {
        self.menu.perform_item(cx, item, action)
    }
}

impl Control for HeadingLevelMenu {
    fn id(&self) -> WidgetId {
        self.menu.id()
    }
}

impl Widget for HeadingLevelMenu {
    fn measure(&mut self, cx: &mut LayoutCx, available: Size) -> Size {
        self.menu.measure(cx, available)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.menu.arrange(cx, bounds);
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        self.menu.paint(cx, canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        self.menu.event(cx, e)
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        self.menu.access(cx, out);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        self.menu.perform(cx, a)
    }
}

// ---------------------------------------------------------------------------- B I U pill

/// Whether the text at the caret or selection is bold, italic or underlined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FormatState {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

fn scan_block(block: &Block, range: TextRange, state: &mut FormatState) {
    if !block.range.contains_range(range) {
        return;
    }
    for inline in &block.inlines {
        scan_inline(inline, range, state);
    }
    for child in &block.children {
        scan_block(child, range, state);
    }
}

fn scan_inline(inline: &Inline, range: TextRange, state: &mut FormatState) {
    if !inline.range.contains_range(range) {
        return;
    }
    if inline.content.contains_range(range) {
        match inline.kind {
            InlineKind::Strong => state.bold = true,
            InlineKind::Emphasis => state.italic = true,
            InlineKind::Underline => state.underline = true,
            _ => {}
        }
    }
    for child in &inline.children {
        scan_inline(child, range, state);
    }
}

/// The formatting at the primary selection of `doc`: a state is on when the whole selection (or
/// the caret) lies inside the content of such an element (`**bo|ld**`, a caret at the edge of the
/// markers does not count).
#[must_use]
pub fn format_state_at(doc: &Document) -> FormatState {
    let range = doc.primary_selection().range();
    let mut state = FormatState::default();
    for block in &doc.ast().blocks {
        scan_block(block, range, &mut state);
    }
    state
}

/// What the pill asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatPillEvent {
    /// Toggle bold, italic or underline (`FormatBold`, `FormatItalic`, `FormatUnderline`): the
    /// command applies to the selection or inserts a marker pair.
    Toggle(CommandId),
}

/// The B I U pill: three toggles that reflect [`FormatState`]. A click does not flip a toggle
/// by itself: it reports [`FormatPillEvent::Toggle`], the host runs the command and calls
/// [`FormatPill::set_state`] with the new state at the caret.
#[derive(Debug)]
pub struct FormatPill {
    id: WidgetId,
    name: String,
    bold: ToggleChip,
    italic: ToggleChip,
    underline: ToggleChip,
    state: FormatState,
    visible: bool,
    bounds: Rect,
    events: Vec<FormatPillEvent>,
}

impl FormatPill {
    /// A visible pill with every toggle off.
    #[must_use]
    pub fn new(localizer: &Localizer) -> Self {
        let chip = |glyph_id: &str, command: CommandId| {
            ToggleChip::text(
                localizer.text(glyph_id),
                localizer.text(command.label_id()),
                false,
            )
        };
        Self {
            id: WidgetId::allocate(),
            name: localizer.text("search-pill-label"),
            bold: chip("search-pill-bold-glyph", CommandId::FormatBold),
            italic: chip("search-pill-italic-glyph", CommandId::FormatItalic),
            underline: chip("search-pill-underline-glyph", CommandId::FormatUnderline),
            state: FormatState::default(),
            visible: true,
            bounds: Rect::ZERO,
            events: Vec::new(),
        }
    }

    /// Applies the motion preference to the toggles.
    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.bold = self.bold.with_motion(motion);
        self.italic = self.italic.with_motion(motion);
        self.underline = self.underline.with_motion(motion);
        self
    }

    /// Shows the formatting at the caret.
    pub fn set_state(&mut self, state: FormatState) {
        self.state = state;
        self.sync();
    }

    #[must_use]
    pub fn state(&self) -> FormatState {
        self.state
    }

    /// Shows or hides the pill (Zen mode, the "show B I U pill" setting).
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// The pill's bounds (empty while hidden or before layout).
    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<FormatPillEvent> {
        std::mem::take(&mut self.events)
    }

    fn sync(&mut self) {
        self.bold.set_on(self.state.bold);
        self.italic.set_on(self.state.italic);
        self.underline.set_on(self.state.underline);
    }

    fn size() -> Size {
        Size::new(
            3.0 * CHIP_SIZE + 2.0 * CHIP_GAP + 2.0 * PAD,
            CHIP_SIZE + 2.0 * PAD,
        )
    }

    fn chips(&self) -> [&ToggleChip; 3] {
        [&self.bold, &self.italic, &self.underline]
    }

    /// Turns what the chips recorded into events and puts them back to the caret state.
    fn collect(&mut self) {
        let commands = [
            (CommandId::FormatBold, &mut self.bold),
            (CommandId::FormatItalic, &mut self.italic),
            (CommandId::FormatUnderline, &mut self.underline),
        ];
        for (command, chip) in commands {
            if chip.take_changed().is_some() {
                self.events.push(FormatPillEvent::Toggle(command));
            }
        }
        self.sync();
    }
}

impl Control for FormatPill {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for FormatPill {
    fn measure(&mut self, _cx: &mut LayoutCx, _available: Size) -> Size {
        if self.visible {
            Self::size()
        } else {
            Size::ZERO
        }
    }

    /// `bounds` is the slot the shell reserved for the pill; the pill sits at its right edge,
    /// vertically centred.
    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = Rect::ZERO;
        if !self.visible {
            return;
        }
        let size = Self::size();
        let pill = Rect::new(
            bounds.right() - size.width,
            bounds.y + (bounds.height - size.height) / 2.0,
            size.width,
            size.height,
        );
        self.bounds = pill;
        let mut x = pill.x + PAD;
        for chip in [&mut self.bold, &mut self.italic, &mut self.underline] {
            let id = chip.id();
            chip.arrange(
                &mut cx.for_widget(id),
                Rect::new(x, pill.y + PAD, CHIP_SIZE, CHIP_SIZE),
            );
            x += CHIP_SIZE + CHIP_GAP;
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.visible || !cx.needs_paint(self.bounds) {
            return;
        }
        paint_overlay(canvas, self.bounds, cx.theme.tokens.surface, cx.theme);
        for chip in self.chips() {
            chip.paint(&cx.for_widget(chip.id()), canvas);
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if !self.visible {
            return EventResult::ignored();
        }
        let children: Vec<&mut dyn Control> = vec![
            &mut self.bold as &mut dyn Control,
            &mut self.italic,
            &mut self.underline,
        ];
        let mut result = route(cx, children, e);
        self.collect();
        if let Event::Pointer(pointer) = e
            && self.bounds.contains(pointer.position)
        {
            result = result.or(EventResult::handled());
        }
        if result.handled {
            cx.invalidate(self.bounds);
        }
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        if !self.visible {
            let mut node = AccessNode::new(self.id, Role::ToolBar, self.name.clone(), Rect::ZERO);
            node.state.offscreen = true;
            out.insert(node);
            return;
        }
        let mut children = Vec::new();
        for chip in self.chips() {
            chip.access(&cx.for_widget(chip.id()), out);
            children.push(chip.id());
        }
        out.insert(
            AccessNode::new(self.id, Role::ToolBar, self.name.clone(), self.bounds)
                .with_children(children),
        );
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------- selection toolbar

/// What the toolbar reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarEvent {
    /// Run this command on the selection.
    Command(CommandId),
    /// The toolbar hid itself (Esc, typing, scrolling, a press outside).
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dropdown {
    Heading,
    List,
    Highlight,
    More,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Command(CommandId),
    Dropdown(Dropdown),
}

#[derive(Debug)]
struct Tool {
    button: Button,
    item: Item,
}

impl Tool {
    /// Dropdowns other than the overflow menu show a chevron.
    fn has_chevron(&self) -> bool {
        matches!(
            self.item,
            Item::Dropdown(Dropdown::Heading | Dropdown::List | Dropdown::Highlight)
        )
    }

    fn width(&self) -> f32 {
        if self.has_chevron() {
            CONTROL_HEIGHT + 12.0
        } else {
            CONTROL_HEIGHT
        }
    }
}

/// The floating toolbar shown after a mouse selection (spec §2.9): Heading▾, Task, List▾,
/// Bold, Italic, Highlight▾ (colors), Link, Table, Image and ⋮ (Underline, Strikethrough,
/// Inline code, Code block, Quote, Callout, Math, Divider, Footnote, Date). It hides on typing,
/// scrolling, Esc and a press outside, reporting [`ToolbarEvent::Hidden`].
#[derive(Debug)]
pub struct SelectionToolbar {
    id: WidgetId,
    localizer: Rc<Localizer>,
    accel: Accelerators,
    tools: Vec<Tool>,
    menu: CommandMenu,
    visible: bool,
    anchor: Rect,
    bounds: Rect,
    open_tool: Option<usize>,
    events: Vec<ToolbarEvent>,
}

impl SelectionToolbar {
    /// A hidden toolbar.
    #[must_use]
    pub fn new(localizer: Rc<Localizer>, shortcuts: &dyn ShortcutLabels) -> Self {
        let l = localizer.as_ref();
        let command = |glyph: char, command: CommandId| Tool {
            button: Button::icon(glyph, l.text(command.label_id())),
            item: Item::Command(command),
        };
        let dropdown = |glyph: char, id: &str, kind: Dropdown| Tool {
            button: Button::icon(glyph, l.text(id)),
            item: Item::Dropdown(kind),
        };
        let tools = vec![
            dropdown('\u{E8D2}', "search-toolbar-heading", Dropdown::Heading),
            command('\u{E73A}', CommandId::FormatTaskToggle),
            dropdown('\u{E8FD}', "search-toolbar-list", Dropdown::List),
            command('\u{E8DD}', CommandId::FormatBold),
            command('\u{E8DB}', CommandId::FormatItalic),
            dropdown('\u{E7E6}', "search-toolbar-highlight", Dropdown::Highlight),
            command('\u{E71B}', CommandId::FormatLink),
            command('\u{E80A}', CommandId::FormatTable),
            command('\u{EB9F}', CommandId::FormatImage),
            dropdown('\u{E712}', "search-toolbar-more", Dropdown::More),
        ];
        let menu = CommandMenu::new(l.text("search-toolbar-label"));
        Self {
            id: WidgetId::allocate(),
            accel: Accelerators::new(shortcuts),
            localizer: Rc::clone(&localizer),
            tools,
            menu,
            visible: false,
            anchor: Rect::ZERO,
            bounds: Rect::ZERO,
            open_tool: None,
            events: Vec::new(),
        }
    }

    /// Applies the motion preference to the buttons.
    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.tools = self
            .tools
            .into_iter()
            .map(|tool| Tool {
                button: tool.button.with_motion(motion),
                item: tool.item,
            })
            .collect();
        self
    }

    /// Shows the toolbar next to `anchor` (the selection's rectangle).
    pub fn show(&mut self, anchor: Rect) {
        self.anchor = anchor;
        self.visible = true;
    }

    /// Hides the toolbar and reports [`ToolbarEvent::Hidden`].
    pub fn hide(&mut self) {
        if self.visible {
            self.visible = false;
            self.bounds = Rect::ZERO;
            self.open_tool = None;
            self.events.push(ToolbarEvent::Hidden);
        }
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// The toolbar's bounds (empty while hidden or before layout).
    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// True while a dropdown menu is open.
    #[must_use]
    pub fn is_menu_open(&self) -> bool {
        self.menu.is_open()
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<ToolbarEvent> {
        std::mem::take(&mut self.events)
    }

    fn dropdown_entries(&self, kind: Dropdown) -> Vec<MenuEntry> {
        let l = &self.localizer;
        let accel = &self.accel;
        match kind {
            Dropdown::Heading => heading_entries(l, accel, None),
            Dropdown::List => command_entries(
                l,
                accel,
                &[
                    CommandId::FormatBulletList,
                    CommandId::FormatNumberedList,
                    CommandId::FormatTaskList,
                ],
            ),
            Dropdown::Highlight => {
                let mut entries = vec![MenuEntry::Item(command_item(
                    l,
                    accel,
                    CommandId::FormatHighlight,
                ))];
                entries.push(MenuEntry::Separator);
                let colors = [
                    (CommandId::FormatHighlightRed, "search-color-red"),
                    (CommandId::FormatHighlightOrange, "search-color-orange"),
                    (CommandId::FormatHighlightYellow, "search-color-yellow"),
                    (CommandId::FormatHighlightGreen, "search-color-green"),
                    (CommandId::FormatHighlightBlue, "search-color-blue"),
                    (CommandId::FormatHighlightPurple, "search-color-purple"),
                ];
                entries.extend(colors.into_iter().map(|(command, label)| {
                    MenuEntry::Item(MenuItem::new(
                        CommandMenu::command_number(command),
                        l.text(label),
                    ))
                }));
                entries
            }
            Dropdown::More => command_entries(
                l,
                accel,
                &[
                    CommandId::FormatUnderline,
                    CommandId::FormatStrikethrough,
                    CommandId::FormatInlineCode,
                    CommandId::FormatCodeBlock,
                    CommandId::FormatQuote,
                    CommandId::FormatCallout,
                    CommandId::FormatMath,
                    CommandId::FormatDivider,
                    CommandId::FormatFootnote,
                    CommandId::EditInsertDate,
                ],
            ),
        }
    }

    fn activate(&mut self, cx: &mut EventCx, index: usize) {
        match self.tools[index].item {
            Item::Command(command) => self.events.push(ToolbarEvent::Command(command)),
            Item::Dropdown(kind) => {
                let rect = self.tools[index].button.bounds();
                let entries = self.dropdown_entries(kind);
                self.menu
                    .open_at(cx, Point::new(rect.x, rect.bottom() + 2.0), entries);
                self.open_tool = Some(index);
            }
        }
    }

    fn drain_menu(&mut self) {
        for event in self.menu.take_events() {
            if let CommandMenuEvent::Command(command) = event {
                self.events.push(ToolbarEvent::Command(command));
            }
            self.open_tool = None;
        }
    }

    fn children_mut(&mut self) -> Vec<&mut dyn Control> {
        self.tools
            .iter_mut()
            .map(|tool| &mut tool.button as &mut dyn Control)
            .collect()
    }

    /// Carries out an accessibility action on a button or a menu item (`child` is the node id).
    pub fn perform_child(&mut self, cx: &mut EventCx, child: WidgetId, a: &AccessAction) -> bool {
        if !self.visible {
            return false;
        }
        let performed =
            if let Some(index) = self.tools.iter().position(|tool| tool.button.id() == child) {
                let done = self.tools[index]
                    .button
                    .perform(&mut cx.for_widget(child), a);
                self.collect(cx);
                done
            } else {
                let done = self.menu.perform_item(cx, child, a);
                self.drain_menu();
                done
            };
        if performed {
            cx.invalidate_all();
        }
        performed
    }

    fn collect(&mut self, cx: &mut EventCx) {
        for index in 0..self.tools.len() {
            if self.tools[index].button.take_activated() {
                self.activate(cx, index);
            }
        }
    }
}

impl Control for SelectionToolbar {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for SelectionToolbar {
    fn measure(&mut self, _cx: &mut LayoutCx, _available: Size) -> Size {
        if !self.visible {
            return Size::ZERO;
        }
        Size::new(
            self.tools.iter().map(Tool::width).sum::<f32>() + 2.0 * PAD,
            CONTROL_HEIGHT + 2.0 * PAD,
        )
    }

    /// `bounds` is the window; the toolbar floats above the anchor (below it when there is no
    /// room above).
    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = Rect::ZERO;
        if !self.visible {
            return;
        }
        let size = Size::new(
            self.tools.iter().map(Tool::width).sum::<f32>() + 2.0 * PAD,
            CONTROL_HEIGHT + 2.0 * PAD,
        );
        // Prefer above the selection: place with the anchor mirrored.
        let above = Rect::new(
            self.anchor.x,
            self.anchor.y - size.height - ANCHOR_GAP,
            size.width,
            size.height,
        );
        let placed = if above.y >= bounds.y + EDGE {
            let x = (self.anchor.center().x - size.width / 2.0)
                .min(bounds.right() - EDGE - size.width)
                .max(bounds.x + EDGE);
            Rect::new(x, above.y, size.width, size.height)
        } else {
            place_near(self.anchor, bounds, size, true)
        };
        self.bounds = placed;
        let mut x = placed.x + PAD;
        for tool in &mut self.tools {
            let width = tool.width();
            let id = tool.button.id();
            tool.button.arrange(
                &mut cx.for_widget(id),
                Rect::new(x, placed.y + PAD, width, CONTROL_HEIGHT),
            );
            x += width;
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.visible {
            return;
        }
        if cx.needs_paint(self.bounds) {
            let tokens = &cx.theme.tokens;
            paint_overlay(canvas, self.bounds, tokens.surface, cx.theme);
            for (index, tool) in self.tools.iter().enumerate() {
                if self.open_tool == Some(index) {
                    canvas.fill_rounded_rect(
                        tool.button.bounds(),
                        CornerRadii::uniform(tokens.metrics.control_radius),
                        tokens.pressed_bg,
                    );
                }
                tool.button.paint(&cx.for_widget(tool.button.id()), canvas);
                if tool.has_chevron() {
                    let rect = tool.button.bounds();
                    canvas.draw_icon(
                        glyph::CHEVRON_DOWN,
                        IconFont::SegoeFluent,
                        ICON_SIZE * 0.5,
                        tokens.text_muted,
                        Point::new(rect.right() - 9.0, rect.center().y),
                    );
                }
            }
        }
        self.menu.paint(&cx.for_widget(self.menu.id()), canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if !self.visible {
            return EventResult::ignored();
        }
        if self.menu.is_open() {
            let id = self.menu.id();
            let result = self.menu.event(&mut cx.for_widget(id), e);
            self.drain_menu();
            cx.invalidate_all();
            return result;
        }
        match e {
            Event::Key(key) if key.is_down && key.vk == vk::ESCAPE && key.mods.is_empty() => {
                self.hide();
                cx.invalidate_all();
                return EventResult::handled();
            }
            Event::Text(_) | Event::Wheel(_) => {
                self.hide();
                cx.invalidate_all();
                return EventResult::ignored();
            }
            Event::Pointer(pointer)
                if pointer.kind == PointerEventKind::Down
                    && !self.bounds.contains(pointer.position) =>
            {
                self.hide();
                cx.invalidate_all();
                return EventResult::ignored();
            }
            _ => {}
        }
        let children = self.children_mut();
        let mut result = route(cx, children, e);
        self.collect(cx);
        if let Event::Pointer(pointer) = e
            && self.bounds.contains(pointer.position)
        {
            result = result.or(EventResult::handled());
        }
        if result.handled {
            cx.invalidate_all();
        }
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let name = self.localizer.text("search-toolbar-label");
        if !self.visible {
            let mut node = AccessNode::new(self.id, Role::ToolBar, name, Rect::ZERO);
            node.state.offscreen = true;
            out.insert(node);
            return;
        }
        let mut children = Vec::new();
        for (index, tool) in self.tools.iter().enumerate() {
            let id = tool.button.id();
            tool.button.access(&cx.for_widget(id), out);
            if matches!(tool.item, Item::Dropdown(_))
                && let Some(mut node) = out.node(id).cloned()
            {
                node.state.expanded = Some(self.open_tool == Some(index));
                node.patterns.expand_collapse = true;
                out.insert(node);
            }
            children.push(id);
        }
        if self.menu.is_open() {
            self.menu.access(&cx.for_widget(self.menu.id()), out);
            children.push(self.menu.id());
        }
        out.insert(
            AccessNode::new(self.id, Role::ToolBar, name, self.bounds).with_children(children),
        );
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------- link popup

/// What the link popup decided; it closes itself before reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEditEvent {
    /// Replace the link's text and address.
    Apply { text: String, url: String },
    /// Remove the link, keeping its text.
    Remove,
    /// Esc or a press outside.
    Cancel,
}

/// Width of the link popup.
const LINK_WIDTH: f32 = 320.0;

/// Height of the link popup: two text boxes and a button row with a gap above, between and below.
fn link_height() -> f32 {
    8.0 * PAD + 3.0 * CONTROL_HEIGHT
}

/// The popup shown when the caret is inside a link (the ✎ affordance): the link text and the
/// address, *Remove link* and *Apply*. `Enter` applies, `Esc` cancels.
#[derive(Debug)]
pub struct LinkEditPopup {
    id: WidgetId,
    name: String,
    text: TextBox,
    url: TextBox,
    remove: Button,
    apply: Button,
    visible: bool,
    anchor: Rect,
    bounds: Rect,
    focus_request: Option<WidgetId>,
    events: Vec<LinkEditEvent>,
}

impl LinkEditPopup {
    /// A hidden popup.
    #[must_use]
    pub fn new(localizer: &Localizer, clipboard: &SharedClipboard) -> Self {
        Self {
            id: WidgetId::allocate(),
            name: localizer.text("search-link-title"),
            text: TextBox::new(localizer.text("search-link-text"), "", Rc::clone(clipboard))
                .with_placeholder(localizer.text("search-link-text")),
            url: TextBox::new(localizer.text("search-link-url"), "", Rc::clone(clipboard))
                .with_placeholder(localizer.text("search-link-url")),
            remove: Button::new(ButtonKind::Standard, localizer.text("search-link-remove")),
            apply: Button::new(ButtonKind::Accent, localizer.text("search-link-apply")),
            visible: false,
            anchor: Rect::ZERO,
            bounds: Rect::ZERO,
            focus_request: None,
            events: Vec::new(),
        }
    }

    /// Applies the motion preference to the controls.
    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.text = self.text.with_motion(motion);
        self.url = self.url.with_motion(motion);
        self.remove = self.remove.with_motion(motion);
        self.apply = self.apply.with_motion(motion);
        self
    }

    /// Shows the popup below `anchor` (the link's rectangle) for a link with this text and
    /// address; the address box gets focus.
    pub fn show(&mut self, anchor: Rect, text: &str, url: &str) {
        self.text.set_text(text);
        self.url.set_text(url);
        self.url.select_all();
        self.anchor = anchor;
        self.visible = true;
        self.focus_request = Some(self.url.id());
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// The popup's bounds (empty while hidden or before layout).
    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The link text box.
    #[must_use]
    pub fn text_box(&self) -> &TextBox {
        &self.text
    }

    /// The address box.
    #[must_use]
    pub fn url_box(&self) -> &TextBox {
        &self.url
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<LinkEditEvent> {
        std::mem::take(&mut self.events)
    }

    /// The widget the host should focus after its next layout pass (the address box), once.
    pub fn take_focus_request(&mut self) -> Option<WidgetId> {
        self.focus_request.take()
    }

    /// True when `id` is the popup or one of its controls.
    #[must_use]
    pub fn owns(&self, id: WidgetId) -> bool {
        id == self.id
            || id == self.text.id()
            || id == self.url.id()
            || id == self.remove.id()
            || id == self.apply.id()
    }

    fn finish(&mut self, event: LinkEditEvent) {
        self.visible = false;
        self.bounds = Rect::ZERO;
        self.events.push(event);
    }

    /// Applies the edited text and address and closes.
    pub fn apply(&mut self) {
        let event = LinkEditEvent::Apply {
            text: self.text.text().to_owned(),
            url: self.url.text().to_owned(),
        };
        self.finish(event);
    }

    fn children(&self) -> [&dyn Control; 4] {
        [
            &self.text as &dyn Control,
            &self.url,
            &self.remove,
            &self.apply,
        ]
    }

    fn children_mut(&mut self) -> Vec<&mut dyn Control> {
        vec![
            &mut self.text as &mut dyn Control,
            &mut self.url,
            &mut self.remove,
            &mut self.apply,
        ]
    }

    fn collect(&mut self) {
        if self.text.take_submitted() | self.url.take_submitted() | self.apply.take_activated() {
            self.apply();
        } else if self.remove.take_activated() {
            self.finish(LinkEditEvent::Remove);
        }
    }

    /// Carries out an accessibility action on a control (`child` is its id).
    pub fn perform_child(&mut self, cx: &mut EventCx, child: WidgetId, a: &AccessAction) -> bool {
        if !self.visible {
            return false;
        }
        let performed = self
            .children_mut()
            .into_iter()
            .find(|control| control.id() == child)
            .is_some_and(|control| control.perform(&mut cx.for_widget(child), a));
        if performed {
            self.collect();
            cx.invalidate_all();
        }
        performed
    }
}

impl Control for LinkEditPopup {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for LinkEditPopup {
    fn measure(&mut self, _cx: &mut LayoutCx, _available: Size) -> Size {
        if self.visible {
            Size::new(LINK_WIDTH, link_height())
        } else {
            Size::ZERO
        }
    }

    /// `bounds` is the window.
    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = Rect::ZERO;
        if !self.visible {
            return;
        }
        let gap = 2.0 * PAD;
        let size = Size::new(
            LINK_WIDTH.min((bounds.width - 2.0 * EDGE).max(0.0)),
            link_height(),
        );
        let panel = place_near(self.anchor, bounds, size, false);
        self.bounds = panel;
        let inner = Rect::new(
            panel.x + gap,
            panel.y + gap,
            (panel.width - 2.0 * gap).max(0.0),
            CONTROL_HEIGHT,
        );
        let url_row = inner.translate(0.0, CONTROL_HEIGHT + gap);
        let button_y = url_row.y + CONTROL_HEIGHT + gap;
        let apply_id = self.apply.id();
        let apply_width = self
            .apply
            .measure(&mut cx.for_widget(apply_id), Size::UNBOUNDED)
            .width;
        let remove_id = self.remove.id();
        let remove_width = self
            .remove
            .measure(&mut cx.for_widget(remove_id), Size::UNBOUNDED)
            .width;
        let apply = Rect::new(
            inner.right() - apply_width,
            button_y,
            apply_width,
            CONTROL_HEIGHT,
        );
        let remove = Rect::new(
            apply.x - gap - remove_width,
            button_y,
            remove_width,
            CONTROL_HEIGHT,
        );
        for (control, rect) in [
            (&mut self.text as &mut dyn Control, inner),
            (&mut self.url, url_row),
            (&mut self.remove, remove),
            (&mut self.apply, apply),
        ] {
            let id = control.id();
            control.arrange(&mut cx.for_widget(id), rect);
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.visible || !cx.needs_paint(self.bounds) {
            return;
        }
        paint_overlay(canvas, self.bounds, cx.theme.tokens.surface, cx.theme);
        for child in self.children() {
            child.paint(&cx.for_widget(child.id()), canvas);
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if !self.visible {
            return EventResult::ignored();
        }
        if let Event::Key(key) = e
            && key.is_down
            && key.vk == vk::ESCAPE
            && key.mods.is_empty()
            && self
                .children()
                .iter()
                .any(|child| cx.focus.is_focused(child.id()))
        {
            self.finish(LinkEditEvent::Cancel);
            cx.invalidate_all();
            return EventResult::handled();
        }
        if let Event::Pointer(pointer) = e
            && pointer.kind == PointerEventKind::Down
            && !self.bounds.contains(pointer.position)
        {
            self.finish(LinkEditEvent::Cancel);
            cx.invalidate_all();
            return EventResult::ignored();
        }
        let children = self.children_mut();
        let mut result = route(cx, children, e);
        self.collect();
        if let Event::Pointer(pointer) = e
            && self.bounds.contains(pointer.position)
        {
            result = result.or(EventResult::handled());
        }
        if result.handled {
            cx.invalidate_all();
        }
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        if !self.visible {
            let mut node = AccessNode::new(self.id, Role::Window, self.name.clone(), Rect::ZERO);
            node.state.offscreen = true;
            out.insert(node);
            return;
        }
        let mut children = Vec::new();
        for child in self.children() {
            let id = child.id();
            child.access(&cx.for_widget(id), out);
            children.push(id);
        }
        out.insert(
            AccessNode::new(self.id, Role::Window, self.name.clone(), self.bounds)
                .with_children(children),
        );
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------- wikilink completion

/// Most completions listed.
const COMPLETION_LIMIT: usize = 50;
/// Rows in view at most.
const COMPLETION_ROWS: usize = 6;
/// Width of the completion list.
const COMPLETION_WIDTH: f32 = 360.0;

/// A chosen completion: replace `replace` of the query typed after `[[` by `insert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikilinkAccept {
    /// What to put in the document.
    pub insert: String,
    /// Byte range of the query (the text between `[[` and the caret) that `insert` replaces.
    pub replace: Range<usize>,
}

/// What the completion list decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WikilinkEvent {
    /// The user chose a row (`Enter`, `Tab` or a click); the list closed.
    Accept(WikilinkAccept),
    /// Esc or a press outside; the list closed.
    Dismissed,
}

/// `[[` completion (spec §3.2): fuzzy matches over the workspace's Markdown documents; after
/// `[[#` the headings of the current document; after a `|` (the alias) or after `target#` there
/// is nothing to complete and the list is empty.
///
/// The editor keeps keyboard focus while the list is open: the host feeds the text typed after
/// `[[` to [`set_query`](Self::set_query) and routes `Up`, `Down`, `PageUp`, `PageDown`,
/// `Enter`, `Tab` and `Esc` to the list while [`is_visible`](Self::is_visible).
#[derive(Debug)]
pub struct WikilinkCompletion {
    id: WidgetId,
    localizer: Rc<Localizer>,
    list: RowList,
    matcher: FuzzyMatcher,
    files: Vec<RelPath>,
    recent: Vec<usize>,
    toc: Toc,
    picks: Vec<WikilinkAccept>,
    open: bool,
    anchor: Rect,
    bounds: Rect,
    events: Vec<WikilinkEvent>,
}

impl WikilinkCompletion {
    /// A hidden list.
    #[must_use]
    pub fn new(localizer: Rc<Localizer>) -> Self {
        let list = RowList::new(localizer.text("search-wikilink-label"), COMPLETION_ROWS);
        Self {
            id: WidgetId::allocate(),
            localizer,
            list,
            matcher: FuzzyMatcher::new(),
            files: Vec::new(),
            recent: Vec::new(),
            toc: Toc::default(),
            picks: Vec::new(),
            open: false,
            anchor: Rect::ZERO,
            bounds: Rect::ZERO,
            events: Vec::new(),
        }
    }

    /// The workspace files and the indices of the most recently used ones (most recent first).
    pub fn set_files(&mut self, files: Vec<RelPath>, recent: Vec<usize>) {
        self.files = files;
        self.recent = recent;
    }

    /// The headings of the current document (for `[[#`).
    pub fn set_outline(&mut self, toc: &Toc) {
        self.toc = toc.clone();
    }

    /// Opens the list below the caret rectangle `anchor` and completes `query` (the text between
    /// `[[` and the caret).
    pub fn open(&mut self, anchor: Rect, query: &str) {
        self.open = true;
        self.anchor = anchor;
        self.set_query(query);
    }

    /// Completes `query` again (the user typed or deleted a character).
    pub fn set_query(&mut self, query: &str) {
        let mut rows = Vec::new();
        let mut picks = Vec::new();
        if query.contains('|') || (query.contains('#') && !query.starts_with('#')) {
            // The alias, or the heading of another document: nothing to complete.
        } else if let Some(fragment) = query.strip_prefix('#') {
            let found = complete_heading(&mut self.matcher, fragment, &self.toc, COMPLETION_LIMIT);
            for completion in found {
                let entry = &self.toc.entries()[completion.heading];
                rows.push(Row {
                    prefix: self.localizer.format(
                        "search-quick-open-heading-level",
                        &[("level", Arg::Int(i64::from(entry.level)))],
                    ),
                    label: entry.title.clone(),
                    highlights: highlight_ranges(&entry.title, &completion.indices),
                    ..Row::default()
                });
                picks.push(WikilinkAccept {
                    insert: completion.insert,
                    replace: 1..query.len(),
                });
            }
        } else {
            let found = complete_wikilink(
                &mut self.matcher,
                query,
                &self.files,
                &self.recent,
                COMPLETION_LIMIT,
            );
            for completion in found {
                let path = self.files[completion.file].as_str();
                rows.push(Row {
                    label: path.to_owned(),
                    highlights: highlight_ranges(path, &completion.indices),
                    detail: completion.insert.clone(),
                    ..Row::default()
                });
                picks.push(WikilinkAccept {
                    insert: completion.insert,
                    replace: 0..query.len(),
                });
            }
        }
        self.picks = picks;
        self.list.set_rows(rows);
    }

    /// True while the list is open and has something to offer.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.open && !self.list.is_empty()
    }

    /// The result list.
    #[must_use]
    pub fn list(&self) -> &RowList {
        &self.list
    }

    /// The list's bounds (empty while hidden or before layout).
    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// Closes the list without reporting anything (the caret left the link).
    pub fn close(&mut self) {
        self.open = false;
        self.bounds = Rect::ZERO;
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<WikilinkEvent> {
        std::mem::take(&mut self.events)
    }

    /// Chooses the selected row and closes. Returns whether there was one.
    pub fn accept(&mut self) -> bool {
        let Some(selected) = self.list.selected() else {
            return false;
        };
        let pick = self.picks[selected].clone();
        self.close();
        self.events.push(WikilinkEvent::Accept(pick));
        true
    }

    fn dismiss(&mut self) {
        self.close();
        self.events.push(WikilinkEvent::Dismissed);
    }

    /// Carries out an accessibility action on the row with id `item`.
    pub fn perform_item(&mut self, item: WidgetId, action: &AccessAction) -> bool {
        match self.list.perform_item(item, action) {
            Some(RowOutcome::Click(RowClick::Item(_))) => self.accept(),
            Some(_) => true,
            None => false,
        }
    }
}

impl Control for WikilinkCompletion {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for WikilinkCompletion {
    fn measure(&mut self, _cx: &mut LayoutCx, _available: Size) -> Size {
        if self.is_visible() {
            Size::new(COMPLETION_WIDTH, self.list.desired_height() + 2.0 * PAD)
        } else {
            Size::ZERO
        }
    }

    /// `bounds` is the window.
    fn arrange(&mut self, _cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = Rect::ZERO;
        if !self.is_visible() {
            return;
        }
        let size = Size::new(
            COMPLETION_WIDTH.min((bounds.width - 2.0 * EDGE).max(0.0)),
            self.list.desired_height() + 2.0 * PAD,
        );
        let panel = place_near(self.anchor, bounds, size, false);
        self.bounds = panel;
        self.list.arrange(Rect::new(
            panel.x + PAD,
            panel.y + PAD,
            (panel.width - 2.0 * PAD).max(0.0),
            self.list.desired_height(),
        ));
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.is_visible() || !cx.needs_paint(self.bounds) {
            return;
        }
        paint_overlay(canvas, self.bounds, cx.theme.tokens.menu_bg, cx.theme);
        self.list.paint(&cx.for_widget(self.list.id()), canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if !self.is_visible() {
            return EventResult::ignored();
        }
        match e {
            Event::Key(key) if key.is_down && key.mods.is_empty() => match key.vk {
                vk::UP | vk::DOWN | vk::PAGE_UP | vk::PAGE_DOWN => {
                    if self.list.key(key) == RowKey::Moved {
                        cx.invalidate(self.bounds);
                    }
                    EventResult::handled()
                }
                vk::RETURN | vk::TAB => {
                    self.accept();
                    cx.invalidate_all();
                    EventResult::handled()
                }
                vk::ESCAPE => {
                    self.dismiss();
                    cx.invalidate_all();
                    EventResult::handled()
                }
                _ => EventResult::ignored(),
            },
            Event::Wheel(wheel) if self.list.wheel(wheel) => {
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            Event::Pointer(pointer) => {
                let inside = self.bounds.contains(pointer.position);
                match pointer.kind {
                    PointerEventKind::Down if !inside => {
                        self.dismiss();
                        cx.invalidate_all();
                        EventResult::ignored()
                    }
                    PointerEventKind::Down if pointer.button == Some(PointerButton::Left) => {
                        if let Some(RowClick::Item(_)) = self.list.click(pointer.position) {
                            self.accept();
                        }
                        cx.invalidate_all();
                        EventResult::handled()
                    }
                    PointerEventKind::Move => {
                        let changed = if inside {
                            self.list.hover_at(pointer.position)
                        } else {
                            self.list.clear_hover()
                        };
                        if changed {
                            cx.invalidate(self.bounds);
                        }
                        if inside {
                            EventResult::handled()
                        } else {
                            EventResult::ignored()
                        }
                    }
                    _ => EventResult::ignored(),
                }
            }
            _ => EventResult::ignored(),
        }
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let name = self.localizer.text("search-wikilink-label");
        if !self.is_visible() {
            let mut node = AccessNode::new(self.id, Role::Group, name, Rect::ZERO);
            node.state.offscreen = true;
            out.insert(node);
            return;
        }
        self.list.access(&cx.for_widget(self.list.id()), out);
        out.insert(
            AccessNode::new(self.id, Role::Group, name, self.bounds)
                .with_children([self.list.id()]),
        );
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}
