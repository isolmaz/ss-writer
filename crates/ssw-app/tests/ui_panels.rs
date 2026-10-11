//! The inspector panel and the context menus (spec §2.6, §3.1, §5, §6, §8): the real controller
//! over a started application in a temporary profile, with scripted dialogs and shell and an
//! in-memory clipboard. Nothing is shown: pointer and keyboard input is delivered to the window
//! content at the position of the widgets (found through their accessibility nodes) and what the
//! user would see is read back from the accessibility tree, the menus the loop owner would
//! show, the document text, the bytes on disk and the clipboard.
//!
//! Deleting moves files to the real Recycle Bin: those tests name their files with a token and
//! purge exactly the bin items carrying it afterwards (see `files_common`).

#[path = "../../ssw-io/tests/files_common/mod.rs"]
mod files_common;

use std::cell::RefCell;
use std::os::windows::fs::FileTimesExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use files_common::RecycleBinGuard;
use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::documents::{canonical_path, replace_text};
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::context_menus::{Action, Popup};
use ssw_app::ui::controller::{Controller, WindowCell};
use ssw_app::ui::inspector::{FeedEnv, file_info};
use ssw_app::ui::menus::menu_id;
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices, ShellRecord};
use ssw_app::ui::shell_view::ShellAction;
use ssw_app::workers::WindowId;
use ssw_core::command::CommandId;
use ssw_core::edit::Selection;
use ssw_core::markdown::ast::CalloutKind;
use ssw_core::ops::table::CellPos;
use ssw_core::text::TextRange;
use ssw_io::fs::RealFs;
use ssw_ui::access::{AccessNode, Role};
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::editor::EditorEvent;
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{
    Event, KeyEvent, Modifiers, PointerButton, PointerEvent, PointerEventKind, TextInput, vk,
};
use ssw_ui::services::{Clipboard, Clock, FakeClock, MemoryClipboard, MessageBoxResult};
use ssw_ui::shell::inspector::InspectorEvent;
use ssw_ui::shell::sidebar::{HeaderMenu, SidebarEvent};
use ssw_ui::text::MonoTextEngine;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::locale::Locale;
use ssw_win::menus::{CheckState, MenuEntry, MenuItem};
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

// ----- the controller without native windows ---------------------------------------------------

