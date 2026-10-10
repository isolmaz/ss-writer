//! Sharing: the `DataPackage` built for a real temporary file (title, operation, storage item
//! read back through the package's view), the data request answered through the seam the
//! `DataRequested` handler uses (also from another apartment), and the sharer driven with a
//! recording sheet. The real share sheet is never shown.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use ssw_win::com::{ComGuard, WinError};
use ssw_win::share::{
    DataRequestTarget, PendingShare, ShareSlot, ShareUi, SharedFile, Sharer, SystemShareUi,
    data_requested_handler,
};
use windows::ApplicationModel::DataTransfer::{
    DataPackage, DataPackageOperation, DataRequest, DataRequestedEventArgs, DataTransferManager,
    IDataRequest_Vtbl, IDataRequestedEventArgs_Vtbl, StandardDataFormats,
};
use windows::Foundation::DateTime;
use windows::Win32::Foundation::{E_FAIL, E_NOINTERFACE, E_NOTIMPL, HWND, S_OK};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{
    GUID, HRESULT, IInspectable, IInspectable_Vtbl, IUnknown, IUnknown_Vtbl, Interface, w,
};

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
fn the_system_sheet_refuses_a_window_that_no_longer_exists() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let ui = SystemShareUi::new().unwrap();
    // The window is destroyed when the guard ends: the handle that is left names nothing, so
    // no sheet can open for it.
    let stale = HiddenWindow::new().0;
    let error = ui.show(stale).unwrap_err();
    assert!(
        matches!(
            error,
            WinError::Os {
                context: "ShowShareUIForWindow",
                ..
            }
        ),
        "{error:?}"
    );
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

/// A hand-written COM object of one WinRT interface. `DataRequest` and `DataRequestedEventArgs`
/// are runtime classes only the system creates (while it shows the share sheet), so the objects
/// the sheet would hand to the `DataRequested` handler are made here, vtable and reference count
/// included. The one getter of the interface (`Data`, `Request`) returns `answer`.
#[repr(C)]
struct FakeObject {
    /// The interface pointer of a COM object points at the pointer to its vtable.
    vtable: *const c_void,
    refs: AtomicU32,
    /// The interface the object implements (it also answers for `IUnknown` and `IInspectable`).
    iid: GUID,
    answer: windows::core::Result<IUnknown>,
}

