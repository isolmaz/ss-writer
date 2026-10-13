//! Plain mode (spec §4.1): the View menu's Word wrap and Render as Markdown for text files, from
//! the menu model and from the command palette. The real controller runs over a started
//! application in a temporary profile; dialogs and the shell are scripted. Nothing is shown.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::commands::Dispatch;
use ssw_app::documents::{DocEntry, DocMode};
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::{Controller, WindowCell};
use ssw_app::ui::inspector::toc_of;
use ssw_app::ui::menus::{build_menu, menu_id};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_app::ui::shell_view::ShellAction;
use ssw_core::command::CommandId;
use ssw_core::edit::{Edit, Selection, Transaction};
use ssw_ui::editor::highlight::Language;
use ssw_ui::editor::{DocumentKind, EditorConfig};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
use ssw_ui::shell::picker::Row;
use ssw_ui::shell::titlebar::MenuKind;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::menus::{CheckState, MenuEntry};
use ssw_win::window::current_os_version;

/// Autosave would write the unsaved edits some tests keep.
const NO_AUTOSAVE: &str = r#"{"general":{"autosave":false}}"#;

/// What `log.txt` holds: a BOM, CRLF line endings and text that is Markdown if read as such.
const LOG: &[u8] = b"\xEF\xBB\xBF# Title\r\n- item\r\n";

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
/// also holds the plain-text files `log.txt` and `other.txt`.
fn with_rig(body: impl FnOnce(&Rig)) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("notes.md"), "# Notes\n").unwrap();
    std::fs::write(work.join("log.txt"), LOG).unwrap();
    std::fs::write(work.join("other.txt"), "other\n").unwrap();
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

/// How a command ended, for assertions.
fn ended(outcome: &Dispatch) -> &'static str {
    match outcome {
        Dispatch::Handled => "handled",
        Dispatch::Unhandled => "unhandled",
        Dispatch::Failed(_) => "failed",
    }
}

