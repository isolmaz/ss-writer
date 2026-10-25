//! External changes (spec §4.3): the folders of open files are watched; a change by another
//! program reloads a clean document with the caret kept, or raises the conflict state when
//! there are unsaved edits (Keep mine / Load theirs / Compare). Own writes never come back as
//! external changes (the stamps of [`ssw_io::watch::ExternalChanges`] recognize them).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use ssw_io::encoding::TextEncoding;
use ssw_io::fs::FsError;
use ssw_io::history::SnapshotReason;
use ssw_io::save::SaveStamp;
use ssw_io::watch::{DocChange, FsEvent, Watcher, path_key};

use crate::session::move_language;
use crate::workers::Waker;

use super::{
    DocEntry, DocError, Documents, ExternalState, Source, load_file, load_file_as, replace_text,
};

/// What happened to a document because its file changed on disk.
#[derive(Debug)]
pub enum ExternalAction {
    /// The document had no unsaved edits and now shows the new file content.
    Reloaded,
    /// The document has unsaved edits and the file changed: the conflict banner is due.
    Conflict,
    /// The file was deleted; the document keeps its text.
    Deleted,
    /// The file moved or was renamed; the document follows it.
    Renamed {
        /// The previous path.
        from: PathBuf,
        /// The new path.
        to: PathBuf,
    },
    /// The file changed but could not be re-read now (still being written, locked).
    ReloadFailed(DocError),
}

/// One document and what happened to it.
#[derive(Debug)]
pub struct ExternalOutcome {
    /// The affected document.
    pub entry: Rc<DocEntry>,
    /// What was done.
    pub action: ExternalAction,
}

/// The choices of the conflict banner (Compare is the UI showing [`Documents::external_text`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Keep the unsaved edits; the next save overwrites the other program's change.
    KeepMine,
    /// Replace the text with the file content (the edits stay in the version history).
    LoadTheirs,
}

struct Watching {
    // Dropping the watcher stops its reader thread.
    _watcher: Watcher,
    events: Receiver<FsEvent>,
}

/// One watcher per watched folder: of the folder alone, or of the whole tree below it.
pub struct FolderWatches {
    waker: Arc<dyn Waker>,
    recursive: bool,
    watchers: HashMap<String, (PathBuf, Watching)>,
    /// When a folder that could not be watched may be tried again (monotonic time).
    retry_at: HashMap<String, Duration>,
}

impl std::fmt::Debug for FolderWatches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderWatches")
            .field("folders", &self.folders())
            .finish_non_exhaustive()
    }
}

impl FolderWatches {
    /// How long a folder that could not be watched is left alone before the next attempt: an
    /// offline share keeps the attempt waiting for a long time, and the caller is the UI thread.
    pub const RETRY_AFTER: Duration = Duration::from_secs(30);

    /// No folders watched; `waker` is called from the reader threads after each batch of events.
    /// Every folder is watched with the whole tree below it when `recursive`, alone otherwise.
    #[must_use]
    pub fn new(waker: Arc<dyn Waker>, recursive: bool) -> Self {
        Self {
            waker,
            recursive,
            watchers: HashMap::new(),
            retry_at: HashMap::new(),
        }
    }

    /// The watched folders.
    #[must_use]
    pub fn folders(&self) -> Vec<PathBuf> {
        let mut folders: Vec<PathBuf> = self
            .watchers
            .values()
            .map(|(folder, _)| folder.clone())
            .collect();
        folders.sort();
        folders
    }

    /// Starts watchers for new folders and stops those no longer wanted. Returns the folders
    /// that could not be watched (deleted, access denied) with the reason; such a folder is
    /// tried again [`FolderWatches::RETRY_AFTER`] later (`now` is the monotonic time), and at
    /// once when it was not wanted in between.
    pub fn sync(&mut self, wanted: &[PathBuf], now: Duration) -> Vec<(PathBuf, FsError)> {
        let keys: Vec<String> = wanted.iter().map(|folder| path_key(folder)).collect();
        self.watchers.retain(|key, _| keys.contains(key));
        self.retry_at.retain(|key, _| keys.contains(key));
        let mut failed = Vec::new();
        for (folder, key) in wanted.iter().zip(keys) {
            let waiting = self.retry_at.get(&key).is_some_and(|at| now < *at);
            if waiting || self.watchers.contains_key(&key) {
                continue;
            }
            let waker = Arc::clone(&self.waker);
            match Watcher::start(folder, self.recursive, move || waker.wake()) {
                Ok((watcher, events)) => {
                    self.retry_at.remove(&key);
                    self.watchers.insert(
                        key,
                        (
                            folder.clone(),
                            Watching {
                                _watcher: watcher,
                                events,
                            },
                        ),
                    );
                }
                Err(error) => {
                    self.retry_at.insert(key, now + Self::RETRY_AFTER);
                    failed.push((folder.clone(), error));
                }
            }
        }
        failed
    }

