//! Command dispatch: every [`CommandId`] has exactly one handler category. The app core runs
//! what it can without a UI (undo, redo, select all, the save family); everything that needs a
//! window, a dialog or the focused editor goes through [`AppActions`], implemented by the
//! visible shell.

use std::path::PathBuf;
use std::rc::Rc;

use ssw_core::command::CommandId;
use ssw_core::edit::Selection;

use crate::documents::{DocEntry, DocError, Documents, Source};

/// Who handles a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandCategory {
    /// Changes or reads the text of the active document (edit, format, table commands).
    Document,
    /// Changes what the window shows (panels, find bar, modes, zoom, folding, panes).
    View,
    /// Files, tabs, windows, export, sidebar tree, settings, help.
    App,
}

/// The handler category of `command`. The match has no wildcard: adding a command to
/// `ssw-core` fails to compile here until it is categorized.
#[must_use]
pub fn category(command: CommandId) -> CommandCategory {
    use CommandId as C;
    match command {
        C::EditUndo
        | C::EditRedo
        | C::EditCut
        | C::EditCopy
        | C::EditPaste
        | C::EditPastePlain
        | C::EditSelectAll
        | C::EditCopyAsPlainText
        | C::EditCopyAsMarkdown
        | C::EditCopyAsRichText
        | C::EditCopyAsHtml
        | C::EditInsertDate
        | C::FormatBold
        | C::FormatItalic
        | C::FormatUnderline
        | C::FormatStrikethrough
        | C::FormatHighlight
        | C::FormatHighlightRed
        | C::FormatHighlightOrange
        | C::FormatHighlightYellow
        | C::FormatHighlightGreen
        | C::FormatHighlightBlue
        | C::FormatHighlightPurple
        | C::FormatInlineCode
        | C::FormatCodeBlock
        | C::FormatLink
        | C::FormatImage
        | C::FormatTable
        | C::FormatQuote
        | C::FormatCallout
        | C::FormatMath
        | C::FormatMathBlock
        | C::FormatDivider
        | C::FormatFootnote
        | C::FormatPageBreak
        | C::FormatHeading1
        | C::FormatHeading2
        | C::FormatHeading3
        | C::FormatHeading4
        | C::FormatHeading5
        | C::FormatHeading6
        | C::FormatParagraph
        | C::FormatBulletList
        | C::FormatNumberedList
        | C::FormatTaskList
        | C::FormatTaskToggle
        | C::TableInsertRowAbove
        | C::TableInsertRowBelow
        | C::TableInsertColumnLeft
        | C::TableInsertColumnRight
        | C::TableDeleteRow
        | C::TableDeleteColumn
        | C::TableAlignLeft
        | C::TableAlignCenter
        | C::TableAlignRight
        | C::TableCopyAsMarkdown
        | C::TableCopyAsCsv
        | C::TableCopyAsHtml
        | C::TableEditRaw => CommandCategory::Document,
        C::SearchFind
        | C::SearchReplace
        | C::SearchGoToLine
        | C::SearchFindNext
        | C::SearchFindPrevious
        | C::SearchQuickOpen
        | C::SearchCommandPalette
        | C::SearchInFolder
        | C::ViewToggleSidebar
        | C::ViewToggleInspector
        | C::ViewToggleStatusBar
        | C::ViewToggleFormatPill
        | C::ViewTypewriterMode
        | C::ViewFocusMode
        | C::ViewFullScreen
        | C::ViewZenMode
        | C::ViewZoomIn
        | C::ViewZoomOut
        | C::ViewZoomReset
        | C::ViewRecenterCaret
        | C::ViewFoldAll
        | C::ViewUnfoldAll
        | C::ViewNextPane => CommandCategory::View,
        C::FileNew
        | C::FileNewWindow
        | C::FileOpen
        | C::FileOpenFolder
        | C::FileSave
        | C::FileSaveAs
        | C::FileSaveBackToWord
        | C::FileVersionHistory
        | C::FileExport
        | C::FilePrint
        | C::FileShare
        | C::FileExit
        | C::TabNew
        | C::TabClose
        | C::TabCloseOthers
        | C::TabCloseToRight
        | C::TabReopenClosed
        | C::TabNext
        | C::TabPrevious
        | C::TabCopyPath
        | C::TabRevealInExplorer
        | C::WindowMergeAll
        | C::ExportPdf
        | C::ExportPng
        | C::ExportJpg
        | C::ExportHtml
        | C::ExportEpub
        | C::ExportDocx
        | C::ExportOdt
        | C::ExportRtf
        | C::ExportTxt
        | C::ExportMarkdownWithAttachments
        | C::ExportTextBundle
        | C::SidebarOpen
        | C::SidebarOpenInNewTab
        | C::SidebarOpenInNewWindow
        | C::SidebarRename
        | C::SidebarDuplicate
        | C::SidebarDelete
        | C::SidebarRevealInExplorer
        | C::SidebarCopyPath
        | C::SidebarCopyRelativePath
        | C::SidebarNewFile
        | C::SidebarNewFolder
        | C::SidebarCollapseAll
        | C::SidebarExpandAll
        | C::SettingsOpen
        | C::SettingsOpenLogsFolder
        | C::SettingsReset
        | C::HelpWelcome
        | C::HelpKeyboardShortcuts
        | C::HelpReportIssue
        | C::HelpCheckForUpdates
        | C::HelpAbout => CommandCategory::App,
    }
}

