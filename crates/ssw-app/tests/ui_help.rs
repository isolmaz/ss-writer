//! The Help menu (spec §1, §13): the welcome and shortcut documents, the About box, "Report an
//! issue" and the update check with its banner. The real controller runs over a started
//! application in a temporary profile; dialogs and the shell are scripted, the network answers
//! from a script, and what the user would see is read back from the document text, the bytes on
//! disk, the dialogs shown, the shell requests and the accessibility tree. Nothing is shown.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::documents::{DocError, canonical_path};
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::{Controller, Request, WindowCell};
use ssw_app::ui::help::{
    ISSUES_URL, THIRD_PARTY_NOTICES, about_message, shortcuts_markdown, write_help_file,
};
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices, ShellRecord};
use ssw_app::update_apply::Launcher;
use ssw_core::command::CommandId;
use ssw_io::fs::{FaultFs, FileSystem, FsFailure, FsOp, RealFs};
use ssw_io::net::{Http, HttpRequest, HttpResponse, NetError, ScriptedHttp, Url};
use ssw_io::update::RELEASES_URL;
use ssw_ui::access::{AccessNode, Role};
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::geometry::Point;
use ssw_ui::input::{Event, PointerButton, PointerEvent, PointerEventKind};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard, MessageBoxResult};
use ssw_ui::shell::banners::BannerKind;
use ssw_ui::shell::settings::DisplayKeyNames;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::window::current_os_version;

const JSON_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json";

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
    engine: MonoTextEngine,
}

struct Host<B> {
    body: Option<B>,
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

/// Runs `body` over a started application whose first window shows `notes.md`.
fn with_rig(settings: &str, body: impl FnOnce(&Rig)) {
    with_rig_of(settings, false, body);
}

/// The same application, started as a portable copy (the `portable` marker next to the
/// executable), which has no package to install.
fn with_portable_rig(body: impl FnOnce(&Rig)) {
    with_rig_of(NO_AUTOSAVE, true, body);
}

fn with_rig_of(settings: &str, portable: bool, body: impl FnOnce(&Rig)) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("notes.md"), "# Notes\n\nhello\n").unwrap();
    let roaming = tmp.path().join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&roaming).unwrap();
    std::fs::write(roaming.join("settings.json"), settings).unwrap();
    let exe_dir = tmp.path().join("profile").join("exe");
    std::fs::create_dir_all(&exe_dir).unwrap();
    if portable {
        std::fs::write(exe_dir.join("portable"), []).unwrap();
    }
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
            services: ScriptedServices::new(),
        },
    );
    assert_eq!(code, 0);
}

impl Rig {
    fn window(&self) -> Rc<WindowCell> {
        Rc::clone(&self.controller.windows()[0])
    }

    fn command(&self, command: CommandId) {
        self.controller.run_command(self.window().id, command);
    }

    fn tab_count(&self) -> usize {
        self.window().view.borrow().tabs().len()
    }

    /// The path, the text and the read-only flag of the active tab.
    fn active(&self) -> (PathBuf, String, bool) {
        let window = self.window();
        let view = window.view.borrow();
        let tab = view.active_tab().expect("a tab is open");
        let text = tab.entry.document().borrow().text();
        (
            tab.entry.path().expect("a file document"),
            text,
            tab.editor.is_read_only(),
        )
    }

    fn banners(&self) -> Vec<BannerKind> {
        self.window().view.borrow_mut().banners_mut().kinds()
    }

