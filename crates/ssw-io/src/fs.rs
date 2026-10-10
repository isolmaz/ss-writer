//! The OS file seam: every file call the editor makes goes through [`FileSystem`].
//!
//! [`RealFs`] talks to Win32 (`ReplaceFileW`, `MoveFileExW`, `FindFirstFileExW`,
//! `IFileOperation`, …). [`FaultFs`] wraps a [`RealFs`] and fails scripted calls with real Win32
//! error codes, so the production error handling (retry, fallback, cleanup) runs against it.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_BAD_NETPATH, ERROR_DIR_NOT_EMPTY,
    ERROR_DISK_FULL, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_HANDLE_DISK_FULL,
    ERROR_INVALID_NAME, ERROR_LOCK_VIOLATION, ERROR_NOT_SUPPORTED, ERROR_PATH_NOT_FOUND,
    ERROR_SHARING_VIOLATION, ERROR_UNABLE_TO_MOVE_REPLACEMENT, ERROR_UNABLE_TO_MOVE_REPLACEMENT_2,
    ERROR_UNABLE_TO_REMOVE_REPLACED, FILETIME, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CopyFileW, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM, FILE_FLAGS_AND_ATTRIBUTES,
    FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW,
    FindNextFileW, GetFileAttributesExW, GetFileExInfoStandard, MOVEFILE_COPY_ALLOWED,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    REPLACEFILE_IGNORE_MERGE_ERRORS, ReplaceFileW, SetFileAttributesW, WIN32_FILE_ATTRIBUTE_DATA,
    WIN32_FIND_DATAW,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    IBindCtx,
};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FileOperation, IFileOperation,
    IFileOperationProgressSink, IShellItem, SHCreateItemFromParsingName,
};
use windows::core::{IUnknown, PCWSTR};

/// Win32 error codes the file layer reacts to.
pub mod win32 {
    /// `ERROR_FILE_NOT_FOUND`.
    pub const FILE_NOT_FOUND: u32 = super::ERROR_FILE_NOT_FOUND.0;
    /// `ERROR_PATH_NOT_FOUND`.
    pub const PATH_NOT_FOUND: u32 = super::ERROR_PATH_NOT_FOUND.0;
    /// `ERROR_ACCESS_DENIED`.
    pub const ACCESS_DENIED: u32 = super::ERROR_ACCESS_DENIED.0;
    /// `ERROR_SHARING_VIOLATION`.
    pub const SHARING_VIOLATION: u32 = super::ERROR_SHARING_VIOLATION.0;
    /// `ERROR_LOCK_VIOLATION`.
    pub const LOCK_VIOLATION: u32 = super::ERROR_LOCK_VIOLATION.0;
    /// `ERROR_DISK_FULL`.
    pub const DISK_FULL: u32 = super::ERROR_DISK_FULL.0;
    /// `ERROR_HANDLE_DISK_FULL`.
    pub const HANDLE_DISK_FULL: u32 = super::ERROR_HANDLE_DISK_FULL.0;
    /// `ERROR_ALREADY_EXISTS`.
    pub const ALREADY_EXISTS: u32 = super::ERROR_ALREADY_EXISTS.0;
    /// `ERROR_FILE_EXISTS`.
    pub const FILE_EXISTS: u32 = super::ERROR_FILE_EXISTS.0;
    /// `ERROR_DIR_NOT_EMPTY`.
    pub const DIR_NOT_EMPTY: u32 = super::ERROR_DIR_NOT_EMPTY.0;
    /// `ERROR_INVALID_NAME`.
    pub const INVALID_NAME: u32 = super::ERROR_INVALID_NAME.0;
    /// `ERROR_BAD_NETPATH`.
    pub const BAD_NETPATH: u32 = super::ERROR_BAD_NETPATH.0;
    /// `ERROR_NOT_SUPPORTED`.
    pub const NOT_SUPPORTED: u32 = super::ERROR_NOT_SUPPORTED.0;
    /// `ERROR_UNABLE_TO_REMOVE_REPLACED`.
    pub const UNABLE_TO_REMOVE_REPLACED: u32 = super::ERROR_UNABLE_TO_REMOVE_REPLACED.0;
    /// `ERROR_UNABLE_TO_MOVE_REPLACEMENT`.
    pub const UNABLE_TO_MOVE_REPLACEMENT: u32 = super::ERROR_UNABLE_TO_MOVE_REPLACEMENT.0;
    /// `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2`.
    pub const UNABLE_TO_MOVE_REPLACEMENT_2: u32 = super::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2.0;
    /// `ERROR_NO_MORE_FILES`.
    pub const NO_MORE_FILES: u32 = 18;
    /// `ERROR_NOT_SAME_DEVICE`.
    pub const NOT_SAME_DEVICE: u32 = 17;
}

