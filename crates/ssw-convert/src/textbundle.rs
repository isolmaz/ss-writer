//! TextBundle (`.textbundle` directory) and TextPack (`.textpack` zip) reading and writing.
//!
//! A bundle holds `info.json`, one `text.*` file (Markdown) and an `assets/` folder; the text
//! refers to assets as `assets/<name>`. A pack is the same layout zipped, optionally inside one
//! top-level folder.

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, Write};
use std::path::Path;

use crate::fault;
use crate::import::Asset;

/// Per-file size cap when reading (guards against zip bombs and absurd bundles).
pub const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;
/// Total size cap of all assets of one bundle.
pub const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
/// Cap on the number of entries of a `.textpack` archive.
pub const MAX_ENTRIES: usize = 10_000;

/// Names tried, in order, for the text file; any other `text.*` file is the last resort.
const TEXT_NAMES: [&str; 6] = [
    "text.markdown",
    "text.md",
    "text.mdown",
    "text.mkdn",
    "text.mkd",
    "text.txt",
];

/// Contents of `info.json` written by [`write_dir`] and [`write_pack`].
pub const INFO_JSON: &str = "{\n  \"version\": 2,\n  \"type\": \"net.daringfireball.markdown\",\n  \"transient\": false,\n  \"creatorIdentifier\": \"app.sswriter\"\n}\n";

/// A bundle's text and assets (`Asset::relative_path` is `assets/<name>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBundle {
    pub text: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, thiserror::Error)]
pub enum TextBundleError {
    #[error("the bundle has no text file")]
    MissingText,
    #[error("the bundle is too large: {0}")]
    TooLarge(String),
    #[error("invalid asset path: {0}")]
    InvalidAssetPath(String),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

fn decode_text(bytes: &[u8]) -> String {
    encoding_rs::UTF_8.decode(bytes).0.into_owned()
}

/// Picks the text file among `names` (file names inside one folder).
fn pick_text_name<'a>(names: &[&'a str]) -> Option<&'a str> {
    for wanted in TEXT_NAMES {
        if let Some(n) = names
            .iter()
            .copied()
            .find(|n| n.eq_ignore_ascii_case(wanted))
        {
            return Some(n);
        }
    }
    let mut others: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| n.len() > 5 && n.as_bytes()[..5].eq_ignore_ascii_case(b"text."))
        .collect();
    others.sort_unstable();
    others.first().copied()
}

fn check_asset_path(path: &str) -> Result<(), TextBundleError> {
    let ok = path.strip_prefix("assets/").is_some_and(|rest| {
        !rest.is_empty()
            && !path.contains(['\\', ':', '\0'])
            && rest
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != "..")
    });
    if ok {
        Ok(())
    } else {
        Err(TextBundleError::InvalidAssetPath(path.to_string()))
    }
}

/// Reads a `.textbundle` directory.
pub fn read_dir(dir: &Path) -> Result<TextBundle, TextBundleError> {
    read_dir_limited(dir, MAX_ENTRY_BYTES, MAX_TOTAL_BYTES)
}

fn read_dir_limited(
    dir: &Path,
    max_entry: u64,
    max_total: u64,
) -> Result<TextBundle, TextBundleError> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = fault::io(entry)?;
        if fault::io(entry.file_type())?.is_file() {
            files.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    let names: Vec<&str> = files.iter().map(String::as_str).collect();
    let text_name = pick_text_name(&names).ok_or(TextBundleError::MissingText)?;
    let text = decode_text(&std::fs::read(dir.join(text_name))?);

    let mut assets = Vec::new();
    let mut total = 0u64;
    let mut stack = vec![(dir.join("assets"), "assets".to_string())];
    while let Some((path, rel)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries {
            let entry = fault::io(entry)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_rel = format!("{rel}/{name}");
            let kind = fault::io(entry.file_type())?;
            if kind.is_dir() {
                stack.push((entry.path(), child_rel));
            } else if kind.is_file() {
                let len = fault::io(entry.metadata())?.len();
                total += len;
                if len > max_entry || total > max_total {
                    return Err(TextBundleError::TooLarge(child_rel));
                }
                assets.push(Asset {
                    relative_path: child_rel,
                    bytes: std::fs::read(entry.path())?,
                });
            }
        }
    }
    assets.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(TextBundle { text, assets })
}

