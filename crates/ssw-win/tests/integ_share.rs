//! Sharing: the `DataPackage` built for a real temporary file (title, operation, storage item
//! read back through the package's view), the data request answered through the seam the
//! `DataRequested` handler uses (also from another apartment), and the sharer driven with a
//! recording sheet. The real share sheet is never shown.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use ssw_win::com::{ComGuard, WinError};
use ssw_win::share::{
    DataRequestTarget, PendingShare, ShareSlot, ShareUi, SharedFile, Sharer, SystemShareUi,
};
use windows::ApplicationModel::DataTransfer::{
    DataPackage, DataPackageOperation, StandardDataFormats,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::w;

/// A temporary folder holding `notes.md`; removed on drop.
struct TempFile {
    dir: PathBuf,
    path: PathBuf,
}

impl TempFile {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ssw-win-share-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, "# shared\n").unwrap();
        Self { dir, path }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A top-level window that is never shown: the sharer needs a window handle.
struct HiddenWindow(HWND);

impl HiddenWindow {
    fn new() -> Self {
        // SAFETY: creates a window of the system `STATIC` class without `WS_VISIBLE`.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("ssw-win share test"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        Self(hwnd)
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        // SAFETY: destroys the window created in `new` on its own thread.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// What a package offers, read back through its view.
#[derive(Debug, PartialEq)]
struct Inspected {
    title: String,
    operation: DataPackageOperation,
    /// (name, canonical path) of every storage item.
    items: Vec<(String, String)>,
}

/// The path as the file system spells it, for comparing what the system reports.
fn canonical(path: &Path) -> String {
    let full = std::fs::canonicalize(path).unwrap();
    full.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_lowercase()
}

fn inspect(package: &DataPackage) -> Inspected {
    let view = package.GetView().unwrap();
    assert!(
        view.Contains(&StandardDataFormats::StorageItems().unwrap())
            .unwrap()
    );
    let items = view.GetStorageItemsAsync().unwrap().join().unwrap();
    let items = (0..items.Size().unwrap())
        .map(|index| {
            let item = items.GetAt(index).unwrap();
            (
                item.Name().unwrap().to_string(),
                canonical(Path::new(&item.Path().unwrap().to_string())),
            )
        })
        .collect();
    Inspected {
        title: view.Properties().unwrap().Title().unwrap().to_string(),
        operation: view.RequestedOperation().unwrap(),
        items,
    }
}

fn expected(title: &str, file: &TempFile, name: &str) -> Inspected {
    Inspected {
        title: title.to_owned(),
        operation: DataPackageOperation::Copy,
        items: vec![(name.to_owned(), canonical(&file.path))],
    }
}

/// A data request that hands out a package of its own.
struct Request {
    package: DataPackage,
}

impl Request {
    fn new() -> Self {
        Self {
            package: DataPackage::new().unwrap(),
        }
    }
}

impl DataRequestTarget for Request {
    fn package(&self) -> Result<DataPackage, WinError> {
        Ok(self.package.clone())
    }
}

/// A data request that was already answered.
struct AnsweredRequest;

impl DataRequestTarget for AnsweredRequest {
    fn package(&self) -> Result<DataPackage, WinError> {
        Err(WinError::Invalid("already answered"))
    }
}

/// A share sheet that records where it would have opened.
struct RecordingSheet {
    shown: Rc<RefCell<Vec<HWND>>>,
    fail: bool,
}

impl ShareUi for RecordingSheet {
    fn show(&self, window: HWND) -> Result<(), WinError> {
        self.shown.borrow_mut().push(window);
        if self.fail {
            Err(WinError::Invalid("no sheet"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn a_shared_file_is_titled_by_its_name_unless_told_otherwise() {
    let file = SharedFile::new(r"C:\notes\chapter.md");
    assert_eq!(file.path, PathBuf::from(r"C:\notes\chapter.md"));
    assert_eq!(file.title, "chapter.md");
    assert_eq!(file.clone().with_title("Chapter one").title, "Chapter one");
    // A path without a file name is its own title.
    assert_eq!(SharedFile::new(r"C:\").title, r"C:\");
}

#[test]
fn the_package_of_a_file_carries_the_title_the_operation_and_the_storage_item() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let temp = TempFile::new("notes.md");
    let pending =
        PendingShare::resolve(&SharedFile::new(&temp.path).with_title("My notes")).unwrap();
    assert_eq!(pending.title(), "My notes");
    let package = pending.build().unwrap();
    assert_eq!(inspect(&package), expected("My notes", &temp, "notes.md"));

    // A blank title falls back to the file name.
    let blank = PendingShare::resolve(&SharedFile::new(&temp.path).with_title("  ")).unwrap();
    assert_eq!(blank.title(), "notes.md");
    assert_eq!(
        inspect(&blank.build().unwrap()),
        expected("notes.md", &temp, "notes.md")
    );

    // Filling an existing package replaces nothing it should keep: the same contents again.
    let existing = DataPackage::new().unwrap();
    pending.fill(&existing).unwrap();
    assert_eq!(inspect(&existing), expected("My notes", &temp, "notes.md"));
}

#[test]
fn files_that_cannot_be_resolved_are_errors_before_any_sheet_opens() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let temp = TempFile::new("notes.md");

    let missing = PendingShare::resolve(&SharedFile::new(temp.dir.join("missing.md")));
    let Err(WinError::Os { source, .. }) = missing else {
        panic!("a missing file must fail to resolve: {missing:?}");
    };
    // ERROR_FILE_NOT_FOUND or ERROR_PATH_NOT_FOUND.
    assert!(
        [0x8007_0002_u32, 0x8007_0003].contains(&(source.code().0 as u32)),
        "{source}"
    );

    let relative = PendingShare::resolve(&SharedFile::new(r"relative\notes.md"));
    assert!(
        matches!(relative, Err(WinError::Os { .. })),
        "a relative path must fail to resolve: {relative:?}"
    );
}

#[test]
fn a_data_request_gets_the_file_of_the_last_share() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let first = TempFile::new("first.md");
    let second = TempFile::new("second.md");
    let slot = ShareSlot::default();

    // Nothing shared yet.
    let error = slot.serve(&Request::new()).unwrap_err();
    assert!(matches!(error, WinError::Invalid(_)), "{error:?}");

    slot.set(PendingShare::resolve(&SharedFile::new(&first.path)).unwrap());
    let request = Request::new();
    slot.serve(&request).unwrap();
    assert_eq!(
        inspect(&request.package),
        expected("first.md", &first, "first.md")
    );

    // A clone is the same slot (the handler holds one), and the next share replaces the file.
    let handler_side = slot.clone();
    slot.set(PendingShare::resolve(&SharedFile::new(&second.path).with_title("Second")).unwrap());
    let request = Request::new();
    handler_side.serve(&request).unwrap();
    assert_eq!(
        inspect(&request.package),
        expected("Second", &second, "second.md")
    );

    // A request that has no package fails with its own error.
    let error = slot.serve(&AnsweredRequest).unwrap_err();
    assert!(
        matches!(error, WinError::Invalid("already answered")),
        "{error:?}"
    );
}

#[test]
fn a_data_request_on_another_thread_resolves_the_file_across_apartments() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let temp = TempFile::new("notes.md");
    let slot = ShareSlot::default();
    slot.set(PendingShare::resolve(&SharedFile::new(&temp.path)).unwrap());
    let from_handler_thread = slot.clone();
    let seen = std::thread::spawn(move || {
        let _com = ComGuard::multi_threaded().unwrap();
        let request = Request::new();
        from_handler_thread.serve(&request).unwrap();
        inspect(&request.package)
    })
    .join()
    .unwrap();
    assert_eq!(seen, expected("notes.md", &temp, "notes.md"));
}

#[test]
fn the_sharer_prepares_the_file_then_opens_the_sheet_through_the_seam() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let window = HiddenWindow::new();
    let temp = TempFile::new("notes.md");
    let shown = Rc::new(RefCell::new(Vec::new()));
    let sharer = Sharer::with_ui(
        window.0,
        RecordingSheet {
            shown: shown.clone(),
            fail: false,
        },
    )
    .unwrap();

    sharer
        .share(&SharedFile::new(&temp.path).with_title("Notes"))
        .unwrap();
    assert_eq!(*shown.borrow(), [window.0]);
    // What the system's data request now gets.
    let request = Request::new();
    sharer.slot().serve(&request).unwrap();
    assert_eq!(
        inspect(&request.package),
        expected("Notes", &temp, "notes.md")
    );

    // A missing file fails before the sheet opens and leaves the previous share alone.
    let missing = sharer.share(&SharedFile::new(temp.dir.join("missing.md")));
    assert!(matches!(missing, Err(WinError::Os { .. })), "{missing:?}");
    assert_eq!(shown.borrow().len(), 1);
    let request = Request::new();
    sharer.slot().serve(&request).unwrap();
    assert_eq!(
        inspect(&request.package),
        expected("Notes", &temp, "notes.md")
    );

    // A second sharer for the window, and one whose sheet refuses to open.
    let refusing = Sharer::with_ui(
        window.0,
        RecordingSheet {
            shown: shown.clone(),
            fail: true,
        },
    )
    .unwrap();
    let refused = refusing.share(&SharedFile::new(&temp.path));
    assert!(
        matches!(refused, Err(WinError::Invalid("no sheet"))),
        "{refused:?}"
    );
    assert_eq!(shown.borrow().len(), 2);

    // The registration is gone with the sharer: a new one starts clean.
    drop(sharer);
    drop(refusing);
    let again = Sharer::with_ui(
        window.0,
        RecordingSheet {
            shown: shown.clone(),
            fail: false,
        },
    )
    .unwrap();
    let error = again.slot().serve(&Request::new()).unwrap_err();
    assert!(matches!(error, WinError::Invalid(_)), "{error:?}");
}

#[test]
fn the_system_sharer_is_created_for_a_window_without_showing_anything() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let window = HiddenWindow::new();
    SystemShareUi::new().unwrap();
    let sharer = Sharer::new(window.0).unwrap();
    // Nothing was shared, so a data request has nothing to hand out.
    let error = sharer.slot().serve(&Request::new()).unwrap_err();
    assert!(matches!(error, WinError::Invalid(_)), "{error:?}");
}