/// Failure of a file operation, classified for the decisions callers make.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FsError {
    /// The file or folder does not exist.
    #[error("not found: {}", .0.display())]
    NotFound(PathBuf),
    /// The target already exists.
    #[error("already exists: {}", .0.display())]
    AlreadyExists(PathBuf),
    /// Access denied (ACL, read-only attribute, directory in the way).
    #[error("access denied: {}", .0.display())]
    AccessDenied(PathBuf),
    /// Another process holds the file open without sharing.
    #[error("sharing violation: {}", .0.display())]
    SharingViolation(PathBuf),
    /// The volume is full.
    #[error("disk full: {}", .0.display())]
    DiskFull(PathBuf),
    /// A folder to remove still has content.
    #[error("directory not empty: {}", .0.display())]
    DirectoryNotEmpty(PathBuf),
    /// `ReplaceFileW` cannot be used on this volume (network share, ReFS, …).
    #[error("replace not supported: {}", .0.display())]
    ReplaceUnsupported(PathBuf),
    /// The name is not valid on Windows.
    #[error("invalid name: {}", .0.display())]
    InvalidName(PathBuf),
    /// Anything else, with the raw Win32 error code (or HRESULT).
    #[error("os error {code}: {}", .path.display())]
    Os {
        /// Win32 error code or HRESULT.
        code: u32,
        /// Path involved.
        path: PathBuf,
    },
}

impl FsError {
    /// Classifies a Win32 error code.
    #[must_use]
    pub fn from_code(code: u32, path: &Path) -> Self {
        let path = path.to_path_buf();
        match code {
            win32::FILE_NOT_FOUND | win32::PATH_NOT_FOUND | win32::BAD_NETPATH => {
                Self::NotFound(path)
            }
            win32::ALREADY_EXISTS | win32::FILE_EXISTS => Self::AlreadyExists(path),
            win32::ACCESS_DENIED => Self::AccessDenied(path),
            win32::SHARING_VIOLATION | win32::LOCK_VIOLATION => Self::SharingViolation(path),
            win32::DISK_FULL | win32::HANDLE_DISK_FULL => Self::DiskFull(path),
            win32::DIR_NOT_EMPTY => Self::DirectoryNotEmpty(path),
            win32::UNABLE_TO_REMOVE_REPLACED
            | win32::UNABLE_TO_MOVE_REPLACEMENT
            | win32::UNABLE_TO_MOVE_REPLACEMENT_2
            | win32::NOT_SUPPORTED => Self::ReplaceUnsupported(path),
            win32::INVALID_NAME => Self::InvalidName(path),
            _ => Self::Os { code, path },
        }
    }

    /// Classifies a `std::io::Error` through its raw OS error.
    #[must_use]
    pub fn from_io(error: &std::io::Error, path: &Path) -> Self {
        let code = error.raw_os_error().unwrap_or(0);
        Self::from_code(code.cast_unsigned(), path)
    }

    /// Classifies a `windows` crate error (HRESULT, usually `HRESULT_FROM_WIN32`).
    #[must_use]
    pub fn from_windows(error: &windows::core::Error, path: &Path) -> Self {
        let hresult = error.code().0.cast_unsigned();
        // HRESULT_FROM_WIN32 keeps the Win32 code in the low word under facility 7.
        let code = if hresult >> 16 == 0x8007 {
            hresult & 0xFFFF
        } else {
            hresult
        };
        Self::from_code(code, path)
    }

    /// The path the failure refers to.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::NotFound(path)
            | Self::AlreadyExists(path)
            | Self::AccessDenied(path)
            | Self::SharingViolation(path)
            | Self::DiskFull(path)
            | Self::DirectoryNotEmpty(path)
            | Self::ReplaceUnsupported(path)
            | Self::InvalidName(path)
            | Self::Os { path, .. } => path,
        }
    }
}

