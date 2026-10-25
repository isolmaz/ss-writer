//! The owner of the message loop ([`UiApp`], [`UiShell`], the single-instance hook): requests,
//! closing, forwarded launches, the Jump List and the startup arms of `UiShell::run`. Windows are
//! hidden; the one test that shows, maximizes and raises real windows runs on a private desktop
//! (`ssw_win::testing::PrivateDesktop`) and never touches the user's screen or focus.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ssw_app::app::{AppCore, EXIT_NO_DEVICE, EXIT_NO_SHELL, ShellHost, run};
use ssw_app::args::{Args, Launch};
use ssw_app::documents::{DocEnv, Documents};
use ssw_app::i18n::localizer_for;
use ssw_app::startup::{InstanceHook, InstanceRole, ProcessEnv};
use ssw_app::ui::controller::Request;
use ssw_app::ui::host::{
    InstanceLink, OFFSCREEN_ORIGIN, UiApp, UiOptions, UiServices, UiShell, WinInstance,
};
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices};
use ssw_app::workers::{JobKey, JobKind, JobSpec, WindowId};
use ssw_core::command::CommandId;
use ssw_io::fs::{FaultFs, FsFailure, FsOp, RealFs};
use ssw_io::session::{RecentKind, WindowPlacement};
use ssw_ui::geometry::Size;
use ssw_ui::input::Cursor;
use ssw_ui::services::{MemoryClipboard, MessageBoxResult};
use ssw_win::com::ComGuard;
use ssw_win::dragdrop::OleGuard;
use ssw_win::gfx::Driver;
use ssw_win::instance::InstanceConfig;
use ssw_win::jumplist::JumpList;
use ssw_win::menus::{PopupHost, PopupRequest};
use ssw_win::window::{current_os_version, send_message};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowRect, HMENU, HWND_MESSAGE, IsWindow,
    IsWindowVisible, IsZoomed, RegisterClassExW, UnregisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_MOUSEMOVE, WNDCLASSEXW,
};
use windows::core::{HSTRING, PCWSTR};

/// Menus are never shown in these tests.
struct NoPopups;

impl PopupHost for NoPopups {
    fn track(&mut self, _menu: HMENU, _request: &PopupRequest) -> Option<u32> {
        None
    }
}

struct Body<F>(Option<F>);

impl<F: FnOnce(AppCore, &Args) -> i32> ShellHost for Body<F> {
    fn run(&mut self, core: AppCore, args: &Args) -> i32 {
        (self.0.take().expect("the body runs once"))(core, args)
    }
}

/// Starts the application over a temporary profile (with `files` as its launch) and hands the
/// core to `body`, which stands in for the shell. Returns the exit code of the run.
fn start(files: &[PathBuf], tmp: &Path, body: impl FnOnce(AppCore, &Args) -> i32) -> i32 {
    let mut args = Args {
        paths: files.to_vec(),
        ..Args::default()
    };
    args.test.hidden = true;
    args.test.profile = Some(tmp.join("profile"));
    let process = ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Hardware,
        install_crash_handler: false,
    };
    let mut dialogs = ScriptedServices::new().dialogs();
    run(&args, process, &mut *dialogs, None, &mut Body(Some(body)))
}

fn options(hidden: bool, instance: Option<InstanceLink>, jump_list: bool) -> UiOptions {
    options_over(&ScriptedServices::new(), hidden, instance, jump_list)
}

/// [`options`] whose dialogs and shell are `services`, so a test sees what the window showed.
fn options_over(
    services: &ScriptedServices,
    hidden: bool,
    instance: Option<InstanceLink>,
    jump_list: bool,
) -> UiOptions {
    UiOptions {
        hidden,
        services: Some(UiServices {
            dialogs: services.dialogs(),
            shell: services.shell(),
            clipboard: Rc::new(RefCell::new(MemoryClipboard::default())),
            popups: Box::new(NoPopups),
        }),
        instance,
        jump_list,
        update_check: false,
        http: None,
        taskbar: None,
    }
}

