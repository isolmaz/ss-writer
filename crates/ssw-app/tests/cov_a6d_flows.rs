//! The failure and "nothing to act on" paths of the banner flows, the version history panel, the
//! inspector feed and the remote image policy: the real controller over a started application
//! in a temporary profile (as `ui_history.rs`), with scripted dialogs and, where a file call has
//! to fail, a `FaultFs` under the documents. Nothing is shown; what the user would see is read
//! back from the dialogs shown, the document text, the banners, the history panel and the disk.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::commands::Dispatch;
use ssw_app::documents::{
    DocEntry, DocEnv, DocError, Documents, ExternalState, canonical_path, load_file,
    loss_message_id, replace_text,
};
use ssw_app::i18n::localizer_for;
use ssw_app::session::StartupPlan;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::{Controller, WindowCell};
use ssw_app::ui::history::HistoryHost;
use ssw_app::ui::inspector::backlinks_of;
use ssw_app::ui::overlays::OverlayEnv;
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices};
use ssw_app::ui::shell_view::ShellAction;
use ssw_app::workers::WindowId;
use ssw_convert::import::{Loss, LossKind};
use ssw_core::command::CommandId;
use ssw_core::i18n::Arg;
use ssw_core::markdown::ast::CalloutKind;
use ssw_core::text::TextRange;
use ssw_io::encoding::TextFormat;
use ssw_io::fs::{
    Attributes, DirEntry, FaultFs, FileMeta, FileSystem, FsError, FsFailure, FsOp, RealFs,
};
use ssw_io::history::{SnapshotOutcome, SnapshotReason};
use ssw_io::recovery::{RecoveryDoc, RecoveryKind, now_millis};
use ssw_io::save::SaveError;
use ssw_io::store::Store;
use ssw_ui::access::{AccessNode, AccessTree, Role};
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::editor::EditorEvent;
use ssw_ui::editor::blocks_media::MediaEvent;
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::input::{
    Event, FocusEvent, KeyEvent, Modifiers, PointerButton, PointerEvent, PointerEventKind,
    TextInput, vk,
};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard, MessageBoxResult};
use ssw_ui::shell::banners::{BannerAction, BannerKind, HostEvent};
use ssw_ui::shell::history::HistoryEvent;
use ssw_ui::shell::inspector::InspectorEvent;
use ssw_ui::shell::statusbar::StatusEvent;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widget::{EventResult, FocusManager, PointerCapture, Requests};
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::window::current_os_version;

/// 2020-01-01 12:30:00 UTC.
const T1: i64 = 1_577_881_800;

/// Autosave would write the unsaved edits these tests keep.
const NO_AUTOSAVE: &str = r#"{"general":{"autosave":false}}"#;

/// What `DocError::message_id` says for a file call that failed (`app-error-file`).
const FILE_ERROR: &str = "The file could not be read or written.";
/// `app-error-not-found`.
const NOT_FOUND: &str = "The file no longer exists.";
/// `flows-history-missing`.
const VERSION_MISSING: &str = "That saved version is no longer available.";

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
/// before the windows are created.
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
    let folder = tmp.path().join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("settings.json"), settings).unwrap();
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
            prepare: Some(prepare),
            body: Some(body),
            work,
            services: ScriptedServices::new(),
        },
    );
    assert_eq!(code, 0);
}

fn nothing(_: &mut AppCore, _: &Path) {}

/// Makes the documents of the started application go through `fs`.
fn install(core: &mut AppCore, fs: Arc<dyn FileSystem>) {
    let env = DocEnv {
        fs,
        ..core.docs.env().clone()
    };
    core.docs = Rc::new(Documents::new(env));
}