pub(crate) fn io<T>(result: std::io::Result<T>, path: &Path) -> Result<T, FsError> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => Err(FsError::from_io(&error, path)),
    }
}

pub(crate) fn win<T>(result: windows::core::Result<T>, path: &Path) -> Result<T, FsError> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => Err(FsError::from_windows(&error, path)),
    }
}

/// Windows `FILETIME`: 100 ns ticks since 1601-01-01 UTC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileTime(pub u64);

const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
const TICKS_PER_SECOND: u64 = 10_000_000;

impl FileTime {
    fn from_filetime(value: FILETIME) -> Self {
        Self((u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime))
    }

    /// Converts to a [`SystemTime`] (dates before 1970 are supported). `SystemTime` on Windows
    /// counts signed 100 ns ticks, so a corrupt `FILETIME` above `i64::MAX` ticks clamps to the
    /// latest representable instant instead of overflowing.
    #[must_use]
    pub fn to_system_time(self) -> SystemTime {
        let ticks = self.0.min(i64::MAX.cast_unsigned());
        if ticks >= UNIX_EPOCH_TICKS {
            UNIX_EPOCH + ticks_to_duration(ticks - UNIX_EPOCH_TICKS)
        } else {
            UNIX_EPOCH - ticks_to_duration(UNIX_EPOCH_TICKS - ticks)
        }
    }

    /// Converts from a [`SystemTime`].
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Self(UNIX_EPOCH_TICKS.saturating_add(duration_to_ticks(after))),
            Err(before) => {
                Self(UNIX_EPOCH_TICKS.saturating_sub(duration_to_ticks(before.duration())))
            }
        }
    }

    /// Milliseconds since the Unix epoch (negative before 1970).
    #[must_use]
    pub fn unix_millis(self) -> i64 {
        let ticks = i128::from(self.0) - i128::from(UNIX_EPOCH_TICKS);
        i64::try_from(ticks / 10_000).unwrap_or(i64::MAX)
    }
}

fn ticks_to_duration(ticks: u64) -> Duration {
    Duration::new(
        ticks / TICKS_PER_SECOND,
        u32::try_from((ticks % TICKS_PER_SECOND) * 100).unwrap_or(0),
    )
}

fn duration_to_ticks(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_mul(TICKS_PER_SECOND)
        .saturating_add(u64::from(duration.subsec_nanos() / 100))
}

/// Win32 file attribute bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Attributes(pub u32);

impl Attributes {
    /// `FILE_ATTRIBUTE_READONLY`.
    pub const READONLY: u32 = FILE_ATTRIBUTE_READONLY.0;
    /// `FILE_ATTRIBUTE_HIDDEN`.
    pub const HIDDEN: u32 = FILE_ATTRIBUTE_HIDDEN.0;
    /// `FILE_ATTRIBUTE_SYSTEM`.
    pub const SYSTEM: u32 = FILE_ATTRIBUTE_SYSTEM.0;
    /// `FILE_ATTRIBUTE_DIRECTORY`.
    pub const DIRECTORY: u32 = FILE_ATTRIBUTE_DIRECTORY.0;
    /// `FILE_ATTRIBUTE_ARCHIVE`.
    pub const ARCHIVE: u32 = FILE_ATTRIBUTE_ARCHIVE.0;
    /// `FILE_ATTRIBUTE_REPARSE_POINT`.
    pub const REPARSE_POINT: u32 = FILE_ATTRIBUTE_REPARSE_POINT.0;
    /// `FILE_ATTRIBUTE_OFFLINE`.
    pub const OFFLINE: u32 = FILE_ATTRIBUTE_OFFLINE.0;
    /// `FILE_ATTRIBUTE_RECALL_ON_OPEN`.
    pub const RECALL_ON_OPEN: u32 = FILE_ATTRIBUTE_RECALL_ON_OPEN.0;
    /// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`.
    pub const RECALL_ON_DATA_ACCESS: u32 = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0;
    /// Bits `SetFileAttributesW` accepts for plain files (read-only, hidden, system, archive,
    /// temporary, offline, not-content-indexed).
    pub const SETTABLE: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x100 | 0x1000 | 0x2000;
    /// Attributes that mark a cloud placeholder whose content is not local.
    pub const PLACEHOLDER: u32 = Self::OFFLINE | Self::RECALL_ON_OPEN | Self::RECALL_ON_DATA_ACCESS;

