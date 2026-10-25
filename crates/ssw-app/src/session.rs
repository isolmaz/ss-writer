//! Session restore and the reopen-closed-tab stack (spec §6): which windows and tabs exist at
//! startup, the persistence of that structure through [`ssw_io::session`], per-file caret,
//! scroll and folds, and the first-run workspace.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ssw_core::settings::StartupBehavior;
use ssw_io::fs::FileSystem;
use ssw_io::session::{
    Session, SessionTab, SessionWindow, TabDocument, ViewState, WindowPlacement,
};
use ssw_io::store::{Store, StoreError, path_key};
use ssw_io::workspace::ensure_default_folder;
use ssw_ui::editor::highlight::Language;

use crate::args::Launch;
use crate::documents::{DocEntry, DocError, Documents, SharedStore, canonical_path};

/// How many closed tabs "Reopen closed tab" remembers.
pub const CLOSED_TAB_LIMIT: usize = 20;

/// Key in the state database that remembers the first-run workspace was created.
const FIRST_RUN_KEY: &str = "first_run_done";

/// Start of the key in the state database under which the language the user chose for a file is
/// remembered; the rest is the file's case-insensitive path key.
const LANGUAGE_KEY_PREFIX: &str = "plain_language.";

/// The stored value of "Plain text" (no language: the document is not coloured).
const PLAIN_TEXT_VALUE: &str = "plain";

/// The key of the language remembered for `path`.
fn language_key(path: &Path) -> Result<String, StoreError> {
    Ok(format!("{LANGUAGE_KEY_PREFIX}{}", path_key(path)?))
}

/// Forgets the language chosen for the file `path`: its row is deleted.
fn forget_language(store: &Store, path: &Path) -> Result<(), StoreError> {
    store.kv_delete(&language_key(path)?)
}

/// The language the user chose for the file `path`: `None` when none was chosen (or the stored
/// value is unreadable), `Some(None)` for "Plain text", `Some(Some(language))` otherwise. A stored
/// choice beats the language that is detected from the file.
pub(crate) fn stored_language(store: &Store, path: &Path) -> Option<Option<Language>> {
    let value = store.kv_get(&language_key(path).ok()?).ok()??;
    if value == PLAIN_TEXT_VALUE {
        Some(None)
    } else {
        Language::from_name(&value).map(Some)
    }
}

/// Remembers `language` (`None`: "Plain text") as the choice for the file `path`.
fn store_language(
    store: &Store,
    path: &Path,
    language: Option<Language>,
) -> Result<(), StoreError> {
    store.kv_set(
        &language_key(path)?,
        language.map_or(PLAIN_TEXT_VALUE, Language::name),
    )
}

/// Makes the row of the file `path` say what an open document's `choice` says (as
/// [`DocEntry::language_choice`]): a choice is remembered, and "Automatic" (`None`) leaves no row.
/// What the file had stored before is replaced either way, so a file that takes the place of
/// another one never inherits that one's choice.
pub(crate) fn remember_language(
    store: &Store,
    path: &Path,
    choice: Option<Option<Language>>,
) -> Result<(), StoreError> {
    match choice {
        Some(language) => store_language(store, path, language),
        None => forget_language(store, path),
    }
}

/// Makes the stored choice follow the file from `from` to `to` (a rename or a move) for the open
/// document whose choice is `choice`: the row of `from` is deleted, so nothing is left behind for
/// an unrelated file that is created there later, and `to` gets the row (see
/// [`remember_language`]). The row of `from` goes first: when only the case of the name changed
/// the two paths share one key, and deleting last would delete the row just written.
pub(crate) fn move_language(
    store: &Store,
    from: &Path,
    to: &Path,
    choice: Option<Option<Language>>,
) -> Result<(), StoreError> {
    forget_language(store, from)?;
    remember_language(store, to, choice)
}

/// A document a planned tab shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlannedDocument {
    /// A file to open.
    File(PathBuf),
    /// An untitled buffer of an earlier session; its text is in the state database.
    RestoredUntitled(String),
    /// A new empty untitled buffer.
    NewUntitled,
}

/// A tab of a planned window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedTab {
    /// What the tab shows.
    pub document: PlannedDocument,
    /// The tab's workspace root.
    pub workspace: Option<PathBuf>,
}

/// A window to create at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedWindow {
    /// Where the window was in the stored session; `None` for a new window.
    pub placement: Option<WindowPlacement>,
    /// Tabs, left to right.
    pub tabs: Vec<PlannedTab>,
    /// Index of the active tab.
    pub active_tab: Option<usize>,
}

/// The windows to create at startup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartupPlan {
    /// Windows in creation order.
    pub windows: Vec<PlannedWindow>,
    /// The window that gets the focus.
    pub active_window: Option<usize>,
}

/// A new window showing `tabs` (never empty: every caller has at least one tab), the first one
/// active.
fn new_window(tabs: Vec<PlannedTab>) -> PlannedWindow {
    PlannedWindow {
        placement: None,
        active_tab: Some(0),
        tabs,
    }
}

fn file_tab(path: &Path) -> PlannedTab {
    PlannedTab {
        document: PlannedDocument::File(path.to_path_buf()),
        workspace: None,
    }
}

fn folder_window(folder: &Path) -> PlannedWindow {
    new_window(vec![PlannedTab {
        document: PlannedDocument::NewUntitled,
        workspace: Some(folder.to_path_buf()),
    }])
}

/// The identity of the file `path` under any spelling of its path, as [`Documents::find`] sees it.
fn file_key(path: &Path) -> String {
    ssw_io::watch::path_key(&canonical_path(path))
}

/// The index of the tab of `window` that shows the file `path`; a tab for it is appended when
/// none does.
fn tab_for(window: &mut PlannedWindow, path: &Path) -> usize {
    let wanted = file_key(path);
    let shown = window.tabs.iter().position(
        |tab| matches!(&tab.document, PlannedDocument::File(open) if file_key(open) == wanted),
    );
    shown.unwrap_or_else(|| {
        window.tabs.push(file_tab(path));
        window.tabs.len() - 1
    })
}