    fn answer(&self, button: MessageBoxResult) {
        self.services
            .answer(DialogKind::MessageBox, Answer::Button(button));
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

    fn nodes(&self) -> Vec<AccessNode> {
        let window = self.window();
        let mut view = window.view.borrow_mut();
        view.paint(&mut RecordingCanvas::new(), &self.engine, false);
        view.access_tree(&self.engine, "SS Writer").nodes().to_vec()
    }

    /// Whether anything on screen carries `text` in its name, description or value.
    fn shows(&self, text: &str) -> bool {
        self.nodes().iter().any(|node| {
            node.name.contains(text)
                || node.description.contains(text)
                || node
                    .value
                    .as_deref()
                    .is_some_and(|value| value.contains(text))
        })
    }

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
        let nodes = self.nodes();
        let area = self
            .window()
            .view
            .borrow_mut()
            .banners_mut()
            .banner_rect(kind)
            .unwrap_or_else(|| panic!("the {kind:?} banner is not shown"));
        let node = nodes
            .iter()
            .find(|node| {
                node.role == Role::Button
                    && node.name == name
                    && area.contains(node.bounds.center())
            })
            .unwrap_or_else(|| panic!("no button {name:?} in the {kind:?} banner"));
        self.click(node.bounds.center());
    }

    /// Points the restart at `launcher` and at a small file of its own instead of the test
    /// executable. Returns that file and the folder the updater copy goes into.
    fn updater_seam(&self, launcher: Arc<dyn Launcher>) -> (PathBuf, PathBuf) {
        let dir = self
            .controller
            .core()
            .borrow()
            .paths
            .local_dir()
            .join("exe");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("SSWriter.exe");
        std::fs::write(&exe, b"the application").unwrap();
        let temp = dir.join("temp");
        std::fs::create_dir_all(&temp).unwrap();
        self.controller
            .set_updater(launcher, exe.clone(), temp.clone());
        (exe, temp)
    }

    /// What the controller asked the loop owner to do.
    fn requests(&self) -> Vec<Request> {
        std::iter::from_fn(|| self.controller.take_request()).collect()
    }

    fn help_dir(&self) -> PathBuf {
        self.controller
            .core()
            .borrow()
            .paths
            .local_dir()
            .join("help")
    }
}

/// The release feed's answer for a release with `tag`.
fn release_json(tag: &str) -> Vec<u8> {
    let version = tag.trim_start_matches('v');
    format!(
        r#"{{"tag_name":"{tag}","html_url":"https://github.com/isolmaz/ss-writer/releases/tag/{tag}","assets":[{{"name":"SSWriter-{version}-x64.msi","browser_download_url":"{}","size":9}},{{"name":"SHA256SUMS.txt","browser_download_url":"{}","size":78}}]}}"#,
        installer_url(tag),
        checksums_url(tag),
    )
    .into_bytes()
}

/// Where the fake release serves its package and its checksum list.
fn installer_url(tag: &str) -> String {
    let version = tag.trim_start_matches('v');
    format!("https://downloads.invalid/{tag}/SSWriter-{version}-x64.msi")
}

fn checksums_url(tag: &str) -> String {
    format!("https://downloads.invalid/{tag}/SHA256SUMS.txt")
}

fn feed_url() -> String {
    Url::parse(RELEASES_URL).unwrap().to_string()
}

/// A network whose first answer is the release `tag`.
fn network_with(tags: &[&str]) -> Arc<ScriptedHttp> {
    let http = Arc::new(ScriptedHttp::new());
    for tag in tags {
        http.respond(&feed_url(), JSON_HEAD, &release_json(tag));
    }
    http
}

fn lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

// ----- the documents ----------------------------------------------------------------------------

#[test]
fn the_shortcut_document_is_written_below_the_local_folder_and_opens_read_only() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.command(CommandId::HelpKeyboardShortcuts);
        let (path, text, read_only) = rig.active();
        assert_eq!(
            canonical_path(path.parent().unwrap()),
            canonical_path(&rig.help_dir())
        );
        assert_eq!(path.file_name().unwrap(), "Keyboard shortcuts.md");
        let expected = shortcuts_markdown(
            rig.controller.i18n(),
            &rig.controller.keymap(),
            &DisplayKeyNames,
        );
        assert_eq!(text, expected);
        assert_eq!(lf(&std::fs::read_to_string(&path).unwrap()), expected);
        assert!(read_only, "help pages cannot be edited");
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
        assert_eq!(rig.tab_count(), 2);

        // Help pages are not the user's files: they stay out of the recent list.
        let recent = rig
            .controller
            .docs()
            .env()
            .store
            .borrow()
            .recent_items(ssw_io::session::RecentKind::File)
            .unwrap();
        assert!(recent.iter().all(|item| item.path != path), "{recent:?}");

        // Asking again shows the tab that is open instead of a second one.
        rig.command(CommandId::HelpKeyboardShortcuts);
        assert_eq!(rig.tab_count(), 2);
    });
}

