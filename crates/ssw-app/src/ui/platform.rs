//! The platform wiring of one window: text services (TSF) over the active editor, the system
//! caret, the UI Automation bridge, the OLE drop target, the spell checker, the editor block
//! renderers and the provider of math, Mermaid and image objects.
//!
//! [`Platform`] is created before the native window exists ([`Platform::new`]) so the window
//! handler can hold it, and connected to the window by [`Platform::attach`]. A piece that
//! cannot be created (no TSF, no spell checker) is logged and left out; typing then still works
//! through `WM_CHAR`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ssw_core::edit::{Edit, Selection, Transaction};
use ssw_core::text::TextRange;
use ssw_io::logging::{Level, Logger};
use ssw_io::net::Http;
use ssw_render::mermaid::{MermaidOutcome, MermaidTheme, render_mermaid};
use ssw_render::scene::{Color as SceneColor, Scene};
use ssw_ui::access::{AccessAction, AccessTree};
use ssw_ui::canvas::{ImageId, SceneId};
use ssw_ui::editor::object::{
    InlineObjectProvider, ObjectInfo, ObjectKind, ObjectRequest, ObjectState, ObjectVisual,
};
use ssw_ui::editor::spell::{SpellCheck, SpellProvider};
use ssw_ui::editor::{EditorView, blocks_media, blocks_table, blocks_text, spell};
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{Event, ImeEvent, PointerEvent, PointerEventKind, PointerType, WheelEvent};
use ssw_ui::scroll_physics::Extent;
use ssw_ui::widget::WidgetId;
use ssw_win::caret::SystemCaret;
use ssw_win::com::WinError;
use ssw_win::dmanip::{
    DmConfig, ScrollDriver, ScrollLayout, pointer_client_position, pointer_is_pen,
};
use ssw_win::dragdrop::{DragContent, DragEvent, DropHandler, Effect, WindowDropTarget};
use ssw_win::spell::{SpellChecker, SpellFactory};
use ssw_win::taskbar::{Progress, Taskbar, TaskbarApi, taskbar_button_created_message};
use ssw_win::tsf::{TextServices, TextStoreHost};
use ssw_win::uia::{ActionSink, UiaBridge};
use ssw_win::window::{WakeHandle, WindowHost};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    DM_POINTERHITTEST, WM_GETOBJECT, WM_POINTERDOWN, WM_SETTINGCHANGE,
};

use super::controller::{Request, WindowCell};
use super::remote_images::{RemoteImagePolicy, fetch_remote_image, is_https, is_remote};
use super::shell_view::ShellView;
use super::window::WindowEnv;
use crate::documents::DocId;
use crate::workers::{JobKey, JobKind, JobSpec, Workers};

// ----- text services ----------------------------------------------------------------------

/// The active editor of a window as the TSF text store sees it.
struct EditorTextHost {
    cell: Rc<WindowCell>,
    env: Rc<WindowEnv>,
}

impl EditorTextHost {
    fn document(&self) -> Option<Rc<std::cell::RefCell<ssw_core::document::Document>>> {
        let view = self.cell.view.try_borrow().ok()?;
        view.active_tab().map(|tab| Rc::clone(tab.entry.document()))
    }
}

impl TextStoreHost for EditorTextHost {
    fn text(&self) -> String {
        self.document()
            .map(|doc| doc.borrow().text())
            .unwrap_or_default()
    }

    fn selection(&self) -> Selection {
        self.document()
            .map(|doc| doc.borrow().primary_selection())
            .unwrap_or_else(|| Selection { anchor: 0, head: 0 })
    }

    fn set_selection(&self, selection: Selection) {
        if let Some(doc) = self.document() {
            doc.borrow_mut().set_selections(selection, &[]);
        }
    }

    fn replace(&self, range: Range<usize>, text: &str) -> bool {
        let Some(doc) = self.document() else {
            return false;
        };
        let mut doc = doc.borrow_mut();
        let before = doc.primary_selection();
        let edit = Edit::replace(TextRange::new(range.start, range.end), text);
        let provisional = Transaction::single(edit.clone(), before, before);
        let after = before.map_through(&provisional);
        doc.apply(Transaction::single(edit, before, after)).is_ok()
    }

    fn is_read_only(&self) -> bool {
        self.cell
            .view
            .try_borrow()
            .ok()
            .and_then(|view| view.active_tab().map(|tab| tab.entry.is_read_only()))
            .unwrap_or(true)
    }

    fn view_rect(&self) -> Option<Rect> {
        let view = self.cell.view.try_borrow().ok()?;
        view.active_tab()?;
        Some(view.layout().rects().editor)
    }

    fn range_rect(&self, range: Range<usize>) -> Option<Rect> {
        let view = self.cell.view.try_borrow().ok()?;
        let editor = &view.active_tab()?.editor;
        let start = editor.rect_at(range.start)?;
        let end = editor.rect_at(range.end).unwrap_or(start);
        Some(start.union(end))
    }

    fn composition_rect(&self, _range: Range<usize>) -> Option<Rect> {
        let view = self.cell.view.try_borrow().ok()?;
        view.active_tab()?.editor.caret_rect()
    }

    fn offset_at_point(&self, point: Point) -> Option<usize> {
        let view = self.cell.view.try_borrow().ok()?;
        view.active_tab()?.editor.offset_at(point)
    }

