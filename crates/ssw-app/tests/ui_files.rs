//! The files of a workspace (spec §3.2, §4.3, §5): link updates after a rename or a move,
//! wikilinks to notes, the workspace watchers and index upkeep, the `[[` completion sources and
//! the background open of OneDrive placeholders. The real controller runs over a real startup in
//! a temporary profile with scripted dialogs; files are real files in a temporary folder and
//! what the user would see is read back from the bytes on disk, the document text, the sidebar
//! rows, the completion rows and the dialogs the app showed.

use std::cell::RefCell;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::{Args, Launch, LaunchItem};
use ssw_app::documents::{DocMode, canonical_path, replace_text};
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::{Controller, Request};
use ssw_app::ui::files::{Move, absolute, link_updates, moves_for, relative_to};
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices, ShellRecord};
use ssw_app::ui::shell_view::ShellAction;
use ssw_app::ui::tree_source::WorkspaceTree;
use ssw_core::edit::Selection;
use ssw_core::links::RelPath;
use ssw_io::fs::{Attributes, FileSystem, RealFs};
use ssw_io::watch::{FsEvent, FsEventKind};
use ssw_ui::editor::{EditorEvent, LinkTarget};
use ssw_ui::geometry::{Rect, Size};
use ssw_ui::services::{
    Clock, Dialogs, FakeClock, MemoryClipboard, MessageBoxOptions, MessageBoxResult,
    OpenFileOptions, PickFolderOptions, SaveFileOptions, ServiceError,
};
use ssw_ui::shell::popups::{WikilinkAccept, WikilinkEvent};
use ssw_ui::shell::sidebar::{SidebarEvent, SidebarOptions, TreeSource};
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::taskbar::Progress;
use ssw_win::window::current_os_version;

/// Autosave would write the unsaved edits these tests keep.
const NO_AUTOSAVE: &str = r#"{"general":{"autosave":false}}"#;

// ----- the harness -----------------------------------------------------------------------------

fn process(args: &Args) -> ProcessEnv {
    ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    }
}

type Hook = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

/// The scripted dialogs, plus something that happens in the world while a message box is open
/// (another program changing a file).
struct Hooked {
    inner: Box<dyn Dialogs>,
    hook: Hook,
}

impl std::fmt::Debug for Hooked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hooked")
    }
}

impl Dialogs for Hooked {
    fn open_file(
        &mut self,
        options: &OpenFileOptions,
    ) -> Result<Option<Vec<PathBuf>>, ServiceError> {
        self.inner.open_file(options)
    }

    fn save_file(&mut self, options: &SaveFileOptions) -> Result<Option<PathBuf>, ServiceError> {
        self.inner.save_file(options)
    }

    fn pick_folder(
        &mut self,
        options: &PickFolderOptions,
    ) -> Result<Option<PathBuf>, ServiceError> {
        self.inner.pick_folder(options)
    }

    fn message_box(
        &mut self,
        options: &MessageBoxOptions,
    ) -> Result<MessageBoxResult, ServiceError> {
        let hook = self.hook.borrow_mut().take();
        if let Some(hook) = hook {
            hook();
        }
        self.inner.message_box(options)
    }
}

