//! The loop owner ([`UiApp`], [`UiShell`], the single-instance hook) at its edges: requests about
//! windows that are gone, a menu that chooses nothing, a quit a window refuses, the Settings
//! window opened without an owner and destroyed by the system, a Jump List that is already up to
//! date, devices that cannot be made, a process id the shell rejects, and (with the
//! `fault-injection` feature) every system call of making a window, of starting the application
//! and of the single-instance handshake failing in turn. Windows are hidden and never shown,
//! raised or activated; the Jump List test uses an application id of its own and deletes it.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::startup::{ProcessEnv, boot};
use ssw_app::ui::controller::Request;
use ssw_app::ui::host::{UiApp, UiOptions, UiServices, UiShell, WinInstance};
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices, ShellRecord};
use ssw_app::ui::shell_view::ShellAction;
use ssw_app::workers::{DeferredWaker, JobKey, JobKind, JobSpec, WindowId};
use ssw_core::command::CommandId;
use ssw_core::edit::{Edit, Selection, Transaction};
use ssw_ui::geometry::Point;
use ssw_ui::services::{MemoryClipboard, MessageBoxResult};
use ssw_ui::shell::settings::SettingsEvent;
use ssw_ui::shell::titlebar::{MenuKind, TitleEvent};
use ssw_ui::theme::{DEFAULT_ACCENT, Theme, ThemeMode};
use ssw_win::com::WinError;
use ssw_win::dragdrop::OleGuard;
use ssw_win::gfx::{DeviceStartup, Driver};
use ssw_win::jumplist::JumpList;
use ssw_win::menus::{MenuModel, PopupHost, PopupRequest};
use ssw_win::taskbar::Progress;
use ssw_win::theme::SystemTheme;
use ssw_win::window::current_os_version;
use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, HMENU};

/// A popup host that "chooses" `choice` at once.
struct Popups(Option<u32>);

impl PopupHost for Popups {
    fn track(&mut self, _menu: HMENU, _request: &PopupRequest) -> Option<u32> {
        self.0
    }
}

struct Body<F>(Option<F>);

impl<F: FnOnce(AppCore, &Args) -> i32> ShellHost for Body<F> {
    fn run(&mut self, core: AppCore, args: &Args) -> i32 {
        (self.0.take().expect("the body runs once"))(core, args)
    }
}

fn test_args(root: &Path, files: &[PathBuf]) -> Args {
    let mut args = Args {
        paths: files.to_vec(),
        ..Args::default()
    };
    args.test.hidden = true;
    args.test.profile = Some(root.join("profile"));
    args
}

fn process(args: &Args) -> ProcessEnv {
    ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    }
}

/// Starts the application over a temporary profile (autosave off: edits stay unsaved) with
/// `files` as its launch and hands the core to `body`, which stands in for the shell.
fn start(files: &[PathBuf], tmp: &Path, body: impl FnOnce(AppCore, &Args) -> i32) -> i32 {
    start_with(r#"{"general":{"autosave":false}}"#, files, tmp, body)
}

/// [`start`] with `settings` as the `settings.json` of the profile.
fn start_with(
    settings: &str,
    files: &[PathBuf],
    tmp: &Path,
    body: impl FnOnce(AppCore, &Args) -> i32,
) -> i32 {
    let args = test_args(tmp, files);
    let roaming = tmp.join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&roaming).unwrap();
    std::fs::write(roaming.join("settings.json"), settings).unwrap();
    let mut dialogs = ScriptedServices::new().dialogs();
    run(
        &args,
        process(&args),
        &mut *dialogs,
        None,
        &mut Body(Some(body)),
    )
}