    fn ime(&self, event: ImeEvent) {
        let Ok(mut view) = self.cell.view.try_borrow_mut() else {
            return;
        };
        let actions = view.event(&Event::Ime(event), &self.env.text);
        drop(view);
        for action in actions {
            self.env
                .controller
                .push(Request::Action(self.cell.id, action));
        }
    }
}

/// What the text store was last told about the document.
struct TsfShadow {
    tab: u64,
    version: u64,
    text: String,
    selection: Selection,
}

// ----- accessibility ------------------------------------------------------------------------

/// Actions assistive technology asked for, handed from UIA threads to the UI thread.
struct AccessQueue {
    items: Mutex<Vec<(WidgetId, AccessAction)>>,
    wake: Mutex<Option<WakeHandle>>,
}

impl ActionSink for AccessQueue {
    fn perform(&self, id: WidgetId, action: AccessAction) -> bool {
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, action));
        if let Some(wake) = *self.wake.lock().unwrap_or_else(PoisonError::into_inner) {
            wake.wake();
        }
        true
    }
}

// ----- spell checking -----------------------------------------------------------------------

/// [`SpellProvider`] over the Windows spell checker.
struct WindowsSpell(SpellChecker);

impl SpellProvider for WindowsSpell {
    fn check(&self, text: &str) -> Vec<Range<usize>> {
        self.0
            .check(text)
            .map(|errors| {
                errors
                    .into_iter()
                    .map(|error| error.range.start..error.range.end)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn suggestions(&self, word: &str) -> Vec<String> {
        self.0.suggestions(word).unwrap_or_default()
    }

    fn add_to_dictionary(&self, word: &str) {
        let _ = self.0.add(word);
    }

    fn ignore(&self, word: &str) {
        let _ = self.0.ignore(word);
    }
}

// ----- objects ------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ObjectKey {
    kind: u8,
    source: String,
    font_bits: u32,
    color: [u8; 4],
    max_width_bits: u32,
    width: Option<u32>,
    height: Option<u32>,
    base: Option<PathBuf>,
}

impl ObjectKey {
    fn job_key(&self) -> JobKey {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        JobKey(hasher.finish())
    }
}

#[derive(Debug, Clone, Copy)]
enum Entry {
    Pending,
    Failed,
    Ready(ObjectInfo),
}

/// A finished background job, applied on the UI thread.
enum Done {
    Scene(ObjectKey, Option<Scene>),
    Image(ObjectKey, Option<Vec<u8>>),
}

/// Math, Mermaid and image objects of the editors of one window.
struct ObjectProvider {
    window: crate::workers::WindowId,
    workers: Rc<Workers>,
    entries: RefCell<HashMap<ObjectKey, Entry>>,
    done: Rc<RefCell<Vec<Done>>>,
    next_id: Cell<u64>,
    base: RefCell<Option<PathBuf>>,
    remote: RemoteSource,
}

/// What the provider needs to load images from the network.
struct RemoteSource {
    http: Arc<dyn Http>,
    /// The state database and the image cache folder, for the worker's own connection.
    database: PathBuf,
    cache_dir: PathBuf,
    /// What the user allowed and opted out of, per document.
    policy: Rc<RemoteImagePolicy>,
    logger: Arc<Logger>,
    /// The document whose objects are being laid out (set by [`Platform::sync`]: the view is
    /// borrowed while it lays out, so the provider cannot ask for it).
    doc: Cell<Option<DocId>>,
    /// Settings → Attachments → "Load remote images" as of the last sync.
    auto_load: Cell<bool>,
    /// What the layout last depended on: when it changes the objects are laid out again.
    stamp: Cell<RemoteStamp>,
}

/// The facts the remote images of a layout depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemoteStamp {
    doc: Option<DocId>,
    epoch: u64,
    auto_load: bool,
}

impl RemoteSource {
    /// Whether the image at `url` may be fetched now: `https` by itself unless the setting is
    /// off or the document opted out; anything the user asked for with "Click to load".
    fn allowed(&self, url: &str) -> bool {
        let doc = self.doc.get();
        let clicked = doc.is_some_and(|doc| self.policy.url_allowed(doc, url));
        let by_default = is_https(url)
            && self.auto_load.get()
            && !doc.is_some_and(|doc| self.policy.is_disabled(doc));
        clicked || by_default
    }
}

impl ObjectProvider {
    fn start(&self, key: &ObjectKey, request: &ObjectRequest<'_>) -> Entry {
        let remote = request.kind == ObjectKind::Image && is_remote(request.source);
        let spec = JobSpec {
            kind: match request.kind {
                ObjectKind::Image if remote => JobKind::Io,
                ObjectKind::Image => JobKind::Decode,
                _ => JobKind::Render,
            },
            window: Some(self.window),
            key: key.job_key(),
            generation: 1,
        };
        let done = Rc::clone(&self.done);
        let owned = key.clone();
        let source = request.source.to_owned();
        let submitted = match request.kind {
            ObjectKind::Math | ObjectKind::DisplayMath => {
                let display = request.kind == ObjectKind::DisplayMath;
                let font = request.font_size;
                let [r, g, b, a] = key.color;
                let color = SceneColor { r, g, b, a };
                self.workers.submit(
                    spec,
                    move |_| ssw_render::math::render_math(&source, display, font, color).ok(),
                    move |result| {
                        done.borrow_mut()
                            .push(Done::Scene(owned, result.ok().flatten()))
                    },
                )
            }
            ObjectKind::Mermaid => {
                let dark = {
                    let c = request.text_color;
                    (c.r + c.g + c.b) / 3.0 > 0.5
                };
                let theme = if dark {
                    MermaidTheme::Dark
                } else {
                    MermaidTheme::Light
                };
                self.workers.submit(
                    spec,
                    move |_| match render_mermaid(&source, theme) {
                        MermaidOutcome::Diagram(scene, _) => Some(scene),
                        MermaidOutcome::Unsupported { .. } => None,
                    },
                    move |result| {
                        done.borrow_mut()
                            .push(Done::Scene(owned, result.ok().flatten()))
                    },
                )
            }
            ObjectKind::Image if remote => {
                let http = Arc::clone(&self.remote.http);
                let database = self.remote.database.clone();
                let cache_dir = self.remote.cache_dir.clone();
                let logger = Arc::clone(&self.remote.logger);
                // An `http` image only gets here after the user asked for it.
                let allow_http = !is_https(&source);
                self.workers.submit(
                    spec,
                    {
                        let source = source.clone();
                        move |_| {
                            fetch_remote_image(
                                http.as_ref(),
                                &database,
                                &cache_dir,
                                &source,
                                allow_http,
                            )
                        }
                    },
                    move |result| {
                        let bytes = match result
                            .map_err(|error| error.to_string())
                            .and_then(|fetched| fetched)
                        {
                            Ok(bytes) => Some(bytes),
                            Err(error) => {
                                logger.log(Level::Warn, "images", &format!("{source}: {error}"));
                                None
                            }
                        };
                        done.borrow_mut().push(Done::Image(owned, bytes));
                    },
                )
            }
            ObjectKind::Image => {
                let base = key.base.clone();
                self.workers.submit(
                    spec,
                    move |_| {
                        let path = std::path::Path::new(&source);
                        let full = if path.is_absolute() {
                            path.to_path_buf()
                        } else {
                            base?.join(path)
                        };
                        std::fs::read(full).ok()
                    },
                    move |result| {
                        done.borrow_mut()
                            .push(Done::Image(owned, result.ok().flatten()))
                    },
                )
            }
        };
        if submitted.is_ok() {
            Entry::Pending
        } else {
            Entry::Failed
        }
    }
}

impl InlineObjectProvider for ObjectProvider {
    fn object(&self, request: &ObjectRequest<'_>) -> ObjectState {
        let remote = request.kind == ObjectKind::Image && is_remote(request.source);
        if remote && !self.remote.allowed(request.source) {
            // `http` images and the images of an opted-out document show "Click to load"
            // (spec §3).
            return ObjectState::Blocked;
        }
        let c = request.text_color.to_rgba8();
        let key = ObjectKey {
            kind: request.kind as u8,
            source: request.source.to_owned(),
            font_bits: request.font_size.to_bits(),
            color: c,
            max_width_bits: request.max_width.to_bits(),
            width: request.width,
            height: request.height,
            // A relative path depends on the document's folder, a URL does not.
            base: (request.kind == ObjectKind::Image && !remote)
                .then(|| self.base.borrow().clone())
                .flatten(),
        };
        let existing = self.entries.borrow().get(&key).copied();
        let entry = match existing {
            Some(entry) => entry,
            None => {
                let entry = self.start(&key, request);
                self.entries.borrow_mut().insert(key, entry);
                entry
            }
        };
        match entry {
            Entry::Pending => ObjectState::Pending,
            Entry::Failed => ObjectState::Failed,
            Entry::Ready(info) => ObjectState::Ready(info),
        }
    }
}

/// `size` scaled down to `max_width` when wider.
pub(super) fn fit(width: f32, height: f32, max_width: f32) -> (f32, f32) {
    if width > max_width && max_width > 0.0 {
        let scale = max_width / width;
        (width * scale, height * scale)
    } else {
        (width, height)
    }
}

// ----- drop target --------------------------------------------------------------------------

/// Files and text dropped on the window.
struct WindowDrops {
    cell: Rc<WindowCell>,
    env: Rc<WindowEnv>,
}

impl WindowDrops {
    fn point(&self, event: &DragEvent<'_>) -> Point {
        let scale = self
            .cell
            .view
            .try_borrow()
            .map_or(1.0, |view| view.layout().scale());
        Point::new(event.x as f32 / scale, event.y as f32 / scale)
    }
}

impl DropHandler for WindowDrops {
    fn drag_enter(&self, event: &DragEvent<'_>) -> Effect {
        self.drag_over(event)
    }

