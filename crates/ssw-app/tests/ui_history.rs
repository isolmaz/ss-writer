//! The version history panel and the banner flows (spec §3.3, §4.3, §4.5, §4.6): the real
//! controller over a real startup in a temporary profile, with scripted dialogs. Nothing is
//! shown: input is delivered to the window content as pointer events at the position of the
//! widgets (found through their accessibility nodes) and what the user would see is read back
//! from the accessibility tree, the document text, the bytes on disk and the history database.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::commands::Dispatch;
use ssw_app::documents::{DocEntry, ExternalState, canonical_path, loss_message_id, replace_text};
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::{CloseOutcome, Controller, Request, WindowCell};
use ssw_app::ui::history::DISK_ENTRY_ID;
use ssw_app::ui::host::{UiApp, UiOptions, UiServices};
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices};
use ssw_app::ui::shell_view::ShellAction;
use ssw_app::workers::WindowId;
use ssw_core::command::CommandId;
use ssw_io::encoding::TextFormat;
use ssw_io::history::{SnapshotInfo, SnapshotOutcome, SnapshotReason};
use ssw_io::recovery::{MANIFEST_NAME, RecoveryDoc, RecoveryKind, now_millis};
use ssw_ui::access::{AccessNode, AccessTree, Role};
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::geometry::Point;
use ssw_ui::input::{Event, PointerButton, PointerEvent, PointerEventKind};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard, MessageBoxResult};
use ssw_ui::shell::banners::BannerKind;
use ssw_ui::shell::tabs::TabsEvent;
use ssw_ui::shell::titlebar::TitleEvent;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::dragdrop::OleGuard;
use ssw_win::gfx::Driver;
use ssw_win::menus::{PopupHost, PopupRequest};
use ssw_win::window::current_os_version;
use windows::Win32::UI::WindowsAndMessaging::HMENU;

/// 2020-01-01 12:30:00 UTC: far older than the snapshots the flows take while a test runs (they
/// use the clock), so the versions a test stores itself are listed after those.
const T1: i64 = 1_577_881_800;

/// Autosave would write the unsaved edits these tests keep.
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

/// Arguments that open `files[0]` of `work` in a hidden window of a profile next to it.
fn arguments(root: &Path, work: &Path, first: &str) -> Args {
    let mut args = Args {
        paths: vec![work.join(first)],
        ..Args::default()
    };
    args.test.hidden = true;
    args.test.profile = Some(root.join("profile"));
    args
}

fn write_settings(root: &Path, settings: &str) {
    let folder = root.join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("settings.json"), settings).unwrap();
}

// ----- the controller without native windows ---------------------------------------------------

/// The controller of a started application and the means to use it like the user.
struct Rig {
    controller: Rc<Controller>,
    services: ScriptedServices,
    engine: MonoTextEngine,
}

struct Host<P, B> {
    prepare: Option<P>,
    body: Option<B>,
    work: PathBuf,
    services: ScriptedServices,
}

