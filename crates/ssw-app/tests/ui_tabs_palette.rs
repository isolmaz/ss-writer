//! Opening a file replaces an untouched Untitled tab (and keeps every other kind of tab), and
//! the command palette lists the commands that do not apply drawn disabled and refuses to run
//! them (manual test SRCH-06), with the same enablement as the menus. The real controller runs
//! over a started application in a temporary profile; dialogs and the shell are scripted.
//! Nothing is shown.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::documents::replace_text;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::{Controller, WindowCell};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_app::ui::shell_view::ShellAction;
use ssw_core::command::CommandId;
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
use ssw_ui::shell::picker::Row;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::window::current_os_version;

/// Autosave would write the unsaved edits some tests keep.
const NO_AUTOSAVE: &str = r#"{"general":{"autosave":false}}"#;

fn process(args: &Args) -> ProcessEnv {
    ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    }
}

struct Rig {
    controller: Rc<Controller>,
    work: PathBuf,
}

struct Host<B> {
    body: Option<B>,
    work: PathBuf,
    services: ScriptedServices,
}

impl<B: FnOnce(&Rig)> ShellHost for Host<B> {
    fn run(&mut self, core: AppCore, _args: &Args) -> i32 {
        let services = self.services.clone();
        let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let plan = core.plan.clone();
        let controller =
            Controller::new(core, services.dialogs(), services.shell(), clipboard, clock);
        controller.open_plan(&plan);
        let rig = Rig {
            controller,
            work: self.work.clone(),
        };
        (self.body.take().expect("the body runs once"))(&rig);
        assert_eq!(services.take_unexpected(), None, "a dialog nobody expected");
        assert!(
            services.unused_answers().is_empty(),
            "an answer no dialog asked for"
        );
        0
    }
}

/// Runs `body` over a started application whose first window shows `notes.md` of a folder that
/// also holds `a.md`, `b.md` and a copy of a Word fixture, `report.docx`.
fn with_rig(body: impl FnOnce(&Rig)) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    for (name, text) in [
        ("notes.md", "# Notes\n"),
        ("a.md", "# A\n"),
        ("b.md", "# B\n"),
    ] {
        std::fs::write(work.join(name), text).unwrap();
    }
    let word = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("docx")
        .join("poi_heading123.docx");
    std::fs::copy(word, work.join("report.docx")).unwrap();
    let roaming = tmp.path().join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&roaming).unwrap();
    std::fs::write(roaming.join("settings.json"), NO_AUTOSAVE).unwrap();
    let mut args = Args {
        paths: vec![work.join("notes.md")],
        ..Args::default()
    };
    args.test.hidden = true;
    args.test.profile = Some(tmp.path().join("profile"));
    let mut startup_dialogs = ScriptedServices::new().dialogs();
    let code = run(
        &args,
        process(&args),
        &mut *startup_dialogs,
        None,
        &mut Host {
            body: Some(body),
            work: ssw_app::documents::canonical_path(&work),
            services: ScriptedServices::new(),
        },
    );
    assert_eq!(code, 0);
}

impl Rig {
    fn window(&self) -> Rc<WindowCell> {
        Rc::clone(&self.controller.windows()[0])
    }

    fn open(&self, name: &str) {
        self.controller
            .open_file(&self.window(), &self.work.join(name))
            .unwrap();
    }

    fn command(&self, command: CommandId) {
        self.controller.run_command(self.window().id, command);
    }

    /// For every tab, left to right: its file name, or `None` for an untitled buffer.
    fn tabs(&self) -> Vec<Option<String>> {
        self.window()
            .view
            .borrow()
            .tabs()
            .iter()
            .map(|tab| {
                tab.entry
                    .path()
                    .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            })
            .collect()
    }

    /// The file name of the active tab, `None` for an untitled buffer.
    fn active_file(&self) -> Option<String> {
        let window = self.window();
        let view = window.view.borrow();
        let tab = view.active_tab().expect("a tab is open");
        tab.entry
            .path()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
    }

    /// Replaces the text of the active document with one unsaved edit.
    fn set_text(&self, text: &str) {
        let window = self.window();
        let view = window.view.borrow();
        let entry = Rc::clone(&view.active_tab().expect("a tab is open").entry);
        let changed = replace_text(&mut entry.document().borrow_mut(), text).unwrap();
        assert!(changed, "the text differs");
    }

    /// How many untitled documents the application still keeps.
    fn untitled_documents(&self) -> usize {
        self.controller
            .docs()
            .all()
            .iter()
            .filter(|entry| entry.is_untitled())
            .count()
    }

    fn rows(&self) -> Vec<Row> {
        self.window()
            .view
            .borrow()
            .overlays()
            .quick_open()
            .list()
            .rows()
            .to_vec()
    }

    fn row(&self, label: &str) -> Row {
        self.rows()
            .into_iter()
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("the palette lists {label:?}"))
    }

    /// Types `query` into the open palette and chooses the selected row like Enter does; what
    /// the overlay asks of the controller is carried out. Returns whether a row was chosen.
    fn choose(&self, query: &str) -> bool {
        let window = self.window();
        let chosen = {
            let mut view = window.view.borrow_mut();
            let quick = view.overlays_mut().quick_open_mut();
            quick.set_input(query);
            quick.activate()
        };
        let actions = window.view.borrow_mut().pump_overlays();
        for action in actions {
            self.controller
                .handle_action(window.id, ShellAction::Overlay(action));
        }
        chosen
    }

    fn palette_visible(&self) -> bool {
        self.window().view.borrow().overlays().quick_open_visible()
    }

    fn status_bar_visible(&self) -> bool {
        self.window().view.borrow().layout().status_bar_visible()
    }
}