    fn drag_over(&self, event: &DragEvent<'_>) -> Effect {
        if event.kinds.text
            && let Ok(mut view) = self.cell.view.try_borrow_mut()
            && let Some(editor) = view.active_editor_mut()
        {
            editor.set_drop_caret(Some(self.point(event)));
        }
        event.effect_into_document()
    }

    fn drag_leave(&self) {
        if let Ok(mut view) = self.cell.view.try_borrow_mut()
            && let Some(editor) = view.active_editor_mut()
        {
            editor.set_drop_caret(None);
        }
    }

    fn dropped(&self, event: &DragEvent<'_>, content: &DragContent) -> Effect {
        self.drag_leave();
        let point = self.point(event);
        for path in &content.files {
            self.env.controller.open_document(&self.cell, path);
        }
        if !content.files.is_empty() {
            self.env.controller.refresh(&self.cell);
            return Effect::Copy;
        }
        if let Some(text) = &content.text
            && let Ok(mut view) = self.cell.view.try_borrow_mut()
            && let Some(editor) = view.active_editor_mut()
            && editor.drop_text(point, text)
        {
            return Effect::Copy;
        }
        Effect::None
    }
}

// ----- scrolling ----------------------------------------------------------------------------

/// Scroll offsets (DIPs) closer than this are the same offset.
const SCROLL_EPSILON: f32 = 0.01;

/// What the scroll driver scrolls: the editor's area and range, the DPI and the line height
/// (the wheel's notch). Without a document there is nothing to scroll.
fn scroll_layout(view: &ShellView) -> ScrollLayout {
    let layout = view.layout();
    let viewport = layout.rects().editor.size();
    let (extent, line_height) = view.active_tab().map_or_else(
        || {
            let height = f64::from(viewport.height);
            (Extent::new(height, height), 1.0)
        },
        |tab| {
            (
                tab.editor.scroll_extent(),
                f64::from(tab.editor.config().line_px()),
            )
        },
    );
    ScrollLayout {
        viewport,
        extent,
        scale: layout.scale(),
        line_height,
    }
}

/// Scrolling of the active editor through DirectManipulation (touch and precision touchpad
/// with inertia) and the animated wheel, over the editor's own scroll offset.
///
/// The driver owns the motion; the editor owns the position it draws. Every pump the driver's
/// offset is written to the editor, and an editor that moved by itself (caret, search, a new
/// tab, typing at the bottom) is followed by the driver.
struct EditorScroll {
    driver: ScrollDriver,
    /// The offset last written to the editor: a different `scroll_y` means the editor moved by
    /// itself.
    written: f32,
    /// The layout the driver was last given.
    layout: ScrollLayout,
    /// The last frame asked for another one (a spring or inertia runs).
    more: bool,
}

impl EditorScroll {
    fn new(hwnd: HWND, view: &ShellView) -> Result<Self, WinError> {
        let layout = scroll_layout(view);
        Ok(Self {
            driver: ScrollDriver::new(hwnd, DmConfig::default(), &layout)?,
            written: 0.0,
            layout,
            more: false,
        })
    }

