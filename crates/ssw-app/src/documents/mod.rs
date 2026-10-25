//! The registry of open documents (spec §4, §6).
//!
//! One [`ssw_core::document::Document`] exists per file: every tab and window that shows the
//! same canonical path shares the same `Rc<RefCell<Document>>`, so edits appear everywhere at
//! once. Untitled buffers have no path. The registry also owns the per-document state the
//! editor needs besides the text: dirty tracking, encoding / BOM / line endings, import loss
//! reports, read-only detection and the external-change state.
//!
//! Everything here runs on the UI thread. File reads that should not block it (large files)
//! use [`load_file`] on a worker and hand the [`Loaded`] result to [`Documents::open_loaded`].

mod external;
mod load;
mod reload;
mod save;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use ssw_convert::import::{Asset, Loss};
use ssw_core::document::Document;
use ssw_core::edit::Selection;
use ssw_core::i18n::{Arg, Localizer};
use ssw_io::encoding::{EncodeError, TextFormat};
use ssw_io::fs::{FileSystem, FsError};
use ssw_io::history::{SnapshotOutcome, SnapshotReason};
use ssw_io::recovery::{RecoveryKind, RecoveryStore, RestoredDoc};
use ssw_io::save::{SaveError, Sleeper, WriteAccess};
use ssw_io::session::RecentKind;
use ssw_io::store::{Store, StoreError};
use ssw_io::watch::{ExternalChanges, path_key};
use ssw_ui::editor::highlight::Language;

use crate::clock::AppClock;
use crate::session::stored_language;

pub use external::{ExternalAction, ExternalOutcome, FolderWatches, Resolution};
pub use load::{
    ConvertedFormat, DocMode, LoadError, Loaded, Source, canonical_path, load_file, load_file_as,
    loss_message_id, mode_for_path, strip_extended_prefix,
};
pub use reload::{line_edits, replace_text};
pub use save::Saved;

/// The state database, shared by the registry, the autosave scheduler and the session.
pub type SharedStore = Rc<RefCell<Store>>;

/// A document shared by every tab and window that shows it.
pub type SharedDocument = Rc<RefCell<Document>>;

/// Identifies an open document for its lifetime in the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocId(pub u64);