    /// Whether any of the bits in `mask` is set.
    #[must_use]
    pub fn any(self, mask: u32) -> bool {
        self.0 & mask != 0
    }

    /// Read-only attribute.
    #[must_use]
    pub fn is_readonly(self) -> bool {
        self.any(Self::READONLY)
    }

    /// Hidden attribute.
    #[must_use]
    pub fn is_hidden(self) -> bool {
        self.any(Self::HIDDEN)
    }

    /// System attribute.
    #[must_use]
    pub fn is_system(self) -> bool {
        self.any(Self::SYSTEM)
    }

    /// Directory attribute.
    #[must_use]
    pub fn is_dir(self) -> bool {
        self.any(Self::DIRECTORY)
    }

    /// OneDrive-style placeholder: `OFFLINE`, `RECALL_ON_OPEN` or `RECALL_ON_DATA_ACCESS`.
    /// Reading such a file hydrates it, so indexing and search must skip it.
    #[must_use]
    pub fn is_placeholder(self) -> bool {
        self.any(Self::PLACEHOLDER)
    }

    /// The same attributes with `bits` set.
    #[must_use]
    pub fn with(self, bits: u32) -> Self {
        Self(self.0 | bits)
    }

    /// The same attributes with `bits` cleared.
    #[must_use]
    pub fn without(self, bits: u32) -> Self {
        Self(self.0 & !bits)
    }
}

/// File or folder metadata (no handle opened, placeholders are not hydrated).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileMeta {
    /// Size in bytes (0 for folders).
    pub size: u64,
    /// Creation time.
    pub created: FileTime,
    /// Last write time.
    pub modified: FileTime,
    /// Attribute bits.
    pub attrs: Attributes,
}

impl FileMeta {
    /// Whether this is a folder.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.attrs.is_dir()
    }

    /// Whether this is a cloud placeholder.
    #[must_use]
    pub fn is_placeholder(&self) -> bool {
        self.attrs.is_placeholder()
    }
}

/// One entry of a folder listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// File name (lossy UTF-16 → UTF-8).
    pub name: String,
    /// Metadata delivered by the directory enumeration itself.
    pub meta: FileMeta,
}

/// Every OS file call the editor makes.
pub trait FileSystem: Send + Sync + std::fmt::Debug {
    /// Reads the whole file.
    ///
    /// # Errors
    /// Any [`FsError`]; placeholders are hydrated by this call.
    fn read(&self, path: &Path) -> Result<Vec<u8>, FsError>;

    /// Metadata without opening the file.
    ///
    /// # Errors
    /// [`FsError::NotFound`] when the path does not exist.
    fn metadata(&self, path: &Path) -> Result<FileMeta, FsError>;

    /// Creates a new file (failing when it exists), writes `data` and flushes it to disk.
    ///
    /// # Errors
    /// [`FsError::AlreadyExists`], [`FsError::DiskFull`], …
    fn write_new(&self, path: &Path, data: &[u8]) -> Result<(), FsError>;

    /// `ReplaceFileW`: atomically replaces `target` by `replacement`, keeping the attributes and
    /// ACLs of `target`.
    ///
    /// # Errors
    /// [`FsError::SharingViolation`], [`FsError::ReplaceUnsupported`], …
    fn replace_file(&self, target: &Path, replacement: &Path) -> Result<(), FsError>;

    /// `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`: the fallback when `ReplaceFileW` is not
    /// available.
    ///
    /// # Errors
    /// Any [`FsError`].
    fn move_replace(&self, from: &Path, to: &Path) -> Result<(), FsError>;

    /// Renames or moves a file or folder; fails when `to` exists. Files may cross volumes.
    ///
    /// # Errors
    /// [`FsError::AlreadyExists`], `Os { code: ERROR_NOT_SAME_DEVICE }` for folders across volumes.
    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError>;

    /// Copies a file; fails when `to` exists.
    ///
    /// # Errors
    /// [`FsError::AlreadyExists`], …
    fn copy_file(&self, from: &Path, to: &Path) -> Result<(), FsError>;

    /// Deletes a file permanently.
    ///
    /// # Errors
    /// Any [`FsError`].
    fn delete_file(&self, path: &Path) -> Result<(), FsError>;

