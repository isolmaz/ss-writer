//! The files of a workspace (spec §3.2, §4.3, §5): link updates after a rename or move, wikilinks
//! to missing notes, the workspace watchers, the index upkeep, the `[[` completion sources and
//! the background open of OneDrive placeholders.
//!
//! * **Link updates.** Renaming an entry in the sidebar, or dropping it into another folder,
//!   asks to update the links that point at it ([`Controller::plan_link_updates`] before the
//!   change, [`Controller::offer_link_updates`] after it). The documents that link to the file
//!   come from the workspace index ([`ssw_io::store::Store::index_links_to`]) plus every open
//!   Markdown document (its text may hold links the index has not seen). The new texts come
//!   from [`ssw_core::links::rename_edits`]; open documents get them as one undoable
//!   transaction, closed files are rewritten atomically in their own encoding.
//! * **Wikilinks.** Ctrl+click on `[[note]]` opens the note; a note that does not exist is
//!   offered for creation, inside the workspace root only ([`Controller::activate_wikilink`]).
//! * **Watching.** [`Controller::pump_files`] is one housekeeping turn: one watcher per
//!   workspace root keeps the sidebar and the index current, `[[` completion is fed with the
//!   workspace files and the headings of the active document, and the results of background
//!   work reach the windows.
//! * **Placeholders.** A OneDrive placeholder downloads while it is read, so opening one runs on
//!   a worker with indeterminate taskbar progress ([`Controller::begin_hydration`]).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use ssw_core::edit::{Edit, Selection};
use ssw_core::i18n::Arg;
use ssw_core::links::{self, RelPath};
use ssw_core::markdown::parse_document;
use ssw_core::text::TextRange;
use ssw_io::encoding;
use ssw_io::fileops;
use ssw_io::fs::{FileSystem, FsError, RealFs};
use ssw_io::index::{IndexChange, IndexOptions};
use ssw_io::logging::Level;
use ssw_io::recovery::now_millis;
use ssw_io::save::save_atomic;
use ssw_io::store::Store;
use ssw_io::watch::{FsEvent, FsEventKind, path_key};
use ssw_ui::services::{MessageBoxButtons, MessageBoxIcon, MessageBoxOptions, MessageBoxResult};
use ssw_ui::shell::sidebar::NodeKey;
use ssw_win::taskbar::Progress;

use super::controller::{Controller, Request, WindowCell};
use super::inspector::toc_of;
use super::overlays::list_workspace_files;
use crate::documents::{
    DocEntry, DocError, DocMode, FolderWatches, LoadError, Source, canonical_path, load_file,
    replace_text, strip_extended_prefix,
};
use crate::workers::{JobKey, JobKind, JobSpec, Waker, WindowId};

/// Job keys of index upkeep start here (each job gets its own key: a newer job must not
/// supersede the changes of an older one).
const INDEX_JOB_BASE: u64 = 1 << 62;
/// Job keys of background opens start here.
const OPEN_JOB_BASE: u64 = 1 << 61;

// ---------------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------------

/// The path of `path` below `root`, compared ignoring case like NTFS does. `None` for the root
/// itself, for paths outside it and for names a workspace path cannot hold.
#[must_use]
pub fn relative_to(root: &Path, path: &Path) -> Option<RelPath> {
    let root = strip_extended_prefix(root);
    let path = strip_extended_prefix(path);
    let mut below = path.components();
    for expected in root.components() {
        let found = below.next()?;
        let same = links::eq_ordinal_ignore_case(
            &expected.as_os_str().to_string_lossy(),
            &found.as_os_str().to_string_lossy(),
        );
        if !same {
            return None;
        }
    }
    let rest: Vec<String> = below
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    RelPath::parse(&rest.join("/")).ok()
}

/// The absolute path of the workspace file `path` below `root`.
#[must_use]
pub fn absolute(root: &Path, path: &RelPath) -> PathBuf {
    root.join(path.segments().collect::<PathBuf>())
}

fn same(a: &RelPath, b: &RelPath) -> bool {
    links::eq_ordinal_ignore_case(a.as_str(), b.as_str())
}

/// Whether `path` is `folder` or below it.
fn is_within(folder: &Path, path: &Path) -> bool {
    let folder = path_key(folder);
    let path = path_key(path);
    path == folder || path.starts_with(&format!("{folder}\\"))
}

