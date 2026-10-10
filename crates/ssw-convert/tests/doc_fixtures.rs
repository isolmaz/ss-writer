//! Legacy `.doc` importer over the real fixtures: goldens, refusals, and never-panic corruption.

use std::path::PathBuf;

use ssw_convert::doc::{self, sniff};
use ssw_convert::import::{ImportError, LossKind};

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/doc")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn golden(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/doc")
        .join(format!("{name}.md"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn all_fixture_names() -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/doc");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn golden_markdown() {
    for name in [
        "simple-list",
        "simple-table",
        "hyperlink",
        "footnote",
        "endingnote",
        "innertable",
        "table-merges",
        "Lists",
        "test-fields",
        "HeaderFooterUnicode",
        "page-break",
    ] {
        let res = doc::import(&fixture(&format!("{name}.doc")), "assets")
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(res.markdown, golden(name), "{name}");
    }
}

#[test]
fn loss_reports_name_what_was_dropped() {
    let kinds = |name: &str| -> Vec<LossKind> {
        let mut k: Vec<LossKind> = doc::import(&fixture(name), "a")
            .unwrap()
            .losses
            .iter()
            .map(|l| l.kind)
            .collect();
        k.sort();
        k
    };
    assert!(kinds("innertable.doc").contains(&LossKind::NestedTables));
    assert!(kinds("innertable.doc").contains(&LossKind::MultiParagraphCells));
    assert!(kinds("hyperlink.doc").contains(&LossKind::Underline));
    assert!(kinds("test-fields.doc").contains(&LossKind::HeadersFooters));
    assert!(kinds("test-fields.doc").contains(&LossKind::Comments));
    assert!(kinds("test-fields.doc").contains(&LossKind::TextBoxes));
    assert!(
        kinds("equation.doc").contains(&LossKind::UnsupportedImages)
            || kinds("equation.doc").contains(&LossKind::UnsupportedEquations)
    );
    assert!(kinds("simple-list.doc").is_empty());
}

#[test]
fn encrypted_and_legacy_versions_are_refused() {
    assert_eq!(
        doc::import(&fixture("PasswordProtected.doc"), "a").unwrap_err(),
        ImportError::Encrypted
    );
    for name in ["Word6.doc", "Word95.doc"] {
        assert!(
            matches!(
                doc::import(&fixture(name), "a"),
                Err(ImportError::UnsupportedVersion(_))
            ),
            "{name}"
        );
    }
}

#[test]
fn other_formats_are_sniffed_not_parsed() {
    let cases: [(&[u8], &str); 4] = [
        (b"{\\rtf1\\ansi x}", "RTF"),
        (b"PK\x03\x04....", "DOCX"),
        (b"%PDF-1.5", "PDF"),
        (b"<html><body></body></html>", "HTML"),
    ];
    for (bytes, detected) in cases {
        assert_eq!(sniff::sniff(bytes), Some(detected));
        assert_eq!(
            doc::import(bytes, "a").unwrap_err(),
            ImportError::WrongFormat {
                expected: "DOC",
                detected
            }
        );
    }
}

#[test]
fn every_fixture_imports_or_fails_cleanly() {
    for name in all_fixture_names() {
        match doc::import(&fixture(&name), "assets") {
            Ok(_) => {}
            Err(
                ImportError::Encrypted
                | ImportError::UnsupportedVersion(_)
                | ImportError::Corrupt(_)
                | ImportError::WrongFormat { .. }
                | ImportError::TooLarge(_),
            ) => {}
        }
    }
}

/// The only picture among the fixtures is an EMF, which cannot be turned into a Markdown image:
/// it is replaced by a placeholder and counted. (Raster extraction is covered by the unit tests
/// of `doc::pictures`, which build the BLIP and DIB structures themselves.)
#[test]
fn vector_pictures_are_omitted_and_reported() {
    let res = doc::import(&fixture("picture.doc"), "picture_assets").unwrap();
    assert!(res.assets.is_empty());
    let start: String = res.markdown.chars().take(60).collect();
    assert!(res.markdown.starts_with("*[image omitted]*"), "{start}");
    let loss = res
        .losses
        .iter()
        .find(|l| l.kind == LossKind::UnsupportedImages)
        .expect("the omitted picture is reported");
    assert_eq!(loss.count, 1);
}

/// Deterministic xorshift so failures reproduce.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[test]
fn truncation_and_bit_flips_never_panic() {
    for name in [
        "simple-table.doc",
        "Lists.doc",
        "footnote.doc",
        "hyperlink.doc",
        "table-merges.doc",
    ] {
        let bytes = fixture(name);
        for cut in (0..bytes.len()).step_by(997) {
            let _ = doc::import(&bytes[..cut], "a");
        }
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..60 {
            let mut copy = bytes.clone();
            for _ in 0..8 {
                let i = (next(&mut state) % copy.len() as u64) as usize;
                copy[i] ^= 1 << (next(&mut state) % 8);
            }
            let _ = doc::import(&copy, "a");
        }
    }
}