fn set_read_only(path: &Path, read_only: bool) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(read_only);
    std::fs::set_permissions(path, permissions).unwrap();
}

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

    fn caret(&self) -> usize {
        self.active().document().borrow().primary_selection().head
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

    /// Removes every tab of the window (the documents stay open in the registry).
    fn close_all_tabs(&self) {
        let window = self.window();
        let keys: Vec<u64> = window
            .view
            .borrow()
            .tabs()
            .iter()
            .map(|tab| tab.key)
            .collect();
        for key in keys {
            let _ = window.view.borrow_mut().close_tab(key);
        }
        self.controller.refresh_all();
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

    fn tree(&self) -> AccessTree {
        let window = self.window();
        let mut view = window.view.borrow_mut();
        view.paint(&mut RecordingCanvas::new(), &self.engine, false);
        view.access_tree(&self.engine, "SS Writer")
    }

    fn find(&self, role: Role, name: &str) -> AccessNode {
        self.tree()
            .nodes()
            .iter()
            .find(|node| node.role == role && node.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {role:?} named {name:?}"))
    }

    fn names(&self, role: Role) -> Vec<String> {
        self.tree()
            .nodes()
            .iter()
            .filter(|node| node.role == role)
            .map(|node| node.name.clone())
            .collect()
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

    /// A banner event arrives without the button having been pressed (the banner may be gone).
    fn banner_event(&self, event: HostEvent) {
        self.controller
            .handle_action(self.id(), ShellAction::Banner(event));
    }

    fn history_event(&self, event: HistoryEvent) {
        self.controller
            .handle_action(self.id(), ShellAction::History(event));
    }

    /// The rows of the history list (the sizes of these tests are all below 1 KB).
    fn version_rows(&self) -> Vec<String> {
        let mut rows = self.names(Role::ListItem);
        rows.retain(|row| row.ends_with(" bytes"));
        rows
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

    fn answer(&self, button: MessageBoxResult) {
        self.services
            .answer(DialogKind::MessageBox, Answer::Button(button));
    }

    /// The texts of the dialogs shown so far.
    fn messages(&self) -> Vec<String> {
        self.services
            .dialog_log()
            .into_iter()
            .map(|dialog| dialog.message)
            .collect()
    }

    /// The state database through a connection of its own, to break it.
    fn database(&self) -> rusqlite::Connection {
        let path = self.controller.core().borrow().paths.state_db();
        rusqlite::Connection::open(path).unwrap()
    }
}

// ----- banner flows ----------------------------------------------------------------------------

#[test]
fn an_import_loss_with_a_detail_is_listed_with_it() {
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "plain\n"), ("other.md", "imported\n")],
        nothing,
        |rig| {
            let other = rig.path().with_file_name("other.md");
            let mut loaded = load_file(&RealFs, &other, None).unwrap();
            loaded.losses = vec![
                Loss {
                    kind: LossKind::Fields,
                    count: 2,
                    detail: Some("TOC".to_owned()),
                },
                Loss {
                    kind: LossKind::Comments,
                    count: 1,
                    detail: None,
                },
            ];
            let entry = rig.controller.docs().open_loaded(&other, loaded);
            rig.window().view.borrow_mut().add_tab(entry, None);
            rig.controller.refresh_all();
            assert_eq!(rig.banners(), [BannerKind::ImportLoss]);

            rig.press_in(BannerKind::ImportLoss, "Details");
            let i18n = rig.controller.i18n();
            let with_detail = format!("{} (TOC)", i18n.text(loss_message_id(LossKind::Fields)));
            let without = i18n.text(loss_message_id(LossKind::Comments));
            let rows = rig.names(Role::ListItem);
            assert!(rows.contains(&with_detail), "{with_detail:?} in {rows:?}");
            assert!(rows.contains(&without), "{without:?} in {rows:?}");
        },
    );
}

#[test]
fn the_conflict_banner_cannot_be_closed_and_a_window_without_tabs_ignores_banner_buttons() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "base\n")], nothing, |rig| {
        let path = rig.path();
        rig.set_text("base\nmine\n");
        std::fs::write(&path, "base\ntheirs\n").unwrap();
        rig.controller.check_external_changes(rig.id());
        assert_eq!(rig.banners(), [BannerKind::Conflict]);
        let entry = rig.active();

        // The user has to choose: a close request changes nothing.
        rig.banner_event(HostEvent::Dismissed(BannerKind::Conflict));
        assert_eq!(rig.banners(), [BannerKind::Conflict]);
        assert_eq!(entry.external(), ExternalState::Conflict);

        // Without a tab there is no document the buttons could act on.
        rig.close_all_tabs();
        assert!(rig.banners().is_empty());
        for action in [
            BannerAction::AllowEditing,
            BannerAction::KeepMine,
            BannerAction::LoadTheirs,
            BannerAction::Compare,
            BannerAction::DontLoadRemoteImages,
        ] {
            rig.banner_event(HostEvent::Action {
                kind: BannerKind::Conflict,
                action,
            });
        }
        rig.banner_event(HostEvent::Dismissed(BannerKind::ReadOnly));
        assert_eq!(entry.external(), ExternalState::Conflict);
        assert_eq!(entry.document().borrow().text(), "base\nmine\n");
        assert!(!rig.controller.remote_images().is_disabled(entry.id()));
        assert!(!rig.history_open());
    });
}

