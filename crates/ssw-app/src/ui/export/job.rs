//! The export pipeline (spec §9): Markdown in, finished files out. Everything here runs on a
//! worker thread and touches no UI state; the controller (`super`) hands it a snapshot of the
//! document ([`ExportJob`]) and reports what comes back.
//!
//! PDF, PNG and JPG go through Typst (`ssw_render::export`), the other formats through
//! `ssw_convert`. Every file is written with [`save_atomic`], so a crash or a full disk never
//! leaves a half-written export behind (a TextBundle *folder* is the one exception: a folder
//! cannot be replaced in one step).

use std::path::{Path, PathBuf};
use std::time::Duration;

use ssw_convert::assets::{AssetProvider, image_mime_for_path, is_external_url, sniff_image_mime};
use ssw_convert::css::Theme as CssTheme;
use ssw_convert::docx::{ExportOptions as WordOptions, MathImage, PageSize as WordPage, Resources};
use ssw_convert::epub::{EpubOptions, export_epub};
use ssw_convert::html::{HtmlOptions, render_document};
use ssw_convert::import::Asset;
use ssw_convert::markdown_export::{self, percent_decode};
use ssw_convert::textbundle;
use ssw_core::settings::{ExportFormat, PageSize};
use ssw_io::fs::FileSystem;
use ssw_io::save::{Sleeper, save_atomic};
use ssw_render::export::{ExportOptions as RenderOptions, Exporter, PageSelection};
use ssw_render::math::{render_math_png, render_math_svg};
use ssw_render::mermaid::{MermaidOutcome, MermaidTheme, render_mermaid_within};
use ssw_render::scene::{Color, Scene};
use ssw_ui::shell::export_dialog::ExportOptions;

use crate::documents::DocError;

/// Resolution of PNG and JPG exports.
const RASTER_DPI: f32 = 150.0;
/// JPEG quality of JPG exports (1–100).
const JPEG_QUALITY: u8 = 90;
/// Resolution of an exported diagram picture.
const DIAGRAM_DPI: f32 = 192.0;
/// How long a diagram may take to draw for an export (more than an editor preview gets).
const MERMAID_LIMIT: Duration = Duration::from_secs(10);
/// Millimeters per inch.
const MM_PER_INCH: f32 = 25.4;
/// Twips per inch.
const TWIPS_PER_INCH: f32 = 1440.0;
/// Font size of formulas in HTML and ePub (the 16 px body text of the style sheet).
const WEB_MATH_PT: f32 = 12.0;
/// Text color of the dark theme (the `#E4E4E4` of the dark style sheet): the ink of formulas
/// on dark pages.
const DARK_INK: Color = Color::rgb(0xE4, 0xE4, 0xE4);

/// A snapshot of one document export.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportJob {
    /// The format to write.
    pub format: ExportFormat,
    /// Page setup and styling.
    pub options: ExportOptions,
    /// The document text.
    pub markdown: String,
    /// The document's folder: relative images and attachments are read from below it.
    pub source_dir: Option<PathBuf>,
    /// The document title (file stem or "Untitled").
    pub title: String,
    /// The file to write; pages of a multi-page PNG/JPG get `-1`, `-2`… before the extension.
    pub target: PathBuf,
}

/// The picture formats of a Mermaid diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagramFormat {
    /// A PNG file.
    Png,
    /// An SVG file.
    Svg,
}

impl DiagramFormat {
    /// The file extension, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            DiagramFormat::Png => "png",
            DiagramFormat::Svg => "svg",
        }
    }
}

/// A snapshot of one diagram export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramJob {
    /// The Mermaid source.
    pub source: String,
    /// PNG or SVG.
    pub format: DiagramFormat,
    /// The file to write.
    pub target: PathBuf,
}

/// Why an export produced no files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportFailure {
    /// The renderer or converter failed (its message).
    Convert(String),
    /// The diagram cannot be drawn.
    Diagram,
    /// A file could not be written.
    Write {
        /// The file.
        path: PathBuf,
        /// The Fluent id (`app.ftl`) of the reason.
        reason_id: &'static str,
    },
}

impl ExportFailure {
    fn write(path: &Path, error: impl Into<DocError>) -> Self {
        Self::Write {
            path: path.to_path_buf(),
            reason_id: error.into().message_id(),
        }
    }

