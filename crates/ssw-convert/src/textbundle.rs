//! TextBundle (`.textbundle` directory) and TextPack (`.textpack` zip) reading and writing.
//!
//! A bundle holds `info.json`, one `text.*` file (Markdown) and an `assets/` folder; the text
//! refers to assets as `assets/<name>`. A pack is the same layout zipped, optionally inside one
//! top-level folder.

use std::io::{Cursor, Read, Write};
use std::path::Path;

use crate::import::Asset;

/// Per-file size cap when reading (guards against zip bombs and absurd bundles).
pub const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;
/// Total size cap of all assets of one bundle.
pub const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

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
        let entry = entry?;
        if entry.file_type()?.is_file() {
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
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_rel = format!("{rel}/{name}");
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push((entry.path(), child_rel));
            } else if kind.is_file() {
                let len = entry.metadata()?.len();
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

/// Reads a `.textpack` archive.
pub fn read_pack(bytes: &[u8]) -> Result<TextBundle, TextBundleError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
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
    // The text file closest to the root decides the bundle folder.
    let mut best: Option<(usize, String)> = None;
    for (_, name) in &entries {
        let (dir, file) = name.rsplit_once('/').unwrap_or(("", name.as_str()));
        let siblings: Vec<&str> = entries
            .iter()
            .filter_map(|(_, n)| {
                let (d, f) = n.rsplit_once('/').unwrap_or(("", n.as_str()));
                (d == dir).then_some(f)
            })
            .collect();
        if pick_text_name(&siblings) == Some(file) {
            let depth = name.matches('/').count();
            if best.as_ref().is_none_or(|(d, _)| depth < *d) {
                best = Some((depth, dir.to_string()));
            }
        }
    }
    let (_, root) = best.ok_or(TextBundleError::MissingText)?;
    let prefix = if root.is_empty() {
        String::new()
    } else {
        format!("{root}/")
    };
    let siblings: Vec<&str> = entries
        .iter()
        .filter_map(|(_, n)| n.strip_prefix(prefix.as_str()).filter(|f| !f.contains('/')))
        .collect();
    let text_name = pick_text_name(&siblings).ok_or(TextBundleError::MissingText)?;
    let text_index = entries
        .iter()
        .find(|(_, n)| n.strip_prefix(prefix.as_str()) == Some(text_name))
        .map(|(i, _)| *i)
        .ok_or(TextBundleError::MissingText)?;
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
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, &a.bytes)?;
    }
    Ok(())
}

/// Builds a `.textpack` archive.
pub fn write_pack(text: &str, assets: &[Asset]) -> Result<Vec<u8>, TextBundleError> {
    for a in assets {
        check_asset_path(&a.relative_path)?;
    }
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("info.json", options)?;
    zip.write_all(INFO_JSON.as_bytes())?;
    zip.start_file("text.markdown", options)?;
    zip.write_all(text.as_bytes())?;
    for a in assets {
        zip.start_file(a.relative_path.as_str(), options)?;
        zip.write_all(&a.bytes)?;
    }
    Ok(zip.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let options = zip::write::SimpleFileOptions::default();
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
        assert!(matches!(
            read_dir(tmp.path()),
            Err(TextBundleError::MissingText)
        ));
        let bytes = zip_of(&[("info.json", b"{}")]);
        assert!(matches!(
            read_pack(&bytes),
            Err(TextBundleError::MissingText)
        ));
        assert!(matches!(
            read_pack(b"not a zip"),
            Err(TextBundleError::Zip(_))
        ));
        assert!(matches!(
            read_dir(&tmp.path().join("nope")),
            Err(TextBundleError::Io(_))
        ));
    }

    #[test]
    fn bundle_without_assets_folder_reads_empty_assets() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("text.txt"), "t").unwrap();
        let b = read_dir(tmp.path()).unwrap();
        assert_eq!((b.text.as_str(), b.assets.len()), ("t", 0));
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
            assert!(
                matches!(
                    write_dir(&tmp.path().join("b"), "t", &assets),
                    Err(TextBundleError::InvalidAssetPath(p)) if p == bad
                ),
                "{bad}"
            );
            assert!(
                matches!(
                    write_pack("t", &assets),
                    Err(TextBundleError::InvalidAssetPath(_))
                ),
                "{bad}"
            );
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
            matches!(&err, TextBundleError::TooLarge(p) if p.starts_with("assets/")),
            "{err}"
        );
        // A single file above the per-entry cap.
        let err = read_dir_limited(tmp.path(), 5, 100).unwrap_err();
        assert!(matches!(err, TextBundleError::TooLarge(_)));
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
        assert!(matches!(err, TextBundleError::TooLarge(_)));
        let ok = read_entry(&mut archive, index, 0).unwrap();
        assert_eq!(ok.1, 16);
    }
}
