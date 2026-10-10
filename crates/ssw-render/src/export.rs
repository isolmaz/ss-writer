//! Paged export: Markdown → Typst → PDF, PNG, JPEG, or one [`Scene`] per page for printing.
//!
//! An [`Exporter`] owns a Typst world rooted at the directory of the open document (the only
//! place images are read from) and is meant to be kept and reused: Typst's `comemo` cache makes
//! re-exporting a slightly changed document cheap.

use std::fmt;
use std::ops::{Range, RangeInclusive};
use std::path::Path;
use std::sync::Mutex;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use ssw_core::markdown::parse_document;
use ssw_core::settings::{ExportSettings, PageSize};
use thiserror::Error;
use typst::diag::SourceDiagnostic;
use typst::utils::Scalar;
use typst_layout::{Page, PagedDocument};
use typst_pdf::PdfOptions;
use typst_render::RenderOptions;

use crate::fonts::FontError;
use crate::frame_to_scene::{convert_paint, frame_to_scene};
use crate::scene::{Color, Fill, FillRule, Paint, Rect, Scene, SceneItem};
use crate::typst_markup::{DiagramSvg, TypstDocument, markdown_to_typst};
use crate::world::{CompileError, InvalidPath, TypstWorld, lock};

/// Lowest and highest resolution accepted for raster export.
const DPI_RANGE: RangeInclusive<f32> = 1.0..=600.0;

/// Points per inch.
const POINTS_PER_INCH: f64 = 72.0;

/// The virtual file a diagram picture export reads its SVG from.
const DIAGRAM_PICTURE: &str = "ssw-diagram-0.svg";

/// White space around an exported diagram picture, in points.
const DIAGRAM_MARGIN_PT: f32 = 8.0;

/// Page setup and styling of an export.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportOptions {
    pub page: PageSize,
    /// Margin on every side in millimetres (clamped to 0–100; not finite means 20).
    pub margin_mm: f32,
    /// Body text size in points (clamped to 6–72; not finite means 11).
    pub base_font_pt: f32,
    /// Print the page number at the bottom of every page.
    pub page_numbers: bool,
    /// Black text and no accent colors (links are underlined instead). Black ink needs light
    /// paper, so it wins over `light_theme: false`.
    pub black_text: bool,
    /// Light theme: dark ink on white paper. Without it the pages are filled with the dark
    /// surface color of the editor's dark theme and the text, code, quote, table and callout
    /// colors follow it (ignored with `black_text`).
    pub light_theme: bool,
    /// Print [`ExportOptions::title`] at the top of the first page.
    pub include_title: bool,
    /// Document title: the PDF title and, with `include_title`, a heading.
    pub title: Option<String>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self::from_settings(&ExportSettings::default(), None)
    }
}

impl ExportOptions {
    /// The options a user configured in the settings, for a document called `title`.
    pub fn from_settings(settings: &ExportSettings, title: Option<String>) -> Self {
        Self {
            page: settings.page_size,
            margin_mm: settings.margin_mm,
            base_font_pt: settings.base_font_size_pt,
            page_numbers: settings.page_numbers,
            black_text: settings.black_text,
            light_theme: settings.light_theme,
            include_title: settings.include_title,
            title,
        }
    }

    /// Whether the pages get the dark palette: the dark theme was asked for and black text
    /// (which needs light paper) was not.
    pub(crate) fn dark(&self) -> bool {
        !self.light_theme && !self.black_text
    }

    /// [`ExportOptions::margin_mm`] made usable.
    pub(crate) fn margin_mm(&self) -> f32 {
        usable(self.margin_mm, 0.0, 100.0, 20.0)
    }

    /// [`ExportOptions::base_font_pt`] made usable.
    pub(crate) fn base_font_pt(&self) -> f32 {
        usable(self.base_font_pt, 6.0, 72.0, 11.0)
    }
}

/// `value` clamped to `min..=max`, or `fallback` when it is not a finite number.
fn usable(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}