/// An application id no other test (and not the user's editor) shares.
fn unique_id(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("SSWriter.Test.{tag}.{}.{nanos}", std::process::id())
}

fn note(tmp: &Path, name: &str, text: &str) -> PathBuf {
    let path = tmp.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn rect_of(hwnd: windows::Win32::Foundation::HWND) -> RECT {
    let mut rect = RECT::default();
    // SAFETY: a valid out pointer; the window belongs to this thread.
    unsafe { GetWindowRect(hwnd, &mut rect) }.unwrap();
    rect
}

#[test]
fn the_app_services_and_windows_describe_themselves_and_an_offscreen_window_waits_far_away() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "a");
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let services = options(true, None, false).services.unwrap();
        assert_eq!(format!("{services:?}"), "UiServices { .. }");

        let app = UiApp::new(core, options(true, None, false)).unwrap();
        assert_eq!(format!("{app:?}"), "UiApp { windows: 0, hidden: true, .. }");
        // SAFETY: a plain query.
        assert!(unsafe { IsWindow(Some(app.message_hwnd())) }.as_bool());
        app.pump();
        assert_eq!(format!("{app:?}"), "UiApp { windows: 1, hidden: true, .. }");

        app.set_offscreen(true);
        let cell = app
            .controller()
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        app.pump();
        let rect = rect_of(app.hwnd(cell.id).unwrap());
        assert_eq!((rect.left, rect.top), (OFFSCREEN_ORIGIN, OFFSCREEN_ORIGIN));
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn a_later_launch_is_forwarded_to_the_first_instance_and_opened_there() {
    let tmp = tempfile::tempdir().unwrap();
    let first = note(tmp.path(), "first.md", "first");
    let second = note(tmp.path(), "second.md", "second");
    let id = unique_id("forward");
    let code = start(&[first], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let mut primary = WinInstance::with_app_id(id.clone());
        assert!(matches!(
            primary.claim(&Launch::default()).unwrap(),
            InstanceRole::Primary
        ));
        let link = primary.link();
        assert_eq!(link.app_id(), id);
        let app = UiApp::new(core, options(true, Some(link), false)).unwrap();
        app.pump();
        assert!(app.controller().docs().find(&second).is_none());

        // A later process of the same application hands its launch over and leaves.
        let launch = Args {
            paths: vec![second.clone()],
            ..Args::default()
        }
        .launch(&RealFs);
        let mut later = WinInstance::with_app_id(id.clone());
        assert!(matches!(
            later.claim(&launch).unwrap(),
            InstanceRole::Forwarded
        ));
        app.pump();
        assert!(app.controller().docs().find(&second).is_some());
        0
    });
    assert_eq!(code, 0);
}

/// The text of the box that names `paths`, none of which opened because it no longer exists.
fn gone_box(paths: &[PathBuf]) -> String {
    let reason = localizer_for("en").text("app-error-not-found");
    let heading = match paths {
        [_] => "The file could not be opened:".to_owned(),
        _ => format!("{} files could not be opened:", paths.len()),
    };
    let lines: Vec<String> = paths
        .iter()
        .map(|path| format!("{}: {reason}", path.display()))
        .collect();
    format!("{heading}\n\n{}", lines.join("\n"))
}

/// Presses OK in the next message box.
fn answer_ok(services: &ScriptedServices) {
    let ok = Answer::Button(MessageBoxResult::Ok);
    services.answer(DialogKind::MessageBox, ok);
}

/// The file names of the recent files, the newest first.
fn recent_names(app: &UiApp) -> Vec<String> {
    let items = app
        .controller()
        .docs()
        .env()
        .store
        .borrow()
        .recent_items(RecentKind::File)
        .unwrap();
    items
        .into_iter()
        .map(|item| {
            let name = item.path.file_name().unwrap();
            name.to_string_lossy().into_owned()
        })
        .collect()
}

