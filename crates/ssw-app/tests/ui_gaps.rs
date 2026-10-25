//! The status bar menus (spec §4.2, §8: encoding, line endings, language) and the first look at a
//! new file (spec §6): the real controller over a started application in a temporary profile,
//! with scripted dialogs and shell. Nothing is shown: pointer input is delivered to the window
//! content at the position of the status bar segments, and what the user would see is read back
//! from the menus the loop owner would show, the status bar texts, the document text and the
//! bytes on disk.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::documents::replace_text;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::context_menus::Popup;
use ssw_app::ui::controller::{Controller, WindowCell};
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices};
use ssw_app::ui::shell_view::ShellAction;
use ssw_app::workers::WindowId;
use ssw_core::command::CommandId;
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::editor::highlight::Language;
use ssw_ui::geometry::Size;
use ssw_ui::input::{Event, PointerButton, PointerEvent, PointerEventKind};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard, MessageBoxResult, TimerId};
use ssw_ui::shell::sidebar::SidebarEvent;
use ssw_ui::shell::statusbar::StatusSegment;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::widget::TimerRequest;
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
    clock: Rc<FakeClock>,
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
        let fake = Rc::new(FakeClock::default());
        let clock: Rc<dyn Clock> = fake.clone();
        let plan = core.plan.clone();
        let controller =
            Controller::new(core, services.dialogs(), services.shell(), clipboard, clock);
        controller.open_plan(&plan);
        let rig = Rig {
            controller,
            services,
            clock: fake,
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
        replace_text(&mut rig.entry().document().borrow_mut(), "edited\n");
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
fn the_word_count_follows_the_text_once_typing_pauses() {
    with_rig(&[("a.md", b"one two\n")], |rig| {
        assert_eq!(rig.status(StatusSegment::Words), "2 words");
        let entry = rig.entry();
        replace_text(&mut entry.document().borrow_mut(), "one two three\n");
        // Counted a moment ago: the previous count stays, and a timer brings it up to date.
        assert_eq!(rig.status(StatusSegment::Words), "2 words");
        let window = rig.window();
        let mut view = window.view.borrow_mut();
        let timer = view
            .requests_mut()
            .timers()
            .iter()
            .find_map(|timer| match timer {
                TimerRequest::Set {
                    id,
                    interval,
                    repeat: false,
                } if *interval == Duration::from_millis(500) => Some(*id),
                _ => None,
            })
            .expect("a timer for the next count");
        // Another timer before the interval ends changes nothing.
        view.event(&Event::Timer(TimerId::allocate()), &rig.engine);
        assert_eq!(view.statusbar().text(StatusSegment::Words), "2 words");
        view.requests_mut().take_invalidation();
        rig.clock.advance(Duration::from_millis(500));
        view.event(&Event::Timer(timer), &rig.engine);
        assert_eq!(view.statusbar().text(StatusSegment::Words), "3 words");
        assert!(
            !view.requests_mut().take_invalidation().is_clean(),
            "the new count is painted"
        );
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

// ----- the language menu -------------------------------------------------------------------------

impl Rig {
    /// The language the active tab colours its document as.
    fn language(&self) -> Option<Language> {
        let window = self.window();
        let view = window.view.borrow();
        view.active_tab()
            .expect("a tab is open")
            .editor
            .config()
            .plain_language
    }

    /// Makes the tab at `index` the active one.
    fn activate(&self, index: usize) {
        let window = self.window();
        let key = window.view.borrow().tabs()[index].key;
        window.view.borrow_mut().activate_tab(key);
        self.controller.refresh_all();
    }

    /// Closes the active tab; its document is gone from the registry with it.
    fn close_active_tab(&self) {
        let path = self.entry().path().expect("a file is open");
        self.command(CommandId::TabClose);
        assert!(self.controller.docs().find(&path).is_none(), "{path:?}");
    }

    /// Chooses the item `label` of `popup` in window `window`, which need not be the window the
    /// menu was opened in.
    fn choose_in(&self, window: WindowId, popup: &Popup, label: &str) {
        let id = items(&popup.model)
            .into_iter()
            .find(|item| item.label == label)
            .unwrap_or_else(|| panic!("no item {label:?}"))
            .id;
        self.controller.run_popup_choice(window, popup, id);
    }
}

/// The labels of the items of `popup` that carry a mark.
fn marked(popup: &Popup) -> Vec<String> {
    items(&popup.model)
        .iter()
        .filter(|item| item.check == CheckState::Radio)
        .map(|item| item.label.clone())
        .collect()
}

#[test]
fn the_language_segment_is_shown_for_plain_text_documents_only() {
    let files: [(&str, &[u8]); 3] = [
        ("page.md", b"# Title\n"),
        ("data.json", b"{\"a\": 1}\n"),
        ("notes.txt", b"hello\n"),
    ];
    with_rig(&files, |rig| {
        // A Markdown document has no language; the segment takes no room.
        assert_eq!(rig.status(StatusSegment::Language), "");
        let rect = rig
            .window()
            .view
            .borrow()
            .statusbar()
            .segment_rect(StatusSegment::Language);
        assert_eq!(rect.width, 0.0);

        rig.open("data.json");
        assert_eq!(rig.status(StatusSegment::Language), "JSON");
        rig.open("notes.txt");
        assert_eq!(rig.status(StatusSegment::Language), "Plain text");
        rig.activate(0);
        assert_eq!(rig.status(StatusSegment::Language), "");

        // Showing a plain file as Markdown takes the language away, showing it as text brings
        // it back.
        rig.activate(1);
        rig.command(CommandId::ViewRenderAsMarkdown);
        assert_eq!(rig.status(StatusSegment::Language), "");
        rig.command(CommandId::ViewRenderAsMarkdown);
        assert_eq!(rig.status(StatusSegment::Language), "JSON");

        // No file, no language; no document, no segment.
        rig.command(CommandId::TabNew);
        assert_eq!(rig.status(StatusSegment::Language), "");
    });
}

#[test]
fn the_language_menu_lists_automatic_plain_text_and_every_language_with_the_choice_marked() {
    let files: [(&str, &[u8]); 2] = [("data.json", b"{}\n"), ("notes.txt", b"hello\n")];
    with_rig(&files, |rig| {
        let popup = rig.status_popup(StatusSegment::Language);
        let labels: Vec<&str> = items(&popup.model)
            .iter()
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(labels.len(), 2 + Language::ALL.len());
        assert_eq!(labels.len(), 22);
        assert_eq!(labels[0], "Automatic");
        assert_eq!(labels[1], "Plain text");
        let mut sorted = labels[2..].to_vec();
        sorted.sort_by_key(|label| label.to_lowercase());
        assert_eq!(
            labels[2..],
            sorted,
            "the languages are in the order of their names"
        );
        for readable in [
            "C#",
            "C++",
            "JavaScript",
            "PowerShell",
            "JSON",
            "YAML",
            "TOML",
            "HTML",
            "XML",
            "CSS",
            "SQL",
            "Markdown",
            "Diff",
        ] {
            assert!(labels.contains(&readable), "{readable}");
        }
        assert!(
            items(&popup.model)
                .iter()
                .all(|item| popup.action(item.id).is_some()),
            "every entry can be chosen"
        );
        // Nothing was chosen: "Automatic" is the current entry, whatever the file is detected as.
        assert_eq!(marked(&popup), ["Automatic"]);
        assert_eq!(rig.status(StatusSegment::Language), "JSON");

        rig.open("notes.txt");
        assert_eq!(
            marked(&rig.status_popup(StatusSegment::Language)),
            ["Automatic"]
        );
        assert_eq!(rig.status(StatusSegment::Language), "Plain text");
        let popup = rig.status_popup(StatusSegment::Language);
        rig.choose(&popup, "Plain text", None);
        assert_eq!(
            marked(&rig.status_popup(StatusSegment::Language)),
            ["Plain text"]
        );
    });
}

#[test]
fn choosing_automatic_gives_the_file_back_to_detection_for_good() {
    let files: [(&str, &[u8]); 2] = [("keep.md", b"# Keep\n"), ("data.json", b"{\"a\": 1}\n")];
    with_rig(&files, |rig| {
        rig.open("data.json");
        assert_eq!(rig.language(), Some(Language::Json));
        assert_eq!(
            marked(&rig.status_popup(StatusSegment::Language)),
            ["Automatic"]
        );

        // A wrong pick is stored and marked.
        let popup = rig.status_popup(StatusSegment::Language);
        rig.choose(&popup, "Python", None);
        assert_eq!(rig.language(), Some(Language::Python));
        assert_eq!(rig.status(StatusSegment::Language), "Python");
        assert_eq!(rig.entry().language_choice(), Some(Some(Language::Python)));
        assert_eq!(
            marked(&rig.status_popup(StatusSegment::Language)),
            ["Python"]
        );
        // The choice is stored: it is back after a close and reopen.
        rig.close_active_tab();
        rig.open("data.json");
        assert_eq!(rig.language(), Some(Language::Python));
        assert_eq!(
            marked(&rig.status_popup(StatusSegment::Language)),
            ["Python"]
        );

        // "Automatic" deletes the row and detection colours the file as JSON at once; the
        // segment shows that language, never the word "Automatic".
        let popup = rig.status_popup(StatusSegment::Language);
        rig.choose(&popup, "Automatic", None);
        assert_eq!(rig.entry().language_choice(), None);
        assert_eq!(rig.language(), Some(Language::Json), "no refresh needed");
        assert_eq!(rig.status(StatusSegment::Language), "JSON");
        assert_eq!(
            marked(&rig.status_popup(StatusSegment::Language)),
            ["Automatic"]
        );

        // Closed and opened again, the stored row is gone and the file is still automatic.
        rig.close_active_tab();
        rig.open("data.json");
        assert_eq!(rig.entry().language_choice(), None);
        assert_eq!(rig.language(), Some(Language::Json));
        assert_eq!(rig.status(StatusSegment::Language), "JSON");
        assert_eq!(
            marked(&rig.status_popup(StatusSegment::Language)),
            ["Automatic"]
        );
    });
}

#[test]
fn automatic_takes_a_stored_plain_text_choice_back_too() {
    with_rig(&[("a.md", b"# A\n"), ("data.json", b"{}\n")], |rig| {
        rig.open("data.json");
        let popup = rig.status_popup(StatusSegment::Language);
        rig.choose(&popup, "Plain text", None);
        assert_eq!(rig.language(), None);
        assert_eq!(rig.status(StatusSegment::Language), "Plain text");
        let popup = rig.status_popup(StatusSegment::Language);
        rig.choose(&popup, "Automatic", None);
        assert_eq!(rig.language(), Some(Language::Json));
        assert_eq!(rig.status(StatusSegment::Language), "JSON");
        assert_eq!(
            marked(&rig.status_popup(StatusSegment::Language)),
            ["Automatic"]
        );
    });
}

#[test]
fn choosing_a_language_colours_the_document_at_once_and_the_file_keeps_it() {
    let files: [(&str, &[u8]); 2] = [("keep.md", b"# Keep\n"), ("notes.txt", b"print('hi')\n")];
    with_rig(&files, |rig| {
        rig.open("notes.txt");
        assert_eq!(rig.language(), None);
        assert_eq!(rig.entry().language_choice(), None);

        let popup = rig.status_popup(StatusSegment::Language);
        rig.choose(&popup, "Python", None);
        assert_eq!(rig.language(), Some(Language::Python), "no refresh needed");
        assert_eq!(rig.status(StatusSegment::Language), "Python");
        assert_eq!(rig.entry().language_choice(), Some(Some(Language::Python)));
        assert_eq!(
            marked(&rig.status_popup(StatusSegment::Language)),
            ["Python"]
        );

        // Opened again, the file still shows Python.
        rig.close_active_tab();
        rig.open("notes.txt");
        assert_eq!(rig.language(), Some(Language::Python));
        assert_eq!(rig.status(StatusSegment::Language), "Python");

        // "Plain text" takes the colors away, and that is remembered as well.
        let popup = rig.status_popup(StatusSegment::Language);
        rig.choose(&popup, "Plain text", None);
        assert_eq!(rig.language(), None);
        assert_eq!(rig.status(StatusSegment::Language), "Plain text");
        rig.close_active_tab();
        rig.open("notes.txt");
        assert_eq!(rig.language(), None);
        assert_eq!(rig.status(StatusSegment::Language), "Plain text");
        assert_eq!(rig.entry().language_choice(), Some(None));
    });
}

#[test]
fn a_stored_choice_beats_detection_even_when_it_names_the_detected_language() {
    let files: [(&str, &[u8]); 2] = [("keep.md", b"# Keep\n"), ("data.json", b"{\"a\": 1}\n")];
    with_rig(&files, |rig| {
        rig.open("data.json");
        assert_eq!(rig.language(), Some(Language::Json), "detected");
        assert_eq!(rig.entry().language_choice(), None, "nothing chosen yet");

        // Plain text for a JSON file: later refreshes and reopening keep it.
        let popup = rig.status_popup(StatusSegment::Language);
        rig.choose(&popup, "Plain text", None);
        rig.controller.refresh_all();
        assert_eq!(rig.language(), None);
        rig.close_active_tab();
        rig.open("data.json");
        assert_eq!(rig.language(), None);
        assert_eq!(rig.status(StatusSegment::Language), "Plain text");

        // Choosing what detection says anyway is a choice too: it is stored.
        let popup = rig.status_popup(StatusSegment::Language);
        rig.choose(&popup, "JSON", None);
        assert_eq!(rig.language(), Some(Language::Json));
        rig.close_active_tab();
        rig.open("data.json");
        assert_eq!(rig.language(), Some(Language::Json));
        assert_eq!(rig.entry().language_choice(), Some(Some(Language::Json)));
    });
}

#[test]
fn choosing_a_language_in_a_window_without_a_tab_changes_nothing() {
    with_rig(&[("data.json", b"{\"a\": 1}\n")], |rig| {
        let popup = rig.status_popup(StatusSegment::Language);
        // A window without any tab has no document to colour, whatever menu is chosen in it.
        let empty = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        assert!(empty.view.borrow().tabs().is_empty());
        let window = rig.window();
        window.dirty.set(false);
        let _ = window.view.borrow_mut().requests_mut().take_invalidation();

        for label in ["Python", "Plain text", "Automatic"] {
            rig.choose_in(empty.id, &popup, label);
            assert!(empty.view.borrow().tabs().is_empty(), "{label}");
        }
        // The first window is not repainted and its document is still detected as before.
        assert!(!window.dirty.get());
        let repaints = window.view.borrow_mut().requests_mut().take_invalidation();
        assert!(repaints.is_clean());
        assert_eq!(rig.entry().language_choice(), None);
        assert_eq!(rig.language(), Some(Language::Json));
        assert_eq!(rig.status(StatusSegment::Language), "JSON");
        assert_eq!(
            marked(&rig.status_popup(StatusSegment::Language)),
            ["Automatic"]
        );
    });
}

// ----- the choice follows the file ---------------------------------------------------------------

impl Rig {
    /// Renames the file `from` (relative to the work folder) to `name` the way the sidebar's
    /// rename box does, with the work folder shown in the sidebar.
    fn rename_in_sidebar(&self, from: &str, name: &str) {
        let window = self.window();
        self.controller.show_workspace(&window, &self.work);
        window
            .view
            .borrow_mut()
            .sidebar_mut()
            .expect("the sidebar exists")
            .expand_all();
        self.controller.refresh_all();
        let wanted = self.work.join(from).to_string_lossy().to_lowercase();
        let key = {
            let tree = window.tree.borrow();
            let paths = tree.as_ref().expect("a workspace is shown");
            (0..)
                .map_while(|key| paths.path(key).map(|found| (key, found)))
                .find(|(_, found)| found.to_string_lossy().to_lowercase() == wanted)
                .map(|(key, _)| key)
                .unwrap_or_else(|| panic!("no sidebar entry for {from}"))
        };
        self.controller.handle_action(
            self.id(),
            ShellAction::Sidebar(SidebarEvent::RenameCommitted {
                key,
                name: name.to_owned(),
            }),
        );
        self.controller.refresh_all();
    }
}

#[test]
fn renaming_a_file_in_the_sidebar_keeps_its_language_and_leaves_nothing_at_the_old_name() {
    let files: [(&str, &[u8]); 2] = [("keep.md", b"# Keep\n"), ("notes.txt", b"print('hi')\n")];
    with_rig(&files, |rig| {
        rig.open("notes.txt");
        let popup = rig.status_popup(StatusSegment::Language);
        rig.choose(&popup, "Python", None);

        rig.rename_in_sidebar("notes.txt", "renamed.txt");
        assert!(rig.work.join("renamed.txt").exists());
        assert!(rig.entry().path().unwrap().ends_with("renamed.txt"));
        assert_eq!(rig.language(), Some(Language::Python));
        assert_eq!(rig.status(StatusSegment::Language), "Python");

        // Closed and opened again under the new name, the file still shows Python.
        rig.close_active_tab();
        rig.open("renamed.txt");
        assert_eq!(rig.language(), Some(Language::Python));
        assert_eq!(rig.entry().language_choice(), Some(Some(Language::Python)));

        // A file that is created at the old name later is another file: no language.
        std::fs::write(rig.work.join("notes.txt"), "print('hi')\n").unwrap();
        rig.open("notes.txt");
        assert_eq!(rig.entry().language_choice(), None);
        assert_eq!(rig.language(), None);
    });
}

#[test]
fn saving_a_file_under_another_name_gives_the_new_file_the_language_too() {
    let files: [(&str, &[u8]); 2] = [("keep.md", b"# Keep\n"), ("notes.txt", b"print('hi')\n")];
    with_rig(&files, |rig| {
        rig.open("notes.txt");
        let popup = rig.status_popup(StatusSegment::Language);
        rig.choose(&popup, "Python", None);

        let copy = rig.work.join("copy.txt");
        rig.services
            .answer(DialogKind::SaveFile, Answer::Paths(vec![copy.clone()]));
        rig.command(CommandId::FileSaveAs);
        assert!(copy.exists());
        assert!(rig.entry().path().unwrap().ends_with("copy.txt"));
        assert_eq!(rig.language(), Some(Language::Python));

        // Opened again, the copy is Python: the choice was stored under its name.
        rig.close_active_tab();
        rig.open("copy.txt");
        assert_eq!(rig.language(), Some(Language::Python));
        assert_eq!(rig.entry().language_choice(), Some(Some(Language::Python)));
        // The file it was saved from is still there, and keeps its own choice.
        rig.close_active_tab();
        rig.open("notes.txt");
        assert_eq!(rig.language(), Some(Language::Python));
    });
}
