//! The owner of the message loop ([`UiApp`], [`UiShell`], the single-instance hook): requests,
//! closing, forwarded launches, the Jump List and the startup arms of `UiShell::run`. Windows are
//! hidden; the one test that shows, maximizes and raises real windows runs on a private desktop
//! (`ssw_win::testing::PrivateDesktop`) and never touches the user's screen or focus.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use ssw_app::app::{AppCore, EXIT_NO_DEVICE, EXIT_NO_SHELL, ShellHost, run};
use ssw_app::args::{Args, Launch};
use ssw_app::startup::{InstanceHook, InstanceRole, ProcessEnv};
use ssw_app::ui::controller::Request;
use ssw_app::ui::host::{
    InstanceLink, OFFSCREEN_ORIGIN, UiApp, UiOptions, UiServices, UiShell, WinInstance,
};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_app::workers::{JobKey, JobKind, JobSpec, WindowId};
use ssw_core::command::CommandId;
use ssw_io::fs::RealFs;
use ssw_io::session::WindowPlacement;
use ssw_ui::geometry::Size;
use ssw_ui::input::Cursor;
use ssw_ui::services::MemoryClipboard;
use ssw_win::com::ComGuard;
use ssw_win::dragdrop::OleGuard;
use ssw_win::gfx::Driver;
use ssw_win::jumplist::JumpList;
use ssw_win::menus::{PopupHost, PopupRequest};
use ssw_win::window::{current_os_version, send_message};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, GetWindowRect, HMENU, IsWindow, IsWindowVisible, IsZoomed, WM_MOUSEMOVE,
};

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
    let services = ScriptedServices::new();
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
    let file = note(tmp.path(), "a.md", "some text to point at\n");
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
