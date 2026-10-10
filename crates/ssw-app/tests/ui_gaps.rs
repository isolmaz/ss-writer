//! The status bar menus (spec §4.2, §8) and the first look at a new file (spec §6): the real
//! controller over a started application in a temporary profile, with scripted dialogs and shell.
//! Nothing is shown: pointer input is delivered to the window content at the position of the
//! status bar segments, and what the user would see is read back from the menus the loop owner
//! would show, the status bar texts, the document text and the bytes on disk.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::documents::replace_text;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::context_menus::Popup;
use ssw_app::ui::controller::{Controller, WindowCell};
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices};
use ssw_app::workers::WindowId;
use ssw_core::command::CommandId;
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::input::{Event, PointerButton, PointerEvent, PointerEventKind};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard, MessageBoxResult};
use ssw_ui::shell::statusbar::StatusSegment;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::menus::{CheckState, MenuEntry, MenuItem, MenuModel};
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
    services: ScriptedServices,
    engine: MonoTextEngine,
    work: PathBuf,
}

struct Host<B> {
    body: Option<B>,
    work: PathBuf,
}

impl<B: FnOnce(&Rig)> ShellHost for Host<B> {
    fn run(&mut self, core: AppCore, _args: &Args) -> i32 {
        let services = ScriptedServices::new();
        let memory = Rc::new(RefCell::new(MemoryClipboard::default()));
        let clipboard: SharedClipboard = memory;
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let plan = core.plan.clone();
        let controller =
            Controller::new(core, services.dialogs(), services.shell(), clipboard, clock);
        controller.open_plan(&plan);
        let rig = Rig {
            controller,
            services,
            engine: MonoTextEngine::default(),
            work: self.work.clone(),
        };
        (self.body.take().expect("the body runs once"))(&rig);
        assert_eq!(
            rig.services.take_unexpected(),
            None,
            "a dialog nobody expected"
        );
        assert!(
            rig.services.unused_answers().is_empty(),
            "an answer no dialog asked for"
        );
        0
    }
}

/// Runs `body` over a started application whose first window shows `files[0]`.
fn with_rig(files: &[(&str, &[u8])], body: impl FnOnce(&Rig)) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    for (name, bytes) in files {
        let path = work.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let roaming = tmp.path().join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&roaming).unwrap();
    std::fs::write(roaming.join("settings.json"), NO_AUTOSAVE).unwrap();
    let mut args = Args {
        paths: vec![work.join(files[0].0)],
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
            work,
        },
    );
    assert_eq!(code, 0);
}

/// The items of `model` and of its submenus, in menu order.
fn items(model: &MenuModel) -> Vec<&MenuItem> {
    let mut found = Vec::new();
    for entry in &model.entries {
        match entry {
            MenuEntry::Item(item) => found.push(item),
            MenuEntry::Submenu(submenu) => found.extend(items(&submenu.menu)),
            MenuEntry::Separator => {}
        }
    }
    found
}

/// The labels of the top-level submenus of `popup` with whether they are enabled.
fn submenus(popup: &Popup) -> Vec<(String, bool)> {
    popup
        .model
        .entries
        .iter()
        .filter_map(|entry| match entry {
            MenuEntry::Submenu(submenu) => Some((submenu.label.clone(), submenu.enabled)),
            _ => None,
        })
        .collect()
}

