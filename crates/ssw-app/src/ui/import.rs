//! Opening word-processor files (spec §4.1): `.docx`, `.odt`, `.rtf` and `.doc` are imported to
//! Markdown on a worker thread, so a large file never freezes the window.
//!
//! The import itself is [`crate::documents::load_file`] (sniffing, `ssw_convert` importers, loss
//! report); this module only moves it off the UI thread and brings the result back: the worker
//! returns the [`Loaded`] document, the UI thread (from the worker poll) registers it with
//! [`crate::documents::Documents::open_loaded`] and adds the tab. The tab shows the Markdown
//! derived from the file; the import-loss banner is raised by `Controller::refresh` from the
//! document's loss list, and saving asks for a target (Save as `.md`, or Save back to Word)
//! because a converted document is never written silently over its source.
//!
//! Every way of opening a file (File → Open, the sidebar, drag and drop, the command line,
//! Jump List and forwarded launches) ends in `Controller::open_document`, which calls
//! [`open_import`] for these formats. A failure (unreadable, encrypted, not a document) is
//! reported like any other open failure.

use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::controller::{Controller, WindowCell};
use crate::documents::{ConvertedFormat, DocError, LoadError, Loaded, load_file};
use crate::workers::{JobError, JobKey, JobKind, JobSpec};

/// Source of the job keys: every import is its own kind of result, none supersedes another.
static NEXT_IMPORT: AtomicU64 = AtomicU64::new(1);

/// Whether `path` is a word-processor file that is imported rather than edited as text.
#[must_use]
pub fn is_import(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| ConvertedFormat::from_extension(&extension.to_string_lossy()))
        .is_some()
}

/// Whether two paths name the same file (case-insensitive on Windows, canonical when possible).
fn same_file(a: &Path, b: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(a)
        .to_string_lossy()
        .eq_ignore_ascii_case(&canonical(b).to_string_lossy())
}

/// Starts importing `path` for window `cell`. A file that a tab of the window shows already is
/// just activated. The tab appears when the worker is done.
pub(super) fn open_import(controller: &Controller, cell: &Rc<WindowCell>, path: &Path) {
    let open_here = cell
        .view
        .borrow()
        .tabs()
        .iter()
        .any(|tab| tab.entry.path().is_some_and(|open| same_file(&open, path)));
    if open_here {
        if let Err(error) = controller.open_file(cell, path) {
            controller.report(&error);
        }
        return;
    }
    let env = controller.docs().env();
    let fs = Arc::clone(&env.fs);
    let tld = env.tld.clone();
    let source = path.to_path_buf();
    let target = source.clone();
    let owner = Rc::downgrade(cell);
    let controller_ref = controller.handle();
    let spec = JobSpec {
        kind: JobKind::Export,
        window: Some(cell.id),
        key: JobKey(NEXT_IMPORT.fetch_add(1, Ordering::Relaxed)),
        generation: 1,
    };
    // A refused submission means the window is closing: there is nobody to show the file to.
    let _ = controller.core().borrow().workers.submit(
        spec,
        move |_| load_file(fs.as_ref(), &source, tld.as_deref()),
        move |outcome| finish(&controller_ref, &owner, &target, outcome),
    );
}

/// Delivers a finished import on the UI thread.
fn finish(
    controller: &Weak<Controller>,
    cell: &Weak<WindowCell>,
    path: &Path,
    outcome: Result<Result<Loaded, LoadError>, JobError>,
) {
    let (Some(controller), Some(cell)) = (controller.upgrade(), cell.upgrade()) else {
        return;
    };
    let loaded = match outcome {
        Ok(Ok(loaded)) => loaded,
        Ok(Err(error)) => {
            controller.report(&DocError::Load(error));
            return;
        }
        Err(error) => {
            controller.report(&DocError::Convert(error.to_string()));
            return;
        }
    };
    match controller.docs().open_loaded(path, loaded) {
        Ok(entry) => {
            let workspace = cell
                .view
                .borrow()
                .active_tab()
                .and_then(|tab| tab.workspace.clone());
            cell.view
                .borrow_mut()
                .open_tab(entry, workspace, controller.docs());
            controller.refresh(&cell);
        }
        Err(error) => controller.report(&error),
    }
}
