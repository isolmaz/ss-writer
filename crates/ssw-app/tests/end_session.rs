//! The system ending the session (`WM_QUERYENDSESSION` / `WM_ENDSESSION`) as the application's
//! windows receive it: no shutdown is held up, and the state a quit saves is saved without a
//! question (the files autosave covers, the session, the untitled buffers), while the text of
//! files with unsaved changes is kept in the recovery folder, and that of the untitled buffers
//! when the next start does not get it from the session. The session also ends inside dialogs
//! and with the core borrowed. Windows are hidden and never shown, raised or activated; the
//! messages go to the application's own windows only.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::documents::{DocEntry, replace_text};
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::host::{UiApp, UiOptions, UiServices};
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices};
use ssw_core::command::CommandId;
use ssw_core::edit::{Edit, Selection, Transaction};
use ssw_io::encoding::TextFormat;
use ssw_io::recovery::{RecoveryDoc, RecoveryKind, RecoveryStore};
use ssw_io::session::TabDocument;
use ssw_ui::services::{
    Dialogs, MemoryClipboard, MessageBoxOptions, MessageBoxResult, OpenFileOptions,
    PickFolderOptions, SaveFileOptions, ServiceError,
};
use ssw_win::dragdrop::OleGuard;
use ssw_win::gfx::Driver;
use ssw_win::menus::{PopupHost, PopupRequest};
use ssw_win::window::{current_os_version, rethrow_pending_panic};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    HMENU, SendMessageW, WM_ENDSESSION, WM_QUERYENDSESSION,
};

const AUTOSAVE_ON: &str = r#"{"general":{"autosave":true}}"#;
const AUTOSAVE_OFF: &str = r#"{"general":{"autosave":false}}"#;
const NEW_DOCUMENT: &str = r#"{"general":{"autosave":true,"on_startup":"new_document"}}"#;

/// The kind of dialog that ends the session while it is up (once), and the window that gets
/// the message.
type Trigger = Rc<Cell<Option<(DialogKind, HWND)>>>;

/// The scripted dialogs, except that the system ends the session while a dialog of the armed
/// kind is up, as it can while a dialog runs its message loop.
struct EndingDialogs {
    inner: Box<dyn Dialogs>,
    trigger: Trigger,
}

impl EndingDialogs {
    fn up(&self, kind: DialogKind) {
        if let Some((armed, hwnd)) = self.trigger.get()
            && armed == kind
        {
            self.trigger.set(None);
            deliver(hwnd, WM_ENDSESSION, 1);
        }
    }
}

impl Dialogs for EndingDialogs {
    fn open_file(
        &mut self,
        options: &OpenFileOptions,
    ) -> Result<Option<Vec<PathBuf>>, ServiceError> {
        self.up(DialogKind::OpenFile);
        self.inner.open_file(options)
    }

    fn save_file(&mut self, options: &SaveFileOptions) -> Result<Option<PathBuf>, ServiceError> {
        self.up(DialogKind::SaveFile);
        self.inner.save_file(options)
    }

    fn pick_folder(
        &mut self,
        options: &PickFolderOptions,
    ) -> Result<Option<PathBuf>, ServiceError> {
        self.up(DialogKind::PickFolder);
        self.inner.pick_folder(options)
    }

    fn message_box(
        &mut self,
        options: &MessageBoxOptions,
    ) -> Result<MessageBoxResult, ServiceError> {
        self.up(DialogKind::MessageBox);
        self.inner.message_box(options)
    }
}

struct Popups;