impl<P: FnOnce(&mut AppCore, &Path), B: FnOnce(&Rig)> ShellHost for Host<P, B> {
    fn run(&mut self, mut core: AppCore, _args: &Args) -> i32 {
        (self.prepare.take().expect("prepared once"))(&mut core, &self.work);
        let services = self.services.clone();
        let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let plan = core.plan.clone();
        let controller =
            Controller::new(core, services.dialogs(), services.shell(), clipboard, clock);
        controller.open_plan(&plan);
        let rig = Rig {
            controller,
            services,
            engine: MonoTextEngine::default(),
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

/// Runs `body` over a started application whose first window shows `files[0]`. `prepare` runs
/// before the windows are created (recovery entries, file attributes).
fn with_rig(
    settings: &str,
    files: &[(&str, &str)],
    prepare: impl FnOnce(&mut AppCore, &Path),
    body: impl FnOnce(&Rig),
) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    for (name, text) in files {
        std::fs::write(work.join(name), text).unwrap();
    }
    write_settings(tmp.path(), settings);
    let args = arguments(tmp.path(), &work, files[0].0);
    let mut startup_dialogs = ScriptedServices::new().dialogs();
    let code = run(
        &args,
        process(&args),
        &mut *startup_dialogs,
        None,
        &mut Host {
            prepare: Some(prepare),
            body: Some(body),
            work,
            services: ScriptedServices::new(),
        },
    );
    assert_eq!(code, 0);
}

fn nothing(_: &mut AppCore, _: &Path) {}

impl Rig {
    fn window(&self) -> Rc<WindowCell> {
        Rc::clone(&self.controller.windows()[0])
    }

    fn id(&self) -> WindowId {
        self.window().id
    }

    fn active(&self) -> Rc<DocEntry> {
        let window = self.window();
        let view = window.view.borrow();
        Rc::clone(&view.active_tab().expect("a tab is open").entry)
    }

    fn path(&self) -> PathBuf {
        self.active().path().expect("a file document")
    }

    fn text(&self) -> String {
        self.active().document().borrow().text()
    }

    /// Replaces the text of the active document with one unsaved edit.
    fn set_text(&self, text: &str) {
        let changed = replace_text(&mut self.active().document().borrow_mut(), text);
        assert!(changed, "the text differs");
        self.controller.refresh_all();
    }

    fn banners(&self) -> Vec<BannerKind> {
        self.window().view.borrow_mut().banners_mut().kinds()
    }

    fn history_open(&self) -> bool {
        self.window().view.borrow().history().is_open()
    }

    fn tab_texts(&self) -> Vec<String> {
        self.window()
            .view
            .borrow()
            .tabs()
            .iter()
            .map(|tab| tab.entry.document().borrow().text())
            .collect()
    }

    /// Activates the tab at `index` the way a click on it does.
    fn activate(&self, index: usize) {
        let key = self.window().view.borrow().tabs()[index].key;
        self.controller.handle_action(
            self.id(),
            ShellAction::Title(TitleEvent::Tabs(TabsEvent::Activated(key))),
        );
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

    /// The accessibility tree of the window after a layout.
    fn tree(&self) -> AccessTree {
        let window = self.window();
        let mut view = window.view.borrow_mut();
        view.paint(&mut RecordingCanvas::new(), &self.engine, false);
        view.access_tree(&self.engine, "SS Writer")
    }

    fn names(&self, role: Role) -> Vec<String> {
        self.tree()
            .nodes()
            .iter()
            .filter(|node| node.role == role)
            .map(|node| node.name.clone())
            .collect()
    }

    fn find(&self, role: Role, name: &str) -> AccessNode {
        self.tree()
            .nodes()
            .iter()
            .find(|node| node.role == role && node.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {role:?} named {name:?}"))
    }

    /// A click: the actions the window content reports are carried out like the loop owner does.
    fn click(&self, at: Point) {
        let window = self.window();
        for kind in [PointerEventKind::Down, PointerEventKind::Up] {
            let event = PointerEvent {
                click_count: 1,
                ..PointerEvent::mouse(kind, Some(PointerButton::Left), at)
            };
            let actions = window
                .view
                .borrow_mut()
                .event(&Event::Pointer(event), &self.engine);
            for action in actions {
                self.controller.handle_action(window.id, action);
            }
        }
    }

    fn press(&self, name: &str) {
        let node = self.find(Role::Button, name);
        self.click(node.bounds.center());
    }

    /// Presses the button `name` of the banner `kind`.
    fn press_in(&self, kind: BannerKind, name: &str) {
        let tree = self.tree();
        let area = self
            .window()
            .view
            .borrow_mut()
            .banners_mut()
            .banner_rect(kind)
            .unwrap_or_else(|| panic!("the {kind:?} banner is not shown"));
        let node = tree
            .nodes()
            .iter()
            .find(|node| {
                node.role == Role::Button
                    && node.name == name
                    && area.contains(node.bounds.center())
            })
            .cloned()
            .unwrap_or_else(|| panic!("no button {name:?} in the {kind:?} banner"));
        self.click(node.bounds.center());
    }

    /// The rows of the history list (the sizes of these tests are all below 1 KB).
    fn version_rows(&self) -> Vec<String> {
        let mut rows = self.names(Role::ListItem);
        rows.retain(|row| row.ends_with(" bytes"));
        rows
    }

    /// Presses the row of the history list that ends with `suffix`.
    fn select_version(&self, suffix: &str) {
        let node = self
            .tree()
            .nodes()
            .iter()
            .find(|node| node.role == Role::ListItem && node.name.ends_with(suffix))
            .cloned()
            .unwrap_or_else(|| panic!("no version ending with {suffix:?}"));
        self.click(node.bounds.center());
        self.settle();
    }

    /// Stores a snapshot of `text` for the active file at `at`; returns its id.
    fn snapshot(&self, text: &str, at: i64) -> i64 {
        let path = self.path();
        let outcome = self
            .controller
            .docs()
            .env()
            .store
            .borrow_mut()
            .history_snapshot(&path, text, SnapshotReason::Periodic, at)
            .unwrap();
        match outcome {
            SnapshotOutcome::Created(id) | SnapshotOutcome::Unchanged(id) => id,
        }
    }

    fn history(&self) -> Vec<SnapshotInfo> {
        self.history_of(&self.path())
    }

    /// The versions of the file at `path` (a file whose tab may be gone), newest first.
    fn history_of(&self, path: &Path) -> Vec<SnapshotInfo> {
        self.controller
            .docs()
            .env()
            .store
            .borrow()
            .history_list(path)
            .unwrap()
    }

    fn snapshot_text(&self, id: i64) -> String {
        self.controller
            .docs()
            .env()
            .store
            .borrow()
            .history_load(id)
            .unwrap()
            .expect("the snapshot exists")
            .content
    }

    fn answer(&self, button: MessageBoxResult) {
        self.services
            .answer(DialogKind::MessageBox, Answer::Button(button));
    }
}

fn set_read_only(path: &Path, read_only: bool) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(read_only);
    std::fs::set_permissions(path, permissions).unwrap();
}

// ----- the history panel -----------------------------------------------------------------------

#[test]
fn the_history_lists_the_versions_newest_first_and_shows_what_changed() {
    let v1 = "alpha\nbeta\ngamma\n";
    let v2 = "alpha\nbeta\ngamma\ndelta\n";
    with_rig(NO_AUTOSAVE, &[("notes.md", v1)], nothing, |rig| {
        let old = rig.snapshot(v1, T1);
        let newer = rig.snapshot(v2, T1 + 600);
        rig.set_text("alpha\nBETA\ngamma\ndelta\nepsilon\n");
        rig.controller.open_history(rig.id(), None);
        {
            let window = rig.window();
            let view = window.view.borrow();
            let panel = view.history().panel().expect("the panel is open");
            let listed: Vec<(i64, usize)> = panel
                .entries()
                .iter()
                .map(|entry| (entry.id, entry.size))
                .collect();
            assert_eq!(listed, [(newer, 23), (old, 17)]);
            assert!(
                panel.entries()[0].time.ends_with(" 12:40 UTC"),
                "{}",
                panel.entries()[0].time
            );
            assert!(
                panel.entries()[1].time.ends_with(" 12:30 UTC"),
                "{}",
                panel.entries()[1].time
            );
            assert_eq!(panel.selected(), None);
        }
        let rows = rig.version_rows();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(
            rows[0].contains("12:40 UTC") && rows[0].ends_with("23 bytes"),
            "{rows:?}"
        );
        assert!(
            rows[1].contains("12:30 UTC") && rows[1].ends_with("17 bytes"),
            "{rows:?}"
        );

        // The changes between the older version and the text now.
        rig.select_version("17 bytes");
        let rows = rig.names(Role::ListItem);
        for expected in [
            "Removed: beta",
            "Added: BETA",
            "Added: delta",
            "Added: epsilon",
        ] {
            assert!(
                rows.iter().any(|row| row == expected),
                "{expected:?} in {rows:?}"
            );
        }
        // The newer one already has the line "delta".
        rig.select_version("23 bytes");
        let rows = rig.names(Role::ListItem);
        assert!(rows.iter().any(|row| row == "Removed: beta"), "{rows:?}");
        assert!(rows.iter().any(|row| row == "Added: epsilon"), "{rows:?}");
        assert!(!rows.iter().any(|row| row == "Added: delta"), "{rows:?}");
        // Looking changed nothing.
        assert_eq!(rig.text(), "alpha\nBETA\ngamma\ndelta\nepsilon\n");
    });
}

#[test]
fn a_restore_snapshots_the_text_first_and_is_undone_by_one_undo() {
    let v1 = "alpha\nbeta\ngamma\n";
    let current = "alpha\nBETA\ngamma\ndelta\n";
    with_rig(NO_AUTOSAVE, &[("notes.md", v1)], nothing, |rig| {
        rig.snapshot(v1, T1);
        rig.set_text(current);
        rig.controller.open_history(rig.id(), None);
        rig.select_version("17 bytes");
        rig.press("Restore this version");

        assert_eq!(rig.text(), v1);
        assert!(
            !rig.history_open(),
            "the panel closes once the document shows the version"
        );
        // Not written to disk: it is an edit like any other.
        assert!(rig.active().is_dirty());
        // The text before the restore is in the history now.
        let versions = rig.history();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].reason, SnapshotReason::BeforeRestore);
        assert_eq!(rig.snapshot_text(versions[0].id), current);

        // One undo brings the text back.
        assert!(rig.active().document().borrow_mut().undo().unwrap());
        assert_eq!(rig.text(), current);

        // The same through the history itself: restore the saved copy of the text.
        rig.controller.open_history(rig.id(), None);
        rig.select_version("23 bytes");
        rig.press("Restore this version");
        assert_eq!(rig.text(), current);
    });
}

#[test]
fn the_history_closes_when_another_tab_is_shown() {
    with_rig(
        NO_AUTOSAVE,
        &[("a.md", "first\n"), ("b.md", "second\n")],
        nothing,
        |rig| {
            let window = rig.window();
            rig.controller
                .open_file(&window, &rig.path().with_file_name("b.md"))
                .unwrap();
            rig.controller.refresh_all();
            assert_eq!(rig.text(), "second\n");
            rig.controller.open_history(rig.id(), None);
            assert!(rig.history_open());
            rig.activate(0);
            assert_eq!(rig.text(), "first\n");
            assert!(
                !rig.history_open(),
                "the versions listed were those of the other file"
            );

            rig.controller.open_history(rig.id(), None);
            assert!(rig.history_open());
            rig.activate(1);
            assert!(!rig.history_open());
        },
    );
}

#[test]
fn the_version_history_command_opens_the_panel_of_the_active_document() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "one\n")], nothing, |rig| {
        rig.snapshot("zero\n", T1);
        assert!(!rig.history_open());
        let outcome = rig
            .controller
            .run_command(rig.id(), CommandId::FileVersionHistory);
        assert!(matches!(outcome, Dispatch::Handled), "{outcome:?}");
        assert!(rig.history_open());
        let rows = rig.version_rows();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0].contains("12:30 UTC") && rows[0].ends_with("5 bytes"),
            "{rows:?}"
        );
    });
}

