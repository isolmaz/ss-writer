//! The export jobs on real files (spec §9): what they do with pictures they cannot use,
//! diagrams on light and dark paper, numbered pages without an extension and every way a
//! target, an attachment folder or a source file can refuse the work. Real temporary folders,
//! real locks; nothing is shown.

use std::io::Cursor;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use ssw_app::ui::export::{
    DiagramFormat, DiagramJob, ExportFailure, ExportJob, layout_pages, run_diagram, run_export,
};
use ssw_convert::ooxml::Package;
use ssw_core::settings::{ExportFormat, ExportSettings, PageSize};
use ssw_io::fs::RealFs;
use ssw_io::save::ThreadSleeper;
use ssw_ui::shell::export_dialog::ExportOptions;

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const DIAGRAM: &str = "```mermaid\ngraph TD\n  A --> B\n```\n";

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

fn dark(mut job: ExportJob) -> ExportJob {
    job.options.light_theme = false;
    job
}

/// A real 2x2 PNG picture.
fn picture() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap();
    bytes
}

/// The write failure of `failure`: the file it names.
fn written_path(failure: ExportFailure) -> PathBuf {
    match failure {
        ExportFailure::Write { path, .. } => path,
        other => panic!("expected a write failure, got {other:?}"),
    }
}

/// The first `<svg …>…</svg>` of `html`.
fn svg_of(html: &str) -> &str {
    let start = html.find("<svg").expect("an inline picture");
    let end = html[start..].find("</svg>").expect("a closed picture") + start + "</svg>".len();
    &html[start..end]
}

/// Opens `path` so that nobody else can read or write it while the guard lives.
fn locked(path: &Path) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .unwrap()
}

#[test]
fn a_diagram_format_names_its_file_extension() {
    assert_eq!(DiagramFormat::Png.extension(), "png");
    assert_eq!(DiagramFormat::Svg.extension(), "svg");
}

#[test]
fn a_picture_typst_cannot_decode_fails_the_page_formats_with_the_layout_message() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("broken.png"), b"this is not a picture").unwrap();
    let markdown = "![x](broken.png)\n";
    for (format, name) in [
        (ExportFormat::Pdf, "out.pdf"),
        (ExportFormat::Png, "out.png"),
        (ExportFormat::Jpg, "out.jpg"),
    ] {
        let target = tmp.path().join(name);
        let failure = run(&job(format, markdown, Some(tmp.path()), target.clone())).unwrap_err();
        let ExportFailure::Convert(message) = failure else {
            panic!("{format:?}: expected a conversion failure, got {failure:?}");
        };
        assert!(message.contains("layout failed"), "{format:?}: {message}");
        assert!(!target.exists(), "{format:?}: nothing is written");
    }
    // Printing lays the same document out and fails the same way.
    let options = ExportOptions::from(&ExportSettings::default());
    let failure = layout_pages(markdown, Some(tmp.path()), &options, "Doc").unwrap_err();
    assert!(matches!(failure, ExportFailure::Convert(_)), "{failure:?}");
}

#[test]
fn diagrams_are_drawn_for_the_paper_the_document_goes_on() {
    let tmp = tempfile::tempdir().unwrap();
    let (light, night) = (tmp.path().join("light.html"), tmp.path().join("dark.html"));
    run(&job(ExportFormat::Html, DIAGRAM, None, light.clone())).unwrap();
    run(&dark(job(ExportFormat::Html, DIAGRAM, None, night.clone()))).unwrap();
    let (light, night) = (
        std::fs::read_to_string(light).unwrap(),
        std::fs::read_to_string(night).unwrap(),
    );
    assert_ne!(svg_of(&light), svg_of(&night), "dark paper, dark diagram");

    // The page formats draw it too; an undrawable diagram is shown as code instead.
    let drawn = tmp.path().join("drawn.png");
    let code = tmp.path().join("code.png");
    run(&job(ExportFormat::Png, DIAGRAM, None, drawn.clone())).unwrap();
    run(&job(
        ExportFormat::Png,
        "```mermaid\nsankey-beta\n  a,b,1\n```\n",
        None,
        code.clone(),
    ))
    .unwrap();
    assert_ne!(std::fs::read(drawn).unwrap(), std::fs::read(code).unwrap());

    let options = ExportOptions::from(&ExportSettings::default());
    let pages = layout_pages(DIAGRAM, None, &options, "Doc").unwrap();
    assert_eq!(pages.len(), 1);
}