    /// Removes an empty folder.
    ///
    /// # Errors
    /// [`FsError::DirectoryNotEmpty`], …
    fn remove_dir(&self, path: &Path) -> Result<(), FsError>;

    /// Creates one folder; fails when it exists.
    ///
    /// # Errors
    /// [`FsError::AlreadyExists`], [`FsError::NotFound`] (missing parent), …
    fn create_dir(&self, path: &Path) -> Result<(), FsError>;

    /// Creates a folder and its missing parents; succeeds when it exists.
    ///
    /// # Errors
    /// Any [`FsError`].
    fn create_dir_all(&self, path: &Path) -> Result<(), FsError>;

    /// Lists a folder (without `.` and `..`), with metadata from the enumeration.
    ///
    /// # Errors
    /// [`FsError::NotFound`] when the folder does not exist.
    fn list_dir(&self, path: &Path) -> Result<Vec<DirEntry>, FsError>;

    /// Sets the attribute bits (only [`Attributes::SETTABLE`] bits are applied).
    ///
    /// # Errors
    /// Any [`FsError`].
    fn set_attributes(&self, path: &Path, attrs: Attributes) -> Result<(), FsError>;

    /// Access check: can the file be opened for writing? `Ok(false)` for access denied
    /// (read-only attribute or ACL); other failures are errors.
    ///
    /// # Errors
    /// [`FsError::NotFound`] and other non-access failures.
    fn is_writable(&self, path: &Path) -> Result<bool, FsError>;

    /// Moves files and folders to the Recycle Bin through `IFileOperation` (undo-able, no UI).
    ///
    /// # Errors
    /// [`FsError::NotFound`] for missing items, [`FsError::AccessDenied`] when the shell aborts.
    fn recycle(&self, paths: &[PathBuf]) -> Result<(), FsError>;
}

/// NUL-terminated UTF-16 for Win32.
pub(crate) fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The production implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct RealFs;

impl RealFs {
    fn meta_from(
        attributes: u32,
        created: FILETIME,
        modified: FILETIME,
        high: u32,
        low: u32,
    ) -> FileMeta {
        FileMeta {
            size: (u64::from(high) << 32) | u64::from(low),
            created: FileTime::from_filetime(created),
            modified: FileTime::from_filetime(modified),
            attrs: Attributes(attributes),
        }
    }
}

impl FileSystem for RealFs {
    fn read(&self, path: &Path) -> Result<Vec<u8>, FsError> {
        io(std::fs::read(path), path)
    }

    fn metadata(&self, path: &Path) -> Result<FileMeta, FsError> {
        let name = wide(path);
        let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
        // SAFETY: `name` is NUL-terminated and outlives the call; `data` is a valid
        // `WIN32_FILE_ATTRIBUTE_DATA`, which is what `GetFileExInfoStandard` writes.
        let result = unsafe {
            GetFileAttributesExW(
                PCWSTR(name.as_ptr()),
                GetFileExInfoStandard,
                (&raw mut data).cast::<c_void>(),
            )
        };
        win(result, path)?;
        Ok(Self::meta_from(
            data.dwFileAttributes,
            data.ftCreationTime,
            data.ftLastWriteTime,
            data.nFileSizeHigh,
            data.nFileSizeLow,
        ))
    }

