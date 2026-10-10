//! The portable zip: the executable, the `portable` marker, licenses and notices.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::error::Error;

/// One file of the zip.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    /// Path inside the archive, `/`-separated.
    pub name: String,
    /// File contents.
    pub bytes: Vec<u8>,
}

/// The contents of `SSWriter-<version>-x64-portable.zip`. The empty `portable` file next to the
/// executable switches the app to keeping settings and data beside it (spec §13).
pub fn portable_entries(
    exe: Vec<u8>,
    license: Vec<u8>,
    notices: Vec<u8>,
    font_licenses: Vec<(String, Vec<u8>)>,
) -> Vec<Entry> {
    let mut entries = vec![
        Entry {
            name: "SSWriter.exe".to_owned(),
            bytes: exe,
        },
        Entry {
            name: "portable".to_owned(),
            bytes: Vec::new(),
        },
        Entry {
            name: "LICENSE".to_owned(),
            bytes: license,
        },
        Entry {
            name: "THIRD-PARTY-NOTICES.txt".to_owned(),
            bytes: notices,
        },
    ];
    entries.extend(font_licenses.into_iter().map(|(name, bytes)| Entry {
        name: format!("licenses/{name}"),
        bytes,
    }));
    entries
}

/// Writes `entries` to a deflate-compressed zip at `path`. Entry times are fixed so the same
/// inputs produce the same archive.
pub fn write_zip(path: &Path, entries: &[Entry]) -> Result<(), Error> {
    let file = File::create(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default());
    let mut zip = ZipWriter::new(file);
    for entry in entries {
        zip.start_file(entry.name.as_str(), options)?;
        zip.write_all(&entry.bytes).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    zip.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn read_back(path: &Path) -> Vec<(String, Vec<u8>)> {
        let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
        (0..archive.len())
            .map(|i| {
                let mut file = archive.by_index(i).unwrap();
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).unwrap();
                (file.name().unwrap().into_owned(), bytes)
            })
            .collect()
    }

    #[test]
    fn portable_zip_holds_exe_marker_licenses_and_notices() {
        let entries = portable_entries(
            b"MZ-exe".to_vec(),
            b"MIT text".to_vec(),
            b"notices".to_vec(),
            vec![("LICENSE-Font.txt".to_owned(), b"OFL".to_vec())],
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.zip");
        write_zip(&path, &entries).unwrap();

        let files = read_back(&path);
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "SSWriter.exe",
                "portable",
                "LICENSE",
                "THIRD-PARTY-NOTICES.txt",
                "licenses/LICENSE-Font.txt"
            ]
        );
        let content = |name: &str| {
            files
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, b)| b.clone())
                .unwrap()
        };
        assert_eq!(content("SSWriter.exe"), b"MZ-exe");
        assert_eq!(content("portable"), b"", "the marker file is empty");
        assert_eq!(content("LICENSE"), b"MIT text");
        assert_eq!(content("THIRD-PARTY-NOTICES.txt"), b"notices");
        assert_eq!(content("licenses/LICENSE-Font.txt"), b"OFL");
    }

    #[test]
    fn the_same_inputs_give_byte_identical_archives() {
        let make = || portable_entries(vec![1, 2, 3], vec![4], vec![5], vec![]);
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.zip"), dir.path().join("b.zip"));
        write_zip(&a, &make()).unwrap();
        write_zip(&b, &make()).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    }

    #[test]
    fn an_unwritable_destination_is_an_error_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = write_zip(&dir.path().join("missing-dir").join("x.zip"), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("x.zip"), "{err}");
    }
}
