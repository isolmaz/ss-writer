//! The document of a preview: a read-only [`EditorView`] with its own layout, paint and event
//! passes, independent of any window. The child window drives it with real input and a swap
//! chain; tests and [`PreviewContent::render`] drive it with a WIC bitmap.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use ssw_core::document::Document;
use ssw_render::scene::Scene;
use ssw_ui::canvas::{Canvas, SceneId};
use ssw_ui::editor::object::InlineObjectProvider;
use ssw_ui::editor::{DocumentKind, EditorView, blocks_media, blocks_table, blocks_text};
use ssw_ui::geometry::{Color, Point, Rect, Size};
use ssw_ui::input::{Cursor, Event, FocusEvent, KeyEvent, vk};
use ssw_ui::services::Timers;
use ssw_ui::theme::Theme;
use ssw_ui::widget::{
    EventCx, EventResult, FocusManager, LayoutCx, PaintCx, PointerCapture, Requests, Widget,
};
use ssw_ui::widgets::{Control, SharedClipboard};
use ssw_win::canvas::{CanvasAssets, D2dCanvas, ImageStore, TargetResources};
use ssw_win::com::WinError;
use ssw_win::dpi::{scale_for_dpi, size_px_to_dip};
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::{WicTarget, d2d_factory};
use ssw_win::services::clock::QpcClock;
use ssw_win::theme::SystemTheme;
use ssw_win::wic::{DecodedImage, Wic};

use crate::appearance::Visuals;
use crate::objects::PreviewObjects;

/// `A` and `C`: the only letter keys the preview answers (select all, copy).
const KEY_A: u16 = 0x41;
const KEY_C: u16 = 0x43;

/// The visible Markdown document of one preview.
pub(crate) struct PreviewContent {
    view: EditorView,
    objects: Rc<PreviewObjects>,
    engine: DWriteEngine,
    kind: DocumentKind,
    theme: Theme,
    focus: FocusManager,
    capture: PointerCapture,
    requests: Requests,
    /// Client size in DIPs.
    size: Size,
    /// `dpi / 96`.
    scale: f32,
}

impl std::fmt::Debug for PreviewContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewContent")
            .field("size", &self.size)
            .field("scale", &self.scale)
            .field("view", &self.view)
            .finish_non_exhaustive()
    }
}

/// True when `theme` has a dark surface (diagrams are drawn with the matching palette).
fn is_dark(theme: &Theme) -> bool {
    theme.tokens.surface.relative_luminance() < 0.5
}

/// Keys the read-only view answers: navigation, select all and copy. Everything else (Tab,
/// Enter, Escape, typing, Alt combinations) belongs to the host that embeds the preview.
fn is_view_key(key: &KeyEvent) -> bool {
    if key.mods.alt() {
        return false;
    }
    match key.vk {
        vk::LEFT
        | vk::RIGHT
        | vk::UP
        | vk::DOWN
        | vk::HOME
        | vk::END
        | vk::PAGE_UP
        | vk::PAGE_DOWN => true,
        KEY_A | KEY_C | vk::INSERT => key.mods.ctrl(),
        _ => false,
    }
}

impl PreviewContent {
    /// A preview of the `kind` document `text` (Markdown, or a whole Mermaid diagram for
    /// `.mmd`), dressed with `visuals` over the `system` theme.
    pub(crate) fn new(
        text: &str,
        kind: DocumentKind,
        visuals: &Visuals,
        system: &SystemTheme,
        engine: DWriteEngine,
    ) -> Self {
        let document = Rc::new(RefCell::new(Document::new(text)));
        let mut view = EditorView::new(document, Rc::new(QpcClock::new()));
        view.set_read_only(true);
        view.set_caret_blink(false);
        blocks_text::register(view.registry_mut());
        blocks_media::register(view.registry_mut());
        // The table commands belong to an editing menu the preview does not have.
        let _ = blocks_table::register(view.registry_mut());
        let theme = visuals.theme(system);
        let objects = Rc::new(PreviewObjects::new(is_dark(&theme)));
        let provider: Rc<dyn InlineObjectProvider> = objects.clone();
        view.set_object_provider(provider);
        view.set_config(visuals.editor_config(kind));
        Self {
            view,
            objects,
            engine,
            kind,
            theme,
            focus: FocusManager::new(),
            capture: PointerCapture::new(),
            requests: Requests::new(),
            size: Size::new(1.0, 1.0),
            scale: 1.0,
        }
    }

    /// The text engine, shared with the canvas that draws the layouts it makes.
    pub(crate) fn engine(&self) -> &DWriteEngine {
        &self.engine
    }