fn options(services: &ScriptedServices, choice: Option<u32>) -> UiOptions {
    UiOptions {
        hidden: true,
        services: Some(UiServices {
            dialogs: services.dialogs(),
            shell: services.shell(),
            clipboard: Rc::new(RefCell::new(MemoryClipboard::default())),
            popups: Box::new(Popups(choice)),
        }),
        instance: None,
        jump_list: false,
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

fn menu_request(menu: MenuKind) -> ShellAction {
    ShellAction::Title(TitleEvent::MenuRequested {
        menu,
        anchor: Point::new(10.0, 10.0),
    })
}

#[test]
fn requests_about_vanished_windows_and_menus_that_choose_nothing_change_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "text");
    let services = ScriptedServices::new();
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        // The popup host "chooses" an id that is no command.
        let app = UiApp::new(core, options(&services, Some(u32::MAX))).unwrap();
        app.pump();
        let first = app.controller().windows()[0].id;
        let zoom = app.controller().settings().appearance.zoom_percent;

        // A window that is gone has no taskbar button and no menu to show.
        let gone = WindowId(999);
        app.controller()
            .push(Request::TaskbarProgress(gone, Progress::Indeterminate));
        app.controller()
            .push(Request::Action(gone, menu_request(MenuKind::File)));
        app.pump();
        assert_eq!(app.controller().windows().len(), 1);
        assert_eq!(
            app.show_popup(gone, &MenuModel::default(), Point::new(0.0, 0.0)),
            None
        );

        // The menu of a window that exists is shown; its choice names no command: nothing runs.
        app.controller()
            .push(Request::Action(first, menu_request(MenuKind::File)));
        app.pump();
        assert_eq!(app.controller().windows().len(), 1);
        assert_eq!(app.controller().settings().appearance.zoom_percent, zoom);
        assert_eq!(app.exit_code(), None);
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn a_quit_a_window_refuses_leaves_the_loop_running() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "text\n");
    let services = ScriptedServices::new();
    let code = start(std::slice::from_ref(&file), tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options(&services, None)).unwrap();
        app.pump();
        assert!(app.idle().is_some(), "the loop sleeps and runs on");

        // An edit nobody saved: the window asks, and the user cancels.
        let entry = app.controller().docs().find(&file).unwrap();
        {
            let mut document = entry.document().borrow_mut();
            let end = document.buffer().len();
            let edit = Transaction::single(
                Edit::insert(end, "more"),
                Selection::caret(end),
                Selection::caret(end + 4),
            );
            document.apply(edit).unwrap();
        }
        services.answer(
            DialogKind::MessageBox,
            Answer::Button(MessageBoxResult::Cancel),
        );
        app.controller().push(Request::Quit);
        app.pump();
        assert_eq!(app.exit_code(), None);
        assert!(app.idle().is_some());
        assert_eq!(app.controller().windows().len(), 1);
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn a_clean_quit_ends_the_loop_through_the_idle_handler() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "text\n");
    let services = ScriptedServices::new();
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options(&services, None)).unwrap();
        app.pump();
        assert!(app.idle().is_some());
        assert_eq!(app.exit_code(), None);
        app.controller().push(Request::Quit);
        assert_eq!(app.idle(), None, "no sleep: the loop ends");
        assert_eq!(app.exit_code(), Some(0));
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn text_of_an_untitled_buffer_survives_the_quit_that_closes_its_window() {
    let tmp = tempfile::tempdir().unwrap();
    let services = ScriptedServices::new();

    // A start without files opens one untitled buffer; it gets text nobody saves.
    let code = start(&[], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options(&services, None)).unwrap();
        app.pump();
        let cell = Rc::clone(&app.controller().windows()[0]);
        app.controller().new_untitled(&cell);
        app.pump();
        let entry = cell
            .view
            .borrow()
            .active_tab()
            .expect("a tab")
            .entry
            .clone();
        assert!(
            entry.untitled_id().is_some(),
            "the new tab is an untitled buffer"
        );
        {
            let mut document = entry.document().borrow_mut();
            let edit = Transaction::single(
                Edit::insert(0, "work in progress"),
                Selection::caret(0),
                Selection::caret(16),
            );
            document.apply(edit).unwrap();
        }

        // Quitting closes the window, which drops the document: the text has to be stored
        // before that, not when the session is written at the end.
        app.controller().push(Request::Quit);
        assert_eq!(app.idle(), None, "the loop ends");
        app.finish();
        0
    });
    assert_eq!(code, 0);

    // The next start finds the buffer again, with its text.
    let code = start(&[], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options(&services, None)).unwrap();
        app.pump();
        let window = &app.controller().windows()[0];
        let view = window.view.borrow();
        let texts: Vec<String> = view
            .tabs()
            .iter()
            .map(|tab| tab.entry.document().borrow().text())
            .collect();
        assert!(
            texts.iter().any(|text| text == "work in progress"),
            "the buffer came back: {texts:?}"
        );
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn the_settings_window_opens_without_an_owner_takes_events_and_closes_when_destroyed() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "text");
    let services = ScriptedServices::new();
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options(&services, None)).unwrap();
        app.pump();
        let first = app.controller().windows()[0].id;

        // The native window of the active window is gone: Settings opens without an owner.
        app.controller().push(Request::CloseWindow(first));
        app.controller().push(Request::OpenSettings);
        app.pump();
        assert!(app.hwnd(first).is_none());
        let settings = app.settings_hwnd().expect("the Settings window exists");

        // What the window reports is carried out by the loop.
        app.controller()
            .push(Request::Settings(SettingsEvent::OpenDefaultApps));
        app.pump();
        assert_eq!(services.shell_log(), [ShellRecord::OpenDefaultApps]);

        // The system destroys the window: the loop notices and lets go of its state.
        // SAFETY: destroys a window of this thread that the test itself opened.
        unsafe { DestroyWindow(settings) }.unwrap();
        app.pump();
        assert!(app.settings_hwnd().is_none());
        assert!(app.controller().settings_cell().is_none());
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn an_up_to_date_jump_list_is_not_written_again() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "a");
    let other = note(tmp.path(), "b.md", "b");
    let id = unique_id("b2jump");
    let exe = std::env::current_exe().unwrap();
    let services = ScriptedServices::new();
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let link = WinInstance::with_app_id(id.clone()).link();
        let mut app_options = options(&services, None);
        app_options.instance = Some(link);
        app_options.jump_list = true;
        let app = UiApp::new(core, app_options).unwrap();
        app.pump();
        let cell = app.controller().windows()[0].clone();
        app.controller().open_file(&cell, &other).unwrap();
        app.pump();

        // The next request writes the list with the file opened since; the one after makes
        // it stale again, but the recent files are the same.
        for _ in 0..2 {
            app.controller()
                .push(Request::Command(cell.id, CommandId::ViewZoomIn));
            app.pump();
        }
        let text = log_text(&app);
        assert!(!text.contains("cannot update the Jump List"), "{text}");

        // Nothing stays behind in the shell.
        JumpList::new(id.clone(), exe.clone(), "Recent", "New window")
            .delete()
            .unwrap();
        0
    });
    assert_eq!(code, 0);
}