/// Adds `text` at the end of `entry`'s document.
fn append(entry: &DocEntry, text: &str) {
    let mut document = entry.document().borrow_mut();
    let end = document.buffer().len();
    document
        .apply(Transaction::single(
            Edit::insert(end, text),
            Selection::caret(end),
            Selection::caret(end + text.len()),
        ))
        .unwrap();
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

    fn command(&self, command: CommandId) -> Dispatch {
        self.controller.run_command(self.window().id, command)
    }

    /// The keys of the window's tabs, left to right.
    fn tab_keys(&self) -> Vec<u64> {
        let window = self.window();
        let view = window.view.borrow();
        view.tabs().iter().map(|tab| tab.key).collect()
    }

    fn activate(&self, key: u64) {
        assert!(self.window().view.borrow_mut().activate_tab(key));
    }

    /// The document of the active tab.
    fn entry(&self) -> Rc<DocEntry> {
        let window = self.window();
        let view = window.view.borrow();
        Rc::clone(&view.active_tab().expect("a tab is open").entry)
    }

    /// How the active tab's editor presents its document.
    fn kind(&self) -> DocumentKind {
        let window = self.window();
        let view = window.view.borrow();
        view.active_tab()
            .expect("a tab is open")
            .editor
            .config()
            .document_kind
    }

    /// Whether the active tab's editor wraps long lines.
    fn wrap(&self) -> bool {
        let window = self.window();
        let view = window.view.borrow();
        view.active_tab()
            .expect("a tab is open")
            .editor
            .config()
            .wrap
    }

    /// The language the active tab's editor colours its document as.
    fn language(&self) -> Option<Language> {
        let window = self.window();
        let view = window.view.borrow();
        view.active_tab()
            .expect("a tab is open")
            .editor
            .config()
            .plain_language
    }

    /// The (enabled, checked) state of the View menu's Word wrap and Render as Markdown items.
    fn switches(&self) -> [(bool, bool); 2] {
        let window = self.window();
        let model = build_menu(
            MenuKind::View,
            self.controller.i18n(),
            &self.controller.keymap(),
            &self.controller.menu_state(&window),
        );
        [CommandId::ViewWordWrap, CommandId::ViewRenderAsMarkdown].map(|command| {
            model
                .entries
                .iter()
                .find_map(|entry| match entry {
                    MenuEntry::Item(item) if item.id == menu_id(command) => {
                        Some((item.enabled, item.check == CheckState::Check))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("the View menu lists {command}"))
        })
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
}

// ----- who has the switches ----------------------------------------------------------------------

#[test]
fn the_menu_lists_both_switches_for_every_document_but_only_plain_files_can_use_them() {
    with_rig(|rig| {
        // A Markdown file: listed, disabled and unmarked; the commands decline.
        assert_eq!(rig.kind(), DocumentKind::Markdown);
        assert_eq!(rig.switches(), [(false, false), (false, false)]);
        for command in [CommandId::ViewWordWrap, CommandId::ViewRenderAsMarkdown] {
            assert_eq!(ended(&rig.command(command)), "unhandled", "{command}");
        }
        assert_eq!(rig.kind(), DocumentKind::Markdown);
        assert!(!rig.entry().render_as_markdown());

        // An untitled buffer is Markdown as well.
        rig.command(CommandId::TabNew);
        assert!(rig.entry().is_untitled());
        assert_eq!(rig.switches(), [(false, false), (false, false)]);
        assert_eq!(ended(&rig.command(CommandId::ViewWordWrap)), "unhandled");

        // A plain-text file: wrapped by default, not rendered.
        rig.open("log.txt");
        assert_eq!(rig.kind(), DocumentKind::PlainText);
        assert_eq!(rig.switches(), [(true, true), (true, false)]);
    });
}

#[test]
fn a_document_without_a_plain_file_type_has_no_render_switch() {
    with_rig(|rig| {
        let docs = rig.controller.docs();
        let diagram = rig.work.join("diagram.mmd");
        std::fs::write(&diagram, "graph TD\n  A-->B\n").unwrap();
        let mermaid = docs.open(&diagram).unwrap();
        let markdown = docs.open(&rig.work.join("notes.md")).unwrap();
        rig.command(CommandId::TabNew);
        let untitled = rig.entry();
        for (entry, mode) in [
            (&mermaid, DocMode::Mermaid),
            (&markdown, DocMode::Markdown),
            (&untitled, DocMode::Markdown),
        ] {
            assert!(!entry.set_render_as_markdown(true));
            assert!(!entry.render_as_markdown());
            assert_eq!((entry.mode(), entry.file_mode()), (mode, mode));
        }

        let text = docs.open(&rig.work.join("other.txt")).unwrap();
        assert_eq!(
            (text.mode(), text.file_mode()),
            (DocMode::Plain, DocMode::Plain)
        );
        assert!(text.set_render_as_markdown(true));
        assert_eq!(
            (text.mode(), text.file_mode()),
            (DocMode::Markdown, DocMode::Plain)
        );
        assert!(text.set_render_as_markdown(false));
        assert_eq!(text.mode(), DocMode::Plain);
    });
}

// ----- word wrap ---------------------------------------------------------------------------------

#[test]
fn word_wrap_is_chosen_per_tab_and_survives_changed_settings() {
    with_rig(|rig| {
        rig.open("log.txt");
        rig.open("other.txt");
        assert!(rig.wrap(), "plain files start wrapped");

        // Off in this tab: the editor lays the lines out unwrapped; the menu shows it.
        assert_eq!(ended(&rig.command(CommandId::ViewWordWrap)), "handled");
        assert!(!rig.wrap());
        assert_eq!(rig.switches(), [(true, false), (true, false)]);
        assert_eq!(rig.kind(), DocumentKind::PlainText);

        // New settings (fonts, zoom) do not undo the choice of the tab.
        let config = EditorConfig::from_settings(&rig.controller.settings());
        rig.window().view.borrow_mut().set_editor_config(config);
        assert!(!rig.wrap());

        // The other plain tab and the Markdown tab are not affected.
        let keys = rig.tab_keys();
        assert_eq!(keys.len(), 3);
        rig.activate(keys[1]);
        assert!(rig.wrap());
        assert_eq!(rig.switches(), [(true, true), (true, false)]);
        rig.activate(keys[0]);
        assert!(rig.wrap(), "a Markdown tab always wraps");
        assert_eq!(rig.switches(), [(false, false), (false, false)]);

        // On again.
        rig.activate(keys[2]);
        assert_eq!(ended(&rig.command(CommandId::ViewWordWrap)), "handled");
        assert!(rig.wrap());
        assert_eq!(rig.switches(), [(true, true), (true, false)]);
    });
}

// ----- syntax colours ----------------------------------------------------------------------------

#[test]
fn a_plain_file_is_coloured_as_its_extension_says_and_an_unknown_one_stays_plain() {
    with_rig(|rig| {
        for (name, text) in [
            ("data.json", "{\"a\": [1, true]}\n"),
            ("page.html", "<p class=\"x\">hi</p>\n"),
            ("tool.py", "def f():\n    return None\n"),
            ("style.css", "p { color: red; }\n"),
        ] {
            std::fs::write(rig.work.join(name), text).unwrap();
        }
        for (name, language) in [
            ("data.json", Language::Json),
            ("page.html", Language::Html),
            ("tool.py", Language::Python),
            ("style.css", Language::Css),
        ] {
            rig.open(name);
            assert_eq!(rig.kind(), DocumentKind::PlainText, "{name}");
            assert_eq!(rig.language(), Some(language), "{name}");
        }

        // `.txt` has no language: its lines stay uncoloured.
        rig.open("log.txt");
        assert_eq!(rig.kind(), DocumentKind::PlainText);
        assert_eq!(rig.language(), None);

        // New settings do not take the language away from a tab.
        rig.activate(rig.tab_keys()[1]);
        assert_eq!(rig.language(), Some(Language::Json));
        let config = EditorConfig::from_settings(&rig.controller.settings());
        assert_eq!(config.plain_language, None);
        rig.window().view.borrow_mut().set_editor_config(config);
        assert_eq!(rig.language(), Some(Language::Json));

        // Showing the file as Markdown and back keeps it too.
        assert_eq!(
            ended(&rig.command(CommandId::ViewRenderAsMarkdown)),
            "handled"
        );
        assert_eq!(rig.kind(), DocumentKind::Markdown);
        assert_eq!(
            ended(&rig.command(CommandId::ViewRenderAsMarkdown)),
            "handled"
        );
        assert_eq!(rig.kind(), DocumentKind::PlainText);
        assert_eq!(rig.language(), Some(Language::Json));
    });
}

// ----- render as Markdown ------------------------------------------------------------------------

#[test]
fn render_as_markdown_shows_a_plain_file_as_markdown_and_saves_it_as_it_was() {
    with_rig(|rig| {
        rig.open("log.txt");
        let entry = rig.entry();
        assert_eq!(entry.mode(), DocMode::Plain);
        assert!(
            toc_of(&entry).entries().is_empty(),
            "plain text has no outline"
        );

        assert_eq!(
            ended(&rig.command(CommandId::ViewRenderAsMarkdown)),
            "handled"
        );
        assert_eq!(rig.kind(), DocumentKind::Markdown);
        assert_eq!(
            (entry.mode(), entry.file_mode(), entry.render_as_markdown()),
            (DocMode::Markdown, DocMode::Plain, true)
        );
        // The rest of the application treats it as Markdown now.
        let headings: Vec<String> = toc_of(&entry)
            .entries()
            .iter()
            .map(|heading| heading.title.clone())
            .collect();
        assert_eq!(headings, ["Title"]);
        // The switch stays available (and marked) to turn it off; there is nothing to wrap.
        assert_eq!(rig.switches(), [(false, false), (true, true)]);
        // It is still the file `log.txt`.
        assert_eq!(entry.title(rig.controller.i18n()), "log.txt");

        // What is written does not depend on the presentation: path, BOM and line endings.
        append(&entry, "more\n");
        assert!(entry.is_dirty());
        assert_eq!(ended(&rig.command(CommandId::FileSave)), "handled");
        assert!(!entry.is_dirty());
        let path = entry.path().unwrap();
        assert!(path.ends_with("log.txt"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\xEF\xBB\xBF# Title\r\n- item\r\nmore\r\n"
        );
        assert!(entry.format().bom);

        // Back to plain lines, wrapped as before.
        assert_eq!(
            ended(&rig.command(CommandId::ViewRenderAsMarkdown)),
            "handled"
        );
        assert_eq!(rig.kind(), DocumentKind::PlainText);
        assert_eq!(entry.mode(), DocMode::Plain);
        assert!(!entry.render_as_markdown());
        assert!(toc_of(&entry).entries().is_empty());
        assert_eq!(rig.switches(), [(true, true), (true, false)]);
    });
}

#[test]
fn every_tab_of_the_document_follows_but_other_documents_do_not() {
    with_rig(|rig| {
        rig.open("log.txt");
        rig.open("other.txt");
        // A second tab on log.txt: the registry hands out the document that is already open.
        let window = rig.window();
        let log = rig.work.join("log.txt");
        let shared = rig.controller.docs().open(&log).unwrap();
        assert_eq!(shared.users(), 2, "the first tab and this one");
        window.view.borrow_mut().add_tab(shared, None);
        assert_eq!(
            ended(&rig.command(CommandId::ViewRenderAsMarkdown)),
            "handled"
        );
        let kinds: Vec<DocumentKind> = window
            .view
            .borrow()
            .tabs()
            .iter()
            .map(|tab| tab.editor.config().document_kind)
            .collect();
        assert_eq!(
            kinds,
            [
                DocumentKind::Markdown,  // notes.md
                DocumentKind::Markdown,  // log.txt, rendered
                DocumentKind::PlainText, // other.txt
                DocumentKind::Markdown,  // log.txt again
            ]
        );
    });
}

// ----- the command palette -----------------------------------------------------------------------

#[test]
fn the_palette_lists_the_switches_disabled_for_a_markdown_file_and_refuses_them() {
    with_rig(|rig| {
        rig.command(CommandId::SearchCommandPalette);
        assert!(rig.palette_visible());
        assert!(rig.row("Word wrap").disabled);
        assert!(rig.row("Render as Markdown").disabled);
        assert!(!rig.choose(">Render as Markdown"));
        assert!(rig.palette_visible(), "refused: the palette stays open");
        assert_eq!(rig.rows()[0].label, "Render as Markdown");
        assert_eq!(rig.kind(), DocumentKind::Markdown);
    });
}

#[test]
fn the_palette_runs_the_switches_for_a_plain_file_and_follows_their_state() {
    with_rig(|rig| {
        rig.open("log.txt");
        rig.command(CommandId::SearchCommandPalette);
        assert!(!rig.row("Word wrap").disabled);
        assert!(!rig.row("Render as Markdown").disabled);
        assert!(rig.choose(">Word wrap"));
        assert!(!rig.palette_visible());
        assert!(!rig.wrap());

        rig.command(CommandId::SearchCommandPalette);
        assert!(rig.choose(">Render as Markdown"));
        assert_eq!(rig.kind(), DocumentKind::Markdown);

        // Rendered: there is no plain text to wrap, but the switch can be turned off again.
        rig.command(CommandId::SearchCommandPalette);
        assert!(rig.row("Word wrap").disabled);
        assert!(!rig.row("Render as Markdown").disabled);
        assert!(rig.choose(">Render as Markdown"));
        assert_eq!(rig.kind(), DocumentKind::PlainText);
        assert!(!rig.wrap(), "the tab kept its choice of not wrapping");
    });
}
