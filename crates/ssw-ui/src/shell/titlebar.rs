//! The title row and the command row (spec §1).
//!
//! The title row holds the sidebar toggle and the document tabs ([`DocumentTabs`]); the area at
//! its right end is reserved for the caption buttons that `ssw-win` draws. The command row holds
//! the File / Edit / Format / View menu buttons (they request native popup menus through
//! [`TitleEvent::MenuRequested`]), the B I U pill and the inspector toggle.
//!
//! [`TitleBar`] answers the window-chrome hit test ([`TitleBar::chrome_hit`]): caption
//! (draggable), tabs and buttons (client), caption buttons and resize borders (from
//! [`ShellLayout`]).

use ssw_core::i18n::Localizer;

use crate::access::{AccessAction, AccessNode, AccessState, AccessTree, Role, ToggleState};
use crate::canvas::{Canvas, IconFont};
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::{Event, FocusEvent, vk};
use crate::text::FontWeight;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::button::{Button, ButtonKind};
use crate::widgets::{
    CONTROL_HEIGHT, Control, FONT_SIZE, ICON_SIZE, Interaction, PointerOutcome, control_state,
    draw_centered, focus_ring, outcome_result, perform_focus, ui_style,
};

use super::layout::{COMMAND_ROW_HEIGHT, ChromeHit, ShellLayout, TITLE_ROW_HEIGHT};
use super::tabs::{DocumentTabs, TabStrings, TabsEvent};

/// Segoe Fluent Icons `GlobalNavButton` (the sidebar toggle).
const SIDEBAR_GLYPH: char = '\u{E700}';
/// Segoe Fluent Icons `OpenPane` (the inspector toggle).
const INSPECTOR_GLYPH: char = '\u{E8A0}';
/// Side of the square toggle buttons.
const TOOL_SIZE: f32 = 32.0;
/// Inset of the first and last element from the row edges, and gap between groups.
const ROW_PADDING: f32 = 8.0;
/// Narrowest draggable area kept between the tabs and the caption buttons.
const DRAG_GAP: f32 = 32.0;
/// Gap between neighbouring menu buttons.
const MENU_GAP: f32 = 2.0;
/// Size of one pill item.
const PILL_ITEM: Size = Size::new(32.0, 28.0);
/// Padding inside the pill around its items.
const PILL_PADDING: f32 = 2.0;

/// Localized strings of the title bar. The menu, toggle and format names reuse the command
/// labels of `main.ftl`; the rest lives in `shell.ftl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleStrings {
    /// Accessible name of the whole bar.
    pub title_bar: String,
    /// Accessible name of the title row (tabs and sidebar toggle).
    pub title_row: String,
    /// Accessible name of the command row.
    pub command_row: String,
    /// Accessible name of the B I U pill.
    pub format_pill: String,
    pub sidebar_toggle: String,
    pub inspector_toggle: String,
    pub file: String,
    pub edit: String,
    pub format: String,
    pub view: String,
    pub bold: String,
    pub italic: String,
    pub underline: String,
    /// The letters drawn in the pill (they differ between languages).
    pub bold_face: String,
    pub italic_face: String,
    pub underline_face: String,
    pub tabs: TabStrings,
}

impl TitleStrings {
    /// Looks the strings up in `main.ftl` and `shell.ftl`.
    #[must_use]
    pub fn from_localizer(localizer: &Localizer) -> Self {
        Self {
            title_bar: localizer.text("titlebar-title-bar"),
            title_row: localizer.text("titlebar-tabs"),
            command_row: localizer.text("titlebar-commands"),
            format_pill: localizer.text("titlebar-format-pill"),
            sidebar_toggle: localizer.text("cmd-view-toggle-sidebar"),
            inspector_toggle: localizer.text("cmd-view-toggle-inspector"),
            file: localizer.text("cmd-group-file"),
            edit: localizer.text("cmd-group-edit"),
            format: localizer.text("cmd-group-format"),
            view: localizer.text("cmd-group-view"),
            bold: localizer.text("cmd-format-bold"),
            italic: localizer.text("cmd-format-italic"),
            underline: localizer.text("cmd-format-underline"),
            bold_face: localizer.text("titlebar-bold-face"),
            italic_face: localizer.text("titlebar-italic-face"),
            underline_face: localizer.text("titlebar-underline-face"),
            tabs: TabStrings::from_localizer(localizer),
        }
    }
}

/// The native menus the command row opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuKind {
    File,
    Edit,
    Format,
    View,
}

impl MenuKind {
    /// Left-to-right order in the command row.
    pub const ALL: [MenuKind; 4] = [
        MenuKind::File,
        MenuKind::Edit,
        MenuKind::Format,
        MenuKind::View,
    ];
}

/// The three toggles of the B I U pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormatToggle {
    Bold,
    Italic,
    Underline,
}