/// The text of the application log so far.
fn log_text(app: &UiApp) -> String {
    let dir = app.controller().core().borrow().paths.logs_dir();
    std::fs::read_to_string(dir.join("ssw.log")).unwrap_or_default()
}

#[test]
fn a_turn_that_delivers_animation_frames_sleeps_for_one_refresh() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "a");
    let services = ScriptedServices::new();
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options(&services, None)).unwrap();
        app.pump();
        // A widget of the window asks for an animation frame (a fade, an eased scroll).
        app.controller().windows()[0]
            .view
            .borrow_mut()
            .requests_mut()
            .request_animation_frame();
        let frame = app.frame_interval();
        let waits: Vec<Option<std::time::Duration>> = (0..10).map(|_| app.idle()).collect();
        assert!(waits.contains(&Some(frame)), "{waits:?} against {frame:?}");
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn a_theme_change_reaches_the_settings_window_too() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "a");
    let services = ScriptedServices::new();
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options(&services, None)).unwrap();
        app.pump();
        app.controller().push(Request::OpenSettings);
        app.pump();
        assert!(app.settings_hwnd().is_some());
        // The system turns dark; the app follows the system by default.
        app.controller().set_system_theme(SystemTheme {
            theme: Theme::dark(DEFAULT_ACCENT),
            dark: true,
            high_contrast: false,
            transparency_effects: false,
            client_animations: true,
            accent: DEFAULT_ACCENT,
        });
        app.pump();
        assert_eq!(app.controller().theme().mode, ThemeMode::Dark);
        assert!(app.settings_hwnd().is_some());
        assert_eq!(
            app.controller().take_theme_change(),
            None,
            "taken by the turn"
        );
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn the_startup_update_check_respects_the_setting() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "a");
    let services = ScriptedServices::new();
    let settings = r#"{"general":{"autosave":false,"check_for_updates":false}}"#;
    let code = start_with(settings, &[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let mut app_options = options(&services, None);
        app_options.update_check = true;
        let app = UiApp::new(core, app_options).unwrap();
        app.pump();
        // Switched off: nothing was asked of the network, so nothing is offered.
        assert!(app.controller().available_update().is_none());
        0
    });
    assert_eq!(code, 0);
}

