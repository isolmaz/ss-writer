//! The failure paths of the performance scenarios (`--perf-scenario`, `ui::perf_driver`): the
//! driver is ticked by the test over a real hidden window. Frames are either produced by sending
//! `WM_PAINT` to our own window (the document is really painted) or recorded by hand
//! ([`Recorder::frame`]), so a test decides exactly when the next frame exists. Nothing is shown,
//! activated or injected; the report goes to a temp dir.
//!
//! The recorder is process-wide: every run holds [`RECORDER_LOCK`] and resets the recorder
//! first, so the tests pass both under nextest (one process per test) and under `cargo test`
//! (threads of one process).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::{Args, PerfScenario};
use ssw_app::perf::{DOC_OPEN_RETURNED, SCROLL_BEGIN, SCROLL_END, TYPE_BEGIN};
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::WindowCell;
use ssw_app::ui::host::{UiApp, UiOptions, UiServices};
use ssw_app::ui::perf_driver::{EXIT_PERF_FAILED, Limits, PerfDriver, Tick};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_ui::services::MemoryClipboard;
use ssw_win::dragdrop::OleGuard;
use ssw_win::gfx::Driver;
use ssw_win::menus::{PopupHost, PopupRequest};
use ssw_win::perf::Recorder;
use ssw_win::window::{current_os_version, post_message, send_message};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, HMENU, MSG, PM_REMOVE, PeekMessageW, TranslateMessage, WM_NULL, WM_PAINT,
    WM_QUIT,
};

/// Held for the whole of one run: the recorder and its marks belong to one run at a time.
static RECORDER_LOCK: Mutex<()> = Mutex::new(());

/// Nothing in these runs waits longer than this for the driver.
const DEADLINE: Duration = Duration::from_secs(120);

struct NoPopups;

impl PopupHost for NoPopups {
    fn track(&mut self, _menu: HMENU, _request: &PopupRequest) -> Option<u32> {
        None
    }
}

/// Runs `body` over a hidden window of a real startup.
struct Probe<F> {
    body: Option<F>,
}

impl<F: FnOnce(&UiApp, &Args)> ShellHost for Probe<F> {
    fn run(&mut self, core: AppCore, args: &Args) -> i32 {
        let _ole = OleGuard::initialize().unwrap();
        let services = ScriptedServices::new();
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
            update_check: false,
            http: None,
            taskbar: None,
        };
        let app = UiApp::new(core, options).unwrap();
        app.pump();
        (self.body.take().unwrap())(&app, args);
        0
    }
}

fn report_path(tmp: &Path) -> PathBuf {
    tmp.join("out").join("report.json")
}

/// Runs `body` with the app of a fresh profile in `tmp`, the arguments of that run (a report
/// path, no scenario) and the process-wide recorder.
fn with_app(tmp: &Path, body: impl FnOnce(&UiApp, &Args, &'static Recorder)) {
    let mut args = Args::default();
    args.test.hidden = true;
    args.test.profile = Some(tmp.join("profile"));
    args.test.perf_report = Some(report_path(tmp));
    let process = ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Hardware,
        install_crash_handler: false,
    };
    // A failed test poisons the lock; the next one resets the recorder anyway.
    let _run = RECORDER_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let recorder = ssw_win::perf::enable();
    let mut dialogs = ScriptedServices::new().dialogs();
    let exit = run(
        &args,
        process,
        &mut *dialogs,
        None,
        &mut Probe {
            body: Some(move |app: &UiApp, args: &Args| body(app, args, recorder)),
        },
    );
    assert_eq!(exit, 0);
}

/// A driver for `scenario` over the report path of `args`.
fn new_driver(
    args: &Args,
    scenario: Option<PerfScenario>,
    target: Option<&Path>,
    step: Option<u32>,
    limits: Limits,
) -> PerfDriver {
    let mut args = args.clone();
    args.test.perf_scenario = scenario;
    args.test.perf_target = target.map(Path::to_path_buf);
    args.test.perf_scroll_step = step;
    PerfDriver::with_limits(&args, limits).unwrap()
}

