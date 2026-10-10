//! Shared harness of the `search_*` integration tests: a minimal host that lays a widget out,
//! routes keyboard, text and pointer events, paints on a [`RecordingCanvas`] and builds the
//! accessibility tree, plus the real English Fluent messages.

// Every test crate uses a different subset of the helpers.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use ssw_core::command::CommandId;
use ssw_core::i18n::Localizer;
use ssw_ui::access::{AccessNode, AccessTree};
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{
    Event, KeyEvent, Modifiers, PointerButton, PointerEvent, PointerEventKind, TextInput,
};
use ssw_ui::services::MemoryClipboard;
use ssw_ui::shell::palette::ShortcutLabels;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widget::{
    AccessCx, EventCx, EventResult, FocusManager, LayoutCx, PaintCx, PointerCapture, Requests,
    WidgetId,
};
use ssw_ui::widgets::{Control, SharedClipboard};

const MAIN_FTL: &str = include_str!("../../../../assets/i18n/en/main.ftl");
const SEARCH_FTL: &str = include_str!("../../../../assets/i18n/en/search.ftl");

/// The English messages of the commands and of the search area.
pub fn localizer() -> Rc<Localizer> {
    let localizer = Localizer::new("en", &[("en", MAIN_FTL), ("en", SEARCH_FTL)]);
    assert!(
        localizer.diagnostics().is_empty(),
        "{:?}",
        localizer.diagnostics()
    );
    Rc::new(localizer)
}

pub fn clipboard() -> SharedClipboard {
    Rc::new(RefCell::new(MemoryClipboard::default()))
}

/// Shortcut texts for two commands.
pub struct Shortcuts;

impl ShortcutLabels for Shortcuts {
    fn shortcut_label(&self, command: CommandId) -> Option<String> {
        match command {
            CommandId::FormatBold => Some("Ctrl+B".to_owned()),
            CommandId::SearchCommandPalette => Some("Ctrl+Shift+P".to_owned()),
            CommandId::FormatHeading1 => Some("Ctrl+1".to_owned()),
            _ => None,
        }
    }
}

pub struct Host {
    pub theme: Theme,
    pub engine: MonoTextEngine,
    pub focus: FocusManager,
    pub capture: PointerCapture,
    pub requests: Requests,
}

impl Host {
    pub fn new() -> Self {
        Self {
            theme: Theme::light(DEFAULT_ACCENT),
            engine: MonoTextEngine::default(),
            focus: FocusManager::new(),
            capture: PointerCapture::new(),
            requests: Requests::new(),
        }
    }

    pub fn cx(&mut self, id: WidgetId) -> EventCx<'_> {
        EventCx::new(
            id,
            &self.theme,
            1.0,
            &self.engine,
            &mut self.focus,
            &mut self.capture,
            &mut self.requests,
        )
    }

    /// Arranges `widget` in `bounds` and commits the tab order it registered.
    pub fn layout(&mut self, widget: &mut dyn Control, bounds: Rect) {
        let id = widget.id();
        self.focus.begin_tab_order();
        let mut cx = LayoutCx::new(
            id,
            &self.theme,
            1.0,
            &self.engine,
            &mut self.focus,
            &mut self.requests,
        );
        widget.arrange(&mut cx, bounds);
        self.focus.commit_tab_order();
    }

    /// The size `widget` wants within `available`.
    pub fn measure(&mut self, widget: &mut dyn Control, available: Size) -> Size {
        let id = widget.id();
        let mut cx = LayoutCx::new(
            id,
            &self.theme,
            1.0,
            &self.engine,
            &mut self.focus,
            &mut self.requests,
        );
        widget.measure(&mut cx, available)
    }

    /// Gives keyboard focus to `id`, which must be in the tab order.
    pub fn focus_on(&mut self, id: WidgetId) {
        self.focus.focus(id);
        assert!(self.focus.is_focused(id), "widget is not in the tab order");
    }

    pub fn send(&mut self, widget: &mut dyn Control, event: Event) -> EventResult {
        let id = widget.id();
        let mut cx = self.cx(id);
        widget.event(&mut cx, &event)
    }

    pub fn key(&mut self, widget: &mut dyn Control, vk: u16) -> EventResult {
        self.key_with(widget, vk, Modifiers::NONE)
    }

    pub fn key_with(&mut self, widget: &mut dyn Control, vk: u16, mods: Modifiers) -> EventResult {
        self.send(widget, Event::Key(KeyEvent::press(vk, mods)))
    }

    pub fn text(&mut self, widget: &mut dyn Control, text: &str) -> EventResult {
        self.send(widget, Event::Text(TextInput(text.to_owned())))
    }

    pub fn pointer(
        &mut self,
        widget: &mut dyn Control,
        kind: PointerEventKind,
        at: Point,
    ) -> EventResult {
        let button = matches!(kind, PointerEventKind::Down | PointerEventKind::Up)
            .then_some(PointerButton::Left);
        let event = PointerEvent {
            click_count: 1,
            ..PointerEvent::mouse(kind, button, at)
        };
        self.send(widget, Event::Pointer(event))
    }

    /// A left click: press and release at `at`.
    pub fn click(&mut self, widget: &mut dyn Control, at: Point) -> EventResult {
        let down = self.pointer(widget, PointerEventKind::Down, at);
        let up = self.pointer(widget, PointerEventKind::Up, at);
        down.or(up)
    }

    pub fn paint(&self, widget: &dyn Control) -> RecordingCanvas {
        self.paint_damaged(widget, None)
    }

    /// Paints while only `damage` is being repainted (`None`: the whole window).
    pub fn paint_damaged(&self, widget: &dyn Control, damage: Option<Rect>) -> RecordingCanvas {
        let mut canvas = RecordingCanvas::new();
        let cx = PaintCx::new(
            widget.id(),
            &self.theme,
            1.0,
            &self.engine,
            &self.focus,
            &self.capture,
            damage,
        );
        widget.paint(&cx, &mut canvas);
        assert!(canvas.is_balanced(), "unbalanced canvas stack");
        canvas
    }

    pub fn tree(&self, widget: &dyn Control) -> AccessTree {
        let mut tree = AccessTree::new(widget.id());
        let cx = AccessCx::new(widget.id(), &self.theme, 1.0, &self.engine, &self.focus);
        widget.access(&cx, &mut tree);
        tree
    }
}

/// The node called `name`; panics when there is none.
pub fn node_named<'a>(tree: &'a AccessTree, name: &str) -> &'a AccessNode {
    tree.nodes()
        .iter()
        .find(|node| node.name == name)
        .unwrap_or_else(|| panic!("no node named {name:?}"))
}

/// The names of the children of `id`, in order.
pub fn child_names(tree: &AccessTree, id: WidgetId) -> Vec<String> {
    tree.children(id).map(|node| node.name.clone()).collect()
}

/// The text of the bytes of `text` covered by `ranges`, concatenated.
pub fn covered(text: &str, ranges: &[std::ops::Range<usize>]) -> String {
    ranges.iter().map(|range| &text[range.clone()]).collect()
}
