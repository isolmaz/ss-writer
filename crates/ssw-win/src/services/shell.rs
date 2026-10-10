//! Shell integration: open URLs and files, reveal in Explorer, move to the Recycle Bin.
//!
//! Request validation (allowed URL schemes, dangerous file types, missing paths) and request
//! construction (the `SHELLEXECUTEINFOW`, the item id list) run in [`WindowsShell`]. The two
//! calls that launch another program or open an Explorer window sit behind [`ShellHost`]:
//! [`SystemShellHost`] makes them, tests substitute a host that inspects the request.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use ssw_ui::services::{ServiceError, Shell};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IBindCtx};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    AssocIsDangerous, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FileOperation,
    IFileOperation, IFileOperationProgressSink, ILCreateFromPathW, ILFree, IShellItem,
    SEE_MASK_NOASYNC, SHCreateItemFromParsingName, SHELLEXECUTEINFOW, SHGetPathFromIDListW,
    SHOpenFolderAndSelectItems, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

use super::failed;
use crate::fault::check;

const SERVICE: &str = "shell";

/// URL schemes the app hands to the system.
const ALLOWED_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

/// The scheme of `url` when it is one the app may open; `None` otherwise (`file:`,
/// `javascript:`, relative paths and everything unknown stay inside the app).
#[must_use]
pub fn allowed_scheme(url: &str) -> Option<&'static str> {
    let (scheme, rest) = url.split_once(':')?;
    if rest.is_empty() {
        return None;
    }
    ALLOWED_SCHEMES
        .into_iter()
        .find(|allowed| allowed.eq_ignore_ascii_case(scheme))
}

/// True when the shell considers files with this extension (with the dot) dangerous to open
/// (executables, scripts, installers).
#[must_use]
pub fn is_dangerous_extension(extension: &str) -> bool {
    // SAFETY: valid NUL-terminated string.
    unsafe { AssocIsDangerous(&HSTRING::from(extension)) }.as_bool()
}

/// The extension of `path` with a leading dot (empty when it has none).
#[must_use]
pub fn dotted_extension(path: &Path) -> String {
    path.extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default()
}

/// A shell item id list for an existing file-system path, freed when dropped.
#[derive(Debug)]
pub struct ItemIdList(*mut ITEMIDLIST);

impl ItemIdList {
    /// The item id list of `path`, or `None` when the shell cannot resolve it (it does not
    /// exist).
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        // SAFETY: valid NUL-terminated path; the returned list is owned by the new value.
        let pidl = unsafe { ILCreateFromPathW(&HSTRING::from(path.as_os_str())) };
        (!pidl.is_null()).then_some(Self(pidl))
    }

    /// The raw list, valid while `self` lives.
    #[must_use]
    pub fn as_ptr(&self) -> *const ITEMIDLIST {
        self.0
    }

    /// The file-system path the list stands for.
    #[must_use]
    pub fn to_path(&self) -> Option<PathBuf> {
        let mut buffer = [0u16; 260];
        // SAFETY: `self.0` is a valid list; the buffer holds `MAX_PATH` characters.
        let found = unsafe { SHGetPathFromIDListW(self.0, &mut buffer) };
        found.as_bool().then(|| {
            let length = buffer
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(buffer.len());
            PathBuf::from(OsString::from_wide(&buffer[..length]))
        })
    }
}

impl Drop for ItemIdList {
    fn drop(&mut self) {
        // SAFETY: the list came from `ILCreateFromPathW` and is freed exactly once.
        unsafe { ILFree(Some(self.0.cast_const())) };
    }
}

/// The two OS calls that start another program or open an Explorer window.
pub trait ShellHost {
    /// Runs the request described by `info` (`ShellExecuteExW`).
    ///
    /// # Errors
    /// The OS refused or failed to run the request.
    fn execute(&mut self, info: &mut SHELLEXECUTEINFOW) -> windows::core::Result<()>;

