//! The export pipeline (spec §9): every format writes a real file with the right magic bytes,
//! pages are numbered, attachments and TextBundle assets are copied to the folder their format
//! requires, diagrams export as PNG and SVG, and failures name the file.

use std::path::{Path, PathBuf};

use ssw_app::ui::export::{
    DiagramFormat, DiagramJob, ExportFailure, ExportJob, extension, format_of, layout_pages,
    run_diagram, run_export,
};
use ssw_convert::textbundle;
use ssw_core::command::CommandId;
use ssw_core::settings::{ExportFormat, ExportSettings};
use ssw_io::fs::RealFs;
use ssw_io::save::ThreadSleeper;
use ssw_ui::shell::export_dialog::ExportOptions;

const DOCUMENT: &str =
    "# Title\n\nBody with $x^2$ and **bold**.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

fn job(
    format: ExportFormat,
    markdown: &str,
    source_dir: Option<&Path>,
    target: PathBuf,
) -> ExportJob {
    ExportJob {
        format,
        options: ExportOptions::from(&ExportSettings::default()),
        markdown: markdown.to_owned(),
        plain: false,
        source_dir: source_dir.map(Path::to_path_buf),
        title: "Doc".to_owned(),
        target,
    }
}

fn run(job: &ExportJob) -> Result<Vec<PathBuf>, ExportFailure> {
    run_export(job, &RealFs, &ThreadSleeper)
}

#[test]
fn every_format_writes_a_file_with_its_magic_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let cases: [(ExportFormat, &[u8]); 11] = [
        (ExportFormat::Pdf, b"%PDF"),
        (ExportFormat::Png, PNG_SIGNATURE),
        (ExportFormat::Jpg, b"\xFF\xD8\xFF"),
        (ExportFormat::Html, b"<!DOCTYPE html>"),
        (ExportFormat::Epub, b"PK"),
        (ExportFormat::Docx, b"PK"),
        (ExportFormat::Odt, b"PK"),
        (ExportFormat::Rtf, b"{\\rtf"),
        (ExportFormat::Txt, b"Title"),
        (ExportFormat::MarkdownWithAttachments, b"# Title"),
        (ExportFormat::Textbundle, b"PK"),
    ];
    for (format, magic) in cases {
        let target = tmp
            .path()
            .join(format!("out-{format:?}.{}", extension(format)));
        let files = run(&job(format, DOCUMENT, None, target.clone())).unwrap();
        assert_eq!(files, std::slice::from_ref(&target), "{format:?}");
        let bytes = std::fs::read(&target).unwrap();
        assert!(
            bytes.starts_with(magic),
            "{format:?}: {:?}",
            &bytes[..bytes.len().min(16)]
        );
    }
}

#[test]
fn html_embeds_formulas_as_svg_and_local_images_as_data() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("pic one.png"), b"not really a png").unwrap();
    let target = tmp.path().join("out.html");
    let markdown = "![p](pic%20one.png)\n\nInline $x^2$ here.\n";
    run(&job(
        ExportFormat::Html,
        markdown,
        Some(tmp.path()),
        target.clone(),
    ))
    .unwrap();
    let html = std::fs::read_to_string(&target).unwrap();
    assert!(html.contains("<svg"), "{html}");
    assert!(html.contains("data:image/png;base64,"), "{html}");
}

#[test]
fn a_long_document_exports_one_numbered_png_per_page() {
    let tmp = tempfile::tempdir().unwrap();
    let markdown = "A line of text.\n\n".repeat(200);
    let target = tmp.path().join("long.png");
    let files = run(&job(ExportFormat::Png, &markdown, None, target)).unwrap();
    assert!(files.len() > 1, "{files:?}");
    for (index, file) in files.iter().enumerate() {
        assert_eq!(
            file.file_name().unwrap().to_string_lossy(),
            format!("long-{}.png", index + 1)
        );
        assert!(std::fs::read(file).unwrap().starts_with(PNG_SIGNATURE));
    }
}

#[test]
fn markdown_with_attachments_copies_files_to_attachments() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    std::fs::create_dir_all(source.join("img")).unwrap();
    std::fs::write(source.join("img").join("a.png"), b"PICTURE").unwrap();
    let out = tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let target = out.join("doc.md");
    run(&job(
        ExportFormat::MarkdownWithAttachments,
        "![a](img/a.png)\n",
        Some(&source),
        target.clone(),
    ))
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "![a](attachments/a.png)\n"
    );
    assert_eq!(
        std::fs::read(out.join("attachments").join("a.png")).unwrap(),
        b"PICTURE"
    );
}

#[test]
fn a_textbundle_keeps_its_files_in_assets() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("a.png"), b"PICTURE").unwrap();

    let pack = tmp.path().join("doc.textpack");
    run(&job(
        ExportFormat::Textbundle,
        "![a](a.png)\n",
        Some(&source),
        pack.clone(),
    ))
    .unwrap();
    let bundle = textbundle::read_pack(&std::fs::read(&pack).unwrap()).unwrap();
    assert_eq!(bundle.text, "![a](assets/a.png)\n");
    assert_eq!(bundle.assets.len(), 1);
    assert_eq!(bundle.assets[0].relative_path, "assets/a.png");
    assert_eq!(bundle.assets[0].bytes, b"PICTURE");

    let folder = tmp.path().join("doc.textbundle");
    run(&job(
        ExportFormat::Textbundle,
        "![a](a.png)\n",
        Some(&source),
        folder.clone(),
    ))
    .unwrap();
    let bundle = textbundle::read_dir(&folder).unwrap();
    assert_eq!(bundle.text, "![a](assets/a.png)\n");
    assert_eq!(bundle.assets[0].relative_path, "assets/a.png");
}

