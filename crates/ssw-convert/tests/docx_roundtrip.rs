//! DOCX export: package structure, schema element order, and write -> read round trips.

use ssw_convert::docx::{ExportOptions, MathImage, Resources, export_docx, import_docx};
use ssw_convert::ooxml::{Element, Ns, Package, parse_xml};

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes
}

fn export(markdown: &str, options: &ExportOptions) -> Vec<u8> {
    let mut image = |path: &str| -> Option<Vec<u8>> {
        match path {
            "pic.png" => Some(png(20, 10)),
            "wide.png" => Some(png(40, 30)),
            _ => None,
        }
    };
    let mut math = |_: &str, _: bool| -> Option<MathImage> { None };
    let mut resources = Resources {
        image: &mut image,
        math: &mut math,
    };
    export_docx(markdown, options, &mut resources).unwrap()
}

fn part(bytes: &[u8], name: &str) -> Element {
    Package::open(bytes)
        .unwrap()
        .read_xml(name)
        .unwrap()
        .unwrap()
}

const SAMPLE: &str = "# Title\n\nSome **bold** and *italic* text with ~~strike~~ and a [link](https://example.com/a).\n\n- one\n- two\n\n1. first\n2. second\n\n| A | B |\n| :-- | --: |\n| 1 | 2 |\n\n![alt text](pic.png)\n";

#[test]
fn package_has_the_required_parts_and_content_types() {
    let bytes = export(SAMPLE, &ExportOptions::default());
    let mut package = Package::open(&bytes).unwrap();
    for name in [
        "[Content_Types].xml",
        "_rels/.rels",
        "docProps/core.xml",
        "docProps/app.xml",
        "word/document.xml",
        "word/_rels/document.xml.rels",
        "word/styles.xml",
        "word/numbering.xml",
        "word/settings.xml",
        "word/footnotes.xml",
        "word/media/image1.png",
    ] {
        assert!(package.contains(name), "{name} missing");
    }
    assert_eq!(package.part_names()[0], "[Content_Types].xml");
    let types = package.read_xml("[Content_Types].xml").unwrap().unwrap();
    let overrides: Vec<&str> = types
        .elements()
        .filter(|e| e.name == "Override")
        .filter_map(|e| e.attr("PartName"))
        .collect();
    assert!(overrides.contains(&"/word/document.xml"));
    assert!(overrides.contains(&"/word/styles.xml"));
    assert!(
        types
            .elements()
            .any(|e| e.name == "Default" && e.attr("Extension") == Some("png"))
    );
}

fn rank(order: &[&str], name: &str) -> usize {
    order
        .iter()
        .position(|n| *n == name)
        .unwrap_or_else(|| panic!("unexpected child {name}"))
}

