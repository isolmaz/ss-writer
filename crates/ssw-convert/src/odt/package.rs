//! Read-only access to the ZIP container of an OpenDocument file, with zip-bomb limits.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use zip::ZipArchive;

use crate::import::ImportError;

/// Largest number of entries accepted.
pub(super) const MAX_ENTRIES: usize = 10_000;
/// Largest total size of everything that is read from a package, in bytes.
pub(super) const MAX_TOTAL: u64 = 200 * 1024 * 1024;

/// Why a package could not be opened or read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum PackageError {
    #[error("not a valid ZIP package: {0}")]
    Corrupt(String),
    #[error("package exceeds the size limit: {0}")]
    TooLarge(String),
}

impl From<PackageError> for ImportError {
    fn from(error: PackageError) -> ImportError {
        match error {
            PackageError::Corrupt(m) => ImportError::Corrupt(m),
            PackageError::TooLarge(m) => ImportError::TooLarge(m),
        }
    }
}

/// Part names are matched without a leading `./` or `/` and with `/` separators.
fn normalize(name: &str) -> String {
    let name = name.replace('\\', "/");
    let mut name = name.as_str();
    while let Some(rest) = name.strip_prefix("./").or_else(|| name.strip_prefix('/')) {
        name = rest;
    }
    name.to_string()
}

/// An opened ZIP archive.
#[derive(Debug)]
pub(super) struct Package<'a> {
    archive: ZipArchive<Cursor<&'a [u8]>>,
    index: HashMap<String, usize>,
    read_total: u64,
}

impl<'a> Package<'a> {
    /// Opens `bytes` as a ZIP archive and checks the entry-count and declared-size limits.
    pub fn open(bytes: &'a [u8]) -> Result<Package<'a>, PackageError> {
        let mut archive = ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| PackageError::Corrupt(e.to_string()))?;
        if archive.len() > MAX_ENTRIES {
            return Err(PackageError::TooLarge(format!(
                "{} entries (limit {MAX_ENTRIES})",
                archive.len()
            )));
        }
        let mut index = HashMap::new();
        let mut declared: u64 = 0;
        for i in 0..archive.len() {
            let file = archive
                .by_index_raw(i)
                .map_err(|e| PackageError::Corrupt(e.to_string()))?;
            declared = declared.saturating_add(file.size());
            if declared > MAX_TOTAL {
                return Err(PackageError::TooLarge(format!(
                    "more than {MAX_TOTAL} bytes uncompressed"
                )));
            }
            if file.is_dir() {
                continue;
            }
            let name = String::from_utf8_lossy(file.name_raw()).into_owned();
            index.entry(normalize(&name)).or_insert(i);
        }
        Ok(Package {
            archive,
            index,
            read_total: 0,
        })
    }

    /// True when a file with this name exists.
    pub fn contains(&self, name: &str) -> bool {
        self.index.contains_key(&normalize(name))
    }

    /// Reads a file; `Ok(None)` when it does not exist. The decompressed length is enforced, so
    /// a lying size field cannot defeat the limit.
    pub fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>, PackageError> {
        let Some(&i) = self.index.get(&normalize(name)) else {
            return Ok(None);
        };
        let file = self
            .archive
            .by_index(i)
            .map_err(|e| PackageError::Corrupt(e.to_string()))?;
        let remaining = MAX_TOTAL.saturating_sub(self.read_total);
        let mut out = Vec::new();
        file.take(remaining + 1)
            .read_to_end(&mut out)
            .map_err(|e| PackageError::Corrupt(e.to_string()))?;
        if out.len() as u64 > remaining {
            return Err(PackageError::TooLarge(format!(
                "more than {MAX_TOTAL} bytes uncompressed"
            )));
        }
        self.read_total += out.len() as u64;
        Ok(Some(out))
    }

    /// Uses up the read budget except for `left` bytes (lets tests reach the size-limit paths
    /// of callers without 200 MB of data).
    #[cfg(test)]
    pub fn exhaust_budget(&mut self, left: u64) {
        self.read_total = MAX_TOTAL.saturating_sub(left);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;

    /// A ZIP archive holding `files` (deflated; names ending in `/` become directories).
    pub(in crate::odt) fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, data) in files {
            if name.ends_with('/') {
                writer.add_directory(*name, options).unwrap();
            } else {
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn reads_parts_by_normalized_name() {
        let bytes = zip(&[
            ("dir/", b""),
            ("Pictures/a b.png", b"png"),
            ("content.xml", b"<x/>"),
        ]);
        let mut package = Package::open(&bytes).unwrap();
        assert!(package.contains("content.xml"));
        assert!(package.contains("./Pictures/a b.png"));
        assert!(package.contains("/Pictures\\a b.png"));
        assert!(!package.contains("dir/"), "directories are not parts");
        assert!(!package.contains("Content.xml"), "names are case sensitive");
        assert_eq!(
            package.read("./content.xml").unwrap(),
            Some(b"<x/>".to_vec())
        );
        assert_eq!(package.read("nothing").unwrap(), None);
    }

    #[test]
    fn rejects_non_zip_data() {
        assert!(matches!(
            Package::open(b"This is not a zip file!"),
            Err(PackageError::Corrupt(_))
        ));
        let bytes = zip(&[("a", b"hello")]);
        assert!(matches!(
            Package::open(&bytes[..20]),
            Err(PackageError::Corrupt(_))
        ));
    }

    #[test]
    fn corrupt_entry_data_is_reported_on_read() {
        // incompressible data, so the deflate stream is as long as the data
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let data: Vec<u8> = (0..2000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        let mut bytes = zip(&[("a", &data)]);
        for b in &mut bytes[100..108] {
            *b ^= 0xFF;
        }
        let mut package = Package::open(&bytes).unwrap();
        assert!(matches!(package.read("a"), Err(PackageError::Corrupt(_))));
    }

    #[test]
    fn size_limits_are_enforced() {
        let names: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("f{i}")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        let many = zip(&files);
        assert!(matches!(
            Package::open(&many),
            Err(PackageError::TooLarge(_))
        ));
        let bytes = zip(&[("a", b"abc")]);
        let mut package = Package::open(&bytes).unwrap();
        package.read_total = MAX_TOTAL;
        assert!(matches!(package.read("a"), Err(PackageError::TooLarge(_))));
    }

    #[test]
    fn errors_map_to_import_errors() {
        assert_eq!(
            ImportError::from(PackageError::Corrupt("x".into())),
            ImportError::Corrupt("x".into())
        );
        assert_eq!(
            ImportError::from(PackageError::TooLarge("y".into())),
            ImportError::TooLarge("y".into())
        );
    }
}