/// Which pages of a document to rasterize.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PageSelection {
    /// Every page.
    #[default]
    All,
    /// Pages `first..=last`, counted from 0.
    Range(RangeInclusive<usize>),
}

impl PageSelection {
    /// The page indices of a document with `pages` pages.
    fn indices(&self, pages: usize) -> Result<Range<usize>, ExportError> {
        match self {
            PageSelection::All => Ok(0..pages),
            PageSelection::Range(range) => {
                let (first, last) = (*range.start(), *range.end());
                if first <= last && last < pages {
                    Ok(first..last + 1)
                } else {
                    Err(ExportError::PageRange { first, last, pages })
                }
            }
        }
    }
}

/// Why an export failed.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ExportError {
    /// Typst could not lay the document out.
    #[error("layout failed: {0}")]
    Layout(#[from] CompileError),
    /// A diagram file name was rejected by the Typst world.
    #[error(transparent)]
    Path(#[from] InvalidPath),
    /// Typst could not write the PDF.
    #[error("PDF export failed: {0}")]
    Pdf(String),
    /// The requested pages do not exist.
    #[error("pages {first}..={last} do not exist in a document of {pages} pages")]
    PageRange {
        first: usize,
        last: usize,
        pages: usize,
    },
    /// The resolution is outside 1–600 dpi (or not a number).
    #[error("resolution {0} dpi is outside 1-600")]
    Resolution(f32),
}

/// How [`Exporter::rasterize`] encodes a page.
#[derive(Clone, Copy)]
pub(crate) enum RasterFormat {
    Png,
    Jpeg { quality: u8 },
}

/// Renders Markdown documents to paged output.
pub struct Exporter {
    world: TypstWorld,
    /// Serializes whole exports: the diagram files live in the shared world.
    gate: Mutex<()>,
}

impl fmt::Debug for Exporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Exporter")
            .field("world", &self.world)
            .finish_non_exhaustive()
    }
}

impl Exporter {
    /// An exporter that reads images from below `root` (the document's directory) only; with
    /// `None` no image is read from disk.
    pub fn new(root: Option<&Path>) -> Self {
        Self {
            world: TypstWorld::new(root),
            gate: Mutex::new(()),
        }
    }

    /// Makes the faces of a font file available to documents (parsed when first used).
    pub fn add_font_file(&mut self, path: &Path) -> Result<usize, FontError> {
        self.world.add_font_file(path)
    }

    /// The Typst source and virtual files an export of `markdown` compiles.
    pub fn to_typst(
        &self,
        markdown: &str,
        options: &ExportOptions,
        diagrams: &dyn DiagramSvg,
    ) -> TypstDocument {
        let parsed = parse_document(markdown);
        markdown_to_typst(markdown, &parsed, options, diagrams, &|dest| {
            self.world.file_available(dest)
        })
    }

    fn compile(
        &self,
        markdown: &str,
        options: &ExportOptions,
        diagrams: &dyn DiagramSvg,
    ) -> Result<PagedDocument, ExportError> {
        let _exclusive = lock(&self.gate);
        let document = self.to_typst(markdown, options, diagrams);
        self.compile_document(document)
    }

    /// Registers the files of `document` in the world and compiles its source; the caller holds
    /// the gate.
    fn compile_document(&self, document: TypstDocument) -> Result<PagedDocument, ExportError> {
        for (path, bytes) in document.files {
            self.world.set_virtual_file(&path, bytes)?;
        }
        Ok(self.world.compile(&document.source)?)
    }

    /// One [`Scene`] per page (in points, white paper included), for printing and previews.
    pub fn layout_pages(
        &self,
        markdown: &str,
        options: &ExportOptions,
        diagrams: &dyn DiagramSvg,
    ) -> Result<Vec<Scene>, ExportError> {
        let document = self.compile(markdown, options, diagrams)?;
        Ok(document.pages().iter().map(page_scene).collect())
    }