    fn convert(error: impl ToString) -> Self {
        Self::Convert(error.to_string())
    }
}

/// The file extension (without the dot) an export in `format` is saved with.
#[must_use]
pub const fn extension(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Pdf => "pdf",
        ExportFormat::Png => "png",
        ExportFormat::Jpg => "jpg",
        ExportFormat::Html => "html",
        ExportFormat::Epub => "epub",
        ExportFormat::Docx => "docx",
        ExportFormat::Odt => "odt",
        ExportFormat::Rtf => "rtf",
        ExportFormat::Txt => "txt",
        ExportFormat::MarkdownWithAttachments => "md",
        ExportFormat::Textbundle => "textpack",
    }
}

/// The options of the Typst renderer for `options` and the document `title`.
#[must_use]
pub fn render_options(options: &ExportOptions, title: &str) -> RenderOptions {
    RenderOptions {
        page: options.page_size,
        margin_mm: options.margin_mm,
        base_font_pt: options.base_font_size_pt,
        page_numbers: options.page_numbers,
        black_text: options.black_text,
        light_theme: options.light_theme,
        include_title: options.include_title,
        title: Some(title.to_owned()),
    }
}

/// Whether an export with `options` is drawn on dark paper: the dark theme was asked for and
/// black text (which needs light paper) was not. Matches `ssw_render::export::ExportOptions`.
fn dark(options: &ExportOptions) -> bool {
    !options.light_theme && !options.black_text
}

/// The HTML / ePub style sheet for dark or light pages.
fn theme(dark: bool) -> CssTheme {
    if dark {
        CssTheme::Dark
    } else {
        CssTheme::Light
    }
}

/// SVG of a Mermaid diagram for an export (drawn for dark paper when `dark`), `None` when it
/// cannot be drawn.
fn mermaid_svg(source: &str, dark: bool) -> Option<String> {
    let theme = if dark {
        MermaidTheme::Dark
    } else {
        MermaidTheme::Light
    };
    match render_mermaid_within(source, theme, MERMAID_LIMIT) {
        MermaidOutcome::Diagram(_, svg) => Some(svg),
        MermaidOutcome::Unsupported { .. } => None,
    }
}

/// The pages of the document laid out by Typst, one scene per page (white paper included).
///
/// # Errors
/// [`ExportFailure::Convert`] when Typst cannot lay the document out.
pub fn layout_pages(
    markdown: &str,
    source_dir: Option<&Path>,
    options: &ExportOptions,
    title: &str,
) -> Result<Vec<Scene>, ExportFailure> {
    let dark = dark(options);
    Exporter::new(source_dir)
        .layout_pages(
            markdown,
            &render_options(options, title),
            &|source: &str| mermaid_svg(source, dark),
        )
        .map_err(ExportFailure::convert)
}

/// Images, formulas and diagrams for the converters.
struct Assets<'a> {
    fs: &'a dyn FileSystem,
    dir: Option<&'a Path>,
    /// Formulas and diagrams are drawn for dark paper.
    dark: bool,
}

impl Assets<'_> {
    /// The bytes of the local file `destination` names (below the document's folder).
    fn read(&self, destination: &str) -> Option<Vec<u8>> {
        if is_external_url(destination) {
            return None;
        }
        let dir = self.dir?;
        let decoded = percent_decode(destination);
        self.fs.read(&dir.join(Path::new(&decoded))).ok()
    }
}

impl AssetProvider for Assets<'_> {
    fn image(&self, path: &str) -> Option<(Vec<u8>, &'static str)> {
        let bytes = self.read(path)?;
        let mime = image_mime_for_path(path).or_else(|| sniff_image_mime(&bytes))?;
        Some((bytes, mime))
    }

    fn math_svg(&self, latex: &str, display: bool) -> Option<String> {
        let ink = if self.dark { DARK_INK } else { Color::BLACK };
        render_math_svg(latex, display, WEB_MATH_PT, ink).ok()
    }

    fn mermaid_svg(&self, source: &str) -> Option<String> {
        mermaid_svg(source, self.dark)
    }
}