struct Rig {
    controller: Rc<Controller>,
    services: ScriptedServices,
    clipboard: Rc<RefCell<MemoryClipboard>>,
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
        let clipboard: SharedClipboard = memory.clone();
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let plan = core.plan.clone();
        let controller =
            Controller::new(core, services.dialogs(), services.shell(), clipboard, clock);
        controller.open_plan(&plan);
        let rig = Rig {
            controller,
            services,
            clipboard: memory,
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

/// Runs `body` over a started application whose first window shows `files[0]` (names may
/// contain folders).
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

fn labels(popup: &Popup) -> Vec<String> {
    popup
        .model
        .entries
        .iter()
        .map(|entry| match entry {
            MenuEntry::Item(item) => item.label.clone(),
            MenuEntry::Separator => "-".to_owned(),
            MenuEntry::Submenu(_) => "submenu".to_owned(),
        })
        .collect()
}

fn item<'a>(popup: &'a Popup, label: &str) -> &'a MenuItem {
    popup
        .model
        .entries
        .iter()
        .find_map(|entry| match entry {
            MenuEntry::Item(item) if item.label == label => Some(item),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no item {label:?} in {:?}", labels(popup)))
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

    fn tab_count(&self) -> usize {
        self.window().view.borrow().tabs().len()
    }

    fn active_path(&self) -> PathBuf {
        let window = self.window();
        let view = window.view.borrow();
        view.active_tab()
            .expect("a tab is open")
            .entry
            .path()
            .expect("a file document")
    }

    fn active_text(&self) -> String {
        let window = self.window();
        let view = window.view.borrow();
        view.active_tab()
            .expect("a tab is open")
            .entry
            .document()
            .borrow()
            .text()
    }

    fn caret(&self) -> usize {
        let window = self.window();
        let view = window.view.borrow();
        view.active_tab()
            .expect("a tab is open")
            .entry
            .document()
            .borrow()
            .primary_selection()
            .head
    }

    fn put_caret(&self, offset: usize) {
        let window = self.window();
        window
            .view
            .borrow_mut()
            .active_editor_mut()
            .expect("an editor")
            .go_to(Selection::caret(offset));
        self.controller.refresh_all();
    }

    fn open(&self, relative: &str) {
        self.controller
            .open_file(&self.window(), &self.work.join(relative))
            .unwrap();
        self.controller.refresh_all();
    }

    fn tab_key(&self) -> u64 {
        self.window().view.borrow().active_tab().unwrap().key
    }

    fn answer(&self, button: MessageBoxResult) {
        self.services
            .answer(DialogKind::MessageBox, Answer::Button(button));
    }

    fn clipboard_text(&self) -> Option<String> {
        self.clipboard.borrow().text().unwrap()
    }

    /// Delivers worker results until nothing is pending.
    fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.controller.core().borrow_mut().pump();
            self.controller.refresh_all();
            if self.controller.core().borrow().workers.pending() == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "the workers did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The accessibility nodes of the window after a layout.
    fn nodes(&self) -> Vec<AccessNode> {
        let window = self.window();
        let mut view = window.view.borrow_mut();
        view.paint(&mut RecordingCanvas::new(), &self.engine, false);
        view.access_tree(&self.engine, "SS Writer").nodes().to_vec()
    }

    fn by_role(&self, role: Role) -> Vec<AccessNode> {
        let mut nodes = self.nodes();
        nodes.retain(|node| node.role == role);
        nodes
    }

    fn node(&self, role: Role, name: &str) -> AccessNode {
        self.by_role(role)
            .into_iter()
            .find(|node| node.name == name)
            .unwrap_or_else(|| panic!("no {role:?} named {name:?}"))
    }

    /// Delivers `event`; popups the loop owner would show are returned, everything else is
    /// carried out like the loop owner does.
    fn deliver(&self, event: &Event) -> Vec<Popup> {
        let window = self.window();
        let actions = window.view.borrow_mut().event(event, &self.engine);
        let mut popups = Vec::new();
        for action in actions {
            match self.controller.popup_for(window.id, &action) {
                Some(popup) => popups.push(popup),
                None => self.controller.handle_action(window.id, action),
            }
        }
        popups
    }

    fn pointer(&self, button: PointerButton, at: Point) -> Vec<Popup> {
        let mut popups = Vec::new();
        for kind in [PointerEventKind::Down, PointerEventKind::Up] {
            let event = PointerEvent {
                click_count: 1,
                ..PointerEvent::mouse(kind, Some(button), at)
            };
            popups.extend(self.deliver(&Event::Pointer(event)));
        }
        popups
    }

    fn click(&self, at: Point) {
        assert!(self.pointer(PointerButton::Left, at).is_empty());
    }

    /// A right click; the popup it opens.
    fn right_click(&self, at: Point) -> Popup {
        self.pointer(PointerButton::Right, at)
            .into_iter()
            .next()
            .expect("the right click opens a popup")
    }

    fn choose(&self, popup: &Popup, label: &str) {
        self.controller
            .run_popup_choice(self.id(), popup, item(popup, label).id);
    }

    // ----- the sidebar ---------------------------------------------------------------------

    fn show_workspace(&self) {
        self.controller.show_workspace(&self.window(), &self.work);
        self.controller.refresh_all();
    }

    fn rows(&self) -> Vec<String> {
        self.nodes();
        self.window().view.borrow().sidebar().unwrap().row_names()
    }

    /// The key and the row center of the entry whose name (without extension) is `stem`.
    fn entry(&self, stem: &str) -> (u64, Point) {
        let rows = self.rows();
        let index = rows
            .iter()
            .position(|name| name.trim_end_matches(".md").trim_end_matches(".txt") == stem)
            .unwrap_or_else(|| panic!("no row {stem:?} in {rows:?}"));
        let window = self.window();
        let view = window.view.borrow();
        let sidebar = view.sidebar().unwrap();
        (
            sidebar.row_key(index).unwrap(),
            sidebar.row_rect(index).center(),
        )
    }

    fn sidebar_menu(&self, key: Option<u64>) -> Popup {
        let action = ShellAction::Sidebar(SidebarEvent::ContextMenu {
            key,
            at: Point::new(30.0, 90.0),
        });
        self.controller
            .popup_for(self.id(), &action)
            .expect("a sidebar menu")
    }

    fn renaming(&self) -> Option<u64> {
        self.nodes();
        self.window().view.borrow().sidebar().unwrap().renaming()
    }

    fn sidebar_event(&self, event: SidebarEvent) {
        self.controller
            .handle_action(self.id(), ShellAction::Sidebar(event));
    }

    // ----- the inspector -------------------------------------------------------------------

    fn show_inspector(&self) {
        self.command(CommandId::ViewToggleInspector);
    }

    fn headings(&self) -> Vec<AccessNode> {
        self.by_role(Role::TreeItem)
    }

    fn open_info_tab(&self) {
        let tab = self.node(Role::TabItem, "Info");
        self.click(tab.bounds.center());
    }

    /// The value shown for the Info row `label`.
    fn info(&self, label: &str) -> String {
        self.node(Role::ListItem, label)
            .value
            .unwrap_or_else(|| panic!("row {label:?} has no value"))
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The names in `dir`, for failure messages.
fn names_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![format!("<{} cannot be listed>", dir.display())];
    };
    entries
        .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
        .collect()
}

// ===== the inspector ===========================================================================

const DOC: &[u8] = b"# Alpha\n\nfirst words\n\n## Beta\n\nsecond words\n\n# Gamma\n\nthird\n";

#[test]
fn the_contents_tab_lists_the_headings_follows_the_caret_and_jumps_on_a_click() {
    with_rig(&[("notes.md", DOC)], |rig| {
        rig.show_inspector();
        let titles: Vec<String> = rig.headings().into_iter().map(|node| node.name).collect();
        assert_eq!(titles, ["Alpha", "Beta", "Gamma"]);
        assert!(rig.node(Role::TreeItem, "Alpha").state.selected);
        assert!(!rig.node(Role::TreeItem, "Gamma").state.selected);

        // The current section follows the caret.
        let text = String::from_utf8(DOC.to_vec()).unwrap();
        rig.put_caret(text.find("third").unwrap());
        assert!(rig.node(Role::TreeItem, "Gamma").state.selected);
        assert!(!rig.node(Role::TreeItem, "Alpha").state.selected);

        // A click moves the caret to the heading.
        let beta = rig.node(Role::TreeItem, "Beta");
        rig.click(beta.bounds.center());
        assert_eq!(rig.caret(), text.find("## Beta").unwrap());
        assert!(rig.node(Role::TreeItem, "Beta").state.selected);
    });
}

#[test]
fn unknown_headings_and_backlinks_do_not_move_anything() {
    with_rig(&[("notes.md", DOC)], |rig| {
        rig.show_inspector();
        let id = rig.id();
        // A heading the list does not know any more: the offset the panel sent is used.
        rig.controller.handle_action(
            id,
            ShellAction::Inspector(InspectorEvent::HeadingClicked {
                index: 99,
                offset: 3,
            }),
        );
        assert_eq!(rig.caret(), 3);
        rig.controller.handle_action(
            id,
            ShellAction::Inspector(InspectorEvent::BacklinkClicked(99)),
        );
        assert_eq!(rig.tab_count(), 1);
        // Tab and level changes are the panel's own business.
        rig.open_info_tab();
        assert_eq!(rig.tab_count(), 1);
    });
}

#[test]
fn plain_text_has_no_headings() {
    with_rig(&[("notes.md", DOC), ("plain.txt", DOC)], |rig| {
        rig.show_inspector();
        assert_eq!(rig.headings().len(), 3);
        rig.open("plain.txt");
        assert!(
            rig.headings().is_empty(),
            "# is not a heading in plain text"
        );
    });
}

#[test]
fn the_info_tab_shows_statistics_for_the_document_and_for_the_selection() {
    with_rig(&[("notes.md", b"one two three\n")], |rig| {
        rig.show_inspector();
        rig.open_info_tab();
        assert_eq!(rig.info("Words"), "3");
        assert_eq!(rig.info("Paragraphs"), "1");
        assert!(
            rig.by_role(Role::Text)
                .iter()
                .any(|node| node.name == "Document")
        );

        rig.command(CommandId::EditSelectAll);
        let sections: Vec<String> = rig
            .by_role(Role::Text)
            .into_iter()
            .map(|node| node.name)
            .collect();
        assert!(sections.contains(&"Selection".to_owned()), "{sections:?}");
        assert!(!sections.contains(&"Document".to_owned()), "{sections:?}");
        assert_eq!(rig.info("Words"), "3");
    });
}

#[test]
fn plain_text_statistics_count_the_text_as_it_is() {
    with_rig(
        &[("notes.md", b"x\n"), ("raw.txt", b"**one** two\n")],
        |rig| {
            rig.show_inspector();
            rig.open("raw.txt");
            rig.open_info_tab();
            assert_eq!(rig.info("Words"), "2");
            rig.command(CommandId::EditSelectAll);
            assert_eq!(rig.info("Words"), "2");
        },
    );
}

#[test]
fn the_info_tab_shows_the_file_details() {
    let notes = b"\xEF\xBB\xBFone\r\ntwo\r\n";
    with_rig(&[("notes.md", notes), ("cr.txt", b"a\rb\r")], |rig| {
        let path = rig.work.join("notes.md");
        let created = UNIX_EPOCH + Duration::from_secs(1_577_881_800);
        let modified = UNIX_EPOCH + Duration::from_secs(300_000_000_000);
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_created(created)
                .set_modified(modified),
        )
        .unwrap();
        drop(file);

        rig.show_inspector();
        rig.open_info_tab();
        let shown = canonical_path(&PathBuf::from(rig.info("Path")));
        assert_eq!(shown, canonical_path(&path));
        let locale = Locale::user_default();
        assert_eq!(
            rig.info("Size"),
            locale.format_file_size(notes.len() as u64)
        );
        assert_eq!(
            rig.info("Created"),
            locale.format_date_short(2020, 1, 1).unwrap()
        );
        // A date beyond the year 9999 cannot be shown.
        assert_eq!(rig.info("Modified"), "—");
        assert!(
            rig.info("Encoding").contains("BOM"),
            "{}",
            rig.info("Encoding")
        );
        assert_eq!(rig.info("Line endings"), "CRLF");

        rig.open("cr.txt");
        assert_eq!(rig.info("Line endings"), "CR");
        assert!(!rig.info("Encoding").contains("BOM"));
    });
}

#[test]
fn the_feed_environment_prints_what_it_was_built_from_and_dates_need_a_known_locale() {
    with_rig(&[("notes.md", b"words\n")], |rig| {
        let docs = rig.controller.docs();
        let store = docs.env().store.borrow();
        let locale = Locale::new("this is not a locale");
        let env = FeedEnv {
            i18n: rig.controller.i18n().as_ref(),
            locale: &locale,
            fs: &RealFs,
            store: &store,
            reading_wpm: 250,
            index_epoch: 3,
        };
        let shown = format!("{env:?}");
        assert!(shown.contains("reading_wpm: 250"), "{shown}");
        assert!(shown.contains("index_epoch: 3"), "{shown}");

        // The locale cannot format a date: the detail reads "unavailable" like a missing one.
        let window = rig.window();
        let view = window.view.borrow();
        let entry = &view.active_tab().unwrap().entry;
        let info = file_info(entry, &env).expect("a file document");
        assert_eq!(info.created, "—");
        assert_eq!(info.modified, "—");
        assert_eq!(info.line_endings, "LF");
    });
}

#[test]
fn a_file_that_vanished_shows_unavailable_details() {
    with_rig(&[("notes.md", b"some words\n")], |rig| {
        rig.show_inspector();
        rig.open_info_tab();
        assert_ne!(rig.info("Size"), "—");
        std::fs::remove_file(rig.work.join("notes.md")).unwrap();
        // A selection changes what the panel is fed from: the details are read again.
        rig.command(CommandId::EditSelectAll);
        for label in ["Size", "Created", "Modified"] {
            assert_eq!(rig.info(label), "—", "{label}");
        }
    });
}

#[test]
fn a_document_without_a_file_says_so_and_closing_every_tab_clears_the_panel() {
    with_rig(&[("notes.md", DOC)], |rig| {
        rig.show_inspector();
        assert_eq!(rig.headings().len(), 3);
        rig.command(CommandId::TabNew);
        rig.open_info_tab();
        let texts: Vec<String> = rig
            .by_role(Role::Text)
            .into_iter()
            .map(|node| node.name)
            .collect();
        assert!(texts.contains(&"No file".to_owned()), "{texts:?}");
        assert!(texts.contains(&"No backlinks".to_owned()), "{texts:?}");

        // A window without a tab (while a tab is replaced) empties the panel.
        let window = rig.window();
        let keys: Vec<u64> = window
            .view
            .borrow()
            .tabs()
            .iter()
            .map(|tab| tab.key)
            .collect();
        for key in keys {
            window.view.borrow_mut().close_tab(key);
        }
        rig.controller.refresh(&window);
        let texts: Vec<String> = rig
            .by_role(Role::Text)
            .into_iter()
            .map(|node| node.name)
            .collect();
        assert!(texts.contains(&"No file".to_owned()), "{texts:?}");
        assert!(rig.by_role(Role::Hyperlink).is_empty());
    });
}

#[test]
fn the_panel_is_fed_again_only_when_it_is_shown_again() {
    with_rig(&[("notes.md", DOC)], |rig| {
        rig.show_inspector();
        assert_eq!(rig.headings().len(), 3);
        rig.command(CommandId::ViewToggleInspector);
        rig.controller.refresh_all();
        assert!(
            rig.by_role(Role::TabItem)
                .iter()
                .all(|node| node.name != "Info")
        );
        rig.show_inspector();
        assert_eq!(rig.headings().len(), 3);
    });
}

#[test]
fn backlinks_come_from_the_workspace_index_and_open_the_linking_document() {
    let files: &[(&str, &[u8])] = &[
        ("b.md", b"# B\n"),
        ("a.md", b"# A\n\nsee [[b]]\n"),
        ("c.md", b"# C\n\nalso [[b]]\n"),
    ];
    with_rig(files, |rig| {
        rig.show_workspace();
        rig.open("b.md");
        // The core is busy: no index is started, and the next refresh starts it.
        let base = rig.controller.core().borrow().workers.pending();
        {
            let window = rig.window();
            window.view.borrow_mut().toggle_inspector();
            let _held = rig.controller.core().borrow_mut();
            rig.controller.refresh(&window);
        }
        assert_eq!(rig.controller.core().borrow().workers.pending(), base);
        rig.controller.refresh_all();
        assert_eq!(rig.controller.core().borrow().workers.pending(), base + 1);
        rig.settle();

        rig.open_info_tab();
        let texts: Vec<String> = rig
            .by_role(Role::Text)
            .into_iter()
            .map(|node| node.name)
            .collect();
        assert!(texts.contains(&"Backlinks (2)".to_owned()), "{texts:?}");
        let links = rig.by_role(Role::Hyperlink);
        let names: Vec<&str> = links.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(names, ["a", "c"]);
        assert_eq!(links[0].description, "a.md");

        let tabs = rig.tab_count();
        rig.click(links[0].bounds.center());
        assert_eq!(rig.tab_count(), tabs + 1);
        assert_eq!(
            canonical_path(&rig.active_path()),
            canonical_path(&rig.work.join("a.md"))
        );
    });
}

// ===== tab menu ================================================================================

#[test]
fn the_tab_menu_offers_what_the_tab_allows_and_runs_the_choice() {
    with_rig(&[("one.md", b"1\n"), ("two.md", b"2\n")], |rig| {
        // A single tab: nothing to close beside itself.
        let tab = rig
            .by_role(Role::TabItem)
            .into_iter()
            .find(|n| n.name.starts_with("one"))
            .unwrap();
        let popup = rig.right_click(tab.bounds.center());
        assert_eq!(
            labels(&popup),
            [
                "Close tab",
                "Close other tabs",
                "Close tabs to the right",
                "-",
                "Copy path",
                "Reveal in Explorer"
            ]
        );
        assert!(item(&popup, "Close tab").enabled);
        assert!(!item(&popup, "Close other tabs").enabled);
        assert!(!item(&popup, "Close tabs to the right").enabled);
        assert!(item(&popup, "Copy path").enabled);
        // A disabled item does nothing.
        assert_eq!(popup.action(item(&popup, "Close other tabs").id), None);

        rig.choose(&popup, "Copy path");
        let path = rig.active_path();
        assert_eq!(rig.clipboard_text(), Some(path_text(&path)));
        rig.choose(&popup, "Reveal in Explorer");
        assert_eq!(rig.services.shell_log(), [ShellRecord::Reveal(path)]);

        // Two tabs: the menu of the first one can close the others.
        rig.open("two.md");
        let first = rig
            .by_role(Role::TabItem)
            .into_iter()
            .find(|n| n.name.starts_with("one"))
            .unwrap();
        let popup = rig.right_click(first.bounds.center());
        assert!(item(&popup, "Close other tabs").enabled);
        assert!(item(&popup, "Close tabs to the right").enabled);
        rig.choose(&popup, "Close tabs to the right");
        assert_eq!(rig.tab_count(), 1);
        assert!(rig.active_path().ends_with("one.md"));
    });
}

#[test]
fn the_menu_of_a_tab_without_a_file_cannot_copy_or_reveal_a_path() {
    with_rig(&[("one.md", b"1\n")], |rig| {
        rig.command(CommandId::TabNew);
        let untitled = rig
            .by_role(Role::TabItem)
            .into_iter()
            .find(|n| n.name.starts_with("Untitled"))
            .unwrap();
        let popup = rig.right_click(untitled.bounds.center());
        assert!(!item(&popup, "Copy path").enabled);
        assert!(!item(&popup, "Reveal in Explorer").enabled);
        // The Untitled tab is the last one: the other tab can be closed, nothing is to its right.
        assert!(item(&popup, "Close other tabs").enabled);
        assert!(!item(&popup, "Close tabs to the right").enabled);
        assert_eq!(
            popup.ids(),
            [
                menu_id(CommandId::TabClose),
                menu_id(CommandId::TabCloseOthers)
            ]
        );
        // Close tab through the menu closes the tab the menu was opened on.
        rig.choose(&popup, "Close tab");
        assert_eq!(rig.tab_count(), 1);
        assert!(rig.active_path().ends_with("one.md"));
    });
}

#[test]
fn menus_for_unknown_windows_and_items_do_nothing() {
    with_rig(&[("one.md", b"1\n")], |rig| {
        let action = ShellAction::Sidebar(SidebarEvent::HeaderMenu {
            menu: HeaderMenu::New,
            anchor: Point::new(1.0, 2.0),
        });
        assert!(rig.controller.popup_for(WindowId(999), &action).is_none());
        let popup = rig.controller.popup_for(rig.id(), &action).unwrap();
        assert_eq!(popup.at, Point::new(1.0, 2.0));
        rig.controller
            .run_popup_choice(WindowId(999), &popup, popup.ids()[0]);
        rig.controller.run_popup_choice(rig.id(), &popup, 424_242);
        // No folder is shown: "New file here" has no folder to create the file in.
        rig.choose(&popup, "New file here");
        assert!(!rig.work.join("Untitled.md").exists());
        // Events that are not menus are not popups.
        let other = ShellAction::Sidebar(SidebarEvent::Selected(1));
        assert!(rig.controller.popup_for(rig.id(), &other).is_none());
    });
}

// ===== editor menus ============================================================================

#[test]
fn the_editor_menu_cuts_copies_pastes_and_selects() {
    with_rig(&[("notes.md", b"hello world\n")], |rig| {
        let at = Point::new(640.0, 400.0);
        let popup = rig.right_click(at);
        assert_eq!(labels(&popup), ["Cut", "Copy", "Paste", "-", "Select all"]);
        assert!(!item(&popup, "Cut").enabled && !item(&popup, "Copy").enabled);
        assert!(item(&popup, "Paste").enabled);

        rig.choose(&popup, "Select all");
        let popup = rig.right_click(at);
        assert!(item(&popup, "Cut").enabled && item(&popup, "Copy").enabled);
        rig.choose(&popup, "Copy");
        assert!(rig.clipboard_text().unwrap().contains("hello world"));
        assert_eq!(rig.active_text(), "hello world\n");

        rig.choose(&popup, "Cut");
        assert_eq!(rig.active_text(), "");
        let popup = rig.right_click(at);
        rig.choose(&popup, "Paste");
        assert!(rig.active_text().contains("hello world"));
    });
}

fn anchor() -> Rect {
    Rect::new(100.0, 100.0, 40.0, 18.0)
}

#[test]
fn a_spelling_menu_replaces_the_word_with_the_chosen_suggestion() {
    with_rig(&[("notes.md", b"I like teh weather\n")], |rig| {
        let key = rig.tab_key();
        let menu = |suggestions: Vec<String>| {
            ShellAction::Editor(
                key,
                EditorEvent::SpellMenu {
                    range: TextRange::new(7, 10),
                    word: "teh".to_owned(),
                    suggestions,
                    anchor: anchor(),
                },
            )
        };
        let none = rig
            .controller
            .popup_for(rig.id(), &menu(Vec::new()))
            .unwrap();
        assert_eq!(labels(&none), ["No spelling suggestions"]);
        assert!(!item(&none, "No spelling suggestions").enabled);
        assert!(none.ids().is_empty());
        assert_eq!(none.at, Point::new(100.0, 118.0));

        let popup = rig
            .controller
            .popup_for(rig.id(), &menu(vec!["the".to_owned(), "tea".to_owned()]))
            .unwrap();
        assert_eq!(labels(&popup), ["the", "tea"]);
        rig.choose(&popup, "the");
        assert_eq!(rig.active_text(), "I like the weather\n");
        assert_eq!(rig.caret(), 10);

        // The word was removed meanwhile: a suggestion for it changes nothing else.
        let entry = {
            let window = rig.window();
            let view = window.view.borrow();
            Rc::clone(&view.active_tab().unwrap().entry)
        };
        replace_text(&mut entry.document().borrow_mut(), "x");
        let stale = rig
            .controller
            .popup_for(rig.id(), &menu(vec!["tea".to_owned()]))
            .unwrap();
        rig.choose(&stale, "tea");
        assert_eq!(rig.active_text(), "x");
    });
}

#[test]
fn a_suggestion_chosen_in_a_window_without_a_document_replaces_nothing() {
    with_rig(&[("notes.md", b"I like teh weather\n")], |rig| {
        let popup = rig
            .controller
            .popup_for(
                rig.id(),
                &ShellAction::Editor(
                    rig.tab_key(),
                    EditorEvent::SpellMenu {
                        range: TextRange::new(7, 10),
                        word: "teh".to_owned(),
                        suggestions: vec!["the".to_owned()],
                        anchor: anchor(),
                    },
                ),
            )
            .unwrap();
        // The window the answer arrives in has no tab, so there is no editor to change.
        let bare = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        rig.controller
            .run_popup_choice(bare.id, &popup, item(&popup, "the").id);
        assert!(bare.view.borrow().tabs().is_empty());
        assert_eq!(rig.active_text(), "I like teh weather\n");
    });
}

#[test]
fn a_callout_menu_marks_the_current_type_and_changes_it() {
    let text = "> [!NOTE]\n> remember\n";
    with_rig(&[("notes.md", text.as_bytes())], |rig| {
        let key = rig.tab_key();
        let action = ShellAction::Editor(
            key,
            EditorEvent::CalloutMenu {
                range: TextRange::new(0, text.len()),
                kind: CalloutKind::Note,
                anchor: anchor(),
            },
        );
        let popup = rig.controller.popup_for(rig.id(), &action).unwrap();
        assert_eq!(labels(&popup).len(), 5);
        assert_eq!(item(&popup, "Note").check, CheckState::Radio);
        assert_eq!(item(&popup, "Warning").check, CheckState::None);
        rig.choose(&popup, "Warning");
        assert_eq!(rig.active_text(), "> [!WARNING]\n> remember\n");
    });
}

#[test]
fn a_callout_menu_in_a_code_block_changes_nothing() {
    let text = "```\ncode\n```\n";
    with_rig(&[("notes.md", text.as_bytes())], |rig| {
        let key = rig.tab_key();
        let action = ShellAction::Editor(
            key,
            EditorEvent::CalloutMenu {
                range: TextRange::new(5, 5),
                kind: CalloutKind::Tip,
                anchor: anchor(),
            },
        );
        let popup = rig.controller.popup_for(rig.id(), &action).unwrap();
        rig.choose(&popup, "Tip");
        assert_eq!(rig.active_text(), text);
    });
}

#[test]
fn the_table_menu_edits_the_table_around_the_caret() {
    let text = "| A | B |\n|---|---|\n| 1 | 2 |\n";
    with_rig(&[("notes.md", text.as_bytes())], |rig| {
        let key = rig.tab_key();
        let menu = |raw: bool| {
            ShellAction::Editor(
                key,
                EditorEvent::TableMenu {
                    anchor: anchor(),
                    cell: CellPos { row: 1, col: 0 },
                    raw,
                },
            )
        };
        let raw = rig.controller.popup_for(rig.id(), &menu(true)).unwrap();
        assert_eq!(labels(&raw), ["Edit table as raw Markdown"]);

        let popup = rig.controller.popup_for(rig.id(), &menu(false)).unwrap();
        let shown = labels(&popup);
        assert_eq!(shown.len(), 6 + 1 + 3 + 1 + 3 + 1 + 1, "{shown:?}");
        assert_eq!(shown[0], "Insert row above");
        assert_eq!(shown.last().unwrap(), "Edit table as raw Markdown");
        assert_eq!(popup.at, Point::new(100.0, 118.0));

        rig.put_caret(text.find('1').unwrap());
        let rows = |text: &str| text.lines().filter(|line| line.starts_with('|')).count();
        let before = rows(&rig.active_text());
        rig.choose(&popup, "Insert row below");
        assert_eq!(
            rows(&rig.active_text()),
            before + 1,
            "{}",
            rig.active_text()
        );
    });
}

// ===== sidebar menus ===========================================================================

const FILES: &[(&str, &[u8])] = &[
    ("notes.md", b"# Notes\n"),
    ("other.md", b"other text\n"),
    ("sub/inner.md", b"inner\n"),
];

#[test]
fn the_sidebar_menu_of_a_file_a_folder_and_the_root() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        let (_, center) = rig.entry("other");
        let file = rig.right_click(center);
        assert_eq!(
            labels(&file),
            [
                "Open",
                "Open in new tab",
                "Open in new window",
                "-",
                "Rename",
                "Duplicate",
                "Delete",
                "-",
                "Reveal in Explorer",
                "Copy path",
                "Copy relative path",
                "-",
                "New file here",
                "New folder here"
            ]
        );
        let (folder_key, _) = rig.entry("sub");
        let folder = rig.sidebar_menu(Some(folder_key));
        assert_eq!(
            labels(&folder),
            [
                "Rename",
                "Duplicate",
                "Delete",
                "-",
                "Reveal in Explorer",
                "Copy path",
                "Copy relative path",
                "-",
                "New file here",
                "New folder here"
            ]
        );
        let root = rig.sidebar_menu(None);
        assert_eq!(
            labels(&root),
            [
                "Reveal in Explorer",
                "Copy path",
                "-",
                "New file here",
                "New folder here"
            ]
        );
        assert!(matches!(
            root.action(item(&root, "Copy path").id),
            Some(Action::Sidebar { target: None, .. })
        ));
    });
}

