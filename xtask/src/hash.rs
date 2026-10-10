//! SHA-256 digests for release checksums and the winget manifest.

use std::fmt::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::Error;
use crate::fsx;

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            // Writing to a `String` cannot fail.
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Lowercase hex SHA-256 of a file's contents.
pub fn sha256_file(path: &Path) -> Result<String, Error> {
    Ok(sha256_hex(&fsx::read_bytes(path)?))
}

/// `sha256sum`-compatible lines (`<hex>  <file name>`) for `(file name, digest)` pairs.
pub fn checksum_lines(entries: &[(String, String)]) -> String {
    entries
        .iter()
        .map(|(name, digest)| format!("{digest}  {name}\n"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn file_digest_matches_the_bytes_digest() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("payload.bin");
        fsx::write_file(&file, b"abc").unwrap();
        assert_eq!(sha256_file(&file).unwrap(), sha256_hex(b"abc"));
        assert!(sha256_file(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn checksum_file_has_one_sha256sum_line_per_entry() {
        let text = checksum_lines(&[
            ("a.msi".to_owned(), "aa".to_owned()),
            ("b.zip".to_owned(), "bb".to_owned()),
        ]);
        assert_eq!(text, "aa  a.msi\nbb  b.zip\n");
    }
}
