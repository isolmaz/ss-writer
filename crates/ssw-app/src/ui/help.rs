//! The Help menu (spec §1, §13): the welcome and keyboard shortcut documents, the About box
//! with the third-party notices, "Report an issue", and the update check (weekly at startup
//! when the setting allows it, and "Check for updates now").
//!
//! Documents are written as read-only files below `<local data>\help\` and opened like any
//! other file; the update check runs on a worker and only ever tells the user (a banner with a
//! Download button that opens the release page); nothing is downloaded.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use ssw_core::command::{CommandGroup, CommandId};
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::keymap::Keymap;
use ssw_io::fs::{Attributes, FileSystem, FsError};
use ssw_io::logging::Level;
use ssw_io::net::{Http, WinHttp};
use ssw_io::session::RecentKind;
use ssw_io::store::Store;
use ssw_io::update::{
    RELEASES_URL, Release, UpdateError, UpdateStatus, Version, check_for_updates, latest_release,
};
use ssw_ui::services::{MessageBoxButtons, MessageBoxIcon, MessageBoxOptions, MessageBoxResult};
use ssw_ui::shell::banners::BannerKind;
use ssw_ui::shell::settings::{DisplayKeyNames, KeyNames};

use super::controller::{Controller, WindowCell};
use super::shell_view::ShellView;
use crate::documents::{DocError, canonical_path};
use crate::i18n::welcome_text;
use crate::workers::{JobKey, JobKind, JobSpec};

/// Where "Report an issue" goes.
pub const ISSUES_URL: &str = "https://github.com/isolmaz/ss-writer/issues/new";

/// The license texts of the components SS Writer is built from (`assets/THIRD_PARTY.md`).
pub const THIRD_PARTY_NOTICES: &str = include_str!("../../../../assets/THIRD_PARTY.md");

/// The worker key of the update check (one check at a time; a newer one supersedes).
const UPDATE_JOB_KEY: JobKey = JobKey(0x5353_5750_5550_4401);

/// The release the user was told about and whether the banner was closed.
#[derive(Debug, Default)]
struct Offer {
    release: Option<Release>,
    dismissed: bool,
}

/// What the Help commands remember between calls.
#[derive(Debug)]
pub(super) struct HelpState {
    http: RefCell<Arc<dyn Http>>,
    offer: RefCell<Offer>,
    generation: Cell<u64>,
}

impl Default for HelpState {
    fn default() -> Self {
        Self {
            http: RefCell::new(Arc::new(WinHttp)),
            offer: RefCell::default(),
            generation: Cell::new(0),
        }
    }
}

/// A code span that survives inside a table cell: pipes are escaped and the fence is longer
/// than any backtick run in the text.
fn code_span(text: &str) -> String {
    let text = text.replace('|', "\\|");
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    if longest == 0 {
        format!("{fence}{text}{fence}")
    } else {
        format!("{fence} {text} {fence}")
    }
}

/// The keyboard shortcut reference: one table per command group, listing the commands that have
/// a chord in `keymap` (so rebinding in Settings is reflected).
#[must_use]
pub fn shortcuts_markdown(i18n: &Localizer, keymap: &Keymap, names: &dyn KeyNames) -> String {
    let mut out = format!(
        "# {}\n\n{}\n",
        i18n.text("help-shortcuts-title"),
        i18n.text("help-shortcuts-intro")
    );
    for &group in CommandGroup::ALL {
        let rows: Vec<String> = CommandId::all()
            .filter(|command| command.group() == group)
            .filter_map(|command| {
                let chords: Vec<String> = keymap
                    .chords_for(command)
                    .map(|chord| code_span(&names.chord_text(chord)))
                    .collect();
                (!chords.is_empty()).then(|| {
                    format!(
                        "| {} | {} |\n",
                        i18n.text(command.label_id()),
                        chords.join(", ")
                    )
                })
            })
            .collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "\n## {}\n\n| {} | {} |\n| --- | --- |\n",
            i18n.text(group.label_id()),
            i18n.text("help-shortcuts-command"),
            i18n.text("help-shortcuts-keys"),
        ));
        out.extend(rows);
    }
    out
}