#[test]
fn an_autosave_that_fails_during_a_turn_is_reported_to_the_user() {
    use std::os::windows::fs::OpenOptionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "locked.md", "start\n");
    let services = ScriptedServices::new();
    let settings = r#"{"general":{"autosave":true,"autosave_delay_ms":200}}"#;
    let code = start_with(
        settings,
        std::slice::from_ref(&file),
        tmp.path(),
        |core, _| {
            let _ole = OleGuard::initialize().unwrap();
            let app = UiApp::new(core, options(&services, None)).unwrap();
            app.pump();
            let entry = app.controller().docs().find(&file).unwrap();
            {
                let mut document = entry.document().borrow_mut();
                let end = document.buffer().len();
                let edit = Transaction::single(
                    Edit::insert(end, "more"),
                    Selection::caret(end),
                    Selection::caret(end + 4),
                );
                document.apply(edit).unwrap();
            }
            app.pump();
            // Another program holds the file without sharing it when the autosave is due.
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&file)
                .unwrap();
            services.answer(DialogKind::MessageBox, Answer::Button(MessageBoxResult::Ok));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while services.dialog_log().is_empty() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "no report of the failed autosave"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
                app.pump();
            }
            drop(lock);
            let shown = services.dialog_log();
            assert_eq!(shown[0].kind, DialogKind::MessageBox);
            0
        },
    );
    assert_eq!(code, 0);
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_menu_that_cannot_be_built_is_logged_and_chooses_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "a");
    let services = ScriptedServices::new();
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        // The host would choose the first command of any menu it is shown.
        let app = UiApp::new(core, options(&services, Some(1))).unwrap();
        app.pump();
        let id = app.controller().windows()[0].id;
        // The first system call of showing the menu (`CreatePopupMenu`) fails.
        ssw_win::fault::arm(0);
        let chosen = app.show_popup(id, &MenuModel::default(), Point::new(4.0, 4.0));
        let reached = ssw_win::fault::tripped();
        ssw_win::fault::disarm();
        assert!(reached);
        assert_eq!(chosen, None);
        let text = log_text(&app);
        assert!(text.contains("cannot show the menu"), "{text}");
        0
    });
    assert_eq!(code, 0);
}

/// The core of a real startup in a fresh profile below `root`, over `devices`.
fn core_with(root: &Path, devices: DeviceStartup) -> AppCore {
    let args = test_args(root, &[]);
    let mut dialogs = ScriptedServices::new().dialogs();
    let outcome = boot(&args, process(&args), &mut *dialogs, None).unwrap();
    let mut started = *outcome.started().expect("startup went on");
    started.devices = devices;
    AppCore::new(started, Arc::new(DeferredWaker::new()))
}

#[test]
fn devices_that_cannot_be_made_stop_the_application_that_waits_for_them() {
    let tmp = tempfile::tempdir().unwrap();
    let _ole = OleGuard::initialize().unwrap();
    let core = core_with(
        tmp.path(),
        DeviceStartup::spawn_with_factory(|| Err(WinError::Invalid("no adapter"))),
    );
    let error = UiApp::new(core, options(&ScriptedServices::new(), None)).unwrap_err();
    assert_eq!(error.to_string(), "no adapter");
}