    fn write_new(&self, path: &Path, data: &[u8]) -> Result<(), FsError> {
        let mut file = io(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path),
            path,
        )?;
        io(file.write_all(data), path)?;
        io(file.sync_all(), path)
    }

    fn replace_file(&self, target: &Path, replacement: &Path) -> Result<(), FsError> {
        let target_name = wide(target);
        let replacement_name = wide(replacement);
        // SAFETY: both names are NUL-terminated and outlive the call; the optional backup,
        // exclude and reserved arguments are null as documented.
        let result = unsafe {
            ReplaceFileW(
                PCWSTR(target_name.as_ptr()),
                PCWSTR(replacement_name.as_ptr()),
                PCWSTR::null(),
                REPLACEFILE_IGNORE_MERGE_ERRORS,
                None,
                None,
            )
        };
        win(result, target)
    }

    fn move_replace(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        let from_name = wide(from);
        let to_name = wide(to);
        // SAFETY: both names are NUL-terminated and outlive the call.
        let result = unsafe {
            MoveFileExW(
                PCWSTR(from_name.as_ptr()),
                PCWSTR(to_name.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        win(result, to)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        let from_name = wide(from);
        let to_name = wide(to);
        // SAFETY: both names are NUL-terminated and outlive the call.
        let result = unsafe {
            MoveFileExW(
                PCWSTR(from_name.as_ptr()),
                PCWSTR(to_name.as_ptr()),
                MOVEFILE_COPY_ALLOWED | MOVEFILE_WRITE_THROUGH,
            )
        };
        win(result, to)
    }

    fn copy_file(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        let from_name = wide(from);
        let to_name = wide(to);
        // SAFETY: both names are NUL-terminated and outlive the call.
        let result =
            unsafe { CopyFileW(PCWSTR(from_name.as_ptr()), PCWSTR(to_name.as_ptr()), true) };
        win(result, to)
    }

    fn delete_file(&self, path: &Path) -> Result<(), FsError> {
        io(std::fs::remove_file(path), path)
    }

    fn remove_dir(&self, path: &Path) -> Result<(), FsError> {
        io(std::fs::remove_dir(path), path)
    }

    fn create_dir(&self, path: &Path) -> Result<(), FsError> {
        io(std::fs::create_dir(path), path)
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), FsError> {
        io(std::fs::create_dir_all(path), path)
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<DirEntry>, FsError> {
        let pattern = wide(&path.join("*"));
        let mut data = WIN32_FIND_DATAW::default();
        // SAFETY: `pattern` is NUL-terminated and outlives the call; `data` is a valid
        // `WIN32_FIND_DATAW`, which is what `FindExInfoBasic` writes.
        let handle = unsafe {
            FindFirstFileExW(
                PCWSTR(pattern.as_ptr()),
                FindExInfoBasic,
                (&raw mut data).cast::<c_void>(),
                FindExSearchNameMatch,
                None,
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        let find = FindGuard(win(handle, path)?);
        let mut entries = Vec::new();
        loop {
            let length = data
                .cFileName
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(data.cFileName.len());
            let name = String::from_utf16_lossy(&data.cFileName[..length]);
            if name != "." && name != ".." {
                entries.push(DirEntry {
                    name,
                    meta: Self::meta_from(
                        data.dwFileAttributes,
                        data.ftCreationTime,
                        data.ftLastWriteTime,
                        data.nFileSizeHigh,
                        data.nFileSizeLow,
                    ),
                });
            }
            // SAFETY: `find` holds the live handle returned by `FindFirstFileExW` and `data`
            // is a valid `WIN32_FIND_DATAW`. Any failure (normally `ERROR_NO_MORE_FILES`) ends
            // the enumeration.
            if unsafe { FindNextFileW(find.0, &mut data) }.is_err() {
                break;
            }
        }
        Ok(entries)
    }

    fn set_attributes(&self, path: &Path, attrs: Attributes) -> Result<(), FsError> {
        let name = wide(path);
        let bits = attrs.0 & Attributes::SETTABLE;
        // `FILE_ATTRIBUTE_NORMAL` is how Win32 expresses "no attribute bits".
        let value = if bits == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            FILE_FLAGS_AND_ATTRIBUTES(bits)
        };
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let result = unsafe { SetFileAttributesW(PCWSTR(name.as_ptr()), value) };
        win(result, path)
    }

    fn is_writable(&self, path: &Path) -> Result<bool, FsError> {
        let opened = io(std::fs::OpenOptions::new().write(true).open(path), path);
        match opened {
            Ok(_) => Ok(true),
            Err(FsError::AccessDenied(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn recycle(&self, paths: &[PathBuf]) -> Result<(), FsError> {
        let Some(first) = paths.first() else {
            return Ok(());
        };
        // IFileOperation wants a single-threaded apartment; a short-lived thread gives it one
        // without touching the caller's COM state. A panic inside propagates out of `scope`.
        let mut result = Ok(());
        std::thread::scope(|scope| {
            scope.spawn(|| result = recycle_on_sta(paths, first));
        });
        result
    }
}

struct FindGuard(HANDLE);

impl Drop for FindGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from `FindFirstFileExW` and is closed exactly once.
        // A failing close leaves nothing to recover.
        let _ = unsafe { FindClose(self.0) };
    }
}

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: balanced with the successful `CoInitializeEx` in `recycle_on_sta`.
        unsafe { CoUninitialize() };
    }
}

fn recycle_on_sta(paths: &[PathBuf], first: &Path) -> Result<(), FsError> {
    // SAFETY: initializes COM for this dedicated thread; balanced by `ComApartment::drop`.
    let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    win(init.ok(), first)?;
    let _apartment = ComApartment;
    // SAFETY: `FileOperation` is the registered CLSID of `IFileOperation`.
    let operation: IFileOperation = win(
        unsafe { CoCreateInstance(&FileOperation, None::<&IUnknown>, CLSCTX_ALL) },
        first,
    )?;
    // SAFETY: valid COM object on its owning thread.
    let flags_set = unsafe {
        operation.SetOperationFlags(FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT)
    };
    win(flags_set, first)?;
    for path in paths {
        let name = wide(path);
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let item: IShellItem = win(
            unsafe { SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None::<&IBindCtx>) },
            path,
        )?;
        // SAFETY: `item` is a valid shell item; no progress sink is used.
        let deleted = unsafe { operation.DeleteItem(&item, None::<&IFileOperationProgressSink>) };
        win(deleted, path)?;
    }
    // SAFETY: valid COM object on its owning thread.
    win(unsafe { operation.PerformOperations() }, first)?;
    // SAFETY: valid COM object on its owning thread.
    let aborted = win(unsafe { operation.GetAnyOperationsAborted() }, first)?;
    (!aborted.as_bool())
        .then_some(())
        .ok_or(FsError::AccessDenied(first.to_path_buf()))
}

/// The kind of call a [`FaultFs`] script targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FsOp {
    /// [`FileSystem::read`].
    Read,
    /// [`FileSystem::metadata`].
    Metadata,
    /// [`FileSystem::write_new`].
    WriteNew,
    /// [`FileSystem::replace_file`].
    ReplaceFile,
    /// [`FileSystem::move_replace`].
    MoveReplace,
    /// [`FileSystem::rename`].
    Rename,
    /// [`FileSystem::copy_file`].
    CopyFile,
    /// [`FileSystem::delete_file`].
    DeleteFile,
    /// [`FileSystem::remove_dir`].
    RemoveDir,
    /// [`FileSystem::create_dir`].
    CreateDir,
    /// [`FileSystem::create_dir_all`].
    CreateDirAll,
    /// [`FileSystem::list_dir`].
    ListDir,
    /// [`FileSystem::set_attributes`].
    SetAttributes,
    /// [`FileSystem::is_writable`].
    IsWritable,
    /// [`FileSystem::recycle`].
    Recycle,
}

/// The failure a [`FaultFs`] injects, as the Win32 error code the real call would report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsFailure {
    /// `ERROR_SHARING_VIOLATION`.
    SharingViolation,
    /// `ERROR_ACCESS_DENIED`.
    AccessDenied,
    /// `ERROR_DISK_FULL`.
    DiskFull,
    /// `ERROR_UNABLE_TO_MOVE_REPLACEMENT` (`ReplaceFileW` failure on shares and some volumes).
    UnableToMoveReplacement,
    /// `ERROR_FILE_NOT_FOUND`.
    NotFound,
    /// Any other Win32 error code.
    Code(u32),
}

impl FsFailure {
    /// The Win32 error code.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            Self::SharingViolation => win32::SHARING_VIOLATION,
            Self::AccessDenied => win32::ACCESS_DENIED,
            Self::DiskFull => win32::DISK_FULL,
            Self::UnableToMoveReplacement => win32::UNABLE_TO_MOVE_REPLACEMENT,
            Self::NotFound => win32::FILE_NOT_FOUND,
            Self::Code(code) => code,
        }
    }
}