#[test]
fn allow_editing_tells_the_user_when_the_permissions_still_refuse_or_the_file_is_gone() {
    let faults = Arc::new(FaultFs::new());
    let installed = Arc::clone(&faults);
    with_rig(
        NO_AUTOSAVE,
        &[("a.md", "locked\n")],
        move |core, work| {
            set_read_only(&work.join("a.md"), true);
            install(core, installed);
        },
        |rig| {
            assert_eq!(rig.banners(), [BannerKind::ReadOnly]);

            // The attribute goes, but the file is still not writable (an ACL).
            faults.fail(FsOp::IsWritable, FsFailure::AccessDenied, 1);
            rig.answer(MessageBoxResult::Ok);
            rig.press_in(BannerKind::ReadOnly, "Allow editing");
            assert_eq!(rig.messages(), ["Access to the file was denied."]);
            assert!(rig.active().is_read_only());
            assert_eq!(rig.banners(), [BannerKind::ReadOnly]);

            // The file cannot be looked at at all.
            faults.fail(FsOp::Metadata, FsFailure::NotFound, 1);
            rig.answer(MessageBoxResult::Ok);
            rig.press_in(BannerKind::ReadOnly, "Allow editing");
            assert_eq!(rig.messages().len(), 2);
            assert_eq!(rig.messages()[1], FILE_ERROR);
            assert!(rig.active().is_read_only());
        },
    );
}

#[test]
fn a_conflict_over_a_deleted_file_reports_compare_and_load_theirs() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "base\n")], nothing, |rig| {
        let path = rig.path();
        rig.set_text("base\nmine\n");
        std::fs::write(&path, "base\ntheirs\n").unwrap();
        rig.controller.check_external_changes(rig.id());
        assert_eq!(rig.banners(), [BannerKind::Conflict]);
        std::fs::remove_file(&path).unwrap();

        rig.answer(MessageBoxResult::Ok);
        rig.press_in(BannerKind::Conflict, "Compare");
        assert_eq!(rig.messages(), [NOT_FOUND]);
        assert!(!rig.history_open(), "there is nothing to compare with");

        rig.answer(MessageBoxResult::Ok);
        rig.press_in(BannerKind::Conflict, "Load theirs");
        assert_eq!(rig.messages(), [NOT_FOUND, NOT_FOUND]);
        // Nothing was decided: the edits and the banner stay.
        assert_eq!(rig.text(), "base\nmine\n");
        assert_eq!(rig.active().external(), ExternalState::Conflict);
        assert_eq!(rig.banners(), [BannerKind::Conflict]);
    });
}