/// What the visible shell provides. The shell owns windows, tabs, dialogs and the editor view.
pub trait AppActions {
    /// The document of the active tab of the active window.
    fn active_document(&self) -> Option<Rc<DocEntry>>;

    /// Shows the "Save as" dialog for `entry`; `None` when the user cancels.
    fn choose_save_path(&mut self, entry: &DocEntry) -> Option<PathBuf>;

    /// Shows a failure of a core operation (banner or message box).
    fn report_error(&mut self, error: &DocError);

    /// Runs a [`CommandCategory::Document`] command in the focused editor (formatting, table
    /// operations, clipboard). Returns whether the editor handled it.
    fn run_in_editor(&mut self, command: CommandId) -> bool;

    /// Runs a [`CommandCategory::View`] command. Returns whether it was handled.
    fn run_view(&mut self, command: CommandId) -> bool;

    /// Runs a [`CommandCategory::App`] command the core does not handle itself (windows, tabs,
    /// dialogs, export, settings, help). Returns whether it was handled.
    fn run_app(&mut self, command: CommandId) -> bool;
}

/// How a dispatched command ended.
#[derive(Debug)]
pub enum Dispatch {
    /// Some handler ran the command.
    Handled,
    /// Nobody handled it (no active document, or the shell declined it).
    Unhandled,
    /// The core handled it and the operation failed; the error was also reported through
    /// [`AppActions::report_error`].
    Failed(DocError),
}

/// Routes commands to the core or the shell.
#[derive(Debug)]
pub struct CommandDispatcher {
    docs: Rc<Documents>,
}

impl CommandDispatcher {
    /// A dispatcher over the document registry.
    #[must_use]
    pub fn new(docs: Rc<Documents>) -> Self {
        Self { docs }
    }

    /// Runs `command`.
    pub fn dispatch(&self, command: CommandId, actions: &mut dyn AppActions) -> Dispatch {
        let handled = match category(command) {
            CommandCategory::Document => {
                self.document_command(command, actions) || actions.run_in_editor(command)
            }
            CommandCategory::View => actions.run_view(command),
            CommandCategory::App => match self.file_command(command, actions) {
                Some(Ok(())) => true,
                Some(Err(error)) => {
                    actions.report_error(&error);
                    return Dispatch::Failed(error);
                }
                None => actions.run_app(command),
            },
        };
        if handled {
            Dispatch::Handled
        } else {
            Dispatch::Unhandled
        }
    }