    /// Whether a contact is down or the offset is still moving.
    fn busy(&self) -> bool {
        self.more || self.driver.has_contact()
    }

    /// The editor moved by itself: the driver continues from where it is.
    fn follow(&mut self, editor_y: f32) -> Result<(), WinError> {
        if (editor_y - self.written).abs() > SCROLL_EPSILON {
            self.driver.jump_to(f64::from(editor_y))?;
            self.written = editor_y;
        }
        Ok(())
    }

    /// Hands `offset` to the editor, which draws the part of it inside its range (the viewport
    /// rubber-bands past the ends).
    fn write(&mut self, view: &mut ShellView, offset: f64) {
        let wanted = self.layout.extent.clamp(offset) as f32;
        if (wanted - self.written).abs() <= SCROLL_EPSILON {
            return;
        }
        let area = view.layout().rects().editor;
        if let Some(editor) = view.active_editor_mut() {
            editor.set_scroll_y(wanted);
        }
        view.requests_mut().invalidate(area);
        self.written = wanted;
    }

    /// One turn of the pump: a new size, DPI or document length reaches the driver while
    /// nothing moves, the editor's own position is adopted and, while a contact is down or the
    /// offset moves, the driver's frame is written to the editor. Returns whether another
    /// frame is needed.
    fn step(&mut self, view: &mut ShellView, now: Duration) -> Result<bool, WinError> {
        let Some(editor_y) = view.active_tab().map(|tab| tab.editor.scroll_y()) else {
            return Ok(false);
        };
        let layout = scroll_layout(view);
        let mut clamped = false;
        if !self.busy() && layout != self.layout {
            clamped = self.driver.set_layout(&layout)?;
            self.layout = layout;
        }
        self.follow(editor_y)?;
        if self.busy() || clamped {
            let frame = self.driver.frame(now)?;
            self.more = frame.more;
            self.write(view, frame.offset);
        }
        Ok(self.busy())
    }

    /// A wheel step the editor applied at once, from `before`, is animated by the driver
    /// instead.
    fn wheel(&mut self, wheel: &WheelEvent, before: f32) -> Result<(), WinError> {
        self.follow(before)?;
        self.more |= self.driver.on_wheel(wheel)?;
        Ok(())
    }
}

// ----- the platform ---------------------------------------------------------------------------

/// Everything platform-specific of one window.
pub struct Platform {
    cell: Rc<WindowCell>,
    env: Rc<WindowEnv>,
    caret: RefCell<Option<SystemCaret>>,
    tsf: RefCell<Option<TextServices>>,
    shadow: RefCell<Option<TsfShadow>>,
    uia: RefCell<Option<UiaBridge>>,
    uia_active: Cell<bool>,
    actions: Arc<AccessQueue>,
    drops: RefCell<Option<WindowDropTarget>>,
    objects: RefCell<Option<Rc<ObjectProvider>>>,
    hwnd: Cell<Option<HWND>>,
    logger: RefCell<Option<Arc<Logger>>>,
    scroll: RefCell<Option<EditorScroll>>,
    /// The clock of the scroll animations.
    started: Instant,
    taskbar: RefCell<Option<WindowTaskbar>>,
    /// The registered message the taskbar sends when it creates the window's button anew.
    button_created: u32,
}

/// The taskbar button of one window, over the real taskbar or a test substitute.
type WindowTaskbar = Taskbar<Box<dyn TaskbarApi>>;

impl std::fmt::Debug for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Platform")
            .field("window", &self.cell.id)
            .field("tsf", &self.tsf.borrow().is_some())
            .field("uia", &self.uia.borrow().is_some())
            .finish_non_exhaustive()
    }
}