/// Failures of document operations.
#[derive(Debug, thiserror::Error)]
pub enum DocError {
    /// Opening or reloading failed.
    #[error(transparent)]
    Load(#[from] LoadError),
    /// Writing failed.
    #[error(transparent)]
    Save(#[from] SaveError),
    /// A file system call failed.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// The text cannot be written in the document's encoding.
    #[error(transparent)]
    Encode(#[from] EncodeError),
    /// The state database failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Converting to a word-processor format or bundle failed.
    #[error("conversion failed: {0}")]
    Convert(String),
    /// The document has no file yet.
    #[error("the document has no file yet")]
    Untitled,
    /// A converted document is written with "Save as" or "Save back", never silently.
    #[error("a converted document is saved with Save as or Save back to the original format")]
    NeedsSaveAs,
    /// The format cannot be written.
    #[error("{0} files cannot be written")]
    Unsupported(&'static str),
    /// Another tab already shows that file.
    #[error("{} is already open", .0.display())]
    AlreadyOpen(PathBuf),
}

impl DocError {
    /// The Fluent message id (`app.ftl`) describing the failure to the user.
    #[must_use]
    pub fn message_id(&self) -> &'static str {
        use ssw_convert::import::ImportError;
        match self {
            Self::Load(LoadError::Fs(FsError::NotFound(_))) => "app-error-not-found",
            Self::Load(LoadError::Import(ImportError::Encrypted)) => "app-error-encrypted",
            Self::Load(LoadError::NotText(_)) => "app-error-not-text",
            Self::Load(LoadError::Import(_) | LoadError::Bundle(_)) | Self::Convert(_) => {
                "app-error-conversion"
            }
            Self::Load(_) | Self::Fs(_) | Self::Store(_) => "app-error-file",
            Self::Save(SaveError::ReadOnly(_)) => "app-error-read-only",
            Self::Save(SaveError::SharingViolation(_)) => "app-error-in-use",
            Self::Save(SaveError::DiskFull(_)) => "app-error-disk-full",
            Self::Save(SaveError::AccessDenied(_)) => "app-error-access-denied",
            Self::Save(SaveError::Fs(_) | SaveError::TempKept { .. }) => "app-error-file",
            Self::Encode(_) => "app-error-encoding",
            Self::Untitled => "app-error-untitled",
            Self::NeedsSaveAs => "app-error-needs-save-as",
            Self::Unsupported(_) => "app-error-unsupported",
            Self::AlreadyOpen(_) => "app-error-already-open",
        }
    }
}

/// How a document relates to the file on disk after an external change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalState {
    /// No unresolved difference.
    InSync,
    /// The file changed on disk while there are unsaved edits (Keep mine / Load theirs /
    /// Compare).
    Conflict,
    /// The file was deleted on disk; saving creates it again.
    Deleted,
}

#[derive(Debug)]
struct Untitled {
    id: String,
    number: u32,
}

#[derive(Debug)]
struct DocState {
    path: Option<PathBuf>,
    untitled: Option<Untitled>,
    format: TextFormat,
    source: Source,
    /// Document version at the last save or load; `u64::MAX` when the text was never saved.
    saved_version: u64,
    access: WriteAccess,
    external: ExternalState,
    losses: Vec<Loss>,
    assets: Vec<Asset>,
    lossy: bool,
    mixed_endings_notice: bool,
    /// "Render as Markdown" (spec §4.1) is on: a plain-text file is shown and edited as
    /// Markdown. Only the presentation changes; the file is written as it was.
    render_markdown: bool,
    /// The language the user chose for the document: `None` when nothing was chosen (the file's
    /// name and text decide), `Some(None)` for "Plain text", `Some(Some(language))` otherwise.
    /// A file's stored choice is read when the file is opened.
    language: Option<Option<Language>>,
    /// The scroll position and folds remembered for the file (spec §6); a tab opening the
    /// document starts from them.
    remembered: Option<ssw_io::session::ViewState>,
}

/// One open document and everything the app tracks about it.
#[derive(Debug)]
pub struct DocEntry {
    id: DocId,
    document: SharedDocument,
    state: RefCell<DocState>,
    users: Cell<usize>,
}

impl DocEntry {
    /// The registry id.
    #[must_use]
    pub fn id(&self) -> DocId {
        self.id
    }

    /// The shared document; every tab binds its editor to this.
    #[must_use]
    pub fn document(&self) -> &SharedDocument {
        &self.document
    }

    /// The canonical path; `None` for an untitled buffer.
    #[must_use]
    pub fn path(&self) -> Option<PathBuf> {
        self.state.borrow().path.clone()
    }

    /// The id of an untitled buffer (stable across sessions); `None` for files.
    #[must_use]
    pub fn untitled_id(&self) -> Option<String> {
        self.state
            .borrow()
            .untitled
            .as_ref()
            .map(|untitled| untitled.id.clone())
    }

    /// Whether this is an untitled buffer.
    #[must_use]
    pub fn is_untitled(&self) -> bool {
        self.state.borrow().untitled.is_some()
    }

    /// Whether this is an untitled buffer nobody has used: no text, no undo or redo history and
    /// no unsaved changes. Opening a file replaces such a tab instead of keeping it beside the
    /// new one.
    #[must_use]
    pub fn is_pristine_untitled(&self) -> bool {
        let document = self.document.borrow();
        self.is_untitled()
            && !self.is_dirty()
            && document.buffer().is_empty()
            && !document.can_undo()
            && !document.can_redo()
    }

    /// The tab title (spec §6): the file stem for Markdown files, the file name for everything
    /// else, "Untitled" / "Untitled 2" for buffers.
    #[must_use]
    pub fn title(&self, i18n: &Localizer) -> String {
        let state = self.state.borrow();
        if let Some(untitled) = &state.untitled {
            return if untitled.number <= 1 {
                i18n.text("app-untitled")
            } else {
                i18n.format(
                    "app-untitled-numbered",
                    &[("n", Arg::Int(i64::from(untitled.number)))],
                )
            };
        }
        // Every entry has a path or an untitled number: the empty path is never reached.
        let path = state.path.as_deref().unwrap_or(Path::new(""));
        let markdown = path.extension().is_some_and(|extension| {
            let extension = extension.to_string_lossy().to_ascii_lowercase();
            matches!(extension.as_str(), "md" | "markdown" | "mdx")
        });
        let name = if markdown {
            path.file_stem()
        } else {
            path.file_name()
        };
        name.map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The scroll position and folds remembered for the file when it was opened (spec §6).
    #[must_use]
    pub fn remembered_view(&self) -> Option<ssw_io::session::ViewState> {
        self.state.borrow().remembered.clone()
    }

    /// Encoding, BOM and line endings restored on save.
    #[must_use]
    pub fn format(&self) -> TextFormat {
        self.state.borrow().format
    }

    /// Where the text came from and how it is written back.
    #[must_use]
    pub fn source(&self) -> Source {
        self.state.borrow().source.clone()
    }

    /// Whether "Save back to Word" applies: the document was imported from a word-processor
    /// file that the exporter can write again.
    #[must_use]
    pub fn can_save_back(&self) -> bool {
        matches!(
            &self.state.borrow().source,
            Source::Converted(format) if format.can_save_back()
        )
    }

    /// How the editor treats the document: what the file type says, except that a plain-text
    /// file the user chose to "Render as Markdown" is treated as Markdown.
    #[must_use]
    pub fn mode(&self) -> DocMode {
        match self.file_mode() {
            DocMode::Plain if self.render_as_markdown() => DocMode::Markdown,
            mode => mode,
        }
    }

    /// What the file type alone says about the document (spec §4.1): Markdown for untitled
    /// buffers, Markdown files and converted documents, Mermaid for `.mmd`, plain for every
    /// other file. [`DocEntry::mode`] differs for plain files rendered as Markdown.
    #[must_use]
    pub fn file_mode(&self) -> DocMode {
        match &self.state.borrow().path {
            Some(path) => mode_for_path(path),
            None => DocMode::Markdown,
        }
    }

    /// Whether "Render as Markdown" is on for this document.
    #[must_use]
    pub fn render_as_markdown(&self) -> bool {
        self.state.borrow().render_markdown
    }

    /// Switches "Render as Markdown" (spec §4.1) on or off. Only plain-text files have it:
    /// returns `false` and changes nothing for any other document. The switch changes how the
    /// document is shown, never what is written: the path, encoding, line endings and bytes of
    /// the next save are those of the plain file.
    pub fn set_render_as_markdown(&self, render: bool) -> bool {
        if self.file_mode() != DocMode::Plain {
            return false;
        }
        self.state.borrow_mut().render_markdown = render;
        true
    }

    /// The language the user chose for this document in the status bar: `None` when nothing was
    /// chosen (what the file's name and text say decides), `Some(None)` for "Plain text" (no
    /// colors), `Some(Some(language))` for a language. A choice always beats detection, and a
    /// file's choice is stored (`SessionManager::choose_language`): it is back when the file is
    /// opened again.
    #[must_use]
    pub fn language_choice(&self) -> Option<Option<Language>> {
        self.state.borrow().language
    }

    /// Remembers the language the user chose (`None`: "Plain text") while the document is open.
    /// `SessionManager::choose_language` also stores the choice of a file.
    pub fn set_language_choice(&self, language: Option<Language>) {
        self.state.borrow_mut().language = Some(language);
    }

    /// Forgets the language the user chose: the file's name and text decide again.
    /// `SessionManager::clear_language` also deletes the stored choice of a file.
    pub fn clear_language_choice(&self) {
        self.state.borrow_mut().language = None;
    }

    /// Whether the file can be written.
    #[must_use]
    pub fn access(&self) -> WriteAccess {
        self.state.borrow().access
    }

    /// Whether the editor must show the read-only banner.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.access() != WriteAccess::Writable
    }

    /// The external-change state (conflict banner, deleted banner).
    #[must_use]
    pub fn external(&self) -> ExternalState {
        self.state.borrow().external
    }

    /// What the importer could not represent (the import banner's details list).
    #[must_use]
    pub fn losses(&self) -> Vec<Loss> {
        self.state.borrow().losses.clone()
    }

    /// Whether decoding replaced malformed input: saving would not reproduce the bytes.
    #[must_use]
    pub fn is_lossy(&self) -> bool {
        self.state.borrow().lossy
    }

    /// Unsaved changes exist: the text differs from what was last saved or loaded.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.document.borrow().version() != self.state.borrow().saved_version
    }

    /// Whether the user must be told once that mixed line endings were normalized. Returns
    /// `true` the first time only.
    pub fn take_mixed_endings_notice(&self) -> bool {
        std::mem::take(&mut self.state.borrow_mut().mixed_endings_notice)
    }

    /// How many tabs hold this document.
    #[must_use]
    pub fn users(&self) -> usize {
        self.users.get()
    }

    /// Changes the encoding / BOM written on the next save ("save with encoding"). The text is
    /// not touched; the document becomes dirty so the new encoding is written.
    pub fn set_save_encoding(&self, encoding: ssw_io::encoding::TextEncoding, bom: bool) {
        let mut state = self.state.borrow_mut();
        state.format.encoding = encoding;
        state.format.bom = bom && encoding.supports_bom();
        state.saved_version = u64::MAX;
    }

    /// Changes the line-ending style written on the next save; the document becomes dirty.
    pub fn set_save_line_ending(&self, line_ending: ssw_io::encoding::LineEnding) {
        let mut state = self.state.borrow_mut();
        state.format.line_ending = line_ending;
        state.saved_version = u64::MAX;
    }
}

/// What the registry needs from the system. Cloned into each component that shares it.
#[derive(Clone)]
pub struct DocEnv {
    /// The file system seam.
    pub fs: Arc<dyn FileSystem>,
    /// Sleeping between sharing-violation retries.
    pub sleeper: Arc<dyn Sleeper>,
    /// Monotonic and wall-clock time.
    pub clock: Rc<dyn AppClock>,
    /// The state database.
    pub store: SharedStore,
    /// Crash-recovery folder.
    pub recovery: Arc<RecoveryStore>,
    /// Top-level-domain hint for the legacy code page guess (`system_tld_hint`).
    pub tld: Option<String>,
}

impl std::fmt::Debug for DocEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocEnv")
            .field("fs", &self.fs)
            .field("clock", &self.clock)
            .field("recovery", &self.recovery)
            .field("tld", &self.tld)
            .finish_non_exhaustive()
    }
}