/// Pumps `app` until `done` holds.
fn pump_until(app: &UiApp, what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        app.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn files_dropped_on_a_window_open_there_and_the_ones_that_fail_are_named_in_one_box() {
    let tmp = tempfile::tempdir().unwrap();
    let first = note(tmp.path(), "first.md", "first");
    let good = note(tmp.path(), "good.md", "good");
    let gone = [tmp.path().join("gone-1.md"), tmp.path().join("gone-2.md")];
    let services = ScriptedServices::new();
    let code = start(&[first], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options_over(&services, true, None, false)).unwrap();
        app.pump();
        let window = app.controller().windows()[0].id;

        // A window that is gone has nothing to open the files in.
        app.controller()
            .push(Request::OpenFiles(WindowId(999), vec![good.clone()]));
        app.pump();
        assert!(app.controller().docs().find(&good).is_none());
        assert!(services.dialog_log().is_empty());

        answer_ok(&services);
        let dropped = vec![gone[0].clone(), good.clone(), gone[1].clone()];
        app.controller().push(Request::OpenFiles(window, dropped));
        app.pump();
        assert!(app.controller().docs().find(&good).is_some());
        let shown = services.dialog_log();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].message, gone_box(&gone));
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn a_forwarded_launch_of_a_recent_file_that_vanished_names_it_and_forgets_it() {
    let tmp = tempfile::tempdir().unwrap();
    let first = note(tmp.path(), "first.md", "first");
    let gone = note(tmp.path(), "gone.md", "gone");
    let id = unique_id("vanished");
    let services = ScriptedServices::new();
    let code = start(&[first], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let mut primary = WinInstance::with_app_id(id.clone());
        assert!(matches!(
            primary.claim(&Launch::default()).unwrap(),
            InstanceRole::Primary
        ));
        let options = options_over(&services, true, Some(primary.link()), false);
        let mut app = UiApp::new(core, options).unwrap();
        app.pump();

        // The file was opened once, so it is a recent file; then it is closed and deleted.
        let cell = app.controller().windows()[0].clone();
        app.controller().open_file(&cell, &gone).unwrap();
        let key = cell.view.borrow().active_tab().unwrap().key;
        app.controller().close_tab(&cell, key);
        std::fs::remove_file(&gone).unwrap();
        assert_eq!(recent_names(&app), ["gone.md", "first.md"]);

        // A Jump List the shell refuses is logged at every refresh, which counts the refreshes.
        let long_program = PathBuf::from(format!(r"C:\{id}\{}.exe", "x".repeat(300)));
        app.set_jump_list(JumpList::new(
            format!("{id}.refused"),
            long_program,
            "Recent",
            "New window",
        ));
        app.pump();
        let log = app.controller().core().borrow().paths.logs_dir();
        let refreshes = || {
            let text = std::fs::read_to_string(log.join("ssw.log")).unwrap();
            text.matches("cannot update the Jump List").count()
        };
        assert_eq!(refreshes(), 1);

        // A later process of the application asks for the vanished file.
        answer_ok(&services);
        let launch = Args {
            paths: vec![gone.clone()],
            ..Args::default()
        }
        .launch(&RealFs);
        let mut later = WinInstance::with_app_id(id.clone());
        assert!(matches!(
            later.claim(&launch).unwrap(),
            InstanceRole::Forwarded
        ));
        app.pump();
        let shown = services.dialog_log();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].message, gone_box(&[gone]));
        assert_eq!(recent_names(&app), ["first.md"]);
        assert_eq!(refreshes(), 2, "the Jump List follows the pruned list");
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn a_file_the_watcher_cannot_reload_is_reported_once_until_it_reloads_again() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "one\n");
    let faults = Arc::new(FaultFs::new());
    let services = ScriptedServices::new();
    let code = start(std::slice::from_ref(&file), tmp.path(), |mut core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let env = DocEnv {
            fs: faults.clone(),
            ..core.docs.env().clone()
        };
        core.docs = Rc::new(Documents::new(env));
        let app = UiApp::new(core, options_over(&services, true, None, false)).unwrap();
        app.pump();
        let entry = app.controller().docs().find(&file).unwrap();
        let boxes = || services.dialog_log().len();
        let text = || entry.document().borrow().text();

        // Reading fails from now on: the reload the watcher asks for fails and is reported.
        faults.fail_always(FsOp::Read, FsFailure::AccessDenied);
        answer_ok(&services);
        std::fs::write(&file, "one, written by another program\n").unwrap();
        pump_until(&app, "the failed reload is reported", || boxes() == 1);

        // The file changes again and still cannot be read: nothing more is said.
        let reads = faults.calls(FsOp::Read);
        std::fs::write(&file, "one, written again by another program\n").unwrap();
        pump_until(&app, "a second read", || faults.calls(FsOp::Read) > reads);
        app.pump();
        assert_eq!(boxes(), 1);

        // A reload that works clears the report: the next failure is shown again.
        faults.clear();
        std::fs::write(&file, "readable again\n").unwrap();
        pump_until(&app, "the file reloads", || text() == "readable again\n");
        faults.fail_always(FsOp::Read, FsFailure::AccessDenied);
        answer_ok(&services);
        std::fs::write(&file, "unreadable once more, and longer\n").unwrap();
        pump_until(&app, "the new failure is reported", || boxes() == 2);
        assert_eq!(services.take_unexpected(), None);
        assert!(services.unused_answers().is_empty());
        0
    });
    assert_eq!(code, 0);
}

