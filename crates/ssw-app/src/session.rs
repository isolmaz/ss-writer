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
use ssw_io::store::StoreError;
use ssw_io::workspace::ensure_default_folder;

use crate::args::Launch;
use crate::documents::{DocEntry, DocError, Documents, SharedStore};

/// How many closed tabs "Reopen closed tab" remembers.
pub const CLOSED_TAB_LIMIT: usize = 20;

/// Key in the state database that remembers the first-run workspace was created.
const FIRST_RUN_KEY: &str = "first_run_done";

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

fn new_window(tabs: Vec<PlannedTab>) -> PlannedWindow {
    PlannedWindow {
        placement: None,
        active_tab: if tabs.is_empty() { None } else { Some(0) },
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

/// Decides which windows and tabs to create (spec §5, §6, §10 "On startup").
///
/// - A stored session is restored when the setting says so and it has windows.
/// - Files of the launch become tabs of the active restored window (a single file in a fresh
///   process opens editor-only, without a workspace); `new_window` gives them a window.
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
        let tabs: Vec<PlannedTab> = files.iter().map(|path| file_tab(path)).collect();
        let restored_active = plan.active_window;
        match restored_active {
            Some(active) if !launch.new_window => {
                let window = &mut plan.windows[active];
                window.active_tab = Some(window.tabs.len());
                window.tabs.extend(tabs);
            }
            _ => {
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
        let now = self.docs.env().clock.unix_seconds();
        for window in &session.windows {
            for tab in &window.tabs {
                if let TabDocument::Untitled(id) = &tab.document
                    && let Some(entry) = self
                        .docs
                        .all()
                        .into_iter()
                        .find(|entry| entry.untitled_id().as_deref() == Some(id.as_str()))
                {
                    let text = entry.document().borrow().text();
                    self.store.borrow().untitled_put(id, &text, now)?;
                }
            }
        }
        self.store.borrow_mut().save_session(session)
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
    use ssw_io::recovery::RecoveryStore;
    use ssw_io::save::ThreadSleeper;
    use ssw_io::store::Store;

    use super::*;
    use crate::args::LaunchItem;
    use crate::clock::{AppClock, ManualClock};
    use crate::documents::DocEnv;

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
}