    /// Every event delivered since the last call.
    #[must_use]
    pub fn drain(&self) -> Vec<FsEvent> {
        self.watchers
            .values()
            .flat_map(|(_, watching)| watching.events.try_iter())
            .collect()
    }
}

impl Documents {
    /// The folders that hold open files (what [`FolderWatches::sync`] should watch). Bundles,
    /// untitled buffers and converted documents' folders are included like any other file.
    #[must_use]
    pub fn watch_folders(&self) -> Vec<PathBuf> {
        let mut folders: Vec<PathBuf> = Vec::new();
        for entry in self.entries.borrow().iter() {
            let state = entry.state.borrow();
            if matches!(state.source, Source::TextBundle { .. }) {
                continue;
            }
            if let Some(folder) = state.path.as_deref().and_then(Path::parent)
                && !folders
                    .iter()
                    .any(|known| path_key(known) == path_key(folder))
            {
                folders.push(folder.to_path_buf());
            }
        }
        folders
    }

    fn find_by_key(&self, key: &str) -> Option<Rc<DocEntry>> {
        self.entries
            .borrow()
            .iter()
            .find(|entry| {
                entry
                    .state
                    .borrow()
                    .path
                    .as_deref()
                    .is_some_and(|path| path_key(path) == key)
            })
            .cloned()
    }

    /// Applies what the watchers reported: reloads clean documents, flags conflicts and
    /// deletions, follows renames.
    pub fn handle_fs_events(&self, events: &[FsEvent]) -> Vec<ExternalOutcome> {
        let decisions = self.tracker.borrow().classify(events);
        let mut outcomes = Vec::new();
        for (path, change) in decisions {
            // `classify` decides only for tracked paths and never reports an unchanged
            // document, so every decision has a document and an action.
            outcomes.extend(self.find_by_key(&path_key(&path)).and_then(|entry| {
                self.apply_change(&entry, &path, change)
                    .map(|action| ExternalOutcome { entry, action })
            }));
        }
        outcomes
    }

    /// Compares one document with its file now (window activation): the same decision as for a
    /// watcher event. `None` when nothing changed.
    pub fn check_external(&self, entry: &Rc<DocEntry>) -> Option<ExternalOutcome> {
        let path = entry.path()?;
        let change = self.tracker.borrow().check(&path);
        let action = self.apply_change(entry, &path, change)?;
        Some(ExternalOutcome {
            entry: Rc::clone(entry),
            action,
        })
    }

    fn apply_change(
        &self,
        entry: &Rc<DocEntry>,
        path: &Path,
        change: DocChange,
    ) -> Option<ExternalAction> {
        Some(match change {
            DocChange::Unchanged => return None,
            DocChange::Changed => self.on_changed(entry),
            DocChange::Deleted => {
                entry.state.borrow_mut().external = ExternalState::Deleted;
                ExternalAction::Deleted
            }
            DocChange::Renamed { to } => match self.rebind_path(entry, &to) {
                Ok(()) => ExternalAction::Renamed {
                    from: path.to_path_buf(),
                    to,
                },
                Err(error) => ExternalAction::ReloadFailed(error),
            },
        })
    }

    fn on_changed(&self, entry: &Rc<DocEntry>) -> ExternalAction {
        if entry.is_dirty() {
            entry.state.borrow_mut().external = ExternalState::Conflict;
            return ExternalAction::Conflict;
        }
        match self.reload(entry) {
            Ok(()) => ExternalAction::Reloaded,
            Err(error) => ExternalAction::ReloadFailed(error),
        }
    }

    /// Replaces the text with the file's content, keeping the caret through a line diff. The
    /// text before the reload is snapshotted in the version history (spec §4.5).
    ///
    /// # Errors
    /// [`DocError::Load`] when the file cannot be read, [`DocError::Untitled`] for buffers.
    pub fn reload(&self, entry: &DocEntry) -> Result<(), DocError> {
        self.reload_with(entry, None)
    }