struct Rig {
    controller: Rc<Controller>,
    services: ScriptedServices,
    hook: Hook,
    /// The workspace folder (canonical).
    work: PathBuf,
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
        let hook: Hook = Rc::new(RefCell::new(None));
        let dialogs = Hooked {
            inner: self.services.dialogs(),
            hook: Rc::clone(&hook),
        };
        let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let plan = core.plan.clone();
        let controller = Controller::new(
            core,
            Box::new(dialogs),
            self.services.shell(),
            clipboard,
            clock,
        );
        controller.open_plan(&plan);
        let rig = Rig {
            controller,
            services: self.services.clone(),
            hook,
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

fn text_file(name: &str, text: &str) -> (String, Vec<u8>) {
    (name.to_owned(), text.as_bytes().to_vec())
}

fn byte_file(name: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    (name.to_owned(), bytes.to_vec())
}

fn nothing(_: &mut AppCore, _: &Path) {}

/// Runs `body` over a started application whose first window shows the file `open` of the
/// workspace folder holding `files`.
fn with_rig(
    files: Vec<(String, Vec<u8>)>,
    open: &str,
    prepare: impl FnOnce(&mut AppCore, &Path),
    body: impl FnOnce(&Rig),
) {
    let tmp = tempfile::tempdir().unwrap();
    let created = tmp.path().join("work");
    std::fs::create_dir_all(&created).unwrap();
    let work = canonical_path(&created);
    for (name, bytes) in &files {
        let path = work.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let folder = tmp.path().join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("settings.json"), NO_AUTOSAVE).unwrap();
    let mut args = Args {
        paths: vec![work.join(open)],
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

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn set_read_only(path: &Path, read_only: bool) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(read_only);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn make_placeholder(path: &Path) {
    let attrs = RealFs.metadata(path).unwrap().attrs;
    RealFs
        .set_attributes(path, attrs.with(Attributes::OFFLINE))
        .unwrap();
}

/// A directory junction (needs no privilege) at `link` pointing at `target`.
fn junction(link: &Path, target: &Path) {
    let out = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(out.status.success(), "mklink /J failed: {out:?}");
}

impl Rig {
    fn window(&self) -> Rc<ssw_app::ui::controller::WindowCell> {
        Rc::clone(&self.controller.windows()[0])
    }

    fn id(&self) -> ssw_app::workers::WindowId {
        self.window().id
    }

    fn active(&self) -> Rc<ssw_app::documents::DocEntry> {
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

    fn tab_count(&self) -> usize {
        self.window().view.borrow().tabs().len()
    }

    fn caret(&self) -> usize {
        self.window()
            .view
            .borrow()
            .active_tab()
            .unwrap()
            .editor
            .selection()
            .head
    }

    /// Replaces the text of the active document with one unsaved edit.
    fn set_text(&self, text: &str) {
        let changed = replace_text(&mut self.active().document().borrow_mut(), text);
        assert!(changed, "the text differs");
        self.controller.refresh_all();
    }

    /// Shows the workspace folder in the sidebar with every folder expanded.
    fn show(&self) {
        let window = self.window();
        self.controller.show_workspace(&window, &self.work);
        window
            .view
            .borrow_mut()
            .sidebar_mut()
            .expect("the sidebar exists")
            .expand_all();
    }

    fn rows(&self) -> Vec<String> {
        self.window()
            .view
            .borrow()
            .sidebar()
            .expect("the sidebar exists")
            .row_names()
    }

    fn has_row(&self, stem: &str) -> bool {
        let with_extension = format!("{stem}.md");
        self.rows()
            .iter()
            .any(|row| row == stem || *row == with_extension)
    }

    /// One turn of the message loop's housekeeping.
    fn pump(&self) {
        self.controller.core().borrow_mut().pump();
        self.controller.pump_files();
        self.controller.refresh_all();
    }

    fn wait_for(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.pump();
            if done() {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Delivers worker results until nothing is pending.
    fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.pump();
            if self.controller.core().borrow().workers.pending() == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "the workers did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The files of the workspace index (relative paths).
    fn indexed(&self) -> Vec<String> {
        self.controller
            .docs()
            .env()
            .store
            .borrow()
            .index_files(&self.work)
            .unwrap()
            .iter()
            .map(|file| file.rel.clone())
            .collect()
    }

    /// Waits until the workspace index lists `count` files.
    fn index_ready(&self, count: usize) {
        self.wait_for("the index", || self.indexed().len() == count);
        self.settle();
    }

    /// The sidebar key of `path` (every folder is expanded).
    fn key(&self, path: &Path) -> u64 {
        let window = self.window();
        let tree = window.tree.borrow();
        let paths = tree.as_ref().expect("a workspace is shown");
        let wanted = path.to_string_lossy().to_lowercase();
        (0..)
            .map_while(|key| paths.path(key).map(|found| (key, found)))
            .find(|(_, found)| found.to_string_lossy().to_lowercase() == wanted)
            .map(|(key, _)| key)
            .unwrap_or_else(|| panic!("no sidebar entry for {path:?}"))
    }

    fn rename(&self, path: &Path, name: &str) {
        let key = self.key(path);
        self.controller.handle_action(
            self.id(),
            ShellAction::Sidebar(SidebarEvent::RenameCommitted {
                key,
                name: name.to_owned(),
            }),
        );
    }

    fn drop_entry(&self, source: u64, target_folder: u64, copy: bool) {
        self.controller.handle_action(
            self.id(),
            ShellAction::Sidebar(SidebarEvent::Dropped {
                source,
                target_folder,
                copy,
            }),
        );
    }

    fn answer(&self, button: MessageBoxResult) {
        self.services
            .answer(DialogKind::MessageBox, Answer::Button(button));
    }

    fn messages(&self) -> Vec<String> {
        self.services
            .dialog_log()
            .into_iter()
            .map(|record| record.message)
            .collect()
    }

    fn on_next_message(&self, hook: impl FnOnce() + 'static) {
        *self.hook.borrow_mut() = Some(Box::new(hook));
    }

    fn wiki(&self, target: &str, heading: Option<&str>) {
        let key = self.window().view.borrow().active_tab().unwrap().key;
        self.controller.handle_action(
            self.id(),
            ShellAction::Editor(
                key,
                EditorEvent::LinkActivated(LinkTarget::Wiki {
                    target: target.to_owned(),
                    heading: heading.map(str::to_owned),
                }),
            ),
        );
    }

    /// The progress the taskbar button was asked to show since the last call.
    fn progress(&self) -> Vec<Progress> {
        let mut shown = Vec::new();
        while let Some(request) = self.controller.take_request() {
            if let Request::TaskbarProgress(_, progress) = request {
                shown.push(progress);
            }
        }
        shown
    }

    /// The rows the `[[` completion list shows for `query` right now.
    fn completion(&self, query: &str) -> Vec<String> {
        let window = self.window();
        let mut view = window.view.borrow_mut();
        let list = view.overlays_mut().wikilink_mut();
        list.open(Rect::new(0.0, 0.0, 10.0, 10.0), query);
        list.list()
            .rows()
            .iter()
            .map(|row| row.label.clone())
            .collect()
    }
}

fn rel(path: &str) -> RelPath {
    RelPath::parse(path).unwrap()
}

fn rels(paths: &[&str]) -> Vec<RelPath> {
    paths.iter().map(|path| rel(path)).collect()
}

// ----- paths and the pure link update ------------------------------------------------------------

#[test]
fn paths_below_a_root_are_relative_ignoring_case_and_the_extended_prefix() {
    let root = Path::new(r"C:\work");
    assert_eq!(
        relative_to(root, Path::new(r"C:\Work\Sub\a.md")),
        Some(rel("Sub/a.md"))
    );
    assert_eq!(
        relative_to(root, Path::new(r"\\?\C:\work\a.md")),
        Some(rel("a.md"))
    );
    // The root itself, a sibling, a parent and a name no workspace path can hold.
    assert_eq!(relative_to(root, Path::new(r"C:\work")), None);
    assert_eq!(relative_to(root, Path::new(r"C:\other\a.md")), None);
    assert_eq!(relative_to(root, Path::new(r"C:\")), None);
    assert_eq!(relative_to(root, Path::new(r"C:\work\a.")), None);
    assert_eq!(
        absolute(root, &rel("a/b.md")),
        Path::new(r"C:\work").join("a").join("b.md")
    );
}

#[test]
fn a_folder_move_moves_every_file_below_it_wherever_the_case_differs() {
    let files = rels(&["notes/a.md", "Notes/sub/b.md", "notes2/c.md", "d.md"]);
    let moves = moves_for(&files, &rel("notes"), &rel("archive/new"), true);
    assert_eq!(
        moves,
        [
            Move {
                from: rel("notes/a.md"),
                to: rel("archive/new/a.md")
            },
            Move {
                from: rel("Notes/sub/b.md"),
                to: rel("archive/new/sub/b.md")
            },
        ]
    );
    // A file is just that file, whatever the list holds.
    assert_eq!(
        moves_for(&files, &rel("d.md"), &rel("e.md"), false),
        [Move {
            from: rel("d.md"),
            to: rel("e.md")
        }]
    );
}

fn texts<'a>(
    table: &'a [(&'a str, &'a str)],
    reads: &'a RefCell<Vec<String>>,
) -> impl FnMut(&RelPath) -> Option<String> + 'a {
    move |path| {
        reads.borrow_mut().push(path.as_str().to_owned());
        table
            .iter()
            .find(|(name, _)| *name == path.as_str())
            .map(|(_, text)| (*text).to_owned())
    }
}

#[test]
fn links_follow_a_rename_in_the_documents_that_point_at_it() {
    let files = rels(&["notes/a.md", "notes/b.md", "c.md"]);
    let moves = [(
        Move {
            from: rel("notes/a.md"),
            to: rel("notes/renamed.md"),
        },
        // The moved file itself is listed too (it links to itself): it is never changed.
        rels(&["notes/b.md", "c.md", "notes/a.md"]),
    )];
    let table = [
        ("notes/b.md", "see [a](a.md) and [[a]]\n"),
        ("c.md", "[x](notes/a.md)\n"),
        ("notes/a.md", "[self](a.md)\n"),
    ];
    let reads = RefCell::new(Vec::new());
    let updates = link_updates(&files, &moves, &mut texts(&table, &reads));
    let summary: Vec<(String, String, usize)> = updates
        .iter()
        .map(|update| {
            (
                update.path.as_str().to_owned(),
                update.text.clone(),
                update.links,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                "notes/b.md".to_owned(),
                "see [a](renamed.md) and [[renamed]]\n".to_owned(),
                2
            ),
            ("c.md".to_owned(), "[x](notes/renamed.md)\n".to_owned(), 1),
        ]
    );
    assert_eq!(updates[0].original, "see [a](a.md) and [[a]]\n");
    assert_eq!(*reads.borrow(), ["notes/b.md", "c.md"]);
}

#[test]
fn a_document_linking_to_several_moved_files_is_read_once_and_gets_every_link_changed() {
    let files = rels(&["notes/a.md", "notes/b.md", "c.md"]);
    let moves = [
        (
            Move {
                from: rel("notes/a.md"),
                to: rel("x/a.md"),
            },
            rels(&["c.md"]),
        ),
        (
            Move {
                from: rel("notes/b.md"),
                to: rel("x/b.md"),
            },
            rels(&["c.md"]),
        ),
    ];
    let table = [("c.md", "[a](notes/a.md) [b](notes/b.md)\n")];
    let reads = RefCell::new(Vec::new());
    let updates = link_updates(&files, &moves, &mut texts(&table, &reads));
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].text, "[a](x/a.md) [b](x/b.md)\n");
    assert_eq!(updates[0].links, 2);
    assert_eq!(reads.borrow().len(), 1);
}

#[test]
fn an_unreadable_document_is_asked_for_once_and_stays_as_it_is() {
    let files = rels(&["notes/a.md", "notes/b.md", "c.md", "d.md"]);
    let moves = [
        (
            Move {
                from: rel("notes/a.md"),
                to: rel("x/a.md"),
            },
            rels(&["c.md", "d.md"]),
        ),
        (
            Move {
                from: rel("notes/b.md"),
                to: rel("x/b.md"),
            },
            rels(&["c.md"]),
        ),
    ];
    // `c.md` cannot be read; `d.md` has no link to the file the index said it has.
    let table = [("d.md", "nothing to see\n")];
    let reads = RefCell::new(Vec::new());
    let updates = link_updates(&files, &moves, &mut texts(&table, &reads));
    assert!(updates.is_empty());
    assert_eq!(*reads.borrow(), ["c.md", "d.md"]);
}

#[test]
fn moves_apply_one_after_the_other() {
    // `a.md` becomes `b.md` and then `c.md`: the link follows both steps.
    let files = rels(&["a.md", "s.md"]);
    let moves = [
        (
            Move {
                from: rel("a.md"),
                to: rel("b.md"),
            },
            rels(&["s.md"]),
        ),
        (
            Move {
                from: rel("b.md"),
                to: rel("c.md"),
            },
            rels(&["s.md"]),
        ),
    ];
    let table = [("s.md", "[[a]]\n")];
    let reads = RefCell::new(Vec::new());
    let updates = link_updates(&files, &moves, &mut texts(&table, &reads));
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].text, "[[c]]\n");
    assert_eq!(updates[0].links, 2);
}

// ----- renames ---------------------------------------------------------------------------------

#[test]
fn a_rename_updates_the_links_of_open_and_closed_documents_in_their_own_format() {
    let open_text = "Open [t](target.md) and [[target]].\n";
    let files = vec![
        text_file("open.md", open_text),
        text_file("target.md", "# Target\n"),
        byte_file("bom.md", b"\xEF\xBB\xBFSee [t](target.md)\r\nsecond\r\n"),
        text_file("sub/deep.md", "[up](../target.md)\n"),
        text_file("unrelated.md", "nothing\n"),
    ];
    with_rig(files, "open.md", nothing, |rig| {
        rig.show();
        rig.index_ready(5);
        rig.answer(MessageBoxResult::Yes);
        rig.rename(&rig.work.join("target.md"), "renamed.md");

        assert_eq!(
            rig.messages(),
            ["The links in 3 other files point to renamed.md. Update them to its new location?"]
        );
        assert!(!rig.work.join("target.md").exists());
        assert_eq!(read(&rig.work.join("renamed.md")), "# Target\n");
        // The open document takes the change as an unsaved edit; the disk keeps the old text.
        assert_eq!(rig.text(), "Open [t](renamed.md) and [[renamed]].\n");
        assert!(rig.active().is_dirty());
        assert_eq!(read(&rig.work.join("open.md")), open_text);
        // One undo takes the whole change back.
        assert!(rig.active().document().borrow_mut().undo().unwrap());
        assert_eq!(rig.text(), open_text);
        // The closed files are rewritten with their BOM and line endings.
        assert_eq!(
            std::fs::read(rig.work.join("bom.md")).unwrap(),
            b"\xEF\xBB\xBFSee [t](renamed.md)\r\nsecond\r\n"
        );
        assert_eq!(read(&rig.work.join("sub/deep.md")), "[up](../renamed.md)\n");
        assert_eq!(read(&rig.work.join("unrelated.md")), "nothing\n");
    });
}

#[test]
fn a_declined_offer_changes_nothing_and_a_rename_nobody_links_to_asks_nothing() {
    let files = vec![
        text_file("open.md", "[t](target.md)\n"),
        text_file("target.md", "# Target\n"),
        text_file("lonely.md", "alone\n"),
    ];
    with_rig(files, "open.md", nothing, |rig| {
        rig.show();
        rig.index_ready(3);
        rig.answer(MessageBoxResult::No);
        rig.rename(&rig.work.join("target.md"), "renamed.md");
        assert_eq!(rig.messages().len(), 1);
        assert!(rig.work.join("renamed.md").exists());
        assert_eq!(rig.text(), "[t](target.md)\n");
        assert!(!rig.active().is_dirty());

        rig.rename(&rig.work.join("lonely.md"), "other.md");
        assert!(rig.work.join("other.md").exists());
        assert_eq!(rig.messages().len(), 1, "no link, no question");
    });
}

#[test]
fn renaming_a_folder_updates_the_links_to_the_files_in_it() {
    let files = vec![
        text_file("index.md", "[a](docs/a.md) [b](docs/sub/b.md) [[a]]\n"),
        text_file("docs/a.md", "# A\n[i](../index.md)\n"),
        text_file("docs/sub/b.md", "B\n"),
        text_file("top/x.md", "[a](../docs/a.md)\n"),
    ];
    with_rig(files, "index.md", nothing, |rig| {
        rig.show();
        rig.index_ready(4);
        rig.answer(MessageBoxResult::Yes);
        rig.rename(&rig.work.join("docs"), "manual");

        assert_eq!(
            rig.messages(),
            ["The links in 2 other files point to manual. Update them to its new location?"]
        );
        assert!(rig.work.join("manual").join("sub").join("b.md").exists());
        // The wikilink `[[a]]` still names exactly one file: it is not touched.
        assert_eq!(rig.text(), "[a](manual/a.md) [b](manual/sub/b.md) [[a]]\n");
        assert_eq!(read(&rig.work.join("top/x.md")), "[a](../manual/a.md)\n");
    });
}

#[test]
fn links_in_an_unsaved_document_follow_even_when_the_index_has_not_seen_them() {
    let files = vec![
        text_file("draft.md", "draft\n"),
        text_file("target.md", "# Target\n"),
    ];
    with_rig(files, "draft.md", nothing, |rig| {
        rig.set_text("draft [t](target.md)\n");
        rig.show();
        rig.index_ready(2);
        rig.answer(MessageBoxResult::Yes);
        rig.rename(&rig.work.join("target.md"), "moved.md");
        assert_eq!(
            rig.messages(),
            ["The links in 1 other file point to moved.md. Update them to its new location?"]
        );
        assert_eq!(rig.text(), "draft [t](moved.md)\n");
    });
}

#[test]
fn files_that_cannot_be_updated_are_counted_and_left_alone() {
    let lossy: &[u8] = b"\xEF\xBB\xBF[x](target.md)\n\xFF";
    let ansi: &[u8] = b"caf\xE9 [[target]]\n";
    let files = vec![
        text_file("target.md", "# Target\n"),
        byte_file("lossy.md", lossy),
        text_file("readonly.md", "[x](target.md)\n"),
        byte_file("ansi.md", ansi),
        text_file("changed.md", "[c](target.md)\n"),
        text_file("ok.md", "[o](target.md)\n"),
    ];
    with_rig(files, "target.md", nothing, |rig| {
        set_read_only(&rig.work.join("readonly.md"), true);
        rig.show();
        rig.index_ready(6);
        // Another program edits a file while the question is open.
        let changed = rig.work.join("changed.md");
        rig.on_next_message(move || std::fs::write(changed, "[c](target.md)\nmore\n").unwrap());
        rig.answer(MessageBoxResult::Yes);
        rig.answer(MessageBoxResult::Ok);
        rig.rename(&rig.work.join("target.md"), "\u{1F4DD}.md");

        let messages = rig.messages();
        assert_eq!(
            messages[0],
            "The links in 5 other files point to \u{1F4DD}.md. Update them to its new location?"
        );
        assert_eq!(
            messages[1],
            "4 files could not be updated. Their links still point to the old location."
        );
        assert_eq!(messages.len(), 2);
        // Read as UTF-8 with a bad byte, read-only, a legacy code page that has no such
        // character, and edited meanwhile: untouched.
        assert_eq!(std::fs::read(rig.work.join("lossy.md")).unwrap(), lossy);
        assert_eq!(read(&rig.work.join("readonly.md")), "[x](target.md)\n");
        assert_eq!(std::fs::read(rig.work.join("ansi.md")).unwrap(), ansi);
        assert_eq!(read(&rig.work.join("changed.md")), "[c](target.md)\nmore\n");
        // The one that could be updated was.
        let updated = read(&rig.work.join("ok.md"));
        assert!(!updated.contains("target.md"), "{updated}");
        set_read_only(&rig.work.join("readonly.md"), false);
    });
}

#[test]
fn an_unreadable_index_still_updates_the_open_documents_and_never_blocks_a_rename() {
    let files = vec![
        text_file("open.md", "[t](target.md)\n"),
        text_file("closed.md", "[t](target.md)\n"),
        text_file("target.md", "# Target\n"),
    ];
    with_rig(files, "open.md", nothing, |rig| {
        rig.show();
        rig.index_ready(3);
        let database = rig.controller.core().borrow().paths.state_db();
        let connection = rusqlite::Connection::open(&database).unwrap();

        // Without the table of links only the open documents are known to link to the file.
        connection.execute_batch("DROP TABLE index_links;").unwrap();
        rig.answer(MessageBoxResult::Yes);
        rig.rename(&rig.work.join("target.md"), "moved.md");
        assert_eq!(
            rig.messages(),
            ["The links in 1 other file point to moved.md. Update them to its new location?"]
        );
        assert_eq!(rig.text(), "[t](moved.md)\n");
        assert_eq!(read(&rig.work.join("closed.md")), "[t](target.md)\n");

        // Without the table of files nothing is planned, and the rename happens all the same;
        // wikilinks are resolved by looking at the folder.
        connection.execute_batch("DROP TABLE index_files;").unwrap();
        rig.rename(&rig.work.join("moved.md"), "final.md");
        assert!(rig.work.join("final.md").exists());
        assert_eq!(rig.messages().len(), 1);
        rig.wiki("final", None);
        assert_eq!(rig.path(), rig.work.join("final.md"));
        // The completion list has nothing to read either.
        rig.pump();
        assert!(rig.completion("").is_empty());
    });
}

#[test]
fn a_plan_exists_only_for_entries_of_the_shown_workspace() {
    let files = vec![text_file("a.md", "[b](b.md)\n"), text_file("b.md", "# B\n")];
    with_rig(files, "a.md", nothing, |rig| {
        let window = rig.window();
        let outside = rig.work.parent().unwrap().join("x.md");
        // No workspace is shown yet.
        assert!(
            rig.controller
                .plan_link_updates(&window, &rig.work.join("b.md"))
                .is_none()
        );
        rig.show();
        rig.index_ready(2);
        assert!(
            rig.controller
                .plan_link_updates(&window, &outside)
                .is_none()
        );
        // Nothing is asked without a plan, nor when the new place is outside the workspace.
        rig.controller.offer_link_updates(None, &outside);
        let plan = rig
            .controller
            .plan_link_updates(&window, &rig.work.join("b.md"));
        assert!(plan.is_some());
        rig.controller.offer_link_updates(plan, &outside);
        assert!(rig.messages().is_empty());
    });
}

// ----- drag and drop ---------------------------------------------------------------------------

#[test]
fn dropping_entries_moves_or_copies_them_and_reports_what_cannot_be_done() {
    let files = vec![
        text_file("b.md", "[a](a.md)\n"),
        text_file("a.md", "# A\n"),
        text_file("c.md", "# C\n"),
        text_file("folder/keep.md", "k\n"),
    ];
    with_rig(files, "b.md", nothing, |rig| {
        rig.show();
        rig.index_ready(4);
        let folder = rig.key(&rig.work.join("folder"));
        let a = rig.key(&rig.work.join("a.md"));
        let b = rig.key(&rig.work.join("b.md"));
        let c = rig.key(&rig.work.join("c.md"));

        // A move: the file lands in the folder and the links to it follow, on request.
        rig.answer(MessageBoxResult::Yes);
        rig.drop_entry(a, folder, false);
        assert!(!rig.work.join("a.md").exists());
        assert_eq!(read(&rig.work.join("folder/a.md")), "# A\n");
        assert_eq!(rig.text(), "[a](folder/a.md)\n");
        assert_eq!(
            rig.messages(),
            ["The links in 1 other file point to a.md. Update them to its new location?"]
        );

        // Ctrl held: a copy, twice (the second one gets another name).
        rig.drop_entry(c, folder, true);
        rig.drop_entry(c, folder, true);
        assert!(rig.work.join("c.md").exists());
        assert_eq!(read(&rig.work.join("folder/c.md")), "# C\n");
        assert_eq!(read(&rig.work.join("folder/c (2).md")), "# C\n");

        // Onto the folder it is in, onto a file, and entries nobody knows: nothing happens.
        rig.drop_entry(b, 0, false);
        rig.drop_entry(b, c, false);
        rig.drop_entry(9_999, folder, false);
        rig.drop_entry(b, 9_999, false);
        assert!(rig.work.join("b.md").exists());

        // A folder cannot go into itself, moved or copied.
        rig.answer(MessageBoxResult::Ok);
        rig.drop_entry(folder, folder, false);
        rig.answer(MessageBoxResult::Ok);
        rig.drop_entry(folder, folder, true);
        let messages = rig.messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[1],
            "A folder cannot be moved or copied into itself."
        );
        assert_eq!(
            messages[2],
            "A folder cannot be moved or copied into itself."
        );

        // The file vanished before it was dropped.
        std::fs::remove_file(rig.work.join("c.md")).unwrap();
        rig.answer(MessageBoxResult::Ok);
        rig.drop_entry(c, folder, false);
        assert_eq!(rig.messages()[3], "The file could not be read or written.");
    });
}

// ----- wikilinks -------------------------------------------------------------------------------

#[test]
fn a_wikilink_opens_the_note_and_moves_to_its_heading() {
    let note = "# Note\nstart\n\n## Second\ntail\n";
    let files = vec![
        text_file("note.md", note),
        text_file("target.md", "# Target\n\n## Second\n\ntext\n"),
    ];
    with_rig(files, "note.md", nothing, |rig| {
        // No workspace is shown and the index is empty: the folder of the file is looked at.
        rig.wiki("target", None);
        assert_eq!(rig.tab_count(), 2);
        assert_eq!(rig.path(), rig.work.join("target.md"));

        let target_text = rig.text();
        rig.wiki("target", Some("Second"));
        assert_eq!(
            rig.tab_count(),
            2,
            "the open note is shown, not opened again"
        );
        assert_eq!(rig.caret(), target_text.find("## Second").unwrap());

        // `[[#heading]]` stays in the current document; unknown headings and empty links do
        // nothing.
        rig.wiki("", Some("Target"));
        assert_eq!(rig.caret(), 0);
        rig.wiki("", Some("Second"));
        assert_eq!(rig.caret(), target_text.find("## Second").unwrap());
        rig.wiki("", Some("Nonexistent"));
        assert_eq!(rig.caret(), target_text.find("## Second").unwrap());
        rig.wiki("  ", None);
        assert_eq!(rig.tab_count(), 2);
        assert!(rig.messages().is_empty());
    });
}

#[test]
fn a_missing_note_is_created_inside_the_workspace_when_the_user_agrees() {
    let files = vec![text_file("note.md", "# Note\n")];
    with_rig(files, "note.md", nothing, |rig| {
        rig.show();
        // Declined: nothing is created.
        rig.answer(MessageBoxResult::No);
        rig.wiki("nope", None);
        assert!(!rig.work.join("nope.md").exists());
        assert_eq!(rig.tab_count(), 1);

        rig.answer(MessageBoxResult::Yes);
        rig.wiki("fresh note", None);
        assert_eq!(
            rig.messages(),
            [
                "There is no note called nope in this workspace. Create it?",
                "There is no note called fresh note in this workspace. Create it?"
            ]
        );
        assert_eq!(read(&rig.work.join("fresh note.md")), "");
        assert_eq!(rig.path(), rig.work.join("fresh note.md"));
        assert!(rig.has_row("fresh note"));

        // Folders on the way are created.
        rig.answer(MessageBoxResult::Yes);
        rig.wiki("deeper/new", None);
        assert!(rig.work.join("deeper").join("new.md").exists());
        assert_eq!(rig.tab_count(), 3);
    });
}

#[test]
fn a_new_note_shows_in_the_sidebar_while_another_window_shows_no_folder() {
    let files = vec![text_file("note.md", "# Note\n")];
    with_rig(files, "note.md", nothing, |rig| {
        rig.show();
        // A second window without a tab: it shows no workspace and has no sidebar.
        let bare = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);

        rig.answer(MessageBoxResult::Yes);
        rig.wiki("fresh note", None);
        assert_eq!(read(&rig.work.join("fresh note.md")), "");
        assert!(rig.has_row("fresh note"));
        assert!(bare.tree.borrow().is_none());
        assert!(bare.view.borrow_mut().sidebar_mut().is_none());
        assert!(bare.view.borrow().tabs().is_empty());
    });
}

#[test]
fn a_wikilink_that_would_leave_the_workspace_is_never_offered() {
    let files = vec![text_file("note.md", "# Note\n")];
    with_rig(files, "note.md", nothing, |rig| {
        let outside = rig.work.parent().unwrap().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let link = rig.work.join("link");
        junction(&link, &outside);
        rig.show();

        for name in [
            "../outside/evil",
            "..\\..\\evil",
            "C:\\evil",
            "CON",
            "link/evil",
        ] {
            rig.answer(MessageBoxResult::Ok);
            rig.wiki(name, None);
        }
        let messages = rig.messages();
        assert_eq!(messages.len(), 5);
        assert_eq!(
            messages[0],
            "A note called ../outside/evil cannot be created inside this workspace."
        );
        assert_eq!(
            messages[4],
            "A note called link/evil cannot be created inside this workspace."
        );
        assert!(!outside.join("evil.md").exists());
        assert!(!rig.work.parent().unwrap().join("evil.md").exists());
        assert_eq!(rig.tab_count(), 1);

        // A workspace that vanished cannot be checked either.
        std::fs::remove_dir(&link).unwrap();
        rig.answer(MessageBoxResult::Ok);
        let gone = rig.work.clone();
        for entry in std::fs::read_dir(&gone).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        std::fs::remove_dir(&gone).unwrap();
        rig.wiki("anything", None);
        assert_eq!(rig.messages().len(), 6);
    });
}

#[test]
fn a_note_that_cannot_be_created_is_reported_and_one_that_exists_is_opened() {
    let files = vec![
        text_file("note.md", "# Note\n"),
        text_file(".ghost.md", "ghost\n"),
    ];
    with_rig(files, "note.md", nothing, |rig| {
        rig.show();
        // A name no file system accepts.
        rig.answer(MessageBoxResult::Yes);
        rig.answer(MessageBoxResult::Ok);
        rig.wiki(&"a".repeat(300), None);
        let messages = rig.messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1], "The file could not be read or written.");
        assert_eq!(rig.tab_count(), 1);

        // Hidden files are not listed, so the note looks missing; it exists, and is opened
        // as it is.
        rig.answer(MessageBoxResult::Yes);
        rig.wiki(".ghost", None);
        assert_eq!(rig.text(), "ghost\n");
        assert_eq!(rig.path(), rig.work.join(".ghost.md"));
    });
}