/// The options of the DOCX and ODT writers.
fn word_options(options: &ExportOptions, title: &str) -> WordOptions {
    WordOptions {
        title: Some(title.to_owned()),
        page: match options.page_size {
            PageSize::A4 => WordPage::A4,
            PageSize::Letter => WordPage::Letter,
        },
        margin_twips: (options.margin_mm.clamp(0.0, 100.0) / MM_PER_INCH * TWIPS_PER_INCH).round()
            as u32,
        font_half_points: (options.base_font_size_pt.clamp(6.0, 72.0) * 2.0).round() as u32,
        black_text: options.black_text,
        ..WordOptions::default()
    }
}

fn write(
    fs: &dyn FileSystem,
    sleeper: &dyn Sleeper,
    path: &Path,
    bytes: &[u8],
) -> Result<(), ExportFailure> {
    save_atomic(fs, sleeper, path, bytes)
        .map(|_| ())
        .map_err(|error| ExportFailure::write(path, error))
}

/// `target` for page `index` of `count`: the target itself for a single page, otherwise
/// `name-1.ext`, `name-2.ext`…
fn page_path(target: &Path, index: usize, count: usize) -> PathBuf {
    if count == 1 {
        return target.to_path_buf();
    }
    let stem = target
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let number = index + 1;
    target.with_file_name(match target.extension() {
        Some(extension) => format!("{stem}-{number}.{}", extension.to_string_lossy()),
        None => format!("{stem}-{number}"),
    })
}

/// Runs the export and returns the files written, the first one being the one to open.
///
/// # Errors
/// [`ExportFailure`]: a failed conversion or a file that cannot be written.
pub fn run_export(
    job: &ExportJob,
    fs: &dyn FileSystem,
    sleeper: &dyn Sleeper,
) -> Result<Vec<PathBuf>, ExportFailure> {
    let source_dir = job.source_dir.as_deref();
    let dark = dark(&job.options);
    let assets = Assets {
        fs,
        dir: source_dir,
        dark,
    };
    let diagrams = |source: &str| mermaid_svg(source, dark);
    let markdown = job.markdown.as_str();
    let single = |bytes: Vec<u8>| -> Result<Vec<PathBuf>, ExportFailure> {
        write(fs, sleeper, &job.target, &bytes)?;
        Ok(vec![job.target.clone()])
    };
    match job.format {
        ExportFormat::Pdf | ExportFormat::Png | ExportFormat::Jpg => {
            let exporter = Exporter::new(source_dir);
            let options = render_options(&job.options, &job.title);
            let pages = match job.format {
                ExportFormat::Pdf => {
                    let bytes = exporter
                        .export_pdf(markdown, &options, &diagrams)
                        .map_err(ExportFailure::convert)?;
                    return single(bytes);
                }
                ExportFormat::Png => exporter.export_png(
                    markdown,
                    &options,
                    &diagrams,
                    &PageSelection::All,
                    RASTER_DPI,
                ),
                _ => exporter.export_jpg(
                    markdown,
                    &options,
                    &diagrams,
                    &PageSelection::All,
                    RASTER_DPI,
                    JPEG_QUALITY,
                ),
            }
            .map_err(ExportFailure::convert)?;
            // Typst lays out at least one page for any document (an empty one gets a blank
            // page: `collect` flushes the staged empty page), and `PageSelection::All` keeps
            // them all, so `pages` is never empty.
            let mut files = Vec::with_capacity(pages.len());
            for (index, bytes) in pages.iter().enumerate() {
                let path = page_path(&job.target, index, pages.len());
                write(fs, sleeper, &path, bytes)?;
                files.push(path);
            }
            Ok(files)
        }
        ExportFormat::Html => {
            let options = HtmlOptions {
                title: Some(job.title.clone()),
                lang: None,
                theme: theme(dark),
            };
            single(render_document(markdown, &assets, &options).into_bytes())
        }
        ExportFormat::Epub => {
            let options = EpubOptions {
                title: Some(job.title.clone()),
                theme: theme(dark),
                ..EpubOptions::default()
            };
            export_epub(markdown, &job.title, &assets, &options)
                .map_err(ExportFailure::convert)
                .and_then(single)
        }
        ExportFormat::Docx | ExportFormat::Odt => {
            let options = word_options(&job.options, &job.title);
            let size = job.options.base_font_size_pt;
            let mut image = |destination: &str| assets.read(destination);
            let mut math = |tex: &str, display: bool| -> Option<MathImage> {
                render_math_png(tex, display, size)
                    .ok()
                    .map(|picture| MathImage {
                        png: picture.png,
                        width_px: picture.width_px,
                        height_px: picture.height_px,
                    })
            };
            let mut resources = Resources {
                image: &mut image,
                math: &mut math,
            };
            let exported = if job.format == ExportFormat::Docx {
                ssw_convert::docx::export_docx(markdown, &options, &mut resources)
                    .map_err(ExportFailure::convert)
            } else {
                ssw_convert::odt::export_odt(markdown, &options, &mut resources)
                    .map_err(ExportFailure::convert)
            };
            exported.and_then(single)
        }
        ExportFormat::Rtf => {
            single(ssw_convert::rtf::markdown_to_rtf(markdown, &assets).into_bytes())
        }
        ExportFormat::Txt => single(ssw_convert::txt::markdown_to_text(markdown).into_bytes()),
        ExportFormat::MarkdownWithAttachments => export_markdown(job, fs, sleeper),
        ExportFormat::Textbundle => export_textbundle(job, fs, sleeper),
    }
}

