//! Shared harness of the settings, history and export tests: a minimal window host that lays a
//! root control out, routes events, performs accessibility actions and records paint.

// Every test crate uses a different subset of the helpers.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use ssw_core::i18n::Localizer;
use ssw_core::settings::Settings;
use ssw_ui::access::{AccessAction, AccessNode, AccessTree, Role};
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::input::{
    Event, KeyEvent, Modifiers, PointerButton, PointerEvent, PointerEventKind, WheelEvent,
};
use ssw_ui::services::MemoryClipboard;
use ssw_ui::shell::settings::{DisplayKeyNames, LanguageChoice, SettingsEnv, SettingsWindow};
use ssw_ui::shortcuts::CharProbe;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widget::{
    AccessCx, EventCx, EventResult, FocusManager, LayoutCx, PaintCx, PointerCapture, Requests,
    WidgetId,
};
use ssw_ui::widgets::Control;
use ssw_ui::widgets::anim::Motion;

pub const MAIN_FTL: &str = include_str!("../../../../assets/i18n/en/main.ftl");
pub const SETTINGS_FTL: &str = include_str!("../../../../assets/i18n/en/settings.ftl");

/// The English messages of the shell.
pub fn localizer() -> Rc<Localizer> {
    localizer_with(SETTINGS_FTL)
}

pub fn localizer_with(settings_ftl: &str) -> Rc<Localizer> {
    Rc::new(Localizer::new(
        "en",
        &[("en", MAIN_FTL), ("en", settings_ftl)],
    ))
}

/// The AltGr layer of the Turkish Q layout: Ctrl+Alt+Q and Ctrl+Alt+E type a character.
pub struct TurkishQ;

impl CharProbe for TurkishQ {
    fn produces_char(&self, vk: u16, _scan: u16, shift: bool, caps: bool) -> bool {
        match vk {
            0x51 | 0x45 => true,
            // AltGr+A types a character only with Shift or Caps Lock.
            0x41 => shift || caps,
            _ => false,
        }
    }
}

pub fn settings_env() -> SettingsEnv {
    SettingsEnv {
        clipboard: Rc::new(RefCell::new(MemoryClipboard::default())),
        char_probe: Box::new(TurkishQ),
        key_names: Box::new(DisplayKeyNames),
        languages: vec![
            LanguageChoice {
                tag: "en".to_owned(),
                name: "English".to_owned(),
            },
            LanguageChoice {
                tag: "tr".to_owned(),
                name: "Türkçe".to_owned(),
            },
        ],
        motion: Motion::reduced(),
    }
}

pub fn settings_window(settings: Settings) -> SettingsWindow {
    SettingsWindow::new(settings, localizer(), settings_env())
}

/// Lays a root control out in a fixed window and routes events like the real host: the layout is
/// redone after every event so enabled states, tab stops and scrolling are always current.
pub struct Host {
    pub theme: Theme,
    pub engine: MonoTextEngine,
    pub focus: FocusManager,
    pub capture: PointerCapture,
    pub requests: Requests,
    pub bounds: Rect,
}

impl Host {
    pub fn new(bounds: Rect) -> Self {
        Self {
            theme: Theme::light(DEFAULT_ACCENT),
            engine: MonoTextEngine::default(),
            focus: FocusManager::new(),
            capture: PointerCapture::new(),
            requests: Requests::new(),
            bounds,
        }
    }

    pub fn layout(&mut self, root: &mut dyn Control) {
        let id = root.id();
        self.focus.begin_tab_order();
        let mut cx = LayoutCx::new(
            id,
            &self.theme,
            1.0,
            &self.engine,
            &mut self.focus,
            &mut self.requests,
        );
        root.arrange(&mut cx, self.bounds);
        self.focus.commit_tab_order();
        self.focus.take_changes();
    }

    pub fn event(&mut self, root: &mut dyn Control, event: Event) -> EventResult {
        let id = root.id();
        let result = {
            let mut cx = EventCx::new(
                id,
                &self.theme,
                1.0,
                &self.engine,
                &mut self.focus,
                &mut self.capture,
                &mut self.requests,
            );
            root.event(&mut cx, &event)
        };
        self.layout(root);
        result
    }

    pub fn perform(
        &mut self,
        root: &mut dyn Control,
        target: WidgetId,
        action: AccessAction,
    ) -> bool {
        let handled = {
            let mut cx = EventCx::new(
                target,
                &self.theme,
                1.0,
                &self.engine,
                &mut self.focus,
                &mut self.capture,
                &mut self.requests,
            );
            root.perform(&mut cx, &action)
        };
        self.layout(root);
        handled
    }

    pub fn tree(&self, root: &dyn Control) -> AccessTree {
        let mut tree = AccessTree::new(root.id());
        let cx = AccessCx::new(root.id(), &self.theme, 1.0, &self.engine, &self.focus);
        root.access(&cx, &mut tree);
        tree
    }

    pub fn paint(&self, root: &dyn Control) -> RecordingCanvas {
        let mut canvas = RecordingCanvas::new();
        let cx = PaintCx::new(
            root.id(),
            &self.theme,
            1.0,
            &self.engine,
            &self.focus,
            &self.capture,
            None,
        );
        root.paint(&cx, &mut canvas);
        assert!(canvas.is_balanced(), "unbalanced canvas stack");
        canvas
    }

    pub fn key(&mut self, root: &mut dyn Control, vk: u16, mods: Modifiers) -> EventResult {
        self.event(root, Event::Key(KeyEvent::press(vk, mods)))
    }

    pub fn click(&mut self, root: &mut dyn Control, at: Point) {
        for kind in [PointerEventKind::Down, PointerEventKind::Up] {
            let event = PointerEvent {
                click_count: 1,
                ..PointerEvent::mouse(kind, Some(PointerButton::Left), at)
            };
            self.event(root, Event::Pointer(event));
        }
    }

    /// Scrolls with a precise wheel: negative `dy` scrolls down.
    pub fn wheel(&mut self, root: &mut dyn Control, at: Point, dy: f32) -> EventResult {
        self.event(
            root,
            Event::Wheel(WheelEvent {
                delta: Point::new(0.0, dy),
                precise: true,
                position: at,
                mods: Modifiers::NONE,
            }),
        )
    }

    /// The first node named `name` that is not a container or row.
    pub fn control(&self, root: &dyn Control, name: &str) -> AccessNode {
        let tree = self.tree(root);
        tree.nodes()
            .iter()
            .find(|node| {
                node.name == name
                    && !matches!(
                        node.role,
                        Role::Group
                            | Role::Window
                            | Role::Pane
                            | Role::List
                            | Role::ListItem
                            | Role::Text
                    )
            })
            .cloned()
            .unwrap_or_else(|| panic!("no control named {name:?}"))
    }

    /// The node with `role` named `name`.
    pub fn node(&self, root: &dyn Control, role: Role, name: &str) -> AccessNode {
        let tree = self.tree(root);
        tree.nodes()
            .iter()
            .find(|node| node.role == role && node.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {role:?} named {name:?}"))
    }

    pub fn has_node(&self, root: &dyn Control, role: Role, name: &str) -> bool {
        self.tree(root)
            .nodes()
            .iter()
            .any(|node| node.role == role && node.name == name)
    }

    /// Moves keyboard focus to the control named `name` through its accessibility action.
    pub fn focus_control(&mut self, root: &mut dyn Control, name: &str) -> AccessNode {
        let node = self.control(root, name);
        assert!(
            self.perform(root, node.id, AccessAction::Focus),
            "{name:?} refused focus"
        );
        self.control(root, name)
    }
}