#[test]
fn a_file_that_cannot_be_reloaded_is_reported_and_an_unknown_window_checks_nothing() {
    let faults = Arc::new(FaultFs::new());
    let installed = Arc::clone(&faults);
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "one\n")],
        move |core, _| install(core, installed),
        |rig| {
            std::fs::write(rig.path(), "one\ntwo\n").unwrap();
            rig.controller.check_external_changes(WindowId(u64::MAX));
            assert_eq!(rig.text(), "one\n", "no window, no check");

            faults.fail(FsOp::Read, FsFailure::AccessDenied, 1);
            rig.answer(MessageBoxResult::Ok);
            rig.controller.check_external_changes(rig.id());
            assert_eq!(rig.messages(), [FILE_ERROR]);
            assert_eq!(rig.text(), "one\n");

            // The next check reads the file again.
            rig.controller.check_external_changes(rig.id());
            assert_eq!(rig.text(), "one\ntwo\n");
            assert_eq!(rig.messages().len(), 1);
        },
    );
}

#[test]
fn a_file_that_stays_unreadable_is_reported_once_and_again_after_it_was_read() {
    let faults = Arc::new(FaultFs::new());
    let installed = Arc::clone(&faults);
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "one\n")],
        move |core, _| install(core, installed),
        |rig| {
            // A sync client keeps the file locked: every activation finds it changed and cannot
            // read it. Closing the box activates the window again, so only the first is shown.
            std::fs::write(rig.path(), "one\ntwo\n").unwrap();
            faults.fail_always(FsOp::Read, FsFailure::AccessDenied);
            rig.answer(MessageBoxResult::Ok);
            for _ in 0..3 {
                rig.controller.check_external_changes(rig.id());
            }
            assert_eq!(rig.messages(), [FILE_ERROR]);
            assert_eq!(rig.text(), "one\n");

            // The file can be read again: it is reloaded, quietly.
            faults.clear();
            rig.controller.check_external_changes(rig.id());
            assert_eq!(rig.text(), "one\ntwo\n");
            assert_eq!(rig.messages().len(), 1);

            // A later failure is news again.
            std::fs::write(rig.path(), "one\ntwo\nthree\n").unwrap();
            faults.fail_always(FsOp::Read, FsFailure::AccessDenied);
            rig.answer(MessageBoxResult::Ok);
            rig.controller.check_external_changes(rig.id());
            rig.controller.check_external_changes(rig.id());
            assert_eq!(rig.messages(), [FILE_ERROR, FILE_ERROR]);

            // The file is put back as the document has it: a check that finds nothing to do ends
            // the failure as well.
            faults.clear();
            std::fs::write(rig.path(), "one\ntwo\n").unwrap();
            rig.controller.check_external_changes(rig.id());
            std::fs::write(rig.path(), "one\ntwo\nfour\n").unwrap();
            faults.fail_always(FsOp::Read, FsFailure::AccessDenied);
            rig.answer(MessageBoxResult::Ok);
            rig.controller.check_external_changes(rig.id());
            assert_eq!(rig.messages().len(), 3);
        },
    );
}

#[test]
fn the_update_banners_download_button_without_an_offer_opens_nothing() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "one\n")], nothing, |rig| {
        rig.banner_event(HostEvent::Action {
            kind: BannerKind::Update,
            action: BannerAction::Download,
        });
        assert!(rig.services.shell_log().is_empty());
    });
}

// ----- a save that loses the file --------------------------------------------------------------

/// A disk whose replace removes the target and then fails, as `ReplaceFileW` does when it cannot
/// move the replacement into place, and whose moves all fail: a save leaves the new text in its
/// temp file only.
#[derive(Debug)]
struct LosingFs;