impl PopupHost for Popups {
    fn track(&mut self, _menu: HMENU, _request: &PopupRequest) -> Option<u32> {
        None
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

fn options(services: &ScriptedServices, trigger: &Trigger) -> UiOptions {
    UiOptions {
        hidden: true,
        services: Some(UiServices {
            dialogs: Box::new(EndingDialogs {
                inner: services.dialogs(),
                trigger: Rc::clone(trigger),
            }),
            shell: services.shell(),
            clipboard: Rc::new(RefCell::new(MemoryClipboard::default())),
            popups: Box::new(Popups),
        }),
        instance: None,
        jump_list: false,
        update_check: false,
        http: None,
        taskbar: None,
    }
}

/// Starts the application over a temporary profile with `settings` and `files` as its launch,
/// and hands the running application to `body`.
fn run_app(
    settings: &str,
    tmp: &Path,
    files: &[PathBuf],
    body: impl FnOnce(&UiApp, &ScriptedServices),
) {
    run_app_with(settings, tmp, files, &Trigger::default(), body);
}

/// [`run_app`] with dialogs that end the session as `trigger` says.
fn run_app_with(
    settings: &str,
    tmp: &Path,
    files: &[PathBuf],
    trigger: &Trigger,
    body: impl FnOnce(&UiApp, &ScriptedServices),
) {
    let args = test_args(tmp, files);
    let roaming = tmp.join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&roaming).unwrap();
    std::fs::write(roaming.join("settings.json"), settings).unwrap();
    let services = ScriptedServices::new();
    let mut dialogs = ScriptedServices::new().dialogs();
    let shell = |core: AppCore, _args: &Args| {
        let _ole = OleGuard::initialize().unwrap();
        let app = UiApp::new(core, options(&services, trigger)).unwrap();
        app.pump();
        body(&app, &services);
        0
    };
    let code = run(
        &args,
        process(&args),
        &mut *dialogs,
        None,
        &mut Body(Some(shell)),
    );
    assert_eq!(code, 0);
}

fn note(tmp: &Path, name: &str, text: &str) -> PathBuf {
    let path = tmp.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn append(entry: &DocEntry, text: &str) {
    let mut document = entry.document().borrow_mut();
    let end = document.buffer().len();
    let edit = Transaction::single(
        Edit::insert(end, text),
        Selection::caret(end),
        Selection::caret(end + text.len()),
    );
    document.apply(edit).unwrap();
}

/// The native window of the `index`-th window of the application.
fn window_of(app: &UiApp, index: usize) -> HWND {
    let id = app.controller().windows()[index].id;
    app.hwnd(id).expect("the window exists")
}

/// A message of the system to `hwnd`, a window of this thread.
fn deliver(hwnd: HWND, msg: u32, wparam: usize) -> isize {
    // SAFETY: a synchronous message to a window of this thread, answered without default
    // processing.
    let result = unsafe { SendMessageW(hwnd, msg, Some(WPARAM(wparam)), Some(LPARAM(0))) }.0;
    rethrow_pending_panic();
    result
}

/// A message of the system to the first window of the application.
fn send(app: &UiApp, msg: u32, wparam: usize) -> isize {
    deliver(window_of(app, 0), msg, wparam)
}

/// A new untitled buffer with text and the open `file` with an appended line, none of it saved.
fn unsaved_work(app: &UiApp, file: &Path) -> (Rc<DocEntry>, Rc<DocEntry>) {
    let cell = Rc::clone(&app.controller().windows()[0]);
    app.controller().new_untitled(&cell);
    app.pump();
    let scratch = cell
        .view
        .borrow()
        .active_tab()
        .expect("a tab")
        .entry
        .clone();
    let opened = app.controller().docs().find(file).expect("file is open");
    append(&opened, "two");
    append(&scratch, "scratch");
    (opened, scratch)
}

fn recovery_of(app: &UiApp) -> Arc<RecoveryStore> {
    Arc::clone(&app.controller().core().borrow().recovery)
}

/// The tabs of the first window of the stored session.
fn stored_tabs(app: &UiApp) -> Vec<TabDocument> {
    let session = app.controller().core().borrow().session.load().unwrap();
    let tabs = &session.windows[0].tabs;
    tabs.iter().map(|tab| tab.document.clone()).collect()
}

fn stored_text(app: &UiApp, id: &str) -> Option<String> {
    let env = app.controller().docs().env();
    env.store.borrow().untitled_get(id).unwrap()
}

fn log_of(tmp: &Path) -> String {
    let log = tmp
        .join("profile")
        .join("local")
        .join("SS Writer")
        .join("logs")
        .join("ssw.log");
    std::fs::read_to_string(log).unwrap()
}

fn is_file_named(tab: &TabDocument, file: &Path) -> bool {
    matches!(tab, TabDocument::File(path) if path.file_name() == file.file_name())
}

#[test]
fn ending_the_session_saves_what_autosave_covers_and_stores_the_session() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "one\n");
    let launch = std::slice::from_ref(&file);
    run_app(AUTOSAVE_ON, tmp.path(), launch, |app, services| {
        let (_, scratch) = unsaved_work(app, &file);
        let id = scratch.untitled_id().expect("an untitled id");

        assert_eq!(send(app, WM_QUERYENDSESSION, 0), 1, "no shutdown waits");
        // Another program refused: the session goes on and nothing is written.
        send(app, WM_ENDSESSION, 0);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
        assert_eq!(stored_text(app, &id), None);

        send(app, WM_ENDSESSION, 1);

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo");
        let tabs = stored_tabs(app);
        assert_eq!(tabs.len(), 2);
        assert!(is_file_named(&tabs[0], &file), "{tabs:?}");
        assert_eq!(tabs[1], TabDocument::Untitled(id.clone()));
        assert_eq!(stored_text(app, &id).as_deref(), Some("scratch"));
        assert!(
            recovery_of(app).list().unwrap().is_empty(),
            "the saved file needs no recovery copy"
        );
        assert!(services.dialog_log().is_empty(), "nothing is asked");
        assert_eq!(app.controller().windows().len(), 1, "no window closes");
    });
}