/// Decides which windows and tabs to create (spec §5, §6, §10 "On startup").
///
/// - A stored session is restored when the setting says so and it has windows.
/// - Files of the launch become tabs of the active restored window, except the ones it shows
///   already: their tab is the one that is activated (a single file in a fresh process opens
///   editor-only, without a workspace); `new_window` gives them a window.
/// - Every folder of the launch gets its own window with that folder as workspace.
/// - Without a session or launch items: a new untitled buffer, in the default folder when the
///   setting says so.
#[must_use]
pub fn plan_startup(
    behavior: StartupBehavior,
    default_folder: Option<&Path>,
    session: &Session,
    launch: &Launch,
) -> StartupPlan {
    let mut plan = StartupPlan::default();
    if behavior == StartupBehavior::RestoreSession {
        for window in &session.windows {
            plan.windows.push(PlannedWindow {
                placement: Some(window.placement),
                tabs: window
                    .tabs
                    .iter()
                    .map(|tab| PlannedTab {
                        document: match &tab.document {
                            TabDocument::File(path) => PlannedDocument::File(path.clone()),
                            TabDocument::Untitled(id) => {
                                PlannedDocument::RestoredUntitled(id.clone())
                            }
                        },
                        workspace: tab.workspace.clone(),
                    })
                    .collect(),
                active_tab: window.active_tab,
            });
        }
        plan.active_window = session
            .active_window
            .or_else(|| plan.windows.len().checked_sub(1));
    }

    let files: Vec<&Path> = launch.files().collect();
    if !files.is_empty() {
        let restored_active = plan.active_window;
        match restored_active {
            Some(active) if !launch.new_window => {
                let window = &mut plan.windows[active];
                let shown: Vec<usize> = files.iter().map(|path| tab_for(window, path)).collect();
                window.active_tab = shown.first().copied();
            }
            _ => {
                let tabs = files.iter().map(|path| file_tab(path)).collect();
                plan.windows.push(new_window(tabs));
                plan.active_window = Some(plan.windows.len() - 1);
            }
        }
    }
    for folder in launch.folders() {
        plan.windows.push(folder_window(folder));
        plan.active_window = Some(plan.windows.len() - 1);
    }

    if plan.windows.is_empty() {
        let window = match (behavior, default_folder) {
            (StartupBehavior::DefaultFolder, Some(folder)) => folder_window(folder),
            _ => new_window(vec![PlannedTab {
                document: PlannedDocument::NewUntitled,
                workspace: None,
            }]),
        };
        plan.windows.push(window);
        plan.active_window = Some(0);
    }
    plan
}

/// A tab closed earlier, remembered for `Ctrl+Shift+T`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosedTab {
    /// The file the tab showed.
    pub path: PathBuf,
    /// The tab's workspace root.
    pub workspace: Option<PathBuf>,
}

/// One tab: the document (shared with other tabs showing the same file) and the tab's
/// workspace root.
#[derive(Debug)]
pub struct TabState {
    /// The document.
    pub entry: Rc<DocEntry>,
    /// The tab's workspace root.
    pub workspace: Option<PathBuf>,
}

/// One window: its placement and tabs.
#[derive(Debug)]
pub struct WindowState {
    /// Where the window is.
    pub placement: WindowPlacement,
    /// The tabs, left to right.
    pub tabs: Vec<TabState>,
    /// Index into `tabs`.
    pub active_tab: Option<usize>,
}

/// The outcome of [`SessionManager::restore`].
#[derive(Debug, Default)]
pub struct RestoredSession {
    /// Windows with at least one tab.
    pub windows: Vec<WindowState>,
    /// Index into `windows`.
    pub active_window: Option<usize>,
    /// Tabs that could not be restored (file gone, unreadable) with the reason.
    pub skipped: Vec<(PathBuf, DocError)>,
}

/// Session persistence on top of the registry and the state database.
#[derive(Debug)]
pub struct SessionManager {
    docs: Rc<Documents>,
    store: SharedStore,
    closed: RefCell<Vec<ClosedTab>>,
}

impl SessionManager {
    /// A manager with an empty closed-tab stack.
    #[must_use]
    pub fn new(docs: Rc<Documents>) -> Self {
        let store = Rc::clone(&docs.env().store);
        Self {
            docs,
            store,
            closed: RefCell::new(Vec::new()),
        }
    }

    /// Writes the session: the structure goes to the database; the text of every untitled
    /// buffer a tab shows is stored beside it so the next start can restore it.
    ///
    /// # Errors
    /// [`StoreError`] when the database rejects the write; nothing is changed then.
    pub fn save(&self, session: &Session) -> Result<(), StoreError> {
        self.store_untitled(&self.untitled_texts(session))?;
        self.store.borrow_mut().save_session(session)
    }

    /// The text of every untitled buffer `session` names whose document is still open, as
    /// (id, text) pairs.
    ///
    /// Quitting closes the windows before the session is written, and closing a tab both drops
    /// its document and deletes the buffer's stored text: the texts have to be taken while the
    /// tabs are there and written again afterwards ([`SessionManager::store_untitled`]), or
    /// the next start finds a tab without a buffer and drops it.
    #[must_use]
    pub fn untitled_texts(&self, session: &Session) -> Vec<(String, String)> {
        let open = self.docs.all();
        session
            .windows
            .iter()
            .flat_map(|window| &window.tabs)
            .filter_map(|tab| match &tab.document {
                TabDocument::Untitled(id) => open
                    .iter()
                    .find(|entry| entry.untitled_id().as_deref() == Some(id.as_str()))
                    .map(|entry| (id.clone(), entry.document().borrow().text())),
                _ => None,
            })
            .collect()
    }

    /// Writes the texts [`SessionManager::untitled_texts`] took.
    ///
    /// # Errors
    /// [`StoreError`] when the database rejects a write.
    pub fn store_untitled(&self, texts: &[(String, String)]) -> Result<(), StoreError> {
        let now = self.docs.env().clock.unix_seconds();
        for (id, text) in texts {
            self.store.borrow().untitled_put(id, text, now)?;
        }
        Ok(())
    }

    /// Reads the stored session.
    ///
    /// # Errors
    /// [`StoreError`] when the database cannot be read.
    pub fn load(&self) -> Result<Session, StoreError> {
        self.store.borrow().load_session()
    }

    /// Opens the documents of `session`. Files that no longer open are skipped and reported;
    /// untitled buffers without a stored text are dropped; windows left without tabs vanish.
    #[must_use]
    pub fn restore(&self, session: &Session) -> RestoredSession {
        let mut restored = RestoredSession::default();
        for (index, window) in session.windows.iter().enumerate() {
            let (tabs, active_tab) = self.restore_window(window, &mut restored.skipped);
            if tabs.is_empty() {
                continue;
            }
            if session.active_window == Some(index) {
                restored.active_window = Some(restored.windows.len());
            }
            restored.windows.push(WindowState {
                placement: window.placement,
                tabs,
                active_tab,
            });
        }
        restored
    }

    /// The restored tabs of `window` and the index of the active one among them.
    fn restore_window(
        &self,
        window: &SessionWindow,
        skipped: &mut Vec<(PathBuf, DocError)>,
    ) -> (Vec<TabState>, Option<usize>) {
        let mut tabs = Vec::new();
        let mut active = None;
        for (index, tab) in window.tabs.iter().enumerate() {
            let entry = match &tab.document {
                TabDocument::File(path) => match self.docs.open(path) {
                    Ok(entry) => entry,
                    Err(error) => {
                        skipped.push((path.clone(), error));
                        continue;
                    }
                },
                TabDocument::Untitled(id) => {
                    let text = self.store.borrow().untitled_get(id).ok().flatten();
                    let Some(text) = text else { continue };
                    self.docs.restore_untitled(id, &text)
                }
            };
            if window.active_tab == Some(index) {
                active = Some(tabs.len());
            }
            tabs.push(TabState {
                entry,
                workspace: tab.workspace.clone(),
            });
        }
        (tabs, active)
    }