// ----- the snapshots of closing and of saving back -----------------------------------------------

#[test]
fn closing_a_tab_keeps_its_unsaved_text_in_the_history() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "first\n")], nothing, |rig| {
        let window = rig.window();
        let path = rig.path();
        let key = window.view.borrow().tabs()[0].key;
        rig.set_text("first\nunsaved\n");
        // "Don't save": the edits leave the file's future, not the history.
        rig.answer(MessageBoxResult::DontSave);
        assert_eq!(rig.controller.close_tab(&window, key), CloseOutcome::Closed);
        let versions = rig.history_of(&path);
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].reason, SnapshotReason::Close);
        assert_eq!(rig.snapshot_text(versions[0].id), "first\nunsaved\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
    });
}

#[test]
fn closing_a_window_keeps_the_unsaved_text_of_every_open_file() {
    with_rig(
        NO_AUTOSAVE,
        &[("a.md", "first\n"), ("b.md", "second\n")],
        nothing,
        |rig| {
            let window = rig.window();
            let a = rig.path();
            let b = canonical_path(&a.with_file_name("b.md"));
            rig.controller.open_file(&window, &b).unwrap();
            rig.controller.refresh_all();
            rig.set_text("second\nunsaved\n");
            rig.activate(0);
            rig.set_text("first\nunsaved\n");
            // The question of each of the two unsaved documents: leave without saving.
            rig.answer(MessageBoxResult::DontSave);
            rig.answer(MessageBoxResult::DontSave);
            assert!(rig.controller.request_close_window(window.id));
            rig.controller.close_window(window.id);
            assert!(rig.controller.windows().is_empty());
            for (path, text) in [(&a, "first\nunsaved\n"), (&b, "second\nunsaved\n")] {
                let versions = rig.history_of(path);
                assert_eq!(versions.len(), 1, "{path:?}");
                assert_eq!(versions[0].reason, SnapshotReason::Close);
                assert_eq!(rig.snapshot_text(versions[0].id), text);
            }
        },
    );
}