/// Whether `target`, which may not exist yet, ends up inside `root` once every link on the way
/// (junctions, symbolic links) is resolved: the nearest existing ancestor is canonicalized.
fn confined(root: &Path, target: &Path) -> bool {
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    // `target` lies below `root`, which exists: the walk up ends at the root at the latest.
    target
        .ancestors()
        .find_map(|ancestor| std::fs::canonicalize(ancestor).ok())
        .is_some_and(|existing| {
            links::is_inside_root(&root.to_string_lossy(), &existing.to_string_lossy())
        })
}

/// The workspace root of the folder tree `cell` shows.
fn workspace_root(cell: &WindowCell) -> Option<PathBuf> {
    Controller::tree_path(cell, 0)
}

// ---------------------------------------------------------------------------------------------
// Link updates (pure part)
// ---------------------------------------------------------------------------------------------

/// A workspace file that moved or was renamed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    /// Where the file was.
    pub from: RelPath,
    /// Where it is now.
    pub to: RelPath,
}

/// The files that move when `old` moves to `new`: just that file, or every file of `files`
/// below `old` when it is a folder (each lands at the same place below `new`).
#[must_use]
pub fn moves_for(files: &[RelPath], old: &RelPath, new: &RelPath, folder: bool) -> Vec<Move> {
    if !folder {
        return vec![Move {
            from: old.clone(),
            to: new.clone(),
        }];
    }
    let prefix = format!("{}/", old.as_str());
    let length = prefix.chars().count();
    files
        .iter()
        .filter_map(|file| {
            let head: String = file.as_str().chars().take(length).collect();
            if !links::eq_ordinal_ignore_case(&head, &prefix) {
                return None;
            }
            let tail = &file.as_str()[head.len()..];
            // `new` and `tail` are valid paths (`tail` is a tail of a valid path starting at a
            // segment), so their join is valid too.
            let to = RelPath::parse(&format!("{}/{tail}", new.as_str()))
                .expect("the join of two valid paths is valid");
            Some(Move {
                from: file.clone(),
                to,
            })
        })
        .collect()
}

/// The new text of one document whose links follow a move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkUpdate {
    /// The document.
    pub path: RelPath,
    /// The text the new text was computed from.
    pub original: String,
    /// The text with every link following the move.
    pub text: String,
    /// How many link destinations changed.
    pub links: usize,
}

/// `text` with the sorted, disjoint `edits` applied.
fn apply_edits(text: &str, edits: &[Edit]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for edit in edits {
        out.push_str(&text[at..edit.range.start]);
        out.push_str(&edit.text);
        at = edit.range.end;
    }
    out.push_str(&text[at..]);
    out
}

/// The new texts of the documents that link to moved files. Every move comes with the
/// documents that may link to its file; `files` is the workspace file list before any of the
/// moves and `read` gives the current text of a document (`None`: it cannot be read, it stays
/// as it is). The moves apply one after the other, so a document linking to several of them
/// gets all its links changed. A moved document is never changed itself: its relative links
/// are judged against a location it no longer has.
pub fn link_updates(
    files: &[RelPath],
    moves: &[(Move, Vec<RelPath>)],
    read: &mut dyn FnMut(&RelPath) -> Option<String>,
) -> Vec<LinkUpdate> {
    let mut known = files.to_vec();
    let mut work: Vec<LinkUpdate> = Vec::new();
    let mut unreadable: Vec<RelPath> = Vec::new();
    for (step, sources) in moves {
        for source in sources {
            let moved = moves
                .iter()
                .any(|(other, _)| same(&other.from, source) || same(&other.to, source));
            if moved || unreadable.iter().any(|path| same(path, source)) {
                continue;
            }
            let index = match work.iter().position(|item| same(&item.path, source)) {
                Some(index) => index,
                None => match read(source) {
                    Some(text) => {
                        work.push(LinkUpdate {
                            path: source.clone(),
                            original: text.clone(),
                            text,
                            links: 0,
                        });
                        work.len() - 1
                    }
                    None => {
                        unreadable.push(source.clone());
                        continue;
                    }
                },
            };
            let item = &mut work[index];
            let document = parse_document(&item.text);
            let edits = links::rename_edits(
                &item.text, &document, &item.path, &known, &step.from, &step.to,
            );
            if !edits.is_empty() {
                item.text = apply_edits(&item.text, &edits);
                item.links += edits.len();
            }
        }
        for file in &mut known {
            if same(file, &step.from) {
                file.clone_from(&step.to);
            }
        }
    }
    work.retain(|item| item.links > 0);
    work
}