    /// The document as a PDF file.
    pub fn export_pdf(
        &self,
        markdown: &str,
        options: &ExportOptions,
        diagrams: &dyn DiagramSvg,
    ) -> Result<Vec<u8>, ExportError> {
        let document = self.compile(markdown, options, diagrams)?;
        typst_pdf::pdf(&document, &PdfOptions::default()).map_err(pdf_error)
    }

    /// The selected pages as PNG files (one entry per page) at `dpi` pixels per inch.
    pub fn export_png(
        &self,
        markdown: &str,
        options: &ExportOptions,
        diagrams: &dyn DiagramSvg,
        pages: &PageSelection,
        dpi: f32,
    ) -> Result<Vec<Vec<u8>>, ExportError> {
        self.rasterize(markdown, options, diagrams, pages, dpi, RasterFormat::Png)
    }

    /// The selected pages as JPEG files (one entry per page) at `dpi` pixels per inch;
    /// `quality` is 1–100.
    pub fn export_jpg(
        &self,
        markdown: &str,
        options: &ExportOptions,
        diagrams: &dyn DiagramSvg,
        pages: &PageSelection,
        dpi: f32,
        quality: u8,
    ) -> Result<Vec<Vec<u8>>, ExportError> {
        let format = RasterFormat::Jpeg {
            quality: quality.clamp(1, 100),
        };
        self.rasterize(markdown, options, diagrams, pages, dpi, format)
    }

    /// A picture in SVG markup (a Mermaid diagram) as a PNG file: the page is as large as the
    /// picture plus a small margin, on white paper, at `dpi` pixels per inch (an SVG user unit
    /// is one point).
    pub fn export_diagram_png(&self, svg: &str, dpi: f32) -> Result<Vec<u8>, ExportError> {
        check_resolution(dpi)?;
        let _exclusive = lock(&self.gate);
        let document = self.compile_document(TypstDocument {
            source: format!(
                "#set page(width: auto, height: auto, margin: {DIAGRAM_MARGIN_PT}pt, fill: white)\n\
                 #image(\"{DIAGRAM_PICTURE}\")\n"
            ),
            files: vec![(DIAGRAM_PICTURE.to_owned(), svg.as_bytes().to_vec())],
        })?;
        let render = RenderOptions {
            pixel_per_pt: Scalar::new(f64::from(dpi) / POINTS_PER_INCH),
            render_bleed: false,
        };
        // A document with one picture always has exactly one page.
        Ok(document
            .pages()
            .first()
            .map(|page| {
                let pixmap = typst_render::render(page, &render);
                encode(
                    RasterFormat::Png,
                    pixmap.width(),
                    pixmap.height(),
                    pixmap.data(),
                )
            })
            .unwrap_or_default())
    }

    fn rasterize(
        &self,
        markdown: &str,
        options: &ExportOptions,
        diagrams: &dyn DiagramSvg,
        pages: &PageSelection,
        dpi: f32,
        format: RasterFormat,
    ) -> Result<Vec<Vec<u8>>, ExportError> {
        check_resolution(dpi)?;
        let document = self.compile(markdown, options, diagrams)?;
        let selected = pages.indices(document.pages().len())?;
        let render = RenderOptions {
            pixel_per_pt: Scalar::new(f64::from(dpi) / POINTS_PER_INCH),
            render_bleed: false,
        };
        Ok(document.pages()[selected]
            .iter()
            .map(|page| {
                let pixmap = typst_render::render(page, &render);
                encode(format, pixmap.width(), pixmap.height(), pixmap.data())
            })
            .collect())
    }
}

/// The PDF writer's diagnostics as one error, messages joined with ` | `.
fn pdf_error(errors: impl IntoIterator<Item = SourceDiagnostic>) -> ExportError {
    let messages: Vec<String> = errors
        .into_iter()
        .map(|error| error.message.to_string())
        .collect();
    ExportError::Pdf(messages.join(" | "))
}

fn check_resolution(dpi: f32) -> Result<(), ExportError> {
    if DPI_RANGE.contains(&dpi) {
        Ok(())
    } else {
        Err(ExportError::Resolution(dpi))
    }
}