    /// "Reopen with encoding" (spec §4.2): [`Documents::reload`] decoding the file as
    /// `encoding` instead of the detected one. The BOM, if the file has one for that encoding,
    /// and the line endings are recorded as for any load.
    ///
    /// # Errors
    /// As [`Documents::reload`].
    pub fn reload_as(&self, entry: &DocEntry, encoding: TextEncoding) -> Result<(), DocError> {
        self.reload_with(entry, Some(encoding))
    }

    fn reload_with(&self, entry: &DocEntry, forced: Option<TextEncoding>) -> Result<(), DocError> {
        let path = entry.path().ok_or(DocError::Untitled)?;
        let loaded = load_file_as(self.env.fs.as_ref(), &path, self.env.tld.as_deref(), forced)?;
        // The history is a safety net: a failed snapshot must not block the reload.
        let _ = self.snapshot(entry, SnapshotReason::BeforeExternalReload);
        let tracked = !matches!(loaded.source, Source::TextBundle { .. });
        let version = {
            let mut document = entry.document.borrow_mut();
            replace_text(&mut document, &loaded.text);
            document.version()
        };
        {
            let mut state = entry.state.borrow_mut();
            state.saved_version = version;
            state.format = loaded.format;
            state.source = loaded.source;
            state.access = loaded.access;
            state.losses = loaded.losses;
            state.assets = loaded.assets;
            state.lossy = loaded.lossy;
            state.external = ExternalState::InSync;
        }
        if tracked {
            self.tracker.borrow_mut().track(&path, loaded.stamp);
        }
        Ok(())
    }

    /// The text of the file on disk, for the conflict banner's "Compare".
    ///
    /// # Errors
    /// [`DocError::Load`] when the file cannot be read.
    pub fn external_text(&self, entry: &DocEntry) -> Result<String, DocError> {
        let path = entry.path().ok_or(DocError::Untitled)?;
        Ok(load_file(self.env.fs.as_ref(), &path, self.env.tld.as_deref())?.text)
    }

    /// Resolves the conflict (or deleted) state of `entry`.
    ///
    /// # Errors
    /// [`DocError::Load`] when "Load theirs" cannot read the file.
    pub fn resolve_conflict(
        &self,
        entry: &DocEntry,
        resolution: Resolution,
    ) -> Result<(), DocError> {
        match resolution {
            Resolution::LoadTheirs => self.reload(entry),
            Resolution::KeepMine => {
                let path = entry.path().ok_or(DocError::Untitled)?;
                // Remember what is on disk now so the same foreign version is not reported
                // again; the next save replaces it. A missing file stays untracked-as-is.
                let fs = self.env.fs.as_ref();
                if let (Ok(meta), Ok(bytes)) = (fs.metadata(&path), fs.read(&path)) {
                    let stamp = SaveStamp::from_read(&meta, &bytes);
                    self.tracker.borrow_mut().track(&path, stamp);
                }
                entry.state.borrow_mut().external = ExternalState::InSync;
                Ok(())
            }
        }
    }

    /// Points `entry` at `to` after its file was renamed or moved (by the sidebar or by another
    /// program). The version history, the change tracking and the language the user chose for
    /// the file follow it; nothing stays behind at the old path.
    ///
    /// # Errors
    /// [`DocError::Store`] when the history or the language choice cannot be moved (the document
    /// is rebound anyway; the history is moved even when the language is not, and the other way
    /// round), [`DocError::Untitled`] for buffers.
    pub fn rebind_path(&self, entry: &DocEntry, to: &Path) -> Result<(), DocError> {
        let old = entry.path().ok_or(DocError::Untitled)?;
        let to = super::canonical_path(to);
        {
            let mut tracker = self.tracker.borrow_mut();
            if let Some(stamp) = tracker.stamp(&old) {
                tracker.untrack(&old);
                tracker.track(&to, stamp);
            }
        }
        entry.state.borrow_mut().path = Some(to.clone());
        // Both are tried: a history that cannot follow must not leave the language behind.
        let store = self.env.store.borrow();
        let history = store.history_move(&old, &to);
        let language = move_language(&store, &old, &to, entry.language_choice());
        Ok(history.and(language)?)
    }
}