/// The registry.
#[derive(Debug)]
pub struct Documents {
    env: DocEnv,
    entries: RefCell<Vec<Rc<DocEntry>>>,
    tracker: RefCell<ExternalChanges>,
    next_id: Cell<u64>,
}

impl Documents {
    /// An empty registry.
    #[must_use]
    pub fn new(env: DocEnv) -> Self {
        let tracker = ExternalChanges::new(Arc::clone(&env.fs));
        Self {
            env,
            entries: RefCell::new(Vec::new()),
            tracker: RefCell::new(tracker),
            next_id: Cell::new(1),
        }
    }

    /// The shared environment.
    #[must_use]
    pub fn env(&self) -> &DocEnv {
        &self.env
    }

    /// Every open document, oldest first.
    #[must_use]
    pub fn all(&self) -> Vec<Rc<DocEntry>> {
        self.entries.borrow().clone()
    }

    /// The open document showing `path` (any spelling of the path), if any.
    #[must_use]
    pub fn find(&self, path: &Path) -> Option<Rc<DocEntry>> {
        let wanted = path_key(&canonical_path(path));
        self.entries
            .borrow()
            .iter()
            .find(|entry| {
                entry
                    .state
                    .borrow()
                    .path
                    .as_deref()
                    .is_some_and(|open| path_key(open) == wanted)
            })
            .cloned()
    }

