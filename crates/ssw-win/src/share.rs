//! Sharing a file through the Windows share sheet (spec §9): the window's
//! `DataTransferManager` (`IDataTransferManagerInterop::GetForWindow`), a `DataRequested`
//! handler that hands the file over as a `StorageFile` in a `DataPackage` with a title, and
//! `ShowShareUIForWindow` behind the [`ShareUi`] seam.
//!
//! [`Sharer::share`] resolves the file to a `StorageFile` first (a missing file is an error
//! before any sheet opens), puts it in the [`ShareSlot`] the handler reads and shows the
//! sheet. When the system asks for the data, the handler calls [`ShareSlot::serve`], which fills
//! the request's `DataPackage` with [`PendingShare::fill`]: the file as a read-only storage item,
//! the title, and the copy operation.
//!
//! The thread needs COM initialized ([`crate::com::ComGuard`]); drop a [`Sharer`] before the
//! guard.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use windows::ApplicationModel::DataTransfer::{
    DataPackage, DataPackageOperation, DataRequest, DataRequestedEventArgs, DataTransferManager,
};
use windows::Foundation::TypedEventHandler;
use windows::Storage::{IStorageItem, StorageFile};
use windows::Win32::Foundation::{E_FAIL, HWND};
use windows::Win32::UI::Shell::IDataTransferManagerInterop;
use windows::core::{AgileReference, Error, HSTRING, Interface, factory};
use windows_collections::IIterable;

use crate::com::{OsResultExt, WinError};

/// A file to share and the title the share sheet shows for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFile {
    /// The absolute path of the file.
    pub path: PathBuf,
    /// The title of the shared item; the file name when blank.
    pub title: String,
}

impl SharedFile {
    /// Shares the file at `path`, titled by its file name.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let title = file_name(&path);
        Self { path, title }
    }

    /// The same file under another title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }
}

/// The last component of `path` (the whole path when it has none, as for `C:\`).
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// A shared file resolved to a `StorageFile`, ready to fill a `DataPackage` on any thread.
#[derive(Debug, Clone)]
pub struct PendingShare {
    file: AgileReference<StorageFile>,
    title: String,
}

impl PendingShare {
    /// Resolves `file` with `StorageFile::GetFileFromPathAsync`, waiting for the result.
    ///
    /// # Errors
    /// The path is not absolute, the file does not exist or it cannot be opened.
    pub fn resolve(file: &SharedFile) -> Result<Self, WinError> {
        let path = HSTRING::from(file.path.as_os_str());
        let operation = StorageFile::GetFileFromPathAsync(&path).ctx("GetFileFromPathAsync")?;
        // The operation completes on a thread-pool thread, so waiting here cannot deadlock.
        let storage = operation.join().ctx("GetFileFromPathAsync result")?;
        let file_ref = AgileReference::new(&storage).ctx("AgileReference::new")?;
        let title = if file.title.trim().is_empty() {
            file_name(&file.path)
        } else {
            file.title.clone()
        };
        Ok(Self {
            file: file_ref,
            title,
        })
    }

    /// The title the package gets.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Puts the file into `package` as its only (read-only) storage item, with the title, and
    /// asks for a copy.
    ///
    /// # Errors
    /// The package refuses the data.
    pub fn fill(&self, package: &DataPackage) -> Result<(), WinError> {
        let storage = self.file.resolve().ctx("AgileReference::resolve")?;
        package
            .SetRequestedOperation(DataPackageOperation::Copy)
            .ctx("SetRequestedOperation")?;
        package
            .Properties()
            .ctx("DataPackage::Properties")?
            .SetTitle(&HSTRING::from(self.title.as_str()))
            .ctx("SetTitle")?;
        let item = storage.cast::<IStorageItem>().ctx("IStorageItem")?;
        let items = IIterable::<IStorageItem>::from(vec![Some(item)]);
        package.SetStorageItems(&items, true).ctx("SetStorageItems")
    }

    /// A new package holding the file ([`PendingShare::fill`]).
    ///
    /// # Errors
    /// The package cannot be created or refuses the data.
    pub fn build(&self) -> Result<DataPackage, WinError> {
        let package = DataPackage::new().ctx("DataPackage::new")?;
        self.fill(&package)?;
        Ok(package)
    }
}

/// What the share sheet asked for data with: the package to fill.
pub trait DataRequestTarget {
    /// The package the data goes into.
    ///
    /// # Errors
    /// The request has no package (it was already answered).
    fn package(&self) -> Result<DataPackage, WinError>;
}

impl DataRequestTarget for DataRequest {
    fn package(&self) -> Result<DataPackage, WinError> {
        self.Data().ctx("DataRequest::Data")
    }
}