#[test]
fn the_welcome_document_opens_read_only_from_the_help_folder() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.command(CommandId::HelpWelcome);
        let (path, text, read_only) = rig.active();
        assert_eq!(
            canonical_path(path.parent().unwrap()),
            canonical_path(&rig.help_dir())
        );
        assert_eq!(path.file_name().unwrap(), "Welcome.md");
        assert!(!text.trim().is_empty());
        assert_eq!(lf(&std::fs::read_to_string(&path).unwrap()), text);
        assert!(read_only);
    });
}

#[test]
fn a_help_folder_that_cannot_be_created_is_reported() {
    with_rig(NO_AUTOSAVE, |rig| {
        // A file where the folder belongs.
        let local = rig.help_dir().parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(rig.help_dir(), "in the way").unwrap();
        rig.answer(MessageBoxResult::Ok);
        rig.command(CommandId::HelpKeyboardShortcuts);
        let shown = rig.services.dialog_log();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].title, "SS Writer");
        assert_eq!(rig.tab_count(), 1, "no help tab was opened");
    });
}

#[test]
fn a_help_page_that_cannot_be_opened_is_reported_and_leaves_no_tab() {
    with_rig(NO_AUTOSAVE, |rig| {
        // The recent list is gone: opening any document fails when it is recorded.
        let database = rig.controller.core().borrow().paths.state_db();
        rusqlite::Connection::open(database)
            .unwrap()
            .execute_batch("DROP TABLE recent_items")
            .unwrap();
        rig.answer(MessageBoxResult::Ok);
        rig.command(CommandId::HelpKeyboardShortcuts);
        assert_eq!(rig.services.dialog_log().len(), 1);
        assert_eq!(rig.tab_count(), 1);
    });
}

#[test]
fn the_about_box_names_the_version_and_yes_opens_the_third_party_notices() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.answer(MessageBoxResult::Yes);
        rig.command(CommandId::HelpAbout);
        let shown = rig.services.dialog_log();
        assert_eq!(shown[0].title, "About SS Writer");
        assert_eq!(shown[0].message, about_message(rig.controller.i18n()));
        assert!(shown[0].message.contains(env!("CARGO_PKG_VERSION")));
        let (path, text, read_only) = rig.active();
        assert_eq!(path.file_name().unwrap(), "Third-party notices.md");
        assert_eq!(text, lf(THIRD_PARTY_NOTICES));
        assert!(read_only);
    });
}

#[test]
fn no_in_the_about_box_opens_nothing() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.answer(MessageBoxResult::No);
        rig.command(CommandId::HelpAbout);
        assert_eq!(rig.tab_count(), 1);
        assert!(!rig.help_dir().exists(), "no help file was written");
    });
}

#[test]
fn report_an_issue_opens_the_issue_page_in_the_browser() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.command(CommandId::HelpReportIssue);
        assert_eq!(
            rig.services.shell_log(),
            [ShellRecord::OpenUrl(ISSUES_URL.to_owned())]
        );
        assert_eq!(rig.tab_count(), 1);
    });
}

#[test]
fn commands_that_are_not_help_commands_are_left_alone() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        assert!(
            !rig.controller
                .run_help_command(&window, CommandId::FileSave)
        );
        assert!(
            rig.controller
                .run_help_command(&window, CommandId::HelpReportIssue)
        );
        assert_eq!(rig.services.shell_log().len(), 1);
        assert!(rig.services.dialog_log().is_empty());
    });
}

