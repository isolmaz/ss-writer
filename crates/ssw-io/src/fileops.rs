//! File operations behind the sidebar context menu (spec §5): rename with Windows name
//! validation, duplicate, new file/folder, Recycle Bin delete, copy/move with conflict naming.

use std::path::{Path, PathBuf};

use crate::fs::{FileSystem, FsError, win32};
use crate::watch::path_key;

/// Longest file name Windows accepts, in UTF-16 units.
pub const MAX_NAME_UTF16: usize = 255;

/// A file name Windows would reject.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// Empty or only spaces.
    #[error("the name is empty")]
    Empty,
    /// Longer than [`MAX_NAME_UTF16`] UTF-16 units.
    #[error("the name is too long")]
    TooLong,
    /// Contains one of `< > : " / \ | ? *` or a control character.
    #[error("the name contains the invalid character {0:?}")]
    InvalidChar(char),
    /// A DOS device name such as `CON`, `NUL`, `COM1` (with or without extension).
    #[error("{0} is a reserved name")]
    Reserved(String),
    /// Ends with a dot or a space (also covers `.` and `..`).
    #[error("the name ends with a dot or a space")]
    TrailingDotOrSpace,
}

/// Why a file operation failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FileOpError {
    /// The requested name is not valid.
    #[error(transparent)]
    Name(#[from] NameError),
    /// A folder cannot be copied or moved into itself.
    #[error("cannot place a folder inside itself: {}", .0.display())]
    IntoSelf(PathBuf),
    /// The OS call failed.
    #[error(transparent)]
    Fs(#[from] FsError),
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validates a single file or folder name (no path separators) against Windows rules.
///
/// # Errors
/// The first violated rule as a [`NameError`].
pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.trim().is_empty() {
        return Err(NameError::Empty);
    }
    if name.encode_utf16().count() > MAX_NAME_UTF16 {
        return Err(NameError::TooLong);
    }
    if let Some(invalid) = name
        .chars()
        .find(|c| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || *c < ' ')
    {
        return Err(NameError::InvalidChar(invalid));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(NameError::TrailingDotOrSpace);
    }
    let device = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_uppercase();
    if RESERVED.contains(&device.as_str()) {
        return Err(NameError::Reserved(device));
    }
    Ok(())
}

/// How a free name is derived from a base name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameStyle {
    /// `Untitled`, `Untitled 2`, `Untitled 3`, …
    Numbered,
    /// `name copy`, `name copy 2`, …
    Copy,
    /// `name`, `name (2)`, `name (3)`, … (the plain name is used when it is free).
    Conflict,
}

fn candidate(stem: &str, style: NameStyle, number: u32) -> String {
    match (style, number) {
        (NameStyle::Numbered | NameStyle::Conflict, 1) => stem.to_owned(),
        (NameStyle::Numbered, n) => format!("{stem} {n}"),
        (NameStyle::Copy, 1) => format!("{stem} copy"),
        (NameStyle::Copy, n) => format!("{stem} copy {n}"),
        (NameStyle::Conflict, n) => format!("{stem} ({n})"),
    }
}

/// First name derived from `stem`/`ext` that does not exist in `dir`.
///
/// # Errors
/// Failures of the existence checks other than "not found".
pub fn free_name(
    fs: &dyn FileSystem,
    dir: &Path,
    stem: &str,
    ext: &str,
    style: NameStyle,
) -> Result<String, FsError> {
    let mut number = 1u32;
    loop {
        let base = candidate(stem, style, number);
        let name = if ext.is_empty() {
            base
        } else {
            format!("{base}.{ext}")
        };
        match fs.metadata(&dir.join(&name)) {
            Err(FsError::NotFound(_)) => return Ok(name),
            Ok(_) => number += 1,
            Err(error) => return Err(error),
        }
    }
}

fn split_name(path: &Path, is_dir: bool) -> (String, String) {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if is_dir {
        return (name, String::new());
    }
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_default();
    (stem, ext)
}

fn parent_of(path: &Path) -> &Path {
    path.parent().unwrap_or(path)
}

/// Renames `path` to `new_name` (a bare name) inside the same folder.
///
/// # Errors
/// [`FileOpError::Name`] for invalid names, [`FsError::AlreadyExists`] when the name is taken.
pub fn rename(fs: &dyn FileSystem, path: &Path, new_name: &str) -> Result<PathBuf, FileOpError> {
    validate_name(new_name)?;
    let target = parent_of(path).join(new_name);
    if target != path {
        fs.rename(path, &target)?;
    }
    Ok(target)
}

/// Creates a new file `name` in `dir` with `content`.
///
/// # Errors
/// [`FileOpError::Name`], [`FsError::AlreadyExists`], …
pub fn new_file(
    fs: &dyn FileSystem,
    dir: &Path,
    name: &str,
    content: &[u8],
) -> Result<PathBuf, FileOpError> {
    validate_name(name)?;
    let path = dir.join(name);
    fs.write_new(&path, content)?;
    Ok(path)
}

/// Creates the next free `Untitled.<ext>` / `Untitled 2.<ext>` in `dir` (empty file).
///
/// # Errors
/// Failures of the name search or the file creation.
pub fn new_untitled_file(
    fs: &dyn FileSystem,
    dir: &Path,
    ext: &str,
) -> Result<PathBuf, FileOpError> {
    let name = crate::workspace::untitled_name(fs, dir, ext)?;
    new_file(fs, dir, &name, b"")
}