fn read_report(tmp: &Path) -> Value {
    let text = std::fs::read_to_string(report_path(tmp)).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The error the last report of the run in `tmp` carries.
fn report_error(tmp: &Path) -> String {
    let report = read_report(tmp);
    assert_eq!(report["completed"], false, "{report}");
    report["error"].as_str().unwrap().to_owned()
}

/// Records a presented frame without drawing anything.
fn present(recorder: &Recorder) {
    recorder.frame(0.0, 0.0, 0.0);
}

/// One turn of the message loop that really paints the active window.
fn paint(app: &UiApp) {
    dispatch_queued();
    app.pump();
    if let Some(cell) = app.controller().active_window()
        && let Some(hwnd) = app.hwnd(cell.id)
    {
        send_message(hwnd, WM_PAINT, 0, 0);
    }
}

/// Dispatches the messages queued for this thread's windows. A `WM_QUIT` that was asked for
/// is the result: its exit code.
fn take_quit() -> Option<i32> {
    let mut message = MSG::default();
    let mut quit = None;
    // SAFETY: peeks and dispatches this thread's own queue with a valid MSG buffer.
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        if message.message == WM_QUIT {
            quit = Some(message.wParam.0 as i32);
        } else {
            // SAFETY: translates and dispatches the message this thread just retrieved.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    quit
}

fn dispatch_queued() {
    let _ = take_quit();
}

/// Ticks `driver` with a frame recorded by hand before each tick (`frames`) or none, until it
/// finishes; returns its exit code.
fn finish(app: &UiApp, driver: &mut PerfDriver, recorder: &Recorder, frames: bool) -> i32 {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if frames {
            present(recorder);
        }
        if let Tick::Finished(code) = driver.tick(app) {
            return code;
        }
        assert!(Instant::now() < deadline, "the scenario never ended");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Ticks `driver` with a frame recorded by hand before each tick until the recorder holds
/// `mark`. The driver then waits for the next frame.
fn until_mark(app: &UiApp, driver: &mut PerfDriver, recorder: &Recorder, mark: &'static str) {
    let deadline = Instant::now() + DEADLINE;
    while !recorder.has_mark(mark) {
        present(recorder);
        assert_eq!(driver.tick(app), Tick::Running);
        assert!(Instant::now() < deadline, "{mark} never came");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Like the loop of the real application: every turn paints the window for real.
fn paint_until_finished(app: &UiApp, driver: &mut PerfDriver) -> i32 {
    let deadline = Instant::now() + DEADLINE;
    loop {
        paint(app);
        if let Tick::Finished(code) = driver.tick(app) {
            return code;
        }
        assert!(Instant::now() < deadline, "the scenario never ended");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// [`paint_until_finished`], but stops when the recorder holds `mark`.
fn paint_until_mark(app: &UiApp, driver: &mut PerfDriver, recorder: &Recorder, mark: &'static str) {
    let deadline = Instant::now() + DEADLINE;
    while !recorder.has_mark(mark) {
        paint(app);
        assert_eq!(driver.tick(app), Tick::Running);
        assert!(Instant::now() < deadline, "{mark} never came");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn close_tabs(cell: &WindowCell) {
    let keys: Vec<u64> = cell
        .view
        .borrow()
        .tabs()
        .iter()
        .map(|tab| tab.key)
        .collect();
    for key in keys {
        let _ = cell.view.borrow_mut().close_tab(key);
    }
}

fn long_document(dir: &Path, paragraphs: usize) -> PathBuf {
    let file = dir.join("long.md");
    let text: String = (0..paragraphs)
        .map(|n| format!("Paragraph {n} of the text.\n\n"))
        .collect();
    std::fs::write(&file, text).unwrap();
    file
}

#[test]
fn a_driver_exists_only_for_a_run_with_a_report_path() {
    assert!(PerfDriver::new(&Args::default()).is_none());
}

#[test]
fn a_plain_run_only_records_and_leaves_the_wait_to_the_loop() {
    let tmp = tempfile::tempdir().unwrap();
    with_app(tmp.path(), |app, args, _| {
        let mut driver = new_driver(args, None, None, None, Limits::default());
        assert_eq!(driver.tick(app), Tick::Running);
        // The loop never sleeps longer than a millisecond while the driver runs ...
        assert_eq!(
            driver.drive(app, Some(Duration::from_millis(500))),
            Some(Duration::from_millis(1))
        );
        // ... but also not longer than the application itself wanted to ...
        assert_eq!(
            driver.drive(app, Some(Duration::from_micros(200))),
            Some(Duration::from_micros(200))
        );
        // ... and it leaves when the application decided to.
        assert_eq!(driver.drive(app, None), None);
        driver.finish();
    });
    let report = read_report(tmp.path());
    assert_eq!(report["scenario"], "manual");
    assert_eq!(report["completed"], true);
}

#[test]
fn a_scenario_waits_for_the_first_frame_then_ends_and_asks_the_loop_to_quit() {
    let tmp = tempfile::tempdir().unwrap();
    with_app(tmp.path(), |app, args, recorder| {
        recorder.reset();
        let mut driver = new_driver(
            args,
            Some(PerfScenario::Startup),
            None,
            None,
            Limits::default(),
        );
        assert_eq!(
            driver.drive(app, Some(Duration::from_millis(500))),
            Some(Duration::from_millis(1)),
            "no frame yet: the scenario goes on"
        );
        assert_eq!(take_quit(), None);
        present(recorder);
        assert_eq!(
            driver.drive(app, None),
            Some(Duration::from_millis(50)),
            "the loop may sleep briefly once the scenario asked it to quit"
        );
        assert_eq!(take_quit(), Some(0));
        // An ended scenario stays ended.
        assert_eq!(driver.tick(app), Tick::Finished(0));
    });
    let report = read_report(tmp.path());
    assert_eq!(report["scenario"], "startup");
    assert_eq!(report["completed"], true);
}

#[test]
fn a_scenario_past_its_time_limit_fails_with_the_stage_it_was_in() {
    let tmp = tempfile::tempdir().unwrap();
    let file = long_document(tmp.path(), 5);
    with_app(tmp.path(), |app, args, recorder| {
        recorder.reset();
        let limits = Limits {
            timeout: Duration::ZERO,
            ..Limits::default()
        };
        let mut driver = new_driver(
            args,
            Some(PerfScenario::Open60k),
            Some(file.as_path()),
            None,
            limits,
        );
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            driver.drive(app, None),
            Some(Duration::from_millis(50)),
            "a failed scenario also asks the loop to quit"
        );
        assert_eq!(take_quit(), Some(EXIT_PERF_FAILED));
        assert_eq!(driver.tick(app), Tick::Finished(EXIT_PERF_FAILED));
    });
    let error = report_error(tmp.path());
    assert!(
        error.starts_with("timed out after 0 s in Booting"),
        "{error}"
    );
}

#[test]
fn a_run_that_ends_before_its_scenario_says_so_in_the_report() {
    let tmp = tempfile::tempdir().unwrap();
    with_app(tmp.path(), |_, args, recorder| {
        recorder.reset();
        let mut driver = new_driver(
            args,
            Some(PerfScenario::TypeLatency),
            None,
            None,
            Limits::default(),
        );
        driver.finish();
    });
    assert_eq!(
        report_error(tmp.path()),
        "the application exited before the scenario finished"
    );
}

#[test]
fn a_report_that_cannot_be_written_fails_the_scenario() {
    let tmp = tempfile::tempdir().unwrap();
    // A folder where the report file should go.
    let blocked = report_path(tmp.path());
    std::fs::create_dir_all(&blocked).unwrap();
    with_app(tmp.path(), |app, args, recorder| {
        recorder.reset();
        let mut ended = new_driver(
            args,
            Some(PerfScenario::Startup),
            None,
            None,
            Limits::default(),
        );
        present(recorder);
        assert_eq!(ended.tick(app), Tick::Finished(EXIT_PERF_FAILED));
        // The end of a plain run reports the same failure and goes on.
        new_driver(args, None, None, None, Limits::default()).finish();
    });
    assert!(blocked.is_dir(), "nothing replaced the folder");
}

#[test]
fn an_opened_document_must_be_painted_and_keep_its_editor_and_window() {
    let tmp = tempfile::tempdir().unwrap();
    let file = long_document(tmp.path(), 40);
    with_app(tmp.path(), |app, args, recorder| {
        let cell = app.controller().active_window().unwrap();
        let open = |app: &UiApp| {
            recorder.reset();
            let mut driver = new_driver(
                args,
                Some(PerfScenario::Open60k),
                Some(file.as_path()),
                None,
                Limits::default(),
            );
            until_mark(app, &mut driver, recorder, DOC_OPEN_RETURNED);
            driver
        };

        // The frame after the open never drew the document.
        let mut driver = open(app);
        assert_eq!(
            driver.tick(app),
            Tick::Running,
            "the frame is still to come"
        );
        present(recorder);
        assert_eq!(driver.tick(app), Tick::Finished(EXIT_PERF_FAILED));
        assert_eq!(
            report_error(tmp.path()),
            "the opened document was not painted"
        );

        // The tab was closed meanwhile.
        let mut driver = open(app);
        close_tabs(&cell);
        present(recorder);
        assert_eq!(driver.tick(app), Tick::Finished(EXIT_PERF_FAILED));
        assert_eq!(report_error(tmp.path()), "the window has no editor");

        // The window was closed meanwhile.
        let mut driver = open(app);
        app.controller().close_window(cell.id);
        present(recorder);
        assert_eq!(driver.tick(app), Tick::Finished(EXIT_PERF_FAILED));
        assert_eq!(report_error(tmp.path()), "the application has no window");
    });
}

#[test]
fn a_scroll_sweep_that_cannot_finish_fails_the_scenario() {
    let tmp = tempfile::tempdir().unwrap();
    // Far more than a viewport, scrolled by a few DIPs per frame.
    let file = long_document(tmp.path(), 600);
    with_app(tmp.path(), |app, args, recorder| {
        let cell = app.controller().active_window().unwrap();
        let sweep = |limits: Limits| {
            recorder.reset();
            new_driver(
                args,
                Some(PerfScenario::Open60k),
                Some(file.as_path()),
                Some(5),
                limits,
            )
        };

        // The document does not end within the frames the limit allows.
        let limits = Limits {
            scroll_frames: 2,
            ..Limits::default()
        };
        let mut driver = sweep(limits);
        assert_eq!(paint_until_finished(app, &mut driver), EXIT_PERF_FAILED);
        let error = report_error(tmp.path());
        assert!(
            error.starts_with("the document did not end within 2 frames (offset "),
            "{error}"
        );

        // The tab was closed while the sweep waited for a frame.
        let mut driver = sweep(Limits::default());
        paint_until_mark(app, &mut driver, recorder, SCROLL_BEGIN);
        close_tabs(&cell);
        present(recorder);
        assert_eq!(driver.tick(app), Tick::Finished(EXIT_PERF_FAILED));
        assert_eq!(report_error(tmp.path()), "the window has no editor");

        // The window was closed while the sweep waited for a frame.
        let mut driver = sweep(Limits::default());
        paint_until_mark(app, &mut driver, recorder, SCROLL_BEGIN);
        app.controller().close_window(cell.id);
        present(recorder);
        assert_eq!(driver.tick(app), Tick::Finished(EXIT_PERF_FAILED));
        assert_eq!(report_error(tmp.path()), "the application has no window");
    });
}

#[test]
fn a_folder_scenario_fails_without_a_target_or_without_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    with_app(tmp.path(), |app, args, recorder| {
        recorder.reset();
        let mut driver = new_driver(
            args,
            Some(PerfScenario::Folder10k),
            None,
            None,
            Limits::default(),
        );
        assert_eq!(finish(app, &mut driver, recorder, true), EXIT_PERF_FAILED);
        assert_eq!(report_error(tmp.path()), "the scenario needs --perf-target");

        recorder.reset();
        let mut driver = new_driver(
            args,
            Some(PerfScenario::Folder10k),
            Some(empty.as_path()),
            None,
            Limits::default(),
        );
        assert_eq!(finish(app, &mut driver, recorder, true), EXIT_PERF_FAILED);
        assert_eq!(report_error(tmp.path()), "the sidebar shows no rows");
    });
}

#[test]
fn a_typing_run_fails_without_a_frame_a_window_handle_or_a_window() {
    let tmp = tempfile::tempdir().unwrap();
    with_app(tmp.path(), |app, args, recorder| {
        let cell = app.controller().active_window().unwrap();
        let handle = cell.hwnd.get();
        assert!(handle.is_some(), "the window of the app has its handle");
        let typing = |limits: Limits| {
            recorder.reset();
            new_driver(args, Some(PerfScenario::TypeLatency), None, None, limits)
        };

        // The posted character is never answered by a frame.
        let limits = Limits {
            type_timeout: Duration::ZERO,
            ..Limits::default()
        };
        let mut driver = typing(limits);
        assert_eq!(finish(app, &mut driver, recorder, true), EXIT_PERF_FAILED);
        assert_eq!(
            report_error(tmp.path()),
            "no frame followed typed character 1"
        );

        // The window lost its handle.
        let mut driver = typing(Limits::default());
        until_mark(app, &mut driver, recorder, TYPE_BEGIN);
        cell.hwnd.set(None);
        assert_eq!(finish(app, &mut driver, recorder, true), EXIT_PERF_FAILED);
        assert_eq!(report_error(tmp.path()), "the window has no handle");
        cell.hwnd.set(handle);

        // The window is gone.
        let mut driver = typing(Limits::default());
        until_mark(app, &mut driver, recorder, TYPE_BEGIN);
        app.controller().close_window(cell.id);
        assert_eq!(finish(app, &mut driver, recorder, true), EXIT_PERF_FAILED);
        assert_eq!(report_error(tmp.path()), "the application has no window");
    });
}

#[test]
fn a_character_the_full_message_queue_refuses_fails_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    with_app(tmp.path(), |app, args, recorder| {
        let cell = app.controller().active_window().unwrap();
        let hwnd = cell.hwnd.get().unwrap();
        recorder.reset();
        let mut driver = new_driver(
            args,
            Some(PerfScenario::TypeLatency),
            None,
            None,
            Limits::default(),
        );
        until_mark(app, &mut driver, recorder, TYPE_BEGIN);
        // The pause between two characters passes, then the queue of our own window (the
        // thread's queue holds a limited number of posted messages) is filled up to that limit.
        std::thread::sleep(Duration::from_millis(60));
        let mut queued = 0_u32;
        while post_message(hwnd, WM_NULL, 0, 0) {
            queued += 1;
            assert!(queued < 1_000_000, "the message queue never filled up");
        }
        assert_eq!(driver.tick(app), Tick::Finished(EXIT_PERF_FAILED));
        // Dispatching the filler makes room again.
        dispatch_queued();
        assert!(
            post_message(hwnd, WM_NULL, 0, 0),
            "the queue has room again"
        );
        dispatch_queued();
    });
    assert_eq!(report_error(tmp.path()), "character 0 could not be posted");
}

#[test]
fn a_flick_sweep_whose_tab_was_closed_on_the_way_back_to_the_top_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let file = long_document(tmp.path(), 40);
    with_app(tmp.path(), |app, args, recorder| {
        let cell = app.controller().active_window().unwrap();
        recorder.reset();
        let mut driver = new_driver(
            args,
            Some(PerfScenario::Open60k),
            Some(file.as_path()),
            None,
            Limits::default(),
        );
        // The stress sweep reached the end and sent the editor back to the top ...
        paint_until_mark(app, &mut driver, recorder, SCROLL_END);
        // ... but the tab is gone when that frame has been drawn: the flick sweep starts with
        // no editor to move, and its first frame finds that out.
        close_tabs(&cell);
        present(recorder);
        assert_eq!(driver.tick(app), Tick::Running);
        present(recorder);
        assert_eq!(driver.tick(app), Tick::Finished(EXIT_PERF_FAILED));
        assert_eq!(report_error(tmp.path()), "the window has no editor");
    });
}

#[test]
fn a_scenario_without_a_window_fails_at_its_first_step() {
    let tmp = tempfile::tempdir().unwrap();
    let file = long_document(tmp.path(), 5);
    let folder = tmp.path().join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    with_app(tmp.path(), |app, args, recorder| {
        let id = app.controller().active_window().unwrap().id;
        app.controller().close_window(id);
        assert!(app.controller().active_window().is_none());
        let scenarios = [
            (PerfScenario::Open60k, Some(file.as_path())),
            (PerfScenario::Folder10k, Some(folder.as_path())),
            (PerfScenario::TypeLatency, None),
        ];
        for (scenario, target) in scenarios {
            recorder.reset();
            let mut driver = new_driver(args, Some(scenario), target, None, Limits::default());
            assert_eq!(finish(app, &mut driver, recorder, true), EXIT_PERF_FAILED);
            assert_eq!(
                report_error(tmp.path()),
                "the application has no window",
                "{scenario:?}"
            );
        }
    });
}