impl Platform {
    /// A platform with nothing attached yet.
    #[must_use]
    pub fn new(cell: Rc<WindowCell>, env: Rc<WindowEnv>) -> Rc<Self> {
        Rc::new(Self {
            cell,
            env,
            caret: RefCell::new(None),
            tsf: RefCell::new(None),
            shadow: RefCell::new(None),
            uia: RefCell::new(None),
            uia_active: Cell::new(false),
            actions: Arc::new(AccessQueue {
                items: Mutex::new(Vec::new()),
                wake: Mutex::new(None),
            }),
            drops: RefCell::new(None),
            objects: RefCell::new(None),
            hwnd: Cell::new(None),
            logger: RefCell::new(None),
            scroll: RefCell::new(None),
            started: Instant::now(),
            taskbar: RefCell::new(None),
            button_created: taskbar_button_created_message(),
        })
    }

    /// Connects the pieces to the native window: editor setup (blocks, spelling, objects), the
    /// caret, text services, accessibility and the drop target.
    pub fn attach(&self, host: &WindowHost, logger: &Arc<Logger>) {
        let hwnd = host.hwnd();
        self.hwnd.set(Some(hwnd));
        *self.logger.borrow_mut() = Some(Arc::clone(logger));
        *self
            .actions
            .wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(WakeHandle::new(hwnd));
        self.attach_editor_setup(logger);
        match EditorScroll::new(hwnd, &self.cell.view.borrow()) {
            Ok(scroll) => *self.scroll.borrow_mut() = Some(scroll),
            Err(error) => logger.log(
                Level::Warn,
                "scroll",
                &format!("touch and touchpad scrolling unavailable: {error}"),
            ),
        }
        *self.caret.borrow_mut() = Some(SystemCaret::new(hwnd));
        let text_host: Rc<dyn TextStoreHost> = Rc::new(EditorTextHost {
            cell: Rc::clone(&self.cell),
            env: Rc::clone(&self.env),
        });
        match TextServices::new(hwnd, text_host) {
            Ok(services) => *self.tsf.borrow_mut() = Some(services),
            Err(error) => logger.log(
                Level::Warn,
                "tsf",
                &format!("text services unavailable: {error}"),
            ),
        }
        let root = self.cell.view.borrow().root_id();
        *self.uia.borrow_mut() = Some(UiaBridge::new(
            hwnd,
            Arc::clone(&self.actions) as Arc<dyn ActionSink>,
            root,
        ));
        let drops: Rc<dyn DropHandler> = Rc::new(WindowDrops {
            cell: Rc::clone(&self.cell),
            env: Rc::clone(&self.env),
        });
        match WindowDropTarget::register(hwnd, drops) {
            Ok(target) => *self.drops.borrow_mut() = Some(target),
            Err(error) => logger.log(
                Level::Warn,
                "drop",
                &format!("drag and drop unavailable: {error}"),
            ),
        }
    }

    fn attach_editor_setup(&self, logger: &Arc<Logger>) {
        let (workers, database, cache_dir) = {
            let core = self.env.controller.core().borrow();
            (
                Rc::clone(&core.workers),
                core.paths.state_db(),
                core.paths.image_cache_dir(),
            )
        };
        let policy = Rc::clone(self.env.controller.remote_images());
        let auto_load = self
            .env
            .controller
            .settings()
            .attachments
            .load_remote_images;
        let remote = RemoteSource {
            http: Arc::clone(&self.env.http),
            database,
            cache_dir,
            logger: Arc::clone(logger),
            doc: Cell::new(None),
            auto_load: Cell::new(auto_load),
            stamp: Cell::new(RemoteStamp {
                doc: None,
                epoch: policy.epoch(),
                auto_load,
            }),
            policy,
        };
        let provider = Rc::new(ObjectProvider {
            window: self.cell.id,
            workers,
            entries: RefCell::new(HashMap::new()),
            done: Rc::new(RefCell::new(Vec::new())),
            next_id: Cell::new(1),
            base: RefCell::new(None),
            remote,
        });
        *self.objects.borrow_mut() = Some(Rc::clone(&provider));
        let (spell_on, languages) = {
            let settings = self.env.controller.settings();
            (
                settings.editor.spell_check,
                settings.editor.spell_languages.clone(),
            )
        };
        let spell_check = match SpellFactory::new().and_then(|factory| factory.checker(&languages))
        {
            Ok(checker) => {
                let check = Rc::new(SpellCheck::new(Rc::new(WindowsSpell(checker))));
                check.set_enabled(spell_on);
                Some(check)
            }
            Err(error) => {
                logger.log(
                    Level::Warn,
                    "spell",
                    &format!("spell checker unavailable: {error}"),
                );
                None
            }
        };
        let i18n = Rc::clone(self.env.controller.i18n());
        if let Some(check) = &spell_check {
            self.env.controller.register_spell(check);
        }
        let setup: super::shell_view::EditorSetup = Rc::new(move |editor: &mut EditorView| {
            let registry = editor.registry_mut();
            blocks_text::register_with(registry, blocks_text::BlockStrings::new(&i18n));
            let media = blocks_media::MediaServices {
                strings: blocks_media::MediaStrings::new(Rc::clone(&i18n)),
                ..Default::default()
            };
            blocks_media::register_with(registry, media);
            let table = blocks_table::register_with(
                registry,
                blocks_table::TableStrings::new(Rc::clone(&i18n)),
            );
            if let Some(check) = &spell_check {
                spell::register(registry, Rc::clone(check));
            }
            editor.set_object_provider(Rc::clone(&provider) as Rc<dyn InlineObjectProvider>);
            Some(table)
        });
        self.cell.view.borrow_mut().set_editor_setup(setup);
    }