fn assert_child_order(element: &Element, order: &[&str]) {
    let ranks: Vec<usize> = element.elements().map(|c| rank(order, &c.name)).collect();
    assert!(
        ranks.windows(2).all(|w| w[0] < w[1]),
        "{} children out of schema order: {:?}",
        element.name,
        element
            .elements()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn document_children_follow_the_schema_sequence() {
    let bytes = export(SAMPLE, &ExportOptions::default());
    let document = part(&bytes, "word/document.xml");
    let body = document.child(Ns::W, "body").unwrap();
    assert_eq!(body.elements().last().unwrap().name, "sectPr");
    assert_child_order(body.child(Ns::W, "sectPr").unwrap(), &["pgSz", "pgMar"]);
    for ppr in document.descendants(Ns::W, "pPr") {
        assert_child_order(ppr, &["pStyle", "numPr", "pBdr", "spacing", "ind", "jc"]);
    }
    for rpr in document.descendants(Ns::W, "rPr") {
        assert_child_order(
            rpr,
            &["rStyle", "b", "i", "strike", "highlight", "u", "shd"],
        );
    }
    for p in document.descendants(Ns::W, "p") {
        if let Some(index) = p.elements().position(|c| c.name == "pPr") {
            assert_eq!(index, 0, "pPr must precede every run");
        }
    }
    let tables = document.descendants(Ns::W, "tbl");
    assert_eq!(tables.len(), 1);
    assert_child_order(
        tables[0].child(Ns::W, "tblPr").unwrap(),
        &[
            "tblStyle",
            "tblW",
            "jc",
            "tblBorders",
            "tblLayout",
            "tblCellMar",
            "tblLook",
        ],
    );
    let order: Vec<&str> = tables[0].elements().map(|c| c.name.as_str()).collect();
    assert_eq!(order, ["tblPr", "tblGrid", "tr", "tr"]);
    for row in document.descendants(Ns::W, "tr") {
        let children: Vec<&str> = row.elements().map(|c| c.name.as_str()).collect();
        assert_eq!(children, ["trPr", "tc", "tc"]);
        for cell in row.elements().filter(|c| c.name == "tc") {
            assert_child_order(
                cell.child(Ns::W, "tcPr").unwrap(),
                &["tcW", "gridSpan", "shd"],
            );
            assert_eq!(cell.elements().last().unwrap().name, "p");
        }
    }
}

#[test]
fn only_the_header_row_repeats_and_alignment_comes_from_gfm() {
    let bytes = export(SAMPLE, &ExportOptions::default());
    let document = part(&bytes, "word/document.xml");
    let rows = document.descendants(Ns::W, "tr");
    let header = rows[0].child(Ns::W, "trPr").unwrap();
    assert!(header.child(Ns::W, "tblHeader").is_some());
    assert!(
        rows[1]
            .child(Ns::W, "trPr")
            .unwrap()
            .child(Ns::W, "tblHeader")
            .is_none()
    );
    let alignments: Vec<Vec<&str>> = rows
        .iter()
        .map(|row| {
            row.descendants(Ns::W, "jc")
                .iter()
                .filter_map(|jc| jc.attr_ns(Ns::W, "val"))
                .collect()
        })
        .collect();
    assert_eq!(alignments, [["left", "right"], ["left", "right"]]);
}

#[test]
fn core_properties_carry_title_and_dates() {
    let options = ExportOptions {
        title: Some("My <Title> & more".to_string()),
        created: Some("2026-01-02T03:04:05Z".to_string()),
        modified: Some("2026-02-03T04:05:06Z".to_string()),
        ..ExportOptions::default()
    };
    let bytes = export("text\n", &options);
    let core = part(&bytes, "docProps/core.xml");
    let text_of = |name: &str| {
        core.elements()
            .find(|e| e.name == name)
            .map(Element::text)
            .unwrap_or_default()
    };
    assert_eq!(text_of("title"), "My <Title> & more");
    assert_eq!(text_of("created"), "2026-01-02T03:04:05Z");
    assert_eq!(text_of("modified"), "2026-02-03T04:05:06Z");
    let untitled = part(
        &export("text\n", &ExportOptions::default()),
        "docProps/core.xml",
    );
    assert!(untitled.elements().all(|e| e.name != "title"));
}

#[test]
fn images_carry_alt_text_and_unique_drawing_ids() {
    let bytes = export(
        "![first alt](pic.png)\n\n![second \"alt\"](wide.png)\n",
        &ExportOptions::default(),
    );
    let document = part(&bytes, "word/document.xml");
    let props = document.descendants(Ns::Wp, "docPr");
    assert_eq!(props.len(), 2);
    assert_eq!(props[0].attr("descr"), Some("first alt"));
    assert_eq!(props[1].attr("descr"), Some("second \"alt\""));
    assert_ne!(props[0].attr("id"), props[1].attr("id"));
    let extents: Vec<(&str, &str)> = document
        .descendants(Ns::Wp, "extent")
        .iter()
        .map(|e| (e.attr("cx").unwrap(), e.attr("cy").unwrap()))
        .collect();
    assert_eq!(extents, [("190500", "95250"), ("381000", "285750")]);
    let mut package = Package::open(&bytes).unwrap();
    assert_eq!(
        package.read_part("word/media/image1.png").unwrap().unwrap(),
        png(20, 10)
    );
    assert_eq!(
        package.read_part("word/media/image2.png").unwrap().unwrap(),
        png(40, 30)
    );
}

#[test]
fn missing_images_fall_back_to_alt_text() {
    let bytes = export("![nothing here](missing.png)\n", &ExportOptions::default());
    let document = part(&bytes, "word/document.xml");
    assert!(document.descendants(Ns::Wp, "docPr").is_empty());
    assert!(document.text().contains("nothing here"));
}

#[test]
fn footnotes_are_real_notes_with_separators() {
    let bytes = export(
        "Text[^a] more.\n\n[^a]: The note.\n",
        &ExportOptions::default(),
    );
    let notes = part(&bytes, "word/footnotes.xml");
    let ids: Vec<(&str, Option<&str>)> = notes
        .elements()
        .map(|n| (n.attr_ns(Ns::W, "id").unwrap(), n.attr_ns(Ns::W, "type")))
        .collect();
    assert_eq!(
        ids,
        [
            ("-1", Some("separator")),
            ("0", Some("continuationSeparator")),
            ("1", None)
        ]
    );
    assert!(notes.text().contains("The note."));
    let document = part(&bytes, "word/document.xml");
    let reference = &document.descendants(Ns::W, "footnoteReference")[0];
    assert_eq!(reference.attr_ns(Ns::W, "id"), Some("1"));
}

#[test]
fn links_use_relationships_and_heading_bookmarks() {
    let bytes = export(
        "# Intro Part\n\n[web](https://example.com/?a=1&b=2) and [jump](#intro-part)\n",
        &ExportOptions::default(),
    );
    let document = part(&bytes, "word/document.xml");
    let links = document.descendants(Ns::W, "hyperlink");
    assert_eq!(links.len(), 2);
    let rid = links[0].attr_ns(Ns::R, "id").unwrap();
    let anchor = links[1].attr_ns(Ns::W, "anchor").unwrap();
    let bookmarks = document.descendants(Ns::W, "bookmarkStart");
    assert_eq!(bookmarks.len(), 1);
    assert_eq!(bookmarks[0].attr_ns(Ns::W, "name"), Some(anchor));
    let rels = part(&bytes, "word/_rels/document.xml.rels");
    let target = rels.elements().find(|r| r.attr("Id") == Some(rid)).unwrap();
    assert_eq!(target.attr("Target"), Some("https://example.com/?a=1&b=2"));
    assert_eq!(target.attr("TargetMode"), Some("External"));
}

#[test]
fn written_parts_are_well_formed_xml() {
    let bytes = export(SAMPLE, &ExportOptions::default());
    let mut package = Package::open(&bytes).unwrap();
    let names: Vec<String> = package.part_names().to_vec();
    for name in names {
        if name.ends_with(".xml") || name.ends_with(".rels") {
            let data = package.read_part(&name).unwrap().unwrap();
            parse_xml(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
}

#[test]
fn rich_markdown_survives_a_write_read_round_trip() {
    let bytes = export(SAMPLE, &ExportOptions::default());
    let result = import_docx(&bytes, "sample").unwrap();
    let md = &result.markdown;
    assert!(md.starts_with("# Title\n\n"), "{md}");
    assert!(md.contains("**bold**"), "{md}");
    assert!(md.contains("*italic*"), "{md}");
    assert!(md.contains("~~strike~~"), "{md}");
    assert!(md.contains("[link](https://example.com/a)"), "{md}");
    assert!(md.contains("- one\n- two"), "{md}");
    // Adjacent lists alternate their delimiter so they stay separate lists.
    assert!(md.contains("- one\n- two\n1) first\n2) second"), "{md}");
    assert!(md.contains("| A | B |\n| --- | ---: |"), "{md}");
    assert!(md.contains("| 1 | 2 |"), "{md}");
    assert!(
        md.contains("![alt text|20](sample_assets/image-1.png)"),
        "{md}"
    );
    assert_eq!(result.assets.len(), 1);
    assert_eq!(result.assets[0].bytes, png(20, 10));
}

#[test]
fn footnotes_and_page_breaks_round_trip() {
    let bytes = export(
        "Text[^n] here.\n\n<!-- pagebreak -->\n\nNext page.\n\n[^n]: The note.\n",
        &ExportOptions::default(),
    );
    let result = import_docx(&bytes, "n").unwrap();
    assert!(
        result.markdown.contains("Text[^1] here."),
        "{}",
        result.markdown
    );
    assert!(
        result.markdown.contains("<!-- pagebreak -->"),
        "{}",
        result.markdown
    );
    assert!(
        result.markdown.contains("[^1]: The note."),
        "{}",
        result.markdown
    );
}