/// Splits an archive path into its folder (empty at the root) and its file name.
fn split_folder(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// Reads a `.textpack` archive.
pub fn read_pack(bytes: &[u8]) -> Result<TextBundle, TextBundleError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() > MAX_ENTRIES {
        return Err(TextBundleError::TooLarge(format!(
            "{} entries (limit {MAX_ENTRIES})",
            archive.len()
        )));
    }
    let mut entries: Vec<(usize, String)> = Vec::new();
    for i in 0..archive.len() {
        let file = archive.by_index(i)?;
        if file.is_dir() {
            continue;
        }
        let Some(path) = file.enclosed_name() else {
            continue;
        };
        let name = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        entries.push((i, name));
    }
    let mut folders: HashMap<&str, Vec<&str>> = HashMap::new();
    for (_, name) in &entries {
        let (dir, file) = split_folder(name);
        folders.entry(dir).or_default().push(file);
    }
    let text_names: HashMap<&str, &str> = folders
        .iter()
        .filter_map(|(dir, files)| Some((*dir, pick_text_name(files)?)))
        .collect();
    // The text file closest to the root decides the bundle folder.
    // (depth, archive index of the text file, its folder)
    let mut best: Option<(usize, usize, &str)> = None;
    for (index, name) in &entries {
        let (dir, file) = split_folder(name);
        if text_names.get(dir) == Some(&file) {
            let depth = name.matches('/').count();
            if best.is_none_or(|(d, ..)| depth < d) {
                best = Some((depth, *index, dir));
            }
        }
    }
    let (_, text_index, root) = best.ok_or(TextBundleError::MissingText)?;
    let prefix = if root.is_empty() {
        String::new()
    } else {
        format!("{root}/")
    };
    let text = decode_text(&read_entry(&mut archive, text_index, 0)?.0);

    let assets_prefix = format!("{prefix}assets/");
    let mut assets = Vec::new();
    let mut total = 0u64;
    for (i, name) in &entries {
        let Some(rest) = name.strip_prefix(assets_prefix.as_str()) else {
            continue;
        };
        let (bytes, read) = read_entry(&mut archive, *i, total)?;
        total += read;
        assets.push(Asset {
            relative_path: format!("assets/{rest}"),
            bytes,
        });
    }
    assets.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(TextBundle { text, assets })
}

/// Reads entry `index`, enforcing the per-entry and total caps. Returns bytes and their count.
fn read_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    index: usize,
    total_so_far: u64,
) -> Result<(Vec<u8>, u64), TextBundleError> {
    let file = archive.by_index(index)?;
    let limit = MAX_ENTRY_BYTES.min(MAX_TOTAL_BYTES.saturating_sub(total_so_far));
    let mut out = Vec::new();
    file.take(limit + 1).read_to_end(&mut out)?;
    let len = out.len() as u64;
    if len > limit {
        return Err(TextBundleError::TooLarge(format!("entry {index}")));
    }
    Ok((out, len))
}

/// Writes a `.textbundle` directory at `dir` (created when missing; existing files are
/// overwritten).
pub fn write_dir(dir: &Path, text: &str, assets: &[Asset]) -> Result<(), TextBundleError> {
    for a in assets {
        check_asset_path(&a.relative_path)?;
    }
    std::fs::create_dir_all(dir.join("assets"))?;
    std::fs::write(dir.join("info.json"), INFO_JSON)?;
    std::fs::write(dir.join("text.markdown"), text)?;
    for a in assets {
        let target = dir.join(&a.relative_path);
        // a validated asset path always has at least the `assets` folder above it
        std::fs::create_dir_all(target.parent().unwrap_or(dir))?;
        std::fs::write(target, &a.bytes)?;
    }
    Ok(())
}