#[test]
fn a_diagram_exports_as_png_and_svg_and_an_unsupported_one_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let source = "graph TD\n  A --> B\n";
    let png = tmp.path().join("diagram.png");
    let svg = tmp.path().join("diagram.svg");
    for (format, target) in [(DiagramFormat::Png, &png), (DiagramFormat::Svg, &svg)] {
        let job = DiagramJob {
            source: source.to_owned(),
            format,
            target: target.clone(),
        };
        assert_eq!(
            run_diagram(&job, &RealFs, &ThreadSleeper).unwrap(),
            std::slice::from_ref(target)
        );
    }
    assert!(std::fs::read(&png).unwrap().starts_with(PNG_SIGNATURE));
    assert!(std::fs::read_to_string(&svg).unwrap().contains("<svg"));

    let unsupported = DiagramJob {
        source: "sankey-beta\n  a,b,1\n".to_owned(),
        format: DiagramFormat::Svg,
        target: tmp.path().join("none.svg"),
    };
    assert_eq!(
        run_diagram(&unsupported, &RealFs, &ThreadSleeper),
        Err(ExportFailure::Diagram)
    );
    assert!(!tmp.path().join("none.svg").exists());
}

#[test]
fn an_unwritable_target_names_the_file_and_leaves_nothing_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("missing folder").join("out.docx");
    let failure = run(&job(ExportFormat::Docx, DOCUMENT, None, target.clone())).unwrap_err();
    let ExportFailure::Write { path, .. } = failure else {
        panic!("expected a write failure, got {failure:?}");
    };
    assert_eq!(path, target);
    assert!(!tmp.path().join("missing folder").exists());
}

#[test]
fn printing_gets_one_scene_per_page() {
    let options = ExportOptions::from(&ExportSettings::default());
    let pages = layout_pages(&"A line of text.\n\n".repeat(200), None, &options, "Doc").unwrap();
    assert!(pages.len() > 1);
    assert!(
        pages
            .iter()
            .all(|page| page.size.width > 0.0 && page.size.height > 0.0)
    );
}

fn dark_job(format: ExportFormat, markdown: &str, target: PathBuf) -> ExportJob {
    let mut job = job(format, markdown, None, target);
    job.options.light_theme = false;
    job
}

#[test]
fn a_dark_png_has_dark_paper_and_the_light_one_white() {
    let tmp = tempfile::tempdir().unwrap();
    let dark = tmp.path().join("dark.png");
    let light = tmp.path().join("light.png");
    run(&dark_job(ExportFormat::Png, DOCUMENT, dark.clone())).unwrap();
    run(&job(ExportFormat::Png, DOCUMENT, None, light.clone())).unwrap();
    let pixel = |path: &Path| {
        image::load_from_memory(&std::fs::read(path).unwrap())
            .unwrap()
            .to_rgba8()
            .get_pixel(0, 0)
            .0
    };
    assert_eq!(pixel(&dark), [0x27, 0x27, 0x27, 255]);
    assert_eq!(pixel(&light), [255, 255, 255, 255]);
}

#[test]
fn black_text_keeps_the_paper_white_in_the_dark_theme() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("black.png");
    let mut job = dark_job(ExportFormat::Png, DOCUMENT, target.clone());
    job.options.black_text = true;
    run(&job).unwrap();
    let page = image::load_from_memory(&std::fs::read(&target).unwrap())
        .unwrap()
        .to_rgba8();
    assert_eq!(page.get_pixel(0, 0).0, [255, 255, 255, 255]);
}

#[test]
fn dark_html_carries_the_dark_sheet_and_light_formulas() {
    let tmp = tempfile::tempdir().unwrap();
    let html = tmp.path().join("dark.html");
    run(&dark_job(ExportFormat::Html, DOCUMENT, html.clone())).unwrap();
    let text = std::fs::read_to_string(&html).unwrap();
    assert!(
        text.contains("color-scheme:dark") && text.contains("#272727"),
        "{text}"
    );
    // The formula is drawn in the text color of the dark sheet, not in black.
    assert!(text.to_ascii_lowercase().contains("#e4e4e4"), "{text}");
    let light = tmp.path().join("light.html");
    run(&job(ExportFormat::Html, DOCUMENT, None, light.clone())).unwrap();
    assert!(
        !std::fs::read_to_string(&light)
            .unwrap()
            .contains("color-scheme:dark")
    );
}

#[test]
fn every_export_command_has_its_format() {
    let commands = [
        (CommandId::ExportPdf, ExportFormat::Pdf),
        (CommandId::ExportPng, ExportFormat::Png),
        (CommandId::ExportJpg, ExportFormat::Jpg),
        (CommandId::ExportHtml, ExportFormat::Html),
        (CommandId::ExportEpub, ExportFormat::Epub),
        (CommandId::ExportDocx, ExportFormat::Docx),
        (CommandId::ExportOdt, ExportFormat::Odt),
        (CommandId::ExportRtf, ExportFormat::Rtf),
        (CommandId::ExportTxt, ExportFormat::Txt),
        (
            CommandId::ExportMarkdownWithAttachments,
            ExportFormat::MarkdownWithAttachments,
        ),
        (CommandId::ExportTextBundle, ExportFormat::Textbundle),
    ];
    for (command, format) in commands {
        assert_eq!(format_of(command), Some(format), "{command}");
    }
    assert_eq!(format_of(CommandId::FileSave), None);
}