impl FileSystem for LosingFs {
    fn read(&self, path: &Path) -> Result<Vec<u8>, FsError> {
        RealFs.read(path)
    }
    fn read_prefix(&self, path: &Path, max: usize) -> Result<Vec<u8>, FsError> {
        RealFs.read_prefix(path, max)
    }
    fn metadata(&self, path: &Path) -> Result<FileMeta, FsError> {
        RealFs.metadata(path)
    }
    fn write_new(&self, path: &Path, data: &[u8]) -> Result<(), FsError> {
        RealFs.write_new(path, data)
    }
    fn replace_file(&self, target: &Path, _replacement: &Path) -> Result<(), FsError> {
        RealFs.delete_file(target)?;
        Err(FsError::ReplaceUnsupported(target.to_path_buf()))
    }
    fn move_replace(&self, _from: &Path, to: &Path) -> Result<(), FsError> {
        Err(FsError::AccessDenied(to.to_path_buf()))
    }
    fn rename(&self, _from: &Path, to: &Path) -> Result<(), FsError> {
        Err(FsError::AccessDenied(to.to_path_buf()))
    }
    fn copy_file(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        RealFs.copy_file(from, to)
    }
    fn delete_file(&self, path: &Path) -> Result<(), FsError> {
        RealFs.delete_file(path)
    }
    fn remove_dir(&self, path: &Path) -> Result<(), FsError> {
        RealFs.remove_dir(path)
    }
    fn create_dir(&self, path: &Path) -> Result<(), FsError> {
        RealFs.create_dir(path)
    }
    fn create_dir_all(&self, path: &Path) -> Result<(), FsError> {
        RealFs.create_dir_all(path)
    }
    fn list_dir(&self, path: &Path) -> Result<Vec<DirEntry>, FsError> {
        RealFs.list_dir(path)
    }
    fn set_attributes(&self, path: &Path, attrs: Attributes) -> Result<(), FsError> {
        RealFs.set_attributes(path, attrs)
    }
    fn is_writable(&self, path: &Path) -> Result<bool, FsError> {
        RealFs.is_writable(path)
    }
    fn recycle(&self, paths: &[PathBuf]) -> Result<(), FsError> {
        RealFs.recycle(paths)
    }
}

#[test]
fn a_save_that_loses_the_file_tells_the_user_where_the_new_text_is() {
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "old\n")],
        |core, _| install(core, Arc::new(LosingFs)),
        |rig| {
            rig.set_text("new\n");
            rig.answer(MessageBoxResult::Ok);

            let outcome = rig.controller.run_command(rig.id(), CommandId::FileSave);

            let Dispatch::Failed(DocError::Save(SaveError::TempKept { temp, .. })) = &outcome
            else {
                panic!("expected the temp file to be kept: {outcome:?}");
            };
            assert_eq!(std::fs::read_to_string(temp).unwrap(), "new\n");
            assert!(!rig.path().exists(), "the original file is gone");
            let expected = localizer_for("en").format(
                "app-error-temp-kept",
                &[("path", Arg::Str(&temp.to_string_lossy()))],
            );
            assert!(expected.contains(&*temp.to_string_lossy()), "{expected}");
            assert_eq!(rig.messages(), [expected]);
            assert!(rig.active().is_dirty());
        },
    );
}

// ----- the recovery offer ----------------------------------------------------------------------

/// Recovery documents of a crashed session that nobody was offered.
fn unoffered_recovery(core: &mut AppCore, work: &Path) {
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
    assert_eq!(core.recovery.list().unwrap().len(), 2);
    core.recoverable.clear();
}

fn recovery_entries(rig: &Rig) -> usize {
    rig.controller.docs().env().recovery.list().unwrap().len()
}

#[test]
fn restore_and_discard_without_an_offer_leave_the_recovery_folder_alone() {
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "notes on disk\n")],
        unoffered_recovery,
        |rig| {
            assert!(rig.banners().is_empty());
            for action in [BannerAction::Restore, BannerAction::Discard] {
                rig.banner_event(HostEvent::Action {
                    kind: BannerKind::Recovery,
                    action,
                });
            }
            assert_eq!(rig.tab_texts(), ["notes on disk\n"]);
            assert_eq!(recovery_entries(rig), 2);
        },
    );
}

#[test]
fn nothing_is_offered_while_no_window_exists() {
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "notes on disk\n")],
        unoffered_recovery,
        |rig| {
            let listed = rig.controller.docs().env().recovery.list().unwrap();
            rig.controller.core().borrow_mut().recoverable = listed;
            rig.controller.close_window(rig.id());
            assert!(rig.controller.windows().is_empty());

            rig.controller.open_plan(&StartupPlan::default());
            assert!(rig.controller.windows().is_empty());
            assert_eq!(
                rig.controller.core().borrow().recoverable.len(),
                2,
                "the documents stay for the next window"
            );
        },
    );
}