unsafe extern "system" fn query(
    this: *mut c_void,
    iid: *const GUID,
    interface: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: `this` is the `FakeObject` of `fake`; `iid` and `interface` are valid per the COM
    // contract of `QueryInterface`.
    unsafe {
        let wanted = *iid;
        if wanted == IUnknown::IID
            || wanted == IInspectable::IID
            || wanted == (*this.cast::<FakeObject>()).iid
        {
            add_ref(this);
            interface.write(this);
            S_OK
        } else {
            interface.write(null_mut());
            E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: `this` is the `FakeObject` of `fake`.
    unsafe {
        (*this.cast::<FakeObject>())
            .refs
            .fetch_add(1, Ordering::SeqCst)
            + 1
    }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: `this` is the `FakeObject` of `fake`; the last reference frees it.
    unsafe {
        let left = (*this.cast::<FakeObject>())
            .refs
            .fetch_sub(1, Ordering::SeqCst)
            - 1;
        if left == 0 {
            drop(Box::from_raw(this.cast::<FakeObject>()));
        }
        left
    }
}

unsafe extern "system" fn get_iids(_: *mut c_void, _: *mut u32, _: *mut *mut GUID) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn get_runtime_class_name(_: *mut c_void, _: *mut *mut c_void) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn get_trust_level(_: *mut c_void, _: *mut i32) -> HRESULT {
    E_NOTIMPL
}

/// The getter the fakes have: hands out a new reference to the answer, or fails with its error.
unsafe extern "system" fn answer(this: *mut c_void, result: *mut *mut c_void) -> HRESULT {
    // SAFETY: `this` is the `FakeObject` of `fake`; `result` is a valid out pointer.
    unsafe {
        match &(*this.cast::<FakeObject>()).answer {
            Ok(value) => {
                result.write(value.clone().into_raw());
                S_OK
            }
            Err(error) => {
                result.write(null_mut());
                error.code()
            }
        }
    }
}

unsafe extern "system" fn set_data(_: *mut c_void, _: *mut c_void) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn deadline(_: *mut c_void, _: *mut DateTime) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn fail_with_display_text(_: *mut c_void, _: *mut c_void) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn get_deferral(_: *mut c_void, _: *mut *mut c_void) -> HRESULT {
    E_NOTIMPL
}

const INSPECTABLE: IInspectable_Vtbl = IInspectable_Vtbl {
    base: IUnknown_Vtbl {
        QueryInterface: query,
        AddRef: add_ref,
        Release: release,
    },
    GetIids: get_iids,
    GetRuntimeClassName: get_runtime_class_name,
    GetTrustLevel: get_trust_level,
};

static REQUEST_ARGS_VTABLE: IDataRequestedEventArgs_Vtbl = IDataRequestedEventArgs_Vtbl {
    base__: INSPECTABLE,
    Request: answer,
};

static REQUEST_VTABLE: IDataRequest_Vtbl = IDataRequest_Vtbl {
    base__: INSPECTABLE,
    Data: answer,
    SetData: set_data,
    Deadline: deadline,
    FailWithDisplayText: fail_with_display_text,
    GetDeferral: get_deferral,
};

/// A COM object of interface `T` with the vtable `vtable`, one reference of which the returned
/// value owns.
fn fake<T: Interface, V>(vtable: &'static V, answer: windows::core::Result<IUnknown>) -> T {
    let object = Box::new(FakeObject {
        vtable: std::ptr::from_ref(vtable).cast(),
        refs: AtomicU32::new(1),
        iid: T::IID,
        answer,
    });
    // SAFETY: the object starts with the pointer to a vtable of the layout of `T`, counts its
    // references and frees itself when the last one is released.
    unsafe { T::from_raw(Box::into_raw(object).cast()) }
}

/// What the share sheet hands out as the data request: it answers with `package`, or fails.
fn data_request(package: windows::core::Result<DataPackage>) -> DataRequest {
    fake(&REQUEST_VTABLE, package.map(Into::into))
}

/// What the share sheet hands to the handler as the arguments of the event.
fn event_args(request: windows::core::Result<DataRequest>) -> DataRequestedEventArgs {
    fake(&REQUEST_ARGS_VTABLE, request.map(Into::into))
}

#[test]
fn the_data_request_of_the_system_hands_out_its_package() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let package = DataPackage::new().unwrap();
    let request = data_request(Ok(package.clone()));
    assert_eq!(request.package().unwrap(), package);

    let error = data_request(Err(E_FAIL.into())).package().unwrap_err();
    assert!(
        matches!(
            &error,
            WinError::Os { context: "DataRequest::Data", source } if source.code() == E_FAIL
        ),
        "{error:?}"
    );
}

#[test]
fn the_data_requested_handler_answers_the_system_from_the_slot() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let temp = TempFile::new("notes.md");
    let slot = ShareSlot::default();
    let handler = data_requested_handler(slot.clone());
    let nobody = None::<&DataTransferManager>;

    // An event without arguments, and one whose request cannot be read.
    assert!(
        handler
            .Invoke(nobody, None::<&DataRequestedEventArgs>)
            .is_err()
    );
    let unreadable = event_args(Err(E_FAIL.into()));
    assert_eq!(
        handler.Invoke(nobody, &unreadable).unwrap_err().code(),
        E_FAIL
    );

    // Nothing is shared yet: the request fails with `E_FAIL` and its package stays empty.
    let package = DataPackage::new().unwrap();
    let args = event_args(Ok(data_request(Ok(package.clone()))));
    assert_eq!(handler.Invoke(nobody, &args).unwrap_err().code(), E_FAIL);
    assert!(
        !package
            .GetView()
            .unwrap()
            .Contains(&StandardDataFormats::StorageItems().unwrap())
            .unwrap()
    );

    // Once a file is shared, the same request is answered with it.
    slot.set(PendingShare::resolve(&SharedFile::new(&temp.path).with_title("Notes")).unwrap());
    handler.Invoke(nobody, &args).unwrap();
    assert_eq!(inspect(&package), expected("Notes", &temp, "notes.md"));

    // A request that has no package to fill cannot be answered either.
    let hollow = event_args(Ok(data_request(Err(E_FAIL.into()))));
    assert_eq!(handler.Invoke(nobody, &hollow).unwrap_err().code(), E_FAIL);
}

/// Every system call of the share that can fail does, in turn: the failure comes back as an OS
/// error of the operation, and the operation succeeds again once nothing fails.
#[cfg(feature = "fault-injection")]
#[test]
fn a_failing_system_call_fails_the_share_with_an_os_error() {
    use ssw_win::fault;

    /// Runs `run` once per system call it makes, failing that call each time; returns how many
    /// calls could fail.
    fn fail_each_call(mut run: impl FnMut() -> Result<(), WinError>) -> u32 {
        for call in 0.. {
            fault::arm(call);
            let result = run();
            let tripped = fault::tripped();
            fault::disarm();
            if !tripped {
                result.unwrap();
                return call;
            }
            let error = result.unwrap_err();
            assert!(
                matches!(error, WinError::Os { .. }),
                "call {call}: {error:?}"
            );
        }
        unreachable!("the loop ends with the first run that trips nothing")
    }

    let _com = ComGuard::apartment_threaded().unwrap();
    let window = HiddenWindow::new();
    let temp = TempFile::new("notes.md");
    let file = SharedFile::new(&temp.path);

    // `GetFileFromPathAsync`, `AgileReference::new`.
    assert_eq!(fail_each_call(|| PendingShare::resolve(&file).map(drop)), 2);
    let pending = PendingShare::resolve(&file).unwrap();
    // Resolve the file again, the operation, `Properties`, `SetTitle`, the cast to a storage
    // item and `SetStorageItems`.
    let fill = || pending.fill(&DataPackage::new().unwrap());
    assert_eq!(fail_each_call(fill), 6);
    // `DataPackage::new` and the six calls of the fill.
    assert_eq!(fail_each_call(|| pending.build().map(drop)), 7);
    // The interop factory.
    assert_eq!(fail_each_call(|| SystemShareUi::new().map(drop)), 1);
    // The interop factory, the manager of the window and the handler registration.
    let sheet = || RecordingSheet {
        shown: Rc::default(),
        fail: false,
    };
    assert_eq!(
        fail_each_call(|| Sharer::with_ui(window.0, sheet()).map(drop)),
        3
    );
    assert_eq!(fail_each_call(|| Sharer::new(window.0).map(drop)), 4);
}