    /// Opens an Explorer window on the parent folder of `item` with the item selected.
    ///
    /// # Errors
    /// The OS failed to open the window.
    fn select_in_explorer(&mut self, item: &ItemIdList) -> windows::core::Result<()>;
}

/// The real [`ShellHost`]: `ShellExecuteExW` and `SHOpenFolderAndSelectItems`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemShellHost;

impl ShellHost for SystemShellHost {
    fn execute(&mut self, info: &mut SHELLEXECUTEINFOW) -> windows::core::Result<()> {
        // SAFETY: the caller fully initializes the structure and keeps its strings alive.
        unsafe { ShellExecuteExW(info) }
    }

    fn select_in_explorer(&mut self, item: &ItemIdList) -> windows::core::Result<()> {
        // SAFETY: `item` is a valid absolute item id list for the duration of the call.
        unsafe { SHOpenFolderAndSelectItems(item.as_ptr(), None, 0) }
    }
}

/// The shell.
#[derive(Debug, Clone, Copy)]
pub struct WindowsShell<H = SystemShellHost> {
    owner: Option<HWND>,
    host: H,
}

impl WindowsShell {
    /// Shell calls made by the system, parented to `owner`.
    #[must_use]
    pub fn new(owner: Option<HWND>) -> Self {
        Self::with_host(owner, SystemShellHost)
    }
}

impl<H: ShellHost> WindowsShell<H> {
    /// Shell calls made through `host`.
    #[must_use]
    pub fn with_host(owner: Option<HWND>, host: H) -> Self {
        Self { owner, host }
    }

    fn execute(&mut self, target: &str) -> Result<(), ServiceError> {
        let target = HSTRING::from(target);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC,
            hwnd: self.owner.unwrap_or_default(),
            lpVerb: w!("open"),
            lpFile: PCWSTR(target.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        self.host
            .execute(&mut info)
            .map_err(|error| failed(SERVICE, error))
    }
}

impl<H: ShellHost> Shell for WindowsShell<H> {
    fn open_url(&mut self, url: &str) -> Result<(), ServiceError> {
        if allowed_scheme(url).is_none() {
            return Err(failed(SERVICE, format!("refusing to open `{url}`")));
        }
        self.execute(url)
    }

    fn open_path(&mut self, path: &Path) -> Result<(), ServiceError> {
        if !path.exists() {
            return Err(failed(
                SERVICE,
                format!("`{}` does not exist", path.display()),
            ));
        }
        if path.is_file() && is_dangerous_extension(&dotted_extension(path)) {
            return Err(failed(
                SERVICE,
                format!("refusing to open executable content `{}`", path.display()),
            ));
        }
        self.execute(&path.to_string_lossy())
    }

    fn reveal_in_explorer(&mut self, path: &Path) -> Result<(), ServiceError> {
        let item = ItemIdList::from_path(path)
            .ok_or_else(|| failed(SERVICE, format!("`{}` does not exist", path.display())))?;
        self.host
            .select_in_explorer(&item)
            .map_err(|error| failed(SERVICE, error))
    }

    fn recycle(&mut self, path: &Path) -> Result<(), ServiceError> {
        // SAFETY: standard IFileOperation usage.
        let operation: IFileOperation =
            check(unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) })
                .map_err(|error| failed(SERVICE, error))?;
        // SAFETY: valid NUL-terminated path.
        let item: IShellItem = unsafe {
            SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None::<&IBindCtx>)
        }
        .map_err(|error| failed(SERVICE, error))?;
        recycle_item(&operation, &item)
    }

    fn open_default_apps_settings(&mut self) -> Result<(), ServiceError> {
        crate::assoc::open_default_apps_settings(&mut self.host, self.owner)
    }
}