    fn allocate_id(&self) -> DocId {
        let id = DocId(self.next_id.get());
        self.next_id.set(id.0 + 1);
        id
    }

    fn register(&self, document: Document, state: DocState) -> Rc<DocEntry> {
        let entry = Rc::new(DocEntry {
            id: self.allocate_id(),
            document: Rc::new(RefCell::new(document)),
            state: RefCell::new(state),
            users: Cell::new(1),
        });
        self.entries.borrow_mut().push(Rc::clone(&entry));
        entry
    }

    /// Opens `path`: the shared document when it is open already, otherwise the file is read
    /// (importers included), the stored caret restored, the file added to the recent list and
    /// tracked for external changes.
    ///
    /// # Errors
    /// [`DocError::Load`] when the file cannot be read or imported.
    pub fn open(&self, path: &Path) -> Result<Rc<DocEntry>, DocError> {
        if let Some(entry) = self.find(path) {
            entry.users.set(entry.users.get() + 1);
            return Ok(entry);
        }
        let canonical = canonical_path(path);
        let loaded = load_file(self.env.fs.as_ref(), &canonical, self.env.tld.as_deref())?;
        self.open_loaded(&canonical, loaded)
    }

    /// Opens a file that was read elsewhere (a worker thread). When the file was opened in the
    /// meantime the existing document is shared and `loaded` is dropped.
    ///
    /// # Errors
    /// [`DocError::Store`] when the recent list cannot be updated.
    pub fn open_loaded(&self, path: &Path, loaded: Loaded) -> Result<Rc<DocEntry>, DocError> {
        let canonical = canonical_path(path);
        if let Some(entry) = self.find(&canonical) {
            entry.users.set(entry.users.get() + 1);
            return Ok(entry);
        }
        let mut document = Document::new(&loaded.text);
        ssw_win::perf::mark_latest(crate::perf::DOC_PARSED);
        let remembered = self
            .env
            .store
            .borrow()
            .view_state(&canonical)
            .ok()
            .flatten();
        if let Some(view) = &remembered {
            let caret = document.buffer().floor_char_boundary(view.caret);
            document.set_selections(Selection::caret(caret), &[]);
        }
        let tracked = !matches!(loaded.source, Source::TextBundle { .. });
        let language = stored_language(&self.env.store.borrow(), &canonical);
        let state = DocState {
            path: Some(canonical.clone()),
            untitled: None,
            format: loaded.format,
            saved_version: document.version(),
            source: loaded.source,
            access: loaded.access,
            external: ExternalState::InSync,
            losses: loaded.losses,
            assets: loaded.assets,
            lossy: loaded.lossy,
            mixed_endings_notice: loaded.format.had_mixed_endings,
            render_markdown: false,
            language,
            remembered,
        };
        if tracked {
            self.tracker.borrow_mut().track(&canonical, loaded.stamp);
        }
        let now = self.env.clock.unix_seconds();
        self.env
            .store
            .borrow_mut()
            .recent_add(RecentKind::File, &canonical, now)?;
        Ok(self.register(document, state))
    }