/// What the index knew about a file before it moved (see [`Controller::plan_link_updates`]).
#[derive(Debug, Clone)]
pub struct LinkPlan {
    root: PathBuf,
    /// The workspace files before the move.
    files: Vec<RelPath>,
    /// The moved entry (a file or a folder).
    old: RelPath,
    folder: bool,
    /// For every file that moves: the documents the index says link to it.
    sources: Vec<(RelPath, Vec<RelPath>)>,
}

// ---------------------------------------------------------------------------------------------
// `[[` completion sources
// ---------------------------------------------------------------------------------------------

/// The Markdown files of the indexed workspace `root` for `[[` completion, and the indices of
/// the files that are open (the most recently opened first).
#[must_use]
pub fn completion_files(
    store: &Store,
    open: &[Rc<DocEntry>],
    root: &Path,
) -> (Vec<RelPath>, Vec<usize>) {
    let files: Vec<RelPath> = store
        .index_files(root)
        .unwrap_or_default()
        .iter()
        .filter(|file| file.markdown)
        .filter_map(|file| RelPath::parse(&file.rel).ok())
        .collect();
    let recent = open
        .iter()
        .rev()
        .filter_map(|entry| relative_to(root, &entry.path()?))
        .filter_map(|rel| files.iter().position(|file| same(file, &rel)))
        .collect();
    (files, recent)
}

/// Whether the caret of `entry` sits after a `[[` on its line.
fn in_wikilink(entry: &DocEntry) -> bool {
    let document = entry.document().borrow();
    let caret = document.primary_selection().head;
    let buffer = document.buffer();
    let line = buffer.line_range(buffer.line_of(caret));
    buffer
        .slice(TextRange::new(line.start.min(caret), caret))
        .contains("[[")
}

/// What one window's `[[` completion was last fed from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CompletionKey {
    /// Workspace root, index epoch and number of open documents.
    files: Option<(String, u64, usize)>,
    /// Tab and document version of the headings.
    outline: Option<(u64, u64)>,
}

// ---------------------------------------------------------------------------------------------
// State of the controller
// ---------------------------------------------------------------------------------------------

/// Work a worker finished that only the controller can show.
#[derive(Debug)]
enum Deferred {
    /// The taskbar button of a window shows this progress.
    Progress(WindowId, Progress),
    /// A background open of a file the user only browsed to found it is no text: its default
    /// app starts instead.
    Launch(PathBuf),
    /// A background open failed.
    Report(Box<DocError>),
}

/// Queues the failure `error` of background work for [`Controller::pump_files`] to show.
fn report_later(outbox: &RefCell<Vec<Deferred>>, error: impl Into<DocError>) {
    outbox
        .borrow_mut()
        .push(Deferred::Report(Box::new(error.into())));
}

/// The index upkeep of one workspace root.
#[derive(Debug, Default)]
struct RootQueue {
    /// A job is writing to the index.
    running: bool,
    /// The whole workspace must be walked again.
    full: bool,
    /// Changes the running job did not see.
    changes: Vec<IndexChange>,
}

/// The index upkeep of every workspace root, by lower-cased path.
#[derive(Debug, Default)]
struct IndexQueues {
    roots: HashMap<String, RootQueue>,
}

/// The state of the workspace file services of a [`Controller`].
#[derive(Debug, Default)]
pub struct FilesState {
    /// One watcher per workspace root (created with the first pump).
    watches: RefCell<Option<FolderWatches>>,
    /// The roots `watches` follows.
    watched: RefCell<Vec<PathBuf>>,
    index: Rc<RefCell<IndexQueues>>,
    job_numbers: Cell<u64>,
    completion: RefCell<HashMap<WindowId, CompletionKey>>,
    /// Files being read on a worker (lower-cased paths).
    hydrating: Rc<RefCell<HashSet<String>>>,
    outbox: Rc<RefCell<Vec<Deferred>>>,
}

impl FilesState {
    fn next_job(&self, base: u64) -> JobKey {
        let number = self.job_numbers.get();
        self.job_numbers.set(number + 1);
        JobKey(base + number)
    }
}

// ---------------------------------------------------------------------------------------------
// The controller side
// ---------------------------------------------------------------------------------------------

impl Controller {
    // ----- link updates -------------------------------------------------------------------