/// Moves `item` to the Recycle Bin with `operation`, synchronously and without any
/// confirmation or error window.
fn recycle_item(operation: &IFileOperation, item: &IShellItem) -> Result<(), ServiceError> {
    // SAFETY: live COM objects; the operation runs synchronously, so nothing outlives the call.
    unsafe {
        operation
            .SetOperationFlags(FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT)
            .map_err(|error| failed(SERVICE, error))?;
        operation
            .DeleteItem(item, None::<&IFileOperationProgressSink>)
            .map_err(|error| failed(SERVICE, error))?;
        operation
            .PerformOperations()
            .map_err(|error| failed(SERVICE, error))?;
        if operation
            .GetAnyOperationsAborted()
            .map_err(|error| failed(SERVICE, error))?
            .as_bool()
        {
            return Err(failed(SERVICE, "the Recycle Bin operation was aborted"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::ComGuard;
    use std::cell::RefCell;
    use std::rc::Rc;
    use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL};
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyChangeArray;
    use windows::Win32::UI::Shell::{
        FILEOPERATION_FLAGS, IFileOperation_Impl, IOperationsProgressDialog, SEE_MASK_FLAG_NO_UI,
    };
    use windows::core::{BOOL, IUnknown, Ref, implement};

    /// The methods of a file operation that recycling never calls.
    macro_rules! unsupported {
        ($($name:ident($($arg:ident: $kind:ty),*) -> $result:ty;)*) => {
            $(
                fn $name(&self $(, $arg: $kind)*) -> windows::core::Result<$result> {
                    Err(E_NOTIMPL.into())
                }
            )*
        };
    }

    /// A file operation that moves nothing: it records the four calls recycling makes, fails
    /// the one scripted and answers whether the operation was aborted.
    #[implement(IFileOperation)]
    struct ScriptedOperation {
        failing: Option<usize>,
        aborted: bool,
        calls: Rc<RefCell<Vec<&'static str>>>,
        flags: Rc<RefCell<Option<i32>>>,
    }

    impl ScriptedOperation {
        fn step(&self, name: &'static str) -> windows::core::Result<()> {
            let index = self.calls.borrow().len();
            self.calls.borrow_mut().push(name);
            if self.failing == Some(index) {
                Err(E_FAIL.into())
            } else {
                Ok(())
            }
        }
    }

    impl IFileOperation_Impl for ScriptedOperation_Impl {
        fn SetOperationFlags(&self, flags: FILEOPERATION_FLAGS) -> windows::core::Result<()> {
            *self.flags.borrow_mut() = Some(flags.0 as i32);
            self.step("SetOperationFlags")
        }

        fn DeleteItem(
            &self,
            _item: Ref<IShellItem>,
            _sink: Ref<IFileOperationProgressSink>,
        ) -> windows::core::Result<()> {
            self.step("DeleteItem")
        }

        fn PerformOperations(&self) -> windows::core::Result<()> {
            self.step("PerformOperations")
        }

        fn GetAnyOperationsAborted(&self) -> windows::core::Result<BOOL> {
            self.step("GetAnyOperationsAborted")?;
            Ok(BOOL::from(self.aborted))
        }

        unsupported! {
            Advise(_sink: Ref<IFileOperationProgressSink>) -> u32;
            Unadvise(_cookie: u32) -> ();
            SetProgressMessage(_message: &PCWSTR) -> ();
            SetProgressDialog(_dialog: Ref<IOperationsProgressDialog>) -> ();
            SetProperties(_properties: Ref<IPropertyChangeArray>) -> ();
            SetOwnerWindow(_owner: HWND) -> ();
            ApplyPropertiesToItem(_item: Ref<IShellItem>) -> ();
            ApplyPropertiesToItems(_items: Ref<IUnknown>) -> ();
            RenameItem(_item: Ref<IShellItem>, _name: &PCWSTR, _sink: Ref<IFileOperationProgressSink>) -> ();
            RenameItems(_items: Ref<IUnknown>, _name: &PCWSTR) -> ();
            MoveItem(_item: Ref<IShellItem>, _to: Ref<IShellItem>, _name: &PCWSTR, _sink: Ref<IFileOperationProgressSink>) -> ();
            MoveItems(_items: Ref<IUnknown>, _to: Ref<IShellItem>) -> ();
            CopyItem(_item: Ref<IShellItem>, _to: Ref<IShellItem>, _name: &PCWSTR, _sink: Ref<IFileOperationProgressSink>) -> ();
            CopyItems(_items: Ref<IUnknown>, _to: Ref<IShellItem>) -> ();
            DeleteItems(_items: Ref<IUnknown>) -> ();
            NewItem(_to: Ref<IShellItem>, _attributes: u32, _name: &PCWSTR, _template: &PCWSTR, _sink: Ref<IFileOperationProgressSink>) -> ();
        }
    }

    /// Recycles a folder through a scripted operation (nothing is moved, whatever happens);
    /// returns the outcome, the calls made and the flags the operation was given.
    fn recycle_through(
        failing: Option<usize>,
        aborted: bool,
    ) -> (Result<(), ServiceError>, Vec<&'static str>, Option<i32>) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let flags = Rc::new(RefCell::new(None));
        let operation: IFileOperation = ScriptedOperation {
            failing,
            aborted,
            calls: Rc::clone(&calls),
            flags: Rc::clone(&flags),
        }
        .into();
        // SAFETY: valid NUL-terminated path of an existing folder.
        let item: IShellItem = unsafe {
            SHCreateItemFromParsingName(
                &HSTRING::from(std::env::temp_dir().as_os_str()),
                None::<&IBindCtx>,
            )
        }
        .unwrap();
        let outcome = recycle_item(&operation, &item);
        let calls = calls.borrow().clone();
        let flags = *flags.borrow();
        (outcome, calls, flags)
    }

    #[test]
    fn recycling_asks_for_no_confirmation_and_no_error_window() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let (outcome, calls, flags) = recycle_through(None, false);
        outcome.unwrap();
        assert_eq!(
            calls,
            [
                "SetOperationFlags",
                "DeleteItem",
                "PerformOperations",
                "GetAnyOperationsAborted"
            ]
        );
        assert_eq!(
            flags,
            Some((FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT).0 as i32),
            "undo (the Recycle Bin), no prompt, no error window, no progress window"
        );
    }

    #[test]
    fn a_failing_step_ends_the_recycling_with_a_shell_error() {
        let _com = ComGuard::apartment_threaded().unwrap();
        for (failing, steps) in [(0, 1), (1, 2), (2, 3), (3, 4)] {
            let (outcome, calls, _) = recycle_through(Some(failing), false);
            let error = outcome.unwrap_err();
            assert!(
                matches!(
                    &error,
                    ServiceError::Failed {
                        service: "shell",
                        ..
                    }
                ),
                "{error:?}"
            );
            assert_eq!(calls.len(), steps, "nothing runs after step {failing}");
        }
    }

    #[test]
    fn the_operation_calls_recycling_never_makes_are_not_implemented() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let operation: IFileOperation = ScriptedOperation {
            failing: None,
            aborted: false,
            calls: Rc::clone(&calls),
            flags: Rc::new(RefCell::new(None)),
        }
        .into();
        let (no_sink, no_item, no_items) = (
            None::<&IFileOperationProgressSink>,
            None::<&IShellItem>,
            None::<&IUnknown>,
        );
        let name = PCWSTR::null();
        // SAFETY: calls into the live scripted operation, which ignores its arguments.
        let failures = unsafe {
            [
                operation.Advise(no_sink).err(),
                operation.Unadvise(0).err(),
                operation.SetProgressMessage(name).err(),
                operation
                    .SetProgressDialog(None::<&IOperationsProgressDialog>)
                    .err(),
                operation.SetProperties(None::<&IPropertyChangeArray>).err(),
                operation.SetOwnerWindow(HWND::default()).err(),
                operation.ApplyPropertiesToItem(no_item).err(),
                operation.ApplyPropertiesToItems(no_items).err(),
                operation.RenameItem(no_item, name, no_sink).err(),
                operation.RenameItems(no_items, name).err(),
                operation.MoveItem(no_item, no_item, name, no_sink).err(),
                operation.MoveItems(no_items, no_item).err(),
                operation.CopyItem(no_item, no_item, name, no_sink).err(),
                operation.CopyItems(no_items, no_item).err(),
                operation.DeleteItems(no_items).err(),
                operation.NewItem(no_item, 0, name, name, no_sink).err(),
            ]
        };
        for failure in failures {
            assert_eq!(failure.unwrap().code(), E_NOTIMPL);
        }
        assert!(
            calls.borrow().is_empty(),
            "none of them is a recycling step"
        );
    }

    #[test]
    fn an_aborted_operation_is_not_a_recycled_file() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let (outcome, calls, _) = recycle_through(None, true);
        assert_eq!(calls.len(), 4, "the operation ran to its end");
        assert_eq!(
            outcome.unwrap_err(),
            ServiceError::Failed {
                service: "shell",
                message: "the Recycle Bin operation was aborted".to_owned(),
            }
        );
    }

    #[test]
    fn only_web_and_mail_schemes_are_allowed() {
        for (url, expected) in [
            ("https://example.com/a?b=c", Some("https")),
            ("HTTP://example.com", Some("http")),
            ("mailto:me@example.com", Some("mailto")),
            ("file:///C:/Windows/System32/calc.exe", None),
            ("javascript:alert(1)", None),
            ("ms-settings:", None),
            ("https:", None),
            ("C:\\Windows\\notepad.exe", None),
            ("relative/path.md", None),
            ("", None),
        ] {
            assert_eq!(allowed_scheme(url), expected, "{url}");
        }
    }

    #[test]
    fn dangerous_extensions_are_reported_by_the_shell() {
        assert!(is_dangerous_extension(".exe"));
        assert!(is_dangerous_extension(".bat"));
        assert!(!is_dangerous_extension(".txt"));
        assert_eq!(dotted_extension(Path::new("C:\\a\\b.MD")), ".MD");
        assert_eq!(dotted_extension(Path::new("C:\\a\\noext")), "");
    }

    #[test]
    fn item_id_lists_round_trip_existing_paths() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let dir = std::env::temp_dir();
        let list = ItemIdList::from_path(&dir).unwrap();
        let back = list.to_path().unwrap();
        assert_eq!(back.canonicalize().unwrap(), dir.canonicalize().unwrap());
        assert!(ItemIdList::from_path(&dir.join("ssw-win-no-such-folder-6c1e")).is_none());
    }

    #[test]
    fn the_system_host_reports_a_missing_target_without_showing_ui() {
        let target = HSTRING::from(
            std::env::temp_dir()
                .join("ssw-win-no-such-file-6c1e.txt")
                .as_os_str(),
        );
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpVerb: w!("open"),
            lpFile: PCWSTR(target.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        assert!(SystemShellHost.execute(&mut info).is_err());
    }

    #[test]
    fn an_item_id_list_exposes_the_pointer_it_owns() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let list = ItemIdList::from_path(&std::env::temp_dir()).unwrap();
        assert!(!list.as_ptr().is_null());
        assert_eq!(list.as_ptr(), list.0.cast_const());
    }

    #[test]
    fn the_system_host_refuses_an_empty_item_without_opening_a_window() {
        let _com = ComGuard::apartment_threaded().unwrap();
        // A list that names nothing: the shell has no folder to open, so no window appears.
        let nothing = ItemIdList(std::ptr::null_mut());
        assert!(SystemShellHost.select_in_explorer(&nothing).is_err());
    }

    #[test]
    fn a_shell_made_without_a_host_validates_before_launching_anything() {
        let mut shell = WindowsShell::new(None);
        for url in ["javascript:alert(1)", "file:///C:/Windows/notepad.exe"] {
            let error = shell.open_url(url).unwrap_err();
            assert!(
                matches!(
                    error,
                    ServiceError::Failed {
                        service: "shell",
                        ..
                    }
                ),
                "{error:?}"
            );
        }
        let missing = std::env::temp_dir().join("ssw-win-no-such-file-6c1e.txt");
        assert!(shell.open_path(&missing).is_err());
        assert!(shell.reveal_in_explorer(&missing).is_err());
    }
}
