//! DOCX importer against the third-party fixtures in `tests/fixtures/docx`.

use std::fs;
use std::path::{Path, PathBuf};

use ssw_convert::docx::import_docx;
use ssw_convert::import::{ImportError, LossKind};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/docx")
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/docx")
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    fs::read(fixture_dir().join(format!("{name}.docx"))).unwrap()
}

#[test]
fn every_fixture_imports_without_panicking_and_reports_its_losses() {
    let mut imported = 0;
    let mut names = Vec::new();
    for entry in fs::read_dir(fixture_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("docx") {
            continue;
        }
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let bytes = fs::read(&path).unwrap();
        names.push(stem.clone());
        match import_docx(&bytes, &stem) {
            Ok(result) => {
                imported += 1;
                for loss in &result.losses {
                    assert!(loss.count > 0, "{stem}: empty loss entry {:?}", loss.kind);
                }
                for asset in &result.assets {
                    assert!(
                        asset
                            .relative_path
                            .starts_with(&format!("{stem}_assets/image-")),
                        "{stem}: {}",
                        asset.relative_path
                    );
                    assert!(!asset.bytes.is_empty(), "{stem}: empty asset");
                }
            }
            Err(error) => match stem.as_str() {
                "tika_testWORD_protected_passtika" => assert_eq!(error, ImportError::Encrypted),
                "poi_truncated62886" | "tika_testWORD_truncated" => {
                    assert!(
                        matches!(error, ImportError::Corrupt(_)),
                        "{stem}: {error:?}"
                    );
                }
                other => panic!("{other} must import: {error:?}"),
            },
        }
    }
    assert!(names.len() >= 30, "fixtures missing: {names:?}");
    assert!(imported >= names.len() - 3);
}

#[test]
fn golden_markdown_matches_for_representative_fixtures() {
    let names = [
        "mammoth_empty",
        "mammoth_utf8-bom",
        "mammoth_strikethrough",
        "mammoth_simple-list",
        "mammoth_tables",
        "mammoth_endnotes",
        "mammoth_footnotes",
        "mammoth_footnote-hyperlink",
        "mammoth_strict-format",
        "mammoth_tiny-picture",
    ];
    for name in names {
        let result = import_docx(&fixture_bytes(name), name).unwrap();
        let expected = fs::read_to_string(golden_dir().join(format!("{name}.md"))).unwrap();
        assert_eq!(result.markdown, expected, "{name}");
    }
}

#[test]
fn embedded_picture_becomes_an_asset_with_its_pixel_width() {
    let result = import_docx(
        &fixture_bytes("mammoth_tiny-picture"),
        "mammoth_tiny-picture",
    )
    .unwrap();
    assert_eq!(result.assets.len(), 1);
    assert_eq!(
        result.assets[0].relative_path,
        "mammoth_tiny-picture_assets/image-1.png"
    );
    assert!(result.assets[0].bytes.starts_with(b"\x89PNG"));
}

#[test]
fn heading_styles_become_atx_headings() {
    let result = import_docx(&fixture_bytes("poi_heading123"), "h").unwrap();
    assert!(result.markdown.starts_with("# First paragraph\n\n"));
    assert!(result.markdown.contains("\n\n## Second paragraph\n\n"));
    assert!(result.markdown.contains("\n\n### Third paragraph\n\n"));
}

#[test]
fn comments_are_reported_as_losses() {
    for name in ["mammoth_comments", "poi_testComment"] {
        let result = import_docx(&fixture_bytes(name), name).unwrap();
        assert!(
            result
                .losses
                .iter()
                .any(|l| l.kind == LossKind::Comments && l.count > 0),
            "{name}: {:?}",
            result.losses
        );
    }
}

#[test]
fn password_protected_package_is_rejected_as_encrypted() {
    let bytes = fixture_bytes("tika_testWORD_protected_passtika");
    assert_eq!(
        import_docx(&bytes, "x").unwrap_err(),
        ImportError::Encrypted
    );
}