#[test]
fn saving_back_to_word_snapshots_the_markdown_before_the_file_is_replaced() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/docx/poi_heading123.docx");
    with_rig(NO_AUTOSAVE, &[("notes.md", "plain\n")], nothing, |rig| {
        // A copy: saving back replaces the Word file.
        let word = rig.path().with_file_name("report.docx");
        std::fs::copy(&fixture, &word).unwrap();
        let window = rig.window();
        rig.controller.open_file(&window, &word).unwrap();
        rig.controller.refresh_all();
        rig.set_text(&format!("{}\nAn added closing line.\n", rig.text()));
        let edited = rig.text();
        let original = std::fs::read(&word).unwrap();

        let outcome = rig
            .controller
            .run_command(rig.id(), CommandId::FileSaveBackToWord);
        assert!(matches!(outcome, Dispatch::Handled), "{outcome:?}");
        assert_ne!(
            std::fs::read(&word).unwrap(),
            original,
            "the file was replaced"
        );
        let versions = rig.history();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].reason, SnapshotReason::BeforeSaveBack);
        assert_eq!(rig.snapshot_text(versions[0].id), edited);
    });
}

// ----- files changed by other programs ---------------------------------------------------------

#[test]
fn compare_shows_the_file_on_disk_and_restoring_it_is_load_theirs() {
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "base\nline two\n")],
        nothing,
        |rig| {
            let path = rig.path();
            rig.set_text("base\nmine\n");
            std::fs::write(&path, "base\ntheirs and more\n").unwrap();
            rig.controller.check_external_changes(rig.id());
            assert_eq!(rig.active().external(), ExternalState::Conflict);
            assert_eq!(rig.banners(), [BannerKind::Conflict]);
            // The unsaved edits are untouched until the user chooses.
            assert_eq!(rig.text(), "base\nmine\n");

            rig.press_in(BannerKind::Conflict, "Compare");
            {
                let window = rig.window();
                let view = window.view.borrow();
                assert!(view.history().is_open());
                assert_eq!(view.history().disk_text(), Some("base\ntheirs and more\n"));
                let panel = view.history().panel().unwrap();
                assert_eq!(panel.selected(), Some(DISK_ENTRY_ID));
                assert_eq!(panel.entries()[0].id, DISK_ENTRY_ID);
            }
            rig.settle();
            let rows = rig.names(Role::ListItem);
            assert!(
                rows.iter().any(|row| row.starts_with("File on disk")),
                "{rows:?}"
            );
            assert!(
                rows.iter().any(|row| row == "Removed: theirs and more"),
                "{rows:?}"
            );
            assert!(rows.iter().any(|row| row == "Added: mine"), "{rows:?}");

            // Restoring the entry of the file on disk loads it.
            rig.press("Restore this version");
            assert_eq!(rig.text(), "base\ntheirs and more\n");
            assert_eq!(rig.active().external(), ExternalState::InSync);
            assert!(!rig.active().is_dirty());
            assert!(rig.banners().is_empty());
            assert!(!rig.history_open());
            // The edits were not lost: they are in the history.
            let versions = rig.history();
            assert_eq!(versions[0].reason, SnapshotReason::BeforeExternalReload);
            assert_eq!(rig.snapshot_text(versions[0].id), "base\nmine\n");
        },
    );
}