    fn next_untitled_number(&self) -> u32 {
        let taken: Vec<u32> = self
            .entries
            .borrow()
            .iter()
            .filter_map(|entry| entry.state.borrow().untitled.as_ref().map(|u| u.number))
            .collect();
        (1..).find(|number| !taken.contains(number)).unwrap_or(1)
    }

    fn untitled_state(&self, id: String, text_is_unsaved: bool) -> DocState {
        DocState {
            path: None,
            untitled: Some(Untitled {
                id,
                number: self.next_untitled_number(),
            }),
            format: TextFormat::default(),
            source: Source::Text,
            saved_version: if text_is_unsaved { u64::MAX } else { 0 },
            access: WriteAccess::Writable,
            external: ExternalState::InSync,
            losses: Vec::new(),
            assets: Vec::new(),
            lossy: false,
            mixed_endings_notice: false,
            render_markdown: false,
            language: None,
            remembered: None,
        }
    }

    /// A new empty untitled buffer (`Ctrl+T` / `Ctrl+N` without a workspace).
    #[must_use]
    pub fn new_untitled(&self) -> Rc<DocEntry> {
        let id = format!(
            "untitled-{}-{}",
            self.env.clock.unix_seconds(),
            self.next_id.get()
        );
        self.register(Document::new(""), self.untitled_state(id, false))
    }

    /// An untitled buffer restored from a session or a recovery entry; the text counts as
    /// unsaved.
    #[must_use]
    pub fn restore_untitled(&self, id: &str, text: &str) -> Rc<DocEntry> {
        self.register(
            Document::new(text),
            self.untitled_state(id.to_owned(), true),
        )
    }