    /// Builds the session to store from windows and their tabs (the UI's own structure).
    #[must_use]
    pub fn describe(windows: &[WindowState], active_window: Option<usize>) -> Session {
        Session {
            windows: windows
                .iter()
                .map(|window| SessionWindow {
                    placement: window.placement,
                    tabs: window
                        .tabs
                        .iter()
                        .map(|tab| SessionTab {
                            document: match (tab.entry.path(), tab.entry.untitled_id()) {
                                (Some(path), _) => TabDocument::File(path),
                                (None, id) => TabDocument::Untitled(id.unwrap_or_default()),
                            },
                            workspace: tab.workspace.clone(),
                        })
                        .collect(),
                    active_tab: window.active_tab,
                })
                .collect(),
            active_window,
        }
    }

    /// Remembers caret, scroll and folds of `entry`'s file, and pushes it onto the
    /// reopen-closed-tab stack. Call when a tab closes (also for the last tab of a document).
    ///
    /// # Errors
    /// [`DocError::Store`] when the view state cannot be stored.
    pub fn tab_closed(
        &self,
        entry: &DocEntry,
        workspace: Option<PathBuf>,
        view: &ViewState,
    ) -> Result<(), DocError> {
        let Some(path) = entry.path() else {
            return Ok(());
        };
        self.docs.remember_view(entry, view)?;
        let mut closed = self.closed.borrow_mut();
        closed.retain(|tab| tab.path != path);
        closed.push(ClosedTab { path, workspace });
        if closed.len() > CLOSED_TAB_LIMIT {
            closed.remove(0);
        }
        Ok(())
    }

    /// Remembers the language the user chose for `entry` in the status bar (`None`: "Plain
    /// text"). The open document is coloured that way from now on, whatever its name or text say,
    /// and a file's choice is stored, so the file comes back that way when it is opened again. A
    /// document without a file keeps the choice only while it is open.
    ///
    /// # Errors
    /// [`DocError::Store`] when the choice cannot be stored; the open document keeps it.
    pub fn choose_language(
        &self,
        entry: &DocEntry,
        language: Option<Language>,
    ) -> Result<(), DocError> {
        entry.set_language_choice(language);
        if let Some(path) = entry.path() {
            store_language(&self.store.borrow(), &path, language)?;
        }
        Ok(())
    }

    /// Takes back the language the user chose for `entry` ("Automatic" in the status bar): the
    /// file's stored choice is deleted and the open document forgets it, so what the file's name
    /// and text say decides again, at once.
    ///
    /// # Errors
    /// [`DocError::Store`] when the stored choice cannot be deleted; the open document has
    /// forgotten it anyway.
    pub fn clear_language(&self, entry: &DocEntry) -> Result<(), DocError> {
        entry.clear_language_choice();
        if let Some(path) = entry.path() {
            forget_language(&self.store.borrow(), &path)?;
        }
        Ok(())
    }

    /// The tabs `Ctrl+Shift+T` would reopen, most recently closed last.
    #[must_use]
    pub fn closed_tabs(&self) -> Vec<ClosedTab> {
        self.closed.borrow().clone()
    }

    /// "Reopen closed tab": the most recently closed file that still opens. Files that are gone
    /// are dropped from the stack. `None` when nothing can be reopened.
    #[must_use]
    pub fn reopen_closed(&self) -> Option<(Rc<DocEntry>, Option<PathBuf>)> {
        loop {
            let tab = self.closed.borrow_mut().pop()?;
            if let Ok(entry) = self.docs.open(&tab.path) {
                return Some((entry, tab.workspace));
            }
        }
    }
}

/// First run: creates `<documents>\SS Writer\Welcome.md` (localized, never overwritten) once
/// and returns that folder; `None` on every later start.
///
/// # Errors
/// File system failures creating the folder or file, or the database failing to record it.
pub fn first_run_workspace(
    store: &SharedStore,
    fs: &dyn FileSystem,
    documents_dir: &Path,
    welcome: &str,
) -> Result<Option<PathBuf>, FirstRunError> {
    if store.borrow().kv_get(FIRST_RUN_KEY)?.is_some() {
        return Ok(None);
    }
    let folder = ensure_default_folder(fs, documents_dir, welcome)?;
    store.borrow().kv_set(FIRST_RUN_KEY, "1")?;
    Ok(Some(folder))
}