#[test]
fn load_theirs_and_keep_mine_settle_the_conflict() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "base\n")], nothing, |rig| {
        let path = rig.path();
        rig.set_text("base\nmine\n");
        std::fs::write(&path, "base\ntheirs\n").unwrap();
        rig.controller.check_external_changes(rig.id());
        assert_eq!(rig.banners(), [BannerKind::Conflict]);

        // Compare, then decide in the banner: the comparison is over.
        rig.press_in(BannerKind::Conflict, "Compare");
        assert!(rig.history_open());
        rig.press_in(BannerKind::Conflict, "Keep mine");
        assert!(!rig.history_open());
        assert_eq!(rig.text(), "base\nmine\n");
        assert_eq!(rig.active().external(), ExternalState::InSync);
        assert!(rig.banners().is_empty());
        // The file is not touched, and the same foreign version is not reported again.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "base\ntheirs\n");
        rig.controller.check_external_changes(rig.id());
        assert_eq!(rig.active().external(), ExternalState::InSync);
        assert!(rig.banners().is_empty());

        // Another change by the other program, this time: Load theirs.
        std::fs::write(&path, "base\ntheirs, again\n").unwrap();
        rig.controller.check_external_changes(rig.id());
        assert_eq!(rig.banners(), [BannerKind::Conflict]);
        rig.press_in(BannerKind::Conflict, "Load theirs");
        assert_eq!(rig.text(), "base\ntheirs, again\n");
        assert!(rig.banners().is_empty());
        assert!(!rig.active().is_dirty());
    });
}

#[test]
fn a_file_changed_while_the_window_was_elsewhere_reloads_when_it_is_activated() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "one\ntwo\n")], nothing, |rig| {
        // Nothing changed: nothing happens.
        rig.controller.check_external_changes(rig.id());
        assert_eq!(rig.text(), "one\ntwo\n");

        std::fs::write(rig.path(), "one\ntwo\nthree\n").unwrap();
        rig.controller.check_external_changes(rig.id());
        assert_eq!(rig.text(), "one\ntwo\nthree\n");
        assert!(!rig.active().is_dirty());
        assert!(rig.banners().is_empty());
        let versions = rig.history();
        assert_eq!(versions[0].reason, SnapshotReason::BeforeExternalReload);
        assert_eq!(rig.snapshot_text(versions[0].id), "one\ntwo\n");
    });
}

// ----- banners the user closed -----------------------------------------------------------------

fn read_only_first(_: &mut AppCore, work: &Path) {
    set_read_only(&work.join("a.md"), true);
}

