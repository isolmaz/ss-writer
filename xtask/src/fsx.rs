//! Thin file-system helpers that attach the path to every error.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Error;

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Reads a UTF-8 text file.
pub fn read_text(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path).map_err(io_error(path))
}

/// Reads a whole file.
pub fn read_bytes(path: &Path) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(io_error(path))
}

/// Writes a whole file, creating missing parent directories.
pub fn write_file(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    fs::write(path, contents).map_err(io_error(path))
}

/// Creates a directory and its parents.
pub fn create_dir(path: &Path) -> Result<(), Error> {
    fs::create_dir_all(path).map_err(io_error(path))
}

/// Removes a directory tree; a missing directory is not an error.
pub fn remove_dir_if_exists(path: &Path) -> Result<(), Error> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(path)(e)),
    }
}

/// The entries of a directory, sorted by file name: `(path, is_dir)`.
pub fn read_dir_sorted(path: &Path) -> Result<Vec<(PathBuf, bool)>, Error> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(io_error(path))? {
        let entry = entry.map_err(io_error(path))?;
        let is_dir = entry.file_type().map_err(io_error(path))?.is_dir();
        entries.push((entry.path(), is_dir));
    }
    entries.sort();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_parents_and_read_returns_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a").join("b").join("c.txt");
        write_file(&file, "héllo").unwrap();
        assert_eq!(read_text(&file).unwrap(), "héllo");
        assert_eq!(read_bytes(&file).unwrap(), "héllo".as_bytes());
    }

    #[test]
    fn missing_file_error_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.txt");
        let err = read_text(&missing).unwrap_err();
        assert!(err.to_string().contains("nope.txt"), "{err}");
    }

    #[test]
    fn directory_listing_is_sorted_and_flags_directories() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("b.txt"), "").unwrap();
        write_file(&dir.path().join("a.txt"), "").unwrap();
        create_dir(&dir.path().join("c")).unwrap();
        let listing: Vec<(String, bool)> = read_dir_sorted(dir.path())
            .unwrap()
            .into_iter()
            .map(|(p, d)| (p.file_name().unwrap().to_string_lossy().into_owned(), d))
            .collect();
        assert_eq!(
            listing,
            vec![
                ("a.txt".to_owned(), false),
                ("b.txt".to_owned(), false),
                ("c".to_owned(), true)
            ]
        );
    }

    #[test]
    fn removing_a_missing_directory_is_fine_and_an_existing_one_vanishes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("gone");
        remove_dir_if_exists(&target).unwrap();
        write_file(&target.join("x"), "1").unwrap();
        remove_dir_if_exists(&target).unwrap();
        assert!(!target.exists());
    }
}