#[test]
fn wikilinks_do_nothing_where_there_is_no_workspace_to_look_in() {
    let files = vec![text_file("note.md", "# Note\n")];
    with_rig(files, "note.md", nothing, |rig| {
        // A window that has no tab yet.
        let fresh = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        rig.controller.activate_wikilink(&fresh, "x", Some("h"));
        rig.controller.activate_wikilink(&fresh, "", Some("h"));
        // An untitled document has no folder.
        let window = rig.window();
        rig.controller.new_untitled(&window);
        rig.wiki("x", None);
        assert!(rig.messages().is_empty());
        assert_eq!(rig.tab_count(), 2);
    });
}

// ----- the watcher and the index ---------------------------------------------------------------

#[test]
fn changes_on_disk_reach_the_sidebar_and_the_index() {
    let files = vec![text_file("a.md", "# A\n"), text_file("secret.md", "s\n")];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        rig.index_ready(2);

        std::fs::write(rig.work.join("b.md"), "# B\n").unwrap();
        rig.wait_for("b.md", || {
            rig.has_row("b") && rig.indexed().iter().any(|rel| rel == "b.md")
        });

        std::fs::rename(rig.work.join("b.md"), rig.work.join("c.md")).unwrap();
        rig.wait_for("the rename", || {
            let indexed = rig.indexed();
            rig.has_row("c")
                && !rig.has_row("b")
                && indexed.iter().any(|rel| rel == "c.md")
                && !indexed.iter().any(|rel| rel == "b.md")
        });

        std::fs::remove_file(rig.work.join("c.md")).unwrap();
        rig.wait_for("the removal", || {
            !rig.has_row("c") && !rig.indexed().iter().any(|rel| rel == "c.md")
        });

        // A new `.gitignore` changes which files count: the whole folder is walked again.
        std::fs::write(rig.work.join(".gitignore"), "secret.md\n").unwrap();
        rig.wait_for("the ignore rules", || {
            !rig.indexed().iter().any(|rel| rel == "secret.md")
        });
        assert_eq!(rig.indexed(), ["a.md"]);
    });
}