#[test]
fn a_closed_banner_stays_closed_until_its_reason_ends_and_returns() {
    with_rig(
        NO_AUTOSAVE,
        &[("a.md", "locked\n"), ("b.md", "free\n")],
        read_only_first,
        |rig| {
            let path = rig.path();
            assert_eq!(rig.banners(), [BannerKind::ReadOnly]);
            rig.press_in(BannerKind::ReadOnly, "Close");
            assert!(rig.banners().is_empty());
            // Housekeeping used to show it again at once.
            rig.controller.refresh_all();
            rig.settle();
            assert!(rig.banners().is_empty());

            // Another tab and back: the document remembers.
            let window = rig.window();
            rig.controller
                .open_file(&window, &path.with_file_name("b.md"))
                .unwrap();
            rig.controller.refresh_all();
            assert!(rig.banners().is_empty());
            rig.activate(0);
            assert!(rig.active().is_read_only());
            assert!(rig.banners().is_empty());

            // The file becomes writable and read-only again: that is news.
            set_read_only(&path, false);
            rig.controller.docs().refresh_access(&rig.active());
            rig.controller.refresh_all();
            assert!(rig.banners().is_empty());
            set_read_only(&path, true);
            rig.controller.docs().refresh_access(&rig.active());
            rig.controller.refresh_all();
            assert_eq!(rig.banners(), [BannerKind::ReadOnly]);
            set_read_only(&path, false);
        },
    );
}

#[test]
fn allow_editing_clears_the_read_only_attribute() {
    with_rig(
        NO_AUTOSAVE,
        &[("a.md", "locked\n")],
        read_only_first,
        |rig| {
            let path = rig.path();
            assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
            assert_eq!(rig.banners(), [BannerKind::ReadOnly]);
            rig.press_in(BannerKind::ReadOnly, "Allow editing");
            assert!(!rig.active().is_read_only());
            assert!(!std::fs::metadata(&path).unwrap().permissions().readonly());
            assert!(rig.banners().is_empty());
        },
    );
}

#[test]
fn the_import_report_lists_what_was_lost_and_stays_closed() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/docx/mammoth_comments.docx");
    with_rig(NO_AUTOSAVE, &[("notes.md", "plain\n")], nothing, |rig| {
        assert!(rig.banners().is_empty());
        let window = rig.window();
        rig.controller.open_file(&window, &fixture).unwrap();
        rig.controller.refresh_all();
        assert_eq!(rig.banners(), [BannerKind::ImportLoss]);

        // Details expands the list of what could not be represented.
        let losses = rig.active().losses();
        assert!(!losses.is_empty());
        rig.press_in(BannerKind::ImportLoss, "Details");
        assert!(window.view.borrow_mut().banners_mut().details_expanded());
        let first = rig.controller.i18n().text(loss_message_id(losses[0].kind));
        let rows = rig.names(Role::ListItem);
        assert!(
            rows.iter().any(|row| row.starts_with(&first)),
            "{first:?} in {rows:?}"
        );

        rig.press_in(BannerKind::ImportLoss, "Close");
        assert!(rig.banners().is_empty());
        rig.activate(0);
        rig.activate(1);
        assert!(
            rig.active()
                .path()
                .unwrap()
                .ends_with("mammoth_comments.docx")
        );
        assert!(rig.banners().is_empty(), "closed for this document");
    });
}

// ----- remote images ---------------------------------------------------------------------------

const REMOTE: &str = "![logo](https://example.com/logo.png)\n";

#[test]
fn the_remote_images_banner_opts_the_document_out() {
    with_rig(
        NO_AUTOSAVE,
        &[("img.md", REMOTE), ("plain.md", "no pictures\n")],
        nothing,
        |rig| {
            let doc = rig.active().id();
            assert_eq!(rig.banners(), [BannerKind::RemoteImages]);
            assert!(!rig.controller.remote_images().is_disabled(doc));
            rig.window().dirty.set(false);
            rig.press_in(BannerKind::RemoteImages, "Don't load remote images");
            assert!(rig.controller.remote_images().is_disabled(doc));
            assert!(
                rig.window().dirty.get(),
                "the window lays its images out again"
            );
            assert!(rig.banners().is_empty());
            rig.controller.refresh_all();
            assert!(
                rig.banners().is_empty(),
                "an opted-out document has nothing to announce"
            );

            // Another document without a picture from the network: no banner.
            let window = rig.window();
            rig.controller
                .open_file(&window, &rig.path().with_file_name("plain.md"))
                .unwrap();
            rig.controller.refresh_all();
            assert!(rig.banners().is_empty());
            assert!(
                !rig.controller
                    .remote_images()
                    .is_disabled(rig.active().id())
            );
        },
    );
}