    /// Looks up, before `old` is renamed or moved, which documents link to it (or, for a
    /// folder, to the files in it). `None` when `old` is not inside the workspace of `cell` or
    /// the index cannot be read.
    pub fn plan_link_updates(&self, cell: &WindowCell, old: &Path) -> Option<LinkPlan> {
        let root = Self::tree_path(cell, 0)?;
        let old_rel = relative_to(&root, old)?;
        let docs = self.docs();
        let folder = docs.env().fs.metadata(old).is_ok_and(|meta| meta.is_dir());
        let store = docs.env().store.borrow();
        let files: Vec<RelPath> = store
            .index_files(&root)
            .ok()?
            .iter()
            .filter_map(|file| RelPath::parse(&file.rel).ok())
            .collect();
        let sources = moves_for(&files, &old_rel, &old_rel, folder)
            .into_iter()
            .map(|moved| {
                let mut linking: Vec<RelPath> = Vec::new();
                let sites = store
                    .index_links_to(&root, &absolute(&root, &moved.from))
                    .unwrap_or_default();
                for site in sites {
                    if let Some(rel) = relative_to(&root, &site.source)
                        && !linking.contains(&rel)
                    {
                        linking.push(rel);
                    }
                }
                (moved.from, linking)
            })
            .collect();
        Some(LinkPlan {
            root,
            files,
            old: old_rel,
            folder,
            sources,
        })
    }