/// A running instance of the application that cannot take a launch: it owns the single-instance
/// mutex and a message window of the application's class, but the window procedure does not
/// handle `WM_COPYDATA`.
struct DeafInstance {
    mutex: HANDLE,
    window: HWND,
    class: HSTRING,
}

/// The window procedure of [`DeafInstance`]: default processing, which answers `WM_COPYDATA`
/// with "not handled".
unsafe extern "system" fn deaf_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: default processing with the original arguments.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

impl DeafInstance {
    fn start(config: &InstanceConfig) -> Self {
        let class = HSTRING::from(config.window_class());
        let name = HSTRING::from(config.mutex_name());
        // SAFETY: a valid NUL-terminated name; default security attributes.
        let mutex = unsafe { CreateMutexW(None, false, &name) }.unwrap();
        let description = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(deaf_proc),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        // SAFETY: the class structure is fully initialized; the name outlives the call.
        assert_ne!(unsafe { RegisterClassExW(&description) }, 0);
        // SAFETY: creates a message-only window of the class registered above.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                &class,
                PCWSTR::null(),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .unwrap();
        Self {
            mutex,
            window,
            class,
        }
    }
}

impl Drop for DeafInstance {
    fn drop(&mut self) {
        // SAFETY: destroys the window made by `start` on its own thread, removes its class and
        // closes the mutex handle, each once.
        unsafe {
            let _ = DestroyWindow(self.window);
            let _ = UnregisterClassW(&self.class, None);
            let _ = CloseHandle(self.mutex);
        }
    }
}

#[test]
fn a_launch_the_running_instance_does_not_take_is_reported_as_an_error() {
    let id = unique_id("deaf");
    let running = DeafInstance::start(&InstanceConfig::new(id.clone()));
    let mut later = WinInstance::with_app_id(id);
    let error = later.claim(&Launch::default()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "single-instance handshake failed: the running instance rejected the launch"
    );
    drop(running);
}