// ----- write_help_file --------------------------------------------------------------------------

fn fault_with(op: FsOp, times: usize) -> FaultFs {
    let fs = FaultFs::new();
    fs.fail(op, FsFailure::AccessDenied, times);
    fs
}

#[test]
fn a_help_file_that_cannot_be_written_fails_at_every_step() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("help");

    // The folder cannot be created.
    let result = write_help_file(&fault_with(FsOp::CreateDirAll, 1), &dir, "A.md", "one\n");
    assert!(matches!(result, Err(DocError::Fs(_))), "{result:?}");
    assert!(!dir.join("A.md").exists());

    // The status of the file cannot be read (anything but "not found").
    let result = write_help_file(&fault_with(FsOp::Metadata, 1), &dir, "A.md", "one\n");
    assert!(matches!(result, Err(DocError::Fs(_))), "{result:?}");

    // The file cannot be created.
    let result = write_help_file(&fault_with(FsOp::WriteNew, 1), &dir, "A.md", "one\n");
    assert!(matches!(result, Err(DocError::Fs(_))), "{result:?}");

    // It is created but cannot be made read-only.
    let result = write_help_file(&fault_with(FsOp::SetAttributes, 1), &dir, "B.md", "one\n");
    assert!(matches!(result, Err(DocError::Fs(_))), "{result:?}");

    // An existing file: its content cannot be compared, its attributes cleared or it replaced.
    let path = write_help_file(&RealFs, &dir, "C.md", "old\n").unwrap();
    for op in [FsOp::Read, FsOp::SetAttributes, FsOp::DeleteFile] {
        let result = write_help_file(&fault_with(op, 1), &dir, "C.md", "new\n");
        assert!(matches!(result, Err(DocError::Fs(_))), "{op:?}: {result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n", "{op:?}");
    }
    // Once the faults are gone the new text is written (and the file is read-only again).
    write_help_file(&RealFs, &dir, "C.md", "new\n").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
    assert!(RealFs.metadata(&path).unwrap().attrs.is_readonly());
}

// ----- the update check -------------------------------------------------------------------------

#[test]
fn checking_for_updates_now_offers_a_newer_release_in_a_banner() {
    with_rig(NO_AUTOSAVE, |rig| {
        let http = network_with(&["v99.0.0"]);
        rig.controller
            .set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        assert_eq!(rig.controller.available_update(), None);
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();

        let release = rig.controller.available_update().expect("an offer");
        assert_eq!(release.tag, "v99.0.0");
        assert_eq!(
            release.page_url,
            "https://github.com/isolmaz/ss-writer/releases/tag/v99.0.0"
        );
        assert_eq!(http.requests(), [feed_url()]);
        assert_eq!(rig.banners(), [BannerKind::Update]);
        assert!(rig.shows("99.0.0"), "the banner names the version");
    });
}

/// A package served as `installer` and the checksum list that covers it.
const PACKAGE: &[u8] = b"installer";
const PACKAGE_SHA256: &str = "9c0d294c05fc1d88d698034609bb81c0c69196327594e4c69d2915c80fd9850c";
const TEXT_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/plain";
const PACKAGE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream";

/// Records what the application asked to start instead of starting a process.
#[derive(Debug, Default)]
struct RecordingLauncher(Mutex<Vec<(PathBuf, Vec<OsString>)>>);

impl RecordingLauncher {
    fn launches(&self) -> Vec<(PathBuf, Vec<OsString>)> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Launcher for RecordingLauncher {
    fn launch(&self, program: &Path, args: &[OsString]) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((program.to_path_buf(), args.to_vec()));
        Ok(())
    }
}

/// Serves the release, its checksum list and its package.
fn update_network(tag: &str, sums: &str) -> Arc<ScriptedHttp> {
    let http = network_with(&[tag]);
    http.respond(&checksums_url(tag), TEXT_HEAD, sums.as_bytes());
    http.respond(&installer_url(tag), PACKAGE_HEAD, PACKAGE);
    http
}

