//! Export, print and share (spec §9).
//!
//! File → Export → *format* asks for the target file (Save dialog: the document's name and
//! folder, or the Documents folder for an untitled buffer), runs the conversion on a worker
//! ([`job`]) with the options of the Export settings, writes atomically, shows taskbar progress
//! while it runs and reports the result with "open" / "show in folder". Print lays the pages
//! out on a worker and prints them through the [`Printing`] seam; Share hands the saved file to
//! the [`Sharing`] seam. The real adapters ([`SystemPrinting`], [`SystemSharing`]) sit over
//! `ssw_win::print` and `ssw_win::share`; tests install recording ones.
//!
//! A worker's completion callback runs inside the application core's pump: nothing here
//! touches [`Controller::core`] from a callback.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use ssw_core::command::CommandId;
use ssw_core::i18n::Arg;
use ssw_core::settings::ExportFormat;
use ssw_render::scene::Scene;
use ssw_ui::services::{
    FileFilter, MessageBoxButtons, MessageBoxIcon, MessageBoxOptions, MessageBoxResult,
    SaveFileOptions, ServiceError,
};
use ssw_ui::shell::export_dialog::ExportOptions;
use ssw_ui::widgets::anim::Motion;
use ssw_win::canvas::{CanvasAssets, ImageStore};
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::SharedDevices;
use ssw_win::print::{PrintSurface, SystemPrintDialogHost, print_with_dialog};
use ssw_win::share::{SharedFile, Sharer};
use ssw_win::taskbar::Progress;
use ssw_win::wic::Wic;
use windows::Win32::Foundation::HWND;

use super::controller::{Controller, Request, WindowCell};
use crate::documents::{DocEntry, canonical_path};
use crate::workers::{JobError, JobKey, JobKind, JobSpec, SubmitError, WindowId};

mod dialog;
mod job;

pub use dialog::ExportHost;
pub use job::{
    DiagramFormat, DiagramJob, ExportFailure, ExportJob, extension, layout_pages, render_options,
    run_diagram, run_export,
};

/// Job keys of exports start here (above the keys of the other jobs).
const EXPORT_KEY_BASE: u64 = 0x5353_5745_0000_0000;

/// The export format a File → Export command stands for.
#[must_use]
pub fn format_of(command: CommandId) -> Option<ExportFormat> {
    Some(match command {
        CommandId::ExportPdf => ExportFormat::Pdf,
        CommandId::ExportPng => ExportFormat::Png,
        CommandId::ExportJpg => ExportFormat::Jpg,
        CommandId::ExportHtml => ExportFormat::Html,
        CommandId::ExportEpub => ExportFormat::Epub,
        CommandId::ExportDocx => ExportFormat::Docx,
        CommandId::ExportOdt => ExportFormat::Odt,
        CommandId::ExportRtf => ExportFormat::Rtf,
        CommandId::ExportTxt => ExportFormat::Txt,
        CommandId::ExportMarkdownWithAttachments => ExportFormat::MarkdownWithAttachments,
        CommandId::ExportTextBundle => ExportFormat::Textbundle,
        _ => return None,
    })
}

/// The Fluent id of the name of `format` (`settings.ftl`).
const fn format_name_id(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Pdf => "settings-opt-format-pdf",
        ExportFormat::Png => "settings-opt-format-png",
        ExportFormat::Jpg => "settings-opt-format-jpg",
        ExportFormat::Html => "settings-opt-format-html",
        ExportFormat::Epub => "settings-opt-format-epub",
        ExportFormat::Docx => "settings-opt-format-docx",
        ExportFormat::Odt => "settings-opt-format-odt",
        ExportFormat::Rtf => "settings-opt-format-rtf",
        ExportFormat::Txt => "settings-opt-format-txt",
        ExportFormat::MarkdownWithAttachments => "settings-opt-format-markdown",
        ExportFormat::Textbundle => "settings-opt-format-textbundle",
    }
}

// ----- seams -------------------------------------------------------------------------------