#[test]
fn the_remote_images_banner_follows_the_text_and_stays_closed() {
    with_rig(
        NO_AUTOSAVE,
        &[("plain.md", "no pictures\n")],
        nothing,
        |rig| {
            assert!(rig.banners().is_empty());
            // A picture on this computer is not remote.
            rig.set_text("![a](images/a.png)\n");
            assert!(rig.banners().is_empty());
            rig.set_text("![a](images/a.png)\n![b](http://example.com/b.png)\n");
            assert_eq!(rig.banners(), [BannerKind::RemoteImages]);

            rig.press_in(BannerKind::RemoteImages, "Close");
            assert!(rig.banners().is_empty());
            // Still pictures from the network: the closed banner stays closed, also after more edits.
            rig.set_text("![a](images/a.png)\n![b](http://example.com/b.png)\nmore text\n");
            assert!(rig.banners().is_empty());
            assert!(
                !rig.controller
                    .remote_images()
                    .is_disabled(rig.active().id())
            );

            // The last remote picture goes: the banner would be news again.
            rig.set_text("![a](images/a.png)\n");
            rig.set_text("![c](https://example.com/c.png)\n");
            assert_eq!(rig.banners(), [BannerKind::RemoteImages]);
        },
    );
}

#[test]
fn the_remote_images_banner_needs_the_setting() {
    let settings = r#"{"general":{"autosave":false},"attachments":{"load_remote_images":false}}"#;
    with_rig(settings, &[("img.md", REMOTE)], nothing, |rig| {
        assert!(rig.banners().is_empty());
    });
}

// ----- the recovery offer ----------------------------------------------------------------------

/// A crashed session left an untitled buffer and unsaved edits of `notes.md`.
fn leave_recovery(core: &mut AppCore, work: &Path) {
    let notes = canonical_path(&work.join("notes.md"));
    let format = TextFormat::default();
    core.recovery
        .backup(
            &RecoveryDoc {
                id: "untitled-1",
                kind: RecoveryKind::Untitled,
                original_path: None,
                display_name: "Untitled",
                text: "recovered scratch\n",
                format,
            },
            now_millis(),
        )
        .unwrap();
    core.recovery
        .backup(
            &RecoveryDoc {
                id: "notes-dirty",
                kind: RecoveryKind::Dirty,
                original_path: Some(&notes),
                display_name: "notes",
                text: "recovered edits\n",
                format,
            },
            now_millis(),
        )
        .unwrap();
    core.recoverable = core.recovery.list().unwrap();
    assert_eq!(core.recoverable.len(), 2);
}

fn recovery_entries(rig: &Rig) -> usize {
    rig.controller.docs().env().recovery.list().unwrap().len()
}

#[test]
fn restore_opens_the_recovered_documents_and_activates_the_tab_of_an_open_file() {
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "notes on disk\n")],
        leave_recovery,
        |rig| {
            assert_eq!(rig.banners(), [BannerKind::Recovery]);
            // The offer is not acted on by itself.
            assert_eq!(rig.tab_texts(), ["notes on disk\n"]);
            assert_eq!(recovery_entries(rig), 2);

            rig.press_in(BannerKind::Recovery, "Restore");

            // The untitled buffer is a new tab; the file that was open shows the recovered edits in
            // its own tab, which is the active one.
            assert_eq!(
                rig.tab_texts(),
                ["recovered edits\n", "recovered scratch\n"]
            );
            assert_eq!(rig.text(), "recovered edits\n");
            assert!(rig.active().is_dirty());
            assert_eq!(
                rig.active().users(),
                1,
                "no second tab was added for the file"
            );
            assert_eq!(rig.controller.docs().all().len(), 2);
            assert!(rig.banners().is_empty());
            assert_eq!(recovery_entries(rig), 0);
            assert!(rig.controller.core().borrow().recoverable.is_empty());
            // The file on disk is only changed by saving.
            assert_eq!(
                std::fs::read_to_string(rig.path()).unwrap(),
                "notes on disk\n"
            );
            rig.activate(1);
            assert!(rig.active().is_untitled());
            assert!(rig.banners().is_empty(), "answered offers do not come back");
        },
    );
}

#[test]
fn discard_empties_the_recovery_folder() {
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "notes on disk\n")],
        leave_recovery,
        |rig| {
            assert_eq!(rig.banners(), [BannerKind::Recovery]);
            rig.press_in(BannerKind::Recovery, "Discard");
            assert_eq!(recovery_entries(rig), 0);
            assert_eq!(rig.tab_texts(), ["notes on disk\n"]);
            assert!(rig.banners().is_empty());
            assert!(rig.controller.core().borrow().recoverable.is_empty());
        },
    );
}