#[test]
fn ending_the_session_keeps_the_text_of_files_that_were_not_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "one\n");
    let launch = std::slice::from_ref(&file);
    run_app(AUTOSAVE_OFF, tmp.path(), launch, |app, services| {
        let (_, scratch) = unsaved_work(app, &file);
        let id = scratch.untitled_id().expect("an untitled id");

        send(app, WM_ENDSESSION, 1);

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
        let recovery = recovery_of(app);
        let kept = recovery.list().unwrap();
        assert_eq!(kept.len(), 1, "only the file: {kept:?}");
        assert_eq!(kept[0].kind, RecoveryKind::Dirty);
        assert_eq!(kept[0].display_name, "a.md");
        let origin = kept[0].original_path.clone().unwrap();
        assert_eq!(origin.file_name(), file.file_name());
        assert_eq!(recovery.restore(&kept[0].id).unwrap().text, "one\ntwo");
        assert_eq!(stored_tabs(app).len(), 2);
        assert_eq!(stored_text(app, &id).as_deref(), Some("scratch"));
        assert!(services.dialog_log().is_empty(), "nothing is asked");
    });
}

#[test]
fn a_file_whose_text_cannot_be_kept_does_not_stop_the_others() {
    let tmp = tempfile::tempdir().unwrap();
    let first_path = note(tmp.path(), "a.md", "one\n");
    let second_path = note(tmp.path(), "b.md", "uno\n");
    let launch = [first_path.clone(), second_path.clone()];
    let mut blocked = String::new();
    run_app(AUTOSAVE_OFF, tmp.path(), &launch, |app, _| {
        let docs = app.controller().docs();
        let first = docs.find(&first_path).unwrap();
        let second = docs.find(&second_path).unwrap();
        append(&first, "two");
        append(&second, "dos");
        // The recovery file of the first document cannot be replaced.
        let folder = app.controller().core().borrow().paths.recovery_dir();
        std::fs::create_dir_all(&folder).unwrap();
        blocked = app
            .controller()
            .core()
            .borrow()
            .autosave
            .recovery_id(&first);
        let data = folder.join(format!("{blocked}.txt"));
        std::fs::write(&data, "old").unwrap();
        let mut permissions = std::fs::metadata(&data).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&data, permissions).unwrap();

        send(app, WM_ENDSESSION, 1);

        let recovery = recovery_of(app);
        let kept = recovery.list().unwrap();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].display_name, "b.md");
        assert_eq!(recovery.restore(&kept[0].id).unwrap().text, "uno\ndos");
    });
    let log = log_of(tmp.path());
    assert!(
        log.contains(&format!("unsaved text of {blocked} not kept")),
        "the failure is in the log: {log}"
    );
}

#[test]
fn a_recovery_folder_that_cannot_be_written_does_not_stop_the_session_from_being_stored() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "one\n");
    let launch = std::slice::from_ref(&file);
    run_app(AUTOSAVE_OFF, tmp.path(), launch, |app, _| {
        let (_, scratch) = unsaved_work(app, &file);
        let id = scratch.untitled_id().expect("an untitled id");
        // A file stands where the recovery folder should be.
        let folder = app.controller().core().borrow().paths.recovery_dir();
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::write(&folder, "not a folder").unwrap();

        send(app, WM_ENDSESSION, 1);

        assert_eq!(stored_tabs(app).len(), 2);
        assert_eq!(stored_text(app, &id).as_deref(), Some("scratch"));
        assert_eq!(std::fs::read_to_string(&folder).unwrap(), "not a folder");
    });
    let log = log_of(tmp.path());
    assert!(
        log.contains("unsaved text not kept"),
        "the failure is in the log: {log}"
    );
}