/// The text of the About box.
#[must_use]
pub fn about_message(i18n: &Localizer) -> String {
    i18n.format(
        "help-about-message",
        &[("version", Arg::Str(env!("CARGO_PKG_VERSION")))],
    )
}

/// Writes `text` as the read-only file `name` in `dir` and returns its path. A file that
/// already has exactly this content is left alone (an open tab is not reloaded for nothing).
///
/// # Errors
/// [`DocError::Fs`] when the folder or file cannot be created or replaced.
pub fn write_help_file(
    fs: &dyn FileSystem,
    dir: &Path,
    name: &str,
    text: &str,
) -> Result<PathBuf, DocError> {
    fs.create_dir_all(dir)?;
    let path = dir.join(name);
    match fs.metadata(&path) {
        Ok(_) if fs.read(&path)? == text.as_bytes() => return Ok(path),
        Ok(_) => {
            fs.set_attributes(&path, Attributes(0))?;
            fs.delete_file(&path)?;
        }
        Err(FsError::NotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }
    fs.write_new(&path, text.as_bytes())?;
    fs.set_attributes(&path, Attributes(Attributes::READONLY))?;
    Ok(path)
}

/// Runs the check on a worker thread: the weekly one honours the schedule, "now" does not.
fn run_check(
    manual: bool,
    database: &Path,
    http: &dyn Http,
    now: i64,
) -> Result<Option<Release>, UpdateError> {
    // The package version is a semantic version by construction (Cargo rejects anything else).
    let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is semver");
    if manual {
        let release = latest_release(http, RELEASES_URL)?;
        return Ok((release.version > current).then_some(release));
    }
    let store = Store::open(database)?;
    Ok(
        match check_for_updates(&store, http, RELEASES_URL, &current, true, now)? {
            UpdateStatus::Available(release) => Some(release),
            UpdateStatus::Disabled | UpdateStatus::NotDue | UpdateStatus::UpToDate => None,
        },
    )
}

impl Controller {
    /// Replaces the network the update check uses (tests answer from a script or a loopback
    /// server instead of GitHub).
    pub fn set_update_http(&self, http: Arc<dyn Http>) {
        *self.help.http.borrow_mut() = http;
    }

    /// The newer release the user was told about, if any.
    #[must_use]
    pub fn available_update(&self) -> Option<Release> {
        self.help.offer.borrow().release.clone()
    }

    /// The weekly automatic check (spec §13): asks GitHub in the background unless the
    /// "check for updates" setting is off or the last check is less than a week old.
    pub fn start_update_check(&self) {
        if self.settings().general.check_for_updates {
            self.submit_update_check(false);
        }
    }

    /// Runs a Help command for window `cell`. Returns false for any other command.
    pub fn run_help_command(&self, cell: &Rc<WindowCell>, command: CommandId) -> bool {
        match command {
            CommandId::HelpWelcome => {
                let name = format!("{}.md", self.i18n.text("help-welcome-file"));
                let locale = self
                    .i18n
                    .loaded_locales()
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                self.open_help_document(cell, &name, welcome_text(&locale));
            }
            CommandId::HelpKeyboardShortcuts => {
                let name = format!("{}.md", self.i18n.text("help-shortcuts-file"));
                let text = shortcuts_markdown(&self.i18n, &self.keymap(), &DisplayKeyNames);
                self.open_help_document(cell, &name, &text);
            }
            CommandId::HelpReportIssue => {
                let _ = self.shell.borrow_mut().open_url(ISSUES_URL);
            }
            CommandId::HelpCheckForUpdates => self.submit_update_check(true),
            CommandId::HelpAbout => self.show_about(cell),
            _ => return false,
        }
        true
    }

    fn show_about(&self, cell: &Rc<WindowCell>) {
        let answer = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: self.i18n.text("help-about-title"),
            message: about_message(&self.i18n),
            buttons: MessageBoxButtons::YesNo,
            icon: Some(MessageBoxIcon::Information),
        });
        if matches!(answer, Ok(MessageBoxResult::Yes)) {
            let name = format!("{}.md", self.i18n.text("help-notices-file"));
            self.open_help_document(cell, &name, THIRD_PARTY_NOTICES);
        }
    }

    /// Writes a help document and opens it read-only in `cell`.
    fn open_help_document(&self, cell: &Rc<WindowCell>, name: &str, text: &str) {
        let dir = self.core().borrow().paths.local_dir().join("help");
        let path = match write_help_file(self.docs().env().fs.as_ref(), &dir, name, text) {
            Ok(path) => path,
            Err(error) => return self.report(&error),
        };
        if let Err(error) = self.open_file(cell, &path) {
            return self.report(&error);
        }
        // `open_file` made the page's tab the active one, so its editor is always there.
        cell.view
            .borrow_mut()
            .active_editor_mut()
            .into_iter()
            .for_each(|editor| editor.set_read_only(true));
        // Help pages are not the user's files: keep them out of the recent list (the open page
        // is recorded under its canonical path).
        let _ = self
            .docs()
            .env()
            .store
            .borrow()
            .recent_remove(RecentKind::File, &canonical_path(&path));
    }

    fn submit_update_check(&self, manual: bool) {
        let core = self.core().borrow();
        let database = core.paths.state_db();
        let http = Arc::clone(&self.help.http.borrow());
        let now = self.docs().env().clock.unix_seconds();
        let logger = Arc::clone(&core.logger);
        let me = self.handle();
        let generation = self.help.generation.get() + 1;
        self.help.generation.set(generation);
        let spec = JobSpec {
            kind: JobKind::Io,
            window: None,
            key: UPDATE_JOB_KEY,
            generation,
        };
        let _ = core.workers.submit(
            spec,
            move |_| run_check(manual, &database, http.as_ref(), now),
            move |outcome| {
                let result = match outcome {
                    Ok(found) => found.map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                if let Err(detail) = &result {
                    logger.log(
                        Level::Warn,
                        "update",
                        &format!("update check failed: {detail}"),
                    );
                }
                if let Some(controller) = me.upgrade() {
                    controller.finish_update_check(manual, result);
                }
            },
        );
    }

    fn finish_update_check(&self, manual: bool, result: Result<Option<Release>, String>) {
        match result {
            Ok(Some(release)) => {
                *self.help.offer.borrow_mut() = Offer {
                    release: Some(release),
                    dismissed: false,
                };
                // A banner of an older offer shows the wrong version: rebuild it.
                for cell in self.windows() {
                    cell.view
                        .borrow_mut()
                        .banners_mut()
                        .dismiss(BannerKind::Update);
                }
                self.refresh_all();
            }
            Ok(None) if manual => {
                let message = self.i18n.format(
                    "help-update-current-message",
                    &[("version", Arg::Str(env!("CARGO_PKG_VERSION")))],
                );
                self.notify(
                    "help-update-current-title",
                    message,
                    MessageBoxIcon::Information,
                );
            }
            Err(detail) if manual => {
                let message = self.i18n.format(
                    "help-update-failed-message",
                    &[("detail", Arg::Str(&detail))],
                );
                self.notify("help-update-failed-title", message, MessageBoxIcon::Warning);
            }
            Ok(None) | Err(_) => {}
        }
    }

    fn notify(&self, title_id: &str, message: String, icon: MessageBoxIcon) {
        let _ = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: self.i18n.text(title_id),
            message,
            buttons: MessageBoxButtons::Ok,
            icon: Some(icon),
        });
    }

    /// Shows the update banner in `view` while there is an offer the user did not close.
    pub(super) fn sync_update_banner(&self, view: &mut ShellView) {
        let offer = self.help.offer.borrow();
        let Some(release) = offer.release.as_ref().filter(|_| !offer.dismissed) else {
            return;
        };
        let banners = view.banners_mut();
        if banners.contains(BannerKind::Update) {
            return;
        }
        let message = self.i18n.format(
            "help-update-message",
            &[
                ("version", Arg::Str(&release.version.to_string())),
                ("current", Arg::Str(env!("CARGO_PKG_VERSION"))),
            ],
        );
        banners.show_update(
            &self.i18n.text("help-update-title"),
            &message,
            &self.i18n.text("help-update-download"),
        );
    }

    /// The Download button of the update banner: opens the release page.
    pub(super) fn open_update_page(&self) {
        let url = self
            .help
            .offer
            .borrow()
            .release
            .as_ref()
            .map(|release| release.page_url.clone());
        if let Some(url) = url {
            let _ = self.shell.borrow_mut().open_url(&url);
        }
    }

    /// The close button of the update banner: it stays closed for this run.
    pub(super) fn dismiss_update(&self) {
        self.help.offer.borrow_mut().dismissed = true;
    }
}

