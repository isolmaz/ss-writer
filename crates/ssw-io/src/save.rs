//! Atomic saves (spec §4.3): temp file in the same folder → `ReplaceFileW`, with a
//! `MoveFileExW` fallback, sharing-violation backoff, attribute preservation, own-write stamps
//! and read-only handling.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::fs::{Attributes, FileMeta, FileSystem, FileTime, FsError};

/// Delays between retries after `ERROR_SHARING_VIOLATION` (50, 100, 200, 400, 800 ms).
pub const SHARING_BACKOFF: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

/// How many temp-file names are tried before giving up on `AlreadyExists`.
const TEMP_NAME_ATTEMPTS: usize = 8;

/// Sleeping seam so tests can run the backoff without waiting.
pub trait Sleeper: Send + Sync {
    /// Blocks for `duration`.
    fn sleep(&self, duration: Duration);
}

/// Sleeps the calling thread.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Identity of a file's content as written (or loaded) by this process. Used to recognize own
/// writes in watcher events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveStamp {
    /// Size in bytes.
    pub size: u64,
    /// Last write time after the write.
    pub mtime: FileTime,
    /// BLAKE3 hash of the content.
    pub hash: [u8; 32],
}

impl SaveStamp {
    /// Stamp of content that was read from disk with metadata `meta`.
    #[must_use]
    pub fn from_read(meta: &FileMeta, content: &[u8]) -> Self {
        Self {
            size: meta.size,
            mtime: meta.modified,
            hash: content_hash(content),
        }
    }

    /// Size and mtime equal those of `meta`.
    #[must_use]
    pub fn matches_meta(&self, meta: &FileMeta) -> bool {
        meta.size == self.size && meta.modified == self.mtime
    }

    /// Size and hash equal those of `content`.
    #[must_use]
    pub fn matches_content(&self, content: &[u8]) -> bool {
        content.len() as u64 == self.size && content_hash(content) == self.hash
    }
}

/// BLAKE3 hash of `content`.
#[must_use]
pub fn content_hash(content: &[u8]) -> [u8; 32] {
    *blake3::hash(content).as_bytes()
}

/// Why a save failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SaveError {
    /// The target carries the read-only attribute; offer "Allow editing".
    #[error("file is read-only: {}", .0.display())]
    ReadOnly(PathBuf),
    /// The file stayed locked by another process through every retry.
    #[error("file is in use by another process: {}", .0.display())]
    SharingViolation(PathBuf),
    /// The volume is full.
    #[error("disk is full: {}", .0.display())]
    DiskFull(PathBuf),
    /// Permission denied (ACL).
    #[error("access denied: {}", .0.display())]
    AccessDenied(PathBuf),
    /// Any other failure.
    #[error(transparent)]
    Fs(FsError),
}

impl From<FsError> for SaveError {
    fn from(error: FsError) -> Self {
        match error {
            FsError::SharingViolation(path) => Self::SharingViolation(path),
            FsError::DiskFull(path) => Self::DiskFull(path),
            FsError::AccessDenied(path) => Self::AccessDenied(path),
            other => Self::Fs(other),
        }
    }
}

/// Runs `op`, retrying after sharing violations with [`SHARING_BACKOFF`].
fn with_retry<T>(
    sleeper: &dyn Sleeper,
    mut op: impl FnMut() -> Result<T, FsError>,
) -> Result<T, FsError> {
    let mut delays = SHARING_BACKOFF.iter();
    loop {
        match op() {
            Err(FsError::SharingViolation(path)) => match delays.next() {
                Some(delay) => sleeper.sleep(*delay),
                None => return Err(FsError::SharingViolation(path)),
            },
            other => return other,
        }
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!(".{name}.{}-{unique}.ssw-tmp", std::process::id()))
}