#[test]
fn opening_from_the_sidebar_menu_uses_a_tab_or_a_new_window() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        let (_, center) = rig.entry("other");
        let popup = rig.right_click(center);
        rig.choose(&popup, "Open");
        assert_eq!(rig.tab_count(), 2);
        assert!(rig.active_path().ends_with("other.md"));
        rig.choose(&popup, "Open in new tab");
        assert_eq!(rig.tab_count(), 2, "the open tab is shown again");

        rig.choose(&popup, "Open in new window");
        let windows = rig.controller.windows();
        assert_eq!(windows.len(), 2);
        let fresh = windows[1].view.borrow();
        assert!(
            fresh
                .active_tab()
                .unwrap()
                .entry
                .path()
                .unwrap()
                .ends_with("other.md")
        );
    });
}

#[test]
fn a_file_gone_before_it_opens_in_a_new_window_is_reported_there() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        let (key, _) = rig.entry("other");
        let popup = rig.sidebar_menu(Some(key));
        std::fs::remove_file(rig.work.join("other.md")).unwrap();
        rig.answer(MessageBoxResult::Ok);
        rig.choose(&popup, "Open in new window");
        assert_eq!(rig.services.dialog_log().len(), 1);
        let windows = rig.controller.windows();
        assert_eq!(windows.len(), 2, "the new window stays, without the file");
        assert!(windows[1].view.borrow().active_tab().is_none_or(|tab| {
            !tab.entry
                .path()
                .is_some_and(|path| path.ends_with("other.md"))
        }));
    });
}