#[test]
fn changes_that_arrive_while_the_index_is_being_updated_are_kept_for_the_next_run() {
    let files = vec![text_file("a.md", "# A\n")];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        rig.index_ready(1);
        let deadline = Instant::now() + Duration::from_secs(30);
        let wait_rows = |stem: &str| {
            // Only the watcher turn runs: no worker result is delivered in between, so the
            // update that is started for the first file is still running for the second.
            while !rig.has_row(stem) {
                rig.controller.pump_files();
                assert!(Instant::now() < deadline, "timed out waiting for {stem}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        std::fs::write(rig.work.join("d.md"), "d\n").unwrap();
        wait_rows("d");
        std::fs::write(rig.work.join("e.md"), "e\n").unwrap();
        wait_rows("e");
        rig.wait_for("both files in the index", || {
            let indexed = rig.indexed();
            indexed.iter().any(|rel| rel == "d.md") && indexed.iter().any(|rel| rel == "e.md")
        });
    });
}

#[test]
fn two_windows_on_one_workspace_both_follow_it_and_a_missing_workspace_is_logged() {
    let files = vec![text_file("a.md", "# A\n")];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        let second = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        rig.controller.new_untitled(&second);
        rig.controller.show_workspace(&second, &rig.work);
        rig.index_ready(1);
        std::fs::write(rig.work.join("b.md"), "b\n").unwrap();
        rig.wait_for("the file in both sidebars", || {
            let other = second.view.borrow();
            rig.has_row("b")
                && other
                    .sidebar()
                    .unwrap()
                    .row_names()
                    .iter()
                    .any(|row| row.starts_with('b'))
        });

        // A folder that cannot be watched is logged, not fatal.
        let gone = rig.work.parent().unwrap().join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        let third = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        rig.controller.new_untitled(&third);
        rig.controller.show_workspace(&third, &gone);
        std::fs::remove_dir(&gone).unwrap();
        rig.pump();
        rig.settle();
        let log = rig.controller.core().borrow().logger.paths();
        let written: String = log
            .iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .collect();
        assert!(written.contains("cannot watch"), "{written}");
    });
}

