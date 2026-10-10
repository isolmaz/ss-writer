//! The graphics devices arrive after the windows opened (`UiApp::start` with a deferred
//! [`DeviceStart`]): the window is made and "shown" (hidden here: nothing appears on a screen,
//! nothing is activated) without a swap chain, and `UiApp::pump` gives it one when the device
//! worker is done. A worker that is held back by the test (or made to fail) makes every order
//! of events deterministic.
//!
//! The recorder of the performance marks is process-wide: every test that reads marks holds
//! [`RECORDER_LOCK`] for its whole run.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, EXIT_NO_DEVICE, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::Request;
use ssw_app::ui::export::{PrintJob, Printing, SystemPrinting};
use ssw_app::ui::host::{DeviceStart, UiApp, UiOptions, UiServices};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_ui::geometry::Size;
use ssw_ui::services::{MemoryClipboard, ServiceError};
use ssw_win::com::{ComGuard, WinError};
use ssw_win::dragdrop::OleGuard;
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::{DeviceStartup, Driver, GfxDevices, SharedDevices};
use ssw_win::menus::{PopupHost, PopupRequest};
use ssw_win::wic::Wic;
use ssw_win::window::{WM_WAKE, current_os_version, send_message};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, HMENU, MSG, PM_REMOVE, PeekMessageW, WM_PAINT,
};

/// Held for the whole of one test that reads marks: the marks belong to one run at a time.
static RECORDER_LOCK: Mutex<()> = Mutex::new(());

fn recorder_lock() -> MutexGuard<'static, ()> {
    // A failed test poisons the lock; the next one starts from a reset recorder anyway.
    RECORDER_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

struct NoPopups;

impl PopupHost for NoPopups {
    fn track(&mut self, _menu: HMENU, _request: &PopupRequest) -> Option<u32> {
        None
    }
}

/// Hands the core of a real startup to `body`.
struct Probe<F> {
    body: Option<F>,
}

impl<F: FnOnce(AppCore)> ShellHost for Probe<F> {
    fn run(&mut self, core: AppCore, _args: &Args) -> i32 {
        let _ole = OleGuard::initialize().unwrap();
        (self.body.take().unwrap())(core);
        0
    }
}

/// Runs `body` over the core of a real startup (WARP devices, a fresh profile).
fn with_core(body: impl FnOnce(AppCore)) {
    let tmp = tempfile::tempdir().unwrap();
    let mut args = Args::default();
    args.test.hidden = true;
    args.test.profile = Some(tmp.path().join("profile"));
    let process = ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    };
    let _run = recorder_lock();
    ssw_win::perf::enable().reset();
    let mut dialogs = ScriptedServices::new().dialogs();
    let exit = run(
        &args,
        process,
        &mut *dialogs,
        None,
        &mut Probe { body: Some(body) },
    );
    assert_eq!(exit, 0);
}

/// The core with its own device creation finished and the recorder cleared of it: a test that
/// brings its own devices reads marks that only its devices and windows made.
fn quiet(mut core: AppCore) -> AppCore {
    core.wait_devices().unwrap();
    ssw_win::perf::enable().reset();
    core
}

/// A hidden window over scripted services, with the devices taken as `start` says.
fn app_over(core: AppCore, start: DeviceStart) -> UiApp {
    let services = ScriptedServices::new();
    let options = UiOptions {
        hidden: true,
        services: Some(UiServices {
            dialogs: services.dialogs(),
            shell: services.shell(),
            clipboard: Rc::new(RefCell::new(MemoryClipboard::default())),
            popups: Box::new(NoPopups),
        }),
        ..UiOptions::default()
    };
    UiApp::start(core, options, start).unwrap()
}

/// A device creation whose worker waits for the returned sender, then runs `create`.
fn held(create: fn() -> Result<GfxDevices, WinError>) -> (DeviceStartup, Sender<()>) {
    let (release, released) = channel::<()>();
    let startup = DeviceStartup::spawn_with_factory(move || {
        let _ = released.recv();
        create()
    });
    (startup, release)
}

/// Devices (WARP) that arrive when the sender is used.
fn held_back() -> (DeviceStartup, Sender<()>) {
    held(|| GfxDevices::create_on(Driver::Warp, 1))
}