/// The page as a scene: paper under the page content. The paper is what the page is filled
/// with (the dark theme's surface; Typst keeps it beside the frame, not in it), white
/// otherwise.
fn page_scene(page: &Page) -> Scene {
    let mut scene = frame_to_scene(&page.frame);
    let bounds = Rect::new(0.0, 0.0, scene.size.width, scene.size.height);
    let paint = page
        .fill_or_white()
        .map_or(Paint::Solid(Color::WHITE), |fill| {
            convert_paint(&fill, bounds)
        });
    let paper = Fill {
        path: bounds.to_path(),
        rule: FillRule::NonZero,
        paint,
    };
    scene.items.insert(0, SceneItem::Fill(paper));
    scene
}

/// Encodes premultiplied RGBA8 pixels.
pub(crate) fn encode(
    format: RasterFormat,
    width: u32,
    height: u32,
    premultiplied: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    // The buffer length matches the size and the size is far below the codec limits (pages are
    // at most a few thousand pixels wide), so encoding into memory cannot fail.
    match format {
        RasterFormat::Png => {
            let mut pixels = Vec::with_capacity(premultiplied.len());
            for px in premultiplied.as_chunks::<4>().0 {
                pixels.extend_from_slice(&straight(*px));
            }
            PngEncoder::new(&mut out)
                .write_image(&pixels, width, height, ExtendedColorType::Rgba8)
                .expect("PNG encoding of a matching in-memory buffer");
        }
        RasterFormat::Jpeg { quality } => {
            let mut pixels = Vec::with_capacity(premultiplied.len() / 4 * 3);
            for px in premultiplied.as_chunks::<4>().0 {
                // Premultiplied color over white paper.
                let uncovered = 255 - px[3];
                pixels.extend(px[..3].iter().map(|c| c.saturating_add(uncovered)));
            }
            JpegEncoder::new_with_quality(&mut out, quality)
                .write_image(&pixels, width, height, ExtendedColorType::Rgb8)
                .expect("JPEG encoding of a matching in-memory buffer");
        }
    }
    out
}

/// A premultiplied RGBA pixel as straight (non-premultiplied) RGBA.
fn straight(px: [u8; 4]) -> [u8; 4] {
    let alpha = u32::from(px[3]);
    if alpha == 0 {
        return [0; 4];
    }
    let channel = |c: u8| {
        let value = (u32::from(c) * 255 + alpha / 2) / alpha;
        // Premultiplied channels never exceed alpha, so the value is at most 255.
        value.min(255) as u8
    };
    [channel(px[0]), channel(px[1]), channel(px[2]), px[3]]
}

#[cfg(test)]
mod tests {
    use typst::syntax::Span;

    use super::*;

    #[test]
    fn defaults_follow_the_export_settings() {
        let options = ExportOptions::default();
        assert_eq!(options.page, PageSize::A4);
        assert_eq!(options.margin_mm, 20.0);
        assert_eq!(options.base_font_pt, 11.0);
        assert!(options.page_numbers && options.include_title && !options.black_text);
        assert!(options.light_theme);
        assert_eq!(options.title, None);

        let settings = ExportSettings {
            page_size: PageSize::Letter,
            margin_mm: 15.0,
            base_font_size_pt: 12.0,
            page_numbers: false,
            black_text: true,
            light_theme: false,
            include_title: false,
            ..ExportSettings::default()
        };
        let options = ExportOptions::from_settings(&settings, Some("Doc".to_owned()));
        assert_eq!(
            options,
            ExportOptions {
                page: PageSize::Letter,
                margin_mm: 15.0,
                base_font_pt: 12.0,
                page_numbers: false,
                black_text: true,
                light_theme: false,
                include_title: false,
                title: Some("Doc".to_owned()),
            }
        );
    }

    #[test]
    fn the_dark_palette_needs_the_dark_theme_and_no_black_text() {
        let dark = |light_theme, black_text| {
            ExportOptions {
                light_theme,
                black_text,
                ..ExportOptions::default()
            }
            .dark()
        };
        assert!(dark(false, false));
        assert!(!dark(true, false));
        assert!(!dark(false, true), "black ink needs light paper");
        assert!(!dark(true, true));
    }