/// The file being shared, shared between [`Sharer::share`] and the `DataRequested` handler (which
/// may run on another thread).
#[derive(Debug, Clone, Default)]
pub struct ShareSlot {
    pending: Arc<Mutex<Option<PendingShare>>>,
}

impl ShareSlot {
    /// Makes `share` the file the next data requests get.
    pub fn set(&self, share: PendingShare) {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(share);
    }

    /// Answers a data request: fills the package of `target` with the file of the last
    /// [`ShareSlot::set`].
    ///
    /// # Errors
    /// Nothing is being shared, or the package refuses the data.
    pub fn serve(&self, target: &impl DataRequestTarget) -> Result<(), WinError> {
        let share = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or(WinError::Invalid("nothing is being shared"))?;
        share.fill(&target.package()?)
    }
}

/// The call that opens the share sheet.
pub trait ShareUi {
    /// Shows the share sheet for `window`.
    ///
    /// # Errors
    /// The system refuses to show the sheet.
    fn show(&self, window: HWND) -> Result<(), WinError>;
}

/// The `IDataTransferManagerInterop` factory of the system.
fn interop() -> Result<IDataTransferManagerInterop, WinError> {
    factory::<DataTransferManager, IDataTransferManagerInterop>().ctx("IDataTransferManagerInterop")
}

/// The real [`ShareUi`]: `ShowShareUIForWindow`. The sheet is modeless and system-drawn.
#[derive(Debug)]
pub struct SystemShareUi {
    interop: IDataTransferManagerInterop,
}

impl SystemShareUi {
    /// Loads the interop factory.
    ///
    /// # Errors
    /// The share sheet is not available (Windows before 10, or COM is not initialized).
    pub fn new() -> Result<Self, WinError> {
        Ok(Self {
            interop: interop()?,
        })
    }
}

impl ShareUi for SystemShareUi {
    fn show(&self, window: HWND) -> Result<(), WinError> {
        // SAFETY: `window` is a window handle of this thread; the call only reads it.
        unsafe { self.interop.ShowShareUIForWindow(window) }.ctx("ShowShareUIForWindow")
    }
}

/// Shares files from one window: the window's `DataTransferManager` with a `DataRequested`
/// handler, and a [`ShareUi`] that opens the sheet. The handler is removed on drop.
#[derive(Debug)]
pub struct Sharer<U: ShareUi = SystemShareUi> {
    window: HWND,
    manager: DataTransferManager,
    token: i64,
    slot: ShareSlot,
    ui: U,
}

impl Sharer {
    /// A sharer for `window` that opens the system share sheet.
    ///
    /// # Errors
    /// The share sheet is not available.
    pub fn new(window: HWND) -> Result<Self, WinError> {
        Self::with_ui(window, SystemShareUi::new()?)
    }
}

impl<U: ShareUi> Sharer<U> {
    /// A sharer for `window` that opens the sheet through `ui`.
    ///
    /// # Errors
    /// The window has no `DataTransferManager`.
    pub fn with_ui(window: HWND, ui: U) -> Result<Self, WinError> {
        let interop = interop()?;
        // SAFETY: `window` is a window handle of this thread; the call only reads it.
        let manager = unsafe { interop.GetForWindow::<DataTransferManager>(window) }
            .ctx("IDataTransferManagerInterop::GetForWindow")?;
        let slot = ShareSlot::default();
        let served = slot.clone();
        let handler = TypedEventHandler::<DataTransferManager, DataRequestedEventArgs>::new(
            move |_, args| {
                let request = args.ok()?.Request()?;
                served.serve(&request).map_err(|_| Error::from(E_FAIL))
            },
        );
        let token = manager.DataRequested(&handler).ctx("DataRequested")?;
        Ok(Self {
            window,
            manager,
            token,
            slot,
            ui,
        })
    }

    /// The file the next data request of the system gets.
    #[must_use]
    pub fn slot(&self) -> &ShareSlot {
        &self.slot
    }

    /// Shares `file`: resolves it, then opens the share sheet. Nothing opens when the file
    /// cannot be resolved.
    ///
    /// # Errors
    /// The file cannot be resolved or the sheet cannot be shown.
    pub fn share(&self, file: &SharedFile) -> Result<(), WinError> {
        self.slot.set(PendingShare::resolve(file)?);
        self.ui.show(self.window)
    }
}

impl<U: ShareUi> Drop for Sharer<U> {
    fn drop(&mut self) {
        // The registration dies with the manager anyway; a failure leaves nothing to report to.
        let _ = self.manager.RemoveDataRequested(self.token);
    }
}