#[test]
fn downloading_the_update_fills_the_banner_and_the_restart_starts_the_updater() {
    with_rig(NO_AUTOSAVE, |rig| {
        let sums = format!("{PACKAGE_SHA256}  SSWriter-99.0.0-x64.msi\n");
        let http = update_network("v99.0.0", &sums);
        rig.controller
            .set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        let launcher = Arc::new(RecordingLauncher::default());
        let (exe, temp) = rig.updater_seam(Arc::clone(&launcher) as Arc<dyn Launcher>);
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();

        rig.press_in(BannerKind::Update, "Download");
        rig.settle();
        // The package was fetched after its checksum list, and verified.
        assert_eq!(
            http.requests(),
            [
                feed_url(),
                checksums_url("v99.0.0"),
                installer_url("v99.0.0")
            ]
        );
        let package = rig
            .controller
            .core()
            .borrow()
            .paths
            .local_dir()
            .join("updates")
            .join("SSWriter-99.0.0-x64.msi");
        assert_eq!(std::fs::read(&package).unwrap(), PACKAGE);
        // The banner now offers the restart instead of the download.
        assert_eq!(rig.banners(), [BannerKind::Update]);
        assert!(rig.shows("ready to install"), "the banner names the state");
        assert!(rig.services.shell_log().is_empty(), "no browser is opened");

        rig.press_in(BannerKind::Update, "Restart and install");
        let launches = launcher.launches();
        assert_eq!(launches.len(), 1, "{launches:?}");
        let (program, args) = &launches[0];
        assert!(
            program.starts_with(&temp) && program.ends_with("SSWriter-updater.exe"),
            "{program:?} is a copy below {temp:?}"
        );
        assert_eq!(
            std::fs::read(program).unwrap(),
            std::fs::read(&exe).unwrap()
        );
        assert_eq!(
            args.iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "--apply-update".to_owned(),
                package.display().to_string(),
                "--updater-parent".to_owned(),
                std::process::id().to_string(),
                "--updater-target".to_owned(),
                exe.display().to_string(),
            ]
        );
        let requests = rig.requests();
        assert!(requests.contains(&Request::Quit), "{requests:?}");
    });
}

#[test]
fn a_package_that_does_not_match_its_checksum_is_refused_and_the_button_comes_back() {
    with_rig(NO_AUTOSAVE, |rig| {
        let wrong = format!("{:064x}  SSWriter-99.0.0-x64.msi\n", 1);
        let http = update_network("v99.0.0", &wrong);
        rig.controller
            .set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();
        rig.answer(MessageBoxResult::Ok);
        rig.press_in(BannerKind::Update, "Download");
        rig.settle();

        let shown = rig.services.dialog_log();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].title, "Download failed");
        assert!(shown[0].message.contains("checksum"), "{:?}", shown[0]);
        let dir = rig
            .controller
            .core()
            .borrow()
            .paths
            .local_dir()
            .join("updates");
        assert!(
            std::fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_none()),
            "nothing is kept in {dir:?}"
        );
        assert!(
            rig.shows("Download"),
            "the banner offers the download again"
        );
    });
}