#[test]
fn a_state_database_that_cannot_be_opened_leaves_the_index_unbuilt() {
    let files = vec![text_file("a.md", "# A\n")];
    with_rig(files, "a.md", nothing, |rig| {
        // The folder of the database is a file now: the workers cannot open it.
        let blocker = rig.work.parent().unwrap().join("blocked");
        std::fs::create_dir_all(&blocker).unwrap();
        std::fs::write(blocker.join("SS Writer"), "not a folder").unwrap();
        let roaming = rig
            .controller
            .core()
            .borrow()
            .paths
            .roaming_dir()
            .to_path_buf();
        let environment = ssw_io::paths::Environment {
            local_app_data: Some(blocker),
            app_data: roaming.parent().map(Path::to_path_buf),
            exe_dir: rig.work.parent().unwrap().join("exe"),
        };
        let broken = ssw_io::paths::AppPaths::resolve(&environment).unwrap();
        let good = std::mem::replace(&mut rig.controller.core().borrow_mut().paths, broken);

        rig.show();
        // The first turn starts the watcher (and the first full index, which fails too).
        rig.wait_for("the sidebar", || rig.has_row("a"));
        rig.settle();
        // A change the watcher sees queues a full walk: its job cannot open the database.
        std::fs::write(rig.work.join(".gitignore"), "x\n").unwrap();
        rig.wait_for("the index job", || {
            rig.controller.core().borrow().workers.pending() > 0
        });
        rig.settle();
        assert!(rig.indexed().is_empty());

        // Once the database opens again the next full run builds the index.
        rig.controller.core().borrow_mut().paths = good;
        std::fs::write(rig.work.join(".gitignore"), "y\n").unwrap();
        rig.wait_for("the index", || !rig.indexed().is_empty());
    });
}

#[test]
fn the_tree_applies_the_changes_it_is_told_about_and_re_reads_on_request() {
    let tmp = tempfile::tempdir().unwrap();
    let root = canonical_path(tmp.path());
    std::fs::write(root.join("a.md"), "a").unwrap();
    let mut tree = WorkspaceTree::open(Arc::new(RealFs), &root).unwrap();
    let paths = tree.paths();
    let options = SidebarOptions::default();
    let names = |tree: &mut WorkspaceTree| -> Vec<String> {
        tree.children(0, &options)
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    };
    assert_eq!(names(&mut tree), ["a.md"]);

    std::fs::write(root.join("b.md"), "b").unwrap();
    paths.queue_event(FsEvent {
        path: root.join("b.md"),
        kind: FsEventKind::Created,
    });
    assert_eq!(names(&mut tree), ["a.md", "b.md"]);

    std::fs::remove_file(root.join("a.md")).unwrap();
    paths.queue_event(FsEvent {
        path: root.join("a.md"),
        kind: FsEventKind::Removed,
    });
    assert_eq!(names(&mut tree), ["b.md"]);

    std::fs::rename(root.join("b.md"), root.join("c.md")).unwrap();
    paths.queue_event(FsEvent {
        path: root.join("c.md"),
        kind: FsEventKind::Renamed {
            from: root.join("b.md"),
        },
    });
    assert_eq!(names(&mut tree), ["c.md"]);

    // A file nobody told the tree about shows after a rescan, not before.
    std::fs::write(root.join("d.md"), "d").unwrap();
    assert_eq!(names(&mut tree), ["c.md"]);
    paths.request_rescan();
    assert_eq!(names(&mut tree), ["c.md", "d.md"]);
}

#[test]
fn a_folder_the_tree_never_handed_out_or_can_no_longer_read_lists_as_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let root = canonical_path(tmp.path());
    std::fs::create_dir(root.join("gone")).unwrap();
    std::fs::write(root.join("a.md"), "a").unwrap();
    let mut tree = WorkspaceTree::open(Arc::new(RealFs), &root).unwrap();
    let paths = tree.paths();
    let options = SidebarOptions::default();
    let top = tree.children(0, &options);
    assert_eq!(
        top.iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["gone", "a.md"]
    );

    // A key nobody was given has no folder behind it.
    assert_eq!(paths.path(9999), None);
    assert!(tree.children(9999, &options).is_empty());

    // The folder vanished before the sidebar expanded it: the listing fails and shows nothing.
    std::fs::remove_dir(root.join("gone")).unwrap();
    assert!(tree.children(top[0].key, &options).is_empty());
}

// ----- `[[` completion -------------------------------------------------------------------------

#[test]
fn the_completion_list_offers_the_workspace_files_and_the_headings_of_the_document() {
    let note = "# One\n## Two\n\nSee [[ta";
    let files = vec![
        text_file("note.md", note),
        text_file("target.md", "# T\n"),
        text_file("zeta.md", "z\n"),
        text_file("sub/other.txt", "x"),
    ];
    with_rig(files, "note.md", nothing, |rig| {
        let window = rig.window();
        window
            .view
            .borrow_mut()
            .active_tab_mut()
            .unwrap()
            .editor
            .go_to(Selection::caret(note.len()));
        rig.show();
        rig.index_ready(4);
        rig.pump();

        // Markdown files only, the open one first.
        assert_eq!(rig.completion(""), ["note.md", "target.md", "zeta.md"]);
        // After `[[#` the headings of the active document.
        assert_eq!(rig.completion("#"), ["One", "Two"]);
        assert_eq!(rig.completion("tar"), ["target.md"]);
        {
            let mut view = window.view.borrow_mut();
            let list = view.overlays_mut().wikilink_mut();
            assert!(list.accept());
            assert_eq!(
                list.take_events(),
                [WikilinkEvent::Accept(WikilinkAccept {
                    insert: "target".to_owned(),
                    replace: 0..3
                })]
            );
        }

        // The most recently opened file leads.
        rig.controller
            .open_file(&window, &rig.work.join("target.md"))
            .unwrap();
        rig.pump();
        assert_eq!(rig.completion(""), ["target.md", "note.md", "zeta.md"]);
    });
}

#[test]
fn headings_fed_after_the_keystroke_fill_the_open_list_and_repaint() {
    let note = "# One\n## Two\n\nSee [[#";
    let files = vec![text_file("note.md", note)];
    with_rig(files, "note.md", nothing, |rig| {
        let window = rig.window();
        window
            .view
            .borrow_mut()
            .active_tab_mut()
            .unwrap()
            .editor
            .go_to(Selection::caret(note.len()));
        // `[[#` opened the list before the housekeeping turn handed over the headings.
        assert!(rig.completion("#").is_empty());
        window.view.borrow_mut().requests_mut().take_invalidation();
        rig.pump();
        let mut view = window.view.borrow_mut();
        let rows: Vec<String> = view
            .overlays()
            .wikilink()
            .list()
            .rows()
            .iter()
            .map(|row| row.label.clone())
            .collect();
        assert_eq!(rows, ["One", "Two"]);
        assert!(
            !view.requests_mut().take_invalidation().is_clean(),
            "the window repaints to show the list"
        );
    });
}

#[test]
fn a_tab_outside_any_workspace_completes_nothing() {
    let files = vec![
        text_file("note.md", "See [[ta"),
        text_file("target.md", "# T\n"),
    ];
    with_rig(files, "note.md", nothing, |rig| {
        let window = rig.window();
        // A tab of a restored session belongs to a workspace although its window shows none;
        // the first tab belongs to none.
        let entry = rig
            .controller
            .docs()
            .open(&rig.work.join("target.md"))
            .unwrap();
        window
            .view
            .borrow_mut()
            .add_tab(entry, Some(rig.work.clone()));
        rig.pump();
        assert!(rig.completion("").is_empty(), "the folder is not indexed");
        let first = window.view.borrow().tabs()[0].key;
        window.view.borrow_mut().activate_tab(first);
        rig.pump();
        assert!(rig.completion("").is_empty());
    });
}

// ----- OneDrive placeholders ---------------------------------------------------------------------

#[test]
fn a_placeholder_opens_in_the_background_with_taskbar_progress() {
    let files = vec![
        text_file("a.md", "# A\n"),
        text_file("cloud.md", "# Cloud\nfrom the cloud\n"),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        let cloud = rig.work.join("cloud.md");
        make_placeholder(&cloud);
        let window = rig.window();
        rig.progress();

        rig.controller.open_file(&window, &cloud).unwrap();
        assert_eq!(rig.tab_count(), 1, "the file is still being read");
        // Asking again while it downloads starts nothing more.
        rig.controller.open_file(&window, &cloud).unwrap();
        assert_eq!(rig.progress(), [Progress::Indeterminate]);

        rig.wait_for("the tab", || rig.tab_count() == 2);
        assert_eq!(rig.text(), "# Cloud\nfrom the cloud\n");
        assert_eq!(rig.progress(), [Progress::None]);
    });
}

