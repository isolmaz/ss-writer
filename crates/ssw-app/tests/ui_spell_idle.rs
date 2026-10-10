//! The spelling checks of the painted blocks run in the housekeeping turns of the loop, once no
//! window asked for a repaint for a moment, and the windows repaint when a check answered. Hidden
//! window, a recording provider: nothing is shown, activated or injected.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::host::{SPELL_IDLE, SPELL_TURN, UiApp, UiOptions, UiServices};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_ui::editor::spell::{SpellCheck, SpellProvider};
use ssw_ui::services::MemoryClipboard;
use ssw_win::dragdrop::OleGuard;
use ssw_win::gfx::Driver;
use ssw_win::menus::{PopupHost, PopupRequest};
use ssw_win::window::current_os_version;
use windows::Win32::UI::WindowsAndMessaging::HMENU;

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

impl<F: FnOnce(&UiApp)> ShellHost for Probe<F> {
    fn run(&mut self, core: AppCore, _args: &Args) -> i32 {
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
        (self.body.take().unwrap())(&app);
        0
    }
}

fn with_app(body: impl FnOnce(&UiApp)) {
    let tmp = tempfile::tempdir().unwrap();
    let mut args = Args::default();
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
        &mut Probe { body: Some(body) },
    );
    assert_eq!(code, 0);
}

/// A provider that finds every word wrong and records what it was asked.
#[derive(Default)]
struct Strict {
    checked: RefCell<Vec<String>>,
}

impl SpellProvider for Strict {
    fn check(&self, text: &str) -> Vec<Range<usize>> {
        self.checked.borrow_mut().push(text.to_owned());
        vec![0..text.len()]
    }
    fn suggestions(&self, _word: &str) -> Vec<String> {
        Vec::new()
    }
    fn add_to_dictionary(&self, _word: &str) {}
    fn ignore(&self, _word: &str) {}
}

#[test]
fn a_check_waits_for_a_quiet_moment_then_runs_and_asks_the_windows_to_repaint() {
    with_app(|app| {
        let provider = Rc::new(Strict::default());
        let check = Rc::new(SpellCheck::new(provider.clone()));
        let controller = app.controller();
        assert!(!controller.spell_pending());
        assert!(!controller.pump_spell(Duration::from_millis(5)));
        controller.register_spell(&check);
        assert!(!controller.spell_pending(), "nothing was requested yet");
        // No housekeeping wait is shortened while nothing waits.
        assert_ne!(app.idle(), Some(SPELL_TURN));

        check.request("teh text");
        assert!(controller.spell_pending());
        assert_eq!(
            app.idle(),
            Some(SPELL_TURN),
            "the loop wakes soon for the check"
        );

        // A window just asked for a repaint: the check does not delay that frame.
        controller.active_window().unwrap().dirty.set(true);
        app.pump();
        assert!(provider.checked.borrow().is_empty());
        assert!(!app.pump_spell());
        assert!(check.pending());

        // A quiet moment later the check runs and the windows are asked to repaint.
        std::thread::sleep(SPELL_IDLE + Duration::from_millis(20));
        assert!(app.pump_spell());
        assert_eq!(*provider.checked.borrow(), ["teh text"]);
        assert!(!controller.spell_pending());
        assert!(check.cached("teh text").is_some());
        // Nothing waits: a quiet turn has nothing to do and asks for no repaint.
        assert!(!app.pump_spell());
        assert_ne!(app.idle(), Some(SPELL_TURN));
    });
}

#[test]
fn a_dropped_service_is_forgotten_by_the_controller() {
    with_app(|app| {
        let controller = app.controller();
        {
            let check = Rc::new(SpellCheck::new(Rc::new(Strict::default())));
            controller.register_spell(&check);
            check.request("gone");
            assert!(controller.spell_pending());
        }
        assert!(!controller.spell_pending());
        assert!(!controller.pump_spell(Duration::from_millis(5)));
    });
}