#[test]
fn a_download_runs_once_and_a_crashing_one_is_reported() {
    with_rig(NO_AUTOSAVE, |rig| {
        let sums = format!("{PACKAGE_SHA256}  SSWriter-99.0.0-x64.msi\n");
        let http = update_network("v99.0.0", &sums);
        rig.controller
            .set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();

        // Pressing Download twice starts one download: the second press finds one running.
        rig.press_in(BannerKind::Update, "Download");
        rig.controller.start_update_download();
        rig.settle();
        assert_eq!(
            http.requests(),
            [
                feed_url(),
                checksums_url("v99.0.0"),
                installer_url("v99.0.0")
            ]
        );
        // The package is there, so a press after the download is ignored as well.
        rig.controller.start_update_download();
        rig.settle();
        assert_eq!(http.requests().len(), 3);
    });

    with_rig(NO_AUTOSAVE, |rig| {
        let http = network_with(&["v99.0.0"]);
        rig.controller
            .set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();
        // The download runs on a worker that panics; the editor survives and says why.
        rig.controller
            .set_update_http(Arc::new(CrashingNetwork) as Arc<dyn Http>);
        rig.answer(MessageBoxResult::Ok);
        rig.press_in(BannerKind::Update, "Download");
        rig.settle();
        let shown = rig.services.dialog_log();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].title, "Download failed");
        assert!(
            shown[0].message.contains("the network stack crashed"),
            "{}",
            shown[0].message
        );
        assert!(
            rig.shows("Download"),
            "the banner offers the download again"
        );
    });
}

#[test]
fn an_updater_that_cannot_be_prepared_is_reported_and_the_package_stays() {
    with_rig(NO_AUTOSAVE, |rig| {
        let sums = format!("{PACKAGE_SHA256}  SSWriter-99.0.0-x64.msi\n");
        let http = update_network("v99.0.0", &sums);
        rig.controller
            .set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();
        rig.press_in(BannerKind::Update, "Download");
        rig.settle();

        // No seam: the application copies the executable of its own folder, which the test
        // profile does not have.
        rig.answer(MessageBoxResult::Ok);
        rig.press_in(BannerKind::Update, "Restart and install");
        let shown = rig.services.dialog_log();
        assert_eq!(shown.len(), 1);
        assert!(
            shown[0].message.contains("cannot copy the application"),
            "{}",
            shown[0].message
        );
        let requests = rig.requests();
        assert!(
            !requests.contains(&Request::Quit),
            "the application stays: {requests:?}"
        );
        let package = rig
            .controller
            .core()
            .borrow()
            .paths
            .local_dir()
            .join("updates")
            .join("SSWriter-99.0.0-x64.msi");
        assert!(package.is_file(), "the package waits for another attempt");
    });
}

#[test]
fn a_portable_build_opens_the_release_page_instead_of_downloading() {
    with_portable_rig(|rig| {
        let http = network_with(&["v99.0.0"]);
        rig.controller
            .set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();
        rig.press_in(BannerKind::Update, "Download");
        rig.settle();
        assert_eq!(
            rig.services.shell_log(),
            [ShellRecord::OpenUrl(
                "https://github.com/isolmaz/ss-writer/releases/tag/v99.0.0".to_owned()
            )]
        );
        assert_eq!(http.requests(), [feed_url()], "nothing is downloaded");
    });
}

#[test]
fn a_newer_offer_replaces_the_banner_and_a_closed_banner_stays_closed() {
    with_rig(NO_AUTOSAVE, |rig| {
        let http = network_with(&["v98.0.0", "v99.0.0"]);
        rig.controller
            .set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();
        assert!(rig.shows("98.0.0"));
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();
        assert!(rig.shows("99.0.0"));
        assert!(
            !rig.shows("98.0.0"),
            "the banner of the older offer is gone"
        );
        assert_eq!(rig.banners(), [BannerKind::Update]);

        // The close button: the banner stays closed for this run.
        rig.press_in(BannerKind::Update, "Close");
        assert!(rig.banners().is_empty());
        rig.controller.refresh_all();
        assert!(rig.banners().is_empty());
        // The offer itself is still known.
        assert_eq!(rig.controller.available_update().unwrap().tag, "v99.0.0");
    });
}

#[test]
fn checking_now_tells_when_this_is_the_latest_version() {
    with_rig(NO_AUTOSAVE, |rig| {
        let http = network_with(&["v0.0.1"]);
        rig.controller.set_update_http(http as Arc<dyn Http>);
        rig.answer(MessageBoxResult::Ok);
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();
        let shown = rig.services.dialog_log();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].title, "Check for updates");
        assert!(
            shown[0].message.contains(env!("CARGO_PKG_VERSION"))
                && shown[0].message.contains("latest version"),
            "{}",
            shown[0].message
        );
        assert_eq!(rig.controller.available_update(), None);
        assert!(rig.banners().is_empty());
    });
}