/// Devices that cannot be made once the sender is used.
fn held_back_failing() -> (DeviceStartup, Sender<()>) {
    held(|| Err(WinError::Invalid("no adapter")))
}

/// When the mark `name` was placed, in ms since the process was created.
fn mark(name: &str) -> Option<f64> {
    ssw_win::perf::enable()
        .snapshot()
        .marks
        .iter()
        .find(|(placed, _)| *placed == name)
        .map(|(_, ms)| *ms)
}

fn assert_in_order(names: &[&str]) {
    for pair in names.windows(2) {
        let (first, second) = (mark(pair[0]), mark(pair[1]));
        assert!(
            matches!((first, second), (Some(first), Some(second)) if first <= second),
            "{} ({first:?}) must not come after {} ({second:?})",
            pair[0],
            pair[1]
        );
    }
}

/// Pumps `app` until `done` holds.
fn pump_until(app: &UiApp, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        app.pump();
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Takes one wake-up of the loop out of the queue of `message`, if there is one.
fn woken(message: HWND) -> bool {
    let mut queued = MSG::default();
    // SAFETY: removes a posted message of this thread's own message window with a valid MSG.
    unsafe { PeekMessageW(&mut queued, Some(message), WM_WAKE, WM_WAKE, PM_REMOVE) }.as_bool()
}

#[test]
fn the_window_opens_before_the_devices_and_gets_its_swap_chain_when_they_arrive() {
    with_core(|core| {
        let recorder = ssw_win::perf::enable();
        let (startup, release) = held_back();
        let app = app_over(quiet(core), DeviceStart::DeferOver(startup));
        app.pump();
        let id = app.controller().windows()[0].id;
        let hwnd = app.hwnd(id).expect("the window exists without the devices");
        assert!(mark("window_shown").is_some());
        assert!(mark("devices_received").is_none());
        assert!(mark("graphics_attached").is_none());
        // Without a swap chain nothing can be drawn: a paint request leaves no frame.
        send_message(hwnd, WM_PAINT, 0, 0);
        assert_eq!(recorder.frame_count(), 0);
        assert!(mark("first_present").is_none());

        release.send(()).unwrap();
        pump_until(&app, "the swap chain", || {
            mark("graphics_attached").is_some()
        });
        send_message(hwnd, WM_PAINT, 0, 0);
        assert!(recorder.frame_count() >= 1);
        // The window was up before the worker had the devices; the frame came last.
        assert_in_order(&[
            "window_created",
            "backdrop_applied",
            "window_shown",
            "d3d_device_ready",
            "devices_received",
            "graphics_attached",
            "first_present",
        ]);
    });
}

#[test]
fn the_process_startup_devices_arrive_in_a_deferred_start() {
    with_core(|core| {
        let recorder = ssw_win::perf::enable();
        let app = app_over(core, DeviceStart::Defer);
        app.pump();
        let id = app.controller().windows()[0].id;
        let hwnd = app.hwnd(id).unwrap();
        pump_until(&app, "the swap chain", || {
            mark("graphics_attached").is_some()
        });
        send_message(hwnd, WM_PAINT, 0, 0);
        assert!(recorder.frame_count() >= 1);
        // Whenever the worker is done, the window is made first.
        assert_in_order(&[
            "window_created",
            "window_shown",
            "devices_received",
            "graphics_attached",
            "first_present",
        ]);
        // A window made once the devices are here has its swap chain at once.
        let frames = recorder.frame_count();
        let later = app
            .controller()
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        app.pump();
        send_message(app.hwnd(later.id).unwrap(), WM_PAINT, 0, 0);
        assert!(
            recorder.frame_count() > frames,
            "the later window drew a frame"
        );
    });
}

#[test]
fn the_worker_wakes_the_loop_when_the_devices_are_ready() {
    with_core(|core| {
        let (startup, release) = held_back();
        let app = app_over(quiet(core), DeviceStart::DeferOver(startup));
        app.pump();
        let message = app.message_hwnd();
        // Wake-ups of other origins (the connection of the waker) are out of the way.
        while woken(message) {}
        release.send(()).unwrap();
        pump_until(&app, "the swap chain", || {
            mark("graphics_attached").is_some()
        });
        assert!(woken(message), "the worker posted a wake-up for the loop");
    });
}

#[test]
fn devices_that_cannot_be_made_end_the_application_with_the_no_device_code() {
    with_core(|core| {
        let (startup, release) = held_back_failing();
        let app = app_over(quiet(core), DeviceStart::DeferOver(startup));
        app.pump();
        assert_eq!(
            app.exit_code(),
            None,
            "nothing is wrong while the worker works"
        );
        release.send(()).unwrap();
        pump_until(&app, "the failure", || app.exit_code().is_some());
        assert_eq!(app.exit_code(), Some(EXIT_NO_DEVICE));
        // Windows stay as they are and a later one opens without a swap chain; the failure is
        // not reported again.
        let late = app
            .controller()
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        app.pump();
        assert!(app.hwnd(late.id).is_some());
        assert_eq!(app.exit_code(), Some(EXIT_NO_DEVICE));
        assert!(mark("devices_received").is_none());
        assert!(mark("graphics_attached").is_none());
    });
}

#[test]
fn a_window_that_cannot_take_the_arriving_devices_is_closed() {
    with_core(|core| {
        let (startup, release) = held_back();
        let app = app_over(quiet(core), DeviceStart::DeferOver(startup));
        app.pump();
        let id = app.controller().windows()[0].id;
        let hwnd = app.hwnd(id).unwrap();
        // The window is destroyed behind the application's back: there is nothing to give a
        // swap chain to.
        // SAFETY: destroys this thread's own window, which is never shown.
        unsafe { DestroyWindow(hwnd) }.unwrap();
        release.send(()).unwrap();
        pump_until(&app, "the arrival", || mark("devices_received").is_some());
        app.pump();
        assert!(app.controller().windows().is_empty());
        assert!(app.hwnd(id).is_none());
        assert_eq!(app.exit_code(), None, "the devices themselves are fine");
    });
}

#[test]
fn the_settings_window_waits_for_devices_that_are_still_being_made() {
    with_core(|core| {
        let (startup, release) = held_back();
        let app = app_over(quiet(core), DeviceStart::DeferOver(startup));
        app.pump();
        assert!(mark("devices_received").is_none());
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            release.send(()).unwrap();
        });
        app.controller().push(Request::OpenSettings);
        // Opening blocks until the worker has the devices; the main window takes them in the
        // same turn.
        app.pump();
        releaser.join().unwrap();
        assert!(app.settings_hwnd().is_some());
        assert!(mark("devices_received").is_some());
        assert!(mark("graphics_attached").is_some());
    });
}