    /// The clipboard Ctrl+C copies to.
    pub(crate) fn set_clipboard(&mut self, clipboard: SharedClipboard) {
        self.view.set_clipboard(clipboard);
    }

    /// Dresses the preview with new colors and font (the host called `IPreviewHandlerVisuals`
    /// or the system theme changed).
    pub(crate) fn set_visuals(&mut self, visuals: &Visuals, system: &SystemTheme) {
        self.theme = visuals.theme(system);
        self.view.set_config(visuals.editor_config(self.kind));
        let dark = is_dark(&self.theme);
        if dark != self.objects.is_dark() {
            self.objects = Rc::new(PreviewObjects::new(dark));
            let provider: Rc<dyn InlineObjectProvider> = self.objects.clone();
            self.view.set_object_provider(provider);
        }
        self.view.invalidate_objects();
        self.requests.invalidate_all();
    }

    /// The color behind the document.
    pub(crate) fn background(&self) -> Color {
        self.theme.tokens.surface
    }

    /// Sets the client size in pixels and the DPI, and lays the document out for them.
    pub(crate) fn resize(&mut self, size_px: (i32, i32), dpi: u32) {
        self.scale = scale_for_dpi(dpi);
        let size = size_px_to_dip(size_px.0.max(1), size_px.1.max(1), self.scale);
        self.size = size;
        self.requests.invalidate_all();
        self.arrange();
    }