/// The pages of a document to print.
#[derive(Debug, Clone)]
pub struct PrintJob {
    /// The window the print dialog belongs to (`None` without a native window).
    pub owner: Option<HWND>,
    /// The job name the print queue shows.
    pub name: String,
    /// One vector scene per page (points, white paper included).
    pub pages: Vec<Scene>,
}

/// Shows the print dialog and prints.
pub trait Printing {
    /// Asks the user for printer, pages and copies and prints. `Ok(None)`: the user cancelled;
    /// `Ok(Some(n))`: `n` pages were sent.
    ///
    /// # Errors
    /// The print dialog or the print job failed.
    fn print(&mut self, job: PrintJob) -> Result<Option<usize>, ServiceError>;
}

/// Opens the system share sheet for a file.
pub trait Sharing {
    /// Shares `file` from the window `owner`, with `title` shown in the sheet.
    ///
    /// # Errors
    /// There is no window, or the share sheet could not be opened.
    fn share(&mut self, owner: Option<HWND>, file: &Path, title: &str) -> Result<(), ServiceError>;
}

fn failed(service: &'static str, error: impl ToString) -> ServiceError {
    ServiceError::Failed {
        service,
        message: error.to_string(),
    }
}

/// [`Printing`] over `ssw_win::print`: the system print dialog and `ID2D1PrintControl`.
pub struct SystemPrinting {
    devices: SharedDevices,
    wic: Wic,
    text: DWriteEngine,
}

impl fmt::Debug for SystemPrinting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemPrinting").finish_non_exhaustive()
    }
}

impl SystemPrinting {
    /// Printing with the graphics devices of the application: devices that exist, or the shared
    /// devices of the window loop, which a print started in the first moments of a launch waits
    /// for.
    #[must_use]
    pub fn new(devices: impl Into<SharedDevices>, wic: Wic, text: DWriteEngine) -> Self {
        Self {
            devices: devices.into(),
            wic,
            text,
        }
    }
}

impl Printing for SystemPrinting {
    fn print(&mut self, job: PrintJob) -> Result<Option<usize>, ServiceError> {
        let devices = self.devices.get().map_err(|error| failed("print", error))?;
        let mut assets = CanvasAssets::with_scene_renderer(
            ImageStore::new(self.wic.clone()),
            self.text.clone(),
            self.wic.clone(),
        )
        .map_err(|error| failed("print", error))?;
        let mut surface = PrintSurface {
            devices: &devices,
            wic: &self.wic,
            assets: &mut assets,
        };
        print_with_dialog(
            &mut SystemPrintDialogHost,
            job.owner.unwrap_or_default(),
            &mut surface,
            &job.name,
            job.pages,
        )
        .map_err(|error| failed("print", error))
    }
}

/// [`Sharing`] over `ssw_win::share`: one `DataTransferManager` per window, kept alive because
/// the share sheet asks for the data after `share` has returned.
#[derive(Debug, Default)]
pub struct SystemSharing {
    sharers: HashMap<isize, Sharer>,
}

impl Sharing for SystemSharing {
    fn share(&mut self, owner: Option<HWND>, file: &Path, title: &str) -> Result<(), ServiceError> {
        let hwnd = owner.ok_or(ServiceError::Unavailable { service: "share" })?;
        let key = hwnd.0 as isize;
        let sharer = match self.sharers.entry(key) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Sharer::new(hwnd).map_err(|error| failed("share", error))?)
            }
        };
        sharer
            .share(&SharedFile::new(file).with_title(title))
            .map_err(|error| failed("share", error))
    }
}

// ----- controller state ----------------------------------------------------------------------

/// What the controller keeps for exports: the seams, the job counter and the number of
/// exports running per window (the taskbar progress ends with the last one).
#[derive(Default)]
pub struct ExportState {
    printing: RefCell<Option<Box<dyn Printing>>>,
    sharing: RefCell<Option<Box<dyn Sharing>>>,
    numbers: Cell<u64>,
    running: RefCell<HashMap<WindowId, usize>>,
}

impl fmt::Debug for ExportState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportState")
            .field("running", &self.running.borrow())
            .finish_non_exhaustive()
    }
}

