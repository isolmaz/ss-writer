//! Drives an [`EditorView`] the way the window host does: layout pass, paint pass on a
//! [`RecordingCanvas`], events routed with focus and pointer capture.

// Every test crate uses a different subset of the helpers.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use ssw_core::document::Document;
use ssw_core::edit::Selection;
use ssw_ui::access::{AccessAction, AccessNode, AccessTree};
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::editor::{EditorConfig, EditorEvent, EditorView};
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{
    Event, FocusEvent, ImeEvent, KeyEvent, Modifiers, PointerButton, PointerEvent,
    PointerEventKind, PointerType, TextInput, WheelEvent,
};
use ssw_ui::services::FakeClock;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widget::{
    AccessCx, EventCx, EventResult, FocusManager, LayoutCx, PaintCx, PointerCapture, Requests,
    Widget,
};
use ssw_ui::widgets::Control;

pub const KEY_A: u16 = 0x41;
pub const KEY_C: u16 = 0x43;
pub const KEY_V: u16 = 0x56;
pub const KEY_X: u16 = 0x58;
pub const KEY_Z: u16 = 0x5A;

/// The viewport of every test: 400 × 200 DIPs at the origin.
pub const VIEWPORT: Size = Size::new(400.0, 200.0);
/// Left edge of the 352 DIP text column inside the viewport.
pub const COLUMN_LEFT: f32 = 24.0;
/// Space above the first block.
pub const TOP: f32 = 24.0;
/// Advance of one grapheme of the test engine.
pub const ADVANCE: f32 = 8.0;
/// Line height of the test engine at the 16 DIP body size: the default 1.75 line height setting
/// makes every line exactly 28 DIPs.
pub const LINE: f32 = 28.0;

pub struct Harness {
    pub theme: Theme,
    pub engine: MonoTextEngine,
    pub focus: FocusManager,
    pub capture: PointerCapture,
    pub requests: Requests,
    pub doc: Rc<RefCell<Document>>,
    pub clock: Rc<FakeClock>,
    pub view: EditorView,
    pub size: Size,
    /// Left edge of the text column.
    pub left: f32,
}

impl Harness {
    /// An arranged, focused editor over `text` in the default viewport.
    pub fn new(text: &str) -> Self {
        Self::sized(text, VIEWPORT)
    }

    /// An arranged, focused editor over `text` in a viewport of `size`.
    pub fn sized(text: &str, size: Size) -> Self {
        let column = (size.width - 48.0).clamp(80.0, 720.0);
        let doc = Rc::new(RefCell::new(Document::new(text)));
        let clock = Rc::new(FakeClock::default());
        let view = EditorView::new(Rc::clone(&doc), clock.clone());
        let mut harness = Self {
            theme: Theme::light(DEFAULT_ACCENT),
            engine: MonoTextEngine::default(),
            focus: FocusManager::new(),
            capture: PointerCapture::new(),
            requests: Requests::new(),
            doc,
            clock,
            view,
            size,
            left: (size.width - column) / 2.0,
        };
        harness.arrange();
        let id = harness.view.id();
        harness.focus.focus(id);
        harness.event(Event::Focus(FocusEvent::Gained));
        harness
    }

    pub fn config(&mut self, change: impl FnOnce(&mut EditorConfig)) {
        let mut config = self.view.config().clone();
        change(&mut config);
        self.view.set_config(config);
    }

    pub fn arrange(&mut self) {
        let id = self.view.id();
        self.focus.begin_tab_order();
        {
            let mut cx = LayoutCx::new(
                id,
                &self.theme,
                1.0,
                &self.engine,
                &mut self.focus,
                &mut self.requests,
            );
            let size = self.view.measure(&mut cx, self.size);
            self.view
                .arrange(&mut cx, Rect::from_origin_size(Point::ZERO, size));
        }
        self.focus.commit_tab_order();
    }

    pub fn paint(&self) -> RecordingCanvas {
        let mut canvas = RecordingCanvas::new();
        let cx = PaintCx::new(
            self.view.id(),
            &self.theme,
            1.0,
            &self.engine,
            &self.focus,
            &self.capture,
            None,
        );
        self.view.paint(&cx, &mut canvas);
        assert!(canvas.is_balanced(), "paint leaves the canvas balanced");
        canvas
    }

    /// The text of every layout drawn.
    pub fn painted_texts(&self) -> Vec<String> {
        self.paint()
            .texts()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    pub fn event(&mut self, event: Event) -> EventResult {
        let id = self.view.id();
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
            self.view.event(&mut cx, &event)
        };
        if result.request_focus {
            self.focus.focus(id);
        }
        result
    }

    pub fn key(&mut self, vk: u16, mods: Modifiers) -> EventResult {
        self.event(Event::Key(KeyEvent::press(vk, mods)))
    }

    pub fn press(&mut self, vk: u16) -> EventResult {
        self.key(vk, Modifiers::NONE)
    }

    pub fn type_text(&mut self, text: &str) -> EventResult {
        self.event(Event::Text(TextInput(text.to_owned())))
    }

    pub fn ime(&mut self, event: ImeEvent) -> EventResult {
        self.event(Event::Ime(event))
    }

    pub fn pointer(
        &mut self,
        kind: PointerEventKind,
        position: Point,
        clicks: u8,
        mods: Modifiers,
    ) -> EventResult {
        let button = match kind {
            PointerEventKind::Down | PointerEventKind::Up => Some(PointerButton::Left),
            _ => None,
        };
        self.event(Event::Pointer(PointerEvent {
            kind,
            button,
            position,
            pointer_type: PointerType::Mouse,
            click_count: clicks,
            mods,
        }))
    }

    /// A press and release at `position`.
    pub fn click(&mut self, position: Point, clicks: u8, mods: Modifiers) {
        self.pointer(PointerEventKind::Down, position, clicks, mods);
        self.pointer(PointerEventKind::Up, position, 0, mods);
    }

    pub fn wheel(&mut self, delta_y: f32, precise: bool, mods: Modifiers) -> EventResult {
        self.event(Event::Wheel(WheelEvent {
            delta: Point::new(0.0, delta_y),
            precise,
            position: Point::new(100.0, 100.0),
            mods,
        }))
    }

    pub fn text(&self) -> String {
        self.doc.borrow().text()
    }

    pub fn selection(&self) -> Selection {
        self.view.selection()
    }

    pub fn set_caret(&mut self, offset: usize) {
        self.view.set_selection(Selection::caret(offset));
    }

    pub fn events(&mut self) -> Vec<EditorEvent> {
        self.view.take_events()
    }

    /// The window position of a character cell of the first line of the first block.
    pub fn first_line_point(&self, column: usize) -> Point {
        Point::new(self.left + column as f32 * ADVANCE + 1.0, TOP + LINE / 2.0)
    }

    pub fn access(&self) -> AccessNode {
        let cx = AccessCx::new(self.view.id(), &self.theme, 1.0, &self.engine, &self.focus);
        let mut tree = AccessTree::new(self.view.id());
        self.view.access(&cx, &mut tree);
        tree.root().expect("the editor inserts its node").clone()
    }

    pub fn perform(&mut self, action: &AccessAction) -> bool {
        let id = self.view.id();
        let mut cx = EventCx::new(
            id,
            &self.theme,
            1.0,
            &self.engine,
            &mut self.focus,
            &mut self.capture,
            &mut self.requests,
        );
        self.view.perform(&mut cx, action)
    }
}

pub fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}
