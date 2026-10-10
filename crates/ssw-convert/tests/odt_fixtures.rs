//! ODT importer over the real fixtures: goldens, losses, refusals and never-panic corruption.

use std::path::PathBuf;

use ssw_convert::import::{ImportError, ImportResult, LossKind};
use ssw_convert::odt::import_odt;

fn fixture_dir(sub: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(sub)
}

fn fixture(name: &str) -> Vec<u8> {
    let path = fixture_dir("odt").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn golden(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/odt")
        .join(format!("{name}.md"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn import(name: &str) -> ImportResult {
    import_odt(&fixture(name), "odt").unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn kinds(result: &ImportResult) -> Vec<LossKind> {
    result.sorted_losses().iter().map(|l| l.kind).collect()
}

#[test]
fn golden_markdown() {
    let names = [
        "LibreOfficeWriter_odt_1.3",
        "testOpenOffice2",
        "testODTEmbeddedImageLink",
        "testStyles",
        "testFooter",
        "testODTStyles2",
        "testODTStyles3",
        "testNPEOpenDocument",
        "testODFwithOOo3",
        "testODT_svgTitleInStyledSpan",
    ];
    for name in names {
        assert_eq!(
            import(&format!("{name}.odt")).markdown,
            golden(name),
            "{name}"
        );
    }
}

/// The loss report of each golden document, counted by hand from its `content.xml`.
#[test]
fn golden_loss_reports() {
    use LossKind::*;
    let cases: [(&str, &[(LossKind, usize)]); 9] = [
        ("LibreOfficeWriter_odt_1.3", &[]),
        ("testOpenOffice2", &[]),
        ("testStyles", &[]),
        ("testFooter", &[(HeadersFooters, 1)]),
        (
            "testODTStyles2",
            &[(MergedCells, 2), (Alignment, 4), (MultiParagraphCells, 1)],
        ),
        (
            "testODTStyles3",
            &[
                (MergedCells, 2),
                (Comments, 2),
                (Alignment, 1),
                (MultiParagraphCells, 1),
            ],
        ),
        ("testNPEOpenDocument", &[]),
        (
            "testODFwithOOo3",
            &[
                (Comments, 1),
                (TrackedChanges, 3),
                (VectorShapes, 1),
                (UnsupportedEquations, 2),
                (Alignment, 1),
                (Fields, 1),
            ],
        ),
        (
            "testODT_svgTitleInStyledSpan",
            &[
                (HeadersFooters, 1),
                (VectorShapes, 3),
                (Alignment, 1),
                (MultiParagraphCells, 2),
            ],
        ),
    ];
    for (name, want) in cases {
        let got: Vec<(LossKind, usize)> = import(&format!("{name}.odt"))
            .sorted_losses()
            .iter()
            .map(|l| (l.kind, l.count))
            .collect();
        assert_eq!(got, want, "{name}");
    }
}

#[test]
fn linked_image_is_extracted_as_an_asset() {
    let result = import("testODTEmbeddedImageLink.odt");
    assert_eq!(result.assets.len(), 1);
    assert_eq!(result.assets[0].relative_path, "odt_assets/image-1.png");
    assert!(
        result.assets[0]
            .bytes
            .starts_with(&[0x89, b'P', b'N', b'G'])
    );
    assert!(result.losses.is_empty());
}

#[test]
fn headings_links_and_lists() {
    let md = import("testNPEOpenDocument.odt").markdown;
    assert!(md.starts_with("## [Pr\u{e1}ctica de GnuPG](http://ceugenio.wordpress.com/2006/08/14/practica-de-gnupg/)\n\n"), "{md}");
    assert!(
        md.contains("[Pr\u{e1}ctica sobre GnuPG](http://bulma.net/body.phtml?nIdNoticia=2331)")
    );
    assert!(
        md.contains("\n1. **Enunciado de la pr\u{e1}ctica**\n2. **Generar la clave**\n"),
        "{md}"
    );
    assert!(
        md.contains("\n- \u{bf}Tipo de clave? seleccionar la opci\u{f3}n predeterminada: 1\n"),
        "{md}"
    );
}

#[test]
fn rich_document_reports_what_markdown_cannot_carry() {
    let result = import("testODFwithOOo3.odt");
    let md = &result.markdown;
    assert!(md.starts_with("# Apache Tika Test Document\n\n"), "{md}");
    assert!(md.contains("[^1]"), "{md}");
    assert!(
        md.contains("[^1]: This is of course a simple footnote"),
        "{md}"
    );
    assert!(
        md.contains("1. First item\n2. Second item\n3. Third item"),
        "{md}"
    );
    assert!(
        md.contains("[Lucene website](http://lucene.apache.org/)"),
        "{md}"
    );
    let found = kinds(&result);
    for kind in [
        LossKind::TrackedChanges,
        LossKind::Comments,
        LossKind::VectorShapes,
    ] {
        assert!(found.contains(&kind), "{kind:?} in {found:?}");
    }
}

#[test]
fn tables_with_merged_cells_report_the_loss() {
    for name in ["testODTStyles2.odt", "testODTStyles3.odt"] {
        let result = import(name);
        assert!(
            result.markdown.contains("| --- |"),
            "{name}: {}",
            result.markdown
        );
        assert!(kinds(&result).contains(&LossKind::MergedCells), "{name}");
    }
    assert!(kinds(&import("testODTStyles3.odt")).contains(&LossKind::Comments));
}

#[test]
fn footers_are_reported() {
    assert!(kinds(&import("testFooter.odt")).contains(&LossKind::HeadersFooters));
}

#[test]
fn table_with_non_latin_text_imports() {
    let result = import("testODT_svgTitleInStyledSpan.odt");
    assert!(result.markdown.contains("| :-: |"), "{}", result.markdown);
}

#[test]
fn encrypted_and_foreign_files_are_refused() {
    assert_eq!(
        import_odt(&fixture("testODTEncrypted.odt"), "x").unwrap_err(),
        ImportError::Encrypted
    );
    assert_eq!(
        import_odt(&fixture("testODTnotaZipFile.odt"), "x").unwrap_err(),
        ImportError::WrongFormat {
            expected: "ODT",
            detected: "text"
        }
    );
    let docx = std::fs::read(fixture_dir("docx").join("mammoth_empty.docx")).unwrap();
    assert_eq!(
        import_odt(&docx, "x").unwrap_err(),
        ImportError::WrongFormat {
            expected: "ODT",
            detected: "DOCX"
        }
    );
    let cases: [(&[u8], &str); 3] = [
        (b"{\\rtf1\\ansi x}", "RTF"),
        (b"%PDF-1.5", "PDF"),
        (b"<html><body></body></html>", "HTML"),
    ];
    for (bytes, detected) in cases {
        assert_eq!(
            import_odt(bytes, "x").unwrap_err(),
            ImportError::WrongFormat {
                expected: "ODT",
                detected
            }
        );
    }
    let ole = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0];
    assert_eq!(
        import_odt(&ole, "x").unwrap_err(),
        ImportError::WrongFormat {
            expected: "ODT",
            detected: "DOC"
        }
    );
    assert!(matches!(
        import_odt(b"PK\x03\x04junk", "x"),
        Err(ImportError::Corrupt(_))
    ));
}

#[test]
fn every_fixture_imports_or_fails_cleanly() {
    let dir = fixture_dir("odt");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(names.len() >= 12);
    for name in names {
        let _ = import_odt(&fixture(&name), "assets");
    }
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
        "testStyles.odt",
        "testODFwithOOo3.odt",
        "testODTStyles2.odt",
        "testODTEmbeddedImageLink.odt",
    ] {
        let bytes = fixture(name);
        for cut in (0..bytes.len()).step_by(499) {
            let _ = import_odt(&bytes[..cut], "a");
        }
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..60 {
            let mut copy = bytes.clone();
            for _ in 0..8 {
                let i = (next(&mut state) % copy.len() as u64) as usize;
                copy[i] ^= 1 << (next(&mut state) % 8);
            }
            let _ = import_odt(&copy, "a");
        }
    }
}