/// The names the messages about one export use.
#[derive(Debug, Clone)]
struct Label {
    /// What was exported (the document title, "Diagram").
    name: String,
    /// The localized format name.
    format: String,
}

/// How a worker's export ended.
type Outcome = Result<Result<Vec<PathBuf>, ExportFailure>, JobError>;

/// `name` made usable as a file name.
fn file_name_safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect()
}

impl Controller {
    /// Installs the printing seam (the real one in the application, a recording one in tests).
    pub fn set_printing(&self, printing: Box<dyn Printing>) {
        *self.export.printing.borrow_mut() = Some(printing);
    }

    /// Installs the sharing seam.
    pub fn set_sharing(&self, sharing: Box<dyn Sharing>) {
        *self.export.sharing.borrow_mut() = Some(sharing);
    }

    fn say(&self, title: &str, message: String, icon: MessageBoxIcon) {
        let _ = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: self.i18n().text(title),
            message,
            buttons: MessageBoxButtons::Ok,
            icon: Some(icon),
        });
    }

    /// The active document and the name its exports get; tells the user when there is none.
    fn export_source(&self, cell: &WindowCell) -> Option<(Rc<DocEntry>, String)> {
        let entry = cell
            .view
            .borrow()
            .active_tab()
            .map(|tab| Rc::clone(&tab.entry));
        let Some(entry) = entry else {
            self.say(
                "export-nothing-title",
                self.i18n().text("export-nothing-message"),
                MessageBoxIcon::Information,
            );
            return None;
        };
        let stem = entry
            .path()
            .and_then(|path| {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| entry.title(self.i18n()));
        Some((entry, file_name_safe(&stem)))
    }

    /// The folder the Save dialog starts in: the document's, or Documents.
    fn export_folder(entry: &DocEntry) -> Option<PathBuf> {
        entry
            .path()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .or_else(|| ssw_io::workspace::known_documents_dir().ok())
    }

    fn choose_export_target(
        &self,
        entry: &DocEntry,
        suggested: &str,
        extensions: &[(String, &str)],
    ) -> Option<PathBuf> {
        let i18n = self.i18n();
        let mut filters: Vec<FileFilter> = extensions
            .iter()
            .map(|(name, extension)| FileFilter {
                name: name.clone(),
                extensions: vec![(*extension).to_owned()],
            })
            .collect();
        filters.push(FileFilter {
            name: i18n.text("export-filter-all"),
            extensions: vec!["*".to_owned()],
        });
        let default = extensions
            .first()
            .map(|(_, extension)| (*extension).to_owned());
        let options = SaveFileOptions {
            title: i18n.text("export-save-title"),
            filters,
            suggested_name: Some(format!(
                "{suggested}.{}",
                default.as_deref().unwrap_or_default()
            )),
            default_extension: default,
            initial_dir: Self::export_folder(entry),
        };
        self.dialogs.borrow_mut().save_file(&options).ok().flatten()
    }

    /// File → Export…: opens the export dialog over the window, preset from the Export
    /// settings; its Export button ends in [`Controller::export_document_with`].
    pub(super) fn open_export_dialog(&self, cell: &Rc<WindowCell>) {
        if self.export_source(cell).is_none() {
            return;
        }
        {
            let settings = self.settings();
            cell.view
                .borrow_mut()
                .overlays_mut()
                .open_export(&settings.export, Motion::default());
        }
        cell.view.borrow_mut().requests_mut().invalidate_all();
        cell.dirty.set(true);
    }

    /// File → Export → `format`: Save dialog, then the export on a worker, with the options of
    /// the Export settings.
    pub(super) fn export_document(&self, cell: &Rc<WindowCell>, format: ExportFormat) {
        let options = ExportOptions::from(&self.settings().export);
        self.export_document_with(cell, format, options);
    }

    /// Exports the active document as `format` with `options`: Save dialog, then the export
    /// on a worker.
    pub(super) fn export_document_with(
        &self,
        cell: &Rc<WindowCell>,
        format: ExportFormat,
        options: ExportOptions,
    ) {
        let Some((entry, stem)) = self.export_source(cell) else {
            return;
        };
        let i18n = self.i18n();
        let mut kinds = vec![(i18n.text(format_name_id(format)), extension(format))];
        if format == ExportFormat::Textbundle {
            kinds = vec![
                (i18n.text("export-filter-textpack"), "textpack"),
                (i18n.text("export-filter-textbundle"), "textbundle"),
            ];
        }
        let Some(target) = self.choose_export_target(&entry, &stem, &kinds) else {
            return;
        };
        let label = Label {
            name: stem.clone(),
            format: i18n.text(format_name_id(format)),
        };
        if entry
            .path()
            .is_some_and(|path| same_path(&canonical_path(&path), &canonical_path(&target)))
        {
            let reason = i18n.text("export-reason-same-file");
            self.report_failure(&label, &reason);
            return;
        }
        let job = ExportJob {
            format,
            options,
            markdown: entry.document().borrow().text(),
            source_dir: entry
                .path()
                .and_then(|path| path.parent().map(Path::to_path_buf)),
            title: stem,
            target,
        };
        let env = self.docs().env();
        let (fs, sleeper) = (Arc::clone(&env.fs), Arc::clone(&env.sleeper));
        self.run_export_job(cell, label, move || {
            run_export(&job, fs.as_ref(), sleeper.as_ref())
        });
    }

    /// Mermaid menu → Export as PNG / SVG: Save dialog, then the picture on a worker.
    pub(super) fn export_diagram(
        &self,
        cell: &Rc<WindowCell>,
        source: String,
        format: DiagramFormat,
    ) {
        let Some((entry, _)) = self.export_source(cell) else {
            return;
        };
        let i18n = self.i18n();
        let format_name = i18n.text(match format {
            DiagramFormat::Png => "export-diagram-png",
            DiagramFormat::Svg => "export-diagram-svg",
        });
        let name = i18n.text("export-mermaid-title");
        let kinds = [(format_name.clone(), format.extension())];
        let Some(target) = self.choose_export_target(&entry, &name, &kinds) else {
            return;
        };
        let label = Label {
            name,
            format: format_name,
        };
        let job = DiagramJob {
            source,
            format,
            target,
        };
        let env = self.docs().env();
        let (fs, sleeper) = (Arc::clone(&env.fs), Arc::clone(&env.sleeper));
        self.run_export_job(cell, label, move || {
            run_diagram(&job, fs.as_ref(), sleeper.as_ref())
        });
    }

    fn run_export_job<F>(&self, cell: &Rc<WindowCell>, label: Label, work: F)
    where
        F: FnOnce() -> Result<Vec<PathBuf>, ExportFailure> + Send + 'static,
    {
        let done_label = label.clone();
        let submitted =
            self.submit_export(cell, JobKind::Export, work, move |controller, outcome| {
                controller.report_export(&done_label, outcome);
            });
        if let Err(error) = submitted {
            let reason = self.i18n().format(
                "export-reason-busy",
                &[("reason", Arg::Str(&error.to_string()))],
            );
            self.report_failure(&label, &reason);
        }
    }

    /// Runs `work` on a worker with taskbar progress; `done` runs on the UI thread.
    fn submit_export<T, F, D>(
        &self,
        cell: &Rc<WindowCell>,
        kind: JobKind,
        work: F,
        done: D,
    ) -> Result<(), SubmitError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(&Controller, Result<T, JobError>) + 'static,
    {
        let id = cell.id;
        let number = self.export.numbers.get() + 1;
        self.export.numbers.set(number);
        let spec = JobSpec {
            kind,
            window: Some(id),
            key: JobKey(EXPORT_KEY_BASE + number),
            generation: number,
        };
        let weak = self.handle();
        self.export_begins(id);
        let submitted = self.core().borrow().workers.submit(
            spec,
            move |_| work(),
            move |outcome| {
                let Some(controller) = weak.upgrade() else {
                    return;
                };
                controller.export_ends(id);
                done(&controller, outcome);
            },
        );
        match submitted {
            Ok(_) => Ok(()),
            Err(error) => {
                self.export_ends(id);
                Err(error)
            }
        }
    }

    fn export_begins(&self, id: WindowId) {
        *self.export.running.borrow_mut().entry(id).or_default() += 1;
        self.push(Request::TaskbarProgress(id, Progress::Indeterminate));
    }

    fn export_ends(&self, id: WindowId) {
        let last = {
            let mut running = self.export.running.borrow_mut();
            let left = running.get_mut(&id).map_or(0, |count| {
                *count = count.saturating_sub(1);
                *count
            });
            if left == 0 {
                running.remove(&id);
            }
            left == 0
        };
        if last {
            self.push(Request::TaskbarProgress(id, Progress::None));
        }
    }

    fn report_export(&self, label: &Label, outcome: Outcome) {
        let i18n = self.i18n();
        match outcome {
            Ok(Ok(files)) => self.report_done(label, &files),
            Ok(Err(failure)) => {
                let reason = self.failure_reason(&failure);
                self.report_failure(label, &reason);
            }
            Err(_) => self.report_failure(label, &i18n.text("export-reason-crashed")),
        }
    }

    fn failure_reason(&self, failure: &ExportFailure) -> String {
        let i18n = self.i18n();
        match failure {
            ExportFailure::Convert(message) => message.clone(),
            ExportFailure::Empty => i18n.text("export-reason-empty"),
            ExportFailure::Diagram => i18n.text("export-mermaid-failed"),
            ExportFailure::Write { path, reason_id } => i18n.format(
                "export-failed-write",
                &[
                    ("path", Arg::Str(&path.to_string_lossy())),
                    ("reason", Arg::Str(&i18n.text(reason_id))),
                ],
            ),
        }
    }

    fn report_failure(&self, label: &Label, reason: &str) {
        let message = self.i18n().format(
            "export-failed-message",
            &[
                ("name", Arg::Str(&label.name)),
                ("format", Arg::Str(&label.format)),
                ("reason", Arg::Str(reason)),
            ],
        );
        self.say("export-failed-title", message, MessageBoxIcon::Error);
    }

    /// Tells the user where the export went; Yes opens the file, No shows it in its folder.
    fn report_done(&self, label: &Label, files: &[PathBuf]) {
        let Some(first) = files.first() else {
            return;
        };
        let i18n = self.i18n();
        let summary = if files.len() == 1 {
            i18n.format(
                "export-done-one",
                &[
                    ("name", Arg::Str(&label.name)),
                    ("path", Arg::Str(&first.to_string_lossy())),
                ],
            )
        } else {
            let folder = first.parent().unwrap_or(first);
            i18n.format(
                "export-done-many",
                &[
                    (
                        "count",
                        Arg::Int(i64::try_from(files.len()).unwrap_or(i64::MAX)),
                    ),
                    ("name", Arg::Str(&label.name)),
                    ("path", Arg::Str(&folder.to_string_lossy())),
                ],
            )
        };
        let answer = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: i18n.text("export-done-title"),
            message: format!("{summary}\n\n{}", i18n.text("export-done-question")),
            buttons: MessageBoxButtons::YesNoCancel,
            icon: Some(MessageBoxIcon::Information),
        });
        match answer {
            Ok(MessageBoxResult::Yes) => {
                let _ = self.shell.borrow_mut().open_path(first);
            }
            Ok(MessageBoxResult::No) => {
                let _ = self.shell.borrow_mut().reveal_in_explorer(first);
            }
            _ => {}
        }
    }

    // ----- print and share ---------------------------------------------------------------

    /// File → Print: the pages are laid out on a worker, then the print dialog opens.
    pub(super) fn print_document(&self, cell: &Rc<WindowCell>) {
        let Some((entry, stem)) = self.export_source(cell) else {
            return;
        };
        let i18n = self.i18n();
        if self.export.printing.borrow().is_none() {
            self.say(
                "export-print-title",
                i18n.text("export-print-unavailable"),
                MessageBoxIcon::Information,
            );
            return;
        }
        let markdown = entry.document().borrow().text();
        let source_dir = entry
            .path()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        // Paper is white: printing never uses the dark export theme.
        let options = ExportOptions {
            light_theme: true,
            ..ExportOptions::from(&self.settings().export)
        };
        let owner = cell.hwnd.get();
        let title = stem.clone();
        let submitted = self.submit_export(
            cell,
            JobKind::Render,
            move || layout_pages(&markdown, source_dir.as_deref(), &options, &title),
            move |controller, outcome| controller.finish_print(owner, &stem, outcome),
        );
        if let Err(error) = submitted {
            self.say_print_failed(&entry.title(i18n), &error.to_string());
        }
    }

    fn say_print_failed(&self, name: &str, reason: &str) {
        let message = self.i18n().format(
            "export-print-failed",
            &[("name", Arg::Str(name)), ("reason", Arg::Str(reason))],
        );
        self.say("export-print-title", message, MessageBoxIcon::Error);
    }

    fn finish_print(
        &self,
        owner: Option<HWND>,
        name: &str,
        outcome: Result<Result<Vec<Scene>, ExportFailure>, JobError>,
    ) {
        let pages = match outcome {
            Ok(Ok(pages)) if !pages.is_empty() => pages,
            Ok(Ok(_)) => {
                self.say_print_failed(name, &self.failure_reason(&ExportFailure::Empty));
                return;
            }
            Ok(Err(failure)) => {
                self.say_print_failed(name, &self.failure_reason(&failure));
                return;
            }
            Err(_) => {
                self.say_print_failed(name, &self.i18n().text("export-reason-crashed"));
                return;
            }
        };
        // The seam is out of its cell while the modal print dialog runs, so a nested request
        // cannot borrow it again.
        let Some(mut printing) = self.export.printing.borrow_mut().take() else {
            self.say(
                "export-print-title",
                self.i18n().text("export-print-unavailable"),
                MessageBoxIcon::Information,
            );
            return;
        };
        let result = printing.print(PrintJob {
            owner,
            name: name.to_owned(),
            pages,
        });
        *self.export.printing.borrow_mut() = Some(printing);
        if let Err(error) = result {
            self.say_print_failed(name, &error.to_string());
        }
    }

    /// File → Share: shares the document's file (saved first when it has unsaved changes).
    pub(super) fn share_document(&self, cell: &Rc<WindowCell>) {
        let Some((entry, _)) = self.export_source(cell) else {
            return;
        };
        let i18n = self.i18n();
        let Some(path) = entry.path() else {
            self.say(
                "export-share-title",
                i18n.text("export-share-unsaved"),
                MessageBoxIcon::Information,
            );
            return;
        };
        if entry.is_dirty() {
            self.run_command(cell.id, CommandId::FileSave);
        }
        let title = entry.title(i18n);
        let Some(mut sharing) = self.export.sharing.borrow_mut().take() else {
            self.say(
                "export-share-title",
                i18n.text("export-share-unavailable"),
                MessageBoxIcon::Information,
            );
            return;
        };
        let result = sharing.share(cell.hwnd.get(), &path, &title);
        *self.export.sharing.borrow_mut() = Some(sharing);
        if let Err(error) = result {
            let message = i18n.format(
                "export-share-failed",
                &[
                    ("name", Arg::Str(&title)),
                    ("reason", Arg::Str(&error.to_string())),
                ],
            );
            self.say("export-share-title", message, MessageBoxIcon::Error);
        }
    }
}