#[test]
fn checking_now_reports_an_unreachable_server() {
    with_rig(NO_AUTOSAVE, |rig| {
        // Nothing is scripted: the host cannot be reached.
        rig.controller
            .set_update_http(Arc::new(ScriptedHttp::new()) as Arc<dyn Http>);
        rig.answer(MessageBoxResult::Ok);
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();
        let shown = rig.services.dialog_log();
        assert_eq!(shown.len(), 1);
        assert!(
            shown[0].message.contains("could not check for updates"),
            "{}",
            shown[0].message
        );
        assert_eq!(rig.controller.available_update(), None);
    });
}

/// A network that crashes the worker that asks it.
#[derive(Debug)]
struct CrashingNetwork;

impl Http for CrashingNetwork {
    fn get(&self, _request: &HttpRequest<'_>) -> Result<HttpResponse, NetError> {
        panic!("the network stack crashed");
    }
}

#[test]
fn a_crashing_check_is_reported_and_the_application_goes_on() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.controller
            .set_update_http(Arc::new(CrashingNetwork) as Arc<dyn Http>);
        rig.answer(MessageBoxResult::Ok);
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();
        let shown = rig.services.dialog_log();
        assert_eq!(shown.len(), 1);
        assert!(
            shown[0].message.contains("the network stack crashed"),
            "{}",
            shown[0].message
        );
        assert_eq!(rig.controller.available_update(), None);
    });
}

#[test]
fn the_weekly_check_offers_a_release_once_and_is_not_due_again() {
    with_rig(NO_AUTOSAVE, |rig| {
        let http = network_with(&["v99.0.0", "v99.0.0"]);
        rig.controller
            .set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        rig.controller.start_update_check();
        rig.settle();
        assert_eq!(rig.controller.available_update().unwrap().tag, "v99.0.0");
        assert_eq!(rig.banners(), [BannerKind::Update]);
        assert_eq!(http.requests().len(), 1);

        // The successful check was recorded: a week has not passed, nothing is asked.
        rig.controller.start_update_check();
        rig.settle();
        assert_eq!(http.requests().len(), 1);
    });
}

#[test]
fn the_weekly_check_is_silent_when_nothing_is_new_or_the_network_fails() {
    with_rig(NO_AUTOSAVE, |rig| {
        // Nothing scripted: the check fails without a word.
        let http = Arc::new(ScriptedHttp::new());
        rig.controller
            .set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        rig.controller.start_update_check();
        rig.settle();
        assert_eq!(http.requests().len(), 1);
        assert_eq!(rig.controller.available_update(), None);

        // An old release: no offer, no dialog.
        http.respond(&feed_url(), JSON_HEAD, &release_json("v0.0.1"));
        rig.controller.start_update_check();
        rig.settle();
        assert_eq!(http.requests().len(), 2);
        assert_eq!(rig.controller.available_update(), None);
        assert!(rig.banners().is_empty());
        assert!(rig.services.dialog_log().is_empty());
    });
}

#[test]
fn the_weekly_check_does_nothing_when_the_setting_is_off() {
    let settings = r#"{"general":{"autosave":false,"check_for_updates":false}}"#;
    with_rig(settings, |rig| {
        let http = network_with(&["v99.0.0"]);
        rig.controller
            .set_update_http(Arc::clone(&http) as Arc<dyn Http>);
        rig.controller.start_update_check();
        rig.settle();
        assert!(http.requests().is_empty());
        assert_eq!(rig.controller.available_update(), None);

        // "Check for updates now" ignores the setting.
        rig.command(CommandId::HelpCheckForUpdates);
        rig.settle();
        assert_eq!(http.requests().len(), 1);
        assert_eq!(rig.controller.available_update().unwrap().tag, "v99.0.0");
    });
}