    fn arrange(&mut self) {
        let id = self.view.id();
        self.focus.begin_tab_order();
        {
            let mut cx = LayoutCx::new(
                id,
                &self.theme,
                self.scale,
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

    /// Lays out what is visible and returns the scenes (formulas, diagrams) typeset since the
    /// last call; the host registers them with its canvas before [`PreviewContent::paint`].
    pub(crate) fn layout(&mut self) -> Vec<(SceneId, Arc<Scene>)> {
        self.arrange();
        self.objects.take_fresh()
    }

    /// Draws the document. Call [`PreviewContent::layout`] first.
    pub(crate) fn paint(&self, canvas: &mut dyn Canvas) {
        let cx = PaintCx::new(
            self.view.id(),
            &self.theme,
            self.scale,
            &self.engine,
            &self.focus,
            &self.capture,
            None,
        );
        self.view.paint(&cx, canvas);
    }

    /// True when the last paint met scenes it could not draw yet (typeset during the paint
    /// pass): the host repaints after registering them.
    pub(crate) fn needs_repaint_for_scenes(&self) -> bool {
        self.objects.has_fresh()
    }

    /// Routes `event` to the view and drops the editor events a preview has no use for (links
    /// are never followed from Explorer).
    pub(crate) fn event(&mut self, event: &Event) -> EventResult {
        if let Event::Key(key) = event
            && !is_view_key(key)
        {
            return EventResult::ignored();
        }
        let id = self.view.id();
        let result = {
            let mut cx = EventCx::new(
                id,
                &self.theme,
                self.scale,
                &self.engine,
                &mut self.focus,
                &mut self.capture,
                &mut self.requests,
            );
            self.view.event(&mut cx, event)
        };
        if result.request_focus {
            self.focus.focus(id);
        }
        let _ = self.view.take_events();
        result
    }

    /// The window gained or lost keyboard focus.
    pub(crate) fn set_focused(&mut self, focused: bool) {
        self.arrange();
        let id = self.view.id();
        if focused {
            self.focus.focus(id);
        } else if self.focus.is_focused(id) {
            self.focus.blur();
        }
        let change = if focused {
            FocusEvent::Gained
        } else {
            FocusEvent::Lost
        };
        let _ = self.event(&Event::Focus(change));
    }

    /// Whether anything asked for a repaint since the last call.
    pub(crate) fn take_repaint(&mut self) -> bool {
        !self.requests.take_invalidation().is_clean()
    }

    /// The cursor the view asked for since the last call.
    pub(crate) fn take_cursor(&mut self) -> Option<Cursor> {
        self.requests.take_cursor()
    }

    /// Hands the timers and animation frames the view asked for to `timers`.
    pub(crate) fn flush_timers(&mut self, timers: &mut dyn Timers) {
        self.requests.flush_timers(timers);
    }

    /// How far the document is scrolled from its top, in DIPs.
    pub(crate) fn scroll_y(&self) -> f32 {
        self.view.scroll_y()
    }

    /// Renders the document into a new `width_px` x `height_px` WIC bitmap at `dpi` with the
    /// software Direct2D target: the same layout, paint and scene passes the window runs.
    ///
    /// # Errors
    /// Fails when WIC, Direct2D or the scene renderer cannot be created or drawing fails.
    pub(crate) fn render(
        &mut self,
        width_px: u32,
        height_px: u32,
        dpi: u32,
    ) -> Result<DecodedImage, WinError> {
        let size_px = (
            i32::try_from(width_px).unwrap_or(i32::MAX),
            i32::try_from(height_px).unwrap_or(i32::MAX),
        );
        self.resize(size_px, dpi);
        let wic = Wic::new()?;
        let factory = d2d_factory(false)?;
        let target = WicTarget::new(&factory, &wic, width_px, height_px, dpi as f32)?;
        let mut assets = CanvasAssets::with_scene_renderer(
            ImageStore::new(wic.clone()),
            self.engine.clone(),
            wic,
        )?;
        // Layout first: it typesets the scenes of everything visible.
        let _ = self.layout();
        for (id, scene) in self.objects.all_scenes() {
            assets.register_scene(id, scene);
        }
        let mut resources = TargetResources::new(factory, 1);
        target.paint(Some(self.background()), |render_target| {
            let mut canvas = D2dCanvas::new(render_target, &assets, &mut resources);
            self.paint(&mut canvas);
            canvas.finish()
        })?;
        target.pixels()
    }
}

#[cfg(test)]
mod tests {
    use ssw_core::edit::Selection;
    use ssw_ui::input::{Modifiers, TextInput, WheelEvent};
    use ssw_ui::theme::DEFAULT_ACCENT;
    use ssw_win::com::ComGuard;

    use super::*;

    const TEXT: &str = "alpha beta\n\ngamma\n";

    fn system() -> SystemTheme {
        SystemTheme {
            theme: Theme::light(DEFAULT_ACCENT),
            dark: false,
            high_contrast: false,
            transparency_effects: true,
            client_animations: true,
            accent: DEFAULT_ACCENT,
        }
    }

    fn com() -> ComGuard {
        ComGuard::apartment_threaded().unwrap()
    }

    fn content(text: &str, kind: DocumentKind) -> PreviewContent {
        let engine = DWriteEngine::new().unwrap();
        let mut content = PreviewContent::new(text, kind, &Visuals::default(), &system(), engine);
        content.resize((400, 200), 96);
        content
    }

    fn key(vk: u16, mods: Modifiers) -> Event {
        Event::Key(KeyEvent::press(vk, mods))
    }

    fn document_text(content: &PreviewContent) -> String {
        content.view.document().borrow().text()
    }

    fn wheel(notches: f32) -> Event {
        Event::Wheel(WheelEvent {
            delta: Point::new(0.0, notches),
            precise: false,
            position: Point::new(50.0, 50.0),
            mods: Modifiers::NONE,
        })
    }

    #[test]
    fn only_navigation_select_all_and_copy_belong_to_the_view() {
        let press = |vk: u16, mods| is_view_key(&KeyEvent::press(vk, mods));
        for vk in [
            vk::LEFT,
            vk::RIGHT,
            vk::UP,
            vk::DOWN,
            vk::HOME,
            vk::END,
            vk::PAGE_UP,
            vk::PAGE_DOWN,
        ] {
            assert!(press(vk, Modifiers::NONE), "{vk:#x}");
            assert!(press(vk, Modifiers::SHIFT), "{vk:#x} extends the selection");
            assert!(!press(vk, Modifiers::ALT), "{vk:#x} with Alt is the host's");
        }
        assert!(press(KEY_A, Modifiers::CTRL) && press(KEY_C, Modifiers::CTRL));
        assert!(press(vk::INSERT, Modifiers::CTRL));
        assert!(!press(KEY_A, Modifiers::NONE) && !press(KEY_C, Modifiers::NONE));
        assert!(
            !press(vk::INSERT, Modifiers::SHIFT),
            "Shift+Insert would paste"
        );
        for vk in [
            vk::TAB,
            vk::RETURN,
            vk::ESCAPE,
            vk::BACK,
            vk::DELETE,
            vk::SPACE,
        ] {
            assert!(!press(vk, Modifiers::NONE), "{vk:#x}");
        }
    }

    #[test]
    fn the_view_is_read_only_but_selects_and_navigates_when_focused() {
        let _com = com();
        let mut c = content(TEXT, DocumentKind::Markdown);
        c.set_focused(true);
        assert!(c.event(&key(KEY_A, Modifiers::CTRL)).handled);
        let selection: Selection = c.view.selection();
        assert_eq!(selection.end() - selection.start(), TEXT.len());
        assert!(c.event(&key(vk::DOWN, Modifiers::NONE)).handled);
        let typed = c.event(&Event::Text(TextInput("x".to_owned())));
        assert!(!typed.handled || document_text(&c) == TEXT);
        assert_eq!(document_text(&c), TEXT, "typing changes nothing");
        assert!(
            !c.event(&key(vk::TAB, Modifiers::NONE)).handled,
            "Tab goes to the host"
        );
        assert!(!c.event(&key(vk::DELETE, Modifiers::NONE)).handled);
        assert_eq!(document_text(&c), TEXT);
    }

    #[test]
    fn keys_wait_for_the_focus() {
        let _com = com();
        let mut c = content(TEXT, DocumentKind::Markdown);
        assert!(
            !c.event(&key(vk::DOWN, Modifiers::NONE)).handled,
            "not focused yet"
        );
        c.set_focused(true);
        assert!(c.event(&key(vk::DOWN, Modifiers::NONE)).handled);
        c.set_focused(false);
        assert!(
            !c.event(&key(vk::DOWN, Modifiers::NONE)).handled,
            "focus lost"
        );
    }

    #[test]
    fn the_wheel_scrolls_and_asks_for_one_repaint() {
        let _com = com();
        let text: String = (0..60).map(|i| format!("Paragraph {i}\n\n")).collect();
        let mut c = content(&text, DocumentKind::Markdown);
        let _ = c.take_repaint();
        assert_eq!(c.scroll_y(), 0.0);
        assert!(c.event(&wheel(-1.0)).handled);
        assert!(c.scroll_y() > 0.0);
        assert!(c.take_repaint(), "a scrolled view repaints");
        assert!(!c.take_repaint(), "once");
        assert!(c.event(&wheel(5.0)).handled);
        assert_eq!(c.scroll_y(), 0.0, "never above the top");
    }

    #[test]
    fn formulas_are_typeset_during_layout_and_handed_out_once() {
        let _com = com();
        let mut c = content("Inline $x^2 + y^2$ math\n", DocumentKind::Markdown);
        let scenes = c.layout();
        assert!(!scenes.is_empty(), "the formula became a scene");
        assert!(c.layout().is_empty(), "scenes are handed out once");
        assert!(!c.needs_repaint_for_scenes());
    }

    #[test]
    fn a_mermaid_document_becomes_a_diagram_scene() {
        let _com = com();
        let mut c = content("graph TD\n  A[Start] --> B[End]\n", DocumentKind::Mermaid);
        assert!(!c.layout().is_empty(), "the diagram was rendered");
    }

    #[test]
    fn rendering_draws_text_on_the_themes_surface_at_any_dpi() {
        let _com = com();
        let mut c = content("# Title\n\nSome text $x^2$ here.\n", DocumentKind::Markdown);
        let image = c.render(300, 200, 96).unwrap();
        assert_eq!((image.width, image.height), (300, 200));
        assert_eq!(
            image.pixel(2, 2),
            Some([255, 255, 255, 255]),
            "the light surface"
        );
        let dark = |x: u32, y: u32| image.pixel(x, y).is_some_and(|p| p[0] < 100 && p[1] < 100);
        assert!((0..200).any(|y| (0..300).any(|x| dark(x, y))), "text ink");
        let sharp = c.render(600, 400, 192).unwrap();
        assert_eq!((sharp.width, sharp.height), (600, 400));
        assert_ne!(sharp.bgra.len(), image.bgra.len());
        // Rendering did not disturb the layout of the window: it is back at 400 x 200 DIPs.
        assert_eq!(c.size, Size::new(600.0 / 2.0, 400.0 / 2.0));
    }

    #[test]
    fn new_visuals_recolor_the_surface_and_switch_the_diagram_palette() {
        let _com = com();
        let mut c = content("text\n", DocumentKind::Markdown);
        let _ = c.take_repaint();
        assert!(!c.objects.is_dark());
        let mut visuals = Visuals::default();
        visuals.set_background(0x0010_1010);
        visuals.set_text(0x00F0_F0F0);
        c.set_visuals(&visuals, &system());
        assert_eq!(c.background(), visuals.background.unwrap());
        assert!(c.objects.is_dark());
        assert!(c.take_repaint());
        let image = c.render(100, 60, 96).unwrap();
        let corner = image.pixel(2, 2).unwrap();
        assert!(
            corner[..3].iter().all(|&channel| channel.abs_diff(16) <= 3),
            "{corner:?}"
        );
    }

    #[test]
    fn the_engine_is_shared_with_the_canvas() {
        let _com = com();
        let c = content("x\n", DocumentKind::Markdown);
        assert!(c.engine().bundled_font_count() >= 1);
    }
}