#[derive(Debug)]
struct Rule {
    op: FsOp,
    failure: FsFailure,
    /// `None`: every call; `Some(n)`: the next `n` calls.
    remaining: Option<usize>,
    /// Only calls whose primary path equals this one.
    path: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct FaultState {
    rules: Vec<Rule>,
    calls: HashMap<FsOp, usize>,
}

/// A [`RealFs`] whose calls fail on script. Everything not scripted runs for real.
#[derive(Debug, Default)]
pub struct FaultFs {
    inner: RealFs,
    state: Mutex<FaultState>,
}

impl FaultFs {
    /// A fault injector with an empty script.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FaultState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Fails the next `times` calls of `op` with `failure`.
    pub fn fail(&self, op: FsOp, failure: FsFailure, times: usize) {
        self.state().rules.push(Rule {
            op,
            failure,
            remaining: Some(times),
            path: None,
        });
    }

    /// Fails every call of `op` with `failure`.
    pub fn fail_always(&self, op: FsOp, failure: FsFailure) {
        self.state().rules.push(Rule {
            op,
            failure,
            remaining: None,
            path: None,
        });
    }

    /// Fails the next `times` calls of `op` whose primary path is `path`.
    pub fn fail_for_path(&self, op: FsOp, path: &Path, failure: FsFailure, times: usize) {
        self.state().rules.push(Rule {
            op,
            failure,
            remaining: Some(times),
            path: Some(path.to_path_buf()),
        });
    }