#[test]
fn requests_run_commands_close_windows_and_quit() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "text");
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options(true, None, false)).unwrap();
        app.pump();
        let first = app.controller().windows()[0].id;

        // A command of the window runs.
        let zoom = app.controller().settings().appearance.zoom_percent;
        app.controller()
            .push(Request::Command(first, CommandId::ViewZoomIn));
        app.pump();
        assert!(app.controller().settings().appearance.zoom_percent > zoom);

        // A window that no longer exists gets no native window.
        app.controller().push(Request::NewWindow(WindowId(999)));
        app.pump();
        assert_eq!(app.controller().windows().len(), 1);

        // Closing one of two windows leaves the other and the loop running.
        let second = app
            .controller()
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        app.pump();
        assert!(app.hwnd(second.id).is_some());
        app.controller().push(Request::CloseRequested(second.id));
        app.pump();
        assert!(app.hwnd(second.id).is_none());
        assert_eq!(app.exit_code(), None);

        // A window the system destroyed is skipped by the housekeeping.
        let hwnd = app.hwnd(first).unwrap();
        // SAFETY: destroys a window of this thread; the host tolerates a destroyed window.
        unsafe { DestroyWindow(hwnd) }.unwrap();
        app.pump();
        assert_eq!(app.exit_code(), None);

        // File -> Exit asks every window and leaves.
        app.controller().push(Request::Quit);
        app.pump();
        assert_eq!(app.exit_code(), Some(0));
        assert!(app.final_session().is_some());
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn a_pointer_over_the_editor_asks_for_the_text_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    // Many lines: the pointer lands on text, not on the empty space below the last block
    // (which asks for the arrow).
    let text = "some text to point at\n".repeat(200);
    let file = note(tmp.path(), "a.md", &text);
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options(true, None, false)).unwrap();
        app.pump();
        let cell = app.controller().windows()[0].clone();
        let (area, scale) = {
            let view = cell.view.borrow();
            (view.layout().rects().editor, view.layout().scale())
        };
        let x = ((area.x + area.width / 2.0) * scale) as isize;
        let y = ((area.y + area.height / 2.0) * scale) as isize;
        send_message(app.hwnd(cell.id).unwrap(), WM_MOUSEMOVE, 0, (y << 16) | x);
        app.pump();
        assert_eq!(cell.cursor.get(), Cursor::IBeam);
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn the_jump_list_follows_the_recent_files_and_a_refusing_shell_is_logged() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "a");
    let other = note(tmp.path(), "b.md", "b");
    let id = unique_id("jumplist");
    let exe = std::env::current_exe().unwrap();
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let link = WinInstance::with_app_id(id.clone()).link();
        let app = UiApp::new(core, options(true, Some(link), true)).unwrap();
        app.pump();
        let cell = app.controller().windows()[0].clone();
        app.controller().open_file(&cell, &other).unwrap();
        // The list of this application id is written for the recent files.
        app.pump();

        // The same files again: nothing new to write. A list the shell cannot build is logged.
        let mut app = app;
        let long_program = PathBuf::from(format!(r"C:\{id}\{}.exe", "x".repeat(300)));
        app.set_jump_list(JumpList::new(
            format!("{id}.refused"),
            long_program,
            "Recent",
            "New window",
        ));
        app.pump();
        let log = app.controller().core().borrow().paths.logs_dir();
        let text = std::fs::read_to_string(log.join("ssw.log")).unwrap();
        assert!(text.contains("cannot update the Jump List"), "{text}");

        // Nothing stays behind in the shell.
        JumpList::new(id.clone(), exe.clone(), "Recent", "New window")
            .delete()
            .unwrap();
        0
    });
    assert_eq!(code, 0);
}

/// The shell over a core whose graphics device somebody else already took.
fn shell_without_device(exit_after_start: bool) -> i32 {
    let tmp = tempfile::tempdir().unwrap();
    start(&[], tmp.path(), |mut core, args| {
        core.take_devices().unwrap();
        let mut args = args.clone();
        args.test.exit_after_start = exit_after_start;
        UiShell::new(None).run(core, &args)
    })
}

#[test]
fn the_quick_exit_without_a_device_ends_with_the_no_device_code() {
    assert_eq!(shell_without_device(true), EXIT_NO_DEVICE);
}

#[test]
fn the_real_startup_without_a_device_ends_with_the_no_device_code() {
    assert_eq!(shell_without_device(false), EXIT_NO_DEVICE);
}

#[test]
fn a_thread_in_another_com_mode_leaves_the_shell_without_ole() {
    let tmp = tempfile::tempdir().unwrap();
    let code = start(&[], tmp.path(), |core, args| {
        let _multi = ComGuard::multi_threaded().unwrap();
        UiShell::new(None).run(core, args)
    });
    assert_eq!(code, EXIT_NO_SHELL);
}

