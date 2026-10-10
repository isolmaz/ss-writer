//! The performance scenarios (`--perf-scenario`) driven against a real hidden window: the driver
//! is ticked by the test, frames are produced by sending `WM_PAINT` to our own window, typed
//! characters are posted to its queue. Nothing is shown, activated or injected.
//!
//! The recorder is process-wide: every scenario run holds [`RECORDER_LOCK`] and resets the
//! recorder first, so the tests pass both under nextest (one process per test) and under
//! `cargo test` (threads of one process).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::{Args, PerfScenario};
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::host::{UiApp, UiOptions, UiServices, UiShell, WinInstance};
use ssw_app::ui::perf_driver::{EXIT_PERF_FAILED, PerfDriver, TYPED_CHARS, Tick};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_ui::services::MemoryClipboard;
use ssw_win::dragdrop::OleGuard;
use ssw_win::gfx::Driver;
use ssw_win::menus::{PopupHost, PopupRequest};
use ssw_win::window::{current_os_version, send_message};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, HMENU, MSG, PM_REMOVE, PeekMessageW, TranslateMessage, WM_PAINT,
};

/// Held for the whole of one scenario run: the recorder and its marks belong to one run at a
/// time.
static RECORDER_LOCK: Mutex<()> = Mutex::new(());

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

/// Dispatches the messages queued for this thread's windows (posted characters).
fn dispatch_queued() {
    let mut message = MSG::default();
    // SAFETY: peeks and dispatches this thread's own queue with a valid MSG buffer.
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        // SAFETY: translates and dispatches the message this thread just retrieved.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Ticks `driver` like the message loop would, producing a frame per turn. The loop's idle
/// handler runs more often than frames are produced: every turn ticks once before its frame
/// (the scenario waits for the frame it asked for) and once after it.
fn drive(app: &UiApp, driver: &mut PerfDriver) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Tick::Finished(code) = driver.tick(app) {
            return code;
        }
        dispatch_queued();
        app.pump();
        if let Some(cell) = app.controller().active_window()
            && let Some(hwnd) = app.hwnd(cell.id)
        {
            send_message(hwnd, WM_PAINT, 0, 0);
        }
        if let Tick::Finished(code) = driver.tick(app) {
            return code;
        }
        assert!(Instant::now() < deadline, "the scenario never ended");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Runs one scenario (`None`: a plain `--perf-report` run, ended by `finish`) over a fresh
/// profile in `tmp`; returns the exit code of the driver and the report it wrote.
fn run_scenario(
    tmp: &Path,
    scenario: Option<PerfScenario>,
    target: Option<PathBuf>,
    step: Option<u32>,
) -> (Option<i32>, Value) {
    let report = tmp.join("out").join("report.json");
    let mut args = Args::default();
    args.test.hidden = true;
    args.test.profile = Some(tmp.join("profile"));
    args.test.perf_report = Some(report.clone());
    args.test.perf_scenario = scenario;
    args.test.perf_target = target;
    args.test.perf_scroll_step = step;
    let process = ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Hardware,
        install_crash_handler: false,
    };
    // A failed test poisons the lock; the next one starts from a reset recorder anyway.
    let _run = RECORDER_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    // Like `main`: the recorder runs from the first moment, so the startup marks exist.
    ssw_win::perf::enable();
    ssw_win::perf::reset();
    ssw_win::perf::mark(ssw_app::perf::MAIN_ENTERED);
    let code = Rc::new(RefCell::new(None));
    let seen = Rc::clone(&code);
    let mut dialogs = ScriptedServices::new().dialogs();
    let exit = run(
        &args,
        process,
        &mut *dialogs,
        None,
        &mut Probe {
            body: Some(move |app: &UiApp, args: &Args| {
                let mut driver = PerfDriver::new(args).unwrap();
                *seen.borrow_mut() = scenario.map(|_| drive(app, &mut driver));
                driver.finish();
            }),
        },
    );
    assert_eq!(exit, 0);
    let text = std::fs::read_to_string(&report).unwrap();
    let code = *code.borrow();
    (code, serde_json::from_str(&text).unwrap())
}