    /// Applies finished objects, answers accessibility requests, moves the caret and tells the
    /// text store what changed. `repaint`: the view changed since the last call.
    pub fn sync(&self, host: &WindowHost, repaint: bool) {
        self.apply_objects(host);
        self.apply_access_actions();
        if repaint {
            self.place_caret();
            if self.uia_active.get() {
                self.publish_access();
            }
        }
        self.notify_text_services();
    }

    fn apply_objects(&self, host: &WindowHost) {
        let Some(provider) = self.objects.borrow().clone() else {
            return;
        };
        if let Ok(view) = self.cell.view.try_borrow() {
            let tab = view.active_tab();
            if let Some(tab) = tab {
                *provider.base.borrow_mut() = tab
                    .entry
                    .path()
                    .and_then(|path| path.parent().map(std::path::Path::to_path_buf));
            }
            provider.remote.doc.set(tab.map(|tab| tab.entry.id()));
        }
        let auto_load = self
            .env
            .controller
            .settings()
            .attachments
            .load_remote_images;
        provider.remote.auto_load.set(auto_load);
        // Another document, a decision of the user or the setting: the remote images are asked
        // for again.
        let stamp = RemoteStamp {
            doc: provider.remote.doc.get(),
            epoch: provider.remote.policy.epoch(),
            auto_load,
        };
        let policy_changed = provider.remote.stamp.replace(stamp) != stamp;
        let finished: Vec<Done> = provider.done.borrow_mut().drain(..).collect();
        if finished.is_empty() && !policy_changed {
            return;
        }
        for item in finished {
            let (key, entry) = match item {
                Done::Scene(key, Some(scene)) => {
                    let (w, h) = fit(
                        scene.size.width,
                        scene.size.height,
                        f32::from_bits(key.max_width_bits),
                    );
                    let id = SceneId(provider.next_id.replace(provider.next_id.get() + 1));
                    let baseline = scene
                        .baseline
                        .map_or(h, |b| b * h / scene.size.height.max(f32::EPSILON));
                    host.with_assets(|assets| assets.register_scene(id, Arc::new(scene)));
                    let info = ObjectInfo {
                        visual: ObjectVisual::Scene(id),
                        size: Size::new(w, h),
                        baseline,
                    };
                    (key, Entry::Ready(info))
                }
                Done::Image(key, Some(bytes)) => {
                    let id = ImageId(provider.next_id.replace(provider.next_id.get() + 1));
                    let decoded = host
                        .with_assets(|assets| assets.images.register_encoded(id, &bytes).ok())
                        .flatten();
                    match decoded {
                        Some((pw, ph)) => {
                            let (mut w, mut h) = (pw as f32, ph as f32);
                            match (key.width, key.height) {
                                (Some(rw), Some(rh)) => (w, h) = (rw as f32, rh as f32),
                                (Some(rw), None) => {
                                    (w, h) = (rw as f32, h * rw as f32 / w.max(1.0))
                                }
                                (None, Some(rh)) => {
                                    (w, h) = (w * rh as f32 / h.max(1.0), rh as f32)
                                }
                                (None, None) => {}
                            }
                            let (w, h) = fit(w, h, f32::from_bits(key.max_width_bits));
                            let info = ObjectInfo {
                                visual: ObjectVisual::Image(id),
                                size: Size::new(w, h),
                                baseline: h,
                            };
                            (key, Entry::Ready(info))
                        }
                        None => (key, Entry::Failed),
                    }
                }
                Done::Scene(key, None) | Done::Image(key, None) => (key, Entry::Failed),
            };
            provider.entries.borrow_mut().insert(key, entry);
        }
        if let Ok(mut view) = self.cell.view.try_borrow_mut() {
            for tab in view.tabs_mut() {
                tab.editor.invalidate_objects();
            }
            view.requests_mut().invalidate_all();
        }
    }

    fn place_caret(&self) {
        let Ok(view) = self.cell.view.try_borrow() else {
            return;
        };
        let rect = view.active_tab().and_then(|tab| tab.editor.caret_rect());
        let scale = view.layout().scale();
        drop(view);
        if let Some(caret) = self.caret.borrow_mut().as_mut() {
            match rect {
                Some(rect) => {
                    let _ = caret.place(rect, scale);
                }
                None => caret.release(),
            }
        }
    }