#[test]
fn closing_the_recovery_banner_asks_before_it_deletes_anything() {
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "notes on disk\n")],
        leave_recovery,
        |rig| {
            // No: the banner stays and so does the recovered text.
            rig.answer(MessageBoxResult::No);
            rig.press_in(BannerKind::Recovery, "Close");
            assert_eq!(rig.banners(), [BannerKind::Recovery]);
            assert_eq!(recovery_entries(rig), 2);
            let asked = rig.services.dialog_log();
            assert_eq!(asked.len(), 1);
            assert!(
                asked[0]
                    .message
                    .contains("Delete the recovered unsaved text?"),
                "{}",
                asked[0].message
            );

            // Yes: gone.
            rig.answer(MessageBoxResult::Yes);
            rig.press_in(BannerKind::Recovery, "Close");
            assert!(rig.banners().is_empty());
            assert_eq!(recovery_entries(rig), 0);
            assert_eq!(rig.tab_texts(), ["notes on disk\n"]);
        },
    );
}

/// Recovery whose manifest cannot be read any more.
fn break_recovery(core: &mut AppCore, work: &Path) {
    leave_recovery(core, work);
    std::fs::write(core.recovery.dir().join(MANIFEST_NAME), "not json").unwrap();
}

#[test]
fn what_cannot_be_restored_or_discarded_is_reported_and_stays_offered() {
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "notes on disk\n")],
        break_recovery,
        |rig| {
            // Each of the two documents is reported.
            rig.answer(MessageBoxResult::Ok);
            rig.answer(MessageBoxResult::Ok);
            rig.press_in(BannerKind::Recovery, "Restore");
            {
                let asked = rig.services.dialog_log();
                assert_eq!(asked.len(), 2);
                assert!(
                    asked[0]
                        .message
                        .starts_with("The recovered text of Untitled could not be restored"),
                    "{}",
                    asked[0].message
                );
                assert!(
                    asked[1]
                        .message
                        .starts_with("The recovered text of notes could not be restored"),
                    "{}",
                    asked[1].message
                );
            }
            assert_eq!(rig.tab_texts(), ["notes on disk\n"]);
            assert_eq!(rig.banners(), [BannerKind::Recovery]);

            rig.answer(MessageBoxResult::Ok);
            rig.press_in(BannerKind::Recovery, "Discard");
            {
                let asked = rig.services.dialog_log();
                assert_eq!(asked.len(), 3);
                assert!(
                    asked[2]
                        .message
                        .starts_with("The recovered text could not be deleted"),
                    "{}",
                    asked[2].message
                );
            }
            assert_eq!(rig.banners(), [BannerKind::Recovery]);

            // Once the folder works again, the offer can be answered.
            let manifest = rig
                .controller
                .docs()
                .env()
                .recovery
                .dir()
                .join(MANIFEST_NAME);
            std::fs::write(manifest, r#"{"version":1,"entries":[]}"#).unwrap();
            rig.press_in(BannerKind::Recovery, "Discard");
            assert!(rig.banners().is_empty());
        },
    );
}

// ----- the native window -----------------------------------------------------------------------

struct NoPopups;

impl PopupHost for NoPopups {
    fn track(&mut self, _menu: HMENU, _request: &PopupRequest) -> Option<u32> {
        None
    }
}

struct AppHost<F> {
    body: Option<F>,
}

impl<F: FnOnce(&UiApp, &Path)> ShellHost for AppHost<F> {
    fn run(&mut self, core: AppCore, args: &Args) -> i32 {
        // Declared first: dropped after every COM object of the window.
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
            ..UiOptions::default()
        };
        let app = UiApp::new(core, options).unwrap();
        app.pump();
        (self.body.take().expect("the body runs once"))(&app, &args.paths[0]);
        0
    }
}

#[test]
fn the_activation_request_of_a_window_reloads_what_another_program_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("notes.md"), "one\ntwo\n").unwrap();
    write_settings(tmp.path(), NO_AUTOSAVE);
    let args = arguments(tmp.path(), &work, "notes.md");
    let mut dialogs = ScriptedServices::new().dialogs();
    let code = run(
        &args,
        process(&args),
        &mut *dialogs,
        None,
        &mut AppHost {
            body: Some(|app: &UiApp, file: &Path| {
                let window = Rc::clone(&app.controller().windows()[0]);
                std::fs::write(file, "one\ntwo\nthree\n").unwrap();
                // What `WM_ACTIVATE` leaves for the loop owner.
                app.controller().push(Request::Activated(window.id));
                app.pump();
                let text = window
                    .view
                    .borrow()
                    .active_tab()
                    .expect("a tab is open")
                    .entry
                    .document()
                    .borrow()
                    .text();
                assert_eq!(text, "one\ntwo\nthree\n");
            }),
        },
    );
    assert_eq!(code, 0);
}