#[test]
fn untitled_text_the_next_start_does_not_restore_is_offered_by_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "one\n");
    let launch = std::slice::from_ref(&file);
    run_app(NEW_DOCUMENT, tmp.path(), launch, |app, services| {
        let (_, scratch) = unsaved_work(app, &file);
        let id = scratch.untitled_id().expect("an untitled id");
        // Text typed and deleted again leaves nothing to offer.
        let cell = Rc::clone(&app.controller().windows()[0]);
        app.controller().new_untitled(&cell);
        let emptied = cell
            .view
            .borrow()
            .active_tab()
            .expect("a tab")
            .entry
            .clone();
        assert!(replace_text(&mut emptied.document().borrow_mut(), "typed"));
        assert!(replace_text(&mut emptied.document().borrow_mut(), ""));

        send(app, WM_ENDSESSION, 1);

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo");
        assert_eq!(stored_tabs(app).len(), 3);
        let recovery = recovery_of(app);
        let kept = recovery.list().unwrap();
        assert_eq!(kept.len(), 1, "only the buffer with text: {kept:?}");
        assert_eq!(kept[0].id, id);
        assert_eq!(kept[0].kind, RecoveryKind::Untitled);
        assert_eq!(kept[0].display_name, "Untitled");
        assert_eq!(kept[0].original_path, None);
        assert_eq!(recovery.restore(&id).unwrap().text, "scratch");
        assert!(services.dialog_log().is_empty(), "nothing is asked");
    });
}

#[test]
fn a_session_that_cannot_be_stored_keeps_the_recovery_copies_and_offers_the_untitled_text() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "one\n");
    let launch = std::slice::from_ref(&file);
    run_app(AUTOSAVE_OFF, tmp.path(), launch, |app, _| {
        let (_, scratch) = unsaved_work(app, &file);
        let id = scratch.untitled_id().expect("an untitled id");
        // The 30-second backup of a buffer that was closed since.
        let backup = RecoveryDoc {
            id: "untitled-closed",
            kind: RecoveryKind::Untitled,
            original_path: None,
            display_name: "Untitled 2",
            text: "backed up",
            format: TextFormat::default(),
        };
        recovery_of(app).backup(&backup, 1).unwrap();
        // Another connection takes the session table away.
        let database = app.controller().core().borrow().paths.state_db();
        rusqlite::Connection::open(database)
            .unwrap()
            .execute_batch("DROP TABLE session_windows")
            .unwrap();

        send(app, WM_ENDSESSION, 1);

        let recovery = recovery_of(app);
        let kept = recovery.list().unwrap();
        assert_eq!(kept.len(), 3, "{kept:?}");
        let text_of = |id: &str| recovery.restore(id).unwrap().text;
        assert_eq!(text_of("untitled-closed"), "backed up");
        assert_eq!(text_of(&id), "scratch");
        let scratch_entry = kept.iter().find(|entry| entry.id == id).unwrap();
        assert_eq!(scratch_entry.kind, RecoveryKind::Untitled);
        let dirty = kept
            .iter()
            .find(|entry| entry.kind == RecoveryKind::Dirty)
            .expect("the file");
        assert_eq!(text_of(&dirty.id), "one\ntwo");
    });
    let log = log_of(tmp.path());
    assert!(log.contains("session not saved"), "{log}");
}

fn ends_with_the_core_held(writing: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "one\n");
    let launch = std::slice::from_ref(&file);
    run_app(AUTOSAVE_OFF, tmp.path(), launch, |app, services| {
        let (_, scratch) = unsaved_work(app, &file);
        let id = scratch.untitled_id().expect("an untitled id");
        // The tick copies the unsaved text for the crash handler.
        app.pump();

        let core = app.controller().core();
        if writing {
            let _held = core.borrow_mut();
            send(app, WM_ENDSESSION, 1);
        } else {
            let _held = core.borrow();
            send(app, WM_ENDSESSION, 1);
        }

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
        let recovery = recovery_of(app);
        let kept = recovery.list().unwrap();
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert_eq!(recovery.restore(&id).unwrap().text, "scratch");
        let scratch_entry = kept.iter().find(|entry| entry.id == id).unwrap();
        assert_eq!(scratch_entry.kind, RecoveryKind::Untitled);
        let dirty = kept
            .iter()
            .find(|entry| entry.kind == RecoveryKind::Dirty)
            .expect("the file");
        assert_eq!(dirty.display_name, "a.md");
        assert_eq!(recovery.restore(&dirty.id).unwrap().text, "one\ntwo");
        let session = app.controller().core().borrow().session.load().unwrap();
        assert!(session.windows.is_empty(), "the session is not stored");
        assert!(services.dialog_log().is_empty(), "nothing is asked");
    });
    let log = log_of(tmp.path());
    assert!(log.contains("the core is busy"), "{log}");
}

