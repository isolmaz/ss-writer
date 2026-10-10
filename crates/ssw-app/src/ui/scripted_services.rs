//! The operating-system seams of the scripted harness: file dialogs, message boxes and the
//! shell.
//!
//! The harness (`ui::test_script`) runs the real [`super::controller::Controller`]; only what
//! would put a window on the user's screen is replaced. [`ScriptedServices`] hands the
//! controller a [`Dialogs`] and a [`Shell`] that never show anything: every request is recorded
//! and answered from the queue the script fills (`answer`, `save-as`). A dialog nobody answered
//! is recorded as *unanswered* and answered with "cancel"; the script runner turns it into a
//! failure so an unexpected message box can never pass unnoticed.
//!
//! The clipboard needs no double of its own: [`ssw_ui::services::MemoryClipboard`] is a
//! complete clipboard held in memory.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ssw_ui::services::{
    Dialogs, MessageBoxOptions, MessageBoxResult, OpenFileOptions, PickFolderOptions,
    SaveFileOptions, ServiceError, Shell,
};

/// The kinds of dialog a script can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    /// The open-file dialog.
    OpenFile,
    /// The save-file dialog.
    SaveFile,
    /// The folder picker.
    PickFolder,
    /// A message box.
    MessageBox,
}

impl DialogKind {
    /// Every kind.
    pub const ALL: [DialogKind; 4] = [
        DialogKind::OpenFile,
        DialogKind::SaveFile,
        DialogKind::PickFolder,
        DialogKind::MessageBox,
    ];

    /// The name scripts use (`answer <name> <choice>`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            DialogKind::OpenFile => "open-file",
            DialogKind::SaveFile => "save-file",
            DialogKind::PickFolder => "pick-folder",
            DialogKind::MessageBox => "message-box",
        }
    }

    /// The kind a script name stands for.
    #[must_use]
    pub fn from_name(name: &str) -> Option<DialogKind> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// How the scripted user answers one dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The user closes the dialog without choosing.
    Cancel,
    /// The user picks these files, this save target or this folder.
    Paths(Vec<PathBuf>),
    /// The user presses this button of a message box.
    Button(MessageBoxResult),
}

/// A dialog the app showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogRecord {
    /// Which dialog.
    pub kind: DialogKind,
    /// Its title.
    pub title: String,
    /// The text of a message box; the suggested file name of a save dialog; empty otherwise.
    pub message: String,
    /// Whether the script had an answer ready; an unanswered dialog was closed with "cancel".
    pub answered: bool,
}

/// What the app asked the shell to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellRecord {
    /// Open a URL in the browser.
    OpenUrl(String),
    /// Open a file or folder with its default handler.
    OpenPath(PathBuf),
    /// Show an item in Explorer.
    Reveal(PathBuf),
    /// Move an item to the Recycle Bin (the item is removed from disk).
    Recycle(PathBuf),
}

#[derive(Debug, Default)]
struct State {
    answers: VecDeque<(DialogKind, Answer)>,
    dialogs: Vec<DialogRecord>,
    shell: Vec<ShellRecord>,
    /// `dialogs[..reported]` were already handed out by `take_unexpected`.
    reported: usize,
    /// Print every request to stderr (the process-level instance, where nobody reads the log).
    echo: bool,
}

/// The shared state behind the scripted [`Dialogs`] and [`Shell`]: a cheap handle, clones see
/// the same queue and log.
#[derive(Debug, Clone, Default)]
pub struct ScriptedServices {
    state: Rc<RefCell<State>>,
}

impl ScriptedServices {
    /// Services with nothing queued and nothing recorded.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Services that also print every request to stderr.
    #[must_use]
    pub fn echoing() -> Self {
        let services = Self::default();
        services.state.borrow_mut().echo = true;
        services
    }

    /// The dialogs, for [`super::controller::Controller::new`].
    #[must_use]
    pub fn dialogs(&self) -> Box<dyn Dialogs> {
        Box::new(ScriptedDialogs {
            state: Rc::clone(&self.state),
        })
    }