#[test]
fn the_settings_window_does_not_open_without_devices() {
    with_core(|core| {
        let (startup, release) = held_back_failing();
        let app = app_over(quiet(core), DeviceStart::DeferOver(startup));
        app.pump();
        release.send(()).unwrap();
        app.controller().push(Request::OpenSettings);
        app.pump();
        assert!(app.settings_hwnd().is_none());
        assert!(app.controller().settings_cell().is_none());
        assert_eq!(app.exit_code(), Some(EXIT_NO_DEVICE));
    });
}

#[test]
fn a_start_over_devices_that_were_taken_already_is_refused() {
    with_core(|mut core| {
        let _taken = core.take_devices().unwrap();
        let error = UiApp::new(core, UiOptions::default()).unwrap_err();
        assert_eq!(error.to_string(), "the device was already taken");
    });
}

#[test]
fn printing_without_devices_fails_before_any_dialog() {
    let _run = recorder_lock();
    let _com = ComGuard::apartment_threaded().unwrap();
    let devices = SharedDevices::new(DeviceStartup::spawn_with_factory(|| {
        Err(WinError::Invalid("no adapter"))
    }));
    let mut printing =
        SystemPrinting::new(devices, Wic::new().unwrap(), DWriteEngine::new().unwrap());
    let result = printing.print(PrintJob {
        owner: None,
        name: "never".to_owned(),
        pages: Vec::new(),
    });
    match result {
        Err(ServiceError::Failed { service, message }) => {
            assert_eq!(service, "print");
            assert_eq!(message, "no adapter");
        }
        other => panic!("expected a failed print, got {other:?}"),
    }
}