/// The document with its local files copied to `attachments/` beside the target.
fn export_markdown(
    job: &ExportJob,
    fs: &dyn FileSystem,
    sleeper: &dyn Sleeper,
) -> Result<Vec<PathBuf>, ExportFailure> {
    let dir = job.target.parent().unwrap_or(Path::new(""));
    let source_dir = job.source_dir.as_deref().unwrap_or(dir);
    let plan = markdown_export::plan(&job.markdown, source_dir);
    for (from, relative) in &plan.copies {
        let to = dir.join(relative);
        // Every copy is `attachments/<file name>` below `dir`, so `to` always has a parent.
        let parent = to.parent().unwrap_or(dir);
        fs.create_dir_all(parent)
            .map_err(|error| ExportFailure::write(parent, error))?;
        let bytes = fs.read(from).map_err(ExportFailure::convert)?;
        write(fs, sleeper, &to, &bytes)?;
    }
    write(fs, sleeper, &job.target, plan.markdown.as_bytes())?;
    Ok(vec![job.target.clone()])
}

/// A TextBundle: a `.textpack` archive, or a `.textbundle` folder when the target has that
/// extension. The files are kept in `assets/`, as the format requires.
fn export_textbundle(
    job: &ExportJob,
    fs: &dyn FileSystem,
    sleeper: &dyn Sleeper,
) -> Result<Vec<PathBuf>, ExportFailure> {
    let dir = job.target.parent().unwrap_or(Path::new(""));
    let source_dir = job.source_dir.as_deref().unwrap_or(dir);
    let plan = markdown_export::plan_with_folder(&job.markdown, source_dir, "assets");
    let mut assets = Vec::with_capacity(plan.copies.len());
    for (from, relative) in &plan.copies {
        let bytes = fs.read(from).map_err(ExportFailure::convert)?;
        assets.push(Asset {
            relative_path: relative.clone(),
            bytes,
        });
    }
    let folder = job
        .target
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("textbundle"));
    let written = if folder {
        textbundle::write_dir(&job.target, &plan.markdown, &assets).map_err(ExportFailure::convert)
    } else {
        textbundle::write_pack(&plan.markdown, &assets)
            .map_err(ExportFailure::convert)
            .and_then(|bytes| write(fs, sleeper, &job.target, &bytes))
    };
    written.map(|()| vec![job.target.clone()])
}

/// Draws the diagram and writes it as a picture.
///
/// # Errors
/// [`ExportFailure::Diagram`] when it cannot be drawn, otherwise as [`run_export`].
pub fn run_diagram(
    job: &DiagramJob,
    fs: &dyn FileSystem,
    sleeper: &dyn Sleeper,
) -> Result<Vec<PathBuf>, ExportFailure> {
    // A diagram picture goes on white paper (PNG) or into any document (SVG): light colors.
    let svg = mermaid_svg(&job.source, false).ok_or(ExportFailure::Diagram)?;
    let bytes = match job.format {
        DiagramFormat::Svg => Ok(svg.into_bytes()),
        DiagramFormat::Png => Exporter::new(None)
            .export_diagram_png(&svg, DIAGRAM_DPI)
            .map_err(ExportFailure::convert),
    };
    bytes
        .and_then(|bytes| write(fs, sleeper, &job.target, &bytes))
        .map(|()| vec![job.target.clone()])
}