#[test]
fn a_process_id_the_shell_rejects_is_logged_and_the_run_goes_on() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "a");
    // Longer than the 128 characters an AppUserModelID may have.
    let link = WinInstance::with_app_id("A".repeat(200)).link();
    let code = start(&[file], tmp.path(), move |core, args| {
        let log = core.paths.logs_dir().join("ssw.log");
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
        let code = UiShell::new(Some(link)).run(core, args);
        let text = std::fs::read_to_string(log).unwrap();
        assert!(text.contains("AppUserModelID not set"), "{text}");
        code
    });
    assert_eq!(code, 0);
}

// ----- every system call of the creation steps failing in turn --------------------------------

#[cfg(feature = "fault-injection")]
#[test]
fn every_piece_of_a_new_window_that_cannot_be_made_closes_the_window_and_is_logged() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "text");
    let services = ScriptedServices::new();
    let code = start(&[file], tmp.path(), |core, _| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options(&services, None)).unwrap();
        app.pump();
        let log = app.controller().core().borrow().paths.logs_dir();
        let mut failed = 0;
        for call in 0..2000 {
            let cell = app.controller().create_window(
                None,
                ssw_ui::geometry::Size::new(800.0, 600.0),
                1.0,
            );
            ssw_win::fault::arm(call);
            app.pump();
            let reached = ssw_win::fault::tripped();
            ssw_win::fault::disarm();
            if app.hwnd(cell.id).is_none() {
                failed += 1;
                assert!(
                    !app.controller()
                        .windows()
                        .iter()
                        .any(|window| window.id == cell.id),
                    "call {call}: a window without a native one is closed"
                );
            } else {
                // Leave nothing behind for the next round.
                app.controller().push(Request::CloseRequested(cell.id));
                app.pump();
            }
            if !reached {
                break;
            }
        }
        assert!(failed >= 3, "only {failed} creation failures were reported");
        assert_eq!(app.controller().windows().len(), 1);
        let text = std::fs::read_to_string(log.join("ssw.log")).unwrap();
        assert!(text.contains("cannot create window"), "{text}");
        0
    });
    assert_eq!(code, 0);
}

#[cfg(feature = "fault-injection")]
#[test]
fn every_piece_of_the_start_that_cannot_be_made_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let _ole = OleGuard::initialize().unwrap();
    let shared = ssw_win::gfx::GfxDevices::create_on(Driver::Warp, 1).unwrap();
    let services = ScriptedServices::new();
    let mut errors = Vec::new();
    let mut finished = false;
    for call in 0..500 {
        let devices = shared.clone();
        let core = core_with(
            tmp.path(),
            DeviceStartup::spawn_with_factory(move || Ok(devices)),
        );
        ssw_win::fault::arm(call);
        let outcome = UiApp::new(core, options(&services, None));
        let reached = ssw_win::fault::tripped();
        ssw_win::fault::disarm();
        if let Err(error) = outcome {
            errors.push(error.to_string());
        }
        if !reached {
            finished = true;
            break;
        }
    }
    assert!(finished, "the sweep did not reach the end of the start");
    assert!(errors.len() >= 3, "{errors:?}");
    assert!(errors.iter().all(|error| !error.is_empty()), "{errors:?}");
}

#[cfg(feature = "fault-injection")]
#[test]
fn every_piece_of_the_instance_handshake_that_cannot_be_made_is_reported() {
    use ssw_app::args::Launch;
    use ssw_app::startup::{InstanceHook, InstanceRole};

    let id = unique_id("b2claim");
    let mut primary = WinInstance::with_app_id(id.clone());
    assert_eq!(
        primary.claim(&Launch::default()).unwrap(),
        InstanceRole::Primary
    );
    // A later process of the same application: each of its system calls fails once.
    let outcomes = ssw_win::fault::sweep(|| {
        let mut later = WinInstance::with_app_id(id.clone());
        later.claim(&Launch::default()).map_err(|error| error.0)
    });
    // The checked calls of a claim: the message window, then the mutex.
    let failures: Vec<&str> = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().err())
        .map(|reason| reason.split(':').next().unwrap_or_default())
        .collect();
    assert_eq!(
        failures,
        ["create message window failed", "CreateMutexW failed"],
        "{outcomes:?}"
    );
}