#[test]
fn a_placeholder_that_finishes_downloading_replaces_the_pristine_untitled_tab() {
    let files = vec![
        text_file("a.md", "# A\n"),
        text_file("cloud.md", "# Cloud\n"),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        let cloud = rig.work.join("cloud.md");
        make_placeholder(&cloud);
        let window = rig.window();
        rig.controller.new_untitled(&window);
        assert_eq!(rig.tab_count(), 2);
        rig.controller.open_file(&window, &cloud).unwrap();
        assert_eq!(
            rig.tab_count(),
            2,
            "the Untitled tab stays while it downloads"
        );
        rig.wait_for("the file", || rig.active().path().is_some());
        assert_eq!(rig.tab_count(), 2, "the file took the Untitled tab's place");
        assert_eq!(rig.path(), canonical_path(&cloud));
    });
}

#[test]
fn a_placeholder_that_cannot_be_read_is_reported_and_a_closed_window_reads_nothing() {
    let files = vec![
        text_file("a.md", "# A\n"),
        text_file("locked.md", "locked\n"),
        text_file("cloud.md", "cloud\n"),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        let locked = rig.work.join("locked.md");
        make_placeholder(&locked);
        // Another program has the file open without sharing it.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .unwrap();
        let window = rig.window();
        rig.progress();
        rig.answer(MessageBoxResult::Ok);
        rig.controller.open_file(&window, &locked).unwrap();
        rig.wait_for("the error", || !rig.messages().is_empty());
        assert_eq!(rig.messages(), ["The file could not be read or written."]);
        assert_eq!(rig.tab_count(), 1);
        assert_eq!(
            rig.progress(),
            [Progress::Indeterminate, Progress::None],
            "the taskbar button is a plain one again"
        );
        drop(lock);

        // The window the file was meant for is gone: the caller reads it directly.
        let cloud = rig.work.join("cloud.md");
        make_placeholder(&cloud);
        let fresh = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        rig.controller.close_window(fresh.id);
        assert!(!rig.controller.begin_hydration(&fresh, &cloud, false));
        assert!(!rig.controller.begin_hydration(&fresh, &cloud, false));
        // Neither a regular file nor a folder is a placeholder.
        assert!(
            !rig.controller
                .begin_hydration(&window, &rig.work.join("a.md"), false)
        );
        assert!(!rig.controller.begin_hydration(&window, &rig.work, false));
    });
}

#[test]
fn a_wikilink_resolves_through_the_index_once_the_workspace_is_indexed() {
    let files = vec![
        text_file("note.md", "# Note\n"),
        text_file("sub/target.md", "# T\n\n## Part\nbody\n"),
    ];
    with_rig(files, "note.md", nothing, |rig| {
        rig.show();
        rig.index_ready(2);
        rig.wiki("target", Some("Part"));
        assert_eq!(rig.path(), rig.work.join("sub").join("target.md"));
        assert_eq!(rig.caret(), "# T\n\n".len());
    });
}

#[test]
fn a_window_without_a_tab_is_left_alone_while_the_others_are_fed() {
    let files = vec![text_file("a.md", "# A\n"), text_file("b.md", "# B\n")];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        rig.index_ready(2);
        let empty = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        rig.pump();
        assert!(empty.view.borrow().active_tab().is_none());
        assert_eq!(rig.completion(""), ["a.md", "b.md"]);
    });
}

#[test]
fn changes_on_disk_still_reach_the_index_of_a_window_whose_sidebar_was_removed() {
    let files = vec![text_file("a.md", "# A\n")];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        rig.index_ready(1);
        rig.window().view.borrow_mut().set_workspace(None);
        std::fs::write(rig.work.join("b.md"), "# B\n").unwrap();
        rig.wait_for("b.md in the index", || {
            rig.indexed().iter().any(|rel| rel == "b.md")
        });
        assert!(rig.window().view.borrow().sidebar().is_none());
    });
}

#[test]
fn the_completion_outline_is_fed_when_the_caret_enters_a_wikilink_after_the_files() {
    let files = vec![
        text_file("note.md", "# One\n## Two\n\nplain"),
        text_file("t.md", "x\n"),
    ];
    with_rig(files, "note.md", nothing, |rig| {
        rig.show();
        rig.index_ready(2);
        rig.pump();
        // Files are fed; now the caret enters a `[[`: only the headings are fed.
        rig.set_text("# One\n## Two\n\nSee [[");
        let end = rig.text().len();
        rig.window()
            .view
            .borrow_mut()
            .active_tab_mut()
            .unwrap()
            .editor
            .go_to(Selection::caret(end));
        rig.pump();
        assert_eq!(rig.completion("#"), ["One", "Two"]);
        assert_eq!(rig.completion(""), ["note.md", "t.md"]);
    });
}

/// The real disk, except that reads panic while the flag is set.
#[derive(Debug)]
struct PanickingRead(std::sync::atomic::AtomicBool);

impl PanickingRead {
    fn arm(&self, armed: bool) {
        self.0.store(armed, std::sync::atomic::Ordering::SeqCst);
    }
}

impl FileSystem for PanickingRead {
    fn read(&self, path: &Path) -> Result<Vec<u8>, ssw_io::fs::FsError> {
        assert!(
            !self.0.load(std::sync::atomic::Ordering::SeqCst),
            "the disk went away"
        );
        RealFs.read(path)
    }
    fn read_prefix(&self, path: &Path, max: usize) -> Result<Vec<u8>, ssw_io::fs::FsError> {
        assert!(
            !self.0.load(std::sync::atomic::Ordering::SeqCst),
            "the disk went away"
        );
        RealFs.read_prefix(path, max)
    }
    fn metadata(&self, path: &Path) -> Result<ssw_io::fs::FileMeta, ssw_io::fs::FsError> {
        RealFs.metadata(path)
    }
    fn write_new(&self, path: &Path, data: &[u8]) -> Result<(), ssw_io::fs::FsError> {
        RealFs.write_new(path, data)
    }
    fn replace_file(&self, target: &Path, with: &Path) -> Result<(), ssw_io::fs::FsError> {
        RealFs.replace_file(target, with)
    }
    fn move_replace(&self, from: &Path, to: &Path) -> Result<(), ssw_io::fs::FsError> {
        RealFs.move_replace(from, to)
    }
    fn rename(&self, from: &Path, to: &Path) -> Result<(), ssw_io::fs::FsError> {
        RealFs.rename(from, to)
    }
    fn copy_file(&self, from: &Path, to: &Path) -> Result<(), ssw_io::fs::FsError> {
        RealFs.copy_file(from, to)
    }
    fn delete_file(&self, path: &Path) -> Result<(), ssw_io::fs::FsError> {
        RealFs.delete_file(path)
    }
    fn remove_dir(&self, path: &Path) -> Result<(), ssw_io::fs::FsError> {
        RealFs.remove_dir(path)
    }
    fn create_dir(&self, path: &Path) -> Result<(), ssw_io::fs::FsError> {
        RealFs.create_dir(path)
    }
    fn create_dir_all(&self, path: &Path) -> Result<(), ssw_io::fs::FsError> {
        RealFs.create_dir_all(path)
    }
    fn list_dir(&self, path: &Path) -> Result<Vec<ssw_io::fs::DirEntry>, ssw_io::fs::FsError> {
        RealFs.list_dir(path)
    }
    fn set_attributes(&self, path: &Path, attrs: Attributes) -> Result<(), ssw_io::fs::FsError> {
        RealFs.set_attributes(path, attrs)
    }
    fn is_writable(&self, path: &Path) -> Result<bool, ssw_io::fs::FsError> {
        RealFs.is_writable(path)
    }
    fn recycle(&self, paths: &[PathBuf]) -> Result<(), ssw_io::fs::FsError> {
        RealFs.recycle(paths)
    }
}

/// Runs `body` like [`with_rig`], over documents that read through `fs`.
fn with_rig_on(
    fs: &Arc<PanickingRead>,
    files: Vec<(String, Vec<u8>)>,
    open: &str,
    body: impl FnOnce(&Rig),
) {
    let installed = Arc::clone(fs);
    with_rig(
        files,
        open,
        move |core, _| {
            let env = ssw_app::documents::DocEnv {
                fs: installed,
                ..core.docs.env().clone()
            };
            core.docs = Rc::new(ssw_app::documents::Documents::new(env));
        },
        body,
    );
}

#[test]
fn a_file_read_that_panics_is_reported_like_any_other_failure_to_read_it() {
    let files = vec![
        text_file("a.md", "# A\n"),
        text_file("cloud.md", "# Cloud\n"),
    ];
    let fs = Arc::new(PanickingRead(std::sync::atomic::AtomicBool::new(false)));
    with_rig_on(&fs, files, "a.md", |rig| {
        let cloud = rig.work.join("cloud.md");
        make_placeholder(&cloud);
        let window = rig.window();
        rig.answer(MessageBoxResult::Ok);
        fs.arm(true);
        rig.controller.open_file(&window, &cloud).unwrap();
        rig.wait_for("the error", || !rig.messages().is_empty());
        fs.arm(false);
        assert_eq!(rig.messages(), ["The file could not be read or written."]);
        assert_eq!(rig.tab_count(), 1);
    });
}