// ----- the history panel -----------------------------------------------------------------------

/// What `HistoryHost::event` needs besides the host.
struct Env {
    theme: Theme,
    engine: MonoTextEngine,
    focus: FocusManager,
    capture: PointerCapture,
    requests: Requests,
}

impl Env {
    fn new() -> Self {
        Self {
            theme: Theme::light(DEFAULT_ACCENT),
            engine: MonoTextEngine::default(),
            focus: FocusManager::new(),
            capture: PointerCapture::new(),
            requests: Requests::new(),
        }
    }

    fn deliver(&mut self, host: &mut HistoryHost, event: &Event) -> EventResult {
        let mut env = OverlayEnv {
            theme: &self.theme,
            scale: 1.0,
            text: &self.engine,
            focus: &mut self.focus,
            capture: &mut self.capture,
            requests: &mut self.requests,
        };
        host.event(event, &mut env)
    }

    /// Whether something asked for a repaint since the last call.
    fn repaint_requested(&mut self) -> bool {
        !self.requests.take_invalidation().is_clean()
    }
}

fn pointer_move(at: Point) -> Event {
    Event::Pointer(PointerEvent::mouse(PointerEventKind::Move, None, at))
}

#[test]
fn the_history_panel_tracks_the_pointer_and_swallows_keys_while_open() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "one\n")], nothing, |rig| {
        rig.snapshot("zero\n", T1);
        assert_eq!(
            format!("{:?}", rig.window().view.borrow().history()),
            "HistoryHost { open: false, session: None, .. }"
        );
        rig.controller.open_history(rig.id(), None);
        let rows = rig.version_rows();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = rig.find(Role::ListItem, &rows[0]).bounds.center();

        let window = rig.window();
        let mut env = Env::new();
        let events = {
            let mut view = window.view.borrow_mut();
            let host = view.history_mut();
            assert!(format!("{host:?}").starts_with("HistoryHost { open: true, session: Some("));

            // Over a row: the list highlights it.
            assert!(env.deliver(host, &pointer_move(row)).handled);
            assert!(env.repaint_requested());
            // Away from the panel: the panel is told the pointer left, once.
            assert!(
                !env.deliver(host, &pointer_move(Point::new(-50.0, -50.0)))
                    .handled
            );
            assert!(env.repaint_requested(), "the highlight went away");
            assert!(
                !env.deliver(host, &pointer_move(Point::new(-60.0, -60.0)))
                    .handled
            );
            assert!(!env.repaint_requested());

            // Focus changes are not the panel's business; frames are offered, not consumed.
            assert!(!env.deliver(host, &Event::Focus(FocusEvent::Gained)).handled);
            let frame = Event::AnimationFrame(Duration::from_millis(16));
            assert!(!env.deliver(host, &frame).handled);
            // Typing never reaches the editor underneath.
            assert!(
                env.deliver(host, &Event::Text(TextInput("x".to_owned())))
                    .handled
            );
            // Escape closes the panel (through the controller).
            let escape = Event::Key(KeyEvent::press(vk::ESCAPE, Modifiers::NONE));
            assert!(env.deliver(host, &escape).handled);
            host.take_events()
        };
        assert!(matches!(events.as_slice(), [HistoryEvent::Close]));
        for event in events {
            rig.history_event(event);
        }
        assert!(!rig.history_open());
        assert_eq!(
            format!("{:?}", rig.window().view.borrow().history()),
            "HistoryHost { open: false, session: None, .. }"
        );
    });
}

#[test]
fn history_requests_without_an_open_panel_a_window_or_a_tab_do_nothing() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "one\n")], nothing, |rig| {
        rig.controller.open_history(WindowId(u64::MAX), None);
        rig.history_event(HistoryEvent::Selected(5));
        rig.history_event(HistoryEvent::Restore(5));
        assert!(!rig.history_open());
        assert_eq!(rig.text(), "one\n");

        // Closing what is not open asks for no refocus.
        let window = rig.window();
        window.view.borrow_mut().history_mut().close();
        assert!(!window.view.borrow_mut().history_mut().take_refocus());

        rig.close_all_tabs();
        rig.controller.open_history(rig.id(), None);
        assert!(!rig.history_open());
    });
}