#[test]
fn an_open_file_moved_where_its_history_cannot_follow_keeps_its_tab_and_says_so() {
    with_rig(FILES, |rig| {
        rig.open("other.md");
        rig.show_workspace();
        let (other, _) = rig.entry("other");
        let (folder, _) = rig.entry("sub");
        // The version history cannot follow the file: its table is gone (a damaged database).
        let database = rig.controller.core().borrow().paths.state_db();
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch("DROP TABLE history_snapshots;")
            .unwrap();
        rig.answer(MessageBoxResult::Ok);
        rig.sidebar_event(SidebarEvent::Dropped {
            source: other,
            target_folder: folder,
            copy: false,
        });
        let target = rig.work.join("sub");
        assert!(
            target.join("other.md").exists(),
            "the folder holds {:?}",
            names_in(&target)
        );
        assert_eq!(rig.services.dialog_log().len(), 1);
        assert!(
            rig.active_path()
                .ends_with(Path::new("sub").join("other.md"))
        );
    });
}

#[test]
fn copying_and_revealing_paths_from_the_sidebar_menu() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        let (key, _) = rig.entry("other");
        let popup = rig.sidebar_menu(Some(key));
        let expected = rig.work.join("other.md");
        rig.choose(&popup, "Copy path");
        assert_eq!(
            canonical_path(&PathBuf::from(rig.clipboard_text().unwrap())),
            canonical_path(&expected)
        );
        rig.choose(&popup, "Copy relative path");
        assert_eq!(rig.clipboard_text(), Some("other.md".to_owned()));
        rig.choose(&popup, "Reveal in Explorer");
        let log = rig.services.shell_log();
        assert!(
            matches!(&log[..], [ShellRecord::Reveal(path)] if canonical_path(path) == canonical_path(&expected)),
            "{log:?}"
        );

        // The root.
        let root = rig.sidebar_menu(None);
        rig.choose(&root, "Copy path");
        assert_eq!(
            canonical_path(&PathBuf::from(rig.clipboard_text().unwrap())),
            canonical_path(&rig.work)
        );
    });
}

