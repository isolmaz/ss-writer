//! RTF import over the real Apache Tika fixtures, and Markdown → RTF → Markdown round trips.

use std::path::PathBuf;

use ssw_convert::assets::NoAssets;
use ssw_convert::import::{ImportError, ImportResult, LossKind};
use ssw_convert::rtf::{import_rtf, markdown_to_rtf};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/rtf")
}

fn fixture(name: &str) -> Vec<u8> {
    let path = fixture_dir().join(format!("{name}.rtf"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn import(name: &str) -> ImportResult {
    import_rtf(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".rtf"))
        .map(|n| n.trim_end_matches(".rtf").to_string())
        .collect();
    names.sort();
    names
}

fn loss_kinds(result: &ImportResult) -> Vec<LossKind> {
    result.sorted_losses().iter().map(|l| l.kind).collect()
}

fn golden(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/rtf")
        .join(format!("{name}.md"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every golden (`tests/golden/rtf/<fixture>.md`) is hand-derived from the Apache Tika RTF source it is named after.
#[test]
fn golden_markdown() {
    let mut seen = 0;
    for entry in
        std::fs::read_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/rtf"))
            .unwrap()
    {
        let file = entry.unwrap().file_name().to_string_lossy().into_owned();
        let name = file.trim_end_matches(".md");
        assert_eq!(import(name).markdown, golden(name), "{name}");
        seen += 1;
    }
    assert_eq!(seen, 6);
}

#[test]
fn every_fixture_imports_and_survives_truncation() {
    let names = fixture_names();
    assert_eq!(names.len(), 18);
    for name in names {
        let bytes = fixture(&name);
        assert!(import_rtf(&bytes).is_ok(), "{name}");
        for cut in [
            1,
            5,
            17,
            64,
            333,
            bytes.len() / 3,
            bytes.len() / 2,
            bytes.len() - 1,
        ] {
            // Truncated and cut-up files must never panic; a damaged header is a clean refusal.
            match import_rtf(&bytes[..cut.min(bytes.len())]) {
                Ok(_) => {}
                Err(e) => assert!(
                    matches!(e, ImportError::WrongFormat { .. }),
                    "{name}@{cut}: {e}"
                ),
            }
        }
    }
}

#[test]
fn code_pages_and_escapes_decode_to_the_right_text() {
    let czech_polish = import("testRTFWindowsCodepage1250").markdown;
    assert!(czech_polish.contains("zażółć gęślą jaźń"), "{czech_polish}");
    assert!(czech_polish.contains("ZAŻÓŁĆ GĘŚLĄ JAŹŃ"), "{czech_polish}");
    for name in ["testRTFJapanese", "testRTF-ms932"] {
        let text = import(name).markdown;
        assert!(
            text.chars().any(|c| ('\u{3040}'..='\u{9fff}').contains(&c)),
            "{name}: {text}"
        );
    }
}

#[test]
fn surrogate_pairs_combine_and_unpaired_ones_are_replaced() {
    let gothic = import("testRTFUnicodeGothic").markdown;
    let letters: Vec<char> = gothic.trim().chars().collect();
    assert_eq!(letters.len(), 6, "{gothic:?}");
    assert!(
        letters
            .iter()
            .all(|c| ('\u{10330}'..='\u{1034f}').contains(c)),
        "{gothic:?}"
    );
    let invalid = import("testRTFInvalidUnicode").markdown;
    assert!(
        invalid.contains("Unpaired hi") && invalid.contains("here"),
        "{invalid:?}"
    );
    assert!(invalid.contains('\u{fffd}'), "{invalid:?}");
}

#[test]
fn tables_hyperlinks_footnotes_and_lists_survive() {
    let table = import("testRTFTableCellSeparation").markdown;
    assert!(
        table.contains("| a | b |\n|---|---|\n| c | d |\n| ä | ë |\n| ö | ü |"),
        "{table}"
    );
    let links = import("testRTFHyperlink").markdown;
    assert!(
        links.contains("](http://r.office.microsoft.com/r/rlidwelcomeFAQ?clid=1033)"),
        "{links}"
    );
    let various = import("testRTFVarious").markdown;
    assert!(
        various.contains("[^1]") && various.contains("[^1]:"),
        "{various}"
    );
    assert!(various.contains("|---"), "{various}");
    for name in ["testRTFListMicrosoftWord", "testRTFListLibreOffice"] {
        let text = import(name).markdown;
        assert!(
            text.lines()
                .any(|l| l.starts_with("- ") || l.starts_with("1. ")),
            "{name}: {text}"
        );
    }
}

#[test]
fn pictures_become_assets_and_comments_are_reported() {
    let images = import("testRTFRegularImages");
    assert!(!images.assets.is_empty());
    for asset in &images.assets {
        assert!(
            asset.bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
            "{}",
            asset.relative_path
        );
        assert!(
            images.markdown.contains(&asset.relative_path),
            "{}",
            asset.relative_path
        );
    }
    for name in ["testComment", "testRTF_annotation_spacing"] {
        assert!(
            loss_kinds(&import(name)).contains(&LossKind::Comments),
            "{name}"
        );
    }
}

#[test]
fn markdown_round_trips_through_rtf_with_its_structure() {
    let source = "# Title\n\nSome **bold** and *italic* text with a [link](https://example.org/a).\n\n\
                  - one\n- two\n\n1. first\n2. second\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
    let rtf = markdown_to_rtf(source, &NoAssets);
    assert!(rtf.starts_with("{\\rtf1"));
    let back = import_rtf(rtf.as_bytes()).unwrap().markdown;
    assert!(back.starts_with("# Title\n"), "{back}");
    assert!(
        back.contains("**bold**") && back.contains("*italic*"),
        "{back}"
    );
    assert!(back.contains("[link](https://example.org/a)"), "{back}");
    assert!(back.contains("- one\n- two"), "{back}");
    assert!(back.contains("1. first\n2. second"), "{back}");
    assert!(back.contains("| a | b |\n|---|---|\n| 1 | 2 |"), "{back}");
}

#[test]
fn unicode_round_trips_through_rtf_escapes() {
    let text = "Çağrı — 日本語 😀 \\ { } end";
    let rtf = markdown_to_rtf(text, &NoAssets);
    assert!(rtf.is_ascii(), "RTF must be 7-bit");
    let back = import_rtf(rtf.as_bytes()).unwrap().markdown;
    assert!(back.contains("Çağrı — 日本語 😀"), "{back}");
    assert!(back.contains("end"), "{back}");
}