    /// The shell, for [`super::controller::Controller::new`].
    #[must_use]
    pub fn shell(&self) -> Box<dyn Shell> {
        Box::new(ScriptedShell {
            state: Rc::clone(&self.state),
        })
    }

    /// Queues the answer to the next dialog of `kind`.
    pub fn answer(&self, kind: DialogKind, answer: Answer) {
        self.state.borrow_mut().answers.push_back((kind, answer));
    }

    /// The answers no dialog has consumed yet.
    #[must_use]
    pub fn unused_answers(&self) -> Vec<(DialogKind, Answer)> {
        self.state.borrow().answers.iter().cloned().collect()
    }

    /// Every dialog shown so far.
    #[must_use]
    pub fn dialog_log(&self) -> Vec<DialogRecord> {
        self.state.borrow().dialogs.clone()
    }

    /// Every shell request so far.
    #[must_use]
    pub fn shell_log(&self) -> Vec<ShellRecord> {
        self.state.borrow().shell.clone()
    }

    /// The first dialog shown without an answer since the last call, if any.
    pub fn take_unexpected(&self) -> Option<DialogRecord> {
        let mut state = self.state.borrow_mut();
        let from = state.reported;
        state.reported = state.dialogs.len();
        state.dialogs[from..]
            .iter()
            .find(|record| !record.answered)
            .cloned()
    }
}

impl fmt::Display for DialogRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} \"{}\"", self.kind.name(), self.title)?;
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        Ok(())
    }
}

/// The scripted [`Dialogs`].
#[derive(Debug)]
struct ScriptedDialogs {
    state: Rc<RefCell<State>>,
}

impl ScriptedDialogs {
    /// Records the dialog and takes its answer from the queue.
    fn ask(&self, kind: DialogKind, title: &str, message: &str) -> Answer {
        let mut state = self.state.borrow_mut();
        let waiting = state.answers.iter().position(|(queued, _)| *queued == kind);
        let answer = waiting.and_then(|index| state.answers.remove(index));
        let record = DialogRecord {
            kind,
            title: title.to_owned(),
            message: message.to_owned(),
            answered: answer.is_some(),
        };
        if state.echo {
            eprintln!("scripted dialog: {record}");
        }
        state.dialogs.push(record);
        answer.map_or(Answer::Cancel, |(_, answer)| answer)
    }
}

/// The extension the real dialog appends when the user typed none.
fn with_default_extension(path: PathBuf, extension: Option<&str>) -> PathBuf {
    match extension {
        Some(extension) if path.extension().is_none() => path.with_extension(extension),
        _ => path,
    }
}

impl Dialogs for ScriptedDialogs {
    fn open_file(
        &mut self,
        options: &OpenFileOptions,
    ) -> Result<Option<Vec<PathBuf>>, ServiceError> {
        Ok(match self.ask(DialogKind::OpenFile, &options.title, "") {
            Answer::Paths(paths) => Some(paths),
            Answer::Cancel | Answer::Button(_) => None,
        })
    }

    fn save_file(&mut self, options: &SaveFileOptions) -> Result<Option<PathBuf>, ServiceError> {
        let suggested = options.suggested_name.as_deref().unwrap_or_default();
        Ok(
            match self.ask(DialogKind::SaveFile, &options.title, suggested) {
                Answer::Paths(paths) => paths
                    .into_iter()
                    .next()
                    .map(|path| with_default_extension(path, options.default_extension.as_deref())),
                Answer::Cancel | Answer::Button(_) => None,
            },
        )
    }

    fn pick_folder(
        &mut self,
        options: &PickFolderOptions,
    ) -> Result<Option<PathBuf>, ServiceError> {
        Ok(match self.ask(DialogKind::PickFolder, &options.title, "") {
            Answer::Paths(paths) => paths.into_iter().next(),
            Answer::Cancel | Answer::Button(_) => None,
        })
    }

    fn message_box(
        &mut self,
        options: &MessageBoxOptions,
    ) -> Result<MessageBoxResult, ServiceError> {
        Ok(
            match self.ask(DialogKind::MessageBox, &options.title, &options.message) {
                Answer::Button(button) => button,
                Answer::Cancel | Answer::Paths(_) => MessageBoxResult::Cancel,
            },
        )
    }
}

