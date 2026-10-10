//! Markdown → ODT → Markdown: what the exporter writes, the importer reads back.

use std::io::Read;

use ssw_convert::docx::{ExportOptions, MathImage, Resources};
use ssw_convert::odt::{export_odt, import_odt};

/// A 2x1 PNG (IHDR only is enough for the exporter, which reads the size).
fn png() -> Vec<u8> {
    let mut v = vec![
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
    ];
    v.extend_from_slice(&2u32.to_be_bytes());
    v.extend_from_slice(&1u32.to_be_bytes());
    v.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
    v
}

fn export(md: &str) -> Vec<u8> {
    let mut image = |dest: &str| (dest == "pic.png").then(png);
    let mut math = |_: &str, _: bool| -> Option<MathImage> { None };
    let mut resources = Resources {
        image: &mut image,
        math: &mut math,
    };
    export_odt(md, &ExportOptions::default(), &mut resources).unwrap()
}

fn round_trip(md: &str) -> String {
    let odt = export(md);
    import_odt(&odt, "doc").unwrap().markdown
}

#[test]
fn constructs_survive_the_round_trip() {
    let cases = [
        "# Title\n",
        "###### Deep\n",
        "Some **bold** and *italic* and ~u~ and ~~s~~ and ==h==.\n",
        "A `code` span and a [link](https://e.com/a_b).\n",
        "- a\n- b\n",
        "3. a\n4. b\n",
        "- a\n    - b\n",
        "- [ ] a\n- [x] b\n",
        "> quote\n",
        "```\nx = 1\n  y\n```\n",
        "---\n",
        "a\n\n<!-- pagebreak -->\n\nb\n",
        "| a | b |\n| --- | --- |\n| 1 | 2 |\n",
        "a[^1]\n\n[^1]: The note.\n",
        "# One\n\n[jump](#one)\n",
    ];
    for md in cases {
        assert_eq!(round_trip(md), md, "{md:?}");
    }
}

#[test]
fn package_layout_follows_the_standard() {
    let odt = export("# T\n\n![pic](pic.png)\n");
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(odt)).unwrap();
    let first = archive.by_index(0).unwrap();
    assert_eq!(first.name_raw(), b"mimetype");
    assert_eq!(first.compression(), zip::CompressionMethod::Stored);
    drop(first);
    let mut mime = String::new();
    archive
        .by_name("mimetype")
        .unwrap()
        .read_to_string(&mut mime)
        .unwrap();
    assert_eq!(mime, "application/vnd.oasis.opendocument.text");
    for name in [
        "META-INF/manifest.xml",
        "content.xml",
        "styles.xml",
        "meta.xml",
        "Pictures/image1.png",
    ] {
        assert!(archive.by_name(name).is_ok(), "{name}");
    }
}

#[test]
fn every_part_is_listed_in_the_manifest() {
    let odt = export("# T\n\n![pic](pic.png)\n\ntext\n");
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(odt)).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
        .collect();
    assert_eq!(names[0], "mimetype");
    let mut manifest = String::new();
    archive
        .by_name("META-INF/manifest.xml")
        .unwrap()
        .read_to_string(&mut manifest)
        .unwrap();
    assert!(
        manifest.contains(r#"manifest:full-path="/""#)
            && manifest.contains("application/vnd.oasis.opendocument.text"),
        "{manifest}"
    );
    for name in &names {
        if name == "mimetype" || name == "META-INF/manifest.xml" {
            continue;
        }
        assert!(
            manifest.contains(&format!(r#"manifest:full-path="{name}""#)),
            "{name} is not in the manifest: {manifest}"
        );
    }
    assert!(names.iter().any(|n| n == "Pictures/image1.png"));
    assert!(manifest.contains(r#"manifest:media-type="image/png""#));
}

#[test]
fn rich_document_round_trips_exactly() {
    let md = concat!(
        "# Title\n\n",
        "Some **bold** and *italic* and ~u~ and ~~s~~ and ==h==.\n\n",
        "A `code` span and a [link](https://e.com/a_b) and [jump](#title).\n\n",
        "- a\n    - b\n- c\n\n",
        "Text between.\n\n",
        "3. x\n4. y\n\n",
        "More text.\n\n",
        "- [ ] open\n- [x] done\n\n",
        "> quote\n\n",
        "```\nx = 1\n  y\n```\n\n",
        "---\n\n",
        "a[^1]\n\n",
        "<!-- pagebreak -->\n\n",
        "| a | b | c | d |\n| :-- | :-: | --: | --- |\n| 1 | 2 | 3 | 4 |\n| 5 | 6 | 7 | 8 |\n\n",
        "[^1]: The note.\n",
    );
    assert_eq!(round_trip(md), md);
}

#[test]
fn table_alignment_survives_without_a_loss_report() {
    for md in ["| a |\n| :-: |\n", "| a | b |\n| --: | :-- |\n| 1 | 2 |\n"] {
        let odt = export(md);
        let result = import_odt(&odt, "doc").unwrap();
        assert_eq!(result.markdown, md);
        assert!(result.losses.is_empty(), "{:?}", result.losses);
    }
}

#[test]
fn images_become_assets_again() {
    let odt = export("before ![pic|40](pic.png) after\n");
    let result = import_odt(&odt, "doc").unwrap();
    assert_eq!(
        result.markdown,
        "before ![pic|40](doc_assets/image-1.png) after\n"
    );
    assert_eq!(result.assets.len(), 1);
    assert_eq!(result.assets[0].bytes, png());
}

#[test]
fn exported_documents_report_no_losses() {
    let result = import_odt(
        &export("# H\n\ntext **b**\n\n> q\n\n`c`\n\n| a |\n| - |\n| 1 |\n"),
        "d",
    )
    .unwrap();
    assert!(result.losses.is_empty(), "{:?}", result.losses);
}