/// Why the first-run workspace could not be prepared.
#[derive(Debug, thiserror::Error)]
pub enum FirstRunError {
    /// The folder or file could not be created.
    #[error(transparent)]
    Fs(#[from] ssw_io::fs::FsError),
    /// The state database failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ssw_core::edit::Selection;
    use ssw_io::fs::RealFs;
    use ssw_io::history::SnapshotReason;
    use ssw_io::recovery::RecoveryStore;
    use ssw_io::save::ThreadSleeper;
    use ssw_io::watch::{FsEvent, FsEventKind};

    use super::*;
    use crate::args::LaunchItem;
    use crate::clock::{AppClock, ManualClock};
    use crate::documents::{DocEnv, canonical_path, replace_text};

    fn placement(x: i32) -> WindowPlacement {
        WindowPlacement {
            x,
            y: 0,
            width: 800,
            height: 600,
            maximized: false,
        }
    }

    fn stored_session() -> Session {
        Session {
            windows: vec![
                SessionWindow {
                    placement: placement(1),
                    tabs: vec![SessionTab {
                        document: TabDocument::File(PathBuf::from(r"C:\a.md")),
                        workspace: None,
                    }],
                    active_tab: Some(0),
                },
                SessionWindow {
                    placement: placement(2),
                    tabs: vec![SessionTab {
                        document: TabDocument::Untitled("u1".to_owned()),
                        workspace: Some(PathBuf::from(r"C:\ws")),
                    }],
                    active_tab: Some(0),
                },
            ],
            active_window: Some(1),
        }
    }

    fn launch(files: &[&str], folders: &[&str], new_window: bool) -> Launch {
        let mut items: Vec<LaunchItem> = files
            .iter()
            .map(|file| LaunchItem::File(PathBuf::from(file)))
            .collect();
        items.extend(
            folders
                .iter()
                .map(|folder| LaunchItem::Folder(PathBuf::from(folder))),
        );
        Launch { items, new_window }
    }

    #[test]
    fn a_stored_session_is_restored_and_launch_files_join_the_active_window() {
        let session = stored_session();
        let plan = plan_startup(
            StartupBehavior::RestoreSession,
            None,
            &session,
            &launch(&[r"C:\new.md"], &[], false),
        );
        assert_eq!(plan.windows.len(), 2);
        assert_eq!(plan.active_window, Some(1));
        assert_eq!(plan.windows[0].placement, Some(placement(1)));
        let active = &plan.windows[1];
        assert_eq!(active.tabs.len(), 2);
        assert_eq!(
            active.tabs[0].document,
            PlannedDocument::RestoredUntitled("u1".to_owned())
        );
        assert_eq!(
            active.tabs[1].document,
            PlannedDocument::File(PathBuf::from(r"C:\new.md"))
        );
        assert_eq!(active.active_tab, Some(1));
    }

    #[test]
    fn a_launch_file_the_restored_window_shows_already_is_activated_not_added_again() {
        let tab = |document| SessionTab {
            document,
            workspace: None,
        };
        let session = Session {
            windows: vec![SessionWindow {
                placement: placement(1),
                tabs: vec![
                    tab(TabDocument::Untitled("u1".to_owned())),
                    tab(TabDocument::File(PathBuf::from(r"C:\a.md"))),
                    tab(TabDocument::File(PathBuf::from(r"C:\b.md"))),
                ],
                active_tab: Some(0),
            }],
            active_window: Some(0),
        };
        let plan_for = |files: &[&str]| {
            plan_startup(
                StartupBehavior::RestoreSession,
                None,
                &session,
                &launch(files, &[], false),
            )
        };
        let documents = |plan: &StartupPlan| -> Vec<PlannedDocument> {
            plan.windows[0]
                .tabs
                .iter()
                .map(|tab| tab.document.clone())
                .collect()
        };
        let restored = vec![
            PlannedDocument::RestoredUntitled("u1".to_owned()),
            PlannedDocument::File(PathBuf::from(r"C:\a.md")),
            PlannedDocument::File(PathBuf::from(r"C:\b.md")),
        ];
        let with_new = || {
            let mut documents = restored.clone();
            documents.push(PlannedDocument::File(PathBuf::from(r"C:\new.md")));
            documents
        };

        // The case of the path does not matter: it is the same file, and its tab comes forward.
        let plan = plan_for(&[r"C:\A.MD"]);
        assert_eq!(documents(&plan), restored);
        assert_eq!(plan.windows[0].active_tab, Some(1));
        assert_eq!(plan.active_window, Some(0));

        // The first file of the launch decides which tab is active, whether it is new or not.
        let plan = plan_for(&[r"C:\B.MD", r"C:\new.md"]);
        assert_eq!(documents(&plan), with_new());
        assert_eq!(plan.windows[0].active_tab, Some(2));
        let plan = plan_for(&[r"C:\new.md", r"C:\b.md"]);
        assert_eq!(documents(&plan), with_new());
        assert_eq!(plan.windows[0].active_tab, Some(3));

        // A file the launch names twice gets one tab.
        let plan = plan_for(&[r"C:\new.md", r"C:\NEW.md"]);
        assert_eq!(documents(&plan), with_new());
        assert_eq!(plan.windows[0].active_tab, Some(3));
    }

    #[test]
    fn new_window_and_folders_get_their_own_windows() {
        let session = stored_session();
        let plan = plan_startup(
            StartupBehavior::RestoreSession,
            None,
            &session,
            &launch(&[r"C:\x.md"], &[r"C:\folder"], true),
        );
        assert_eq!(plan.windows.len(), 4);
        assert_eq!(
            plan.windows[2].tabs[0].document,
            PlannedDocument::File(PathBuf::from(r"C:\x.md"))
        );
        assert_eq!(
            plan.windows[3].tabs[0].workspace,
            Some(PathBuf::from(r"C:\folder"))
        );
        assert_eq!(plan.active_window, Some(3));
    }

    #[test]
    fn without_a_session_the_startup_setting_decides() {
        let empty = Session::default();
        let none = launch(&[], &[], false);
        for behavior in [
            StartupBehavior::RestoreSession,
            StartupBehavior::NewDocument,
        ] {
            let plan = plan_startup(behavior, Some(Path::new(r"C:\docs")), &empty, &none);
            assert_eq!(plan.windows.len(), 1);
            assert_eq!(
                plan.windows[0].tabs[0].document,
                PlannedDocument::NewUntitled
            );
            assert_eq!(plan.windows[0].tabs[0].workspace, None);
        }
        let plan = plan_startup(
            StartupBehavior::DefaultFolder,
            Some(Path::new(r"C:\docs")),
            &empty,
            &none,
        );
        assert_eq!(
            plan.windows[0].tabs[0].workspace,
            Some(PathBuf::from(r"C:\docs"))
        );
        // A new-document startup ignores the stored session.
        let plan = plan_startup(StartupBehavior::NewDocument, None, &stored_session(), &none);
        assert_eq!(plan.windows.len(), 1);
        // A single file in a fresh process opens editor-only.
        let plan = plan_startup(
            StartupBehavior::RestoreSession,
            None,
            &empty,
            &launch(&[r"C:\one.md"], &[], false),
        );
        assert_eq!(plan.windows[0].tabs[0].workspace, None);
        assert_eq!(plan.windows[0].tabs.len(), 1);
    }

    fn manager(tmp: &Path) -> (Rc<Documents>, SessionManager) {
        let store: SharedStore = Rc::new(RefCell::new(Store::open(&tmp.join("state.db")).unwrap()));
        let clock: Rc<dyn AppClock> = Rc::new(ManualClock::new(100));
        let docs = Rc::new(Documents::new(DocEnv {
            fs: Arc::new(RealFs),
            sleeper: Arc::new(ThreadSleeper),
            clock,
            store,
            recovery: Arc::new(RecoveryStore::new(Arc::new(RealFs), tmp.join("recovery"))),
            tld: None,
        }));
        (Rc::clone(&docs), SessionManager::new(docs))
    }

    #[test]
    fn a_session_round_trips_through_the_database_with_untitled_text() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.md");
        std::fs::write(&file, "disk\n").unwrap();
        let gone = tmp.path().join("gone.md");
        let (docs, sessions) = manager(tmp.path());
        let doc = docs.open(&file).unwrap();
        let scratch = docs.new_untitled();
        scratch
            .document()
            .borrow_mut()
            .set_selections(Selection::caret(0), &[]);
        {
            use ssw_core::edit::{Edit, Transaction};
            let mut document = scratch.document().borrow_mut();
            let tx = Transaction::single(
                Edit::insert(0, "scratch"),
                Selection::caret(0),
                Selection::caret(7),
            );
            document.apply(tx).unwrap();
        }
        let windows = [WindowState {
            placement: placement(5),
            tabs: vec![
                TabState {
                    entry: Rc::clone(&doc),
                    workspace: Some(tmp.path().to_path_buf()),
                },
                TabState {
                    entry: Rc::clone(&scratch),
                    workspace: None,
                },
            ],
            active_tab: Some(1),
        }];
        let described = SessionManager::describe(&windows, Some(0));
        sessions.save(&described).unwrap();
        assert_eq!(sessions.load().unwrap(), described);

        // A later start: the file vanished, the untitled text comes back.
        let mut stored = sessions.load().unwrap();
        stored.windows[0].tabs.insert(
            0,
            SessionTab {
                document: TabDocument::File(gone.clone()),
                workspace: None,
            },
        );
        stored.windows[0].active_tab = Some(2);
        let (docs2, sessions2) = manager(tmp.path());
        let restored = sessions2.restore(&stored);
        assert_eq!(restored.skipped.len(), 1);
        assert_eq!(restored.skipped[0].0, gone);
        assert_eq!(restored.active_window, Some(0));
        let window = &restored.windows[0];
        assert_eq!(window.tabs.len(), 2);
        assert_eq!(window.active_tab, Some(1));
        assert_eq!(window.tabs[1].entry.document().borrow().text(), "scratch");
        assert!(window.tabs[1].entry.is_dirty());
        assert_eq!(docs2.all().len(), 2);
    }