/// The scripted [`Shell`].
#[derive(Debug)]
struct ScriptedShell {
    state: Rc<RefCell<State>>,
}

impl ScriptedShell {
    fn record(&self, record: ShellRecord) {
        let mut state = self.state.borrow_mut();
        if state.echo {
            eprintln!("scripted shell: {record:?}");
        }
        state.shell.push(record);
    }
}

impl Shell for ScriptedShell {
    fn open_url(&mut self, url: &str) -> Result<(), ServiceError> {
        self.record(ShellRecord::OpenUrl(url.to_owned()));
        Ok(())
    }

    fn open_path(&mut self, path: &Path) -> Result<(), ServiceError> {
        self.record(ShellRecord::OpenPath(path.to_path_buf()));
        Ok(())
    }

    fn reveal_in_explorer(&mut self, path: &Path) -> Result<(), ServiceError> {
        self.record(ShellRecord::Reveal(path.to_path_buf()));
        Ok(())
    }

    fn recycle(&mut self, path: &Path) -> Result<(), ServiceError> {
        self.record(ShellRecord::Recycle(path.to_path_buf()));
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        removed.map_err(|error| ServiceError::Failed {
            service: "recycle bin",
            message: error.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use ssw_ui::services::MessageBoxButtons;

    use super::*;

    fn message(title: &str, text: &str) -> MessageBoxOptions {
        MessageBoxOptions {
            title: title.to_owned(),
            message: text.to_owned(),
            buttons: MessageBoxButtons::SaveDontSaveCancel,
            icon: None,
        }
    }

    #[test]
    fn kinds_round_trip_through_their_script_names() {
        for kind in DialogKind::ALL {
            assert_eq!(DialogKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(DialogKind::from_name("popup"), None);
    }

    #[test]
    fn answers_are_consumed_in_order_per_kind() {
        let services = ScriptedServices::new();
        let mut dialogs = services.dialogs();
        services.answer(
            DialogKind::MessageBox,
            Answer::Button(MessageBoxResult::Save),
        );
        services.answer(
            DialogKind::OpenFile,
            Answer::Paths(vec![PathBuf::from("a.md"), PathBuf::from("b.md")]),
        );
        services.answer(
            DialogKind::MessageBox,
            Answer::Button(MessageBoxResult::DontSave),
        );
        assert_eq!(
            dialogs.open_file(&OpenFileOptions::default()),
            Ok(Some(vec![PathBuf::from("a.md"), PathBuf::from("b.md")]))
        );
        assert_eq!(
            dialogs.message_box(&message("t", "first")),
            Ok(MessageBoxResult::Save)
        );
        assert_eq!(
            dialogs.message_box(&message("t", "second")),
            Ok(MessageBoxResult::DontSave)
        );
        assert!(services.unused_answers().is_empty());
        assert_eq!(services.take_unexpected(), None);
        let log = services.dialog_log();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].kind, DialogKind::OpenFile);
        assert_eq!(log[1].message, "first");
        assert!(log.iter().all(|record| record.answered));
    }

    #[test]
    fn an_unanswered_dialog_is_cancelled_and_reported_once() {
        let services = ScriptedServices::new();
        let mut dialogs = services.dialogs();
        services.answer(DialogKind::SaveFile, Answer::Cancel);
        assert_eq!(dialogs.open_file(&OpenFileOptions::default()), Ok(None));
        assert_eq!(dialogs.pick_folder(&PickFolderOptions::default()), Ok(None));
        assert_eq!(
            dialogs.message_box(&message("Unsaved", "Save changes?")),
            Ok(MessageBoxResult::Cancel)
        );
        let unexpected = services.take_unexpected().unwrap();
        assert_eq!(unexpected.kind, DialogKind::OpenFile);
        assert!(!unexpected.answered);
        assert_eq!(unexpected.to_string(), "open-file \"\"");
        assert_eq!(
            services.unused_answers(),
            vec![(DialogKind::SaveFile, Answer::Cancel)]
        );
        // Handed out once: nothing new happened since.
        assert_eq!(services.take_unexpected(), None);
        assert_eq!(
            dialogs.message_box(&message("Again", "Again?")).unwrap(),
            MessageBoxResult::Cancel
        );
        let again = services.take_unexpected().unwrap();
        assert_eq!(again.to_string(), "message-box \"Again\": Again?");
    }

    #[test]
    fn a_save_target_without_extension_gets_the_default_one() {
        let services = ScriptedServices::new();
        let mut dialogs = services.dialogs();
        let options = SaveFileOptions {
            title: "Save as".to_owned(),
            suggested_name: Some("Untitled.md".to_owned()),
            default_extension: Some("md".to_owned()),
            ..SaveFileOptions::default()
        };
        services.answer(
            DialogKind::SaveFile,
            Answer::Paths(vec![PathBuf::from("notes")]),
        );
        assert_eq!(
            dialogs.save_file(&options),
            Ok(Some(PathBuf::from("notes.md")))
        );
        services.answer(
            DialogKind::SaveFile,
            Answer::Paths(vec![PathBuf::from("notes.txt")]),
        );
        assert_eq!(
            dialogs.save_file(&options),
            Ok(Some(PathBuf::from("notes.txt")))
        );
        services.answer(
            DialogKind::SaveFile,
            Answer::Paths(vec![PathBuf::from("raw")]),
        );
        assert_eq!(
            dialogs.save_file(&SaveFileOptions::default()),
            Ok(Some(PathBuf::from("raw")))
        );
        services.answer(DialogKind::SaveFile, Answer::Cancel);
        assert_eq!(dialogs.save_file(&options), Ok(None));
        let log = services.dialog_log();
        assert_eq!(log[0].message, "Untitled.md");
        assert_eq!(log[2].message, "");
    }

    #[test]
    fn a_folder_answer_picks_the_first_path() {
        let services = ScriptedServices::new();
        let mut dialogs = services.dialogs();
        services.answer(
            DialogKind::PickFolder,
            Answer::Paths(vec![PathBuf::from("one"), PathBuf::from("two")]),
        );
        assert_eq!(
            dialogs.pick_folder(&PickFolderOptions::default()),
            Ok(Some(PathBuf::from("one")))
        );
    }

    #[test]
    fn the_shell_records_requests_and_the_recycle_bin_removes_the_item() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("gone.md");
        std::fs::write(&file, "x").unwrap();
        let folder = tmp.path().join("folder");
        std::fs::create_dir_all(folder.join("inner")).unwrap();
        let services = ScriptedServices::echoing();
        let mut shell = services.shell();
        assert_eq!(shell.open_url("https://example.com/"), Ok(()));
        assert_eq!(shell.open_path(&file), Ok(()));
        assert_eq!(shell.reveal_in_explorer(&file), Ok(()));
        assert_eq!(shell.recycle(&file), Ok(()));
        assert_eq!(shell.recycle(&folder), Ok(()));
        assert!(!file.exists());
        assert!(!folder.exists());
        assert!(matches!(
            shell.recycle(&file),
            Err(ServiceError::Failed {
                service: "recycle bin",
                ..
            })
        ));
        assert_eq!(
            services.shell_log(),
            vec![
                ShellRecord::OpenUrl("https://example.com/".to_owned()),
                ShellRecord::OpenPath(file.clone()),
                ShellRecord::Reveal(file.clone()),
                ShellRecord::Recycle(file.clone()),
                ShellRecord::Recycle(folder),
                ShellRecord::Recycle(file),
            ]
        );
    }

    #[test]
    fn echoing_services_still_answer_and_record() {
        let services = ScriptedServices::echoing();
        let mut dialogs = services.dialogs();
        services.answer(DialogKind::MessageBox, Answer::Button(MessageBoxResult::Ok));
        assert_eq!(
            dialogs.message_box(&message("Error", "Boom")),
            Ok(MessageBoxResult::Ok)
        );
        assert_eq!(services.dialog_log().len(), 1);
    }
}