#[test]
fn duplicating_copies_the_file_next_to_it_and_a_missing_file_is_reported() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        let (key, _) = rig.entry("other");
        let popup = rig.sidebar_menu(Some(key));
        rig.choose(&popup, "Duplicate");
        let copy = rig.work.join("other copy.md");
        assert_eq!(std::fs::read(&copy).unwrap(), b"other text\n");
        assert!(rig.rows().iter().any(|row| row.starts_with("other copy")));

        std::fs::remove_file(rig.work.join("other.md")).unwrap();
        rig.answer(MessageBoxResult::Ok);
        rig.choose(&popup, "Duplicate");
        assert_eq!(rig.services.dialog_log().len(), 1);
        assert!(!rig.work.join("other copy 2.md").exists());
    });
}

#[test]
fn deleting_asks_first_and_moves_the_file_to_the_recycle_bin() {
    let bin = RecycleBinGuard::new();
    let doomed = bin.name("-delete-me.md");
    with_rig(&[("notes.md", b"keep\n"), (&doomed, b"bye\n")], |rig| {
        rig.show_workspace();
        let stem = doomed.trim_end_matches(".md");
        let (key, _) = rig.entry(stem);
        let popup = rig.sidebar_menu(Some(key));

        // No: nothing happens.
        rig.answer(MessageBoxResult::No);
        rig.choose(&popup, "Delete");
        assert!(rig.work.join(&doomed).exists());
        let asked = rig.services.dialog_log();
        assert_eq!(asked[0].title, "Delete");
        assert!(asked[0].message.contains(&doomed), "{}", asked[0].message);

        // The Delete key asks the same question.
        rig.answer(MessageBoxResult::No);
        rig.sidebar_event(SidebarEvent::DeleteRequested(key));
        assert!(rig.work.join(&doomed).exists());

        // Yes: the file is gone from the folder and from the list.
        rig.answer(MessageBoxResult::Yes);
        rig.choose(&popup, "Delete");
        assert!(!rig.work.join(&doomed).exists());
        assert!(rig.rows().iter().all(|row| !row.contains("delete-me")));
        assert_eq!(bin.purge(), 1, "it went to the Recycle Bin");
    });
}