#[test]
fn a_document_without_a_file_has_no_versions_and_cannot_restore_one() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "one\n")], nothing, |rig| {
        let entry = rig.controller.docs().new_untitled();
        rig.window().view.borrow_mut().add_tab(entry, None);
        rig.controller.refresh_all();

        rig.controller.open_history(rig.id(), None);
        assert!(rig.history_open());
        assert!(
            rig.window()
                .view
                .borrow()
                .history()
                .panel()
                .unwrap()
                .entries()
                .is_empty()
        );

        rig.answer(MessageBoxResult::Ok);
        rig.history_event(HistoryEvent::Restore(1));
        assert_eq!(rig.messages(), ["The document has no file yet."]);
        assert!(rig.history_open());
    });
}

#[test]
fn versions_that_vanish_or_a_broken_database_are_reported_and_the_panel_stays() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "one\n")], nothing, |rig| {
        let old = rig.snapshot("zero\n", T1);
        rig.controller.open_history(rig.id(), None);
        assert_eq!(rig.version_rows().len(), 1);
        let database = rig.database();

        // The version is gone between listing and selecting / restoring it.
        database
            .execute_batch("DELETE FROM history_snapshots")
            .unwrap();
        rig.answer(MessageBoxResult::Ok);
        rig.history_event(HistoryEvent::Selected(old));
        rig.answer(MessageBoxResult::Ok);
        rig.history_event(HistoryEvent::Restore(old));
        assert_eq!(rig.messages(), [VERSION_MISSING, VERSION_MISSING]);
        assert!(rig.history_open());
        assert_eq!(rig.text(), "one\n");

        // The table itself is gone.
        database
            .execute_batch("DROP TABLE history_snapshots")
            .unwrap();
        rig.answer(MessageBoxResult::Ok);
        rig.history_event(HistoryEvent::Selected(old));
        rig.answer(MessageBoxResult::Ok);
        rig.history_event(HistoryEvent::Restore(old));
        rig.answer(MessageBoxResult::Ok);
        rig.controller.open_history(rig.id(), None);
        assert_eq!(
            rig.messages()[2..],
            [FILE_ERROR, FILE_ERROR, FILE_ERROR],
            "{:?}",
            rig.messages()
        );
        assert!(rig.history_open(), "the panel opens without versions");
        assert_eq!(rig.text(), "one\n");
    });
}

#[test]
fn a_diff_that_arrives_after_the_panel_closed_is_dropped() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "one\n")], nothing, |rig| {
        let old = rig.snapshot("zero\n", T1);
        rig.controller.open_history(rig.id(), None);
        rig.history_event(HistoryEvent::Selected(old));
        rig.controller.close_history(&rig.window());
        rig.settle();
        assert!(!rig.history_open());
        assert_eq!(rig.text(), "one\n");
    });
}

// ----- the inspector and the remote images -----------------------------------------------------

#[test]
fn inspector_clicks_without_a_feed_use_the_offset_the_panel_sent() {
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "# Alpha\n\nbody text\n")],
        nothing,
        |rig| {
            let id = rig.id();
            // The inspector was never shown: no heading list, no backlinks.
            rig.controller.handle_action(
                id,
                ShellAction::Inspector(InspectorEvent::HeadingClicked {
                    index: 0,
                    offset: 9,
                }),
            );
            assert_eq!(rig.caret(), 9);
            rig.controller.handle_action(
                id,
                ShellAction::Inspector(InspectorEvent::BacklinkClicked(0)),
            );
            assert_eq!(rig.window().view.borrow().tabs().len(), 1);

            // Shown, and then the last tab goes: the feed is cleared once and stays clear.
            rig.controller
                .run_command(id, CommandId::ViewToggleInspector);
            let entry = rig.active();
            rig.close_all_tabs();
            rig.controller.refresh_all();
            let caret = entry.document().borrow().primary_selection().head;
            rig.controller.handle_action(
                id,
                ShellAction::Inspector(InspectorEvent::HeadingClicked {
                    index: 0,
                    offset: 2,
                }),
            );
            assert_eq!(entry.document().borrow().primary_selection().head, caret);
        },
    );
}