fn mark(report: &Value, name: &str) -> f64 {
    report["marks"][name]
        .as_f64()
        .unwrap_or_else(|| panic!("mark {name} missing in {}", report["marks"]))
}

fn assert_in_order(report: &Value, names: &[&str]) {
    for pair in names.windows(2) {
        assert!(
            mark(report, pair[0]) <= mark(report, pair[1]),
            "{} must not come after {}",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn the_shell_drives_a_scenario_from_its_own_loop_and_exits_with_its_code() {
    let tmp = tempfile::tempdir().unwrap();
    let report = tmp.path().join("out").join("report.json");
    let mut args = Args::default();
    // A hidden window is never painted by the system: the window is shown, off-screen and
    // without activation, on a desktop of its own.
    args.test.perf_offscreen = true;
    args.test.profile = Some(tmp.path().join("profile"));
    args.test.perf_report = Some(report.clone());
    args.test.perf_scenario = Some(PerfScenario::Startup);
    let process = ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Hardware,
        install_crash_handler: false,
    };
    let _run = RECORDER_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let desktop = ssw_win::testing::PrivateDesktop::create().unwrap();
    let exit = desktop.run(move || {
        ssw_win::perf::enable();
        ssw_win::perf::reset();
        ssw_win::perf::mark(ssw_app::perf::MAIN_ENTERED);
        // An application id of its own: the user's editor is never involved.
        let id = format!("SSWriter.Test.perfshell.{}", std::process::id());
        let mut shell = UiShell::new(Some(WinInstance::with_app_id(id).link()));
        let mut dialogs = ScriptedServices::new().dialogs();
        run(&args, process, &mut *dialogs, None, &mut shell)
    });
    assert_eq!(exit, 0);
    let report: Value = serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(report["scenario"], "startup");
    assert_eq!(report["completed"], true);
}

#[test]
fn startup_records_the_window_and_first_frame_marks() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, report) = run_scenario(tmp.path(), Some(PerfScenario::Startup), None, None);
    assert_eq!(code, Some(0));
    assert_eq!(report["scenario"], "startup");
    assert_eq!(report["completed"], true);
    assert_in_order(
        &report,
        &[
            "main_entered",
            "startup_done",
            "devices_received",
            "window_created",
            "backdrop_applied",
            "graphics_attached",
            "window_shown",
            "first_present",
        ],
    );
    assert!(mark(&report, "first_content_present") >= mark(&report, "first_content_painted"));
    assert!(mark(&report, "d3d_device_ready") <= mark(&report, "devices_received"));
    assert!(
        !report["frames"]["presented_ms"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(report["refresh_hz"].as_f64().unwrap() > 0.0);
}

#[test]
fn opening_a_document_is_timed_step_by_step_and_scrolled_to_its_end() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("long.md");
    let text: String = (0..1500)
        .map(|n| format!("Paragraph {n} of the text.\n\n"))
        .collect();
    std::fs::write(&file, &text).unwrap();
    let (code, report) = run_scenario(
        tmp.path(),
        Some(PerfScenario::Open60k),
        Some(file),
        Some(400),
    );
    assert_eq!(code, Some(0), "{}", report["error"]);
    assert_in_order(
        &report,
        &[
            "doc_open_begin",
            "doc_read",
            "doc_decoded",
            "doc_parsed",
            "doc_open_returned",
            "doc_painted",
            "doc_presented",
            "scroll_begin",
            "scroll_end",
            "flick_begin",
            "flick_end",
        ],
    );
    let details = &report["details"];
    assert_eq!(details["doc_bytes"].as_f64().unwrap(), text.len() as f64);
    assert_eq!(details["scroll_step_dip"], 400.0);
    assert!(details["laid_out_items"].as_f64().unwrap() > 0.0);
    let frames = details["scroll_frames"].as_f64().unwrap();
    assert!(frames >= 3.0, "{frames}");
    let phases = report["frames"]["phase"].as_array().unwrap();
    let in_phase = |phase: u64| phases.iter().filter(|p| p.as_u64() == Some(phase)).count();
    let scrolled = in_phase(2);
    assert!(
        scrolled as f64 >= frames,
        "{scrolled} frames of {frames} requests"
    );
    // The flick sweep starts from the top after one cold frame of its own, with a small step.
    assert_eq!(details["flick_step_dip"], 48.0);
    let flicks = details["flick_frames"].as_f64().unwrap();
    assert!(flicks >= 1.0, "{flicks}");
    assert!(in_phase(4) as f64 >= flicks, "{} flicks", in_phase(4));
    assert!(in_phase(5) >= 1, "the frame that went back to the top");
    assert!(
        details["flick_frames"].as_f64().unwrap() > details["scroll_frames"].as_f64().unwrap(),
        "a small step needs more frames than a large one for the same distance"
    );
}

#[test]
fn opening_a_folder_times_the_tree_and_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    for folder in ["a", "b"] {
        std::fs::create_dir_all(tree.join(folder)).unwrap();
        for n in 0..5 {
            std::fs::write(
                tree.join(folder).join(format!("n{n}.md")),
                format!("# N{n}\n\n[[n{}]]\n", n + 1),
            )
            .unwrap();
        }
    }
    let (code, report) = run_scenario(tmp.path(), Some(PerfScenario::Folder10k), Some(tree), None);
    assert_eq!(code, Some(0), "{}", report["error"]);
    assert_in_order(
        &report,
        &[
            "folder_open_begin",
            "folder_open_returned",
            "index_requested",
            "index_started",
            "index_finished",
        ],
    );
    // The tree is drawn after it was opened, whatever the index worker does meanwhile.
    assert_in_order(
        &report,
        &["folder_open_returned", "tree_painted", "tree_presented"],
    );
    assert!(report["details"]["tree_rows"].as_f64().unwrap() >= 2.0);
}

