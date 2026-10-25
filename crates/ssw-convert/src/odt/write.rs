//! Markdown → ODT.
//!
//! The package is assembled from hand-built OpenDocument XML: `content.xml` with the automatic
//! styles the document needs, `styles.xml` with the named styles (`Heading_20_1`,
//! `Quotations`, `Preformatted_20_Text`, `Source_20_Text`, …), `meta.xml`, the manifest and
//! the pictures. The `mimetype` entry comes first and is stored uncompressed.

use std::collections::HashMap;
use std::io::{Cursor, Seek, Write};

use ssw_core::markdown::ast::{
    Alignment, Block, BlockKind, CalloutKind, Inline, InlineKind, Table,
};
use ssw_core::markdown::definitions::normalize_label;
use ssw_core::markdown::{parse_document, unescape_markdown};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::css::{ACCENT, highlight_hex};
use crate::docx::{ExportOptions, Resources};

use super::slug::unique_slug;

const XML_DECL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;
const MIME: &str = "application/vnd.oasis.opendocument.text";
const NS: &str = concat!(
    r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" "#,
    r#"xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" "#,
    r#"xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" "#,
    r#"xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" "#,
    r#"xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" "#,
    r#"xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" "#,
    r#"xmlns:xlink="http://www.w3.org/1999/xlink" "#,
    r#"xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
    r#"xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" "#,
    r#"xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0""#
);
const FONT_FACES: &str = concat!(
    r#"<office:font-face-decls>"#,
    r#"<style:font-face style:name="Calibri" svg:font-family="Calibri" style:font-family-generic="swiss" style:font-pitch="variable"/>"#,
    r#"<style:font-face style:name="Cascadia Mono" svg:font-family="'Cascadia Mono'" style:font-family-generic="modern" style:font-pitch="fixed"/>"#,
    r#"</office:font-face-decls>"#
);
/// Heading sizes in points, as in the DOCX export.
const HEADING_POINTS: [u32; 6] = [20, 17, 15, 13, 12, 11];
/// Bullet characters by list level.
const BULLETS: [&str; 3] = ["\u{2022}", "\u{25E6}", "\u{25AA}"];
const LIST_LEVELS: usize = 10;

/// Why an ODT could not be written.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ExportError {
    #[error("could not write the ODT package: {0}")]
    Package(String),
}

impl From<zip::result::ZipError> for ExportError {
    fn from(error: zip::result::ZipError) -> ExportError {
        ExportError::Package(error.to_string())
    }
}

impl From<std::io::Error> for ExportError {
    fn from(error: std::io::Error) -> ExportError {
        ExportError::Package(error.to_string())
    }
}

/// Escapes `s` for XML text and drops characters XML 1.0 cannot carry.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if u32::from(c) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// An attribute value: like [`esc`], with line breaks flattened.
fn attr(s: &str) -> String {
    esc(&s.replace(['\n', '\r', '\t'], " "))
}

/// A length in centimeters without trailing zeros.
fn cm(value: f64) -> String {
    let text = format!("{value:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn twips_cm(twips: u32) -> f64 {
    f64::from(twips) * 2.54 / 1440.0
}

fn px_cm(px: u32) -> f64 {
    f64::from(px) * 2.54 / 96.0
}

/// Character formatting of a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RunFmt {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    highlight: Option<&'static str>,
    code: bool,
}

/// Paragraph formatting beyond the named style.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ParaProps {
    align: Option<&'static str>,
    /// Extra left margin in centimeters.
    margin: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParaKey {
    parent: String,
    page_break: bool,
    props: ParaProps,
}

#[derive(Debug, Clone, Copy, Default)]
struct Ctx {
    /// Block quote nesting depth.
    quote: u8,
    /// Inside a list item.
    in_list: bool,
}

/// The kind of a run of list items; each kind has its own list style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Bullet,
    Number,
    Open,
    Done,
}

impl Flavor {
    fn index(self) -> usize {
        match self {
            Flavor::Bullet => 0,
            Flavor::Number => 1,
            Flavor::Open => 2,
            Flavor::Done => 3,
        }
    }

    fn style(self) -> &'static str {
        ["L1", "L2", "L3", "L4"][self.index()]
    }
}

/// A raster image format the exporter embeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageInfo {
    ext: &'static str,
    mime: &'static str,
    width: u32,
    height: u32,
}

/// The `N` bytes at `at`; `None` when the data ends before them.
fn bytes_at<const N: usize>(b: &[u8], at: usize) -> Option<[u8; N]> {
    b.get(at..).and_then(|rest| rest.first_chunk()).copied()
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    bytes_at(b, at).map(u16::from_be_bytes)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    bytes_at(b, at).map(u32::from_be_bytes)
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    bytes_at(b, at).map(u16::from_le_bytes)
}

/// Format and pixel size of a PNG, JPEG or GIF.
fn image_info(b: &[u8]) -> Option<ImageInfo> {
    let (ext, mime, width, height) =
        if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
            if b.get(12..16) != Some(&b"IHDR"[..]) {
                return None;
            }
            ("png", "image/png", be32(b, 16)?, be32(b, 20)?)
        } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
            (
                "gif",
                "image/gif",
                u32::from(le16(b, 6)?),
                u32::from(le16(b, 8)?),
            )
        } else if b.starts_with(&[0xFF, 0xD8]) {
            let (w, h) = jpeg_size(b)?;
            ("jpg", "image/jpeg", w, h)
        } else {
            return None;
        };
    (width > 0 && height > 0).then_some(ImageInfo {
        ext,
        mime,
        width,
        height,
    })
}

/// Size from the first start-of-frame segment of a JPEG.
fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    loop {
        while *b.get(i)? != 0xFF {
            i += 1;
        }
        // A run of 0xFF bytes is padding before the marker byte.
        let marker = loop {
            i += 1;
            let byte = *b.get(i)?;
            if byte != 0xFF {
                break byte;
            }
        };
        match marker {
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                // Height comes first in the frame header, then width.
                let height = u32::from(be16(b, i + 4)?);
                let width = u32::from(be16(b, i + 6)?);
                return Some((width, height));
            }
            0x01 | 0xD0..=0xD8 => i += 1,
            _ => i += 1 + usize::from(be16(b, i + 1)?),
        }
    }
}

/// Plain text of an inline sequence (anchors, alt text and fallbacks).
fn inline_text(src: &str, inlines: &[Inline], out: &mut String) {
    for inline in inlines {
        match &inline.kind {
            InlineKind::Text | InlineKind::RawHtml => {
                out.push_str(inline.range.get(src).unwrap_or(""))
            }
            InlineKind::Escape => out.push_str(inline.content.get(src).unwrap_or("")),
            InlineKind::Entity { decoded } => out.push_str(decoded),
            InlineKind::Code { text } => out.push_str(text),
            InlineKind::InlineMath | InlineKind::WikiLink(_) => {
                out.push_str(inline.content.get(src).unwrap_or(""))
            }
            InlineKind::SoftBreak | InlineKind::HardBreak => out.push(' '),
            InlineKind::Image(_) | InlineKind::FootnoteRef { .. } => {}
            _ => inline_text(src, &inline.children, out),
        }
    }
}

fn block_text(src: &str, block: &Block) -> String {
    let mut out = String::new();
    inline_text(src, &block.inlines, &mut out);
    for child in &block.children {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&block_text(src, child));
    }
    out
}

fn callout_label(kind: CalloutKind) -> &'static str {
    match kind {
        CalloutKind::Note => "Note",
        CalloutKind::Tip => "Tip",
        CalloutKind::Important => "Important",
        CalloutKind::Warning => "Warning",
        CalloutKind::Caution => "Caution",
    }
}

struct Writer<'a, 'r> {
    src: &'a str,
    resources: &'a mut Resources<'r>,
    body: String,
    text_styles: Vec<(RunFmt, String)>,
    para_styles: Vec<(ParaKey, String)>,
    /// Table and column styles.
    table_styles: String,
    column_counts: Vec<usize>,
    tables: u32,
    lists_used: [bool; 4],
    /// A page break waits for the next paragraph or table, which carries it.
    pending_break: bool,
    /// The last character written was a space (ODF collapses runs of spaces).
    prev_space: bool,
    in_note: bool,
    media: Vec<(String, &'static str, Vec<u8>)>,
    note_count: u32,
    footnote_defs: HashMap<String, &'a Block>,
    /// Heading anchor (`#slug`) → bookmark name.
    anchors: HashMap<String, String>,
    /// Heading source offset → bookmark name.
    heading_bookmarks: HashMap<usize, String>,
    content_width_px: u32,
    text_width_cm: f64,
}

impl<'a, 'r> Writer<'a, 'r> {
    fn new(src: &'a str, options: &ExportOptions, resources: &'a mut Resources<'r>) -> Self {
        let (width, _) = options.page.twips();
        let content = width.saturating_sub(options.margin_twips.saturating_mul(2));
        Writer {
            src,
            resources,
            body: String::new(),
            text_styles: Vec::new(),
            para_styles: Vec::new(),
            table_styles: String::new(),
            column_counts: Vec::new(),
            tables: 0,
            lists_used: [false; 4],
            pending_break: false,
            prev_space: true,
            in_note: false,
            media: Vec::new(),
            note_count: 0,
            footnote_defs: HashMap::new(),
            anchors: HashMap::new(),
            heading_bookmarks: HashMap::new(),
            content_width_px: (content / 15).max(1),
            text_width_cm: twips_cm(content).max(1.0),
        }
    }