#[test]
fn a_file_that_cannot_be_recycled_is_reported() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        let (key, _) = rig.entry("other");
        let popup = rig.sidebar_menu(Some(key));
        std::fs::remove_file(rig.work.join("other.md")).unwrap();
        rig.answer(MessageBoxResult::Yes);
        rig.answer(MessageBoxResult::Ok);
        rig.choose(&popup, "Delete");
        assert_eq!(rig.services.dialog_log().len(), 2);
    });
}

#[test]
fn new_files_and_folders_are_created_and_offered_for_renaming() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        // In the root.
        let root = rig.sidebar_menu(None);
        rig.choose(&root, "New file here");
        assert_eq!(std::fs::read(rig.work.join("Untitled.md")).unwrap(), b"");
        let (key, _) = rig.entry("Untitled");
        assert_eq!(rig.renaming(), Some(key));
        assert_eq!(
            rig.window().view.borrow().sidebar().unwrap().selected_key(),
            Some(key)
        );

        // Next to a file: in its folder, with the next free name.
        let (other, _) = rig.entry("other");
        rig.choose(&rig.sidebar_menu(Some(other)), "New file here");
        assert!(rig.work.join("Untitled 2.md").exists());

        // A folder.
        rig.choose(&root, "New folder here");
        assert!(rig.work.join("New folder").is_dir());
        rig.choose(&root, "New folder here");
        assert!(rig.work.join("New folder 2").is_dir());
        let header = ShellAction::Sidebar(SidebarEvent::HeaderMenu {
            menu: HeaderMenu::New,
            anchor: Point::new(10.0, 10.0),
        });
        let popup = rig.controller.popup_for(rig.id(), &header).unwrap();
        assert_eq!(labels(&popup), ["New file here", "New folder here"]);
    });
}