#[test]
fn typing_gives_every_character_its_own_latency_sample() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, report) = run_scenario(tmp.path(), Some(PerfScenario::TypeLatency), None, None);
    assert_eq!(code, Some(0), "{}", report["error"]);
    assert_in_order(&report, &["type_begin", "type_end"]);
    let samples = report["input_latency_ms"].as_array().unwrap();
    assert_eq!(samples.len(), TYPED_CHARS as usize);
    assert!(samples.iter().all(|ms| ms.as_f64().unwrap() >= 0.0));
    assert_eq!(report["details"]["typed_chars"], f64::from(TYPED_CHARS));
}

#[test]
fn a_scenario_that_cannot_run_fails_with_the_reason_in_its_report() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nothing.md");
    let (code, report) = run_scenario(tmp.path(), Some(PerfScenario::Open60k), Some(missing), None);
    assert_eq!(code, Some(EXIT_PERF_FAILED));
    assert_eq!(report["completed"], false);
    assert!(
        report["error"].as_str().unwrap().contains("cannot open"),
        "{report}"
    );
}

#[test]
fn a_folder_scenario_needs_a_folder_and_a_target() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("plain.md");
    std::fs::write(&file, "x").unwrap();
    let (code, report) = run_scenario(tmp.path(), Some(PerfScenario::Folder10k), Some(file), None);
    assert_eq!(code, Some(EXIT_PERF_FAILED));
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("is not a folder")
    );
}

#[test]
fn a_scenario_without_its_target_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, report) = run_scenario(tmp.path(), Some(PerfScenario::Open60k), None, None);
    assert_eq!(code, Some(EXIT_PERF_FAILED));
    assert!(report["error"].as_str().unwrap().contains("--perf-target"));
}

#[test]
fn a_plain_report_run_writes_a_manual_report_when_the_app_ends() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, report) = run_scenario(tmp.path(), None, None, None);
    assert_eq!(code, None);
    assert_eq!(report["scenario"], "manual");
    assert_eq!(report["completed"], true);
    assert!(report["error"].is_null());
}