    #[test]
    fn closed_tabs_are_reopened_most_recent_first_and_missing_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let mut paths = Vec::new();
        for name in ["a.md", "b.md", "c.md"] {
            let path = tmp.path().join(name);
            std::fs::write(&path, name).unwrap();
            paths.push(path);
        }
        let view = ViewState {
            caret: 1,
            scroll_anchor: 0,
            scroll_delta: 0.0,
            folds: vec![],
        };
        for path in &paths {
            let entry = docs.open(path).unwrap();
            sessions
                .tab_closed(&entry, Some(tmp.path().to_path_buf()), &view)
                .unwrap();
            docs.release(&entry);
        }
        // Closing an untitled buffer is not remembered.
        sessions
            .tab_closed(&docs.new_untitled(), None, &view)
            .unwrap();
        assert_eq!(sessions.closed_tabs().len(), 3);
        std::fs::remove_file(&paths[2]).unwrap();
        let (entry, workspace) = sessions.reopen_closed().unwrap();
        assert!(entry.path().unwrap().ends_with("b.md"));
        assert_eq!(workspace, Some(tmp.path().to_path_buf()));
        // The stored caret comes back with the reopened file.
        assert_eq!(
            entry.document().borrow().primary_selection(),
            Selection::caret(1)
        );
        let (first, _) = sessions.reopen_closed().unwrap();
        assert!(first.path().unwrap().ends_with("a.md"));
        assert!(sessions.reopen_closed().is_none());