#[test]
fn a_file_created_in_a_folder_that_is_not_listed_opens_in_a_tab() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        let (sub, _) = rig.entry("sub");
        let popup = rig.sidebar_menu(Some(sub));
        rig.choose(&popup, "New file here");
        assert!(rig.work.join("sub").join("Untitled.md").exists());
        assert_eq!(rig.tab_count(), 2);
        assert!(rig.active_path().ends_with("Untitled.md"));
        // A folder in the unlisted folder has nothing to show.
        rig.choose(&popup, "New folder here");
        assert!(rig.work.join("sub").join("New folder").is_dir());
        assert_eq!(rig.tab_count(), 2);
    });
}

#[test]
fn creating_in_a_folder_that_vanished_is_reported() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        let (sub, _) = rig.entry("sub");
        let popup = rig.sidebar_menu(Some(sub));
        std::fs::remove_dir_all(rig.work.join("sub")).unwrap();
        rig.answer(MessageBoxResult::Ok);
        rig.choose(&popup, "New file here");
        rig.answer(MessageBoxResult::Ok);
        rig.choose(&popup, "New folder here");
        assert_eq!(rig.services.dialog_log().len(), 2);
    });
}

#[test]
fn the_header_menus_collapse_and_expand_every_folder() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        let (sub, _) = rig.entry("sub");
        let options = ShellAction::Sidebar(SidebarEvent::HeaderMenu {
            menu: HeaderMenu::Options,
            anchor: Point::new(10.0, 10.0),
        });
        let popup = rig.controller.popup_for(rig.id(), &options).unwrap();
        assert_eq!(labels(&popup), ["Collapse all", "Expand all"]);
        let expanded = |rig: &Rig| {
            rig.window()
                .view
                .borrow()
                .sidebar()
                .unwrap()
                .is_expanded(sub)
        };
        rig.choose(&popup, "Expand all");
        assert_eq!(expanded(rig), Some(true));
        rig.choose(&popup, "Collapse all");
        assert_eq!(expanded(rig), Some(false));
        // The workspace closed while the menu was open: nothing is left to fold.
        rig.window().view.borrow_mut().set_workspace(None);
        rig.choose(&popup, "Expand all");
        assert!(rig.window().view.borrow().sidebar().is_none());
    });
}