    /// Removes every scripted failure (call counters are kept).
    pub fn clear(&self) {
        self.state().rules.clear();
    }

    /// How many times `op` was called (failed or not).
    #[must_use]
    pub fn calls(&self, op: FsOp) -> usize {
        self.state().calls.get(&op).copied().unwrap_or(0)
    }

    fn check(&self, op: FsOp, path: &Path) -> Result<(), FsError> {
        let mut state = self.state();
        *state.calls.entry(op).or_insert(0) += 1;
        let hit = state.rules.iter_mut().find(|rule| {
            rule.op == op
                && rule.remaining != Some(0)
                && rule.path.as_deref().is_none_or(|expected| expected == path)
        });
        match hit {
            Some(rule) => {
                if let Some(remaining) = rule.remaining.as_mut() {
                    *remaining -= 1;
                }
                Err(FsError::from_code(rule.failure.code(), path))
            }
            None => Ok(()),
        }
    }
}

impl FileSystem for FaultFs {
    fn read(&self, path: &Path) -> Result<Vec<u8>, FsError> {
        self.check(FsOp::Read, path)?;
        self.inner.read(path)
    }

    fn metadata(&self, path: &Path) -> Result<FileMeta, FsError> {
        self.check(FsOp::Metadata, path)?;
        self.inner.metadata(path)
    }

    fn write_new(&self, path: &Path, data: &[u8]) -> Result<(), FsError> {
        self.check(FsOp::WriteNew, path)?;
        self.inner.write_new(path, data)
    }

    fn replace_file(&self, target: &Path, replacement: &Path) -> Result<(), FsError> {
        self.check(FsOp::ReplaceFile, target)?;
        self.inner.replace_file(target, replacement)
    }

    fn move_replace(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        self.check(FsOp::MoveReplace, to)?;
        self.inner.move_replace(from, to)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        self.check(FsOp::Rename, to)?;
        self.inner.rename(from, to)
    }

    fn copy_file(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        self.check(FsOp::CopyFile, to)?;
        self.inner.copy_file(from, to)
    }

    fn delete_file(&self, path: &Path) -> Result<(), FsError> {
        self.check(FsOp::DeleteFile, path)?;
        self.inner.delete_file(path)
    }

    fn remove_dir(&self, path: &Path) -> Result<(), FsError> {
        self.check(FsOp::RemoveDir, path)?;
        self.inner.remove_dir(path)
    }

    fn create_dir(&self, path: &Path) -> Result<(), FsError> {
        self.check(FsOp::CreateDir, path)?;
        self.inner.create_dir(path)
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), FsError> {
        self.check(FsOp::CreateDirAll, path)?;
        self.inner.create_dir_all(path)
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<DirEntry>, FsError> {
        self.check(FsOp::ListDir, path)?;
        self.inner.list_dir(path)
    }

    fn set_attributes(&self, path: &Path, attrs: Attributes) -> Result<(), FsError> {
        self.check(FsOp::SetAttributes, path)?;
        self.inner.set_attributes(path, attrs)
    }

    fn is_writable(&self, path: &Path) -> Result<bool, FsError> {
        self.check(FsOp::IsWritable, path)?;
        self.inner.is_writable(path)
    }

    fn recycle(&self, paths: &[PathBuf]) -> Result<(), FsError> {
        self.check(
            FsOp::Recycle,
            paths.first().map_or(Path::new(""), PathBuf::as_path),
        )?;
        self.inner.recycle(paths)
    }
}