    #[test]
    fn unusable_numbers_are_replaced() {
        let table = [
            (f32::NAN, 20.0, 11.0),
            (f32::INFINITY, 20.0, 11.0),
            (-5.0, 0.0, 6.0),
            (1000.0, 100.0, 72.0),
            (13.0, 13.0, 13.0),
        ];
        for (value, margin, font) in table {
            let options = ExportOptions {
                margin_mm: value,
                base_font_pt: value,
                ..ExportOptions::default()
            };
            assert_eq!(options.margin_mm(), margin, "margin {value}");
            assert_eq!(options.base_font_pt(), font, "font {value}");
        }
    }

    #[test]
    fn page_selection_validates_ranges() {
        assert_eq!(PageSelection::default().indices(3), Ok(0..3));
        assert_eq!(PageSelection::All.indices(0), Ok(0..0));
        assert_eq!(PageSelection::Range(1..=1).indices(3), Ok(1..2));
        assert_eq!(PageSelection::Range(0..=2).indices(3), Ok(0..3));
        let outside = PageSelection::Range(2..=3).indices(3).expect_err("outside");
        assert_eq!(
            outside,
            ExportError::PageRange {
                first: 2,
                last: 3,
                pages: 3
            }
        );
        assert!(outside.to_string().contains("2..=3"));
        // A reversed range (built at run time) selects nothing.
        assert!(
            PageSelection::Range(RangeInclusive::new(2, 1))
                .indices(3)
                .is_err()
        );
        assert!(PageSelection::Range(0..=0).indices(0).is_err());
    }

    #[test]
    fn resolution_bounds_are_inclusive() {
        for dpi in [1.0, 72.0, 300.0, 600.0] {
            assert_eq!(check_resolution(dpi), Ok(()), "{dpi}");
        }
        for dpi in [0.99, 0.0, -1.0, 600.01, f32::NAN, f32::INFINITY] {
            // NaN never equals itself: compare the rendered message.
            assert_eq!(
                check_resolution(dpi)
                    .expect_err("outside the range")
                    .to_string(),
                format!("resolution {dpi} dpi is outside 1-600"),
                "{dpi}"
            );
        }
    }

    #[test]
    fn pixel_conversion() {
        assert_eq!(straight([0, 0, 0, 0]), [0, 0, 0, 0]);
        assert_eq!(straight([10, 20, 30, 255]), [10, 20, 30, 255]);
        // 50 % black and 50 % white (premultiplied 128/128).
        assert_eq!(straight([0, 0, 0, 128]), [0, 0, 0, 128]);
        assert_eq!(straight([128, 128, 128, 128]), [255, 255, 255, 128]);
        // Out-of-range premultiplied data is clamped.
        assert_eq!(straight([200, 0, 0, 100]), [255, 0, 0, 100]);
    }

    #[test]
    fn encoders_produce_decodable_images() {
        // 2x1: opaque red, transparent.
        let pixels = [255, 0, 0, 255, 0, 0, 0, 0];
        let png = encode(RasterFormat::Png, 2, 1, &pixels);
        let decoded = image::load_from_memory(&png).expect("png").to_rgba8();
        assert_eq!(decoded.dimensions(), (2, 1));
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(decoded.get_pixel(1, 0).0[3], 0);

        let red = [255, 0, 0, 255].repeat(64);
        let jpeg = encode(RasterFormat::Jpeg { quality: 100 }, 8, 8, &red);
        assert_eq!(&jpeg[..3], &[0xff, 0xd8, 0xff]);
        let decoded = image::load_from_memory(&jpeg).expect("jpeg").to_rgb8();
        assert_eq!(decoded.dimensions(), (8, 8));
        let [r, g, b] = decoded.get_pixel(3, 3).0;
        assert!(r > 230 && g < 40 && b < 40, "{r} {g} {b}");

        // Transparent pixels turn into white paper.
        let clear = [0, 0, 0, 0].repeat(64);
        let jpeg = encode(RasterFormat::Jpeg { quality: 100 }, 8, 8, &clear);
        let decoded = image::load_from_memory(&jpeg).expect("jpeg").to_rgb8();
        assert!(decoded.get_pixel(3, 3).0.iter().all(|c| *c > 240));
    }