// ===== renaming, dropping ======================================================================

#[test]
fn renaming_from_the_menu_with_the_keyboard_renames_the_file_and_its_open_tab() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        rig.open("other.md");
        let (key, _) = rig.entry("other");
        rig.choose(&rig.sidebar_menu(Some(key)), "Rename");
        assert_eq!(rig.renaming(), Some(key));

        // The name is selected up to the extension: typing replaces it.
        assert!(
            rig.deliver(&Event::Text(TextInput("renamed".to_owned())))
                .is_empty()
        );
        assert!(
            rig.deliver(&Event::Key(KeyEvent::press(vk::RETURN, Modifiers::NONE)))
                .is_empty()
        );
        assert!(!rig.work.join("other.md").exists());
        assert_eq!(
            std::fs::read(rig.work.join("renamed.md")).unwrap(),
            b"other text\n"
        );
        assert_eq!(rig.renaming(), None);
        assert!(rig.rows().iter().any(|row| row.starts_with("renamed")));
        // The open document follows its file.
        assert!(rig.active_path().ends_with("renamed.md"));
    });
}

#[test]
fn renaming_a_folder_moves_the_documents_inside_it() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        rig.open("sub/inner.md");
        let (key, _) = rig.entry("sub");
        rig.sidebar_event(SidebarEvent::RenameCommitted {
            key,
            name: "deeper".to_owned(),
        });
        assert!(rig.work.join("deeper").join("inner.md").exists());
        assert!(
            canonical_path(&rig.active_path()).ends_with("deeper\\inner.md"),
            "{:?}",
            rig.active_path()
        );
        // An entry the tree does not know is ignored.
        rig.sidebar_event(SidebarEvent::RenameCommitted {
            key: 9_999,
            name: "x.md".to_owned(),
        });
        assert!(!rig.work.join("x.md").exists());
    });
}

#[test]
fn a_rename_that_cannot_be_done_explains_why() {
    with_rig(FILES, |rig| {
        rig.show_workspace();
        let (key, _) = rig.entry("other");
        let long = "a".repeat(300);
        let cases: [(&str, &str); 6] = [
            ("", "cannot be empty"),
            (&long, "too long"),
            ("a/b.md", "cannot contain"),
            ("CON", "reserves"),
            ("name.", "cannot end with a dot or a space"),
            ("notes.md", "SS Writer"),
        ];
        for (name, expected) in cases {
            rig.answer(MessageBoxResult::Ok);
            rig.sidebar_event(SidebarEvent::RenameCommitted {
                key,
                name: name.to_owned(),
            });
            let shown = rig.services.dialog_log();
            let last = shown.last().unwrap();
            assert!(
                last.message.contains(expected) || last.title.contains(expected),
                "{name:?}: {last:?}"
            );
        }
        assert!(rig.work.join("other.md").exists());
        // Cancelling and selecting are not file operations.
        rig.sidebar_event(SidebarEvent::RenameCancelled(key));
        rig.sidebar_event(SidebarEvent::Selected(key));
        assert_eq!(rig.services.dialog_log().len(), 6);
    });
}

#[test]
fn dropping_moves_a_file_and_never_a_folder_into_itself() {
    with_rig(&[("notes.md", b"n\n"), ("sub/deep/x.md", b"x\n")], |rig| {
        rig.show_workspace();
        rig.window()
            .view
            .borrow_mut()
            .sidebar_mut()
            .unwrap()
            .expand_all();
        let (notes, _) = rig.entry("notes");
        let (sub, _) = rig.entry("sub");
        let (deep, _) = rig.entry("deep");
        rig.sidebar_event(SidebarEvent::Dropped {
            source: notes,
            target_folder: sub,
            copy: false,
        });
        assert!(rig.work.join("sub").join("notes.md").exists());
        assert!(!rig.work.join("notes.md").exists());

        rig.answer(MessageBoxResult::Ok);
        rig.sidebar_event(SidebarEvent::Dropped {
            source: sub,
            target_folder: deep,
            copy: false,
        });
        let shown = rig.services.dialog_log();
        assert!(
            shown[0].message.contains("into itself"),
            "{}",
            shown[0].message
        );
        assert!(rig.work.join("sub").join("deep").join("x.md").exists());
    });
}