/// The children of the submenu `label` of `popup`.
fn children<'a>(popup: &'a Popup, label: &str) -> Vec<&'a MenuItem> {
    popup
        .model
        .entries
        .iter()
        .find_map(|entry| match entry {
            MenuEntry::Submenu(submenu) if submenu.label == label => Some(items(&submenu.menu)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no submenu {label:?}"))
}

impl Rig {
    fn window(&self) -> Rc<WindowCell> {
        Rc::clone(&self.controller.windows()[0])
    }

    fn id(&self) -> WindowId {
        self.window().id
    }

    fn command(&self, command: CommandId) {
        self.controller.run_command(self.id(), command);
    }

    fn entry(&self) -> Rc<ssw_app::documents::DocEntry> {
        let window = self.window();
        let view = window.view.borrow();
        Rc::clone(&view.active_tab().expect("a tab is open").entry)
    }

    fn text(&self) -> String {
        self.entry().document().borrow().text()
    }

    fn open(&self, relative: &str) {
        self.controller
            .open_file(&self.window(), &self.work.join(relative))
            .unwrap();
        self.controller.refresh_all();
    }

    fn answer(&self, button: MessageBoxResult) {
        self.services
            .answer(DialogKind::MessageBox, Answer::Button(button));
    }

    /// Lays the window out and paints it once.
    fn paint(&self) {
        let window = self.window();
        window
            .view
            .borrow_mut()
            .paint(&mut RecordingCanvas::new(), &self.engine, false);
    }

    /// What the status bar shows for `segment`.
    fn status(&self, segment: StatusSegment) -> String {
        self.paint();
        self.window()
            .view
            .borrow()
            .statusbar()
            .text(segment)
            .to_owned()
    }

    /// Clicks the status bar segment and returns the popups the click opens.
    fn click_status(&self, segment: StatusSegment) -> Vec<Popup> {
        self.paint();
        let at = self
            .window()
            .view
            .borrow()
            .statusbar()
            .segment_rect(segment)
            .center();
        let mut popups = Vec::new();
        for kind in [PointerEventKind::Down, PointerEventKind::Up] {
            let event = PointerEvent {
                click_count: 1,
                ..PointerEvent::mouse(kind, Some(PointerButton::Left), at)
            };
            let window = self.window();
            let actions = window
                .view
                .borrow_mut()
                .event(&Event::Pointer(event), &self.engine);
            for action in actions {
                match self.controller.popup_for(window.id, &action) {
                    Some(popup) => popups.push(popup),
                    None => self.controller.handle_action(window.id, action),
                }
            }
        }
        popups
    }

    /// Clicks `segment`, which opens exactly one popup.
    fn status_popup(&self, segment: StatusSegment) -> Popup {
        let mut popups = self.click_status(segment);
        assert_eq!(popups.len(), 1, "one popup");
        popups.remove(0)
    }

    fn choose(&self, popup: &Popup, label: &str, submenu: Option<&str>) {
        let candidates = match submenu {
            Some(submenu) => children(popup, submenu),
            None => items(&popup.model),
        };
        let id = candidates
            .into_iter()
            .find(|item| item.label == label)
            .unwrap_or_else(|| panic!("no item {label:?}"))
            .id;
        self.controller.run_popup_choice(self.id(), popup, id);
    }
}

// ----- a new file opens at the top ---------------------------------------------------------------

#[test]
fn a_file_opened_for_the_first_time_shows_its_top_with_the_caret_at_the_start() {
    let long: String = (1..=80).map(|n| format!("Line number {n}\n\n")).collect();
    with_rig(&[("fresh.md", long.as_bytes())], |rig| {
        // Layout and a first paint (which would scroll to a caret that was not at the top).
        rig.paint();
        rig.controller.refresh_all();
        rig.paint();
        let window = rig.window();
        let view = window.view.borrow();
        let tab = view.active_tab().unwrap();
        assert_eq!(tab.entry.document().borrow().primary_selection().head, 0);
        assert_eq!(tab.editor.scroll_y(), 0.0);
        assert_eq!(
            view.statusbar().text(StatusSegment::Position),
            "Ln 1, Col 1"
        );
    });
}

// ----- the encoding menu -------------------------------------------------------------------------

/// "Привет" in Windows-1251.
const CYRILLIC_1251: &[u8] = b"\xCF\xF0\xE8\xE2\xE5\xF2\n";

#[test]
fn reopening_with_an_encoding_decodes_the_file_again_and_the_status_bar_follows() {
    with_rig(&[("cyr.md", CYRILLIC_1251)], |rig| {
        let popup = rig.status_popup(StatusSegment::Encoding);
        assert_eq!(
            submenus(&popup),
            [
                ("Reopen with encoding".to_owned(), true),
                ("Save with encoding".to_owned(), true)
            ]
        );
        let reopen = children(&popup, "Reopen with encoding");
        let names: Vec<&str> = reopen.iter().map(|item| item.label.as_str()).collect();
        assert!(
            names.contains(&"UTF-8") && names.contains(&"windows-1254"),
            "{names:?}"
        );

        rig.choose(&popup, "windows-1252", Some("Reopen with encoding"));
        assert_eq!(rig.text(), "Ïðèâåò\n");
        assert_eq!(rig.status(StatusSegment::Encoding), "windows-1252");
        assert!(!rig.entry().is_dirty());

        let popup = rig.status_popup(StatusSegment::Encoding);
        rig.choose(&popup, "windows-1251", Some("Reopen with encoding"));
        assert_eq!(rig.text(), "Привет\n");
        assert_eq!(rig.status(StatusSegment::Encoding), "windows-1251");
        let marked: Vec<String> = children(
            &rig.status_popup(StatusSegment::Encoding),
            "Reopen with encoding",
        )
        .iter()
        .filter(|item| item.check == CheckState::Radio)
        .map(|item| item.label.clone())
        .collect();
        assert_eq!(marked, ["windows-1251"]);
    });
}

#[test]
fn reopening_a_document_with_unsaved_changes_asks_before_discarding_them() {
    with_rig(&[("cyr.md", CYRILLIC_1251)], |rig| {
        replace_text(&mut rig.entry().document().borrow_mut(), "edited\n").unwrap();
        assert!(rig.entry().is_dirty());

        let popup = rig.status_popup(StatusSegment::Encoding);
        rig.answer(MessageBoxResult::No);
        rig.choose(&popup, "windows-1251", Some("Reopen with encoding"));
        assert_eq!(rig.text(), "edited\n", "declined: the edits stay");
        assert!(rig.entry().is_dirty());

        rig.answer(MessageBoxResult::Yes);
        rig.choose(&popup, "windows-1251", Some("Reopen with encoding"));
        assert_eq!(rig.text(), "Привет\n");
        assert!(!rig.entry().is_dirty());
        let asked = rig.services.dialog_log();
        assert_eq!(asked.len(), 2);
        assert!(asked[0].message.contains("cyr"), "{}", asked[0].message);
    });
}

#[test]
fn reopening_a_file_that_is_gone_reports_the_error_and_keeps_the_text() {
    with_rig(&[("cyr.md", CYRILLIC_1251)], |rig| {
        let popup = rig.status_popup(StatusSegment::Encoding);
        let before = rig.text();
        std::fs::remove_file(rig.work.join("cyr.md")).unwrap();
        rig.answer(MessageBoxResult::Ok);
        rig.choose(&popup, "windows-1252", Some("Reopen with encoding"));
        assert_eq!(rig.text(), before);
        assert_eq!(rig.services.dialog_log().len(), 1);
    });
}

#[test]
fn saving_with_an_encoding_writes_the_document_that_way() {
    with_rig(&[("note.md", b"plain\n")], |rig| {
        let popup = rig.status_popup(StatusSegment::Encoding);
        let save = children(&popup, "Save with encoding");
        let labels: Vec<&str> = save.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(
            &labels[..3],
            ["UTF-8", "UTF-8 with BOM", "UTF-16LE"],
            "{labels:?}"
        );
        // UTF-8 without a BOM is what the file has.
        assert_eq!(save[0].check, CheckState::Radio);
        assert_eq!(save[1].check, CheckState::None);

        rig.choose(&popup, "UTF-16LE", Some("Save with encoding"));
        assert!(rig.entry().is_dirty(), "the new encoding has to be written");
        assert!(rig.status(StatusSegment::Encoding).starts_with("UTF-16LE"));
        rig.command(CommandId::FileSave);
        let mut expected = vec![0xFF, 0xFE];
        expected.extend("plain\n".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(std::fs::read(rig.work.join("note.md")).unwrap(), expected);

        let popup = rig.status_popup(StatusSegment::Encoding);
        rig.choose(&popup, "UTF-8 with BOM", Some("Save with encoding"));
        rig.command(CommandId::FileSave);
        assert_eq!(
            std::fs::read(rig.work.join("note.md")).unwrap(),
            b"\xEF\xBB\xBFplain\n"
        );
        assert!(rig.status(StatusSegment::Encoding).ends_with("with BOM"));
    });
}

#[test]
fn the_encoding_menu_only_offers_what_the_document_can_do() {
    let docx = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/docx/mammoth_comments.docx"),
    )
    .unwrap();
    with_rig(&[("a.md", b"x\n"), ("b.docx", &docx)], |rig| {
        // An untitled buffer has no file to read again, but can be saved in any encoding.
        rig.command(CommandId::TabNew);
        let popup = rig.status_popup(StatusSegment::Encoding);
        assert_eq!(
            submenus(&popup),
            [
                ("Reopen with encoding".to_owned(), false),
                ("Save with encoding".to_owned(), true)
            ]
        );
        assert!(
            children(&popup, "Reopen with encoding")
                .iter()
                .all(|item| popup.action(item.id).is_none()),
            "a disabled submenu offers no actions"
        );

        // A document imported from Word is written by its converter: nothing to choose.
        rig.open("b.docx");
        rig.controller.refresh_all();
        let popup = rig.status_popup(StatusSegment::Encoding);
        assert_eq!(
            submenus(&popup),
            [
                ("Reopen with encoding".to_owned(), false),
                ("Save with encoding".to_owned(), false)
            ]
        );
        let eol = rig.status_popup(StatusSegment::LineEnding);
        assert!(items(&eol.model).iter().all(|item| !item.enabled));
        assert!(eol.ids().is_empty());
    });
}

// ----- the line-ending menu ----------------------------------------------------------------------

#[test]
fn the_line_ending_menu_marks_the_current_style_and_changes_what_is_saved() {
    with_rig(&[("eol.md", b"one\ntwo\n")], |rig| {
        assert_eq!(rig.status(StatusSegment::LineEnding), "LF");
        let popup = rig.status_popup(StatusSegment::LineEnding);
        let labels: Vec<(&str, CheckState)> = items(&popup.model)
            .iter()
            .map(|item| (item.label.as_str(), item.check))
            .collect();
        assert_eq!(
            labels,
            [
                ("LF", CheckState::Radio),
                ("CRLF", CheckState::None),
                ("CR", CheckState::None)
            ]
        );

        rig.choose(&popup, "CRLF", None);
        assert!(rig.entry().is_dirty());
        assert_eq!(rig.status(StatusSegment::LineEnding), "CRLF");
        assert_eq!(
            rig.text(),
            "one\ntwo\n",
            "the text keeps its \\n internally"
        );
        rig.command(CommandId::FileSave);
        assert_eq!(
            std::fs::read(rig.work.join("eol.md")).unwrap(),
            b"one\r\ntwo\r\n"
        );

        let popup = rig.status_popup(StatusSegment::LineEnding);
        rig.choose(&popup, "CR", None);
        rig.command(CommandId::FileSave);
        assert_eq!(
            std::fs::read(rig.work.join("eol.md")).unwrap(),
            b"one\rtwo\r"
        );
        assert_eq!(rig.status(StatusSegment::LineEnding), "CR");
    });
}

#[test]
fn the_other_status_segments_are_not_clickable() {
    with_rig(&[("a.md", b"one two\n")], |rig| {
        for segment in [
            StatusSegment::Position,
            StatusSegment::Words,
            StatusSegment::Zoom,
        ] {
            assert!(rig.click_status(segment).is_empty(), "{segment:?}");
        }
    });
}