/// Creates a new folder `name` in `dir`.
///
/// # Errors
/// [`FileOpError::Name`], [`FsError::AlreadyExists`], …
pub fn new_folder(fs: &dyn FileSystem, dir: &Path, name: &str) -> Result<PathBuf, FileOpError> {
    validate_name(name)?;
    let path = dir.join(name);
    fs.create_dir(&path)?;
    Ok(path)
}

/// Creates the next free `New folder` / `New folder 2` in `dir`.
///
/// # Errors
/// Failures of the name search or the folder creation.
pub fn new_untitled_folder(
    fs: &dyn FileSystem,
    dir: &Path,
    base: &str,
) -> Result<PathBuf, FileOpError> {
    let name = free_name(fs, dir, base, "", NameStyle::Numbered)?;
    new_folder(fs, dir, &name)
}

/// Recursively copies the folder `from` to the new folder `to`.
///
/// # Errors
/// The first failing OS call.
pub fn copy_tree(fs: &dyn FileSystem, from: &Path, to: &Path) -> Result<(), FsError> {
    fs.create_dir(to)?;
    for entry in fs.list_dir(from)? {
        let source = from.join(&entry.name);
        let target = to.join(&entry.name);
        if entry.meta.is_dir() {
            copy_tree(fs, &source, &target)?;
        } else {
            fs.copy_file(&source, &target)?;
        }
    }
    Ok(())
}

/// Recursively deletes the folder `path` permanently.
///
/// # Errors
/// The first failing OS call.
pub fn remove_tree(fs: &dyn FileSystem, path: &Path) -> Result<(), FsError> {
    for entry in fs.list_dir(path)? {
        let child = path.join(&entry.name);
        if entry.meta.is_dir() {
            remove_tree(fs, &child)?;
        } else {
            fs.delete_file(&child)?;
        }
    }
    fs.remove_dir(path)
}

/// Whether `candidate` is `folder` or below it (case-insensitive like NTFS).
fn is_inside(folder: &Path, candidate: &Path) -> bool {
    let folder = path_key(folder);
    let candidate = path_key(candidate);
    candidate == folder || candidate.starts_with(&format!("{folder}\\"))
}

fn copy_item(
    fs: &dyn FileSystem,
    source: &Path,
    target: &Path,
    is_dir: bool,
) -> Result<(), FsError> {
    if is_dir {
        copy_tree(fs, source, target)
    } else {
        fs.copy_file(source, target)
    }
}

/// Duplicates a file or folder next to itself as `name copy.ext`, `name copy 2.ext`, ….
///
/// # Errors
/// OS failures.
pub fn duplicate(fs: &dyn FileSystem, path: &Path) -> Result<PathBuf, FileOpError> {
    let is_dir = fs.metadata(path)?.is_dir();
    let (stem, ext) = split_name(path, is_dir);
    let dir = parent_of(path);
    let target = dir.join(free_name(fs, dir, &stem, &ext, NameStyle::Copy)?);
    copy_item(fs, path, &target, is_dir)?;
    Ok(target)
}

/// Copies `source` into the folder `dest_dir`; a name clash becomes `name (2).ext`.
///
/// # Errors
/// [`FileOpError::IntoSelf`] when a folder would be copied into itself, OS failures.
pub fn copy_into(
    fs: &dyn FileSystem,
    source: &Path,
    dest_dir: &Path,
) -> Result<PathBuf, FileOpError> {
    let is_dir = fs.metadata(source)?.is_dir();
    if is_dir && is_inside(source, dest_dir) {
        return Err(FileOpError::IntoSelf(source.to_path_buf()));
    }
    let (stem, ext) = split_name(source, is_dir);
    let target = dest_dir.join(free_name(fs, dest_dir, &stem, &ext, NameStyle::Conflict)?);
    copy_item(fs, source, &target, is_dir)?;
    Ok(target)
}

/// Moves `source` into the folder `dest_dir`; a name clash becomes `name (2).ext`. Moving into
/// the current folder is a no-op. Folders across volumes are copied and then removed.
///
/// # Errors
/// [`FileOpError::IntoSelf`] when a folder would be moved into itself, OS failures.
pub fn move_into(
    fs: &dyn FileSystem,
    source: &Path,
    dest_dir: &Path,
) -> Result<PathBuf, FileOpError> {
    let is_dir = fs.metadata(source)?.is_dir();
    if is_dir && is_inside(source, dest_dir) {
        return Err(FileOpError::IntoSelf(source.to_path_buf()));
    }
    if parent_of(source) == dest_dir {
        return Ok(source.to_path_buf());
    }
    let (stem, ext) = split_name(source, is_dir);
    let target = dest_dir.join(free_name(fs, dest_dir, &stem, &ext, NameStyle::Conflict)?);
    match fs.rename(source, &target) {
        Ok(()) => {}
        Err(FsError::Os { code, .. }) if code == win32::NOT_SAME_DEVICE => {
            copy_tree(fs, source, &target)?;
            remove_tree(fs, source)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(target)
}

/// Moves items to the Recycle Bin (undo-able in Explorer).
///
/// # Errors
/// [`FsError::NotFound`] for missing items, [`FsError::AccessDenied`] when the shell aborts.
pub fn recycle(fs: &dyn FileSystem, paths: &[PathBuf]) -> Result<(), FsError> {
    fs.recycle(paths)
}