#[cfg(test)]
mod tests {
    use ssw_core::keymap::KeyChord;
    use ssw_io::fs::RealFs;

    use super::*;
    use crate::i18n::localizer_for;

    struct Odd;

    impl KeyNames for Odd {
        fn chord_text(&self, _chord: KeyChord) -> String {
            "a|b`c".to_owned()
        }
    }

    #[test]
    fn the_shortcut_document_lists_bound_commands_by_group() {
        let i18n = localizer_for("en");
        let text = shortcuts_markdown(&i18n, &Keymap::default(), &DisplayKeyNames);
        assert!(text.starts_with("# Keyboard shortcuts\n"), "{text}");
        assert!(text.contains("\n## File\n\n| Command | Shortcut |\n| --- | --- |\n"));
        assert!(text.contains("| Save | `Ctrl+S` |\n"), "{text}");
        assert!(text.contains("| Find next | `F3` |\n"), "{text}");
        // Commands without a chord and groups without any row are left out.
        assert!(!text.contains("## Help"));
        assert!(!text.contains("| Welcome document |"));
    }

    #[test]
    fn chord_names_survive_markdown_tables() {
        let i18n = localizer_for("en");
        let text = shortcuts_markdown(&i18n, &Keymap::default(), &Odd);
        assert!(text.contains("| Save | `` a\\|b`c `` |\n"), "{text}");
        assert_eq!(code_span("Ctrl+S"), "`Ctrl+S`");
    }

