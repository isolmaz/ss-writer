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
        // SAFETY: standard IFileOperation usage; the operation runs synchronously.
        unsafe {
            let operation: IFileOperation =
                CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)
                    .map_err(|error| failed(SERVICE, error))?;
            operation
                .SetOperationFlags(FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT)
                .map_err(|error| failed(SERVICE, error))?;
            let item: IShellItem =
                SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None::<&IBindCtx>)
                    .map_err(|error| failed(SERVICE, error))?;
            operation
                .DeleteItem(&item, None::<&IFileOperationProgressSink>)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::ComGuard;
    use windows::Win32::UI::Shell::SEE_MASK_FLAG_NO_UI;

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
}