fn write_temp(fs: &dyn FileSystem, target: &Path, data: &[u8]) -> Result<PathBuf, FsError> {
    let mut last = FsError::AlreadyExists(target.to_path_buf());
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let temp = temp_path(target);
        match fs.write_new(&temp, data) {
            Ok(()) => return Ok(temp),
            Err(error @ FsError::AlreadyExists(_)) => last = error,
            Err(error) => {
                // A partially written temp file must not be left behind (best effort).
                let _ = fs.delete_file(&temp);
                return Err(error);
            }
        }
    }
    Err(last)
}

fn replace_existing(
    fs: &dyn FileSystem,
    sleeper: &dyn Sleeper,
    target: &Path,
    temp: &Path,
    attrs: Attributes,
) -> Result<(), FsError> {
    match with_retry(sleeper, || fs.replace_file(target, temp)) {
        Err(FsError::ReplaceUnsupported(_)) => {
            // Network shares and some volumes cannot do ReplaceFileW: carry the attributes over
            // by hand and fall back to MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH).
            fs.set_attributes(temp, attrs)?;
            with_retry(sleeper, || fs.move_replace(temp, target))
        }
        other => other,
    }
}

/// Saves `data` to `path` atomically and returns the stamp of what was written.
///
/// The bytes go to a temp file next to `path` first; the target is then replaced in one step so
/// a crash never leaves a half-written file. Attributes of an existing target are preserved.
///
/// # Errors
/// [`SaveError::ReadOnly`] when the target has the read-only attribute,
/// [`SaveError::SharingViolation`] after the retries are exhausted, [`SaveError::DiskFull`],
/// [`SaveError::AccessDenied`] and [`SaveError::Fs`] for everything else. The temp file is
/// removed on failure.
pub fn save_atomic(
    fs: &dyn FileSystem,
    sleeper: &dyn Sleeper,
    path: &Path,
    data: &[u8],
) -> Result<SaveStamp, SaveError> {
    let existing = match fs.metadata(path) {
        Ok(meta) => Some(meta),
        Err(FsError::NotFound(_)) => None,
        Err(error) => return Err(error.into()),
    };
    if existing.is_some_and(|meta| meta.attrs.is_readonly()) {
        return Err(SaveError::ReadOnly(path.to_path_buf()));
    }
    let temp = write_temp(fs, path, data)?;
    let outcome = match existing {
        Some(meta) => replace_existing(fs, sleeper, path, &temp, meta.attrs),
        None => with_retry(sleeper, || fs.rename(&temp, path)),
    };
    if let Err(error) = outcome {
        let _ = fs.delete_file(&temp);
        return Err(error.into());
    }
    let meta = fs.metadata(path)?;
    Ok(SaveStamp {
        size: meta.size,
        mtime: meta.modified,
        hash: content_hash(data),
    })
}

/// Why a file cannot be edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteAccess {
    /// Can be written.
    Writable,
    /// The read-only attribute is set ("Allow editing" clears it).
    ReadOnlyAttribute,
    /// The attribute is clear but the ACL denies writing.
    AccessDenied,
}

/// Read-only detection: attribute first, then an access check.
///
/// # Errors
/// [`FsError::NotFound`] and other failures of the underlying calls.
pub fn write_access(fs: &dyn FileSystem, path: &Path) -> Result<WriteAccess, FsError> {
    if fs.metadata(path)?.attrs.is_readonly() {
        return Ok(WriteAccess::ReadOnlyAttribute);
    }
    Ok(if fs.is_writable(path)? {
        WriteAccess::Writable
    } else {
        WriteAccess::AccessDenied
    })
}

/// "Allow editing": clears the read-only attribute and verifies the file is writable.
///
/// # Errors
/// [`FsError::AccessDenied`] when the file is still not writable afterwards (ACL).
pub fn allow_editing(fs: &dyn FileSystem, path: &Path) -> Result<(), FsError> {
    let meta = fs.metadata(path)?;
    if meta.attrs.is_readonly() {
        fs.set_attributes(path, meta.attrs.without(Attributes::READONLY))?;
    }
    fs.is_writable(path)?
        .then_some(())
        .ok_or(FsError::AccessDenied(path.to_path_buf()))
}
