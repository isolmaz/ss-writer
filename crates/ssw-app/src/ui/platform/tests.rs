//! The platform wiring of a window over hidden windows of a real startup: the text store's
//! host, accessibility actions, the drop handler, the object provider, scrolling, the taskbar
//! button and every piece that cannot be created. Windows are never shown or activated; input
//! is made by calling the pieces, never by `SendInput`.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ssw_core::edit::Selection;
use ssw_core::text::TextRange;
use ssw_io::logging::{LogOptions, Logger};
use ssw_io::net::{Http, HttpRequest, HttpResponse, NetError};
use ssw_ui::access::{AccessAction, Role};
use ssw_ui::canvas::{Canvas, DrawOp, RecordingCanvas};
use ssw_ui::editor::EditorEvent;
use ssw_ui::editor::object::{
    InlineObjectProvider, ObjectKind, ObjectRequest, ObjectState, ObjectVisual,
};
use ssw_ui::editor::spell::SpellProvider;
use ssw_ui::geometry::{Color, Point, Rect};
use ssw_ui::input::{
    Event, ImeEvent, Modifiers, PointerEvent, PointerEventKind, PointerType, WheelEvent,
};
use ssw_ui::scroll_physics::Extent;
use ssw_ui::services::MemoryClipboard;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widgets::Control;
use ssw_win::com::{ComGuard, WinError};
use ssw_win::dragdrop::{
    AllowedEffects, DragContent, DragEvent, DragKeys, DropHandler, DropKinds, Effect, OleGuard,
};
use ssw_win::gfx::Driver;
use ssw_win::menus::{PopupHost, PopupRequest};
use ssw_win::spell::{SpellChecker, SpellFactory};
use ssw_win::taskbar::{Progress, TaskbarApi, TaskbarFactory};
use ssw_win::tsf::TextStoreHost;
use ssw_win::uia::ActionSink;
use ssw_win::wic::{DecodedImage, Wic};
use ssw_win::window::{
    WindowHandler, WindowHost, WindowInfo, WindowOptions, current_os_version, send_message,
};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::Threading::{
    CreateMutexW, INFINITE, ReleaseMutex, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{TBPF_NOPROGRESS, TBPF_NORMAL, TBPFLAG};
use windows::Win32::UI::WindowsAndMessaging::{
    HMENU, WM_CHAR, WM_GETOBJECT, WM_PAINT, WM_POINTERDOWN,
};
use windows::core::HSTRING;

use super::{
    Done, EditorTextHost, Entry, ObjectKey, ObjectProvider, Platform, WindowDrops, WindowsSpell,
    diff, fit, scroll_layout,
};
use crate::app::{AppCore, ShellHost, run};
use crate::args::Args;
use crate::startup::ProcessEnv;
use crate::ui::controller::Request;
use crate::ui::host::{UiApp, UiOptions, UiServices};
use crate::ui::scripted_services::ScriptedServices;
use crate::ui::shell_view::ShellAction;

// ----- the harness ----------------------------------------------------------------------------

/// Menus are never shown in these tests.
struct NoPopups;

impl PopupHost for NoPopups {
    fn track(&mut self, _menu: HMENU, _request: &PopupRequest) -> Option<u32> {
        None
    }
}

/// What a test substitutes for the real thing.
#[derive(Default)]
struct Seams {
    http: Option<Arc<dyn Http>>,
    taskbar: Option<Rc<dyn TaskbarFactory>>,
}

struct Probe<F> {
    body: Option<F>,
    seams: Seams,
}

impl<F: FnOnce(&UiApp, &Path)> ShellHost for Probe<F> {
    fn run(&mut self, core: AppCore, args: &Args) -> i32 {
        // Declared first: dropped after every COM object of the window.
        let _ole = OleGuard::initialize().unwrap();
        let services = ScriptedServices::new();
        let seams = std::mem::take(&mut self.seams);
        let options = UiOptions {
            hidden: true,
            services: Some(UiServices {
                dialogs: services.dialogs(),
                shell: services.shell(),
                clipboard: Rc::new(RefCell::new(MemoryClipboard::default())),
                popups: Box::new(NoPopups),
            }),
            instance: None,
            jump_list: false,
            http: seams.http,
            taskbar: seams.taskbar,
        };
        let app = UiApp::new(core, options).unwrap();
        app.pump();
        let file = args.paths[0].clone();
        (self.body.take().unwrap())(&app, &file);
        0
    }
}

/// Runs `body` inside a real startup over a temporary profile with `notes.md` (holding `text`)
/// open in a hidden window and the platform of that window.
fn with_seams(text: &str, seams: Seams, body: impl FnOnce(&UiApp, &Rc<Platform>, &Path)) {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("notes.md");
    std::fs::write(&file, text).unwrap();
    let mut args = Args {
        paths: vec![file],
        ..Args::default()
    };
    args.test.hidden = true;
    args.test.profile = Some(tmp.path().join("profile"));
    let process = ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Hardware,
        install_crash_handler: false,
    };
    let mut dialogs = ScriptedServices::new().dialogs();
    let code = run(
        &args,
        process,
        &mut *dialogs,
        None,
        &mut Probe {
            body: Some(move |app: &UiApp, file: &Path| {
                let id = app.controller().windows()[0].id;
                let platform = app.platform(id).expect("the window has a platform");
                body(app, &platform, file);
            }),
            seams,
        },
    );
    assert_eq!(code, 0);
}

fn with_app(text: &str, body: impl FnOnce(&UiApp, &Rc<Platform>, &Path)) {
    with_seams(text, Seams::default(), body);
}

/// A document of a few hundred screens.
fn long_document() -> String {
    (0..400)
        .map(|n| format!("Paragraph number {n} of a long document.\n\n"))
        .collect()
}

struct Idle;

impl WindowHandler for Idle {
    fn on_event(&mut self, _info: &WindowInfo, _event: Event) {}
    fn on_paint(&mut self, _info: &WindowInfo, _canvas: &mut dyn Canvas) {}
}

/// A second hidden window, for platforms that are attached to a window of their own.
fn spare_window() -> WindowHost {
    let options = WindowOptions {
        title: "spare".to_owned(),
        rect: Some((-32000, -32000, 800, 600)),
        dark: false,
        no_activate: true,
    };
    WindowHost::create(&options, Box::new(Idle), Theme::light(DEFAULT_ACCENT)).unwrap()
}

/// A platform over the shared window state that was never attached to a window.
fn bare(platform: &Platform) -> Rc<Platform> {
    Platform::new(Rc::clone(&platform.cell), Rc::clone(&platform.env))
}

fn text_host(platform: &Platform) -> EditorTextHost {
    EditorTextHost {
        cell: Rc::clone(&platform.cell),
        env: Rc::clone(&platform.env),
    }
}