    /// Restores a document from the recovery folder: an untitled buffer, or the file the text
    /// belongs to with the recovered text as unsaved edits (a file that cannot be opened any more
    /// keeps the text as an untitled buffer). The recovery entry is discarded.
    #[must_use]
    pub fn restore_recovered(&self, restored: RestoredDoc) -> Rc<DocEntry> {
        let RestoredDoc { entry, text } = restored;
        let result = match (entry.kind, entry.original_path.as_deref()) {
            (RecoveryKind::Dirty, Some(path)) => {
                let document = self
                    .open(path)
                    .unwrap_or_else(|_| self.restore_untitled(&entry.id, &text));
                if !document.is_untitled() {
                    replace_text(&mut document.document.borrow_mut(), &text);
                    document.state.borrow_mut().saved_version = u64::MAX;
                    document.state.borrow_mut().format = entry.format;
                }
                document
            }
            _ => {
                let document = self.restore_untitled(&entry.id, &text);
                document.state.borrow_mut().format = entry.format;
                document
            }
        };
        // The recovery entry is consumed even when it was already gone.
        let _ = self.env.recovery.discard(&entry.id);
        result
    }

    /// Takes a snapshot of a file document in the version history.
    ///
    /// # Errors
    /// [`DocError::Store`] on database failure; untitled buffers have no history.
    pub fn snapshot(
        &self,
        entry: &DocEntry,
        reason: SnapshotReason,
    ) -> Result<Option<SnapshotOutcome>, DocError> {
        let Some(path) = entry.path() else {
            return Ok(None);
        };
        let text = entry.document.borrow().text();
        let now = self.env.clock.unix_seconds();
        Ok(Some(
            self.env
                .store
                .borrow_mut()
                .history_snapshot(&path, &text, reason, now)?,
        ))
    }

    /// A tab let go of `entry`. When it was the last tab the document is closed: a file's text,
    /// unsaved edits included, is snapshotted (spec §4.5 "on close"; the history stores nothing
    /// when the newest version is identical), the file stops being tracked and an untitled
    /// buffer's backups are removed. Returns whether the document was closed.
    pub fn release(&self, entry: &Rc<DocEntry>) -> bool {
        let remaining = entry.users.get().saturating_sub(1);
        entry.users.set(remaining);
        if remaining > 0 {
            return false;
        }
        if entry.path().is_some() {
            // A failed snapshot must not keep the tab open.
            let _ = self.snapshot(entry, SnapshotReason::Close);
        }
        if let Some(path) = entry.path() {
            self.tracker.borrow_mut().untrack(&path);
        }
        if let Some(id) = entry.untitled_id() {
            self.drop_untitled_backups(&id);
        }
        self.entries
            .borrow_mut()
            .retain(|open| !Rc::ptr_eq(open, entry));
        true
    }

    /// Removes the session and recovery backups of the untitled buffer `id` (saved or
    /// discarded).
    pub(crate) fn drop_untitled_backups(&self, id: &str) {
        // Both stores may legitimately not know the buffer yet.
        let _ = self.env.store.borrow().untitled_delete(id);
        let _ = self.env.recovery.discard(id);
    }

    /// "Allow editing": clears the read-only attribute when permitted.
    ///
    /// # Errors
    /// [`DocError::Fs`] with [`FsError::AccessDenied`] when the file stays read-only (ACL).
    pub fn allow_editing(&self, entry: &DocEntry) -> Result<(), DocError> {
        let Some(path) = entry.path() else {
            return Err(DocError::Untitled);
        };
        ssw_io::save::allow_editing(self.env.fs.as_ref(), &path)?;
        entry.state.borrow_mut().access = WriteAccess::Writable;
        Ok(())
    }

    /// Re-reads the read-only state of the file (after the user changed it in Explorer).
    pub fn refresh_access(&self, entry: &DocEntry) {
        if let Some(path) = entry.path()
            && let Ok(access) = ssw_io::save::write_access(self.env.fs.as_ref(), &path)
        {
            entry.state.borrow_mut().access = access;
        }
    }

    /// The caret, scroll and folds remembered for `path` (spec §6), for the editor view.
    #[must_use]
    pub fn view_state(&self, path: &Path) -> Option<ssw_io::session::ViewState> {
        self.env
            .store
            .borrow()
            .view_state(&canonical_path(path))
            .ok()
            .flatten()
    }

    /// Remembers the view state of `entry`'s file.
    ///
    /// # Errors
    /// [`DocError::Store`] on database failure.
    pub fn remember_view(
        &self,
        entry: &DocEntry,
        view: &ssw_io::session::ViewState,
    ) -> Result<(), DocError> {
        if let Some(path) = entry.path() {
            let now = self.env.clock.unix_seconds();
            self.env.store.borrow().view_state_put(&path, view, now)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