        for index in 0..(CLOSED_TAB_LIMIT + 5) {
            let path = tmp.path().join(format!("n{index}.md"));
            std::fs::write(&path, "x").unwrap();
            let entry = docs.open(&path).unwrap();
            sessions.tab_closed(&entry, None, &view).unwrap();
        }
        assert_eq!(sessions.closed_tabs().len(), CLOSED_TAB_LIMIT);
    }

    #[test]
    fn a_chosen_language_is_stored_per_file_and_found_when_the_file_opens_again() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.txt");
        let other = tmp.path().join("other.txt");
        std::fs::write(&file, "x = 1\n").unwrap();
        std::fs::write(&other, "y = 2\n").unwrap();
        let (docs, sessions) = manager(tmp.path());
        let entry = docs.open(&file).unwrap();
        assert_eq!(entry.language_choice(), None);

        sessions
            .choose_language(&entry, Some(Language::Python))
            .unwrap();
        assert_eq!(entry.language_choice(), Some(Some(Language::Python)));
        docs.release(&entry);

        // A later start finds the choice again; another file has none.
        let (docs, sessions) = manager(tmp.path());
        let reopened = docs.open(&file).unwrap();
        assert_eq!(reopened.language_choice(), Some(Some(Language::Python)));
        let untouched = docs.open(&other).unwrap();
        assert_eq!(untouched.language_choice(), None);

        // "Plain text" is a choice, and so is a language that detection would have found.
        sessions.choose_language(&reopened, None).unwrap();
        sessions
            .choose_language(&untouched, Some(Language::Json))
            .unwrap();
        let (docs, _) = manager(tmp.path());
        assert_eq!(docs.open(&file).unwrap().language_choice(), Some(None));
        assert_eq!(
            docs.open(&other).unwrap().language_choice(),
            Some(Some(Language::Json))
        );
    }

    #[test]
    fn every_language_survives_the_trip_through_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let path = canonical_path(&tmp.path().join("a.txt"));
        let (docs, _) = manager(tmp.path());
        let store = docs.env().store.borrow();
        assert_eq!(stored_language(&store, &path), None);
        for language in Language::ALL {
            store_language(&store, &path, Some(language)).unwrap();
            assert_eq!(stored_language(&store, &path), Some(Some(language)));
        }
        store_language(&store, &path, None).unwrap();
        assert_eq!(stored_language(&store, &path), Some(None));
        // The key is the file's path, whatever its case or slashes.
        let spelled = PathBuf::from(path.to_string_lossy().to_lowercase().replace('\\', "/"));
        assert_eq!(stored_language(&store, &spelled), Some(None));
        // A value nobody wrote is ignored.
        store
            .kv_set(&language_key(&path).unwrap(), "klingon")
            .unwrap();
        assert_eq!(stored_language(&store, &path), None);
    }

    #[test]
    fn a_document_without_a_file_keeps_its_language_in_memory_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let scratch = docs.new_untitled();
        sessions
            .choose_language(&scratch, Some(Language::Rust))
            .unwrap();
        assert_eq!(scratch.language_choice(), Some(Some(Language::Rust)));
        let rows: i64 = rusqlite::Connection::open(tmp.path().join("state.db"))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM kv WHERE key LIKE ?1",
                [format!("{LANGUAGE_KEY_PREFIX}%")],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "nothing is stored for a buffer");
    }

    #[test]
    fn a_language_that_cannot_be_stored_is_still_used_while_the_document_is_open() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, "x\n").unwrap();
        let (docs, sessions) = manager(tmp.path());
        let entry = docs.open(&file).unwrap();
        sabotage(tmp.path(), "DROP TABLE kv");
        let error = sessions
            .choose_language(&entry, Some(Language::Go))
            .unwrap_err();
        assert_eq!(error.to_string(), "database error: no such table: kv");
        assert_eq!(entry.language_choice(), Some(Some(Language::Go)));
    }

    #[test]
    fn taking_the_language_back_deletes_the_stored_choice_and_detection_decides_again() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.txt");
        let other = tmp.path().join("other.txt");
        std::fs::write(&file, "x = 1\n").unwrap();
        std::fs::write(&other, "y = 2\n").unwrap();
        let (docs, sessions) = manager(tmp.path());
        let entry = docs.open(&file).unwrap();
        let kept = docs.open(&other).unwrap();
        sessions
            .choose_language(&entry, Some(Language::Python))
            .unwrap();
        sessions.choose_language(&kept, None).unwrap();
        let rows = |tmp: &Path| -> i64 {
            rusqlite::Connection::open(tmp.join("state.db"))
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM kv WHERE key LIKE ?1",
                    [format!("{LANGUAGE_KEY_PREFIX}%")],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(rows(tmp.path()), 2);

        sessions.clear_language(&entry).unwrap();
        assert_eq!(entry.language_choice(), None);
        assert_eq!(rows(tmp.path()), 1, "the row is deleted, not overwritten");
        // Taking back a choice that is not there is fine too.
        sessions.clear_language(&entry).unwrap();
        assert_eq!(rows(tmp.path()), 1);
        docs.release(&entry);
        docs.release(&kept);

        // The next start finds no choice for the file, and the other file keeps its own.
        let (docs, _) = manager(tmp.path());
        assert_eq!(docs.open(&file).unwrap().language_choice(), None);
        assert_eq!(docs.open(&other).unwrap().language_choice(), Some(None));
    }

    #[test]
    fn a_document_without_a_file_takes_its_language_back_in_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let scratch = docs.new_untitled();
        sessions
            .choose_language(&scratch, Some(Language::Rust))
            .unwrap();
        sessions.clear_language(&scratch).unwrap();
        assert_eq!(scratch.language_choice(), None);
    }

    #[test]
    fn a_language_that_cannot_be_deleted_is_still_taken_back_while_the_document_is_open() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, "x\n").unwrap();
        let (docs, sessions) = manager(tmp.path());
        let entry = docs.open(&file).unwrap();
        sessions
            .choose_language(&entry, Some(Language::Go))
            .unwrap();
        sabotage(tmp.path(), "DROP TABLE kv");
        let error = sessions.clear_language(&entry).unwrap_err();
        assert_eq!(error.to_string(), "database error: no such table: kv");
        assert_eq!(entry.language_choice(), None);
    }

    /// A path with an unpaired surrogate: a legal Windows file name that has no UTF-8 text.
    fn non_unicode_path() -> PathBuf {
        use std::os::windows::ffi::OsStringExt;
        let name = std::ffi::OsString::from_wide(&[0x43, 0x3A, 0x5C, 0x61, 0xD800]);
        PathBuf::from(name)
    }

    /// How many languages the state database of [`manager`] remembers.
    fn stored_language_rows(tmp: &Path) -> i64 {
        rusqlite::Connection::open(tmp.join("state.db"))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM kv WHERE key LIKE ?1",
                [format!("{LANGUAGE_KEY_PREFIX}%")],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn a_file_name_that_is_not_unicode_has_no_key_and_no_stored_language() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, _) = manager(tmp.path());
        let store = docs.env().store.borrow();
        let odd = non_unicode_path();
        // Not the key, not storing, not forgetting: the path has no text to be a key of.
        for error in [
            language_key(&odd).unwrap_err(),
            store_language(&store, &odd, Some(Language::Rust)).unwrap_err(),
            store_language(&store, &odd, None).unwrap_err(),
            forget_language(&store, &odd).unwrap_err(),
        ] {
            assert!(error.to_string().contains("not valid Unicode"), "{error}");
        }
        assert_eq!(stored_language(&store, &odd), None);
        assert_eq!(stored_language_rows(tmp.path()), 0);

        // A file next to it is unaffected.
        let good = canonical_path(&tmp.path().join("a.txt"));
        store_language(&store, &good, Some(Language::Rust)).unwrap();
        assert_eq!(stored_language(&store, &good), Some(Some(Language::Rust)));
        assert_eq!(stored_language(&store, &odd), None);
        forget_language(&store, &good).unwrap();
        assert_eq!(stored_language(&store, &good), None);
    }

    #[test]
    fn a_state_database_that_cannot_be_read_has_no_stored_language() {
        let tmp = tempfile::tempdir().unwrap();
        let path = canonical_path(&tmp.path().join("a.txt"));
        let (docs, _) = manager(tmp.path());
        let store = docs.env().store.borrow();
        store_language(&store, &path, Some(Language::Go)).unwrap();
        assert_eq!(stored_language(&store, &path), Some(Some(Language::Go)));
        sabotage(tmp.path(), "DROP TABLE kv");
        assert_eq!(stored_language(&store, &path), None);
    }

    /// The language remembered for the file `path`.
    fn remembered(docs: &Documents, path: &Path) -> Option<Option<Language>> {
        stored_language(&docs.env().store.borrow(), path)
    }

    /// An untitled buffer that holds `text`.
    fn untitled_with(docs: &Documents, text: &str) -> Rc<DocEntry> {
        let entry = docs.new_untitled();
        assert!(replace_text(&mut entry.document().borrow_mut(), text));
        entry
    }

    #[test]
    fn a_renamed_file_keeps_its_language_and_nothing_stays_behind_at_the_old_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let old = tmp.path().join("notes.txt");
        let new = tmp.path().join("renamed.txt");
        std::fs::write(&old, "x = 1\n").unwrap();
        let entry = docs.open(&old).unwrap();
        sessions
            .choose_language(&entry, Some(Language::Python))
            .unwrap();
        let before = entry.path().unwrap();

        std::fs::rename(&old, &new).unwrap();
        docs.rebind_path(&entry, &new).unwrap();
        let after = entry.path().unwrap();
        assert_ne!(before, after);
        assert_eq!(remembered(&docs, &after), Some(Some(Language::Python)));
        assert_eq!(remembered(&docs, &before), None, "no row is left behind");
        assert_eq!(stored_language_rows(tmp.path()), 1);
        assert_eq!(entry.language_choice(), Some(Some(Language::Python)));

        // A file that is created at the old path later is not the renamed one: no language.
        docs.release(&entry);
        std::fs::write(&old, "y = 2\n").unwrap();
        assert_eq!(docs.open(&old).unwrap().language_choice(), None);
        // The renamed file still has its language when it is opened again.
        assert_eq!(
            docs.open(&new).unwrap().language_choice(),
            Some(Some(Language::Python))
        );
    }

    #[test]
    fn a_plain_text_choice_follows_a_renamed_file_too() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let old = tmp.path().join("data.json");
        let new = tmp.path().join("data.txt");
        std::fs::write(&old, "{}\n").unwrap();
        let entry = docs.open(&old).unwrap();
        sessions.choose_language(&entry, None).unwrap();
        let before = entry.path().unwrap();

        std::fs::rename(&old, &new).unwrap();
        docs.rebind_path(&entry, &new).unwrap();
        assert_eq!(remembered(&docs, &entry.path().unwrap()), Some(None));
        assert_eq!(remembered(&docs, &before), None);
        assert_eq!(stored_language_rows(tmp.path()), 1);
    }

    #[test]
    fn a_file_without_a_choice_does_not_inherit_a_row_left_at_its_new_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, _) = manager(tmp.path());
        let old = tmp.path().join("notes.txt");
        let new = tmp.path().join("renamed.txt");
        std::fs::write(&old, "x = 1\n").unwrap();
        let entry = docs.open(&old).unwrap();
        // What an earlier file of that name left behind.
        let leftover = canonical_path(&new);
        store_language(&docs.env().store.borrow(), &leftover, Some(Language::Go)).unwrap();

        std::fs::rename(&old, &new).unwrap();
        docs.rebind_path(&entry, &new).unwrap();
        assert_eq!(entry.language_choice(), None);
        assert_eq!(remembered(&docs, &leftover), None);
        assert_eq!(stored_language_rows(tmp.path()), 0);
    }

    #[test]
    fn a_rename_that_only_changes_the_case_keeps_the_row() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let old = tmp.path().join("Notes.txt");
        let new = tmp.path().join("notes.txt");
        std::fs::write(&old, "x = 1\n").unwrap();
        let entry = docs.open(&old).unwrap();
        sessions
            .choose_language(&entry, Some(Language::Go))
            .unwrap();

        // Both names are one key: the old row must be deleted before the new one is written.
        std::fs::rename(&old, &new).unwrap();
        docs.rebind_path(&entry, &new).unwrap();
        assert_eq!(
            remembered(&docs, &entry.path().unwrap()),
            Some(Some(Language::Go))
        );
        assert_eq!(stored_language_rows(tmp.path()), 1);
    }

    #[test]
    fn an_external_rename_moves_the_language_choice_too() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let old = tmp.path().join("notes.txt");
        let new = tmp.path().join("moved.txt");
        std::fs::write(&old, "x = 1\n").unwrap();
        let entry = docs.open(&old).unwrap();
        sessions
            .choose_language(&entry, Some(Language::Python))
            .unwrap();
        let before = entry.path().unwrap();

        std::fs::rename(&old, &new).unwrap();
        let outcomes = docs.handle_fs_events(&[FsEvent {
            path: new.clone(),
            kind: FsEventKind::Renamed { from: old.clone() },
        }]);
        assert_eq!(outcomes.len(), 1);
        let action = format!("{:?}", outcomes[0].action);
        assert!(action.starts_with("Renamed"), "{action}");
        assert_eq!(
            remembered(&docs, &entry.path().unwrap()),
            Some(Some(Language::Python))
        );
        assert_eq!(remembered(&docs, &before), None);
        assert_eq!(stored_language_rows(tmp.path()), 1);
    }

    #[test]
    fn a_history_that_cannot_follow_a_rename_does_not_leave_the_language_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let old = tmp.path().join("notes.txt");
        let new = tmp.path().join("renamed.txt");
        std::fs::write(&old, "x = 1\n").unwrap();
        let entry = docs.open(&old).unwrap();
        sessions
            .choose_language(&entry, Some(Language::Python))
            .unwrap();
        let before = entry.path().unwrap();
        sabotage(tmp.path(), "DROP TABLE history_snapshots");

        std::fs::rename(&old, &new).unwrap();
        let error = docs.rebind_path(&entry, &new).unwrap_err();
        assert_eq!(
            error.to_string(),
            "database error: no such table: history_snapshots"
        );
        assert_eq!(
            remembered(&docs, &entry.path().unwrap()),
            Some(Some(Language::Python))
        );
        assert_eq!(remembered(&docs, &before), None);
    }

    #[test]
    fn a_language_that_cannot_follow_a_rename_does_not_leave_the_history_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, _) = manager(tmp.path());
        let old = tmp.path().join("notes.txt");
        let new = tmp.path().join("renamed.txt");
        std::fs::write(&old, "x = 1\n").unwrap();
        let entry = docs.open(&old).unwrap();
        let before = entry.path().unwrap();
        docs.env()
            .store
            .borrow_mut()
            .history_snapshot(&before, "x = 1\n", SnapshotReason::Periodic, 100)
            .unwrap();
        sabotage(tmp.path(), "DROP TABLE kv");

        std::fs::rename(&old, &new).unwrap();
        let error = docs.rebind_path(&entry, &new).unwrap_err();
        assert_eq!(error.to_string(), "database error: no such table: kv");
        // The document is rebound and its history followed anyway.
        let after = entry.path().unwrap();
        assert_ne!(before, after);
        let store = docs.env().store.borrow();
        assert_eq!(store.history_list(&after).unwrap().len(), 1);
        assert!(store.history_list(&before).unwrap().is_empty());
    }

    #[test]
    fn save_as_stores_the_language_of_a_buffer_under_the_new_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        // A language and "Plain text" are both choices a buffer can carry to its file.
        for (name, choice) in [("rust.txt", Some(Language::Rust)), ("plain.txt", None)] {
            let scratch = untitled_with(&docs, "text\n");
            sessions.choose_language(&scratch, choice).unwrap();
            let target = tmp.path().join(name);
            assert_eq!(remembered(&docs, &canonical_path(&target)), None);

            docs.save_as(&scratch, &target).unwrap();
            assert_eq!(remembered(&docs, &scratch.path().unwrap()), Some(choice));
            docs.release(&scratch);
            assert_eq!(docs.open(&target).unwrap().language_choice(), Some(choice));
        }
        assert_eq!(stored_language_rows(tmp.path()), 2);
    }

    #[test]
    fn save_as_of_a_file_gives_the_copy_the_language_and_the_original_keeps_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let original = tmp.path().join("notes.txt");
        let copy = tmp.path().join("copy.txt");
        std::fs::write(&original, "x = 1\n").unwrap();
        let entry = docs.open(&original).unwrap();
        sessions
            .choose_language(&entry, Some(Language::Python))
            .unwrap();
        let original_path = entry.path().unwrap();

        docs.save_as(&entry, &copy).unwrap();
        let copy_path = entry.path().unwrap();
        assert_ne!(original_path, copy_path);
        assert_eq!(remembered(&docs, &copy_path), Some(Some(Language::Python)));
        // The file it was saved from is still there, and so is its choice.
        assert_eq!(
            remembered(&docs, &original_path),
            Some(Some(Language::Python))
        );
        assert_eq!(stored_language_rows(tmp.path()), 2);
    }

    #[test]
    fn save_as_without_a_choice_does_not_inherit_a_row_left_at_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, _) = manager(tmp.path());
        let target = tmp.path().join("old.txt");
        let leftover = canonical_path(&target);
        store_language(&docs.env().store.borrow(), &leftover, Some(Language::Go)).unwrap();
        let scratch = untitled_with(&docs, "text\n");

        docs.save_as(&scratch, &target).unwrap();
        assert_eq!(scratch.language_choice(), None);
        assert_eq!(remembered(&docs, &leftover), None);
        assert_eq!(stored_language_rows(tmp.path()), 0);
    }

    #[test]
    fn a_language_that_cannot_be_stored_by_save_as_leaves_the_file_saved_and_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, _) = manager(tmp.path());
        let target = tmp.path().join("kept.txt");
        let scratch = untitled_with(&docs, "text\n");
        sabotage(tmp.path(), "DROP TABLE kv");

        let error = docs.save_as(&scratch, &target).unwrap_err();
        assert_eq!(error.to_string(), "database error: no such table: kv");
        assert_eq!(std::fs::read(&target).unwrap(), b"text\n");
        assert_eq!(scratch.path(), Some(canonical_path(&target)));
        assert!(!scratch.is_untitled() && !scratch.is_dirty());
    }

    #[test]
    fn a_language_cannot_follow_a_file_from_or_to_a_name_that_is_not_unicode() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, _) = manager(tmp.path());
        let store = docs.env().store.borrow();
        let odd = non_unicode_path();
        let good = canonical_path(&tmp.path().join("a.txt"));
        store_language(&store, &good, Some(Language::Rust)).unwrap();

        // From a name that has no key nothing is deleted, and the other file keeps its row.
        let error = move_language(&store, &odd, &good, None).unwrap_err();
        assert!(error.to_string().contains("not valid Unicode"), "{error}");
        assert_eq!(stored_language(&store, &good), Some(Some(Language::Rust)));
        // To a name that has no key nothing is stored.
        let error = move_language(&store, &good, &odd, Some(None)).unwrap_err();
        assert!(error.to_string().contains("not valid Unicode"), "{error}");
        assert_eq!(stored_language(&store, &odd), None);
    }

    #[test]
    fn the_first_run_workspace_is_created_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store: SharedStore = Rc::new(RefCell::new(
            Store::open(&tmp.path().join("state.db")).unwrap(),
        ));
        let documents = tmp.path().join("Documents");
        std::fs::create_dir(&documents).unwrap();
        let folder = first_run_workspace(&store, &RealFs, &documents, "# Welcome\n")
            .unwrap()
            .unwrap();
        assert_eq!(folder, documents.join("SS Writer"));
        assert_eq!(
            std::fs::read_to_string(folder.join("Welcome.md")).unwrap(),
            "# Welcome\n"
        );
        // The user edits or deletes the file: it is never created again.
        std::fs::remove_file(folder.join("Welcome.md")).unwrap();
        assert_eq!(
            first_run_workspace(&store, &RealFs, &documents, "# Welcome\n").unwrap(),
            None
        );
        assert!(!folder.join("Welcome.md").exists());
    }

    /// Runs `sql` on a second connection to the state database of [`manager`].
    fn sabotage(tmp: &Path, sql: &str) {
        rusqlite::Connection::open(tmp.join("state.db"))
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    fn first_run_kind(error: &FirstRunError) -> &'static str {
        match error {
            FirstRunError::Fs(_) => "fs",
            FirstRunError::Store(_) => "store",
        }
    }

    #[test]
    fn windows_without_a_restorable_tab_vanish_and_the_active_window_follows_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let file = |name: &str| {
            let path = tmp.path().join(name);
            std::fs::write(&path, name).unwrap();
            path
        };
        let (b, c) = (file("b.md"), file("c.md"));
        let tab = |document: TabDocument| SessionTab {
            document,
            workspace: None,
        };
        let stored = Session {
            windows: vec![
                SessionWindow {
                    placement: placement(1),
                    tabs: vec![tab(TabDocument::File(tmp.path().join("gone.md")))],
                    active_tab: Some(0),
                },
                SessionWindow {
                    placement: placement(2),
                    tabs: vec![
                        tab(TabDocument::Untitled("lost".to_owned())),
                        tab(TabDocument::File(b)),
                    ],
                    active_tab: Some(1),
                },
                SessionWindow {
                    placement: placement(3),
                    tabs: vec![tab(TabDocument::File(c))],
                    active_tab: Some(0),
                },
            ],
            active_window: Some(2),
        };
        let restored = sessions.restore(&stored);
        assert_eq!(restored.skipped.len(), 1);
        assert!(restored.skipped[0].0.ends_with("gone.md"));
        let placements: Vec<WindowPlacement> = restored
            .windows
            .iter()
            .map(|window| window.placement)
            .collect();
        assert_eq!(placements, [placement(2), placement(3)]);
        // The untitled buffer without a stored text is dropped; the active tab is still b.md.
        let second = &restored.windows[0];
        assert_eq!(second.tabs.len(), 1);
        assert_eq!(second.active_tab, Some(0));
        assert!(second.tabs[0].entry.path().unwrap().ends_with("b.md"));
        assert_eq!(restored.active_window, Some(1));
        assert_eq!(docs.all().len(), 2);
    }

    #[test]
    fn a_session_the_database_refuses_is_reported_and_stores_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let scratch = docs.new_untitled();
        let windows = [WindowState {
            placement: placement(5),
            tabs: vec![TabState {
                entry: Rc::clone(&scratch),
                workspace: None,
            }],
            active_tab: Some(0),
        }];
        let described = SessionManager::describe(&windows, Some(0));
        sabotage(tmp.path(), "DROP TABLE untitled_buffers");
        let error = sessions.save(&described).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no such table: untitled_buffers"),
            "{error}"
        );
        assert!(sessions.load().unwrap().windows.is_empty());
    }

    #[test]
    fn a_tab_whose_view_cannot_be_stored_is_not_offered_for_reopening() {
        let tmp = tempfile::tempdir().unwrap();
        let (docs, sessions) = manager(tmp.path());
        let file = tmp.path().join("a.md");
        std::fs::write(&file, "a").unwrap();
        let entry = docs.open(&file).unwrap();
        sabotage(tmp.path(), "DROP TABLE file_view_state");
        let view = ViewState {
            caret: 1,
            scroll_anchor: 0,
            scroll_delta: 0.0,
            folds: vec![],
        };
        let error = sessions.tab_closed(&entry, None, &view).unwrap_err();
        assert_eq!(error.message_id(), "app-error-file");
        assert!(
            error.to_string().contains("no such table: file_view_state"),
            "{error}"
        );
        assert!(sessions.closed_tabs().is_empty());
    }

    #[test]
    fn the_first_run_is_recorded_only_once_its_workspace_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let store: SharedStore = Rc::new(RefCell::new(
            Store::open(&tmp.path().join("state.db")).unwrap(),
        ));
        let welcome = "# Welcome\n";

        // Documents is a file: the folder cannot be created and the run stays unrecorded.
        let blocked = tmp.path().join("blocked");
        std::fs::write(&blocked, "x").unwrap();
        let error = first_run_workspace(&store, &RealFs, &blocked, welcome).unwrap_err();
        assert_eq!(first_run_kind(&error), "fs");
        assert_eq!(store.borrow().kv_get(FIRST_RUN_KEY).unwrap(), None);

        // The database refuses to record it after the workspace was created.
        let documents = tmp.path().join("Documents");
        std::fs::create_dir(&documents).unwrap();
        sabotage(
            tmp.path(),
            "CREATE TRIGGER refuse_kv BEFORE INSERT ON kv
             BEGIN SELECT RAISE(ABORT, 'kv refused'); END;",
        );
        let error = first_run_workspace(&store, &RealFs, &documents, welcome).unwrap_err();
        assert_eq!(first_run_kind(&error), "store");
        assert!(error.to_string().contains("kv refused"), "{error}");
        let folder = documents.join("SS Writer");
        assert_eq!(
            std::fs::read_to_string(folder.join("Welcome.md")).unwrap(),
            welcome
        );
        // The next start tries again, keeps the file and records the run.
        sabotage(tmp.path(), "DROP TRIGGER refuse_kv");
        assert_eq!(
            first_run_workspace(&store, &RealFs, &documents, "# Changed\n").unwrap(),
            Some(folder.clone())
        );
        assert_eq!(
            std::fs::read_to_string(folder.join("Welcome.md")).unwrap(),
            welcome
        );
        assert_eq!(
            first_run_workspace(&store, &RealFs, &documents, welcome).unwrap(),
            None
        );

        // A database that cannot even be asked.
        sabotage(tmp.path(), "DROP TABLE kv");
        let error = first_run_workspace(&store, &RealFs, &documents, welcome).unwrap_err();
        assert_eq!(first_run_kind(&error), "store");
        assert!(error.to_string().contains("no such table: kv"), "{error}");
    }
}