/// Lays the active editor out (what a paint does).
fn layout_now(platform: &Platform) {
    let mut canvas = RecordingCanvas::new();
    platform
        .cell
        .view
        .borrow_mut()
        .paint(&mut canvas, &platform.env.text, false);
}

fn doc_text(platform: &Platform) -> String {
    let view = platform.cell.view.borrow();
    view.active_tab().unwrap().entry.document().borrow().text()
}

fn hwnd_of(app: &UiApp, platform: &Platform) -> HWND {
    app.hwnd(platform.cell.id)
        .expect("the native window exists")
}

fn close_active_tab(platform: &Platform) {
    let mut view = platform.cell.view.borrow_mut();
    let key = view.active_tab().expect("a tab is open").key;
    view.close_tab(key);
    assert!(view.active_tab().is_none());
}

fn set_editor_scroll(platform: &Platform, y: f32) {
    platform
        .cell
        .view
        .borrow_mut()
        .active_editor_mut()
        .expect("a tab is open")
        .set_scroll_y(y);
}

/// A client pixel well inside the editor.
fn editor_pixel(platform: &Platform) -> (i32, i32) {
    let view = platform.cell.view.borrow();
    let area = view.layout().rects().editor;
    let scale = view.layout().scale();
    (
        ((area.x + area.width / 2.0) * scale) as i32,
        ((area.y + 80.0) * scale) as i32,
    )
}

fn drain_requests(platform: &Platform) -> Vec<Request> {
    let mut requests = Vec::new();
    while let Some(request) = platform.env.controller.take_request() {
        requests.push(request);
    }
    requests
}