    #[test]
    fn the_about_text_names_the_version_and_the_license() {
        let text = about_message(&localizer_for("en"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
        assert!(text.contains("MIT"), "{text}");
        assert!(!THIRD_PARTY_NOTICES.is_empty());
    }

    #[test]
    fn the_weekly_check_needs_the_state_database_and_does_not_ask_the_network_without_it() {
        let tmp = tempfile::tempdir().unwrap();
        // The database's folder is a file: the store cannot be opened (the schedule lives in it).
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let error = run_check(false, &blocker.join("state.db"), &WinHttp, 0).unwrap_err();
        // The failure of the store, not of the network.
        let shown = format!("{error:?}");
        assert!(shown.starts_with("Store("), "{shown}");
    }

    #[test]
    fn help_files_are_read_only_and_rewritten_only_when_the_text_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("help");
        let path = write_help_file(&RealFs, &dir, "Keys.md", "one\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");
        assert!(RealFs.metadata(&path).unwrap().attrs.is_readonly());

        let again = write_help_file(&RealFs, &dir, "Keys.md", "one\n").unwrap();
        assert_eq!(again, path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");

        write_help_file(&RealFs, &dir, "Keys.md", "two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two\n");
        assert!(RealFs.metadata(&path).unwrap().attrs.is_readonly());
    }
}