#[test]
fn html_keeps_the_address_of_every_picture_it_cannot_embed() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("unknown.dat"), b"no picture here").unwrap();
    let mut sniffed = PNG_SIGNATURE.to_vec();
    sniffed.extend_from_slice(b"rest");
    std::fs::write(tmp.path().join("sniffed"), sniffed).unwrap();
    let target = tmp.path().join("out.html");
    let markdown = "![a](missing.png)\n\n![b](https://example.com/b.png)\n\n\
                    ![c](unknown.dat)\n\n![d](sniffed)\n";
    run(&job(
        ExportFormat::Html,
        markdown,
        Some(tmp.path()),
        target.clone(),
    ))
    .unwrap();
    let html = std::fs::read_to_string(target).unwrap();
    for kept in ["missing.png", "https://example.com/b.png", "unknown.dat"] {
        assert!(html.contains(&format!("src=\"{kept}\"")), "{kept}: {html}");
    }
    // The file without an extension is recognised by its first bytes.
    assert_eq!(html.matches("src=\"data:image/png;base64,").count(), 1);
}

#[test]
fn word_documents_embed_local_pictures_and_name_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let png = picture();
    std::fs::write(tmp.path().join("pic.png"), &png).unwrap();
    let markdown = "![pic](pic.png)\n\n![remote](https://example.com/r.png)\n\n![gone](gone.png)\n";
    for format in [ExportFormat::Docx, ExportFormat::Odt] {
        let with_folder = tmp.path().join(format!("with.{format:?}"));
        let without_folder = tmp.path().join(format!("without.{format:?}"));
        run(&job(
            format,
            markdown,
            Some(tmp.path()),
            with_folder.clone(),
        ))
        .unwrap();
        run(&job(format, markdown, None, without_folder.clone())).unwrap();
        let read = |path: &Path| {
            let bytes = std::fs::read(path).unwrap();
            if format == ExportFormat::Docx {
                ssw_convert::docx::import_docx(&bytes, "doc").unwrap()
            } else {
                ssw_convert::odt::import_odt(&bytes, "doc").unwrap()
            }
        };
        let with = read(&with_folder);
        assert_eq!(with.assets.len(), 1, "{format:?}: only the local picture");
        assert_eq!(with.assets[0].bytes, png, "{format:?}");
        assert!(read(&without_folder).assets.is_empty(), "{format:?}");
    }
}

#[test]
fn a_word_document_takes_the_page_size_of_the_options() {
    let tmp = tempfile::tempdir().unwrap();
    let width = |size: PageSize| {
        let target = tmp.path().join(format!("{size:?}.docx"));
        let mut job = job(ExportFormat::Docx, "text\n", None, target.clone());
        job.options.page_size = size;
        run(&job).unwrap();
        let bytes = std::fs::read(&target).unwrap();
        let mut package = Package::open(&bytes).unwrap();
        let xml = package.read_part("word/document.xml").unwrap().unwrap();
        String::from_utf8(xml).unwrap()
    };
    assert!(width(PageSize::Letter).contains("w:w=\"12240\""));
    assert!(width(PageSize::A4).contains("w:w=\"11906\""));
}

#[test]
fn numbered_pages_of_a_target_without_extension_get_only_the_number() {
    let tmp = tempfile::tempdir().unwrap();
    let markdown = "A line of text.\n\n".repeat(200);
    let files = run(&job(
        ExportFormat::Png,
        &markdown,
        None,
        tmp.path().join("long"),
    ))
    .unwrap();
    assert!(files.len() > 1, "{files:?}");
    for (index, file) in files.iter().enumerate() {
        assert_eq!(
            file.file_name().unwrap().to_string_lossy(),
            format!("long-{}", index + 1)
        );
        assert!(std::fs::read(file).unwrap().starts_with(PNG_SIGNATURE));
    }
}

#[test]
fn pages_that_cannot_be_written_name_the_first_one() {
    let tmp = tempfile::tempdir().unwrap();
    let markdown = "A line of text.\n\n".repeat(200);
    let target = tmp.path().join("missing").join("long.png");
    let failure = run(&job(ExportFormat::Png, &markdown, None, target)).unwrap_err();
    assert_eq!(
        written_path(failure),
        tmp.path().join("missing").join("long-1.png")
    );
}