#[test]
fn the_shell_runs_its_loop_until_a_job_ends_it_and_survives_a_refused_app_id() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "a");
    // The shell refuses ids with spaces: the run goes on without an explicit id.
    let link = WinInstance::with_app_id("not a valid id").link();
    let code = start(&[file], tmp.path(), move |core, args| {
        core.workers
            .submit(
                JobSpec {
                    kind: JobKind::Io,
                    window: None,
                    key: JobKey(1),
                    generation: 1,
                },
                |_| (),
                |_| ssw_win::window::quit(0),
            )
            .unwrap();
        UiShell::new(Some(link)).run(core, args)
    });
    assert_eq!(code, 0);
}

#[test]
fn windows_are_shown_maximized_off_screen_and_raised_on_a_private_desktop() {
    let desktop = ssw_win::testing::PrivateDesktop::create().unwrap();
    let name = desktop.name().to_owned();
    let id = unique_id("desktop");
    let tmp = tempfile::tempdir().unwrap();
    let first = note(tmp.path(), "first.md", "first");
    let later_file = note(tmp.path(), "later.md", "later");
    let tmp_path = tmp.path().to_path_buf();
    let code = desktop.run(move || {
        assert_eq!(ssw_win::testing::current_desktop_name(), name);
        start(&[first], &tmp_path, |core, _| {
            let _ole = OleGuard::initialize().unwrap();
            let mut primary = WinInstance::with_app_id(id.clone());
            primary.claim(&Launch::default()).unwrap();
            let app = UiApp::new(core, options(false, Some(primary.link()), false)).unwrap();
            app.pump();
            let normal = app.controller().windows()[0].id;
            // SAFETY: plain queries of windows of this thread.
            assert!(unsafe { IsWindowVisible(app.hwnd(normal).unwrap()) }.as_bool());
            // SAFETY: a plain query of a window of this thread.
            assert!(!unsafe { IsZoomed(app.hwnd(normal).unwrap()) }.as_bool());

            // A window that was maximized when it was stored comes back maximized.
            let stored = WindowPlacement {
                x: 50,
                y: 50,
                width: 800,
                height: 600,
                maximized: true,
            };
            let maximized =
                app.controller()
                    .create_window(Some(stored), Size::new(800.0, 600.0), 1.0);
            app.pump();
            // SAFETY: a plain query of a window of this thread.
            assert!(unsafe { IsZoomed(app.hwnd(maximized.id).unwrap()) }.as_bool());

            // An off-screen window is shown where nobody sees it.
            app.set_offscreen(true);
            let away = app
                .controller()
                .create_window(None, Size::new(800.0, 600.0), 1.0);
            app.pump();
            let hwnd = app.hwnd(away.id).unwrap();
            // SAFETY: a plain query of a window of this thread.
            assert!(unsafe { IsWindowVisible(hwnd) }.as_bool());
            assert_eq!(rect_of(hwnd).left, OFFSCREEN_ORIGIN);

            // A launch of a later process brings the window to the front.
            let launch = Args {
                paths: vec![later_file.clone()],
                ..Args::default()
            }
            .launch(&RealFs);
            let mut later = WinInstance::with_app_id(id.clone());
            assert!(matches!(
                later.claim(&launch).unwrap(),
                InstanceRole::Forwarded
            ));
            app.pump();
            assert!(app.controller().docs().find(&later_file).is_some());

            // The Settings window is shown, and asking again raises it.
            app.controller().push(Request::OpenSettings);
            app.pump();
            let settings = app.settings_hwnd().unwrap();
            // SAFETY: a plain query of a window of this thread.
            assert!(unsafe { IsWindowVisible(settings) }.as_bool());
            app.controller().push(Request::OpenSettings);
            app.pump();
            assert_eq!(app.settings_hwnd(), Some(settings));
            0
        })
    });
    assert_eq!(code, 0);
}