    // ---- styles ------------------------------------------------------------------------------

    fn text_style(&mut self, fmt: RunFmt) -> String {
        if let Some((_, name)) = self.text_styles.iter().find(|(f, _)| *f == fmt) {
            return name.clone();
        }
        let name = format!("T{}", self.text_styles.len() + 1);
        self.text_styles.push((fmt, name.clone()));
        name
    }

    fn para_style(&mut self, key: ParaKey) -> String {
        if let Some((_, name)) = self.para_styles.iter().find(|(k, _)| *k == key) {
            return name.clone();
        }
        let name = format!("P{}", self.para_styles.len() + 1);
        self.para_styles.push((key, name.clone()));
        name
    }

    /// The opening tag of a paragraph or heading; consumes a pending page break.
    fn open(&mut self, tag: &str, parent: &str, props: ParaProps, extra: &str) -> String {
        let page_break = std::mem::take(&mut self.pending_break);
        let style = if page_break || props != ParaProps::default() {
            self.para_style(ParaKey {
                parent: parent.to_string(),
                page_break,
                props,
            })
        } else {
            parent.to_string()
        };
        format!(r#"<{tag} text:style-name="{style}"{extra}>"#)
    }

    // ---- runs --------------------------------------------------------------------------------

    /// Text with ODF white-space handling: a space that follows a space (or starts the
    /// paragraph) becomes `<text:s/>`, tabs and line feeds become elements.
    fn text_xml(&mut self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut extra = 0usize;
        let mut buf = [0u8; 4];
        for c in text.chars() {
            if c == ' ' {
                if self.prev_space {
                    extra += 1;
                } else {
                    out.push(' ');
                    self.prev_space = true;
                }
                continue;
            }
            if extra > 0 {
                out.push_str(&format!(r#"<text:s text:c="{extra}"/>"#));
                extra = 0;
            }
            match c {
                '\t' => out.push_str("<text:tab/>"),
                '\n' | '\r' => out.push_str("<text:line-break/>"),
                c => out.push_str(&esc(c.encode_utf8(&mut buf))),
            }
            self.prev_space = matches!(c, '\t' | '\n' | '\r');
        }
        if extra > 0 {
            out.push_str(&format!(r#"<text:s text:c="{extra}"/>"#));
        }
        out
    }

    fn run(&mut self, fmt: RunFmt, inner: &str) -> String {
        if fmt == RunFmt::default() {
            return inner.to_string();
        }
        let style = self.text_style(fmt);
        format!(r#"<text:span text:style-name="{style}">{inner}</text:span>"#)
    }

    fn text_run(&mut self, fmt: RunFmt, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let inner = self.text_xml(text);
        self.run(fmt, &inner)
    }

    fn line_break(&mut self) -> String {
        self.prev_space = true;
        "<text:line-break/>".to_string()
    }

    /// Display size in pixels: explicit `|W` / `|WxH`, else the intrinsic size, clamped to the
    /// text width with the aspect ratio kept.
    fn image_size(&self, info: &ImageInfo, width: Option<u32>, height: Option<u32>) -> (u32, u32) {
        let (iw, ih) = (u64::from(info.width), u64::from(info.height));
        let scaled = |value: u32, num: u64, den: u64| {
            u32::try_from((u64::from(value.max(1)) * num / den).max(1)).unwrap_or(u32::MAX)
        };
        let (mut w, mut h) = match (width, height) {
            (Some(w), Some(h)) => (w.max(1), h.max(1)),
            (Some(w), None) => (w.max(1), scaled(w, ih, iw)),
            (None, Some(h)) => (scaled(h, iw, ih), h.max(1)),
            (None, None) => (info.width, info.height),
        };
        if w > self.content_width_px {
            h = scaled(h, u64::from(self.content_width_px), u64::from(w));
            w = self.content_width_px;
        }
        (w, h)
    }

    fn frame(&mut self, bytes: Vec<u8>, info: &ImageInfo, size: (u32, u32), alt: &str) -> String {
        let n = self.media.len() + 1;
        let file = format!("image{n}.{}", info.ext);
        self.media.push((file.clone(), info.mime, bytes));
        let desc = if alt.is_empty() {
            String::new()
        } else {
            format!("<svg:desc>{}</svg:desc>", esc(alt))
        };
        self.prev_space = false;
        format!(
            r#"<draw:frame draw:style-name="fr1" draw:name="Image{n}" text:anchor-type="as-char" svg:width="{w}cm" svg:height="{h}cm" draw:z-index="0"><draw:image xlink:href="Pictures/{file}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>{desc}</draw:frame>"#,
            w = cm(px_cm(size.0)),
            h = cm(px_cm(size.1)),
        )
    }

    fn image_run(&mut self, img: &ssw_core::markdown::ast::Image) -> String {
        let alt = unescape_markdown(img.alt.get(self.src).unwrap_or(""));
        let label = if alt.is_empty() {
            img.dest.clone()
        } else {
            alt.clone()
        };
        let Some(bytes) = (self.resources.image)(&img.dest) else {
            return self.text_run(RunFmt::default(), &label);
        };
        let Some(info) = image_info(&bytes) else {
            return self.text_run(RunFmt::default(), &label);
        };
        let size = self.image_size(&info, img.width, img.height);
        self.frame(bytes, &info, size, &alt)
    }

    fn code_run(&mut self, text: &str) -> String {
        self.text_run(
            RunFmt {
                code: true,
                ..RunFmt::default()
            },
            text,
        )
    }

    fn math_run(&mut self, tex: &str, display: bool) -> String {
        let Some(image) = (self.resources.math)(tex, display) else {
            return self.code_run(tex);
        };
        match image_info(&image.png) {
            Some(info) => {
                let size = (image.width_px.max(1), image.height_px.max(1));
                self.frame(image.png, &info, size, tex)
            }
            None => self.code_run(tex),
        }
    }

    fn footnote_ref(&mut self, label: &str) -> String {
        let definition = if self.in_note {
            None
        } else {
            self.footnote_defs.get(&normalize_label(label)).copied()
        };
        let Some(definition) = definition else {
            return self.text_run(RunFmt::default(), &format!("[^{label}]"));
        };
        self.note_count += 1;
        let id = self.note_count;
        self.in_note = true;
        let mut paragraphs = String::new();
        for child in &definition.children {
            self.prev_space = true;
            let inner = match child.kind {
                BlockKind::Paragraph => self.inlines(&child.inlines, RunFmt::default()),
                _ => {
                    let text = block_text(self.src, child);
                    self.text_run(RunFmt::default(), &text)
                }
            };
            paragraphs.push_str(&format!(
                r#"<text:p text:style-name="Footnote">{inner}</text:p>"#
            ));
        }
        if paragraphs.is_empty() {
            paragraphs.push_str(r#"<text:p text:style-name="Footnote"/>"#);
        }
        self.in_note = false;
        self.prev_space = false;
        format!(
            r#"<text:note text:id="ftn{id}" text:note-class="footnote"><text:note-citation>{id}</text:note-citation><text:note-body>{paragraphs}</text:note-body></text:note>"#
        )
    }

    // ---- inlines -----------------------------------------------------------------------------

    fn inlines(&mut self, inlines: &[Inline], fmt: RunFmt) -> String {
        let mut out = String::new();
        for inline in inlines {
            out.push_str(&self.inline(inline, fmt));
        }
        out
    }

    fn inline(&mut self, inline: &Inline, fmt: RunFmt) -> String {
        let src = self.src;
        match &inline.kind {
            InlineKind::Text => self.text_run(fmt, inline.range.get(src).unwrap_or("")),
            InlineKind::Escape => self.text_run(fmt, inline.content.get(src).unwrap_or("")),
            InlineKind::Entity { decoded } => self.text_run(fmt, decoded),
            InlineKind::Emphasis => self.inlines(
                &inline.children,
                RunFmt {
                    italic: true,
                    ..fmt
                },
            ),
            InlineKind::Strong => self.inlines(&inline.children, RunFmt { bold: true, ..fmt }),
            InlineKind::Underline => self.inlines(
                &inline.children,
                RunFmt {
                    underline: true,
                    ..fmt
                },
            ),
            InlineKind::Strikethrough => self.inlines(
                &inline.children,
                RunFmt {
                    strike: true,
                    ..fmt
                },
            ),
            InlineKind::Highlight { color } => self.inlines(
                &inline.children,
                RunFmt {
                    highlight: Some(highlight_hex(*color)),
                    ..fmt
                },
            ),
            InlineKind::Code { text } => self.text_run(RunFmt { code: true, ..fmt }, text),
            InlineKind::Link(link) => {
                let inner = self.inlines(&inline.children, fmt);
                let target = match link.dest.strip_prefix('#') {
                    Some(anchor) => format!(
                        "#{}",
                        self.anchors.get(anchor).map_or(anchor, String::as_str)
                    ),
                    None => link.dest.clone(),
                };
                format!(
                    r#"<text:a xlink:type="simple" xlink:href="{}" text:style-name="Internet_20_link">{inner}</text:a>"#,
                    attr(&target)
                )
            }
            InlineKind::Image(img) => self.image_run(img),
            InlineKind::WikiLink(_) => self.text_run(fmt, inline.content.get(src).unwrap_or("")),
            InlineKind::FootnoteRef { label } => self.footnote_ref(label),
            InlineKind::InlineMath => {
                let tex = inline.content.get(src).unwrap_or("").to_string();
                self.math_run(&tex, false)
            }
            InlineKind::HardBreak => self.line_break(),
            InlineKind::SoftBreak => self.text_run(fmt, " "),
            InlineKind::RawHtml => {
                let raw = inline.range.get(src).unwrap_or("");
                let tag = raw.trim_matches(['<', '>', '/', ' ']).to_ascii_lowercase();
                if tag == "br" {
                    self.line_break()
                } else {
                    self.text_run(fmt, raw)
                }
            }
        }
    }

    // ---- blocks ------------------------------------------------------------------------------

    /// One paragraph (or heading) element around `inner`.
    fn element(&mut self, tag: &str, parent: &str, props: ParaProps, extra: &str, inner: &str) {
        let open = self.open(tag, parent, props, extra);
        self.body.push_str(&format!("{open}{inner}</{tag}>"));
    }

    fn paragraph(&mut self, parent: &str, props: ParaProps, inlines: &[Inline]) {
        self.prev_space = true;
        let inner = self.inlines(inlines, RunFmt::default());
        self.element("text:p", parent, props, "", &inner);
    }

    fn blocks(&mut self, blocks: &'a [Block], ctx: Ctx) {
        for block in blocks {
            self.block(block, ctx);
        }
    }

    fn quote_paragraph(ctx: Ctx) -> (&'static str, ParaProps) {
        (
            "Quotations",
            ParaProps {
                align: None,
                margin: u32::from(ctx.quote.saturating_sub(1)),
            },
        )
    }

    fn block(&mut self, block: &'a Block, ctx: Ctx) {
        match &block.kind {
            BlockKind::Heading { level, .. } => {
                self.prev_space = true;
                let inner = self.inlines(&block.inlines, RunFmt::default());
                // `export_odt` registers a bookmark for every heading before any block is written
                let name = attr(&self.heading_bookmarks[&block.range.start]);
                let inner = format!(
                    r#"<text:bookmark-start text:name="{name}"/>{inner}<text:bookmark-end text:name="{name}"/>"#
                );
                let level = (*level).clamp(1, 6);
                let extra = format!(r#" text:outline-level="{level}""#);
                self.element(
                    "text:h",
                    &format!("Heading_20_{level}"),
                    ParaProps::default(),
                    &extra,
                    &inner,
                );
            }
            BlockKind::Paragraph => {
                let (parent, props) = if ctx.quote > 0 {
                    Self::quote_paragraph(ctx)
                } else if ctx.in_list {
                    ("List_20_Contents", ParaProps::default())
                } else {
                    ("Text_20_body", ParaProps::default())
                };
                self.paragraph(parent, props, &block.inlines);
            }
            BlockKind::List { ordered, start, .. } => self.list(block, *ordered, *start),
            BlockKind::BlockQuote => {
                let inner = Ctx {
                    quote: ctx.quote.saturating_add(1),
                    ..ctx
                };
                self.blocks(&block.children, inner);
            }
            BlockKind::Callout { kind, title, .. } => {
                let label = match title.and_then(|t| t.get(self.src)) {
                    Some(text) if !text.trim().is_empty() => text.trim().to_string(),
                    _ => callout_label(*kind).to_string(),
                };
                let inner = Ctx {
                    quote: ctx.quote.saturating_add(1),
                    ..ctx
                };
                self.prev_space = true;
                let runs = self.text_run(
                    RunFmt {
                        bold: true,
                        ..RunFmt::default()
                    },
                    &label,
                );
                let (parent, props) = Self::quote_paragraph(inner);
                self.element("text:p", parent, props, "", &runs);
                self.blocks(&block.children, inner);
            }
            BlockKind::CodeBlock(code) => {
                let text = code.text(self.src);
                self.code_lines(text.trim_end_matches('\n'));
            }
            BlockKind::MathBlock(math) => {
                let tex = math.text(self.src);
                self.prev_space = true;
                let run = self.math_run(&tex, true);
                self.element(
                    "text:p",
                    "Text_20_body",
                    ParaProps {
                        align: Some("center"),
                        margin: 0,
                    },
                    "",
                    &run,
                );
            }
            BlockKind::Table(table) => self.table(table),
            BlockKind::ThematicBreak => {
                self.element("text:p", "Horizontal_20_Line", ParaProps::default(), "", "")
            }
            BlockKind::PageBreak => self.pending_break = true,
            BlockKind::HtmlBlock(html) => {
                if block.inlines.is_empty() {
                    let text = html.text(self.src);
                    for line in text.lines().filter(|l| !l.trim().is_empty()) {
                        self.prev_space = true;
                        let run = self.text_run(RunFmt::default(), line);
                        self.element("text:p", "Text_20_body", ParaProps::default(), "", &run);
                    }
                } else {
                    self.paragraph("Text_20_body", ParaProps::default(), &block.inlines);
                }
            }
            // list items are written by `list`; the rest is not part of the page
            BlockKind::ListItem { .. }
            | BlockKind::FrontMatter { .. }
            | BlockKind::FootnoteDefinition { .. }
            | BlockKind::LinkReferenceDefinition(_) => {}
        }
    }

    fn code_lines(&mut self, text: &str) {
        for line in text.split('\n') {
            self.prev_space = true;
            let inner = self.text_xml(line);
            self.element(
                "text:p",
                "Preformatted_20_Text",
                ParaProps::default(),
                "",
                &inner,
            );
        }
    }

    /// A list: `block` holds the items, `ordered` and `start` come from its kind.
    fn list(&mut self, block: &'a Block, ordered: bool, start: u32) {
        let mut current: Option<Flavor> = None;
        let mut lists = 0;
        for (index, item) in block.children.iter().enumerate() {
            let task = match &item.kind {
                BlockKind::ListItem { task, .. } => *task,
                _ => None,
            };
            let flavor = match task {
                Some(t) if t.checked => Flavor::Done,
                Some(_) => Flavor::Open,
                None if ordered => Flavor::Number,
                None => Flavor::Bullet,
            };
            if current != Some(flavor) {
                if current.is_some() {
                    self.body.push_str("</text:list>");
                }
                let resume = if flavor == Flavor::Number && lists > 0 {
                    r#" text:continue-numbering="true""#
                } else {
                    ""
                };
                self.lists_used[flavor.index()] = true;
                self.body.push_str(&format!(
                    r#"<text:list text:style-name="{}"{resume}>"#,
                    flavor.style()
                ));
                current = Some(flavor);
                lists += 1;
            }
            let number = if flavor == Flavor::Number && index == 0 && start != 1 {
                format!(r#" text:start-value="{start}""#)
            } else {
                String::new()
            };
            self.body.push_str(&format!("<text:list-item{number}>"));
            let inner = Ctx {
                quote: 0,
                in_list: true,
            };
            let mut first = true;
            for child in &item.children {
                if first && matches!(child.kind, BlockKind::Paragraph) {
                    self.paragraph("List_20_Contents", ParaProps::default(), &child.inlines);
                } else {
                    if first {
                        self.element("text:p", "List_20_Contents", ParaProps::default(), "", "");
                    }
                    self.block(child, inner);
                }
                first = false;
            }
            if first {
                self.element("text:p", "List_20_Contents", ParaProps::default(), "", "");
            }
            self.body.push_str("</text:list-item>");
        }
        if current.is_some() {
            self.body.push_str("</text:list>");
        }
    }

    fn table(&mut self, table: &Table) {
        let columns = table.header.cells.len().max(1);
        self.tables += 1;
        let name = format!("Table{}", self.tables);
        let page_break = std::mem::take(&mut self.pending_break);
        let width = cm(self.text_width_cm);
        let brk = if page_break {
            r#" fo:break-before="page""#
        } else {
            ""
        };
        self.table_styles.push_str(&format!(
            r#"<style:style style:name="{name}" style:family="table"><style:table-properties style:width="{width}cm" table:align="margins" fo:margin-top="0cm" fo:margin-bottom="0.25cm"{brk}/></style:style>"#
        ));
        if !self.column_counts.contains(&columns) {
            self.column_counts.push(columns);
            let column_width = cm(self.text_width_cm / columns as f64);
            self.table_styles.push_str(&format!(
                r#"<style:style style:name="Columns{columns}" style:family="table-column"><style:table-column-properties style:column-width="{column_width}cm"/></style:style>"#
            ));
        }
        self.body.push_str(&format!(
            r#"<table:table table:name="{name}" table:style-name="{name}">"#
        ));
        self.body.push_str(&format!(r#"<table:table-column table:style-name="Columns{columns}" table:number-columns-repeated="{columns}"/>"#));
        let rows = std::iter::once(&table.header).chain(table.rows.iter());
        for (row_index, row) in rows.enumerate() {
            let header = row_index == 0;
            self.body.push_str(if header {
                "<table:table-header-rows><table:table-row>"
            } else {
                "<table:table-row>"
            });
            for column in 0..columns {
                let align = match table.alignments.get(column) {
                    Some(Alignment::Left) => Some("start"),
                    Some(Alignment::Center) => Some("center"),
                    Some(Alignment::Right) => Some("end"),
                    _ => None,
                };
                self.prev_space = true;
                let inner = match row.cells.get(column) {
                    Some(cell) => self.inlines(&cell.inlines, RunFmt::default()),
                    None => String::new(),
                };
                let parent = if header {
                    "Table_20_Heading"
                } else {
                    "Table_20_Contents"
                };
                let open = self.open("text:p", parent, ParaProps { align, margin: 0 }, "");
                let cell_style = if header { "HeadCell" } else { "BodyCell" };
                self.body.push_str(&format!(
                    r#"<table:table-cell table:style-name="{cell_style}" office:value-type="string">{open}{inner}</text:p></table:table-cell>"#
                ));
            }
            self.body.push_str(if header {
                "</table:table-row></table:table-header-rows>"
            } else {
                "</table:table-row>"
            });
        }
        self.body.push_str("</table:table>");
    }

    // ---- output ------------------------------------------------------------------------------

    fn text_styles_xml(&self) -> String {
        let mut out = String::new();
        for (fmt, name) in &self.text_styles {
            let mut props = String::new();
            if fmt.bold {
                props.push_str(r#" fo:font-weight="bold" style:font-weight-asian="bold" style:font-weight-complex="bold""#);
            }
            if fmt.italic {
                props.push_str(r#" fo:font-style="italic" style:font-style-asian="italic" style:font-style-complex="italic""#);
            }
            if fmt.underline {
                props.push_str(r#" style:text-underline-style="solid" style:text-underline-width="auto" style:text-underline-color="font-color""#);
            }
            if fmt.strike {
                props.push_str(r#" style:text-line-through-style="solid" style:text-line-through-type="single""#);
            }
            if let Some(hex) = fmt.highlight {
                props.push_str(&format!(r#" fo:background-color="{hex}""#));
            }
            let parent = if fmt.code {
                r#" style:parent-style-name="Source_20_Text""#
            } else {
                ""
            };
            out.push_str(&format!(
                r#"<style:style style:name="{name}" style:family="text"{parent}><style:text-properties{props}/></style:style>"#
            ));
        }
        out
    }

    fn para_styles_xml(&self) -> String {
        let mut out = String::new();
        for (key, name) in &self.para_styles {
            let mut props = String::new();
            if key.page_break {
                props.push_str(r#" fo:break-before="page""#);
            }
            if let Some(align) = key.props.align {
                props.push_str(&format!(r#" fo:text-align="{align}""#));
            }
            if key.props.margin > 0 {
                props.push_str(&format!(r#" fo:margin-left="{}cm""#, key.props.margin));
            }
            out.push_str(&format!(
                r#"<style:style style:name="{name}" style:family="paragraph" style:parent-style-name="{}"><style:paragraph-properties{props}/></style:style>"#,
                key.parent
            ));
        }
        out
    }

    fn list_styles_xml(&self) -> String {
        let mut out = String::new();
        for flavor in [Flavor::Bullet, Flavor::Number, Flavor::Open, Flavor::Done] {
            if !self.lists_used[flavor.index()] {
                continue;
            }
            out.push_str(&format!(
                r#"<text:list-style style:name="{}">"#,
                flavor.style()
            ));
            for level in 1..=LIST_LEVELS {
                let margin = cm(0.635 * (level as f64 + 1.0));
                let alignment = format!(
                    r#"<style:list-level-properties text:list-level-position-and-space-mode="label-alignment"><style:list-level-label-alignment text:label-followed-by="listtab" text:list-tab-stop-position="{margin}cm" fo:text-indent="-0.635cm" fo:margin-left="{margin}cm"/></style:list-level-properties>"#
                );
                match flavor {
                    Flavor::Number => out.push_str(&format!(
                        r#"<text:list-level-style-number text:level="{level}" text:style-name="Numbering_20_Symbols" style:num-suffix="." style:num-format="1">{alignment}</text:list-level-style-number>"#
                    )),
                    _ => {
                        let bullet = match flavor {
                            Flavor::Open => "\u{2610}",
                            Flavor::Done => "\u{2611}",
                            _ => BULLETS[(level - 1) % BULLETS.len()],
                        };
                        out.push_str(&format!(
                            r#"<text:list-level-style-bullet text:level="{level}" text:style-name="Bullet_20_Symbols" text:bullet-char="{bullet}">{alignment}</text:list-level-style-bullet>"#
                        ));
                    }
                }
            }
            out.push_str("</text:list-style>");
        }
        out
    }

    fn content_xml(&self) -> String {
        let cell = |name: &str, fill: &str| {
            format!(
                r##"<style:style style:name="{name}" style:family="table-cell"><style:table-cell-properties fo:padding="0.1cm" fo:border="0.5pt solid #7F7F7F"{fill}/></style:style>"##
            )
        };
        let frame = r#"<style:style style:name="fr1" style:family="graphic" style:parent-style-name="Graphics"><style:graphic-properties style:vertical-pos="top" style:vertical-rel="baseline" fo:padding="0cm" fo:border="none" style:wrap="none"/></style:style>"#;
        format!(
            r#"{XML_DECL}<office:document-content {NS} office:version="1.2">{FONT_FACES}<office:automatic-styles>{}{}{}{}{}{}{frame}</office:automatic-styles><office:body><office:text>{}</office:text></office:body></office:document-content>"#,
            self.text_styles_xml(),
            self.para_styles_xml(),
            self.list_styles_xml(),
            self.table_styles,
            cell("HeadCell", r##" fo:background-color="#D9E2F3""##),
            cell("BodyCell", ""),
            self.body
        )
    }
}

fn point_size(half_points: u32) -> String {
    if half_points % 2 == 1 {
        format!("{}.5pt", half_points / 2)
    } else {
        format!("{}pt", half_points / 2)
    }
}

/// `styles.xml`: fonts, named styles, page layout.
fn styles_xml(options: &ExportOptions) -> String {
    let (page_w, page_h) = options.page.twips();
    let (width, height, margin) = (
        cm(twips_cm(page_w)),
        cm(twips_cm(page_h)),
        cm(twips_cm(options.margin_twips)),
    );
    let heading_color = if options.black_text {
        "#000000"
    } else {
        "#1F3864"
    };
    let quote_color = if options.black_text {
        "#000000"
    } else {
        "#595959"
    };
    let size = point_size(options.font_half_points);
    let mut s = format!(
        r#"{XML_DECL}<office:document-styles {NS} office:version="1.2">{FONT_FACES}<office:styles><style:default-style style:family="paragraph"><style:paragraph-properties fo:orphans="2" fo:widows="2" style:tab-stop-distance="1.27cm"/><style:text-properties style:font-name="Calibri" fo:font-size="{size}" fo:language="en" fo:country="US"/></style:default-style><style:style style:name="Standard" style:family="paragraph" style:class="text"/><style:style style:name="Text_20_body" style:display-name="Text body" style:family="paragraph" style:parent-style-name="Standard" style:class="text"><style:paragraph-properties fo:margin-top="0cm" fo:margin-bottom="0.25cm" fo:line-height="115%"/></style:style><style:style style:name="Heading" style:family="paragraph" style:parent-style-name="Standard" style:next-style-name="Text_20_body" style:class="text"><style:paragraph-properties fo:margin-top="0.42cm" fo:margin-bottom="0.14cm" fo:keep-with-next="always"/></style:style>"#
    );
    for (n, points) in HEADING_POINTS.iter().enumerate() {
        let level = n + 1;
        s.push_str(&format!(
            r#"<style:style style:name="Heading_20_{level}" style:display-name="Heading {level}" style:family="paragraph" style:parent-style-name="Heading" style:next-style-name="Text_20_body" style:default-outline-level="{level}" style:class="text"><style:text-properties fo:color="{heading_color}" fo:font-size="{points}pt" fo:font-weight="bold" style:font-weight-asian="bold" style:font-weight-complex="bold"/></style:style>"#
        ));
    }
    s.push_str(&format!(
        r##"<style:style style:name="Quotations" style:family="paragraph" style:parent-style-name="Text_20_body" style:class="html"><style:paragraph-properties fo:margin-left="1cm" fo:margin-right="1cm" fo:padding-left="0.3cm" fo:border-left="1.5pt solid #A6A6A6"/><style:text-properties fo:font-style="italic" fo:color="{quote_color}"/></style:style><style:style style:name="Preformatted_20_Text" style:display-name="Preformatted Text" style:family="paragraph" style:parent-style-name="Standard" style:class="html"><style:paragraph-properties fo:margin-top="0cm" fo:margin-bottom="0cm" fo:background-color="#F2F2F2" fo:padding="0.05cm" fo:border="none"/><style:text-properties style:font-name="Cascadia Mono" fo:font-size="10pt"/></style:style><style:style style:name="Horizontal_20_Line" style:display-name="Horizontal Line" style:family="paragraph" style:parent-style-name="Standard" style:class="html"><style:paragraph-properties fo:margin-top="0cm" fo:margin-bottom="0.25cm" fo:border-bottom="0.5pt solid #808080" fo:padding="0cm"/><style:text-properties fo:font-size="6pt"/></style:style><style:style style:name="List_20_Contents" style:display-name="List Contents" style:family="paragraph" style:parent-style-name="Standard" style:class="list"><style:paragraph-properties fo:margin-top="0cm" fo:margin-bottom="0.1cm"/></style:style><style:style style:name="Table_20_Contents" style:display-name="Table Contents" style:family="paragraph" style:parent-style-name="Standard" style:class="extra"><style:paragraph-properties fo:margin-top="0.05cm" fo:margin-bottom="0.05cm"/></style:style><style:style style:name="Table_20_Heading" style:display-name="Table Heading" style:family="paragraph" style:parent-style-name="Table_20_Contents" style:class="extra"><style:text-properties fo:font-weight="bold" style:font-weight-asian="bold" style:font-weight-complex="bold"/></style:style><style:style style:name="Footnote" style:family="paragraph" style:parent-style-name="Standard" style:class="extra"><style:paragraph-properties fo:margin-left="0.5cm" fo:text-indent="-0.5cm"/><style:text-properties fo:font-size="10pt"/></style:style><style:style style:name="Source_20_Text" style:display-name="Source Text" style:family="text"><style:text-properties style:font-name="Cascadia Mono" fo:font-size="10pt"/></style:style><style:style style:name="Internet_20_link" style:display-name="Internet link" style:family="text"><style:text-properties fo:color="{ACCENT}" style:text-underline-style="solid" style:text-underline-width="auto" style:text-underline-color="font-color"/></style:style><style:style style:name="Numbering_20_Symbols" style:display-name="Numbering Symbols" style:family="text"/><style:style style:name="Bullet_20_Symbols" style:display-name="Bullet Symbols" style:family="text"/><style:style style:name="Graphics" style:family="graphic"><style:graphic-properties text:anchor-type="paragraph" svg:x="0cm" svg:y="0cm" style:wrap="none" style:vertical-pos="top" style:vertical-rel="paragraph" style:horizontal-pos="center" style:horizontal-rel="paragraph"/></style:style></office:styles><office:automatic-styles><style:page-layout style:name="pm1"><style:page-layout-properties fo:page-width="{width}cm" fo:page-height="{height}cm" style:print-orientation="portrait" fo:margin-top="{margin}cm" fo:margin-bottom="{margin}cm" fo:margin-left="{margin}cm" fo:margin-right="{margin}cm"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="Standard" style:page-layout-name="pm1"/></office:master-styles></office:document-styles>"##
    ));
    s
}

fn meta_xml(options: &ExportOptions) -> String {
    let mut s = format!(
        r#"{XML_DECL}<office:document-meta {NS} office:version="1.2"><office:meta><meta:generator>SS Writer</meta:generator>"#
    );
    if let Some(title) = &options.title {
        s.push_str(&format!("<dc:title>{}</dc:title>", esc(title)));
    }
    s.push_str("<meta:initial-creator>SS Writer</meta:initial-creator>");
    if let Some(created) = &options.created {
        s.push_str(&format!(
            "<meta:creation-date>{}</meta:creation-date>",
            esc(created)
        ));
    }
    if let Some(modified) = &options.modified {
        s.push_str(&format!("<dc:date>{}</dc:date>", esc(modified)));
    }
    s.push_str("</office:meta></office:document-meta>");
    s
}

fn manifest_xml(media: &[(String, &'static str, Vec<u8>)]) -> String {
    let mut s = format!(
        r#"{XML_DECL}<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:version="1.2" manifest:media-type="{MIME}"/>"#
    );
    for part in ["content.xml", "styles.xml", "meta.xml"] {
        s.push_str(&format!(
            r#"<manifest:file-entry manifest:full-path="{part}" manifest:media-type="text/xml"/>"#
        ));
    }
    for (file, mime, _) in media {
        s.push_str(&format!(r#"<manifest:file-entry manifest:full-path="Pictures/{file}" manifest:media-type="{mime}"/>"#));
    }
    s.push_str("</manifest:manifest>");
    s
}

/// Converts Markdown to an ODT package.
pub fn export_odt(
    markdown: &str,
    options: &ExportOptions,
    resources: &mut Resources<'_>,
) -> Result<Vec<u8>, ExportError> {
    let document = parse_document(markdown);
    let mut writer = Writer::new(markdown, options, resources);
    for block in document.iter_blocks() {
        if let BlockKind::FootnoteDefinition { label } = &block.kind {
            writer
                .footnote_defs
                .entry(normalize_label(label))
                .or_insert(block);
        }
    }
    let mut used: HashMap<String, usize> = HashMap::new();
    for block in document.iter_blocks() {
        if matches!(block.kind, BlockKind::Heading { .. }) {
            let mut text = String::new();
            inline_text(markdown, &block.inlines, &mut text);
            let title = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let slug = unique_slug(&title, &mut used);
            let name = format!("_ssw_h{}", writer.heading_bookmarks.len() + 1);
            writer.anchors.insert(slug, name.clone());
            writer.heading_bookmarks.insert(block.range.start, name);
        }
    }
    writer.blocks(&document.blocks, Ctx::default());
    if writer.pending_break {
        writer.element("text:p", "Text_20_body", ParaProps::default(), "", "");
    }

    let parts = [
        ("META-INF/manifest.xml", manifest_xml(&writer.media)),
        ("content.xml", writer.content_xml()),
        ("styles.xml", styles_xml(options)),
        ("meta.xml", meta_xml(options)),
    ];
    zip_package(Cursor::new(Vec::new()), &parts, &writer.media).map(Cursor::into_inner)
}

/// Writes the package into `sink`: the stored `mimetype` first, then the XML `parts` and the
/// `Pictures/` of `media`, all deflated.
fn zip_package<W: Write + Seek>(
    sink: W,
    parts: &[(&str, String)],
    media: &[(String, &'static str, Vec<u8>)],
) -> Result<W, ExportError> {
    let mut zip = ZipWriter::new(sink);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("mimetype", stored)?;
    zip.write_all(MIME.as_bytes())?;
    let entries = parts
        .iter()
        .map(|(name, xml)| (name.to_string(), xml.as_bytes()))
        .chain(
            media
                .iter()
                .map(|(file, _, bytes)| (format!("Pictures/{file}"), bytes.as_slice())),
        );
    for (name, bytes) in entries {
        zip.start_file(name, deflated)?;
        zip.write_all(bytes)?;
    }
    Ok(zip.finish()?)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::docx::{MathImage, PageSize};
    use crate::test_sink::{Limited, noise};

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
        ];
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0]);
        v
    }

    fn gif(w: u16, h: u16) -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[0; 8]);
        v
    }

    /// A resource provider without any images.
    fn no_image(_: &str) -> Option<Vec<u8>> {
        None
    }

    /// A resource provider without any rendered formulas.
    fn no_math(_: &str, _: bool) -> Option<MathImage> {
        None
    }

    #[test]
    fn formulas_and_images_without_a_provider_fall_back_to_text() {
        let bytes = export_with(
            "$x$ and ![alt](a.png)\n",
            &ExportOptions::default(),
            &mut no_image,
            &mut no_math,
        );
        let xml = text_part(&bytes, "content.xml");
        assert_lacks(&xml, &["draw:frame", "Pictures/"]);
        assert!(
            archive_names(&bytes)
                .iter()
                .all(|n| !n.starts_with("Pictures/"))
        );
    }

    fn archive_names(bytes: &[u8]) -> Vec<String> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
            .collect()
    }

    #[test]
    fn a_sink_that_gives_out_at_any_byte_fails_the_package() {
        let parts = [("content.xml", "<office:document-content/>".to_string())];
        let media = [("image1.png".to_string(), "image/png", png(2, 2))];
        let full = zip_package(Cursor::new(Vec::new()), &parts, &media)
            .unwrap()
            .into_inner();
        // The failure lands in the mimetype, in the XML part, in the picture or in the central
        // directory, depending on the budget; every one of them is reported.
        for budget in 0..full.len() as u64 {
            let sink = Limited {
                inner: Cursor::new(Vec::new()),
                budget,
            };
            let err = zip_package(sink, &parts, &media).expect_err(&format!(
                "a budget of {budget} bytes cannot hold the package"
            ));
            assert!(err.to_string().contains("sink is full"), "{budget}: {err}");
        }
        let sink = Limited {
            inner: Cursor::new(Vec::new()),
            budget: full.len() as u64,
        };
        let written = zip_package(sink, &parts, &media).unwrap();
        assert_eq!(written.inner.into_inner(), full);
    }

    #[test]
    fn a_sink_that_gives_out_while_large_entries_are_deflated_fails_the_package() {
        // Incompressible entries: the deflater passes output on to the sink during the write
        // of the entry itself, so these budgets fail inside the XML part and inside the picture.
        let xml: String = noise(150_000)
            .iter()
            .map(|b| char::from(b'a' + b % 26))
            .collect();
        let parts = [("content.xml", xml)];
        let media = [("big.bin".to_string(), "image/png", noise(150_000))];
        let full = zip_package(Cursor::new(Vec::new()), &parts, &media)
            .unwrap()
            .into_inner();
        let step = full.len() as u64 / 40;
        for budget in (0..full.len() as u64).step_by(step as usize) {
            let sink = Limited {
                inner: Cursor::new(Vec::new()),
                budget,
            };
            let err = zip_package(sink, &parts, &media).expect_err(&format!(
                "a budget of {budget} bytes cannot hold the package"
            ));
            assert!(err.to_string().contains("sink is full"), "{budget}: {err}");
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(&full[..])).unwrap();
        let mut content = String::new();
        archive
            .by_name("content.xml")
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, parts[0].1);
        let mut picture = Vec::new();
        archive
            .by_name("Pictures/big.bin")
            .unwrap()
            .read_to_end(&mut picture)
            .unwrap();
        assert_eq!(picture, media[0].2);
    }

    fn export_with(
        md: &str,
        options: &ExportOptions,
        image: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
        math: &mut dyn FnMut(&str, bool) -> Option<MathImage>,
    ) -> Vec<u8> {
        let mut resources = Resources { image, math };
        export_odt(md, options, &mut resources).unwrap()
    }

    /// The package of `md` with `p.png` (96x48), `p.jpg`, `p.gif` and `bad.png` available.
    fn package(md: &str, options: &ExportOptions) -> Vec<u8> {
        let mut image = |dest: &str| match dest {
            "p.png" => Some(png(96, 48)),
            "p.jpg" => Some(jpeg(10, 20)),
            "p.gif" => Some(gif(30, 40)),
            "bad.png" => Some(b"not an image".to_vec()),
            _ => None,
        };
        let mut math = |tex: &str, display: bool| match (tex, display) {
            ("x", true) => Some(MathImage {
                png: png(30, 10),
                width_px: 30,
                height_px: 10,
            }),
            ("bad", _) => Some(MathImage {
                png: b"junk".to_vec(),
                width_px: 1,
                height_px: 1,
            }),
            _ => None,
        };
        export_with(md, options, &mut image, &mut math)
    }

    fn part(bytes: &[u8], name: &str) -> Vec<u8> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut out = Vec::new();
        archive
            .by_name(name)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    fn text_part(bytes: &[u8], name: &str) -> String {
        String::from_utf8(part(bytes, name)).unwrap()
    }

    fn content(md: &str) -> String {
        text_part(&package(md, &ExportOptions::default()), "content.xml")
    }

    fn assert_has(xml: &str, fragments: &[&str]) {
        for fragment in fragments {
            assert!(xml.contains(fragment), "missing {fragment}\nin {xml}");
        }
    }

    fn assert_lacks(xml: &str, fragments: &[&str]) {
        for fragment in fragments {
            assert!(!xml.contains(fragment), "unexpected {fragment}\nin {xml}");
        }
    }

    #[test]
    fn escaping_and_lengths() {
        assert_eq!(
            esc("a&b<c>d\"e\t\n\r\u{1}\u{FFFE}\u{FFFF}x"),
            "a&amp;b&lt;c&gt;d&quot;e\t\n\rx"
        );
        assert_eq!(attr("a\nb\tc\r<"), "a b c &lt;");
        assert_eq!(
            [cm(2.5), cm(2.0), cm(1.0 / 3.0), cm(0.0)],
            ["2.5", "2", "0.333", "0"]
        );
        assert_eq!(cm(twips_cm(1440)), "2.54");
        assert_eq!(cm(px_cm(96)), "2.54");
        assert_eq!([point_size(22), point_size(21)], ["11pt", "10.5pt"]);
    }

    #[test]
    fn image_formats_and_sizes_are_read_from_the_headers() {
        let info = |b: &[u8]| image_info(b).map(|i| (i.ext, i.mime, i.width, i.height));
        assert_eq!(info(&png(5, 7)), Some(("png", "image/png", 5, 7)));
        assert_eq!(info(&gif(3, 4)), Some(("gif", "image/gif", 3, 4)));
        assert_eq!(
            info(b"GIF87a\x01\x00\x02\x00"),
            Some(("gif", "image/gif", 1, 2))
        );
        assert_eq!(info(&jpeg(6, 8)), Some(("jpg", "image/jpeg", 6, 8)));
        let mut skipping = vec![
            0xFF, 0xD8, 0xFF, 0xFF, 0xC4, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xD0, 0xFF, 0x01,
        ];
        skipping.extend_from_slice(&jpeg(9, 11)[2..]);
        assert_eq!(info(&skipping), Some(("jpg", "image/jpeg", 9, 11)));
        let mut wrong_chunk = png(5, 7);
        wrong_chunk[12] = b'X';
        let cases: [Vec<u8>; 15] = [
            wrong_chunk,
            png(0, 7),
            png(5, 0),
            png(5, 7)[..18].to_vec(),
            png(5, 7)[..20].to_vec(),
            gif(0, 4),
            b"GIF89a".to_vec(),
            b"GIF89a\x01".to_vec(),
            b"GIF89a\x01\x00".to_vec(),
            // a JPEG that ends in padding, before the marker, in the frame height or width
            vec![0xFF, 0xD8, 0xFF],
            jpeg(2, 3)[..4].to_vec(),
            jpeg(2, 3)[..9].to_vec(),
            vec![0xFF, 0xD8, 0xFF, 0xD9, 0x00],
            vec![0xFF, 0xD8],
            b"BM nothing".to_vec(),
        ];
        for bytes in cases {
            assert_eq!(info(&bytes), None, "{bytes:?}");
        }
    }

    #[test]
    fn package_layout_and_metadata() {
        let options = ExportOptions {
            title: Some("T&U".into()),
            created: Some("2024-01-02T03:04:05Z".into()),
            modified: Some("2024-02-03T04:05:06Z".into()),
            ..ExportOptions::default()
        };
        let bytes = package("![a](p.png)\n", &options);
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes[..])).unwrap();
        assert_eq!(archive.by_index(0).unwrap().name_raw(), b"mimetype");
        assert_eq!(
            archive.by_index(0).unwrap().compression(),
            CompressionMethod::Stored
        );
        assert_eq!(part(&bytes, "mimetype"), MIME.as_bytes());
        assert_eq!(part(&bytes, "Pictures/image1.png"), png(96, 48));
        let meta = text_part(&bytes, "meta.xml");
        assert_has(
            &meta,
            &[
                "<dc:title>T&amp;U</dc:title>",
                "<meta:creation-date>2024-01-02T03:04:05Z</meta:creation-date>",
                "<dc:date>2024-02-03T04:05:06Z</dc:date>",
            ],
        );
        let plain = text_part(&package("x\n", &ExportOptions::default()), "meta.xml");
        assert_lacks(&plain, &["<dc:title>", "<meta:creation-date>", "<dc:date>"]);
        let manifest = text_part(&bytes, "META-INF/manifest.xml");
        assert_has(
            &manifest,
            &[
                r#"manifest:full-path="Pictures/image1.png" manifest:media-type="image/png""#,
                r#"manifest:full-path="content.xml""#,
            ],
        );
    }

    #[test]
    fn page_and_style_options_reach_styles_xml() {
        let styles = |options: &ExportOptions| text_part(&package("x\n", options), "styles.xml");
        let letter = styles(&ExportOptions {
            page: PageSize::Letter,
            font_half_points: 21,
            ..ExportOptions::default()
        });
        assert_has(
            &letter,
            &[
                "21.59cm",
                "10.5pt",
                "#1F3864",
                "#595959",
                "Heading_20_6",
                "Quotations",
                "Preformatted_20_Text",
                "Source_20_Text",
            ],
        );
        let black = styles(&ExportOptions {
            black_text: true,
            page: PageSize::A4,
            ..ExportOptions::default()
        });
        assert_lacks(&black, &["#1F3864", "#595959"]);
        assert_has(&black, &["#000000"]);
    }

    #[test]
    fn headings_paragraphs_and_inline_formatting() {
        let xml = content("# A\n\n## B\n\n###### F\n\nplain **b** *i* ~u~ ~~s~~ ==h== `c` **b**\n");
        assert_has(
            &xml,
            &[
                r#"<text:h text:style-name="Heading_20_1" text:outline-level="1"><text:bookmark-start text:name="_ssw_h1"/>A<text:bookmark-end text:name="_ssw_h1"/></text:h>"#,
                r#"<text:h text:style-name="Heading_20_2" text:outline-level="2">"#,
                r#"<text:h text:style-name="Heading_20_6" text:outline-level="6">"#,
                r#"<text:p text:style-name="Text_20_body">plain "#,
                r#"fo:font-weight="bold""#,
                r#"fo:font-style="italic""#,
                r#"style:text-underline-style="solid""#,
                r#"style:text-line-through-style="solid""#,
                r#"fo:background-color="#,
                r#"style:parent-style-name="Source_20_Text""#,
            ],
        );
        assert_eq!(
            xml.matches(r#"<text:span text:style-name="T1">"#).count(),
            2,
            "the bold style is reused"
        );
        assert_lacks(&xml, &["T8"]);
    }

    #[test]
    fn whitespace_special_characters_and_breaks() {
        let xml = content(
            "```\n\tx\n  y\na  b\n```\n\nline<br>two\\\nthree and <b>tag</b> &amp; &copy; \\*star\\*\nsoft\n",
        );
        assert_has(
            &xml,
            &[
                r#"<text:p text:style-name="Preformatted_20_Text"><text:tab/>x</text:p>"#,
                r#"<text:p text:style-name="Preformatted_20_Text"><text:s text:c="2"/>y</text:p>"#,
                r#"<text:p text:style-name="Preformatted_20_Text">a <text:s text:c="1"/>b</text:p>"#,
                "line<text:line-break/>two<text:line-break/>three",
                "&lt;b&gt;tag&lt;/b&gt;",
                "&amp; \u{a9} *star* soft",
            ],
        );
    }

    #[test]
    fn lists_have_styles_numbering_and_nesting() {
        let xml =
            content("- a\n    - b\n- c\n\ntext\n\n3. x\n4. y\n\ntext\n\n- [ ] open\n- [x] done\n");
        assert_has(
            &xml,
            &[
                r#"<text:list text:style-name="L1"><text:list-item><text:p text:style-name="List_20_Contents">a</text:p><text:list text:style-name="L1">"#,
                r#"<text:list text:style-name="L2"><text:list-item text:start-value="3">"#,
                r#"<text:list text:style-name="L3">"#,
                r#"<text:list text:style-name="L4">"#,
                r#"<text:list-style style:name="L1">"#,
                r#"text:list-level-style-number text:level="10""#,
                r#"style:num-format="1""#,
                "text:bullet-char=\"\u{2022}\"",
                "text:bullet-char=\"\u{25e6}\"",
                "text:bullet-char=\"\u{25aa}\"",
                "text:bullet-char=\"\u{2610}\"",
                "text:bullet-char=\"\u{2611}\"",
            ],
        );
        let ordered = content("1. [ ] a\n2. b\n3. c\n");
        assert_has(
            &ordered,
            &[
                r#"<text:list text:style-name="L3">"#,
                r#"<text:list text:style-name="L2" text:continue-numbering="true">"#,
            ],
        );
        assert_lacks(&ordered, &["text:start-value"]);
        let plain = content("- a\n");
        assert_lacks(
            &plain,
            &[
                r#"style:name="L2""#,
                r#"style:name="L3""#,
                r#"style:name="L4""#,
            ],
        );
    }

    #[test]
    fn list_items_with_several_blocks_or_none() {
        let xml = content("- a\n\n  b\n- # h\n-\n- last\n");
        assert_has(
            &xml,
            &[
                r#"<text:p text:style-name="List_20_Contents">a</text:p><text:p text:style-name="List_20_Contents">b</text:p></text:list-item>"#,
                r#"<text:list-item><text:p text:style-name="List_20_Contents"></text:p><text:h"#,
                r#"<text:list-item><text:p text:style-name="List_20_Contents"></text:p></text:list-item>"#,
            ],
        );
    }

    #[test]
    fn quotes_callouts_rules_and_page_breaks() {
        let xml = content(
            "> a\n>\n> > b\n\n> [!NOTE]\n> body\n\n> [!TIP] My tip\n> body\n\n---\n\n<!-- pagebreak -->\n\nafter\n\n<!-- pagebreak -->\n",
        );
        assert_has(
            &xml,
            &[
                r#"<text:p text:style-name="Quotations">a</text:p>"#,
                r#"<text:p text:style-name="P1">b</text:p>"#,
                r#"style:name="P1" style:family="paragraph" style:parent-style-name="Quotations"><style:paragraph-properties fo:margin-left="1cm"/>"#,
                ">Note</text:span>",
                ">My tip</text:span>",
                r#"<text:p text:style-name="Horizontal_20_Line"></text:p>"#,
                r#"fo:break-before="page""#,
                r#"</text:p></office:text>"#,
            ],
        );
        for (keyword, label) in [
            ("NOTE", "Note"),
            ("TIP", "Tip"),
            ("IMPORTANT", "Important"),
            ("WARNING", "Warning"),
            ("CAUTION", "Caution"),
        ] {
            let callout = content(&format!("> [!{keyword}]\n> x\n"));
            assert_has(&callout, &[&format!(">{label}</text:span>")]);
        }
    }

    #[test]
    fn tables_have_alignment_columns_and_padding() {
        let xml = content(
            "| a | b | c |\n| :-- | :-: | --: |\n| 1 | 2 | 3 |\n\n| x | y |\n| - | - |\n| 1 |\n\n<!-- pagebreak -->\n\n| z | w |\n| - | - |\n| 1 | 2 |\n",
        );
        assert_has(
            &xml,
            &[
                r#"<table:table table:name="Table1" table:style-name="Table1">"#,
                r#"<table:table-column table:style-name="Columns3" table:number-columns-repeated="3"/>"#,
                "<table:table-header-rows><table:table-row>",
                r#"fo:text-align="start""#,
                r#"fo:text-align="center""#,
                r#"fo:text-align="end""#,
                r#"<table:table-cell table:style-name="BodyCell" office:value-type="string"><text:p text:style-name="Table_20_Contents"></text:p></table:table-cell>"#,
                r#"style:name="Table3""#,
                r#"fo:break-before="page""#,
            ],
        );
        assert_eq!(
            xml.matches(r#"style:name="Columns2" style:family="table-column""#)
                .count(),
            1
        );
    }

    #[test]
    fn footnotes_are_inlined_where_they_are_referenced() {
        let xml = content(
            "a[^1] and [^2] and b[^3]\n\n[^1]: The note.\n\n[^3]: See[^1]\n\n    second paragraph\n",
        );
        assert_has(
            &xml,
            &[
                r#"<text:note text:id="ftn1" text:note-class="footnote"><text:note-citation>1</text:note-citation><text:note-body><text:p text:style-name="Footnote">The note.</text:p></text:note-body></text:note>"#,
                r#"<text:note text:id="ftn2" text:note-class="footnote">"#,
                "[^2]",
                r#"<text:p text:style-name="Footnote">second paragraph</text:p>"#,
            ],
        );
        assert_lacks(&xml, &["ftn3"]);
        assert_lacks(&content("[^1]: lonely\n"), &["<text:note"]);
    }

    #[test]
    fn links_use_bookmarks_for_headings() {
        let xml = content("# One\n\n[a](https://e.com/x?a=1&b=2) [b](#one) [c](#nope)\n");
        assert_has(
            &xml,
            &[
                r#"xlink:href="https://e.com/x?a=1&amp;b=2" text:style-name="Internet_20_link""#,
                r##"xlink:href="#_ssw_h1""##,
                r##"xlink:href="#nope""##,
            ],
        );
    }

    #[test]
    fn images_are_embedded_clamped_or_replaced_by_their_text() {
        let xml = content(
            "![alt](p.png) ![](missing.png) ![x](bad.png) ![w|40](p.png) ![wh|40x10](p.png) ![](p.jpg) ![](p.gif)\n",
        );
        assert_has(
            &xml,
            &[
                r#"svg:width="2.54cm" svg:height="1.27cm""#,
                r#"svg:width="1.058cm" svg:height="0.529cm""#,
                r#"svg:width="1.058cm" svg:height="0.265cm""#,
                "<svg:desc>alt</svg:desc>",
                r#"xlink:href="Pictures/image1.png""#,
                r#"xlink:href="Pictures/image4.jpg""#,
                r#"xlink:href="Pictures/image5.gif""#,
                "</draw:frame> missing.png x <draw:frame",
            ],
        );
        let huge = text_part(
            &package("![](huge.png)\n", &ExportOptions::default()),
            "content.xml",
        );
        assert_lacks(&huge, &["draw:frame"]);
        let mut big = |_: &str| Some(png(100_000, 50_000));
        let bytes = export_with(
            "![](big.png)\n",
            &ExportOptions::default(),
            &mut big,
            &mut no_math,
        );
        let clamped = text_part(&bytes, "content.xml");
        assert_has(&clamped, &["draw:frame"]);
        assert_lacks(&clamped, &["100000"]);
    }

    #[test]
    fn math_becomes_an_image_or_code() {
        let block = content("$$x$$\n");
        assert_has(
            &block,
            &[
                r#"fo:text-align="center""#,
                "<svg:desc>x</svg:desc>",
                r#"xlink:href="Pictures/image1.png""#,
            ],
        );
        let inline = content("a $x$ b $bad$ c $$bad$$\n");
        assert_lacks(&inline, &["draw:frame"]);
        assert_has(
            &inline,
            &[
                r#"style:parent-style-name="Source_20_Text""#,
                ">x</text:span>",
                ">bad</text:span>",
            ],
        );
    }

    #[test]
    fn html_front_matter_and_definitions() {
        let xml = content(
            "---\ntitle: hidden\n---\n\n[ref]: http://x.org\n\n<div>one</div>\n<p>two</p>\n\n<img src=\"p.png\" alt=\"pic\">\n\n[[Wiki Page]]\n",
        );
        assert_has(
            &xml,
            &[
                "&lt;div&gt;one&lt;/div&gt;",
                "&lt;p&gt;two&lt;/p&gt;",
                "<svg:desc>pic</svg:desc>",
                "Wiki Page",
            ],
        );
        assert_lacks(&xml, &["hidden", "http://x.org"]);
    }

    #[test]
    fn list_children_that_are_not_items_count_as_plain_items() {
        let md = "- a\n- b\n";
        let mut document = parse_document(md);
        document.blocks[0].children[1].kind = BlockKind::ThematicBreak;
        let options = ExportOptions::default();
        let mut resources = Resources {
            image: &mut no_image,
            math: &mut no_math,
        };
        let mut writer = Writer::new(md, &options, &mut resources);
        writer.blocks(&document.blocks, Ctx::default());
        assert_eq!(writer.body.matches("<text:list-item>").count(), 2);
        assert_eq!(writer.body.matches("<text:list ").count(), 1);
    }

    #[test]
    fn a_list_without_items_writes_nothing() {
        let md = "- a\n";
        let mut document = parse_document(md);
        document.blocks[0].children.clear();
        let options = ExportOptions::default();
        let mut resources = Resources {
            image: &mut no_image,
            math: &mut no_math,
        };
        let mut writer = Writer::new(md, &options, &mut resources);
        writer.blocks(&document.blocks, Ctx::default());
        assert_eq!(writer.body, "");
    }

    #[test]
    fn jpeg_headers_may_have_noise_between_segments() {
        let mut noisy = vec![0xFF, 0xD8, 0x12, 0x34, 0xFF];
        noisy.extend_from_slice(&jpeg(2, 3)[3..]);
        let info = image_info(&noisy).unwrap();
        assert_eq!((info.ext, info.width, info.height), ("jpg", 2, 3));
    }

    #[test]
    fn plain_text_of_inlines_skips_images_and_footnote_references() {
        let src = "a \\* &amp; `c` $x$ [[Page]] ![i](p.png) j[^1] *k*\nl\\\nm\n\n[^1]: note\n";
        let document = parse_document(src);
        let mut text = String::new();
        inline_text(src, &document.blocks[0].inlines, &mut text);
        assert_eq!(text, "a * & c x Page  j k l m");
    }

    #[test]
    fn block_text_joins_nested_blocks_with_spaces() {
        let src = "- one\n- two\n";
        let document = parse_document(src);
        assert_eq!(block_text(src, &document.blocks[0]), "one two");
    }

    #[test]
    fn text_xml_keeps_line_breaks_tabs_and_trailing_spaces() {
        let mut resources = Resources {
            image: &mut no_image,
            math: &mut no_math,
        };
        let mut writer = Writer::new("", &ExportOptions::default(), &mut resources);
        assert_eq!(
            writer.text_xml("a\nb\rc\td"),
            "a<text:line-break/>b<text:line-break/>c<text:tab/>d"
        );
        writer.prev_space = true;
        assert_eq!(writer.text_xml("x  "), r#"x <text:s text:c="1"/>"#);
        assert_eq!(writer.text_run(RunFmt::default(), ""), "");
    }

    #[test]
    fn height_only_images_keep_their_aspect_ratio() {
        // p.jpg is 10x20 pixels: a height of 40 gives a width of 20
        let xml = content("<img src=\"p.jpg\" height=\"40\">\n");
        assert_has(&xml, &[r#"svg:width="0.529cm" svg:height="1.058cm""#]);
    }

    #[test]
    fn footnotes_with_lists_and_without_content() {
        let xml = content("x[^1] y[^2]\n\n[^1]: first\n\n    - one\n    - two\n\n[^2]:\n");
        assert_has(
            &xml,
            &[
                r#"<text:p text:style-name="Footnote">first</text:p><text:p text:style-name="Footnote">one two</text:p>"#,
                r#"<text:note-body><text:p text:style-name="Footnote"/></text:note-body>"#,
            ],
        );
    }

    #[test]
    fn the_first_definition_of_a_footnote_label_wins() {
        let xml = content("x[^a]\n\n[^a]: first\n\n[^a]: second\n");
        assert_has(
            &xml,
            &[r#"<text:p text:style-name="Footnote">first</text:p>"#],
        );
        assert_lacks(&xml, &["second"]);
    }

    #[test]
    fn errors_convert_into_export_errors() {
        let io = ExportError::from(std::io::Error::other("disk full"));
        assert_eq!(io, ExportError::Package("disk full".into()));
        let zip = ExportError::from(zip::result::ZipError::FileNotFound);
        assert_eq!(
            zip,
            ExportError::Package("specified file not found in archive".into())
        );
        assert!(io.to_string().contains("disk full"));
    }
}