    fn apply_access_actions(&self) {
        // A busy view leaves the actions queued for the next turn.
        let Ok(mut view) = self.cell.view.try_borrow_mut() else {
            return;
        };
        let queued: Vec<(WidgetId, AccessAction)> = std::mem::take(
            &mut *self
                .actions
                .items
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if queued.is_empty() {
            return;
        }
        let mut shell_actions = Vec::new();
        for (id, action) in queued {
            shell_actions.extend(view.perform_access(id, &action, &self.env.text));
        }
        drop(view);
        for action in shell_actions {
            self.env
                .controller
                .push(Request::Action(self.cell.id, action));
        }
    }

    fn access_tree(&self) -> Option<AccessTree> {
        let view = self.cell.view.try_borrow().ok()?;
        Some(view.access_tree(
            &self.env.text,
            &self.env.controller.i18n().text("window-title-app"),
        ))
    }

    fn publish_access(&self) {
        if let Some(tree) = self.access_tree()
            && let Some(bridge) = self.uia.borrow().as_ref()
        {
            let _ = bridge.update(tree);
        }
    }

    /// Reports edits and selection changes of the active document to the text store.
    fn notify_text_services(&self) {
        let tsf = self.tsf.borrow();
        let Some(services) = tsf.as_ref() else {
            return;
        };
        let current = {
            let Ok(view) = self.cell.view.try_borrow() else {
                return;
            };
            view.active_tab().map(|tab| {
                let document = Rc::clone(tab.entry.document());
                let (version, selection) = {
                    let doc = document.borrow();
                    (doc.version(), doc.primary_selection())
                };
                (tab.key, version, selection, document)
            })
        };
        let Some((tab, version, selection, document)) = current else {
            *self.shadow.borrow_mut() = None;
            return;
        };
        let mut shadow = self.shadow.borrow_mut();
        match shadow.as_mut() {
            Some(last) if last.tab == tab && last.version == version => {
                if last.selection != selection {
                    last.selection = selection;
                    drop(shadow);
                    services.store().selection_changed();
                }
            }
            Some(last) if last.tab == tab => {
                let text = document.borrow().text();
                let (start, removed, inserted) = diff(&last.text, &text);
                let (removed, inserted) = (removed.to_owned(), inserted.to_owned());
                last.version = version;
                last.selection = selection;
                last.text = text;
                drop(shadow);
                services.store().text_changed(start, &removed, &inserted);
                services.store().selection_changed();
            }
            _ => {
                let text = document.borrow().text();
                *shadow = Some(TsfShadow {
                    tab,
                    version,
                    text,
                    selection,
                });
                drop(shadow);
                services.store().layout_changed();
            }
        }
    }

    /// The layout changed (resize, DPI): text services and the caret follow.
    pub fn layout_changed(&self) {
        if let Some(services) = self.tsf.borrow().as_ref() {
            services.store().layout_changed();
        }
        self.place_caret();
    }

    /// The window was activated or deactivated: text services take or give up the focus and
    /// DirectManipulation follows.
    pub fn activation_changed(&self, active: bool) {
        if active && let Some(services) = self.tsf.borrow().as_ref() {
            services.set_focus();
        }
        self.with_scroll(|scroll| {
            if active {
                scroll.driver.activate()
            } else {
                scroll.driver.deactivate()
            }
        });
    }

    /// Answers `WM_GETOBJECT` with the UI Automation provider of the window and watches the
    /// messages the platform pieces care about: the start of a touch or touchpad contact, a
    /// system settings change and the taskbar re-creating the window's button. None of them
    /// is consumed (`None`): the window's own handling follows.
    pub fn on_message(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_GETOBJECT => {
                if !self.uia_active.replace(true) {
                    self.publish_access();
                }
                return self
                    .uia
                    .borrow()
                    .as_ref()
                    .and_then(|bridge| bridge.handle_get_object(wparam, lparam));
            }
            WM_POINTERDOWN | DM_POINTERHITTEST => self.start_contact(msg, wparam),
            WM_SETTINGCHANGE => {
                self.with_scroll(|scroll| {
                    // Turning animations off finishes a running one: its last offset is
                    // written by the next frame.
                    scroll.more |= scroll.driver.refresh_system_settings();
                    Ok(())
                });
            }
            _ if msg == self.button_created => {
                if let Ok(mut taskbar) = self.taskbar.try_borrow_mut()
                    && let Some(taskbar) = taskbar.as_mut()
                {
                    let _ = taskbar.button_created();
                }
            }
            _ => {}
        }
        None
    }

    /// The window is being destroyed: the accessibility providers are detached and the scroll
    /// driver and the taskbar button are released while the window still exists.
    pub fn disconnect(&self) {
        if let Some(bridge) = self.uia.borrow().as_ref() {
            bridge.disconnect();
        }
        if let Ok(mut scroll) = self.scroll.try_borrow_mut() {
            *scroll = None;
        }
        if let Ok(mut taskbar) = self.taskbar.try_borrow_mut() {
            *taskbar = None;
        }
    }

    // ----- scrolling -----------------------------------------------------------------------

    /// Runs `step` on the scroll driver. A driver that fails is logged and dropped: scrolling
    /// then goes on with the editor's own handling, as when DirectManipulation is missing.
    fn with_scroll<R>(
        &self,
        step: impl FnOnce(&mut EditorScroll) -> Result<R, WinError>,
    ) -> Option<R> {
        let mut slot = self.scroll.try_borrow_mut().ok()?;
        let scroll = slot.as_mut()?;
        match step(scroll) {
            Ok(value) => Some(value),
            Err(error) => {
                *slot = None;
                if let Some(logger) = self.logger.borrow().as_ref() {
                    logger.log(
                        Level::Warn,
                        "scroll",
                        &format!("touch and touchpad scrolling stopped: {error}"),
                    );
                }
                None
            }
        }
    }

    /// The scroll offset of the active editor.
    fn editor_scroll(&self) -> Option<f32> {
        let view = self.cell.view.try_borrow().ok()?;
        view.active_tab().map(|tab| tab.editor.scroll_y())
    }

    /// The scroll offset (DIPs) of the window's scroll driver; `None` when DirectManipulation
    /// is not available.
    #[must_use]
    pub fn scroll_offset(&self) -> Option<f64> {
        let scroll = self.scroll.try_borrow().ok()?;
        scroll.as_ref().map(|scroll| scroll.driver.offset())
    }