    /// After `plan`'s entry moved to `new`: asks whether the links that pointed at it should
    /// follow, and rewrites them when the answer is yes. Nothing is asked when no link is
    /// affected.
    pub fn offer_link_updates(&self, plan: Option<LinkPlan>, new: &Path) {
        let Some(plan) = plan else {
            return;
        };
        let Some(new_rel) = relative_to(&plan.root, new) else {
            return;
        };
        let open = self.open_markdown_under(&plan.root);
        let moves: Vec<(Move, Vec<RelPath>)> =
            moves_for(&plan.files, &plan.old, &new_rel, plan.folder)
                .into_iter()
                .map(|step| {
                    let mut sources = plan
                        .sources
                        .iter()
                        .find(|(from, _)| same(from, &step.from))
                        .map(|(_, linking)| linking.clone())
                        .unwrap_or_default();
                    for path in &open {
                        if !sources.iter().any(|known| same(known, path)) {
                            sources.push(path.clone());
                        }
                    }
                    (step, sources)
                })
                .collect();
        let updates = link_updates(&plan.files, &moves, &mut |rel| {
            self.source_text(&plan.root, rel)
        });
        if updates.is_empty() {
            return;
        }
        let i18n = self.i18n();
        let name = new
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let answer = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: i18n.text("files-links-title"),
            message: i18n.format(
                "files-links-message",
                &[
                    ("name", Arg::Str(&name)),
                    ("files", Arg::Int(count(updates.len()))),
                ],
            ),
            buttons: MessageBoxButtons::YesNo,
            icon: Some(MessageBoxIcon::Question),
        });
        if !matches!(answer, Ok(MessageBoxResult::Yes)) {
            return;
        }
        let mut failed = 0;
        for update in &updates {
            if !self.apply_link_update(&plan.root, update) {
                failed += 1;
            }
        }
        if failed > 0 {
            let _ = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
                title: i18n.text("files-links-title"),
                message: i18n.format("files-links-failed", &[("files", Arg::Int(count(failed)))]),
                buttons: MessageBoxButtons::Ok,
                icon: Some(MessageBoxIcon::Warning),
            });
        }
        self.refresh_all();
    }

    /// The open Markdown documents below `root` (not converted ones: they are not text files).
    fn open_markdown_under(&self, root: &Path) -> Vec<RelPath> {
        self.docs()
            .all()
            .iter()
            .filter(|entry| {
                entry.mode() == DocMode::Markdown && matches!(entry.source(), Source::Text)
            })
            .filter_map(|entry| relative_to(root, &entry.path()?))
            .collect()
    }

    /// The text of the workspace file `rel`: the open document's, else what is on disk. A
    /// OneDrive placeholder is not read (that would download it, spec §4.3).
    fn source_text(&self, root: &Path, rel: &RelPath) -> Option<String> {
        let path = absolute(root, rel);
        if let Some(entry) = self.docs().find(&path) {
            return Some(entry.document().borrow().text());
        }
        let env = self.docs().env();
        let meta = env.fs.metadata(&path).ok()?;
        if meta.is_dir() || meta.is_placeholder() {
            return None;
        }
        // The sources of a link update are the index's Markdown files (`.md`, `.markdown`,
        // `.mdx`, never a folder: checked above) and the open text documents: all of them are
        // read as text.
        load_file(env.fs.as_ref(), &path, env.tld.as_deref())
            .ok()
            .map(|loaded| loaded.text)
    }

    /// Writes one update: an open document takes it as one transaction (it stays unsaved like
    /// any edit), a closed file is replaced atomically in its own encoding and line endings.
    /// `false` when the file could not be changed: it changed since the text was computed (or
    /// cannot be written back faithfully), so nothing was written.
    fn apply_link_update(&self, root: &Path, update: &LinkUpdate) -> bool {
        let path = absolute(root, &update.path);
        if let Some(entry) = self.docs().find(&path) {
            return replace_text(&mut entry.document().borrow_mut(), &update.text);
        }
        self.rewrite_closed(&path, update).unwrap_or(false)
    }

    /// Replaces the closed file `path` by the new text in its own encoding and line endings.
    fn rewrite_closed(&self, path: &Path, update: &LinkUpdate) -> Result<bool, DocError> {
        let env = self.docs().env();
        let loaded = load_file(env.fs.as_ref(), path, env.tld.as_deref())?;
        if !matches!(loaded.source, Source::Text) || loaded.lossy || loaded.text != update.original
        {
            return Ok(false);
        }
        let bytes = encoding::encode(&update.text, &loaded.format)?;
        save_atomic(env.fs.as_ref(), env.sleeper.as_ref(), path, &bytes)?;
        Ok(true)
    }

    // ----- drag and drop ------------------------------------------------------------------

    /// An entry of the sidebar was dropped on the folder `target_folder`: it moves there (the
    /// links to it follow, on request), or is copied when `copy` (Ctrl held).
    pub fn drop_entry(
        &self,
        cell: &Rc<WindowCell>,
        source: NodeKey,
        target_folder: NodeKey,
        copy: bool,
    ) {
        let (Some(from), Some(folder)) = (
            Self::tree_path(cell, source),
            Self::tree_path(cell, target_folder),
        ) else {
            return;
        };
        let fs = self.docs().env().fs.clone();
        if !fs.metadata(&folder).is_ok_and(|meta| meta.is_dir()) {
            return;
        }
        if copy {
            match fileops::copy_into(fs.as_ref(), &from, &folder) {
                Ok(_) => self.refresh_sidebars(),
                Err(error) => self.report_file_op(&error),
            }
            return;
        }
        let old_canonical = canonical_path(&from);
        let plan = self.plan_link_updates(cell, &from);
        match fileops::move_into(fs.as_ref(), &from, &folder) {
            // Dropped on the folder it is in.
            Ok(new) if new == from => {}
            Ok(new) => {
                self.rebind_open_documents(&old_canonical, &canonical_path(&new));
                self.refresh_sidebars();
                self.offer_link_updates(plan, &new);
            }
            Err(error) => self.report_file_op(&error),
        }
    }

    // ----- wikilinks ----------------------------------------------------------------------

    /// Ctrl+click on `[[name#heading]]` in the window `cell`: opens the note and moves to the
    /// heading; `[[#heading]]` moves to a heading of the current document; a note that does
    /// not exist is offered for creation.
    pub fn activate_wikilink(&self, cell: &Rc<WindowCell>, name: &str, heading: Option<&str>) {
        if name.trim().is_empty() {
            if let Some(heading) = heading {
                Self::go_to_heading(cell, heading);
            }
            return;
        }
        let Some(root) = Self::wikilink_root(cell) else {
            return;
        };
        let files = self.workspace_files(&root);
        match links::resolve_wikilink(&files, name) {
            Some(found) => {
                self.open_path(cell, &absolute(&root, found), false);
                if let Some(heading) = heading {
                    Self::go_to_heading(cell, heading);
                }
            }
            None => self.offer_note(cell, &root, name),
        }
    }

    /// The folder wikilinks of the active document resolve in: its workspace, else the folder
    /// the sidebar shows, else the folder of the file.
    fn wikilink_root(cell: &WindowCell) -> Option<PathBuf> {
        let view = cell.view.borrow();
        let tab = view.active_tab()?;
        tab.workspace
            .clone()
            .or_else(|| workspace_root(cell))
            .or_else(|| {
                tab.entry
                    .path()
                    .and_then(|path| path.parent().map(Path::to_path_buf))
            })
    }

    /// Every file of the workspace `root`: from the index, or (not indexed yet) by walking the
    /// folder.
    fn workspace_files(&self, root: &Path) -> Vec<RelPath> {
        let indexed: Vec<RelPath> = self
            .docs()
            .env()
            .store
            .borrow()
            .index_files(root)
            .unwrap_or_default()
            .iter()
            .filter_map(|file| RelPath::parse(&file.rel).ok())
            .collect();
        if !indexed.is_empty() {
            return indexed;
        }
        let show_hidden = self.settings().sidebar.show_hidden_items;
        list_workspace_files(root, show_hidden)
            .iter()
            .filter_map(|rel| RelPath::parse(rel).ok())
            .collect()
    }

    /// Moves the caret of the active editor to the heading `wanted`.
    fn go_to_heading(cell: &WindowCell, wanted: &str) {
        let mut view = cell.view.borrow_mut();
        let Some(tab) = view.active_tab_mut() else {
            return;
        };
        let toc = toc_of(&tab.entry);
        if let Some(index) = links::find_heading(&toc, wanted) {
            tab.editor
                .go_to(Selection::caret(toc.entries()[index].range.start));
        }
    }

    /// Asks whether to create the note `name` and opens it. Only paths that stay inside `root`
    /// are offered (spec §3.2).
    fn offer_note(&self, cell: &Rc<WindowCell>, root: &Path, name: &str) {
        let i18n = self.i18n();
        let name_arg = [("name", Arg::Str(name.trim()))];
        let target = links::wikilink_new_file(name)
            .ok()
            .map(|rel| absolute(root, &rel))
            .filter(|target| confined(root, target));
        let Some(target) = target else {
            let _ = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
                title: i18n.text("files-wikilink-title"),
                message: i18n.format("files-wikilink-invalid", &name_arg),
                buttons: MessageBoxButtons::Ok,
                icon: Some(MessageBoxIcon::Error),
            });
            return;
        };
        let answer = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: i18n.text("files-wikilink-title"),
            message: i18n.format("files-wikilink-create", &name_arg),
            buttons: MessageBoxButtons::YesNo,
            icon: Some(MessageBoxIcon::Question),
        });
        if !matches!(answer, Ok(MessageBoxResult::Yes)) {
            return;
        }
        let fs = self.docs().env().fs.clone();
        let created = fs
            .create_dir_all(target.parent().unwrap_or(root))
            .and_then(|()| fs.write_new(&target, b""));
        match created {
            // Created meanwhile by someone else: open it all the same.
            Ok(()) | Err(FsError::AlreadyExists(_)) => {
                self.refresh_sidebars();
                self.open_path(cell, &target, false);
            }
            Err(error) => self.report(&DocError::Fs(error)),
        }
    }

    // ----- housekeeping -------------------------------------------------------------------

    /// One turn of housekeeping for the workspaces: results of background work reach the
    /// windows, the watchers (one per workspace root) refresh the sidebars and queue index
    /// updates, the index is built the first time, and `[[` completion is fed. Call it after
    /// [`crate::app::AppCore::pump`], never while the core is borrowed.
    pub fn pump_files(&self) {
        self.forward_deferred();
        let windows = self.windows();
        let mut roots: Vec<PathBuf> = Vec::new();
        for cell in &windows {
            if let Some(root) = workspace_root(cell)
                && !roots.iter().any(|known| path_key(known) == path_key(&root))
            {
                roots.push(root);
            }
        }
        let events = self.watch_roots(&roots);
        for root in &roots {
            let mine: Vec<&FsEvent> = events
                .iter()
                .filter(|event| is_within(root, &event.path))
                .collect();
            self.enqueue_index_changes(root, &mine);
            let showing = windows.iter().filter(|cell| {
                workspace_root(cell).is_some_and(|shown| path_key(&shown) == path_key(root))
            });
            for cell in showing {
                self.ensure_index(cell.id, root);
                if !mine.is_empty() {
                    Self::show_changes(cell, &mine);
                }
            }
            self.flush_index(root);
        }
        for cell in &windows {
            self.feed_completion(cell);
        }
    }

    /// Hands what workers finished to the places that can show it.
    fn forward_deferred(&self) {
        let pending = std::mem::take(&mut *self.files.outbox.borrow_mut());
        for item in pending {
            match item {
                Deferred::Progress(window, progress) => {
                    self.push(Request::TaskbarProgress(window, progress));
                }
                Deferred::Launch(path) => {
                    let _ = self.shell.borrow_mut().open_path(&path);
                }
                Deferred::Report(error) => self.report(&error),
            }
        }
    }

    /// Keeps one watcher per workspace root and returns what happened since the last call.
    fn watch_roots(&self, roots: &[PathBuf]) -> Vec<FsEvent> {
        let mut slot = self.files.watches.borrow_mut();
        let watches = slot.get_or_insert_with(|| {
            let waker: Arc<dyn Waker> = self.core().borrow().waker.clone();
            FolderWatches::new(waker)
        });
        let mut watched = self.files.watched.borrow_mut();
        if watched.as_slice() != roots {
            for (folder, error) in watches.sync(roots) {
                self.core().borrow().logger.log(
                    Level::Warn,
                    "watch",
                    &format!("cannot watch {}: {error}", folder.display()),
                );
            }
            *watched = roots.to_vec();
        }
        watches.drain()
    }

    /// Shows changes on disk in the sidebar of `cell`.
    fn show_changes(cell: &WindowCell, events: &[&FsEvent]) {
        // Only windows that show a workspace get here: `workspace_root` found their tree.
        let tree = cell.tree.borrow();
        let paths = tree
            .as_ref()
            .expect("a window showing a workspace has its tree");
        for event in events {
            paths.queue_event((*event).clone());
        }
        let mut view = cell.view.borrow_mut();
        if let Some(sidebar) = view.sidebar_mut() {
            sidebar.refresh_all();
        }
        view.requests_mut().invalidate_all();
        cell.dirty.set(true);
    }

    /// Remembers what the index of `root` has to learn from `events`.
    fn enqueue_index_changes(&self, root: &Path, events: &[&FsEvent]) {
        if events.is_empty() {
            return;
        }
        let mut queues = self.files.index.borrow_mut();
        let queue = queues.roots.entry(path_key(root)).or_default();
        for event in events {
            let ignore_rules = event
                .path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case(".gitignore"));
            match &event.kind {
                FsEventKind::Rescan => queue.full = true,
                // A changed `.gitignore` changes which files count: the walk decides.
                _ if ignore_rules => queue.full = true,
                FsEventKind::Created | FsEventKind::Modified => {
                    queue.changes.push(IndexChange::Changed(event.path.clone()));
                }
                FsEventKind::Removed => {
                    queue.changes.push(IndexChange::Removed(event.path.clone()))
                }
                FsEventKind::Renamed { from } => {
                    queue.changes.push(IndexChange::Removed(from.clone()));
                    queue.changes.push(IndexChange::Changed(event.path.clone()));
                }
            }
        }
    }

    /// Starts the job that brings the index of `root` up to date unless one is running (it
    /// then runs on the next pump after the running one finished).
    fn flush_index(&self, root: &Path) {
        let key = path_key(root);
        let (full, changes) = {
            let mut queues = self.files.index.borrow_mut();
            let Some(queue) = queues.roots.get_mut(&key) else {
                return;
            };
            if queue.running || (!queue.full && queue.changes.is_empty()) {
                return;
            }
            queue.running = true;
            (
                std::mem::take(&mut queue.full),
                std::mem::take(&mut queue.changes),
            )
        };
        let core = self.core().borrow();
        let database = core.paths.state_db();
        let job_root = root.to_path_buf();
        let spec = JobSpec {
            kind: JobKind::Index,
            window: None,
            key: self.files.next_job(INDEX_JOB_BASE),
            generation: 0,
        };
        let queues = Rc::clone(&self.files.index);
        let epoch = Rc::clone(&self.index_epoch);
        let done_key = key.clone();
        // The job has no window, so the pool refuses it only while it shuts down (the process
        // is leaving): there is no later turn that could use the queue.
        let _ = core.workers.submit(
            spec,
            move |_cancel| {
                let cancel = AtomicBool::new(false);
                let fs = RealFs;
                // OneDrive placeholders are listed, never read (spec §4.3).
                let placeholder =
                    |path: &Path| fs.metadata(path).is_ok_and(|meta| meta.is_placeholder());
                let options = IndexOptions::default();
                let Ok(mut store) = Store::open(&database) else {
                    return false;
                };
                if full {
                    let now = i64::try_from(now_millis() / 1000).unwrap_or(i64::MAX);
                    store
                        .index_workspace(&job_root, &options, &placeholder, &cancel, now)
                        .is_ok()
                } else {
                    store
                        .index_apply_changes(&job_root, &changes, &options, &placeholder)
                        .is_ok()
                }
            },
            move |result| {
                // The queue of the root exists since the job was queued and is never removed.
                queues
                    .borrow_mut()
                    .roots
                    .entry(done_key)
                    .or_default()
                    .running = false;
                if matches!(result, Ok(true)) {
                    epoch.set(epoch.get() + 1);
                }
            },
        );
    }

    /// Feeds the `[[` completion of `cell`: the workspace files whenever they may have changed
    /// and the headings of the active document while the caret is inside a `[[`.
    fn feed_completion(&self, cell: &WindowCell) {
        let (entry, tab_key, root) = {
            let view = cell.view.borrow();
            let Some(tab) = view.active_tab() else {
                return;
            };
            (
                Rc::clone(&tab.entry),
                tab.key,
                tab.workspace.clone().or_else(|| workspace_root(cell)),
            )
        };
        let open = self.docs().all();
        let files_key = root
            .as_ref()
            .map(|root| (path_key(root), self.index_epoch.get(), open.len()));
        let version = entry.document().borrow().version();
        let wants_outline = entry.mode() == DocMode::Markdown && in_wikilink(&entry);
        let mut keys = self.files.completion.borrow_mut();
        let key = keys.entry(cell.id).or_default();
        let feed_files = key.files != files_key;
        let feed_outline = wants_outline && key.outline != Some((tab_key, version));
        if !feed_files && !feed_outline {
            return;
        }
        let mut view = cell.view.borrow_mut();
        let completion = view.overlays_mut().wikilink_mut();
        if feed_files {
            let (files, recent) = match &root {
                Some(root) => completion_files(&self.docs().env().store.borrow(), &open, root),
                None => (Vec::new(), Vec::new()),
            };
            completion.set_files(files, recent);
            key.files = files_key;
        }
        if feed_outline {
            completion.set_outline(&toc_of(&entry));
            key.outline = Some((tab_key, version));
        }
        // The sources came after the keystroke that opened the list: show what they offer.
        if completion.is_visible() {
            view.requests_mut().invalidate_all();
        }
    }

    // ----- placeholders -------------------------------------------------------------------

    /// Opens `path` in the background when it is a OneDrive placeholder: reading it downloads
    /// it, which can take long, so a worker reads it while the taskbar button shows
    /// indeterminate progress, and the tab appears when the file is there. Returns whether the
    /// open is under way; `false` for every other file (the caller opens it directly).
    /// `launch_binary` is set for a file the user only browsed to: when it turns out to be no
    /// text, its default app starts instead of an error showing.
    pub fn begin_hydration(
        &self,
        cell: &Rc<WindowCell>,
        path: &Path,
        launch_binary: bool,
    ) -> bool {
        let env = self.docs().env();
        if !env
            .fs
            .metadata(path)
            .is_ok_and(|meta| !meta.is_dir() && meta.is_placeholder())
        {
            return false;
        }
        let key = path_key(path);
        if !self.files.hydrating.borrow_mut().insert(key.clone()) {
            // Already downloading: the tab appears when that finishes.
            return true;
        }
        let fs = Arc::clone(&env.fs);
        let tld = env.tld.clone();
        let job_path = path.to_path_buf();
        let done_path = path.to_path_buf();
        let docs = Rc::clone(self.docs());
        let target = Rc::clone(cell);
        let hydrating = Rc::clone(&self.files.hydrating);
        let outbox = Rc::clone(&self.files.outbox);
        let done_key = key.clone();
        let window = cell.id;
        let spec = JobSpec {
            kind: JobKind::Io,
            window: Some(window),
            key: self.files.next_job(OPEN_JOB_BASE),
            generation: 0,
        };
        let submitted = self.core().borrow().workers.submit(
            spec,
            move |_cancel| load_file(fs.as_ref(), &job_path, tld.as_deref()),
            move |outcome| {
                hydrating.borrow_mut().remove(&done_key);
                outbox
                    .borrow_mut()
                    .push(Deferred::Progress(window, Progress::None));
                let loaded = match outcome {
                    Ok(Ok(loaded)) => loaded,
                    Ok(Err(LoadError::NotText(_))) if launch_binary => {
                        return outbox.borrow_mut().push(Deferred::Launch(done_path));
                    }
                    Ok(Err(error)) => return report_later(&outbox, error),
                    // The read panicked: all that is known is that the file could not be read.
                    Err(_) => {
                        let error = FsError::Os {
                            code: 0,
                            path: done_path,
                        };
                        return report_later(&outbox, error);
                    }
                };
                match docs.open_loaded(&done_path, loaded) {
                    Ok(entry) => {
                        let mut view = target.view.borrow_mut();
                        let workspace = view.active_tab().and_then(|tab| tab.workspace.clone());
                        view.open_tab(entry, workspace, &docs);
                        target.dirty.set(true);
                    }
                    Err(error) => report_later(&outbox, error),
                }
            },
        );
        if submitted.is_err() {
            self.files.hydrating.borrow_mut().remove(&key);
            return false;
        }
        self.push(Request::TaskbarProgress(window, Progress::Indeterminate));
        true
    }
}

/// A count for a Fluent plural.
fn count(number: usize) -> i64 {
    i64::try_from(number).unwrap_or(i64::MAX)
}