#[test]
fn markdown_with_attachments_names_what_blocks_the_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("a.png"), b"PICTURE").unwrap();
    let markdown = "![a](a.png)\n";
    let export = |out: &Path| {
        run(&job(
            ExportFormat::MarkdownWithAttachments,
            markdown,
            Some(&source),
            out.join("doc.md"),
        ))
    };

    // The attachments folder is a file.
    let blocked = tmp.path().join("blocked");
    std::fs::create_dir_all(&blocked).unwrap();
    std::fs::write(blocked.join("attachments"), b"a file").unwrap();
    assert_eq!(
        written_path(export(&blocked).unwrap_err()),
        blocked.join("attachments")
    );

    // The copy's name is taken by a folder.
    let taken = tmp.path().join("taken");
    std::fs::create_dir_all(taken.join("attachments").join("a.png")).unwrap();
    assert_eq!(
        written_path(export(&taken).unwrap_err()),
        taken.join("attachments").join("a.png")
    );

    // The document's own name is taken by a folder.
    let named = tmp.path().join("named");
    std::fs::create_dir_all(named.join("doc.md")).unwrap();
    let plain = run(&job(
        ExportFormat::MarkdownWithAttachments,
        "no pictures\n",
        Some(&source),
        named.join("doc.md"),
    ));
    assert_eq!(written_path(plain.unwrap_err()), named.join("doc.md"));

    // The picture is locked by another program: the conversion fails, nothing is written.
    let free = tmp.path().join("free");
    std::fs::create_dir_all(&free).unwrap();
    let _lock = locked(&source.join("a.png"));
    let failure = export(&free).unwrap_err();
    assert!(matches!(failure, ExportFailure::Convert(_)), "{failure:?}");
    assert!(!free.join("doc.md").exists());
}

#[test]
fn a_textbundle_names_what_blocks_it() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("a.png"), b"PICTURE").unwrap();
    let markdown = "![a](a.png)\n";
    let bundle = |target: PathBuf| {
        run(&job(
            ExportFormat::Textbundle,
            markdown,
            Some(&source),
            target,
        ))
    };

    // A package whose name is taken by a folder.
    let pack = tmp.path().join("doc.textpack");
    std::fs::create_dir_all(&pack).unwrap();
    assert_eq!(written_path(bundle(pack.clone()).unwrap_err()), pack);

    // A bundle folder whose name is taken by a file.
    let folder = tmp.path().join("doc.textbundle");
    std::fs::write(&folder, b"a file").unwrap();
    let failure = bundle(folder.clone()).unwrap_err();
    assert!(matches!(failure, ExportFailure::Convert(_)), "{failure:?}");
    assert_eq!(std::fs::read(&folder).unwrap(), b"a file");

    // A locked picture cannot be packed.
    let _lock = locked(&source.join("a.png"));
    let failure = bundle(tmp.path().join("free.textpack")).unwrap_err();
    assert!(matches!(failure, ExportFailure::Convert(_)), "{failure:?}");
}

const PLAIN_TEXT: &str = "# not a heading\n**not bold**\n| a | b |";

/// The job of a plain-text document: its text is never read as Markdown.
fn plain_job(format: ExportFormat, target: PathBuf) -> ExportJob {
    let mut job = job(format, PLAIN_TEXT, None, target);
    job.plain = true;
    job
}

#[test]
fn a_plain_text_document_exports_to_html_without_being_read_as_markdown() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("plain.html");
    run(&plain_job(ExportFormat::Html, target.clone())).unwrap();
    let html = std::fs::read_to_string(&target).unwrap();
    assert!(html.contains("# not a heading"), "{html}");
    assert!(html.contains("**not bold**"), "{html}");
    assert!(!html.contains("<h1"), "{html}");
    assert!(!html.contains("<strong"), "{html}");
}

#[test]
fn a_plain_text_document_exports_to_txt_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("plain.txt");
    run(&plain_job(ExportFormat::Txt, target.clone())).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), PLAIN_TEXT.as_bytes());
}

#[test]
fn a_diagram_that_cannot_be_written_names_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("missing").join("diagram.svg");
    let job = DiagramJob {
        source: "graph TD\n  A --> B\n".to_owned(),
        format: DiagramFormat::Svg,
        target: target.clone(),
    };
    let failure = run_diagram(&job, &RealFs, &ThreadSleeper).unwrap_err();
    assert_eq!(written_path(failure), target);
}