    /// Commands the core runs on the active document without a UI.
    fn document_command(&self, command: CommandId, actions: &dyn AppActions) -> bool {
        let Some(entry) = actions.active_document() else {
            return false;
        };
        let mut document = entry.document().borrow_mut();
        match command {
            CommandId::EditUndo => document.undo().is_ok(),
            CommandId::EditRedo => document.redo().is_ok(),
            CommandId::EditSelectAll => {
                let end = document.buffer().len();
                document.set_selections(Selection::new(0, end), &[]);
                true
            }
            _ => false,
        }
    }

    /// The save family, run against the active document. `None` for other commands.
    fn file_command(
        &self,
        command: CommandId,
        actions: &mut dyn AppActions,
    ) -> Option<Result<(), DocError>> {
        if !matches!(
            command,
            CommandId::FileSave | CommandId::FileSaveAs | CommandId::FileSaveBackToWord
        ) {
            return None;
        }
        let entry = actions.active_document()?;
        let needs_target = command == CommandId::FileSaveAs
            || entry.is_untitled()
            || (command == CommandId::FileSave && matches!(entry.source(), Source::Converted(_)));
        let result = match command {
            CommandId::FileSaveBackToWord => self.docs.save_back(&entry).map(|_| ()),
            _ if needs_target => match actions.choose_save_path(&entry) {
                Some(path) => self.docs.save_as(&entry, &path).map(|_| ()),
                // The user cancelled the dialog.
                None => Ok(()),
            },
            _ => self.docs.save(&entry).map(|_| ()),
        };
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Arc;

    use ssw_core::command::CommandGroup;
    use ssw_core::edit::{Edit, Transaction};
    use ssw_io::fs::RealFs;
    use ssw_io::recovery::RecoveryStore;
    use ssw_io::save::ThreadSleeper;
    use ssw_io::store::Store;

    use super::*;
    use crate::clock::{AppClock, ManualClock};
    use crate::documents::{DocEnv, SharedStore};

    struct Recording {
        document: Option<Rc<DocEntry>>,
        save_path: Option<PathBuf>,
        seen: Vec<(CommandCategory, CommandId)>,
        errors: usize,
        accept: bool,
    }

    impl AppActions for Recording {
        fn active_document(&self) -> Option<Rc<DocEntry>> {
            self.document.clone()
        }

        fn choose_save_path(&mut self, _entry: &DocEntry) -> Option<PathBuf> {
            self.save_path.clone()
        }

        fn report_error(&mut self, _error: &DocError) {
            self.errors += 1;
        }

        fn run_in_editor(&mut self, command: CommandId) -> bool {
            self.seen.push((CommandCategory::Document, command));
            self.accept
        }

        fn run_view(&mut self, command: CommandId) -> bool {
            self.seen.push((CommandCategory::View, command));
            self.accept
        }

        fn run_app(&mut self, command: CommandId) -> bool {
            self.seen.push((CommandCategory::App, command));
            self.accept
        }
    }

    fn rig(tmp: &std::path::Path) -> (Rc<Documents>, CommandDispatcher) {
        let store: SharedStore = Rc::new(RefCell::new(Store::open(&tmp.join("state.db")).unwrap()));
        let clock: Rc<dyn AppClock> = Rc::new(ManualClock::new(1));
        let docs = Rc::new(Documents::new(DocEnv {
            fs: Arc::new(RealFs),
            sleeper: Arc::new(ThreadSleeper),
            clock,
            store,
            recovery: Arc::new(RecoveryStore::new(Arc::new(RealFs), tmp.join("recovery"))),
            tld: None,
        }));
        (Rc::clone(&docs), CommandDispatcher::new(docs))
    }

    fn type_text(entry: &DocEntry, text: &str) {
        let mut document = entry.document().borrow_mut();
        let end = document.buffer().len();
        let tx = Transaction::single(
            Edit::insert(end, text),
            Selection::caret(end),
            Selection::caret(end + text.len()),
        );
        document.apply(tx).unwrap();
    }

    #[test]
    fn categories_follow_the_command_groups() {
        for command in CommandId::all() {
            let expected = match command.group() {
                CommandGroup::Edit | CommandGroup::Format | CommandGroup::Table => {
                    CommandCategory::Document
                }
                CommandGroup::Search | CommandGroup::View => CommandCategory::View,
                CommandGroup::File
                | CommandGroup::Tab
                | CommandGroup::Window
                | CommandGroup::Export
                | CommandGroup::Sidebar
                | CommandGroup::Settings
                | CommandGroup::Help => CommandCategory::App,
            };
            assert_eq!(category(command), expected, "{command}");
        }
    }

    #[test]
    fn every_command_reaches_exactly_one_handler() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, dispatcher) = rig(tmp.path());
        let entry = docs.new_untitled();
        type_text(&entry, "text");
        let mut actions = Recording {
            document: Some(Rc::clone(&entry)),
            save_path: Some(tmp.path().join("saved.md")),
            seen: Vec::new(),
            errors: 0,
            accept: true,
        };
        let core_handled = [
            CommandId::EditUndo,
            CommandId::EditRedo,
            CommandId::EditSelectAll,
            CommandId::FileSave,
            CommandId::FileSaveAs,
            CommandId::FileSaveBackToWord,
        ];
        for command in CommandId::all() {
            let outcome = dispatcher.dispatch(command, &mut actions);
            if command == CommandId::FileSaveBackToWord {
                assert!(
                    matches!(outcome, Dispatch::Failed(DocError::Unsupported(_))),
                    "{outcome:?}"
                );
            } else {
                assert!(
                    matches!(outcome, Dispatch::Handled),
                    "{command}: {outcome:?}"
                );
            }
        }
        assert_eq!(actions.errors, 1);
        let forwarded: Vec<CommandId> = actions.seen.iter().map(|(_, command)| *command).collect();
        let expected: Vec<CommandId> = CommandId::all()
            .filter(|command| !core_handled.contains(command))
            .collect();
        assert_eq!(forwarded, expected);
        assert!(
            actions
                .seen
                .iter()
                .all(|(handler, command)| *handler == category(*command))
        );
        assert_eq!(std::fs::read(tmp.path().join("saved.md")).unwrap(), b"text");
    }

    #[test]
    fn a_shell_that_declines_leaves_the_command_unhandled() {
        let tmp = tempfile::tempdir().unwrap();
        let (_docs, dispatcher) = rig(tmp.path());
        let mut actions = Recording {
            document: None,
            save_path: None,
            seen: Vec::new(),
            errors: 0,
            accept: false,
        };
        for command in [
            CommandId::FormatBold,
            CommandId::ViewZenMode,
            CommandId::FileSave,
            CommandId::EditUndo,
        ] {
            assert!(matches!(
                dispatcher.dispatch(command, &mut actions),
                Dispatch::Unhandled
            ));
        }
        assert_eq!(
            actions.seen.len(),
            4,
            "without a document even the core commands go to the shell"
        );
    }

    #[test]
    fn undo_redo_and_select_all_run_on_the_active_document() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, dispatcher) = rig(tmp.path());
        let entry = docs.new_untitled();
        type_text(&entry, "hello");
        let mut actions = Recording {
            document: Some(Rc::clone(&entry)),
            save_path: None,
            seen: Vec::new(),
            errors: 0,
            accept: false,
        };
        assert!(matches!(
            dispatcher.dispatch(CommandId::EditUndo, &mut actions),
            Dispatch::Handled
        ));
        assert_eq!(entry.document().borrow().text(), "");
        assert!(matches!(
            dispatcher.dispatch(CommandId::EditRedo, &mut actions),
            Dispatch::Handled
        ));
        assert_eq!(entry.document().borrow().text(), "hello");
        assert!(matches!(
            dispatcher.dispatch(CommandId::EditSelectAll, &mut actions),
            Dispatch::Handled
        ));
        assert_eq!(
            entry.document().borrow().primary_selection(),
            Selection::new(0, 5)
        );
        // Cancelling the Save as dialog is not an error.
        assert!(matches!(
            dispatcher.dispatch(CommandId::FileSaveAs, &mut actions),
            Dispatch::Handled
        ));
        assert!(entry.is_untitled());
        assert!(actions.seen.is_empty() && actions.errors == 0);
    }
}