#[test]
fn an_index_that_cannot_be_asked_lists_no_backlinks() {
    use std::os::windows::ffi::OsStringExt;

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("state.db")).unwrap();
    // A name the index cannot hold (not Unicode).
    let name = std::ffi::OsString::from_wide(&[0x61, 0xD800, 0x2E, 0x6D, 0x64]);
    let file = tmp.path().join(name);
    assert!(backlinks_of(&store, tmp.path(), &file).is_empty());
}

#[test]
fn no_index_job_is_queued_for_a_window_that_is_closing() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "# One\n")], nothing, |rig| {
        let id = rig.id();
        let work = rig.path().parent().unwrap().to_path_buf();
        rig.controller.core().borrow().workers.cancel_window(id);
        rig.controller.show_workspace(&rig.window(), &work);
        rig.controller
            .run_command(id, CommandId::ViewToggleInspector);
        rig.controller.refresh_all();
        assert_eq!(rig.controller.core().borrow().workers.pending(), 0);
    });
}

#[test]
fn click_to_load_needs_a_document() {
    with_rig(NO_AUTOSAVE, &[("notes.md", "one\n")], nothing, |rig| {
        let id = rig.id();
        let doc = rig.active().id();
        let policy = Rc::clone(rig.controller.remote_images());
        let load = |dest: &str| {
            ShellAction::Editor(
                0,
                EditorEvent::Media(MediaEvent::LoadImage {
                    dest: dest.to_owned(),
                    range: TextRange { start: 0, end: 0 },
                }),
            )
        };
        let before = policy.epoch();
        rig.controller
            .handle_action(id, load("http://example.com/a.png"));
        assert!(policy.url_allowed(doc, "http://example.com/a.png"));
        assert_eq!(policy.epoch(), before + 1);

        rig.close_all_tabs();
        rig.controller
            .handle_action(id, load("http://example.com/b.png"));
        assert!(!policy.url_allowed(doc, "http://example.com/b.png"));
        assert_eq!(policy.epoch(), before + 1);
    });
}

// ----- popup menus -----------------------------------------------------------------------------

#[test]
fn popup_choices_that_act_on_the_active_document_do_nothing_without_one() {
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", "> [!NOTE]\n> text\n")],
        nothing,
        |rig| {
            let id = rig.id();
            let anchor = Rect::new(10.0, 10.0, 0.0, 0.0);
            let actions = [
                ShellAction::Editor(
                    0,
                    EditorEvent::CalloutMenu {
                        range: TextRange { start: 0, end: 0 },
                        kind: CalloutKind::Note,
                        anchor,
                    },
                ),
                ShellAction::Status(StatusEvent::EncodingClicked { anchor }),
                ShellAction::Status(StatusEvent::LineEndingClicked { anchor }),
            ];
            let popups: Vec<_> = actions
                .iter()
                .map(|action| rig.controller.popup_for(id, action).expect("a menu"))
                .collect();
            let entry = rig.active();
            let shape = |entry: &DocEntry| {
                let format = entry.format();
                (
                    format.encoding.name().to_owned(),
                    format.bom,
                    format.line_ending,
                )
            };
            let before = shape(&entry);
            let text = entry.document().borrow().text();

            // The menus were opened on the document; the tab is gone when an item is chosen.
            rig.close_all_tabs();
            for popup in &popups {
                for item in popup.ids() {
                    rig.controller.run_popup_choice(id, popup, item);
                }
            }
            assert_eq!(shape(&entry), before);
            assert_eq!(entry.document().borrow().text(), text);
        },
    );
}