fn log_of(logger: &Logger) -> String {
    logger
        .paths()
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

fn app_log(platform: &Platform) -> String {
    let logger = Arc::clone(&platform.env.controller.core().borrow().logger);
    log_of(&logger)
}

fn temp_logger(dir: &Path) -> Arc<Logger> {
    Arc::new(Logger::new(dir, LogOptions::default()).unwrap())
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.5
}

fn wheel_event() -> WheelEvent {
    WheelEvent {
        delta: Point::new(0.0, -1.0),
        precise: false,
        position: Point::new(300.0, 300.0),
        mods: Modifiers::NONE,
    }
}

// ----- small pure helpers -----------------------------------------------------------------------

#[test]
fn diff_finds_the_changed_middle_on_char_boundaries() {
    assert_eq!(diff("hello world", "hello brave world"), (6, "", "brave "));
    assert_eq!(diff("abc", "abc"), (3, "", ""));
    assert_eq!(diff("aéb", "aèb"), (1, "é", "è"));
    assert_eq!(diff("", "x"), (0, "", "x"));
    // The two characters share their last byte: the common tail ends inside them.
    assert_eq!(diff("é", "©"), (0, "é", "©"));
    assert_eq!(diff("aé", "a©"), (1, "é", "©"));
}

#[test]
fn objects_are_scaled_down_to_the_column_but_never_up() {
    assert_eq!(fit(200.0, 100.0, 100.0), (100.0, 50.0));
    assert_eq!(fit(80.0, 40.0, 100.0), (80.0, 40.0));
    assert_eq!(fit(200.0, 100.0, 200.0), (200.0, 100.0));
    // No column at all: the object keeps its size.
    assert_eq!(fit(200.0, 100.0, 0.0), (200.0, 100.0));
}

// ----- the text store's host ----------------------------------------------------------------------

#[test]
fn the_text_host_reads_and_edits_the_active_document() {
    with_app("hello world\n", |_app, platform, _| {
        layout_now(platform);
        let host = text_host(platform);
        assert_eq!(host.text(), "hello world\n");
        host.set_selection(Selection {
            anchor: 6,
            head: 11,
        });
        assert_eq!(
            host.selection(),
            Selection {
                anchor: 6,
                head: 11
            }
        );

        // The edit is verbatim and the selection follows it.
        assert!(host.replace(0..5, "Hi"));
        assert_eq!(host.text(), "Hi world\n");
        assert_eq!(host.selection(), Selection { anchor: 3, head: 8 });
        // An edit outside the text is refused and changes nothing.
        assert!(!host.replace(3..99, "x"));
        assert_eq!(host.text(), "Hi world\n");
        assert!(!host.is_read_only());

        layout_now(platform);
        let area = platform.cell.view.borrow().layout().rects().editor;
        assert_eq!(host.view_rect(), Some(area));
        let caret = host.range_rect(3..3).expect("offset 3 is on screen");
        assert!(area.contains(Point::new(caret.x, caret.y)));
        let word = host.range_rect(3..8).expect("the word is on screen");
        assert!(word.width > caret.width);
        let at = Point::new(caret.x + 1.0, caret.y + caret.height / 2.0);
        assert_eq!(host.offset_at_point(at), Some(3));
        // The composition is drawn at the caret (offset 8).
        let head = host.range_rect(8..8);
        assert_eq!(host.composition_rect(0..0), head);
        assert!(head.is_some());
    });
}

#[test]
fn a_range_whose_end_is_off_screen_is_the_rectangle_of_its_start() {
    with_app(&long_document(), |_app, platform, _| {
        layout_now(platform);
        let host = text_host(platform);
        let start = host.range_rect(0..0).expect("the top is on screen");
        let far = host.text().len() - 2;
        assert_eq!(host.range_rect(far..far), None, "the end is not laid out");
        assert_eq!(host.range_rect(0..far), Some(start));
        assert_eq!(host.range_rect(far..far + 1), None);
        // A point outside the editor is over no text.
        assert_eq!(host.offset_at_point(Point::new(-5.0, -5.0)), None);
    });
}

fn assert_no_document(host: &EditorTextHost) {
    assert_eq!(host.text(), "");
    assert_eq!(host.selection(), Selection { anchor: 0, head: 0 });
    assert!(host.is_read_only());
    assert!(!host.replace(0..0, "x"));
    assert_eq!(host.view_rect(), None);
    assert_eq!(host.range_rect(0..1), None);
    assert_eq!(host.composition_rect(0..0), None);
    assert_eq!(host.offset_at_point(Point::new(300.0, 300.0)), None);
}

#[test]
fn a_busy_view_leaves_the_text_host_without_a_document_and_ignores_its_edits() {
    with_app("hello world\n", |_app, platform, _| {
        layout_now(platform);
        let host = text_host(platform);
        drain_requests(platform);
        {
            let _busy = platform.cell.view.borrow_mut();
            assert_no_document(&host);
            host.set_selection(Selection { anchor: 2, head: 4 });
            host.ime(ImeEvent::Commit("x".to_owned()));
        }
        assert_eq!(doc_text(platform), "hello world\n");
        assert_eq!(host.selection(), Selection { anchor: 0, head: 0 });
        assert!(drain_requests(platform).is_empty());
        assert_eq!(host.text(), "hello world\n");
    });
}

#[test]
fn a_window_without_a_tab_gives_the_text_host_nothing_to_edit() {
    with_app("hello\n", |_app, platform, _| {
        let host = text_host(platform);
        close_active_tab(platform);
        drain_requests(platform);
        assert_no_document(&host);
        host.set_selection(Selection { anchor: 1, head: 2 });
        host.ime(ImeEvent::Start);
        assert_eq!(host.selection(), Selection { anchor: 0, head: 0 });
        assert!(drain_requests(platform).is_empty());
    });
}

#[test]
fn a_read_only_file_makes_the_text_host_read_only() {
    with_app("hello\n", |_app, platform, file| {
        let host = text_host(platform);
        let entry = Rc::clone(&platform.cell.view.borrow().active_tab().unwrap().entry);
        let set_read_only = |read_only: bool| {
            let mut permissions = std::fs::metadata(file).unwrap().permissions();
            permissions.set_readonly(read_only);
            std::fs::set_permissions(file, permissions).unwrap();
            platform.env.controller.docs().refresh_access(&entry);
        };
        set_read_only(true);
        let read_only = host.is_read_only();
        set_read_only(false);
        assert!(read_only);
        assert!(!host.is_read_only());
    });
}

#[test]
fn composition_events_reach_the_editor_and_its_events_the_controller() {
    with_app("hello world\n", |_app, platform, _| {
        layout_now(platform);
        let host = text_host(platform);
        drain_requests(platform);
        host.ime(ImeEvent::Start);
        host.ime(ImeEvent::Commit("é".to_owned()));
        host.ime(ImeEvent::End);
        assert_eq!(doc_text(platform), "éhello world\n");
        let requests = drain_requests(platform);
        let edited = requests.iter().any(|request| {
            matches!(
                request,
                Request::Action(id, ShellAction::Editor(_, EditorEvent::Edited))
                    if *id == platform.cell.id
            )
        });
        assert!(edited, "{requests:?}");
    });
}

// ----- accessibility --------------------------------------------------------------------------------

#[test]
fn queued_accessibility_actions_run_on_the_next_turn_and_survive_a_busy_view() {
    with_app(&long_document(), |_app, platform, _| {
        layout_now(platform);
        let editor = platform
            .cell
            .view
            .borrow()
            .active_tab()
            .unwrap()
            .editor
            .id();
        let scroll_to = |percent: f32| AccessAction::ScrollTo {
            horizontal: None,
            vertical: Some(percent),
        };
        // The sink answers the UIA thread at once; the UI thread does the work later.
        assert!(platform.actions.perform(editor, scroll_to(100.0)));
        assert_eq!(platform.editor_scroll(), Some(0.0));
        {
            let _busy = platform.cell.view.borrow_mut();
            platform.apply_access_actions();
        }
        assert_eq!(platform.editor_scroll(), Some(0.0), "the action is kept");
        platform.apply_access_actions();
        let end = platform.editor_scroll().unwrap();
        assert!(end > 100.0, "{end}");
        // Nothing is left to run.
        set_editor_scroll(platform, 0.0);
        platform.apply_access_actions();
        assert_eq!(platform.editor_scroll(), Some(0.0));
    });
}

#[test]
fn an_accessibility_action_on_a_title_bar_item_reaches_the_shell() {
    with_app("hello\n", |app, platform, _| {
        let tree = platform.access_tree().expect("the view is free");
        let toggle = tree
            .nodes()
            .iter()
            .find(|node| node.role == Role::ToggleButton && node.name == "Sidebar")
            .expect("the sidebar toggle")
            .id;
        let before = platform.cell.view.borrow().layout().sidebar_visible();
        assert!(platform.actions.perform(toggle, AccessAction::Toggle));
        // The first turn takes the action, the second one runs what it asked for.
        app.pump();
        app.pump();
        let after = platform.cell.view.borrow().layout().sidebar_visible();
        assert_ne!(before, after);
    });
}

#[test]
fn the_accessibility_tree_is_published_once_a_client_asks_and_follows_the_view() {
    with_app("hello\n", |app, platform, _| {
        let hwnd = hwnd_of(app, platform);
        assert!(!platform.uia_active.get());
        send_message(hwnd, WM_GETOBJECT, 0, 0);
        assert!(platform.uia_active.get());
        let title = platform.env.controller.i18n().text("window-title-app");
        let tree = platform.uia.borrow().as_ref().unwrap().tree();
        assert!(tree.len() > 1);
        assert_eq!(tree.root().unwrap().name, title);

        // An edit repaints and publishes the new tree.
        send_message(hwnd, WM_CHAR, usize::from(b'!'), 0);
        app.pump();
        let published = platform.uia.borrow().as_ref().unwrap().tree();
        assert_eq!(published.len(), platform.access_tree().unwrap().len());

        // A busy view has no tree to publish.
        let _busy = platform.cell.view.borrow_mut();
        assert!(platform.access_tree().is_none());
    });
}

#[test]
fn a_platform_that_is_not_attached_still_answers_what_needs_no_window() {
    with_app(&long_document(), |_app, platform, _| {
        layout_now(platform);
        let spare = spare_window();
        let loose = bare(platform);
        assert_eq!(
            format!("{loose:?}"),
            format!(
                "Platform {{ window: {:?}, tsf: false, uia: false, .. }}",
                platform.cell.id
            )
        );
        assert!(format!("{platform:?}").contains("uia: true"));

        // It has no text services: nothing is recorded for them.
        loose.notify_text_services();
        assert!(loose.shadow.borrow().is_none());
        // A touch contact has no window to ask about, and the taskbar no window to decorate.
        assert!(
            loose
                .on_message(WM_POINTERDOWN, WPARAM(0x7777), LPARAM(0))
                .is_none()
        );
        assert_eq!(loose.scroll_offset(), None);
        // Nothing to detach.
        loose.disconnect();

        // Without objects it still takes the accessibility actions.
        let editor = platform
            .cell
            .view
            .borrow()
            .active_tab()
            .unwrap()
            .editor
            .id();
        let action = AccessAction::ScrollTo {
            horizontal: None,
            vertical: Some(100.0),
        };
        assert!(loose.actions.perform(editor, action));
        loose.sync(&spare, true);
        assert!(platform.editor_scroll().unwrap() > 100.0);
    });
}

// ----- the taskbar button -----------------------------------------------------------------------------

struct Recorder(Rc<RefCell<Vec<i32>>>);

impl TaskbarApi for Recorder {
    fn set_state(&mut self, _hwnd: HWND, state: TBPFLAG) -> windows::core::Result<()> {
        self.0.borrow_mut().push(state.0);
        Ok(())
    }

    fn set_value(
        &mut self,
        _hwnd: HWND,
        _completed: u64,
        _total: u64,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct Taskbars {
    created: Rc<Cell<usize>>,
    states: Rc<RefCell<Vec<i32>>>,
    fail: bool,
}

impl TaskbarFactory for Taskbars {
    fn create(&self) -> Result<Box<dyn TaskbarApi>, WinError> {
        self.created.set(self.created.get() + 1);
        if self.fail {
            return Err(WinError::Invalid("no taskbar"));
        }
        Ok(Box::new(Recorder(Rc::clone(&self.states))))
    }
}

/// The seams, the number of taskbar objects made and the progress states they were given.
type TaskbarSeams = (Seams, Rc<Cell<usize>>, Rc<RefCell<Vec<i32>>>);

fn taskbar_seams(fail: bool) -> TaskbarSeams {
    let created = Rc::new(Cell::new(0));
    let states = Rc::new(RefCell::new(Vec::new()));
    let factory = Taskbars {
        created: Rc::clone(&created),
        states: Rc::clone(&states),
        fail,
    };
    let seams = Seams {
        taskbar: Some(Rc::new(factory)),
        ..Seams::default()
    };
    (seams, created, states)
}

#[test]
fn progress_is_shown_by_one_taskbar_object_per_window_made_on_the_first_request() {
    let (seams, created, states) = taskbar_seams(false);
    with_seams("hello\n", seams, |_app, platform, _| {
        let progress = Progress::Normal {
            completed: 3,
            total: 10,
        };
        // No window yet: nothing is asked of the taskbar.
        bare(platform).set_progress(progress);
        assert_eq!(created.get(), 0);
        // The taskbar object is in use (a modal loop): the request is dropped.
        {
            let _busy = platform.taskbar.borrow_mut();
            platform.set_progress(progress);
        }
        assert_eq!(created.get(), 0);
        assert!(states.borrow().is_empty());

        platform.set_progress(progress);
        assert_eq!(created.get(), 1);
        assert_eq!(*states.borrow(), [TBPF_NORMAL.0]);
        // The next request uses the object that exists.
        platform.set_progress(Progress::None);
        assert_eq!(created.get(), 1);
        assert_eq!(*states.borrow(), [TBPF_NORMAL.0, TBPF_NOPROGRESS.0]);
    });
}

#[test]
fn a_taskbar_that_cannot_be_made_is_asked_for_again_with_the_next_request() {
    let (seams, created, states) = taskbar_seams(true);
    with_seams("hello\n", seams, |_app, platform, _| {
        let progress = Progress::Normal {
            completed: 1,
            total: 2,
        };
        platform.set_progress(progress);
        assert!(platform.taskbar.borrow().is_none());
        platform.set_progress(progress);
        assert_eq!(created.get(), 2);
        assert!(states.borrow().is_empty());
    });
}

// ----- scrolling -------------------------------------------------------------------------------------------

#[test]
fn a_window_without_a_document_scrolls_a_range_as_tall_as_its_viewport() {
    with_app("hello\n", |_app, platform, _| {
        close_active_tab(platform);
        let view = platform.cell.view.borrow();
        let area = view.layout().rects().editor.size();
        let layout = scroll_layout(&view);
        let height = f64::from(area.height);
        assert_eq!(layout.viewport, area);
        assert_eq!(layout.extent, Extent::new(height, height));
        assert_eq!(layout.line_height, 1.0);
        assert_eq!(layout.scale, view.layout().scale());
    });
}

#[test]
fn a_scroll_driver_that_cannot_run_is_dropped_and_logged() {
    with_app("hello\n", |_app, platform, _| {
        assert_eq!(platform.with_scroll(|_| Ok(7)), Some(7));
        {
            let _held = platform.scroll.borrow_mut();
            assert_eq!(platform.with_scroll(|_| Ok(7)), None);
            assert_eq!(platform.scroll_offset(), None);
        }
        assert!(platform.scroll_offset().is_some());
        assert_eq!(bare(platform).with_scroll(|_| Ok(7)), None);

        let failed = platform.with_scroll(|_| Err::<(), WinError>(WinError::Invalid("boom")));
        assert_eq!(failed, None);
        assert_eq!(platform.scroll_offset(), None);
        assert!(app_log(platform).contains("touch and touchpad scrolling stopped: boom"));
        // Without a driver nothing runs.
        assert_eq!(platform.with_scroll(|_| Ok(7)), None);
    });
}

#[test]
fn a_busy_view_keeps_the_scroll_frames_coming_only_while_something_moves() {
    with_app(&long_document(), |_app, platform, _| {
        let guard = platform.cell.view.borrow_mut();
        assert_eq!(platform.editor_scroll(), None);
        assert!(!platform.over_editor((300, 300)));
        assert!(!platform.scroll_frame());
        platform.scroll.borrow_mut().as_mut().unwrap().more = true;
        assert!(platform.scroll_frame());
        platform.scroll.borrow_mut().as_mut().unwrap().more = false;
        drop(guard);
        assert!(!platform.scroll_frame());
    });
}

#[test]
fn only_touch_and_touchpad_moves_of_a_tracked_contact_are_scrolling_contacts() {
    with_app(&long_document(), |_app, platform, _| {
        let pointer = |kind: PointerEventKind, pointer_type: PointerType| PointerEvent {
            kind,
            button: None,
            position: Point::new(10.0, 10.0),
            pointer_type,
            click_count: 0,
            mods: Modifiers::NONE,
        };
        let touch_move = pointer(PointerEventKind::Move, PointerType::Touch);
        assert!(
            !platform.is_scrolling_contact(&touch_move),
            "no contact yet"
        );
        platform
            .scroll
            .borrow_mut()
            .as_mut()
            .unwrap()
            .driver
            .start_contact_with(5, &mut |_, _| Ok(()))
            .unwrap();
        assert!(platform.is_scrolling_contact(&touch_move));
        assert!(
            platform.is_scrolling_contact(&pointer(PointerEventKind::Move, PointerType::Touchpad))
        );
        assert!(
            !platform.is_scrolling_contact(&pointer(PointerEventKind::Move, PointerType::Mouse))
        );
        assert!(
            !platform.is_scrolling_contact(&pointer(PointerEventKind::Down, PointerType::Touch))
        );
        // The contact keeps the pump going until it lifts.
        assert!(platform.scroll.borrow().as_ref().unwrap().busy());
    });
}

#[test]
fn a_contact_on_the_editor_starts_from_the_position_the_editor_has() {
    with_app(&long_document(), |_app, platform, _| {
        layout_now(platform);
        // The driver learns the scroll range of the laid-out document in a pump turn; until
        // then it only knows the range of the empty editor of the attach (and clamps to it).
        assert!(!platform.scroll_frame());
        let inside = editor_pixel(platform);
        let msg = WM_POINTERDOWN;
        let wparam = WPARAM(0x7777);
        set_editor_scroll(platform, 400.0);
        // A pen stays a pointer; a pointer the system does not know, one on the title bar and a
        // busy driver leave the driver where it was.
        platform.route_contact(msg, wparam, true, Some(inside));
        platform.route_contact(msg, wparam, false, None);
        platform.route_contact(msg, wparam, false, Some((2, 2)));
        {
            let _busy = platform.scroll.borrow_mut();
            platform.route_contact(msg, wparam, false, Some(inside));
        }
        assert_eq!(platform.scroll_offset(), Some(0.0));
        // Over the editor the driver continues from the editor's own offset.
        platform.route_contact(msg, wparam, false, Some(inside));
        let offset = platform.scroll_offset().unwrap();
        assert!((offset - 400.0).abs() < 0.5, "{offset}");
        // Without a driver there is nobody to hand the contact to.
        bare(platform).route_contact(msg, wparam, false, Some(inside));
    });
}

#[test]
fn a_wheel_over_a_window_without_a_tab_is_delivered_and_nothing_else() {
    with_app(&long_document(), |app, platform, _| {
        layout_now(platform);
        app.pump();
        let wheel = wheel_event();
        close_active_tab(platform);
        let mut delivered = false;
        platform.wheel(&wheel, || delivered = true);
        assert!(delivered);
        assert_eq!(platform.scroll_offset(), Some(0.0));
    });
}

#[test]
fn a_wheel_is_not_undone_while_the_view_is_busy_or_the_editor_is_gone() {
    with_app(&long_document(), |app, platform, _| {
        layout_now(platform);
        app.pump();
        let wheel = wheel_event();
        // The view is busy when the editor has handled the wheel: nothing is undone.
        let reader = platform.cell.view.borrow();
        platform.wheel(&wheel, || {});
        drop(reader);
        assert_eq!(platform.scroll_offset(), Some(0.0));
        // The editor loses its tab while it handles the wheel.
        platform.wheel(&wheel, || close_active_tab(platform));
        assert_eq!(platform.scroll_offset(), Some(0.0));
    });
}

#[test]
fn activation_follows_the_window_without_losing_the_scroll_driver() {
    with_app("hello\n", |_app, platform, _| {
        platform.activation_changed(true);
        assert!(platform.scroll_offset().is_some());
        platform.activation_changed(false);
        assert!(platform.scroll_offset().is_some());
    });
}

#[test]
fn disconnecting_releases_what_is_not_in_use() {
    with_app("hello\n", |_app, platform, _| {
        {
            let _scroll = platform.scroll.borrow_mut();
            let _taskbar = platform.taskbar.borrow_mut();
            platform.disconnect();
        }
        assert!(platform.scroll_offset().is_some(), "the driver was in use");
        platform.disconnect();
        assert_eq!(platform.scroll_offset(), None);
    });
}

// ----- the text store's view of the document ------------------------------------------------------------

#[test]
fn text_services_are_told_only_what_changed_while_the_view_is_free() {
    with_app("hello\n", |_app, platform, _| {
        platform.notify_text_services();
        let first = platform
            .shadow
            .borrow()
            .as_ref()
            .map(|s| (s.version, s.text.clone()));
        assert_eq!(first.as_ref().map(|s| s.1.as_str()), Some("hello\n"));

        let host = text_host(platform);
        assert!(host.replace(0..0, "oh "));
        {
            let _busy = platform.cell.view.borrow_mut();
            platform.notify_text_services();
        }
        assert_eq!(
            platform.shadow.borrow().as_ref().map(|s| s.text.clone()),
            Some("hello\n".to_owned())
        );
        platform.notify_text_services();
        {
            let slot = platform.shadow.borrow();
            let shadow = slot.as_ref().unwrap();
            assert_eq!(shadow.text, "oh hello\n");
            assert!(shadow.version > first.unwrap().0);
        }

        // Without a tab there is nothing to follow.
        close_active_tab(platform);
        platform.notify_text_services();
        assert!(platform.shadow.borrow().is_none());
    });
}

#[test]
fn a_synced_window_without_a_tab_or_with_a_busy_view_keeps_what_it_has() {
    with_app("hello\n", |_app, platform, _| {
        let spare = spare_window();
        let provider = Rc::clone(&platform.objects.borrow().clone().unwrap());
        let key = ObjectKey {
            kind: ObjectKind::Math as u8,
            source: "k".to_owned(),
            font_bits: 0,
            color: [0, 0, 0, 255],
            max_width_bits: 0,
            width: None,
            height: None,
            base: None,
        };
        // A finished job is stored even when the view cannot be told.
        provider
            .done
            .borrow_mut()
            .push(Done::Scene(key.clone(), None));
        {
            let _busy = platform.cell.view.borrow_mut();
            platform.apply_objects(&spare);
        }
        assert!(matches!(
            provider.entries.borrow().get(&key),
            Some(Entry::Failed)
        ));
        // Pictures that cannot be decoded (here: no graphics on this window) fail.
        let picture = ObjectKey {
            source: "p".to_owned(),
            ..key
        };
        provider
            .done
            .borrow_mut()
            .push(Done::Image(picture.clone(), Some(b"not an image".to_vec())));
        close_active_tab(platform);
        platform.sync(&spare, true);
        assert!(matches!(
            provider.entries.borrow().get(&picture),
            Some(Entry::Failed)
        ));
    });
}

// ----- spelling ----------------------------------------------------------------------------------------------------

/// Exclusive use of a resource of the machine across processes (the same named mutex as the
/// spelling tests of `ssw-win`).
struct MachineLock(HANDLE);

impl MachineLock {
    fn acquire(resource: &str) -> Self {
        let name = HSTRING::from(format!(r"Local\ssw-win-tests-{resource}"));
        // SAFETY: `name` is a valid NUL-terminated string; default security attributes.
        let handle = unsafe { CreateMutexW(None, false, &name) }.expect("CreateMutexW");
        // SAFETY: `handle` is the mutex that was just created or opened.
        let outcome = unsafe { WaitForSingleObject(handle, INFINITE) };
        assert!(outcome == WAIT_OBJECT_0 || outcome == WAIT_ABANDONED);
        Self(handle)
    }
}

impl Drop for MachineLock {
    fn drop(&mut self) {
        // SAFETY: releases and closes the mutex this value acquired on this thread.
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// The user's dictionary files of the folders `neutral` and `languages`, put back on drop.
struct DictionaryFiles {
    saved: Vec<(PathBuf, Option<Vec<u8>>)>,
    _exclusive: MachineLock,
}

impl DictionaryFiles {
    fn protect(languages: &[&str]) -> Self {
        let exclusive = MachineLock::acquire("spelling-dictionaries");
        let appdata = std::env::var_os("APPDATA").expect("APPDATA is set for a user session");
        let root = Path::new(&appdata).join(r"Microsoft\Spelling");
        let mut saved = Vec::new();
        for folder in std::iter::once("neutral").chain(languages.iter().copied()) {
            for name in ["default.dic", "default.exc", "default.acl"] {
                let path = root.join(folder).join(name);
                let content = std::fs::read(&path).ok();
                saved.push((path, content));
            }
        }
        Self {
            saved,
            _exclusive: exclusive,
        }
    }

    fn restore(&self) {
        for (path, content) in &self.saved {
            match content {
                Some(bytes) => {
                    if std::fs::read(path).ok().as_ref() != Some(bytes) {
                        if let Some(folder) = path.parent() {
                            let _ = std::fs::create_dir_all(folder);
                        }
                        let _ = std::fs::write(path, bytes);
                    }
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

impl Drop for DictionaryFiles {
    fn drop(&mut self) {
        // The service may write its files a moment after the call that caused it.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stable = 0;
        while stable < 2 && Instant::now() < deadline {
            self.restore();
            std::thread::sleep(Duration::from_millis(150));
            let unchanged = self
                .saved
                .iter()
                .all(|(path, content)| std::fs::read(path).ok() == *content);
            stable = if unchanged { stable + 1 } else { 0 };
        }
    }
}

fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}

fn unique_word() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let letters: String = nanos
        .to_string()
        .bytes()
        .map(|digit| char::from(b'a' + (digit - b'0')))
        .collect();
    format!(
        "zqx{letters}{}",
        char::from(b'a' + (std::process::id() % 26) as u8)
    )
}

#[test]
fn the_spell_provider_suggests_ignores_and_adds_words_of_the_windows_checker() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = SpellFactory::new().unwrap();
    if !factory.is_supported("en-US") {
        return;
    }
    let _files = DictionaryFiles::protect(&["en-US"]);
    let checker: SpellChecker = factory.checker(&["en-US".to_owned()]).unwrap();
    let spell = WindowsSpell(checker);

    let suggestions = spell.suggestions("speling");
    assert!(
        suggestions.iter().any(|word| word == "spelling"),
        "{suggestions:?}"
    );

    let ignored = unique_word();
    let text = format!("{ignored} stays");
    assert_eq!(spell.check(&text), [0..ignored.len()]);
    spell.ignore(&ignored);
    assert!(eventually(|| spell.check(&text).is_empty()));

    let added = format!("{}x", unique_word());
    let text = format!("{added} stays");
    assert_eq!(spell.check(&text), [0..added.len()]);
    spell.add_to_dictionary(&added);
    assert!(eventually(|| spell.check(&text).is_empty()));
}

// ----- dropping on the window --------------------------------------------------------------------------------------

fn drop_carets(platform: &Platform) -> usize {
    let mut canvas = RecordingCanvas::new();
    platform
        .cell
        .view
        .borrow_mut()
        .paint(&mut canvas, &platform.env.text, false);
    canvas
        .ops()
        .filter(|op| matches!(op, DrawOp::FillRect { rect, .. } if (rect.width - 2.0).abs() < 1e-3))
        .count()
}

fn drops_of(platform: &Platform) -> WindowDrops {
    WindowDrops {
        cell: Rc::clone(&platform.cell),
        env: Rc::clone(&platform.env),
    }
}

/// A drag at the DIP position `at` of the window.
fn drag_at(platform: &Platform, at: Point, text: bool) -> DragEvent<'static> {
    let scale = platform.cell.view.borrow().layout().scale();
    DragEvent {
        x: (at.x * scale).round() as i32,
        y: (at.y * scale).round() as i32,
        keys: DragKeys::default(),
        allowed: AllowedEffects::ALL,
        kinds: DropKinds {
            files: false,
            text,
            html: false,
        },
        files: &[],
    }
}

#[test]
fn dragged_text_shows_a_drop_caret_until_the_drag_leaves() {
    with_app("hello world\n", |_app, platform, _| {
        layout_now(platform);
        let drops = drops_of(platform);
        let caret: Rect = text_host(platform).range_rect(6..6).unwrap();
        let over = Point::new(caret.x + 1.0, caret.y + caret.height / 2.0);
        let event = drag_at(platform, over, true);
        let base = drop_carets(platform);

        // Over text the drop caret appears; the answer is the effect Windows would pick.
        assert_eq!(drops.drag_enter(&event), Effect::Copy);
        assert_eq!(drop_carets(platform), base + 1);
        assert_eq!(drops.drag_over(&event), Effect::Copy);
        let moved = drag_at(platform, Point::new(caret.x + 1.0, caret.y + 1.0), true);
        assert_eq!(drops.drag_over(&moved), Effect::Copy);
        drops.drag_leave();
        assert_eq!(drop_carets(platform), base);

        // Files and other kinds of data show no caret.
        let files = drag_at(platform, over, false);
        assert_eq!(drops.drag_over(&files), Effect::Copy);
        assert_eq!(drop_carets(platform), base);

        // A busy view is not touched; the pointer is taken as DIPs (scale 1).
        let _busy = platform.cell.view.borrow_mut();
        assert_eq!(drops.drag_over(&event), Effect::Copy);
        drops.drag_leave();
        let at = drops.point(&event);
        assert_eq!(at, Point::new(event.x as f32, event.y as f32));
    });
}

#[test]
fn dropped_text_lands_under_the_pointer_and_dropped_files_open_in_tabs() {
    with_app("hello world\n", |_app, platform, file| {
        layout_now(platform);
        let drops = drops_of(platform);
        let host = text_host(platform);
        let caret = host.range_rect(6..6).unwrap();
        let over = Point::new(caret.x + 1.0, caret.y + caret.height / 2.0);
        let text = |text: &str| DragContent {
            text: Some(text.to_owned()),
            ..DragContent::default()
        };

        // Over the title bar there is no text to drop on; an empty drop is refused too.
        let outside = drag_at(platform, Point::new(2.0, 2.0), true);
        assert_eq!(drops.dropped(&outside, &text("lost ")), Effect::None);
        assert_eq!(
            drops.dropped(&outside, &DragContent::default()),
            Effect::None
        );
        assert_eq!(doc_text(platform), "hello world\n");

        let event = drag_at(platform, over, true);
        assert_eq!(drops.dropped(&event, &text("big ")), Effect::Copy);
        assert_eq!(doc_text(platform), "hello big world\n");

        // A read-only editor refuses it.
        layout_now(platform);
        platform
            .cell
            .view
            .borrow_mut()
            .active_editor_mut()
            .unwrap()
            .set_read_only(true);
        assert_eq!(drops.dropped(&event, &text("more ")), Effect::None);
        assert_eq!(doc_text(platform), "hello big world\n");

        // Files are opened, in order; the answer is a copy.
        let second = file.with_file_name("second.md");
        std::fs::write(&second, "second\n").unwrap();
        let content = DragContent {
            files: vec![second],
            ..DragContent::default()
        };
        assert_eq!(drops.dropped(&event, &content), Effect::Copy);
        let view = platform.cell.view.borrow();
        let path = view.active_tab().unwrap().entry.path().unwrap();
        assert_eq!(path.file_name().unwrap(), "second.md");
    });
}

// ----- objects -------------------------------------------------------------------------------------------------------------

const BLACK: Color = Color {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};
const WHITE: Color = Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 1.0,
};

fn provider_of(platform: &Platform) -> Rc<ObjectProvider> {
    Rc::clone(platform.objects.borrow().as_ref().expect("attached"))
}

fn request(kind: ObjectKind, source: &str, color: Color) -> ObjectRequest<'_> {
    ObjectRequest {
        kind,
        source,
        alt: "",
        width: None,
        height: None,
        font_size: 16.0,
        max_width: 10_000.0,
        text_color: color,
        range: TextRange::new(0, 1),
    }
}

/// Asks for the object until it is no longer pending (the pump delivers the worker's result).
fn settled(app: &UiApp, platform: &Platform, request: &ObjectRequest<'_>) -> ObjectState {
    let provider = provider_of(platform);
    let hwnd = hwnd_of(app, platform);
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let state = provider.object(request);
        if state != ObjectState::Pending {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "timed out for {:?}",
            request.source
        );
        app.pump();
        send_message(hwnd, WM_PAINT, 0, 0);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn ready(state: ObjectState) -> ssw_ui::editor::object::ObjectInfo {
    match state {
        ObjectState::Ready(info) => info,
        other => panic!("not ready: {other:?}"),
    }
}

#[test]
fn math_and_diagrams_are_typeset_in_the_background_and_fitted_to_the_column() {
    with_app("hello\n", |app, platform, _| {
        let provider = provider_of(platform);
        let inline = request(ObjectKind::Math, "x^2", BLACK);
        assert_eq!(provider.object(&inline), ObjectState::Pending);
        let info = ready(settled(app, platform, &inline));
        assert!(matches!(info.visual, ObjectVisual::Scene(_)));
        assert!(info.size.width > 0.0 && info.size.height > 0.0);
        assert!(info.baseline > 0.0 && info.baseline <= info.size.height + 0.01);

        let display = request(ObjectKind::DisplayMath, "\\frac{a}{b}", BLACK);
        let shown = ready(settled(app, platform, &display));
        assert_ne!(
            shown.visual, info.visual,
            "every scene has an id of its own"
        );

        // Dark text gets the dark diagram theme, black text the light one; a narrow column
        // scales the diagram down.
        let source = "graph TD\n  A-->B\n";
        let light = ready(settled(
            app,
            platform,
            &request(ObjectKind::Mermaid, source, BLACK),
        ));
        let mut narrow = request(ObjectKind::Mermaid, source, WHITE);
        narrow.max_width = 40.0;
        let dark = ready(settled(app, platform, &narrow));
        assert!(dark.size.width <= 40.01, "{:?}", dark.size);
        assert!(light.size.width > dark.size.width);
        assert_ne!(light.visual, dark.visual);

        // Formulas and diagrams that cannot be drawn fail, and stay failed.
        let broken = request(ObjectKind::Math, "\\left x", BLACK);
        assert_eq!(settled(app, platform, &broken), ObjectState::Failed);
        assert_eq!(provider.object(&broken), ObjectState::Failed);
        let nonsense = request(ObjectKind::Mermaid, "this is not a diagram", BLACK);
        assert_eq!(settled(app, platform, &nonsense), ObjectState::Failed);
    });
}

#[test]
fn local_images_are_read_next_to_the_document_and_sized_by_the_request() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let wic = Wic::new().unwrap();
    let pixels = vec![255; 240 * 180 * 4];
    let picture = wic
        .encode_png(&DecodedImage {
            width: 240,
            height: 180,
            bgra: pixels,
        })
        .unwrap();
    drop(wic);
    drop(_com);
    with_app("hello\n", move |app, platform, file| {
        let dir = file.parent().unwrap();
        std::fs::write(dir.join("pic.png"), &picture).unwrap();
        std::fs::write(dir.join("bad.png"), b"not an image").unwrap();
        let size = |request: &ObjectRequest<'_>| {
            let info = ready(settled(app, platform, request));
            assert!(matches!(info.visual, ObjectVisual::Image(_)));
            assert_eq!(info.baseline, info.size.height);
            (info.size.width, info.size.height)
        };
        let base = request(ObjectKind::Image, "pic.png", BLACK);
        assert_eq!(size(&base), (240.0, 180.0));

        let mut width = base.clone();
        width.width = Some(120);
        let (w, h) = size(&width);
        assert!(close(w, 120.0) && close(h, 90.0), "{w} x {h}");
        let mut height = base.clone();
        height.height = Some(90);
        let (w, h) = size(&height);
        assert!(close(w, 120.0) && close(h, 90.0), "{w} x {h}");
        let mut both = base.clone();
        both.width = Some(50);
        both.height = Some(60);
        assert_eq!(size(&both), (50.0, 60.0));
        let mut column = base.clone();
        column.max_width = 100.0;
        let (w, h) = size(&column);
        assert!(close(w, 100.0) && close(h, 75.0), "{w} x {h}");

        // An absolute path needs no document; a missing or undecodable file fails.
        let absolute = dir.join("pic.png").to_string_lossy().into_owned();
        assert_eq!(
            size(&request(ObjectKind::Image, &absolute, BLACK)),
            (240.0, 180.0)
        );
        let missing = request(ObjectKind::Image, "nope.png", BLACK);
        assert_eq!(settled(app, platform, &missing), ObjectState::Failed);
        let bad = request(ObjectKind::Image, "bad.png", BLACK);
        assert_eq!(settled(app, platform, &bad), ObjectState::Failed);

        // An untitled document has no folder to resolve a relative path against.
        platform.env.controller.new_untitled(&platform.cell);
        app.pump();
        assert_eq!(settled(app, platform, &base), ObjectState::Failed);
    });
}

#[test]
fn objects_of_a_window_whose_jobs_were_cancelled_fail_at_once() {
    with_app("hello\n", |_app, platform, _| {
        let workers = Rc::clone(&platform.env.controller.core().borrow().workers);
        workers.cancel_window(platform.cell.id);
        let provider = provider_of(platform);
        let state = provider.object(&request(ObjectKind::Math, "y^2", BLACK));
        assert_eq!(state, ObjectState::Failed);
    });
}

#[derive(Debug)]
struct PanickingHttp;

impl Http for PanickingHttp {
    fn get(&self, _request: &HttpRequest<'_>) -> Result<HttpResponse, NetError> {
        panic!("the network is gone");
    }
}

#[test]
fn a_remote_image_whose_job_panics_fails_and_the_reason_is_logged() {
    let seams = Seams {
        http: Some(Arc::new(PanickingHttp)),
        ..Seams::default()
    };
    with_seams("hello\n", seams, |app, platform, _| {
        let url = "https://example.com/pic.png";
        let remote = request(ObjectKind::Image, url, BLACK);
        assert_eq!(settled(app, platform, &remote), ObjectState::Failed);
        let log = app_log(platform);
        assert!(
            log.contains(&format!("{url}: job panicked: the network is gone")),
            "{log}"
        );
    });
}

// ----- the pieces that cannot be created -----------------------------------------------------------------------------

#[test]
fn a_window_that_is_already_a_drop_target_is_logged_and_left_without_drops() {
    with_app("hello\n", |_app, platform, _| {
        let spare = spare_window();
        let dir = tempfile::tempdir().unwrap();
        let (first_log, second_log) = (
            temp_logger(&dir.path().join("first")),
            temp_logger(&dir.path().join("second")),
        );
        let first = bare(platform);
        first.attach(&spare, &first_log);
        let second = bare(platform);
        second.attach(&spare, &second_log);
        assert!(first.drops.borrow().is_some());
        assert!(second.drops.borrow().is_none());
        assert!(log_of(&second_log).contains("drag and drop unavailable"));
        assert!(!log_of(&first_log).contains("unavailable"));
        // The rest was attached all the same.
        assert!(second.uia.borrow().is_some());
        assert!(second.scroll_offset().is_some());
    });
}

#[cfg(feature = "fault-injection")]
fn scrolling_stops_when(prepare: impl FnOnce(&Platform), act: impl FnOnce(&Platform)) {
    with_app(&long_document(), |app, platform, _| {
        layout_now(platform);
        app.pump();
        platform.scroll_frame();
        prepare(platform);
        ssw_win::fault::arm(0);
        act(platform);
        let reached = ssw_win::fault::tripped();
        ssw_win::fault::disarm();
        assert!(reached, "the action made a DirectManipulation call");
        assert_eq!(
            platform.scroll_offset(),
            None,
            "the failed driver is dropped"
        );
        assert!(app_log(platform).contains("touch and touchpad scrolling stopped"));
    });
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_failing_call_while_the_driver_follows_the_editor_stops_touch_scrolling() {
    scrolling_stops_when(
        |platform| set_editor_scroll(platform, 400.0),
        |platform| {
            platform.scroll_frame();
        },
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_failing_call_while_a_new_layout_reaches_the_driver_stops_touch_scrolling() {
    scrolling_stops_when(
        |platform| {
            platform
                .scroll
                .borrow_mut()
                .as_mut()
                .unwrap()
                .layout
                .viewport = ssw_ui::geometry::Size::new(1.0, 1.0);
        },
        |platform| {
            platform.scroll_frame();
        },
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_failing_frame_stops_touch_scrolling() {
    scrolling_stops_when(
        |platform| platform.scroll.borrow_mut().as_mut().unwrap().more = true,
        |platform| {
            platform.scroll_frame();
        },
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_failing_call_while_a_wheel_step_is_handed_over_stops_touch_scrolling() {
    // The editor had moved by itself: the driver is brought along before the step.
    scrolling_stops_when(
        |platform| set_editor_scroll(platform, 400.0),
        |platform| platform.wheel(&wheel_event(), || set_editor_scroll(platform, 450.0)),
    );
    // The driver is in step with the editor: the step itself fails.
    scrolling_stops_when(
        |_| {},
        |platform| platform.wheel(&wheel_event(), || set_editor_scroll(platform, 50.0)),
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_failing_deactivation_stops_touch_scrolling() {
    scrolling_stops_when(|_| {}, |platform| platform.activation_changed(false));
    scrolling_stops_when(|_| {}, |platform| platform.activation_changed(true));
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_contact_that_cannot_be_handed_over_is_not_an_error() {
    with_app(&long_document(), |app, platform, _| {
        layout_now(platform);
        app.pump();
        platform.scroll_frame();
        let inside = editor_pixel(platform);
        set_editor_scroll(platform, 400.0);
        ssw_win::fault::arm(0);
        platform.route_contact(WM_POINTERDOWN, WPARAM(0x7777), false, Some(inside));
        let reached = ssw_win::fault::tripped();
        ssw_win::fault::disarm();
        assert!(reached);
        // The driver goes on: a quick tap is still a pointer for the editor.
        assert!(platform.scroll_offset().is_some());
        assert!(!app_log(platform).contains("scrolling stopped"));
    });
}

#[cfg(feature = "fault-injection")]
#[test]
fn every_piece_that_cannot_be_created_is_logged_and_left_out() {
    with_app("hello\n", |_app, platform, _| {
        let dir = tempfile::tempdir().unwrap();
        let (mut scroll_lost, mut tsf_lost, mut spell_lost) = (false, false, false);
        for call in 0u32.. {
            let spare = spare_window();
            let logger = temp_logger(&dir.path().join(format!("run{call}")));
            let fresh = bare(platform);
            ssw_win::fault::arm(call);
            fresh.attach(&spare, &logger);
            let reached = ssw_win::fault::tripped();
            ssw_win::fault::disarm();
            if !reached {
                break;
            }
            let log = log_of(&logger);
            let scroll = fresh.scroll.borrow().is_none();
            let tsf = fresh.tsf.borrow().is_none();
            assert_eq!(
                scroll,
                log.contains("touch and touchpad scrolling unavailable"),
                "{log}"
            );
            assert_eq!(tsf, log.contains("text services unavailable"), "{log}");
            scroll_lost |= scroll;
            tsf_lost |= tsf;
            spell_lost |= log.contains("spell checker unavailable");
            // Whatever was left out, the window still has its accessibility bridge and objects.
            assert!(fresh.uia.borrow().is_some());
            assert!(fresh.objects.borrow().is_some());
        }
        assert!(scroll_lost && tsf_lost && spell_lost);
    });
}