/// The formatting at the caret, shown by the pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FormatState {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// What the user did in the title or command row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TitleEvent {
    ToggleSidebar,
    ToggleInspector,
    /// Show the native popup menu `menu` with its top-left corner at `anchor` (the bottom-left of
    /// the button, in window DIPs).
    MenuRequested {
        menu: MenuKind,
        anchor: Point,
    },
    /// A pill item was activated: apply or remove that formatting.
    Format(FormatToggle),
    /// Something happened to the tabs.
    Tabs(TabsEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Face {
    Icon(char),
    Letter {
        bold: bool,
        italic: bool,
        underline: bool,
    },
}

/// A toggle button showing an icon or a styled letter.
#[derive(Debug, Clone)]
struct ToolButton {
    id: WidgetId,
    name: String,
    text: String,
    face: Face,
    on: bool,
    bounds: Rect,
    interaction: Interaction,
    activated: u32,
}

impl ToolButton {
    fn icon(glyph: char, name: &str) -> Self {
        Self::new(Face::Icon(glyph), name, "")
    }

    fn letter(face: &str, name: &str, bold: bool, italic: bool, underline: bool) -> Self {
        Self::new(
            Face::Letter {
                bold,
                italic,
                underline,
            },
            name,
            face,
        )
    }

    fn new(face: Face, name: &str, text: &str) -> Self {
        Self {
            id: WidgetId::allocate(),
            name: name.to_owned(),
            text: text.to_owned(),
            face,
            on: false,
            bounds: Rect::ZERO,
            interaction: Interaction::new(Motion::full()),
            activated: 0,
        }
    }

    fn take_activated(&mut self) -> bool {
        std::mem::take(&mut self.activated) > 0
    }

    fn activate(&mut self) {
        self.activated += 1;
    }
}

impl Control for ToolButton {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for ToolButton {
    fn measure(&mut self, _cx: &mut LayoutCx, _available: Size) -> Size {
        Size::new(TOOL_SIZE, TOOL_SIZE)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        cx.add_tab_stop();
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        let radius = tokens.metrics.control_radius;
        let base = if self.on {
            tokens.pressed_bg
        } else {
            tokens.hover_bg.with_alpha(0.0)
        };
        let fill = self
            .interaction
            .blend(base, tokens.hover_bg, tokens.pressed_bg);
        canvas.fill_rounded_rect(self.bounds, CornerRadii::uniform(radius), fill);
        let color = if self.on { tokens.accent } else { tokens.text };
        match self.face {
            Face::Icon(glyph) => canvas.draw_icon(
                glyph,
                IconFont::SegoeFluent,
                ICON_SIZE,
                color,
                self.bounds.center(),
            ),
            Face::Letter {
                bold,
                italic,
                underline,
            } => {
                let weight = if bold {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                };
                let style = ui_style(cx.theme, FONT_SIZE)
                    .with_weight(weight)
                    .with_italic(italic)
                    .with_underline(underline);
                let layout = cx.text.layout(&self.text, &style, &[], &[], f32::INFINITY);
                draw_centered(canvas, layout.as_ref(), self.bounds, color);
            }
        }
        if cx.is_focused() {
            focus_ring::paint(canvas, self.bounds, radius, cx.theme);
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => {
                let outcome = self.interaction.pointer(cx, self.bounds, event, true);
                if outcome == PointerOutcome::Clicked {
                    self.activate();
                }
                outcome_result(outcome)
            }
            Event::Key(key)
                if key.is_down
                    && cx.is_focused()
                    && matches!(key.vk, vk::SPACE | vk::RETURN)
                    && key.mods.is_empty() =>
            {
                if !key.repeat {
                    self.activate();
                }
                EventResult::handled()
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) => {
                cx.invalidate(self.bounds.outset(Insets::uniform(2.0)));
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
        let mut state: AccessState = control_state(cx, true);
        state.toggle = Some(if self.on {
            ToggleState::On
        } else {
            ToggleState::Off
        });
        let mut node = AccessNode::new(self.id, Role::ToggleButton, self.name.clone(), self.bounds)
            .with_state(state);
        node.patterns.toggle = true;
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::Invoke | AccessAction::Toggle => {
                self.activate();
                true
            }
            AccessAction::Focus => perform_focus(cx),
            _ => false,
        }
    }
}

/// The title row and command row.
#[derive(Debug, Clone)]
pub struct TitleBar {
    id: WidgetId,
    title_row_id: WidgetId,
    command_row_id: WidgetId,
    pill_id: WidgetId,
    strings: TitleStrings,
    sidebar: ToolButton,
    tabs: DocumentTabs,
    menus: Vec<(MenuKind, Button)>,
    bold: ToolButton,
    italic: ToolButton,
    underline: ToolButton,
    inspector: ToolButton,
    pill_visible: bool,
    caption_width: f32,
    bounds: Rect,
    title_rect: Rect,
    command_rect: Rect,
    pill_rect: Rect,
    events: Vec<TitleEvent>,
}

impl TitleBar {
    /// A title bar with no tabs, the pill shown and no caption buttons reserved.
    #[must_use]
    pub fn new(strings: TitleStrings) -> Self {
        let menus = MenuKind::ALL
            .into_iter()
            .map(|menu| {
                let label = match menu {
                    MenuKind::File => &strings.file,
                    MenuKind::Edit => &strings.edit,
                    MenuKind::Format => &strings.format,
                    MenuKind::View => &strings.view,
                };
                (menu, Button::new(ButtonKind::Subtle, label))
            })
            .collect();
        Self {
            id: WidgetId::allocate(),
            title_row_id: WidgetId::allocate(),
            command_row_id: WidgetId::allocate(),
            pill_id: WidgetId::allocate(),
            sidebar: ToolButton::icon(SIDEBAR_GLYPH, &strings.sidebar_toggle),
            tabs: DocumentTabs::new(&strings.tabs),
            menus,
            bold: ToolButton::letter(&strings.bold_face, &strings.bold, true, false, false),
            italic: ToolButton::letter(&strings.italic_face, &strings.italic, false, true, false),
            underline: ToolButton::letter(
                &strings.underline_face,
                &strings.underline,
                false,
                false,
                true,
            ),
            inspector: ToolButton::icon(INSPECTOR_GLYPH, &strings.inspector_toggle),
            strings,
            pill_visible: true,
            caption_width: 0.0,
            bounds: Rect::ZERO,
            title_rect: Rect::ZERO,
            command_rect: Rect::ZERO,
            pill_rect: Rect::ZERO,
            events: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    /// Applies the motion preference to every animated child.
    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.sidebar.interaction.set_motion(motion);
        self.inspector.interaction.set_motion(motion);
        self.bold.interaction.set_motion(motion);
        self.italic.interaction.set_motion(motion);
        self.underline.interaction.set_motion(motion);
        self.menus = self
            .menus
            .into_iter()
            .map(|(menu, button)| (menu, button.with_motion(motion)))
            .collect();
        self.tabs = self.tabs.with_motion(motion);
        self
    }

    /// The tabs of the title row.
    #[must_use]
    pub fn tabs(&self) -> &DocumentTabs {
        &self.tabs
    }

    pub fn tabs_mut(&mut self) -> &mut DocumentTabs {
        &mut self.tabs
    }

    /// Width reserved at the right end of the title row for the caption buttons
    /// ([`ShellRects::caption_buttons`](super::layout::ShellRects::caption_buttons)); 0 when the
    /// window has none. Takes effect on the next arrange.
    pub fn set_caption_width(&mut self, width: f32) {
        self.caption_width = width;
    }

    /// Shows or hides the B I U pill (View → Formatting pill).
    pub fn set_pill_visible(&mut self, visible: bool) {
        self.pill_visible = visible;
    }

    #[must_use]
    pub fn pill_visible(&self) -> bool {
        self.pill_visible
    }

    /// Shows which panes are open on the two toggles.
    pub fn set_panes_open(&mut self, sidebar: bool, inspector: bool) {
        self.sidebar.on = sidebar;
        self.inspector.on = inspector;
    }

    /// The formatting at the caret.
    pub fn set_format_state(&mut self, state: FormatState) {
        self.bold.on = state.bold;
        self.italic.on = state.italic;
        self.underline.on = state.underline;
    }

    #[must_use]
    pub fn format_state(&self) -> FormatState {
        FormatState {
            bold: self.bold.on,
            italic: self.italic.on,
            underline: self.underline.on,
        }
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The title row rectangle (valid after arrange).
    #[must_use]
    pub fn title_rect(&self) -> Rect {
        self.title_rect
    }

    /// The command row rectangle (valid after arrange).
    #[must_use]
    pub fn command_rect(&self) -> Rect {
        self.command_rect
    }

    #[must_use]
    pub fn sidebar_toggle_rect(&self) -> Rect {
        self.sidebar.bounds
    }

    #[must_use]
    pub fn inspector_toggle_rect(&self) -> Rect {
        self.inspector.bounds
    }

    /// The pill container; empty while the pill is hidden.
    #[must_use]
    pub fn pill_rect(&self) -> Rect {
        if self.pill_visible {
            self.pill_rect
        } else {
            Rect::ZERO
        }
    }

    /// The rectangle of one menu button.
    #[must_use]
    pub fn menu_rect(&self, menu: MenuKind) -> Rect {
        self.menus
            .iter()
            .find(|(kind, _)| *kind == menu)
            .map_or(Rect::ZERO, |(_, button)| button.bounds())
    }

    /// The rectangle of one pill item; empty while the pill is hidden.
    #[must_use]
    pub fn pill_item_rect(&self, toggle: FormatToggle) -> Rect {
        if !self.pill_visible {
            return Rect::ZERO;
        }
        match toggle {
            FormatToggle::Bold => self.bold.bounds,
            FormatToggle::Italic => self.italic.bounds,
            FormatToggle::Underline => self.underline.bounds,
        }
    }

    /// The `WM_NCHITTEST` answer for `point`: [`ShellLayout::chrome_hit`] with the draggable
    /// caption narrowed to the title row's empty space (the sidebar toggle, the tabs, the
    /// new-tab button and the close buttons are client area).
    #[must_use]
    pub fn chrome_hit(&self, layout: &ShellLayout, point: Point) -> ChromeHit {
        match layout.chrome_hit(point) {
            ChromeHit::Caption
                if self.sidebar.bounds.contains(point) || self.tabs.hit_test(point).is_some() =>
            {
                ChromeHit::Client
            }
            hit => hit,
        }
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<TitleEvent> {
        std::mem::take(&mut self.events)
    }

    fn children_mut(&mut self) -> Vec<&mut dyn Control> {
        let mut list: Vec<&mut dyn Control> = vec![&mut self.sidebar, &mut self.tabs];
        for (_, button) in &mut self.menus {
            list.push(button);
        }
        if self.pill_visible {
            list.push(&mut self.bold);
            list.push(&mut self.italic);
            list.push(&mut self.underline);
        }
        list.push(&mut self.inspector);
        list
    }

    fn collect(&mut self) {
        if self.sidebar.take_activated() {
            self.events.push(TitleEvent::ToggleSidebar);
        }
        if self.inspector.take_activated() {
            self.events.push(TitleEvent::ToggleInspector);
        }
        for (menu, button) in &mut self.menus {
            if button.take_activated() {
                let bounds = button.bounds();
                self.events.push(TitleEvent::MenuRequested {
                    menu: *menu,
                    anchor: Point::new(bounds.x, bounds.bottom()),
                });
            }
        }
        for (toggle, button) in [
            (FormatToggle::Bold, &mut self.bold),
            (FormatToggle::Italic, &mut self.italic),
            (FormatToggle::Underline, &mut self.underline),
        ] {
            if button.take_activated() {
                self.events.push(TitleEvent::Format(toggle));
            }
        }
        for event in self.tabs.take_events() {
            self.events.push(TitleEvent::Tabs(event));
        }
    }

    /// Carries out an access action on the child whose access node has id `item` (a menu button,
    /// toggle, pill item or tab item).
    pub fn perform_item(
        &mut self,
        cx: &mut EventCx,
        item: WidgetId,
        action: &AccessAction,
    ) -> bool {
        let mut performed = None;
        for child in self.children_mut() {
            if child.id() == item {
                performed = Some(child.perform(&mut cx.for_widget(item), action));
                break;
            }
        }
        // Not a direct child: it may be one of the tab items.
        let done = match performed {
            Some(done) => done,
            None => self.tabs.perform_item(cx, item, action),
        };
        self.collect();
        done
    }
}

impl Control for TitleBar {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for TitleBar {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        let width = if available.width.is_finite() {
            available.width
        } else {
            0.0
        };
        Size::new(width, TITLE_ROW_HEIGHT + COMMAND_ROW_HEIGHT)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        let title = Rect::new(
            bounds.x,
            bounds.y,
            bounds.width,
            TITLE_ROW_HEIGHT.min(bounds.height),
        );
        let command = Rect::new(
            bounds.x,
            title.bottom(),
            bounds.width,
            (bounds.height - title.height).max(0.0),
        );
        self.title_rect = title;
        self.command_rect = command;

        // Title row: sidebar toggle, tabs, reserved caption buttons.
        let toggle_top = title.y + (title.height - TOOL_SIZE) / 2.0;
        let toggle = Rect::new(title.x + ROW_PADDING, toggle_top, TOOL_SIZE, TOOL_SIZE);
        let id = self.sidebar.id;
        self.sidebar.arrange(&mut cx.for_widget(id), toggle);
        let tabs_x = toggle.right() + ROW_PADDING;
        let tabs_width = (title.right() - self.caption_width - DRAG_GAP - tabs_x).max(0.0);
        let id = self.tabs.id();
        self.tabs.arrange(
            &mut cx.for_widget(id),
            Rect::new(tabs_x, title.y, tabs_width, title.height),
        );

        // Command row: menus on the left, pill and inspector toggle on the right.
        let row_top = command.y + (command.height - CONTROL_HEIGHT) / 2.0;
        let mut x = command.x + ROW_PADDING;
        for (_, button) in &mut self.menus {
            let id = button.id();
            let width = button
                .measure(&mut cx.for_widget(id), Size::UNBOUNDED)
                .width;
            button.arrange(
                &mut cx.for_widget(id),
                Rect::new(x, row_top, width, CONTROL_HEIGHT),
            );
            x += width + MENU_GAP;
        }
        if self.pill_visible {
            let width = 3.0 * PILL_ITEM.width + 2.0 * PILL_PADDING;
            let height = PILL_ITEM.height + 2.0 * PILL_PADDING;
            let pill_x = command.right() - ROW_PADDING - TOOL_SIZE - ROW_PADDING - width;
            self.pill_rect = Rect::new(
                pill_x,
                command.y + (command.height - height) / 2.0,
                width,
                height,
            );
            let item_top = self.pill_rect.y + PILL_PADDING;
            let mut item_x = self.pill_rect.x + PILL_PADDING;
            for button in [&mut self.bold, &mut self.italic, &mut self.underline] {
                let id = button.id;
                button.arrange(
                    &mut cx.for_widget(id),
                    Rect::new(item_x, item_top, PILL_ITEM.width, PILL_ITEM.height),
                );
                item_x += PILL_ITEM.width;
            }
        }
        let id = self.inspector.id;
        self.inspector.arrange(
            &mut cx.for_widget(id),
            Rect::new(
                command.right() - ROW_PADDING - TOOL_SIZE,
                row_top,
                TOOL_SIZE,
                TOOL_SIZE,
            ),
        );
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        self.sidebar.paint(&cx.for_widget(self.sidebar.id), canvas);
        self.tabs.paint(&cx.for_widget(self.tabs.id()), canvas);
        for (_, button) in &self.menus {
            button.paint(&cx.for_widget(button.id()), canvas);
        }
        if self.pill_visible {
            let radii = CornerRadii::uniform(tokens.metrics.control_radius + PILL_PADDING);
            canvas.fill_rounded_rect(self.pill_rect, radii, tokens.surface);
            canvas.stroke_rounded_rect(
                self.pill_rect.inset(Insets::uniform(0.5)),
                radii,
                tokens.hairline,
                1.0,
            );
            for button in [&self.bold, &self.italic, &self.underline] {
                button.paint(&cx.for_widget(button.id), canvas);
            }
        }
        self.inspector
            .paint(&cx.for_widget(self.inspector.id), canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let mut result = EventResult::ignored();
        for child in self.children_mut() {
            let id = child.id();
            result = result.or(child.event(&mut cx.for_widget(id), e));
        }
        self.collect();
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        self.sidebar.access(&cx.for_widget(self.sidebar.id), out);
        self.tabs.access(&cx.for_widget(self.tabs.id()), out);
        out.insert(
            AccessNode::new(
                self.title_row_id,
                Role::TitleBar,
                self.strings.title_row.clone(),
                self.title_rect,
            )
            .with_children([self.sidebar.id, self.tabs.id()]),
        );

        let mut command_children = Vec::new();
        for (_, button) in &self.menus {
            button.access(&cx.for_widget(button.id()), out);
            command_children.push(button.id());
        }
        if self.pill_visible {
            let items = [&self.bold, &self.italic, &self.underline];
            for button in items {
                button.access(&cx.for_widget(button.id), out);
            }
            out.insert(
                AccessNode::new(
                    self.pill_id,
                    Role::Group,
                    self.strings.format_pill.clone(),
                    self.pill_rect,
                )
                .with_children(items.map(|button| button.id)),
            );
            command_children.push(self.pill_id);
        }
        self.inspector
            .access(&cx.for_widget(self.inspector.id), out);
        command_children.push(self.inspector.id);
        out.insert(
            AccessNode::new(
                self.command_row_id,
                Role::ToolBar,
                self.strings.command_row.clone(),
                self.command_rect,
            )
            .with_children(command_children),
        );

        out.insert(
            AccessNode::new(
                self.id,
                Role::Group,
                self.strings.title_bar.clone(),
                self.bounds,
            )
            .with_children([self.title_row_id, self.command_row_id]),
        );
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::input::{KeyEvent, Modifiers, PointerButton, PointerEventKind};
    use crate::shell::layout::CaptionButton;
    use crate::widgets::testkit::Harness;

    const MAIN_FTL: &str = include_str!("../../../../assets/i18n/en/main.ftl");
    const SHELL_FTL: &str = include_str!("../../../../assets/i18n/en/shell.ftl");

    fn strings() -> TitleStrings {
        let localizer = Localizer::new("en", &[("en", MAIN_FTL), ("en", SHELL_FTL)]);
        assert!(
            localizer.diagnostics().is_empty(),
            "{:?}",
            localizer.diagnostics()
        );
        TitleStrings::from_localizer(&localizer)
    }

    /// A 1000 DIP wide bar with caption buttons reserved, and a layout with the same width.
    fn setup() -> (Harness, TitleBar, ShellLayout) {
        let mut harness = Harness::new();
        let mut bar = TitleBar::new(strings()).with_motion(Motion::reduced());
        bar.tabs_mut().add_file(1, "C:\\notes\\todo.md");
        bar.tabs_mut().add_file(2, "C:\\notes\\data.csv");
        let layout = ShellLayout::new(Size::new(1000.0, 600.0), 1.0);
        bar.set_caption_width(138.0);
        harness.arrange(&mut bar, Rect::new(0.0, 0.0, 1000.0, 80.0));
        (harness, bar, layout)
    }

    #[test]
    fn strings_reuse_command_labels_and_shell_ids() {
        let strings = strings();
        assert_eq!(strings.file, "File");
        assert_eq!(strings.edit, "Edit");
        assert_eq!(strings.format, "Format");
        assert_eq!(strings.view, "View");
        assert_eq!(strings.bold, "Bold");
        assert_eq!(strings.sidebar_toggle, "Sidebar");
        assert_eq!(strings.inspector_toggle, "Inspector");
        assert_eq!(strings.bold_face, "B");
        assert_eq!(strings.italic_face, "I");
        assert_eq!(strings.underline_face, "U");
        assert!(!strings.title_bar.starts_with("titlebar-"));
        assert!(!strings.title_row.starts_with("titlebar-"));
        assert!(!strings.command_row.starts_with("titlebar-"));
        assert!(!strings.format_pill.starts_with("titlebar-"));
        assert_eq!(strings.tabs.untitled, "Untitled");
    }

    #[test]
    fn rows_arrange_toggles_tabs_menus_pill_and_inspector() {
        let (_, bar, _) = setup();
        assert_eq!(bar.title_rect(), Rect::new(0.0, 0.0, 1000.0, 40.0));
        assert_eq!(bar.command_rect(), Rect::new(0.0, 40.0, 1000.0, 40.0));
        assert_eq!(bar.sidebar_toggle_rect(), Rect::new(8.0, 4.0, 32.0, 32.0));
        // Tabs start after the toggle and stop before the draggable gap and the caption buttons.
        assert_eq!(
            bar.tabs().strip().bounds(),
            Rect::new(48.0, 0.0, 782.0, 40.0)
        );
        // Menus: 8 DIP per glyph + 22 padding, 2 DIP apart, vertically centered in the row.
        assert_eq!(
            bar.menu_rect(MenuKind::File),
            Rect::new(8.0, 44.0, 54.0, 32.0)
        );
        assert_eq!(
            bar.menu_rect(MenuKind::Edit),
            Rect::new(64.0, 44.0, 54.0, 32.0)
        );
        assert_eq!(
            bar.menu_rect(MenuKind::Format),
            Rect::new(120.0, 44.0, 70.0, 32.0)
        );
        assert_eq!(
            bar.menu_rect(MenuKind::View),
            Rect::new(192.0, 44.0, 54.0, 32.0)
        );
        assert_eq!(
            bar.inspector_toggle_rect(),
            Rect::new(960.0, 44.0, 32.0, 32.0)
        );
        assert_eq!(bar.pill_rect(), Rect::new(852.0, 44.0, 100.0, 32.0));
        assert_eq!(
            bar.pill_item_rect(FormatToggle::Bold),
            Rect::new(854.0, 46.0, 32.0, 28.0)
        );
        assert_eq!(
            bar.pill_item_rect(FormatToggle::Italic),
            Rect::new(886.0, 46.0, 32.0, 28.0)
        );
        assert_eq!(
            bar.pill_item_rect(FormatToggle::Underline),
            Rect::new(918.0, 46.0, 32.0, 28.0)
        );
        assert_eq!(bar.bounds(), Rect::new(0.0, 0.0, 1000.0, 80.0));
    }

    #[test]
    fn measuring_asks_for_both_rows() {
        let mut harness = Harness::new();
        let mut bar = TitleBar::new(strings());
        let size = harness.place(&mut bar, Point::ZERO).size();
        assert_eq!(size, Size::new(0.0, 80.0));
        let mut cx = LayoutCx::new(
            bar.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &mut harness.focus,
            &mut harness.requests,
        );
        assert_eq!(
            bar.measure(&mut cx, Size::new(640.0, 200.0)),
            Size::new(640.0, 80.0)
        );
    }

    #[test]
    fn tab_order_follows_the_visual_order() {
        let (harness, bar, _) = setup();
        let order = harness.focus.tab_order();
        // toggle, tabs, 4 menus, 3 pill items, inspector toggle, and the strip's new-tab button.
        assert_eq!(order.len(), 11);
        assert_eq!(order[0], bar.sidebar.id);
        assert_eq!(order[1], bar.tabs().id());
        assert_eq!(order[order.len() - 1], bar.inspector.id);
    }

    #[test]
    fn menu_buttons_request_native_menus_below_themselves() {
        let (mut harness, mut bar, _) = setup();
        for menu in MenuKind::ALL {
            let rect = bar.menu_rect(menu);
            harness.click(&mut bar, rect.center());
            assert_eq!(
                bar.take_events(),
                vec![TitleEvent::MenuRequested {
                    menu,
                    anchor: Point::new(rect.x, rect.bottom()),
                }]
            );
        }
        // The keyboard activates a focused menu button too.
        let id = bar.menus[2].1.id();
        harness.focus.focus(id);
        assert!(harness.key(&mut bar, vk::RETURN).handled);
        let rect = bar.menu_rect(MenuKind::Format);
        assert_eq!(
            bar.take_events(),
            vec![TitleEvent::MenuRequested {
                menu: MenuKind::Format,
                anchor: Point::new(rect.x, rect.bottom()),
            }]
        );
    }

    #[test]
    fn toggles_emit_pane_events_and_show_their_state() {
        let (mut harness, mut bar, _) = setup();
        let sidebar = bar.sidebar_toggle_rect().center();
        let inspector = bar.inspector_toggle_rect().center();
        harness.click(&mut bar, sidebar);
        harness.click(&mut bar, inspector);
        assert_eq!(
            bar.take_events(),
            vec![TitleEvent::ToggleSidebar, TitleEvent::ToggleInspector]
        );

        bar.set_panes_open(true, false);
        let tree = harness.tree(&bar);
        let sidebar_node = tree
            .nodes()
            .iter()
            .find(|node| node.name == "Sidebar")
            .expect("sidebar toggle node");
        assert_eq!(sidebar_node.role, Role::ToggleButton);
        assert_eq!(sidebar_node.state.toggle, Some(ToggleState::On));
        let inspector_node = tree
            .nodes()
            .iter()
            .find(|node| node.name == "Inspector")
            .expect("inspector toggle node");
        assert_eq!(inspector_node.state.toggle, Some(ToggleState::Off));
    }

    #[test]
    fn pill_items_report_and_reflect_formatting() {
        let (mut harness, mut bar, _) = setup();
        for toggle in [
            FormatToggle::Bold,
            FormatToggle::Italic,
            FormatToggle::Underline,
        ] {
            let center = bar.pill_item_rect(toggle).center();
            harness.click(&mut bar, center);
            assert_eq!(bar.take_events(), vec![TitleEvent::Format(toggle)]);
        }
        // Clicking does not change the shown state: the app answers with the caret's formatting.
        assert_eq!(bar.format_state(), FormatState::default());
        let state = FormatState {
            bold: true,
            italic: false,
            underline: true,
        };
        bar.set_format_state(state);
        assert_eq!(bar.format_state(), state);
        let tree = harness.tree(&bar);
        let toggled = |name: &str| {
            tree.nodes()
                .iter()
                .find(|node| node.name == name)
                .and_then(|node| node.state.toggle)
        };
        assert_eq!(toggled("Bold"), Some(ToggleState::On));
        assert_eq!(toggled("Italic"), Some(ToggleState::Off));
        assert_eq!(toggled("Underline"), Some(ToggleState::On));
    }

    #[test]
    fn pill_keyboard_and_access_activation() {
        let (mut harness, mut bar, _) = setup();
        let bold = bar.bold.id;
        harness.focus.focus(bold);
        assert!(harness.key(&mut bar, vk::SPACE).handled);
        let repeat = KeyEvent {
            repeat: true,
            ..KeyEvent::press(vk::SPACE, Modifiers::NONE)
        };
        assert!(harness.send(&mut bar, Event::Key(repeat)).handled);
        assert!(!harness.key(&mut bar, vk::TAB).handled);
        assert_eq!(
            bar.take_events(),
            vec![TitleEvent::Format(FormatToggle::Bold)]
        );

        let italic = bar.italic.id;
        let mut cx = harness.event_cx(bar.id());
        assert!(bar.perform_item(&mut cx, italic, &AccessAction::Toggle));
        let mut cx = harness.event_cx(bar.id());
        assert!(bar.perform_item(&mut cx, bar.underline.id, &AccessAction::Invoke));
        let mut cx = harness.event_cx(bar.id());
        assert!(!bar.perform_item(&mut cx, bar.bold.id, &AccessAction::Expand));
        assert_eq!(
            bar.take_events(),
            vec![
                TitleEvent::Format(FormatToggle::Italic),
                TitleEvent::Format(FormatToggle::Underline)
            ]
        );
        let mut cx = harness.event_cx(bar.id());
        assert!(bar.perform_item(&mut cx, bar.bold.id, &AccessAction::Focus));
        assert!(harness.focus.is_focused(bar.bold.id));
    }

    #[test]
    fn menu_buttons_and_tab_items_perform_through_the_bar() {
        let (mut harness, mut bar, _) = setup();
        let file = bar.menus[0].1.id();
        let mut cx = harness.event_cx(bar.id());
        assert!(bar.perform_item(&mut cx, file, &AccessAction::Invoke));
        assert!(matches!(
            bar.take_events().as_slice(),
            [TitleEvent::MenuRequested {
                menu: MenuKind::File,
                ..
            }]
        ));
        let tree = harness.tree(&bar);
        let tab = tree
            .nodes()
            .iter()
            .find(|node| node.role == Role::TabItem && node.name == "data.csv")
            .expect("tab item")
            .id;
        let mut cx = harness.event_cx(bar.id());
        assert!(bar.perform_item(&mut cx, tab, &AccessAction::Select));
        assert_eq!(
            bar.take_events(),
            vec![TitleEvent::Tabs(TabsEvent::Activated(2))]
        );
        let mut cx = harness.event_cx(bar.id());
        assert!(!bar.perform_item(&mut cx, WidgetId::new(u64::MAX), &AccessAction::Select));
        let mut cx = harness.event_cx(bar.id());
        assert!(!bar.perform(&mut cx, &AccessAction::Focus));
    }

    #[test]
    fn tab_events_are_forwarded() {
        let (mut harness, mut bar, _) = setup();
        harness.click(&mut bar, Point::new(300.0, 20.0));
        assert_eq!(
            bar.take_events(),
            vec![TitleEvent::Tabs(TabsEvent::Activated(2))]
        );
        let new_tab = bar.tabs().strip().new_tab_rect().center();
        harness.click(&mut bar, new_tab);
        assert_eq!(bar.take_events(), vec![TitleEvent::Tabs(TabsEvent::NewTab)]);
    }

    #[test]
    fn chrome_hit_keeps_the_empty_caption_draggable() {
        let (_, bar, layout) = setup();
        // Empty space between the new-tab button and the caption buttons.
        assert_eq!(
            bar.chrome_hit(&layout, Point::new(800.0, 20.0)),
            ChromeHit::Caption
        );
        // The sidebar toggle, a tab and the new-tab button are client area.
        assert_eq!(
            bar.chrome_hit(&layout, Point::new(20.0, 20.0)),
            ChromeHit::Client
        );
        assert_eq!(
            bar.chrome_hit(&layout, Point::new(100.0, 20.0)),
            ChromeHit::Client
        );
        let new_tab = bar.tabs().strip().new_tab_rect().center();
        assert_eq!(bar.chrome_hit(&layout, new_tab), ChromeHit::Client);
        // Free space between the tabs and the caption buttons.
        assert_eq!(
            bar.chrome_hit(&layout, Point::new(600.0, 20.0)),
            ChromeHit::Caption
        );
        // Caption buttons and resize borders pass through from the layout.
        assert_eq!(
            bar.chrome_hit(&layout, Point::new(950.0, 20.0)),
            ChromeHit::CaptionButton(CaptionButton::Maximize)
        );
        assert!(matches!(
            bar.chrome_hit(&layout, Point::new(500.0, 1.0)),
            ChromeHit::Resize(_)
        ));
        // The command row is never caption.
        assert_eq!(
            bar.chrome_hit(&layout, Point::new(500.0, 60.0)),
            ChromeHit::Client
        );
    }

    #[test]
    fn a_hidden_pill_takes_no_space_focus_or_nodes() {
        let (mut harness, mut bar, _) = setup();
        bar.set_pill_visible(false);
        assert!(!bar.pill_visible());
        harness.arrange(&mut bar, Rect::new(0.0, 0.0, 1000.0, 80.0));
        assert_eq!(bar.pill_rect(), Rect::ZERO);
        assert_eq!(bar.pill_item_rect(FormatToggle::Bold), Rect::ZERO);
        assert_eq!(harness.focus.tab_order().len(), 8);
        // A click where the pill used to be does nothing.
        harness.click(&mut bar, Point::new(870.0, 60.0));
        assert!(bar.take_events().is_empty());
        let tree = harness.tree(&bar);
        assert!(tree.nodes().iter().all(|node| node.name != "Bold"));
        let canvas = harness.paint(&bar);
        assert!(!canvas.texts().contains(&"B"));
        bar.set_pill_visible(true);
        assert!(bar.pill_visible());
    }

    #[test]
    fn access_tree_nests_rows_toolbar_and_pill() {
        let (harness, bar, _) = setup();
        let tree = harness.tree(&bar);
        let root = tree.root().expect("root");
        assert_eq!(root.role, Role::Group);
        assert_eq!(root.name, "Title bar");
        let rows: Vec<_> = tree.children(bar.id()).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].role, Role::TitleBar);
        assert_eq!(rows[1].role, Role::ToolBar);
        let title_children: Vec<Role> = tree.children(rows[0].id).map(|n| n.role).collect();
        assert_eq!(title_children, vec![Role::ToggleButton, Role::Tab]);
        let command_children: Vec<_> = tree.children(rows[1].id).collect();
        let names: Vec<&str> = command_children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names[..4], ["File", "Edit", "Format", "View"]);
        assert_eq!(command_children[4].role, Role::Group);
        assert_eq!(tree.children(command_children[4].id).count(), 3);
        assert_eq!(command_children[5].name, "Inspector");
        // Every node is reachable from the root.
        assert_eq!(tree.walk().len(), tree.len());
    }

    #[test]
    fn painting_draws_menus_pill_letters_and_toggle_glyphs() {
        let (mut harness, mut bar, _) = setup();
        bar.set_panes_open(true, true);
        bar.set_format_state(FormatState {
            bold: true,
            ..FormatState::default()
        });
        let canvas = harness.paint(&bar);
        let texts = canvas.texts();
        for expected in [
            "todo", "data.csv", "File", "Edit", "Format", "View", "B", "I", "U",
        ] {
            assert!(texts.contains(&expected), "missing {expected}: {texts:?}");
        }
        let glyphs: Vec<char> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::Icon { glyph, .. } => Some(*glyph),
                _ => None,
            })
            .collect();
        assert!(glyphs.contains(&SIDEBAR_GLYPH));
        assert!(glyphs.contains(&INSPECTOR_GLYPH));
        // The pill container is filled with the surface color.
        let tokens = harness.theme.tokens;
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::FillRoundedRect { rect, color, .. }
                if *rect == bar.pill_rect() && *color == tokens.surface
        )));
        // The active bold item is tinted with the accent color.
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::Text { text, color, .. } if text == "B" && *color == tokens.accent
        )));
    }

    #[test]
    fn hover_animates_and_focus_changes_repaint() {
        let (mut harness, mut bar, _) = setup();
        let center = bar.sidebar_toggle_rect().center();
        harness.hover(&mut bar, center);
        harness.frame(&mut bar, 16);
        harness.send(&mut bar, Event::Focus(FocusEvent::Gained));
        harness.send(&mut bar, Event::Focus(FocusEvent::Lost));
        harness.pointer(
            &mut bar,
            PointerEventKind::Down,
            Some(PointerButton::Right),
            center,
        );
        assert!(bar.take_events().is_empty());
        harness.focus.focus(bar.sidebar.id);
        let canvas = harness.paint(&bar);
        assert!(canvas.is_balanced());
    }
}