    #[test]
    fn export_errors_describe_the_problem() {
        let err = ExportError::Resolution(0.0);
        assert!(err.to_string().contains("0 dpi"));
        let layout = ExportError::from(CompileError {
            message: "boom".to_owned(),
            hints: Vec::new(),
        });
        assert_eq!(layout.to_string(), "layout failed: boom");
        assert!(
            ExportError::from(InvalidPath("../x".to_owned()))
                .to_string()
                .contains("../x")
        );
        assert!(
            ExportError::Pdf("bad".to_owned())
                .to_string()
                .contains("bad")
        );
    }

    #[test]
    fn a_diagram_becomes_a_png_on_white_paper() {
        let exporter = Exporter::new(None);
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"40\" height=\"20\" \
                   viewBox=\"0 0 40 20\"><rect width=\"40\" height=\"20\" fill=\"#ff0000\"/></svg>";
        let png = exporter.export_diagram_png(svg, 72.0).expect("renders");
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        let decoded = image::load_from_memory(&png).expect("png").to_rgba8();
        let (width, height) = decoded.dimensions();
        // The picture is wider than tall and has a margin of white paper around it.
        assert!(width > height && width > 40, "{width}x{height}");
        assert_eq!(decoded.get_pixel(1, 1).0, [255, 255, 255, 255]);
        assert_eq!(decoded.get_pixel(width / 2, height / 2).0, [255, 0, 0, 255]);
        // Twice the resolution, twice the pixels.
        let double = exporter.export_diagram_png(svg, 144.0).expect("renders");
        let decoded = image::load_from_memory(&double).expect("png").to_rgba8();
        let (double_width, double_height) = decoded.dimensions();
        assert!(
            double_width.abs_diff(2 * width) <= 1,
            "{double_width} vs {width}"
        );
        assert!(
            double_height.abs_diff(2 * height) <= 1,
            "{double_height} vs {height}"
        );
        assert_eq!(
            exporter.export_diagram_png(svg, 0.0),
            Err(ExportError::Resolution(0.0))
        );
        let invalid = exporter
            .export_diagram_png("not an svg", 72.0)
            .expect_err("not a picture");
        assert!(
            invalid.to_string().starts_with("layout failed: "),
            "{invalid}"
        );
    }

    #[test]
    fn pdf_errors_join_the_writer_messages() {
        let diagnostics = ["first problem", "second problem"]
            .map(|message| SourceDiagnostic::error(Span::detached(), message));
        assert_eq!(
            pdf_error(diagnostics),
            ExportError::Pdf("first problem | second problem".to_owned())
        );
        assert_eq!(pdf_error(Vec::new()), ExportError::Pdf(String::new()));
        assert_eq!(
            ExportError::Pdf("one | two".to_owned()).to_string(),
            "PDF export failed: one | two"
        );
    }

    #[test]
    fn a_document_file_outside_the_root_is_rejected_before_compiling() {
        let exporter = Exporter::new(None);
        let document = TypstDocument {
            source: "text".to_owned(),
            files: vec![
                ("ssw-diagram-0.svg".to_owned(), Vec::new()),
                ("../escape.svg".to_owned(), Vec::new()),
            ],
        };
        let err = exporter
            .compile_document(document)
            .expect_err("the second file escapes the root");
        assert_eq!(
            err,
            ExportError::Path(InvalidPath("../escape.svg".to_owned()))
        );
        assert_eq!(
            err.to_string(),
            "invalid virtual file path \"../escape.svg\""
        );
    }

    #[test]
    fn exporter_debug_names_the_type() {
        let exporter = Exporter::new(None);
        assert!(format!("{exporter:?}").contains("Exporter"));
    }
}