/// Whether two canonical paths name the same file (case-insensitive on Windows).
fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use ssw_io::paths::Environment;
    use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
    use ssw_ui::widgets::SharedClipboard;
    use ssw_win::gfx::Driver;
    use ssw_win::window::OsVersion;

    use super::*;
    use crate::app::AppCore;
    use crate::args::Args;
    use crate::startup::{Boot, ProcessEnv, boot};
    use crate::ui::scripted_services::{DialogRecord, ScriptedServices};
    use crate::workers::DeferredWaker;

    /// A controller over a fresh profile below `root` with scripted dialogs.
    fn controller(root: &Path, services: &ScriptedServices) -> Rc<Controller> {
        let process = ProcessEnv {
            os: OsVersion {
                major: 10,
                build: 22621,
            },
            env: Environment {
                local_app_data: Some(root.join("local")),
                app_data: Some(root.join("roaming")),
                exe_dir: root.join("exe"),
            },
            os_language: "en-US".to_owned(),
            driver: Driver::Warp,
            install_crash_handler: false,
        };
        let mut dialogs = ScriptedServices::new().dialogs();
        let core = match boot(&Args::default(), process, &mut *dialogs, None).unwrap() {
            Boot::Ready(started) => AppCore::new(*started, Arc::new(DeferredWaker::new())),
            Boot::Exit(code) => panic!("startup exited with {code}"),
        };
        let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let plan = core.plan.clone();
        let controller =
            Controller::new(core, services.dialogs(), services.shell(), clipboard, clock);
        controller.open_plan(&plan);
        controller
    }

    fn label() -> Label {
        Label {
            name: "notes".to_owned(),
            format: "PDF".to_owned(),
        }
    }

    /// The (title, message) pairs of the dialogs shown so far.
    fn shown(services: &ScriptedServices) -> Vec<(String, String)> {
        services
            .dialog_log()
            .into_iter()
            .map(|DialogRecord { title, message, .. }| (title, message))
            .collect()
    }

    #[test]
    fn file_names_lose_the_characters_windows_forbids() {
        assert_eq!(
            file_name_safe("a<b>c:d\"e/f\\g|h?i*j\tk"),
            "a_b_c_d_e_f_g_h_i_j_k"
        );
        assert_eq!(file_name_safe("Meeting notes 2026"), "Meeting notes 2026");
    }

    #[test]
    fn an_export_that_ended_without_a_file_to_name_says_why() {
        let tmp = tempfile::tempdir().unwrap();
        let services = ScriptedServices::new();
        let controller = controller(tmp.path(), &services);
        let failures = [
            (
                Err(JobError::Panicked("boom".to_owned())),
                "The exporter stopped unexpectedly.",
            ),
            (
                Ok(Err(ExportFailure::Empty)),
                "The export produced no pages.",
            ),
            (
                Ok(Err(ExportFailure::Diagram)),
                "The diagram could not be drawn",
            ),
            (
                Ok(Err(ExportFailure::Convert("the layout broke".to_owned()))),
                "the layout broke",
            ),
        ];
        for (outcome, reason) in failures {
            controller.report_export(&label(), outcome);
            let dialogs = shown(&services);
            let (title, message) = dialogs.last().unwrap();
            assert_eq!(title, "Export failed");
            assert!(message.contains("could not be exported as"), "{message}");
            assert!(message.contains(reason), "{message}");
        }
        // Finished with no file at all: there is nothing to open, so nothing is asked.
        let before = shown(&services).len();
        controller.report_export(&label(), Ok(Ok(Vec::new())));
        assert_eq!(shown(&services).len(), before);
    }

    #[test]
    fn a_print_that_ended_without_pages_says_why() {
        let tmp = tempfile::tempdir().unwrap();
        let services = ScriptedServices::new();
        let controller = controller(tmp.path(), &services);
        let outcomes = [
            (Ok(Ok(Vec::new())), "The export produced no pages."),
            (
                Ok(Err(ExportFailure::Convert("the layout broke".to_owned()))),
                "the layout broke",
            ),
            (
                Err(JobError::Panicked("boom".to_owned())),
                "The exporter stopped unexpectedly.",
            ),
        ];
        for (outcome, reason) in outcomes {
            controller.finish_print(None, "notes", outcome);
            let dialogs = shown(&services);
            let (title, message) = dialogs.last().unwrap();
            assert_eq!(title, "Print");
            assert!(message.contains("could not be printed"), "{message}");
            assert!(message.contains(reason), "{message}");
        }
        // Pages but no printer left to take them.
        controller.finish_print(None, "notes", Ok(Ok(vec![Scene::default()])));
        let dialogs = shown(&services);
        assert_eq!(
            dialogs.last().unwrap(),
            &(
                "Print".to_owned(),
                "Printing is not available in this window.".to_owned()
            )
        );
        assert_eq!(dialogs.len(), 4);
    }
}