/// Builds a `.textpack` archive.
pub fn write_pack(text: &str, assets: &[Asset]) -> Result<Vec<u8>, TextBundleError> {
    for a in assets {
        check_asset_path(&a.relative_path)?;
    }
    zip_into(Cursor::new(Vec::new()), text, assets).map(Cursor::into_inner)
}

/// Zips the bundle into `sink`.
fn zip_into<W: Write + Seek>(sink: W, text: &str, assets: &[Asset]) -> Result<W, TextBundleError> {
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut zip = zip::ZipWriter::new(sink);
    let entries = [
        ("info.json", INFO_JSON.as_bytes()),
        ("text.markdown", text.as_bytes()),
    ]
    .into_iter()
    .chain(
        assets
            .iter()
            .map(|a| (a.relative_path.as_str(), a.bytes.as_slice())),
    );
    for (name, bytes) in entries {
        zip.start_file(name, options)?;
        zip.write_all(bytes)?;
    }
    Ok(zip.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sink::{Limited, noise};

    fn asset(path: &str, bytes: &[u8]) -> Asset {
        Asset {
            relative_path: path.to_string(),
            bytes: bytes.to_vec(),
        }
    }

    fn sample_assets() -> Vec<Asset> {
        vec![
            asset("assets/b.png", &[1, 2, 3]),
            asset("assets/sub/a.bin", &[9; 10]),
        ]
    }

    #[test]
    fn dir_round_trip_keeps_text_and_assets() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("doc.textbundle");
        write_dir(&dir, "# Hi ğ\n\n![](assets/b.png)\n", &sample_assets()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("info.json")).unwrap(),
            INFO_JSON
        );
        assert!(dir.join("text.markdown").is_file());
        let back = read_dir(&dir).unwrap();
        assert_eq!(back.text, "# Hi ğ\n\n![](assets/b.png)\n");
        assert_eq!(
            back.assets,
            vec![
                asset("assets/b.png", &[1, 2, 3]),
                asset("assets/sub/a.bin", &[9; 10])
            ]
        );
    }

    #[test]
    fn pack_round_trip_and_zip_layout() {
        let bytes = write_pack("text ü", &sample_assets()).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes[..])).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "info.json",
                "text.markdown",
                "assets/b.png",
                "assets/sub/a.bin"
            ]
        );
        let back = read_pack(&bytes).unwrap();
        assert_eq!(back.text, "text ü");
        assert_eq!(back.assets, sample_assets_sorted());
    }

    fn sample_assets_sorted() -> Vec<Asset> {
        vec![
            asset("assets/b.png", &[1, 2, 3]),
            asset("assets/sub/a.bin", &[9; 10]),
        ]
    }

    /// An archive of `entries` (names ending in `/` are folders), stored uncompressed so that the
    /// bytes of every entry can be found in it.
    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            if name.ends_with('/') {
                zip.add_directory(*name, options).unwrap();
            } else {
                zip.start_file(*name, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn pack_with_top_level_folder_and_alternative_text_names() {
        let bytes = zip_of(&[
            ("My.textbundle/", b""),
            ("My.textbundle/info.json", b"{}"),
            ("My.textbundle/text.md", b"\xEF\xBB\xBFbody"),
            ("My.textbundle/assets/p.png", b"PNG"),
            ("other/assets/x.png", b"no"),
            ("../evil.txt", b"x"),
        ]);
        let b = read_pack(&bytes).unwrap();
        assert_eq!(b.text, "body");
        assert_eq!(b.assets, vec![asset("assets/p.png", b"PNG")]);
    }

    #[test]
    fn pack_prefers_the_text_file_closest_to_the_root() {
        let bytes = zip_of(&[
            ("deep/inner/text.md", b"deep"),
            ("deep/inner/assets/d.png", b"D"),
            ("one/text.md", b"one"),
            ("one/assets/o.png", b"O"),
            ("text.md", b"root"),
            ("assets/r.png", b"R"),
            ("other/text.md", b"other"),
        ]);
        let b = read_pack(&bytes).unwrap();
        assert_eq!(b.text, "root");
        assert_eq!(b.assets, vec![asset("assets/r.png", b"R")]);
    }

    #[test]
    fn text_file_preference_order() {
        let table: &[(&[&str], Option<&str>)] = &[
            (&["info.json", "text.txt", "text.md"], Some("text.md")),
            (&["text.markdown", "text.md"], Some("text.markdown")),
            (&["TEXT.MD"], Some("TEXT.MD")),
            (&["text.weird", "text.abc"], Some("text.abc")),
            (&["text.", "info.json"], None),
            (&["readme.md"], None),
            (&[], None),
        ];
        for (names, expected) in table {
            assert_eq!(pick_text_name(names), *expected, "{names:?}");
        }
    }

    #[test]
    fn missing_text_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("info.json"), "{}").unwrap();
        let missing = "the bundle has no text file";
        assert_eq!(read_dir(tmp.path()).unwrap_err().to_string(), missing);
        let bytes = zip_of(&[("info.json", b"{}")]);
        assert_eq!(read_pack(&bytes).unwrap_err().to_string(), missing);
        let message = read_pack(b"not a zip").unwrap_err().to_string();
        assert!(message.starts_with("zip error: "), "{message}");
        let message = read_dir(&tmp.path().join("nope")).unwrap_err().to_string();
        assert!(message.starts_with("i/o error: "), "{message}");
    }

    #[test]
    fn a_pack_with_too_many_entries_is_refused() {
        let names: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("f{i}")).collect();
        let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        let error = read_pack(&zip_of(&entries)).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "the bundle is too large: {} entries (limit {MAX_ENTRIES})",
                MAX_ENTRIES + 1
            )
        );
    }

    #[test]
    fn bundle_without_assets_folder_reads_empty_assets() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("text.txt"), "t").unwrap();
        let b = read_dir(tmp.path()).unwrap();
        assert_eq!((b.text.as_str(), b.assets.len()), ("t", 0));
    }

    /// Makes `link` a directory link to `target`; `false` when the platform refuses.
    fn link_dir(link: &Path, target: &Path) -> bool {
        #[cfg(windows)]
        {
            // a junction needs no privilege
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .is_ok_and(|o| o.status.success())
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }

    #[test]
    fn linked_folders_inside_assets_are_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("text.md"), "t").unwrap();
        let assets = tmp.path().join("assets");
        std::fs::create_dir(&assets).unwrap();
        std::fs::write(assets.join("a.bin"), [1u8]).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.bin"), [2u8]).unwrap();
        assert!(link_dir(&assets.join("link"), outside.path()));
        let bundle = read_dir(tmp.path()).unwrap();
        let paths: Vec<&str> = bundle
            .assets
            .iter()
            .map(|a| a.relative_path.as_str())
            .collect();
        assert_eq!(paths, ["assets/a.bin"]);
    }

    #[test]
    fn invalid_asset_paths_are_rejected_when_writing() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in [
            "x.png",
            "assets/",
            "assets/../x",
            "assets//x",
            "assets/./x",
            "assets\\x",
            "assets/c:x",
            "/assets/x",
        ] {
            let assets = [asset(bad, b"x")];
            let expected = TextBundleError::InvalidAssetPath(bad.to_string()).to_string();
            let from_dir = write_dir(&tmp.path().join("b"), "t", &assets).unwrap_err();
            assert_eq!(from_dir.to_string(), expected, "{bad}");
            let from_pack = write_pack("t", &assets).unwrap_err();
            assert_eq!(from_pack.to_string(), expected, "{bad}");
        }
    }

    #[test]
    fn oversized_directory_assets_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("text.md"), "t").unwrap();
        std::fs::create_dir(tmp.path().join("assets")).unwrap();
        std::fs::write(tmp.path().join("assets").join("a.bin"), [0u8; 6]).unwrap();
        std::fs::write(tmp.path().join("assets").join("b.bin"), [0u8; 6]).unwrap();
        // Each file fits the per-entry cap but together they exceed the total cap.
        let err = read_dir_limited(tmp.path(), 8, 10).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("the bundle is too large: assets/"),
            "{err}"
        );
        // A single file above the per-entry cap.
        let err = read_dir_limited(tmp.path(), 5, 100).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("the bundle is too large: assets/"),
            "{err}"
        );
        let ok = read_dir_limited(tmp.path(), 6, 12).unwrap();
        assert_eq!(ok.assets.len(), 2);
    }

    #[test]
    fn oversized_entries_are_rejected() {
        let big = vec![0u8; 16];
        let bytes = zip_of(&[("text.md", b"t"), ("assets/a.bin", &big)]);
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes[..])).unwrap();
        let index = (0..archive.len())
            .find(|i| archive.by_index(*i).unwrap().name().unwrap() == "assets/a.bin")
            .unwrap();
        // A remaining total budget of 8 bytes makes the 16-byte entry exceed the limit.
        let err = read_entry(&mut archive, index, MAX_TOTAL_BYTES - 8).unwrap_err();
        assert!(
            err.to_string().starts_with("the bundle is too large: "),
            "{err}"
        );
        let ok = read_entry(&mut archive, index, 0).unwrap();
        assert_eq!(ok.1, 16);
    }

    /// `entries` stored in an archive in which one byte of `needle` (the contents of one entry)
    /// is flipped: the checksum of that entry no longer matches.
    fn zip_with_damaged_entry(entries: &[(&str, &[u8])], needle: &[u8]) -> Vec<u8> {
        let mut bytes = zip_of(entries);
        let at = bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap();
        bytes[at] ^= 0xFF;
        bytes
    }

    #[test]
    fn entries_that_cannot_be_read_fail_the_pack() {
        let entries: &[(&str, &[u8])] =
            &[("text.md", b"text-body"), ("assets/a.bin", b"asset-bytes")];
        // The contents of the text file, then of an asset, do not match their checksum.
        for needle in [&b"text-body"[..], b"asset-bytes"] {
            let bytes = zip_with_damaged_entry(entries, needle);
            assert_eq!(
                read_pack(&bytes).unwrap_err().to_string(),
                "i/o error: Invalid checksum",
                "{needle:?}"
            );
        }
        // The first entry has lost its local header.
        let mut bytes = zip_of(entries);
        bytes[0] = b'X';
        assert_eq!(
            read_pack(&bytes).unwrap_err().to_string(),
            "zip error: invalid Zip archive: Invalid local file header"
        );
        // An index beyond the archive.
        let bytes = zip_of(entries);
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes[..])).unwrap();
        assert_eq!(
            read_entry(&mut archive, 9, 0).unwrap_err().to_string(),
            "zip error: specified file not found in archive"
        );
    }

    #[test]
    fn assets_that_share_a_path_cannot_be_packed() {
        let assets = [asset("assets/a.png", b"1"), asset("assets/a.png", b"2")];
        let message = write_pack("t", &assets).unwrap_err().to_string();
        assert!(message.starts_with("zip error: "), "{message}");
        assert!(message.contains("Duplicate filename"), "{message}");
    }

    #[test]
    fn a_sink_that_gives_out_at_any_byte_fails_the_pack() {
        let assets = sample_assets();
        let full = write_pack("text ü", &assets).unwrap();
        // The failure lands in a header, in the data of an entry or in the central directory,
        // depending on the budget; every one of them is reported.
        for budget in 0..full.len() as u64 {
            let sink = Limited {
                inner: Cursor::new(Vec::new()),
                budget,
            };
            let err = zip_into(sink, "text ü", &assets)
                .expect_err(&format!("a budget of {budget} bytes cannot hold the pack"));
            assert!(err.to_string().contains("sink is full"), "{budget}: {err}");
        }
        let sink = Limited {
            inner: Cursor::new(Vec::new()),
            budget: full.len() as u64,
        };
        let written = zip_into(sink, "text ü", &assets).unwrap();
        assert_eq!(written.inner.into_inner(), full);
    }

    #[test]
    fn a_sink_that_gives_out_while_large_entries_are_deflated_fails_the_pack() {
        // Incompressible entries: the deflater passes output on to the sink during the write of
        // the entry itself, so these budgets fail inside the text and inside the asset.
        let text: String = noise(150_000)
            .iter()
            .map(|b| char::from(b'a' + b % 26))
            .collect();
        let assets = [asset("assets/big.bin", &noise(150_000))];
        let full = write_pack(&text, &assets).unwrap();
        let step = full.len() / 40;
        for budget in (0..full.len() as u64).step_by(step) {
            let sink = Limited {
                inner: Cursor::new(Vec::new()),
                budget,
            };
            let err = zip_into(sink, &text, &assets)
                .expect_err(&format!("a budget of {budget} bytes cannot hold the pack"));
            assert!(err.to_string().contains("sink is full"), "{budget}: {err}");
        }
        let back = read_pack(&full).unwrap();
        assert_eq!(back.text, text);
        assert_eq!(back.assets, assets);
    }

    /// Asserts that `result` fails with an I/O error.
    fn assert_io_failure(result: Result<(), TextBundleError>) {
        let message = result.unwrap_err().to_string();
        assert!(message.starts_with("i/o error: "), "{message}");
    }

    #[test]
    fn a_bundle_that_cannot_be_written_reports_the_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        // The bundle folder is a file.
        let file = tmp.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert_io_failure(write_dir(&file, "t", &[]));
        // `info.json`, then `text.markdown`, is a folder.
        for name in ["info.json", "text.markdown"] {
            let dir = tmp.path().join(format!("blocked-{name}"));
            std::fs::create_dir_all(dir.join(name)).unwrap();
            assert_io_failure(write_dir(&dir, "t", &[]));
        }
        // A folder an asset belongs in is a file.
        let dir = tmp.path().join("blocked-folder");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets").join("sub"), "x").unwrap();
        assert_io_failure(write_dir(&dir, "t", &[asset("assets/sub/a.bin", b"1")]));
        // An asset is a folder.
        let dir = tmp.path().join("blocked-asset");
        std::fs::create_dir_all(dir.join("assets").join("a.bin")).unwrap();
        assert_io_failure(write_dir(&dir, "t", &[asset("assets/a.bin", b"1")]));
    }

    /// Holds `path` open without sharing, so that nobody else can read it.
    #[cfg(windows)]
    fn lock(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn files_that_cannot_be_read_fail_the_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let text = tmp.path().join("text.md");
        std::fs::write(&text, "t").unwrap();
        let guard = lock(&text);
        let message = read_dir(tmp.path()).unwrap_err().to_string();
        assert!(message.starts_with("i/o error: "), "{message}");
        drop(guard);

        std::fs::create_dir(tmp.path().join("assets")).unwrap();
        let asset = tmp.path().join("assets").join("a.bin");
        std::fs::write(&asset, [1u8]).unwrap();
        let guard = lock(&asset);
        let message = read_dir(tmp.path()).unwrap_err().to_string();
        assert!(message.starts_with("i/o error: "), "{message}");
        drop(guard);
        assert_eq!(read_dir(tmp.path()).unwrap().assets.len(), 1);
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn a_folder_that_fails_while_it_is_listed_is_reported_wherever_it_fails() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("text.md"), "t").unwrap();
        std::fs::create_dir_all(tmp.path().join("assets").join("sub")).unwrap();
        std::fs::write(tmp.path().join("assets").join("a.bin"), [1u8]).unwrap();
        std::fs::write(tmp.path().join("assets").join("sub").join("b.bin"), [2u8]).unwrap();
        let expected = read_dir(tmp.path()).unwrap();
        assert_eq!(expected.assets.len(), 2);
        let mut calls = 0;
        loop {
            crate::fault::arm(calls);
            let result = read_dir(tmp.path());
            crate::fault::disarm();
            if !crate::fault::tripped() {
                assert_eq!(
                    result.unwrap(),
                    expected,
                    "{calls} checked calls all passed"
                );
                break;
            }
            assert_eq!(
                result.unwrap_err().to_string(),
                format!("i/o error: {}", crate::fault::INJECTED),
                "call {calls}"
            );
            calls += 1;
        }
        assert!(calls > 0, "the listing makes checked calls");
    }
}