// ----- an unused Untitled tab is replaced --------------------------------------------------------

#[test]
fn opening_a_file_replaces_the_pristine_untitled_tab() {
    with_rig(|rig| {
        rig.command(CommandId::TabNew);
        assert_eq!(rig.tabs(), [Some("notes.md".to_owned()), None]);
        assert_eq!(rig.untitled_documents(), 1);

        rig.open("a.md");
        assert_eq!(
            rig.tabs(),
            [Some("notes.md".to_owned()), Some("a.md".to_owned())]
        );
        assert_eq!(rig.active_file().as_deref(), Some("a.md"));
        assert_eq!(
            rig.untitled_documents(),
            0,
            "the replaced buffer is released"
        );
        // Nothing is left to replace now: the next file gets a tab of its own.
        rig.open("b.md");
        assert_eq!(rig.tabs().len(), 3);
        assert_eq!(rig.active_file().as_deref(), Some("b.md"));
    });
}

#[test]
fn a_file_chosen_in_quick_open_replaces_the_pristine_untitled_tab() {
    with_rig(|rig| {
        rig.command(CommandId::TabNew);
        // Quick Open lists the workspace of the active tab.
        rig.controller.show_workspace(&rig.window(), &rig.work);
        rig.command(CommandId::SearchQuickOpen);
        assert!(rig.palette_visible());
        assert!(rig.choose("b.md"));
        assert_eq!(
            rig.tabs(),
            [Some("notes.md".to_owned()), Some("b.md".to_owned())]
        );
        assert_eq!(rig.untitled_documents(), 0);
    });
}

#[test]
fn an_untitled_tab_with_text_or_with_an_undo_history_stays() {
    with_rig(|rig| {
        rig.command(CommandId::TabNew);
        rig.set_text("draft\n");
        rig.open("a.md");
        assert_eq!(
            rig.tabs(),
            [Some("notes.md".to_owned()), None, Some("a.md".to_owned())]
        );
        assert_eq!(rig.untitled_documents(), 1);

        // Typed and deleted again: the text is empty, the buffer has been used.
        rig.command(CommandId::TabNew);
        rig.set_text("x\n");
        rig.set_text("");
        rig.open("b.md");
        assert_eq!(
            rig.tabs(),
            [
                Some("notes.md".to_owned()),
                None,
                Some("a.md".to_owned()),
                None,
                Some("b.md".to_owned())
            ]
        );
        assert_eq!(rig.untitled_documents(), 2);
    });
}

#[test]
fn an_untitled_tab_that_is_not_the_active_one_stays() {
    with_rig(|rig| {
        rig.command(CommandId::TabNew);
        let window = rig.window();
        let notes = window.view.borrow().tabs()[0].key;
        window.view.borrow_mut().activate_tab(notes);
        rig.open("a.md");
        assert_eq!(
            rig.tabs(),
            [Some("notes.md".to_owned()), None, Some("a.md".to_owned())]
        );
        assert_eq!(rig.active_file().as_deref(), Some("a.md"));
    });
}

#[test]
fn a_file_that_is_already_open_is_shown_and_the_untitled_tab_stays() {
    with_rig(|rig| {
        rig.command(CommandId::TabNew);
        rig.open("notes.md");
        assert_eq!(rig.tabs(), [Some("notes.md".to_owned()), None]);
        assert_eq!(rig.active_file().as_deref(), Some("notes.md"));
    });
}

// ----- the palette follows the menus' enablement --------------------------------------------------

#[test]
fn the_palette_draws_commands_that_do_not_apply_disabled_and_refuses_them() {
    with_rig(|rig| {
        rig.command(CommandId::SearchCommandPalette);
        assert!(rig.palette_visible());
        // A plain Markdown file cannot be saved back to Word; the commands that need nothing
        // special apply.
        assert!(rig.row("Save back to Word").disabled);
        assert!(!rig.row("Save").disabled);
        assert!(!rig.row("Status bar").disabled);

        // Enter on the disabled command does nothing: the palette stays open and the command
        // is not run (it would report "cannot save back" in a dialog nobody expects).
        assert!(!rig.choose(">Save back to Word"));
        assert!(rig.palette_visible());
        assert_eq!(rig.rows()[0].label, "Save back to Word");

        // An enabled command still runs from the same list.
        let before = rig.status_bar_visible();
        assert!(rig.choose(">Status bar"));
        assert!(!rig.palette_visible());
        assert_eq!(rig.status_bar_visible(), !before);
    });
}

#[test]
fn quick_open_in_command_mode_uses_the_same_enablement() {
    with_rig(|rig| {
        // The same list when it is reached by typing `>` into Quick Open.
        rig.command(CommandId::SearchQuickOpen);
        assert!(!rig.choose(">Save back to Word"));
        assert!(rig.palette_visible());
        assert!(rig.row("Save back to Word").disabled);
    });
}

#[test]
fn the_palette_enables_saving_back_for_an_imported_word_document() {
    with_rig(|rig| {
        rig.open("report.docx");
        let state = rig.controller.menu_state(&rig.window());
        assert!(state.enabled(CommandId::FileSaveBackToWord));
        rig.command(CommandId::SearchCommandPalette);
        assert!(!rig.row("Save back to Word").disabled);
        // With an empty query the rows are the commands in menu order: the palette agrees with
        // the menus for every one of them.
        let rows = rig.rows();
        assert_eq!(rows.len(), CommandId::ALL.len());
        for (row, command) in rows.iter().zip(CommandId::all()) {
            assert_eq!(!row.disabled, state.enabled(command), "{}", row.label);
        }
    });
}