#[test]
fn ending_the_session_while_the_core_is_read_keeps_the_unsaved_text_from_the_crash_copy() {
    ends_with_the_core_held(false);
}

#[test]
fn ending_the_session_while_the_core_is_written_keeps_the_unsaved_text_from_the_crash_copy() {
    ends_with_the_core_held(true);
}

/// What an ended session leaves when the session is stored and restored: the tabs and the
/// untitled text in the session, the text of the file nobody saved in the recovery folder.
fn assert_ended_like_a_quit(app: &UiApp, scratch_id: &str) {
    assert_eq!(stored_tabs(app).len(), 2);
    assert_eq!(stored_text(app, scratch_id).as_deref(), Some("scratch"));
    let recovery = recovery_of(app);
    let kept = recovery.list().unwrap();
    assert_eq!(kept.len(), 1, "only the file: {kept:?}");
    assert_eq!(kept[0].kind, RecoveryKind::Dirty);
    assert_eq!(recovery.restore(&kept[0].id).unwrap().text, "one\ntwo");
}

#[test]
fn the_session_can_end_while_a_command_shows_a_dialog() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "one\n");
    let launch = std::slice::from_ref(&file);
    let trigger = Trigger::default();
    run_app_with(AUTOSAVE_OFF, tmp.path(), launch, &trigger, |app, _| {
        let (_, scratch) = unsaved_work(app, &file);
        let id = scratch.untitled_id().expect("an untitled id");
        trigger.set(Some((DialogKind::OpenFile, window_of(app, 0))));
        let window = app.controller().windows()[0].id;

        // File → Open: the system ends the session while the dialog is up.
        app.controller().run_command(window, CommandId::FileOpen);

        assert!(trigger.get().is_none(), "the dialog was up");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
        assert_ended_like_a_quit(app, &id);
    });
}

#[test]
fn the_session_can_end_while_a_worker_result_shows_a_message_box() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "one\n");
    let launch = std::slice::from_ref(&file);
    let refused = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/doc/PasswordProtected.doc");
    let trigger = Trigger::default();
    run_app_with(
        AUTOSAVE_OFF,
        tmp.path(),
        launch,
        &trigger,
        |app, services| {
            let (_, scratch) = unsaved_work(app, &file);
            let id = scratch.untitled_id().expect("an untitled id");
            // The importer refuses the file on a worker; the refusal is reported when the loop
            // delivers the result.
            services.answer(DialogKind::OpenFile, Answer::Paths(vec![refused]));
            let window = app.controller().windows()[0].id;
            app.controller().run_command(window, CommandId::FileOpen);
            trigger.set(Some((DialogKind::MessageBox, window_of(app, 0))));

            let deadline = Instant::now() + Duration::from_secs(30);
            while trigger.get().is_some() {
                assert!(Instant::now() < deadline, "the refusal was not reported");
                app.pump();
                std::thread::sleep(Duration::from_millis(5));
            }

            assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
            assert_ended_like_a_quit(app, &id);
        },
    );
}

#[test]
fn the_end_of_the_session_is_handled_once_however_many_windows_hear_it() {
    let tmp = tempfile::tempdir().unwrap();
    let file = note(tmp.path(), "a.md", "one\n");
    let launch = std::slice::from_ref(&file);
    run_app(AUTOSAVE_OFF, tmp.path(), launch, |app, _| {
        unsaved_work(app, &file);
        let first = app.controller().windows()[0].id;
        app.controller()
            .run_command(first, CommandId::FileNewWindow);
        app.pump();
        assert_eq!(app.controller().windows().len(), 2);

        deliver(window_of(app, 0), WM_ENDSESSION, 1);
        // A second round would delete what this run wrote, the copies of the first included.
        let marker = RecoveryDoc {
            id: "marker",
            kind: RecoveryKind::Untitled,
            original_path: None,
            display_name: "Untitled 2",
            text: "left by the first round",
            format: TextFormat::default(),
        };
        recovery_of(app).backup(&marker, 1).unwrap();
        deliver(window_of(app, 1), WM_ENDSESSION, 1);

        let kept = recovery_of(app).list().unwrap();
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(kept.iter().any(|entry| entry.id == "marker"), "{kept:?}");
        assert!(
            kept.iter().any(|entry| entry.kind == RecoveryKind::Dirty),
            "{kept:?}"
        );
    });
    let log = log_of(tmp.path());
    assert_eq!(log.matches("SS Writer exiting").count(), 1, "{log}");
}