    /// Whether the client pixel `(x, y)` lies on the editor: a touch contact there scrolls it.
    #[must_use]
    pub fn over_editor(&self, (x, y): (i32, i32)) -> bool {
        let Ok(view) = self.cell.view.try_borrow() else {
            return false;
        };
        let scale = view.layout().scale();
        view.active_tab().is_some()
            && view
                .layout()
                .rects()
                .editor
                .contains(Point::new(x as f32 / scale, y as f32 / scale))
    }

    /// `WM_POINTERDOWN` or `DM_POINTERHITTEST`: a touch or precision-touchpad contact on the
    /// editor goes to DirectManipulation (the pen stays a pointer). A contact that is over
    /// before it can be handed over (a quick tap) is not an error: its pointer events still
    /// reach the editor.
    fn start_contact(&self, msg: u32, wparam: WPARAM) {
        let Some(hwnd) = self.hwnd.get() else {
            return;
        };
        self.route_contact(
            msg,
            wparam,
            pointer_is_pen(wparam),
            pointer_client_position(hwnd, wparam),
        );
    }

    /// [`Platform::start_contact`] once the system was asked about the pointer: whether it is a
    /// pen and where it is (client pixels; `None` for a pointer the system does not know).
    fn route_contact(&self, msg: u32, wparam: WPARAM, pen: bool, position: Option<(i32, i32)>) {
        if pen {
            return;
        }
        let Some(position) = position else {
            return;
        };
        if !self.over_editor(position) {
            return;
        }
        let editor_y = self.editor_scroll();
        let Ok(mut slot) = self.scroll.try_borrow_mut() else {
            return;
        };
        if let Some(scroll) = slot.as_mut() {
            let adopted = editor_y.map_or(Ok(()), |y| scroll.follow(y));
            let _ = adopted.and_then(|()| scroll.driver.on_pointer_message(msg, wparam));
        }
    }

    /// Whether `pointer` is a touch or touchpad move of a contact DirectManipulation is
    /// scrolling with: it must not also select text.
    #[must_use]
    pub fn is_scrolling_contact(&self, pointer: &PointerEvent) -> bool {
        matches!(pointer.kind, PointerEventKind::Move)
            && matches!(
                pointer.pointer_type,
                PointerType::Touch | PointerType::Touchpad
            )
            && self.scroll.try_borrow().is_ok_and(|scroll| {
                scroll
                    .as_ref()
                    .is_some_and(|scroll| scroll.driver.has_contact())
            })
    }

    /// Delivers a wheel event to the view with `deliver` and animates what the editor scrolled:
    /// the editor jumps by a step, which is undone and handed to the scroll driver instead
    /// (system lines per notch, spring for detented wheels, direct for precise ones). Wheel
    /// events that do not change the editor's vertical offset (zoom, horizontal scrolling,
    /// other panels, an editor at its top) are left as they are.
    pub fn wheel(&self, wheel: &WheelEvent, deliver: impl FnOnce()) {
        let before = self.editor_scroll();
        deliver();
        let Some(before) = before else {
            return;
        };
        let Ok(mut view) = self.cell.view.try_borrow_mut() else {
            return;
        };
        let Some(editor) = view.active_editor_mut() else {
            return;
        };
        if (editor.scroll_y() - before).abs() <= SCROLL_EPSILON {
            return;
        }
        editor.set_scroll_y(before);
        drop(view);
        self.with_scroll(|scroll| scroll.wheel(wheel, before));
    }

    /// One turn of scrolling for the pump: the driver and the editor agree on the offset, the
    /// driver's motion (contact, inertia, wheel spring) is applied to the editor and the editor
    /// is invalidated. Returns whether another frame is needed.
    pub fn scroll_frame(&self) -> bool {
        let now = self.started.elapsed();
        let Ok(mut view) = self.cell.view.try_borrow_mut() else {
            // A modal loop holds the view: keep the frames coming while something moves.
            return self
                .scroll
                .try_borrow()
                .is_ok_and(|scroll| scroll.as_ref().is_some_and(EditorScroll::busy));
        };
        self.with_scroll(|scroll| scroll.step(&mut view, now))
            .unwrap_or(false)
    }

    // ----- taskbar -------------------------------------------------------------------------

    /// Shows `progress` on the window's taskbar button. The taskbar object is made on the first
    /// request; it shows the progress again when the taskbar re-creates the button.
    pub fn set_progress(&self, progress: Progress) {
        let Some(hwnd) = self.hwnd.get() else {
            return;
        };
        let Ok(mut taskbar) = self.taskbar.try_borrow_mut() else {
            return;
        };
        if taskbar.is_none() {
            *taskbar = self
                .env
                .taskbar
                .create()
                .ok()
                .map(|api| Taskbar::with_api(hwnd, api));
        }
        if let Some(taskbar) = taskbar.as_mut() {
            let _ = taskbar.set(progress);
        }
    }
}

/// The edit that turns `old` into `new`: the byte offset, the removed and the inserted text
/// (common prefix and suffix are kept, on char boundaries).
fn diff<'a>(old: &'a str, new: &'a str) -> (usize, &'a str, &'a str) {
    let mut start = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    let max_tail = old.len().min(new.len()) - start;
    let mut tail = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(max_tail)
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - tail) || !new.is_char_boundary(new.len() - tail) {
        tail -= 1;
    }
    (
        start,
        &old[start..old.len() - tail],
        &new[start..new.len() - tail],
    )
}

#[cfg(test)]
mod tests;