fn rtf_fixture() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/rtf/testRTFBoldItalic.rtf"),
    )
    .unwrap()
}

fn open_in_sidebar(rig: &Rig, path: &Path) {
    let key = rig.key(path);
    rig.controller.handle_action(
        rig.id(),
        ShellAction::Sidebar(SidebarEvent::Open {
            key,
            new_tab: false,
        }),
    );
}

#[test]
fn a_word_processor_file_is_imported_once_and_opening_it_again_shows_its_tab() {
    let files = vec![
        text_file("a.md", "# A\n"),
        byte_file("doc.rtf", &rtf_fixture()),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        let doc = rig.work.join("doc.rtf");
        open_in_sidebar(rig, &doc);
        rig.wait_for("the import", || rig.tab_count() == 2);
        assert_eq!(rig.path(), canonical_path(&doc));

        // Back to the first tab; opening the file again activates its tab, nothing is imported.
        let first = rig.window().view.borrow().tabs()[0].key;
        rig.window().view.borrow_mut().activate_tab(first);
        assert_eq!(rig.path(), canonical_path(&rig.work.join("a.md")));
        // The workspace index of the sidebar may still be running: let it end first, so that
        // a job submitted by the second open is the only one that can be pending.
        rig.settle();
        open_in_sidebar(rig, &doc);
        assert_eq!(rig.tab_count(), 2);
        assert_eq!(rig.path(), canonical_path(&doc));
        assert_eq!(
            rig.controller.core().borrow().workers.pending(),
            0,
            "no import was started"
        );
    });
}

#[test]
fn converted_and_untitled_tabs_never_take_part_in_a_link_update() {
    let files = vec![
        text_file("a.md", "[t](target.md)\n"),
        text_file("target.md", "# Target\n"),
        byte_file("doc.rtf", &rtf_fixture()),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        rig.index_ready(3);
        open_in_sidebar(rig, &rig.work.join("doc.rtf"));
        rig.wait_for("the import", || rig.tab_count() == 2);
        rig.controller.new_untitled(&rig.window());
        assert_eq!(rig.tab_count(), 3);
        rig.answer(MessageBoxResult::Yes);
        rig.rename(&rig.work.join("target.md"), "moved.md");
        assert_eq!(
            rig.messages(),
            ["The links in 1 other file point to moved.md. Update them to its new location?"]
        );
        let docs = rig.controller.docs().all();
        let linking = docs
            .iter()
            .find(|entry| entry.path().is_some_and(|path| path.ends_with("a.md")))
            .expect("a.md is open");
        assert_eq!(linking.document().borrow().text(), "[t](moved.md)\n");
    });
}

#[test]
fn files_that_vanish_or_stop_being_files_before_the_update_are_skipped() {
    let files = vec![
        text_file("target.md", "# Target\n"),
        text_file("gone.md", "[g](target.md)\n"),
        text_file("cloud.md", "[c](target.md)\n"),
        text_file("folder.md", "[f](target.md)\n"),
        text_file("locked.md", "[l](target.md)\n"),
        text_file("ok.md", "[o](target.md)\n"),
    ];
    with_rig(files, "target.md", nothing, |rig| {
        rig.show();
        rig.index_ready(6);
        // Nothing is pumped from here on: the index still lists the old state.
        std::fs::remove_file(rig.work.join("gone.md")).unwrap();
        make_placeholder(&rig.work.join("cloud.md"));
        std::fs::remove_file(rig.work.join("folder.md")).unwrap();
        std::fs::create_dir(rig.work.join("folder.md")).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(rig.work.join("locked.md"))
            .unwrap();
        rig.answer(MessageBoxResult::Yes);
        rig.rename(&rig.work.join("target.md"), "moved.md");
        drop(lock);
        assert_eq!(
            rig.messages(),
            ["The links in 1 other file point to moved.md. Update them to its new location?"]
        );
        assert_eq!(read(&rig.work.join("ok.md")), "[o](moved.md)\n");
        assert_eq!(read(&rig.work.join("locked.md")), "[l](target.md)\n");
    });
}

#[test]
fn files_that_cannot_be_read_or_became_bundles_while_the_question_was_open_stay_as_they_are() {
    let files = vec![
        text_file("target.md", "# Target\n"),
        text_file("locked.md", "[l](target.md)\n"),
        text_file("bundle.md", "[b](target.md)\n"),
        text_file("ok.md", "[o](target.md)\n"),
    ];
    with_rig(files, "target.md", nothing, |rig| {
        rig.show();
        rig.index_ready(4);
        let lock: Rc<RefCell<Option<std::fs::File>>> = Rc::default();
        let held = Rc::clone(&lock);
        let locked = rig.work.join("locked.md");
        let bundle = rig.work.join("bundle.md");
        rig.on_next_message(move || {
            // Another program locks one file and swaps another for a bundle folder.
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&locked)
                .unwrap();
            *held.borrow_mut() = Some(file);
            std::fs::remove_file(&bundle).unwrap();
            std::fs::create_dir(&bundle).unwrap();
            std::fs::write(bundle.join("text.markdown"), "[b](target.md)\n").unwrap();
        });
        rig.answer(MessageBoxResult::Yes);
        rig.answer(MessageBoxResult::Ok);
        rig.rename(&rig.work.join("target.md"), "moved.md");
        lock.borrow_mut().take();

        assert_eq!(
            rig.messages(),
            [
                "The links in 3 other files point to moved.md. Update them to its new location?",
                "2 files could not be updated. Their links still point to the old location."
            ]
        );
        assert_eq!(read(&rig.work.join("locked.md")), "[l](target.md)\n");
        assert_eq!(
            read(&rig.work.join("bundle.md").join("text.markdown")),
            "[b](target.md)\n"
        );
        assert_eq!(read(&rig.work.join("ok.md")), "[o](moved.md)\n");
    });
}

#[test]
fn a_workspace_whose_watch_is_lost_has_its_index_rebuilt_by_a_full_run() {
    let files = vec![text_file("a.md", "# A\n")];
    with_rig(files, "a.md", nothing, |rig| {
        rig.settle();
        let delivered = || rig.controller.core().borrow().workers.stats().delivered;
        let before = delivered();
        let lost = rig.work.parent().unwrap().join("lost");
        std::fs::create_dir(&lost).unwrap();
        std::fs::write(lost.join("n.md"), "# N\n").unwrap();
        rig.controller.show_workspace(&rig.window(), &lost);
        // The folder is a file by the time the first housekeeping turn starts its watcher: a
        // watch that cannot be read ends with a "changes were lost" event for the root.
        std::fs::remove_dir_all(&lost).unwrap();
        std::fs::write(&lost, "no folder any more").unwrap();
        // One full run starts with the new workspace; the lost watch asks for a second one.
        rig.wait_for("the second full index run", || delivered() >= before + 2);
    });
}

#[test]
fn a_document_the_recent_list_refuses_is_reported_after_its_background_read() {
    let files = vec![
        text_file("a.md", "# A\n"),
        text_file("cloud.md", "# Cloud\n"),
        byte_file("doc.rtf", &rtf_fixture()),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        let database = rig.controller.core().borrow().paths.state_db();
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch("DROP TABLE recent_items;")
            .unwrap();
        make_placeholder(&rig.work.join("cloud.md"));
        rig.answer(MessageBoxResult::Ok);
        rig.answer(MessageBoxResult::Ok);
        // A placeholder is read on a worker, a word-processor file is imported on one.
        rig.controller
            .open_file(&rig.window(), &rig.work.join("cloud.md"))
            .unwrap();
        open_in_sidebar(rig, &rig.work.join("doc.rtf"));
        rig.wait_for("both errors", || rig.messages().len() == 2);
        assert_eq!(
            rig.messages(),
            [
                "The file could not be read or written.",
                "The file could not be read or written."
            ]
        );
        assert_eq!(rig.tab_count(), 1);
    });
}

#[test]
fn an_import_whose_worker_panics_is_reported_as_a_failed_conversion() {
    let files = vec![
        text_file("a.md", "# A\n"),
        byte_file("doc.rtf", &rtf_fixture()),
    ];
    let fs = Arc::new(PanickingRead(std::sync::atomic::AtomicBool::new(false)));
    with_rig_on(&fs, files, "a.md", |rig| {
        rig.show();
        rig.answer(MessageBoxResult::Ok);
        fs.arm(true);
        open_in_sidebar(rig, &rig.work.join("doc.rtf"));
        rig.wait_for("the error", || !rig.messages().is_empty());
        fs.arm(false);
        assert_eq!(rig.messages(), ["The document could not be converted."]);
        assert_eq!(rig.tab_count(), 1);
    });
}

#[test]
fn an_import_that_finishes_after_its_window_was_merged_away_opens_nothing() {
    let files = vec![
        text_file("a.md", "# A\n"),
        byte_file("doc.rtf", &rtf_fixture()),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        let doc = rig.work.join("doc.rtf");
        let first = Rc::downgrade(&rig.window());
        open_in_sidebar(rig, &doc);
        let second = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        // Window > Merge all windows in the second window: the first one ends while its import
        // is still under way.
        let _ = rig
            .controller
            .run_command(second.id, ssw_core::command::CommandId::WindowMergeAll);
        assert!(first.upgrade().is_none(), "the merged window is gone");
        rig.settle();

        assert_eq!(second.view.borrow().tabs().len(), 1, "only a.md");
        assert!(
            rig.controller
                .docs()
                .all()
                .iter()
                .all(|entry| entry.path() != Some(canonical_path(&doc))),
            "the import was dropped before it was registered"
        );
        assert_eq!(rig.messages(), Vec::<String>::new());
    });
}

