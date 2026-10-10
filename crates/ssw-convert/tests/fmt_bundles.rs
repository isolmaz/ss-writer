//! TextBundle / TextPack read-write and "Markdown with attachments" export on real directories.

use std::path::Path;

use ssw_convert::import::Asset;
use ssw_convert::markdown_export::{ATTACHMENTS_DIR, MarkdownExportError, export_to_folder};
use ssw_convert::textbundle::{TextBundleError, read_dir, read_pack, write_dir, write_pack};

fn asset(path: &str, bytes: &[u8]) -> Asset {
    Asset {
        relative_path: path.to_string(),
        bytes: bytes.to_vec(),
    }
}

#[test]
fn textbundle_directory_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("note.textbundle");
    let assets = [
        asset("assets/a.png", b"\x89PNG\r\n\x1a\nA"),
        asset("assets/sub/b.bin", &[0, 1, 2, 255]),
    ];
    write_dir(&bundle, "# Note\n\n![a](assets/a.png)\n", &assets).unwrap();
    let info = std::fs::read_to_string(bundle.join("info.json")).unwrap();
    assert!(info.contains("net.daringfireball.markdown"), "{info}");
    assert_eq!(
        std::fs::read(bundle.join("assets/sub/b.bin")).unwrap(),
        [0, 1, 2, 255]
    );
    let back = read_dir(&bundle).unwrap();
    assert_eq!(back.text, "# Note\n\n![a](assets/a.png)\n");
    let mut got: Vec<(String, Vec<u8>)> = back
        .assets
        .iter()
        .map(|a| (a.relative_path.clone(), a.bytes.clone()))
        .collect();
    got.sort();
    let mut expected: Vec<(String, Vec<u8>)> = assets
        .iter()
        .map(|a| (a.relative_path.clone(), a.bytes.clone()))
        .collect();
    expected.sort();
    assert_eq!(got, expected);
}

#[test]
fn textpack_round_trip_and_unsafe_paths() {
    let assets = [asset("assets/é ü.png", b"data")];
    let pack = write_pack("text ünïcode\n", &assets).unwrap();
    assert!(pack.starts_with(b"PK\x03\x04"));
    let back = read_pack(&pack).unwrap();
    assert_eq!(back.text, "text ünïcode\n");
    assert_eq!(back.assets, assets);
    for bad in ["../evil.png", "/abs.png", "assets/../../x", ""] {
        let result = write_pack("x", &[asset(bad, b"x")]);
        assert!(
            matches!(result, Err(TextBundleError::InvalidAssetPath(_))),
            "{bad:?}"
        );
    }
    assert!(matches!(
        read_pack(b"not a zip"),
        Err(TextBundleError::Zip(_))
    ));
    let empty = write_pack("", &[]).unwrap();
    assert_eq!(read_pack(&empty).unwrap().text, "");
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn markdown_export_copies_attachments_and_rewrites_links() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    write(&source.path().join("pics/a b.png"), b"PNG-A");
    write(&source.path().join("docs/report.pdf"), b"%PDF-1.4");
    let md = "![pic](pics/a%20b.png)\n\n[report](docs/report.pdf)\n\n![web](https://x.org/r.png)\n\n![gone](nope.png)\n";
    let report = export_to_folder(md, source.path(), target.path(), "out.md").unwrap();
    assert_eq!(report.markdown_path, target.path().join("out.md"));
    assert_eq!(report.missing, ["nope.png"]);
    assert_eq!(report.attachments.len(), 2);
    for relative in &report.attachments {
        assert!(
            relative.starts_with(&format!("{ATTACHMENTS_DIR}/")),
            "{relative}"
        );
    }
    let mut copied: Vec<Vec<u8>> = report
        .attachments
        .iter()
        .map(|r| std::fs::read(target.path().join(r)).unwrap())
        .collect();
    copied.sort();
    assert_eq!(copied, [b"%PDF-1.4".to_vec(), b"PNG-A".to_vec()]);
    let written = std::fs::read_to_string(&report.markdown_path).unwrap();
    assert_eq!(
        written.matches(&format!("({ATTACHMENTS_DIR}/")).count(),
        2,
        "{written}"
    );
    assert!(
        written.contains("![web](https://x.org/r.png)") && written.contains("![gone](nope.png)")
    );
    assert!(
        !written.contains("pics/") && !written.contains("docs/"),
        "{written}"
    );
}

#[test]
fn markdown_export_rejects_bad_file_names() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["", ".", "..", "a/b.md", "a\\b.md", "c:x.md"] {
        let result = export_to_folder("x", dir.path(), dir.path(), name);
        assert!(
            matches!(result, Err(MarkdownExportError::InvalidFileName(_))),
            "{name:?}"
        );
    }
}