// ----- files of no known type -------------------------------------------------------------------

/// What the user is told about a binary file.
const NOT_TEXT: &str = "This file is not a text file.";

/// Text files without a known extension, and what they show.
const UNKNOWN_TEXT: [(&str, &str); 6] = [
    ("notes.kt", "fun main() {}\n"),
    ("Dockerfile", "FROM scratch\n"),
    ("Dockerfile.prod", "FROM scratch\nENV MODE=prod\n"),
    (".gitignore", "target/\n*.log\n"),
    (".env.example", "TOKEN=\n"),
    ("LICENSE", "MIT\n"),
];

/// The start of a PNG: its signature and a 1 x 1 header, which carry NUL bytes.
const PNG_START: &[u8] =
    b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00";

/// The UTF-16 little-endian bytes of `text`, behind a BOM when `bom`.
fn utf16le(text: &str, bom: bool) -> Vec<u8> {
    let bom: &[u8] = if bom { &[0xFF, 0xFE] } else { &[] };
    let units = text.encode_utf16().flat_map(u16::to_le_bytes);
    bom.iter().copied().chain(units).collect()
}

/// The messages of the message boxes shown so far (the open dialog is no message).
fn message_boxes(rig: &Rig) -> Vec<String> {
    rig.services
        .dialog_log()
        .into_iter()
        .filter(|record| record.kind == DialogKind::MessageBox)
        .map(|record| record.message)
        .collect()
}

#[test]
fn a_text_file_of_no_known_type_opens_in_plain_mode_from_the_sidebar() {
    let mut files = vec![text_file("a.md", "# A\n")];
    for (name, text) in UNKNOWN_TEXT {
        files.push(text_file(name, text));
    }
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        for (index, (name, text)) in UNKNOWN_TEXT.iter().enumerate() {
            let path = rig.work.join(name);
            open_in_sidebar(rig, &path);
            assert_eq!(rig.tab_count(), index + 2, "{name}");
            assert_eq!(rig.path(), canonical_path(&path), "{name}");
            assert_eq!(rig.text(), *text, "{name}");
            assert_eq!(rig.active().mode(), DocMode::Plain, "{name}");
        }
        assert_eq!(message_boxes(rig), Vec::<String>::new());
        assert!(rig.services.shell_log().is_empty(), "no default app");
    });
}

#[test]
fn a_text_file_of_no_known_type_given_on_the_command_line_opens_in_plain_mode() {
    let files = vec![text_file("notes.kt", "fun main() {}\n")];
    with_rig(files, "notes.kt", nothing, |rig| {
        assert_eq!(rig.tab_count(), 1);
        assert_eq!(rig.path(), canonical_path(&rig.work.join("notes.kt")));
        assert_eq!(rig.text(), "fun main() {}\n");
        assert_eq!(rig.active().mode(), DocMode::Plain);
    });
}

#[test]
fn a_utf16_file_with_a_bom_is_text_although_it_holds_nul_bytes() {
    let text = "héllo\nwörld\n";
    let with_bom = utf16le(text, true);
    assert!(with_bom.contains(&0));
    let files = vec![
        text_file("a.md", "# A\n"),
        byte_file("wide.dat", &with_bom),
        // A known text type is not looked at: its encoding detection finds the UTF-16.
        byte_file("wide.txt", &utf16le(text, false)),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        for (index, name) in ["wide.dat", "wide.txt"].into_iter().enumerate() {
            open_in_sidebar(rig, &rig.work.join(name));
            assert_eq!(rig.tab_count(), index + 2, "{name}");
            assert_eq!(rig.text(), text, "{name}");
            assert_eq!(rig.active().mode(), DocMode::Plain, "{name}");
        }
        assert_eq!(message_boxes(rig), Vec::<String>::new());
    });
}

#[test]
fn a_binary_file_clicked_in_the_sidebar_starts_its_default_app_with_no_tab_and_no_message() {
    let files = vec![
        text_file("a.md", "# A\n"),
        byte_file(
            "report.pdf",
            b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n1 0 obj\x00\x00",
        ),
        byte_file("tool.exe", b"MZ\x90\x00\x03\x00\x00\x00\x04\x00\x00\x00"),
        byte_file("blob", b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00"),
        byte_file("pic.png", PNG_START),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        let names = ["report.pdf", "tool.exe", "blob", "pic.png"];
        for name in names {
            open_in_sidebar(rig, &rig.work.join(name));
        }
        assert_eq!(rig.tab_count(), 1, "no tab shows a binary file");
        assert_eq!(message_boxes(rig), Vec::<String>::new());
        let started: Vec<ShellRecord> = names
            .iter()
            .map(|name| ShellRecord::OpenPath(rig.work.join(name)))
            .collect();
        assert_eq!(rig.services.shell_log(), started);
        assert_eq!(
            rig.controller.docs().all().len(),
            1,
            "no document holds one"
        );
    });
}

#[test]
fn a_binary_placeholder_clicked_in_the_sidebar_starts_its_default_app_once_it_is_downloaded() {
    let files = vec![
        text_file("a.md", "# A\n"),
        byte_file(
            "report.pdf",
            b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n1 0 obj\x00\x00",
        ),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        rig.show();
        let report = rig.work.join("report.pdf");
        make_placeholder(&report);
        rig.progress();

        open_in_sidebar(rig, &report);
        assert!(
            rig.services.shell_log().is_empty(),
            "nothing starts while the file downloads"
        );
        rig.wait_for("the default app", || !rig.services.shell_log().is_empty());

        assert_eq!(
            rig.services.shell_log(),
            [ShellRecord::OpenPath(report.clone())]
        );
        assert_eq!(rig.tab_count(), 1, "no tab shows a binary file");
        assert_eq!(message_boxes(rig), Vec::<String>::new());
        assert_eq!(
            rig.progress(),
            [Progress::Indeterminate, Progress::None],
            "the taskbar button is a plain one again"
        );
    });
}

#[test]
fn a_binary_file_is_refused_with_a_message_by_every_way_of_asking_the_editor_to_open_it() {
    let files = vec![
        text_file("a.md", "# A\n"),
        text_file("notes.kt", "fun main() {}\n"),
        byte_file("tool.exe", b"MZ\x90\x00\x03\x00\x00\x00\x04\x00\x00\x00"),
        byte_file("pic.png", PNG_START),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        // File > Open: the binary file is refused, the text file after it opens.
        rig.services.answer(
            DialogKind::OpenFile,
            Answer::Paths(vec![rig.work.join("tool.exe"), rig.work.join("notes.kt")]),
        );
        rig.answer(MessageBoxResult::Ok);
        let _ = rig
            .controller
            .run_command(rig.id(), ssw_core::command::CommandId::FileOpen);
        assert_eq!(message_boxes(rig), [NOT_TEXT]);
        assert_eq!(rig.tab_count(), 2);
        assert_eq!(rig.path(), canonical_path(&rig.work.join("notes.kt")));
        assert_eq!(rig.active().mode(), DocMode::Plain);

        // A launch (Explorer "Open with", the command line, a forwarded second start) and a
        // dropped file end in the same call: a picture is no more text than a program.
        for name in ["tool.exe", "pic.png"] {
            rig.answer(MessageBoxResult::Ok);
            rig.controller.open_launch(&Launch {
                items: vec![LaunchItem::File(rig.work.join(name))],
                new_window: false,
            });
        }
        assert_eq!(message_boxes(rig), [NOT_TEXT; 3]);
        assert_eq!(rig.tab_count(), 2, "no tab shows a refused file");
        let mut open: Vec<PathBuf> = rig
            .controller
            .docs()
            .all()
            .iter()
            .filter_map(|entry| entry.path())
            .collect();
        open.sort();
        let mut expected = vec![
            canonical_path(&rig.work.join("a.md")),
            canonical_path(&rig.work.join("notes.kt")),
        ];
        expected.sort();
        assert_eq!(open, expected, "no document holds a refused file");
        assert!(
            rig.services.shell_log().is_empty(),
            "nothing was started either"
        );
    });
}

#[test]
fn a_link_never_opens_a_file_of_no_known_type_in_the_app() {
    let files = vec![
        text_file("a.md", "[k](notes.kt) [b](blob.dat)\n"),
        text_file("notes.kt", "fun main() {}\n"),
        byte_file("blob.dat", b"\x00\x01\x02\x03"),
    ];
    with_rig(files, "a.md", nothing, |rig| {
        let key = rig.window().view.borrow().active_tab().unwrap().key;
        for target in ["notes.kt", "blob.dat"] {
            rig.controller.handle_action(
                rig.id(),
                ShellAction::Editor(
                    key,
                    EditorEvent::LinkActivated(LinkTarget::Url(target.to_owned())),
                ),
            );
        }
        // Links keep their rules: the default app or nothing, neither a tab nor an error.
        assert_eq!(rig.tab_count(), 1);
        assert_eq!(message_boxes(rig), Vec::<String>::new());
        assert!(
            rig.services
                .shell_log()
                .iter()
                .all(|record| matches!(record, ShellRecord::OpenPath(_))),
            "{:?}",
            rig.services.shell_log()
        );
    });
}
