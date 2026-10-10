//! Markdown → DOCX.
//!
//! The package is assembled from hand-built WordprocessingML (see `ooxml`). Child elements are
//! emitted in the order the ECMA-376 schema requires.

use std::collections::HashMap;

use ssw_core::markdown::ast::{
    Alignment, Block, BlockKind, CalloutKind, HighlightColor, Inline, InlineKind,
};
use ssw_core::markdown::definitions::normalize_label;
use ssw_core::markdown::{parse_document, unescape_markdown};

use crate::ooxml::{
    ContentTypes, EMU_PER_PX, ImageFormat, PackageError, PackageWriter, RelationshipsBuilder,
    XML_DECL, escape_xml, image_info, rel,
};

use super::numbering::numbering_xml;
use super::read::slugify;
use super::styles::{settings_xml, styles_xml};

/// Page size of the exported document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSize {
    A4,
    Letter,
}

impl PageSize {
    /// Width and height in twentieths of a point.
    pub fn twips(self) -> (u32, u32) {
        match self {
            PageSize::A4 => (11906, 16838),
            PageSize::Letter => (12240, 15840),
        }
    }
}

/// Export settings. Dates are ISO 8601 / W3CDTF strings supplied by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOptions {
    pub title: Option<String>,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub page: PageSize,
    /// Margin on every side, in twips.
    pub margin_twips: u32,
    /// Base font size in half-points.
    pub font_half_points: u32,
    /// Headings and quotes in black instead of theme colors.
    pub black_text: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            title: None,
            created: None,
            modified: None,
            page: PageSize::A4,
            margin_twips: 1440,
            font_half_points: 22,
            black_text: false,
        }
    }
}

/// A rendered formula: PNG bytes and the size to display them at, in CSS pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MathImage {
    pub png: Vec<u8>,
    pub width_px: u32,
    pub height_px: u32,
}

/// Callbacks the exporter uses for content it cannot produce itself.
pub struct Resources<'a> {
    /// Image bytes (PNG, JPEG or GIF) for an image destination; `None` when unavailable.
    pub image: &'a mut dyn FnMut(&str) -> Option<Vec<u8>>,
    /// Rendered formula for TeX source (`display` = block math); `None` falls back to code text.
    pub math: &'a mut dyn FnMut(&str, bool) -> Option<MathImage>,
}

impl std::fmt::Debug for Resources<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Resources")
    }
}

/// Why a DOCX could not be written.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ExportError {
    #[error(transparent)]
    Package(#[from] PackageError),
}

const NS_DECLS: &str = concat!(
    r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" "#,
    r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" "#,
    r#"xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" "#,
    r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
    r#"xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture""#
);

const CT_PREFIX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml";

#[derive(Debug, Clone, Default)]
struct RunFmt {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    highlight: Option<HighlightColor>,
    style: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Ctx {
    /// Block quote nesting depth.
    quote: u8,
    /// Inside a list item.
    in_list: bool,
    /// List nesting level of the enclosing item.
    level: u8,
}

struct Writer<'a, 'r> {
    src: &'a str,
    options: &'a ExportOptions,
    resources: &'a mut Resources<'r>,
    body: String,
    rels: RelationshipsBuilder,
    media: Vec<(String, Vec<u8>)>,
    drawing_id: u32,
    bookmark_id: u32,
    footnotes: Vec<String>,
    footnote_defs: HashMap<String, &'a Block>,
    /// (start, level) of every ordered list instance; instance `i` is `w:numId` `100 + i`.
    ordered: Vec<(u32, u8)>,
    /// Heading anchor (`#slug`) → bookmark name.
    anchors: HashMap<String, String>,
    /// Heading source offset → bookmark name.
    heading_bookmarks: HashMap<usize, String>,
    content_width_px: u32,
}

fn num_pr(num_id: u32, level: u8) -> String {
    format!(r#"<w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="{num_id}"/></w:numPr>"#)
}

fn highlight_name(color: HighlightColor) -> Option<&'static str> {
    match color {
        HighlightColor::Yellow => Some("yellow"),
        HighlightColor::Green => Some("green"),
        HighlightColor::Blue => Some("cyan"),
        HighlightColor::Red => Some("red"),
        HighlightColor::Purple => Some("magenta"),
        HighlightColor::Orange => None,
    }
}

/// Plain text of an inline sequence (used for anchors, alt text and fallbacks).
fn inline_text(src: &str, inlines: &[Inline], out: &mut String) {
    for inline in inlines {
        match &inline.kind {
            InlineKind::Text | InlineKind::RawHtml => {
                out.push_str(inline.range.get(src).unwrap_or(""));
            }
            InlineKind::Escape => out.push_str(inline.content.get(src).unwrap_or("")),
            InlineKind::Entity { decoded } => out.push_str(decoded),
            InlineKind::Code { text } => out.push_str(text),
            InlineKind::InlineMath | InlineKind::WikiLink(_) => {
                out.push_str(inline.content.get(src).unwrap_or(""));
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

impl<'a, 'r> Writer<'a, 'r> {
    fn new(src: &'a str, options: &'a ExportOptions, resources: &'a mut Resources<'r>) -> Self {
        let (width, _) = options.page.twips();
        let content = width.saturating_sub(options.margin_twips.saturating_mul(2));
        let mut rels = RelationshipsBuilder::new();
        rels.add(rel::STYLES, "styles.xml", false);
        rels.add(rel::NUMBERING, "numbering.xml", false);
        rels.add(rel::SETTINGS, "settings.xml", false);
        rels.add(rel::FOOTNOTES, "footnotes.xml", false);
        Writer {
            src,
            options,
            resources,
            body: String::new(),
            rels,
            media: Vec::new(),
            drawing_id: 0,
            bookmark_id: 0,
            footnotes: Vec::new(),
            footnote_defs: HashMap::new(),
            ordered: Vec::new(),
            anchors: HashMap::new(),
            heading_bookmarks: HashMap::new(),
            content_width_px: (content / 15).max(1),
        }
    }

    // ---- runs --------------------------------------------------------------------------------

    fn run(&self, fmt: &RunFmt, inner: &str) -> String {
        let mut props = String::new();
        if let Some(style) = fmt.style {
            props.push_str(&format!(r#"<w:rStyle w:val="{style}"/>"#));
        }
        if fmt.bold {
            props.push_str("<w:b/>");
        }
        if fmt.italic {
            props.push_str("<w:i/>");
        }
        if fmt.strike {
            props.push_str("<w:strike/>");
        }
        if let Some(color) = fmt.highlight
            && let Some(name) = highlight_name(color)
        {
            props.push_str(&format!(r#"<w:highlight w:val="{name}"/>"#));
        }
        if fmt.underline {
            props.push_str(r#"<w:u w:val="single"/>"#);
        }
        if fmt.highlight == Some(HighlightColor::Orange) {
            props.push_str(r#"<w:shd w:val="clear" w:color="auto" w:fill="FFC000"/>"#);
        }
        if props.is_empty() {
            format!("<w:r>{inner}</w:r>")
        } else {
            format!("<w:r><w:rPr>{props}</w:rPr>{inner}</w:r>")
        }
    }

    fn text_run(&self, fmt: &RunFmt, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        self.run(
            fmt,
            &format!(r#"<w:t xml:space="preserve">{}</w:t>"#, escape_xml(text)),
        )
    }

    fn external_rel(&mut self, url: &str) -> String {
        self.rels.add_unique(rel::HYPERLINK, url, true)
    }

    fn drawing(
        &mut self,
        bytes: Vec<u8>,
        format: ImageFormat,
        size: (u32, u32),
        alt: &str,
    ) -> String {
        let n = self.media.len() + 1;
        let file = format!("image{n}.{}", format.extension());
        let rid = self.rels.add(rel::IMAGE, &format!("media/{file}"), false);
        self.media.push((file.clone(), bytes));
        self.drawing_id += 1;
        let id = self.drawing_id;
        let cx = u64::from(size.0) * EMU_PER_PX;
        let cy = u64::from(size.1) * EMU_PER_PX;
        let alt = escape_xml(alt);
        let name = escape_xml(&file);
        format!(
            r#"<w:r><w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="{cx}" cy="{cy}"/><wp:docPr id="{id}" name="Picture {id}" descr="{alt}"/><wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect="1"/></wp:cNvGraphicFramePr><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:nvPicPr><pic:cNvPr id="{id}" name="{name}" descr="{alt}"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#
        )
    }

    /// Display size in pixels: explicit `|W` / `|WxH`, else the intrinsic size, clamped to the
    /// text width with the aspect ratio kept.
    fn image_size(&self, info: (u32, u32), width: Option<u32>, height: Option<u32>) -> (u32, u32) {
        let (iw, ih) = (info.0.max(1), info.1.max(1));
        let (mut w, mut h) = match (width, height) {
            (Some(w), Some(h)) => (w.max(1), h.max(1)),
            (Some(w), None) => (
                w.max(1),
                (u64::from(w.max(1)) * u64::from(ih) / u64::from(iw)).max(1) as u32,
            ),
            (None, Some(h)) => (
                (u64::from(h.max(1)) * u64::from(iw) / u64::from(ih)).max(1) as u32,
                h.max(1),
            ),
            (None, None) => (iw, ih),
        };
        if w > self.content_width_px {
            h = (u64::from(h) * u64::from(self.content_width_px) / u64::from(w)).max(1) as u32;
            w = self.content_width_px;
        }
        (w, h)
    }

    fn image_run(&mut self, img: &ssw_core::markdown::ast::Image) -> String {
        let alt = unescape_markdown(img.alt.get(self.src).unwrap_or(""));
        let label = if alt.is_empty() {
            img.dest.clone()
        } else {
            alt.clone()
        };
        let Some(bytes) = (self.resources.image)(&img.dest) else {
            return self.text_run(&RunFmt::default(), &label);
        };
        let Some(info) = image_info(&bytes) else {
            return self.text_run(&RunFmt::default(), &label);
        };
        let size = self.image_size((info.width, info.height), img.width, img.height);
        self.drawing(bytes, info.format, size, &alt)
    }

    fn math_run(&mut self, tex: &str, display: bool) -> String {
        match (self.resources.math)(tex, display) {
            Some(image) => match image_info(&image.png) {
                Some(info) => {
                    let size = (image.width_px.max(1), image.height_px.max(1));
                    self.drawing(image.png, info.format, size, tex)
                }
                None => self.code_run(tex),
            },
            None => self.code_run(tex),
        }
    }

    fn code_run(&self, text: &str) -> String {
        self.text_run(
            &RunFmt {
                style: Some("InlineCode"),
                ..RunFmt::default()
            },
            text,
        )
    }

    fn footnote_ref(&mut self, label: &str) -> String {
        let Some(&definition) = self.footnote_defs.get(&normalize_label(label)) else {
            return self.text_run(&RunFmt::default(), &format!("[^{label}]"));
        };
        let id = self.footnotes.len() + 1;
        let mut paragraphs = String::new();
        let mut first = true;
        let children: Vec<&Block> = definition.children.iter().collect();
        for child in children {
            let runs = match child.kind {
                BlockKind::Paragraph => self.inlines(&child.inlines, &RunFmt::default()),
                _ => {
                    let text = block_text(self.src, child);
                    self.text_run(&RunFmt::default(), &text)
                }
            };
            let mark = if first {
                r#"<w:r><w:rPr><w:rStyle w:val="FootnoteReference"/></w:rPr><w:footnoteRef/></w:r>"#
            } else {
                ""
            };
            let lead = if first {
                self.text_run(&RunFmt::default(), " ")
            } else {
                String::new()
            };
            paragraphs.push_str(&format!(
                r#"<w:p><w:pPr><w:pStyle w:val="FootnoteText"/></w:pPr>{mark}{lead}{runs}</w:p>"#
            ));
            first = false;
        }
        if first {
            paragraphs.push_str(
                r#"<w:p><w:pPr><w:pStyle w:val="FootnoteText"/></w:pPr><w:r><w:rPr><w:rStyle w:val="FootnoteReference"/></w:rPr><w:footnoteRef/></w:r></w:p>"#,
            );
        }
        self.footnotes.push(format!(
            r#"<w:footnote w:id="{id}">{paragraphs}</w:footnote>"#
        ));
        format!(
            r#"<w:r><w:rPr><w:rStyle w:val="FootnoteReference"/></w:rPr><w:footnoteReference w:id="{id}"/></w:r>"#
        )
    }

    // ---- inlines -----------------------------------------------------------------------------

    fn inlines(&mut self, inlines: &[Inline], fmt: &RunFmt) -> String {
        let mut out = String::new();
        for inline in inlines {
            out.push_str(&self.inline(inline, fmt));
        }
        out
    }

    fn inline(&mut self, inline: &Inline, fmt: &RunFmt) -> String {
        let src = self.src;
        match &inline.kind {
            InlineKind::Text => self.text_run(fmt, inline.range.get(src).unwrap_or("")),
            InlineKind::Escape => self.text_run(fmt, inline.content.get(src).unwrap_or("")),
            InlineKind::Entity { decoded } => self.text_run(fmt, decoded),
            InlineKind::Emphasis => self.inlines(
                &inline.children,
                &RunFmt {
                    italic: true,
                    ..fmt.clone()
                },
            ),
            InlineKind::Strong => self.inlines(
                &inline.children,
                &RunFmt {
                    bold: true,
                    ..fmt.clone()
                },
            ),
            InlineKind::Underline => self.inlines(
                &inline.children,
                &RunFmt {
                    underline: true,
                    ..fmt.clone()
                },
            ),
            InlineKind::Strikethrough => self.inlines(
                &inline.children,
                &RunFmt {
                    strike: true,
                    ..fmt.clone()
                },
            ),
            InlineKind::Highlight { color } => self.inlines(
                &inline.children,
                &RunFmt {
                    highlight: Some(color.unwrap_or(HighlightColor::Yellow)),
                    ..fmt.clone()
                },
            ),
            InlineKind::Code { text } => self.text_run(
                &RunFmt {
                    style: Some("InlineCode"),
                    ..fmt.clone()
                },
                text,
            ),
            InlineKind::Link(link) => {
                let inner = self.inlines(
                    &inline.children,
                    &RunFmt {
                        style: Some("Hyperlink"),
                        ..fmt.clone()
                    },
                );
                if let Some(anchor) = link.dest.strip_prefix('#') {
                    let name = self
                        .anchors
                        .get(anchor)
                        .cloned()
                        .unwrap_or_else(|| anchor.to_string());
                    format!(
                        r#"<w:hyperlink w:anchor="{}" w:history="1">{inner}</w:hyperlink>"#,
                        escape_xml(&name)
                    )
                } else {
                    let id = self.external_rel(&link.dest);
                    format!(r#"<w:hyperlink r:id="{id}" w:history="1">{inner}</w:hyperlink>"#)
                }
            }
            InlineKind::Image(img) => self.image_run(img),
            InlineKind::WikiLink(_) => self.text_run(fmt, inline.content.get(src).unwrap_or("")),
            InlineKind::FootnoteRef { label } => self.footnote_ref(label),
            InlineKind::InlineMath => {
                let tex = inline.content.get(src).unwrap_or("").to_string();
                self.math_run(&tex, false)
            }
            InlineKind::HardBreak => self.run(fmt, "<w:br/>"),
            InlineKind::SoftBreak => self.text_run(fmt, " "),
            InlineKind::RawHtml => {
                let raw = inline.range.get(src).unwrap_or("");
                let tag = raw.trim_matches(['<', '>', '/', ' ']).to_ascii_lowercase();
                if tag == "br" {
                    self.run(fmt, "<w:br/>")
                } else {
                    self.text_run(fmt, raw)
                }
            }
        }
    }

    // ---- blocks ------------------------------------------------------------------------------

    fn paragraph_xml(&mut self, style: Option<&str>, props: &str, inner: &str) {
        let style = style
            .map(|s| format!(r#"<w:pStyle w:val="{s}"/>"#))
            .unwrap_or_default();
        let ppr = if style.is_empty() && props.is_empty() {
            String::new()
        } else {
            format!("<w:pPr>{style}{props}</w:pPr>")
        };
        self.body.push_str(&format!("<w:p>{ppr}{inner}</w:p>"));
    }

    fn quote_props(ctx: Ctx) -> String {
        if ctx.quote > 1 {
            format!(
                r#"<w:ind w:left="{}" w:right="567"/>"#,
                567 * u32::from(ctx.quote)
            )
        } else {
            String::new()
        }
    }

    fn blocks(&mut self, blocks: &'a [Block], ctx: Ctx) {
        for block in blocks {
            self.block(block, ctx);
        }
    }

    fn block(&mut self, block: &'a Block, ctx: Ctx) {
        match &block.kind {
            BlockKind::Heading { level, .. } => {
                let inner = self.inlines(&block.inlines, &RunFmt::default());
                let inner = match self.heading_bookmarks.get(&block.range.start).cloned() {
                    Some(name) => {
                        let id = self.bookmark_id;
                        self.bookmark_id += 1;
                        format!(
                            r#"<w:bookmarkStart w:id="{id}" w:name="{}"/>{inner}<w:bookmarkEnd w:id="{id}"/>"#,
                            escape_xml(&name)
                        )
                    }
                    None => inner,
                };
                self.paragraph_xml(Some(&format!("Heading{level}")), "", &inner);
            }
            BlockKind::Paragraph => {
                let inner = self.inlines(&block.inlines, &RunFmt::default());
                if ctx.quote > 0 {
                    let props = Self::quote_props(ctx);
                    self.paragraph_xml(Some("Quote"), &props, &inner);
                } else if ctx.in_list {
                    let props = format!(r#"<w:ind w:left="{}"/>"#, 720 * (u32::from(ctx.level) + 1));
                    self.paragraph_xml(Some("ListParagraph"), &props, &inner);
                } else {
                    self.paragraph_xml(None, "", &inner);
                }
            }
            BlockKind::List { .. } => self.list(block, ctx),
            BlockKind::ListItem { .. } => self.blocks(&block.children, ctx),
            BlockKind::BlockQuote => {
                let inner = Ctx { quote: ctx.quote + 1, ..ctx };
                self.blocks(&block.children, inner);
            }
            BlockKind::Callout { kind, title, .. } => {
                let label = match title.and_then(|t| t.get(self.src)) {
                    Some(text) if !text.trim().is_empty() => text.trim().to_string(),
                    _ => callout_label(*kind).to_string(),
                };
                let inner = Ctx { quote: ctx.quote + 1, ..ctx };
                let runs = self.text_run(&RunFmt { bold: true, ..RunFmt::default() }, &label);
                let props = Self::quote_props(inner);
                self.paragraph_xml(Some("Quote"), &props, &runs);
                self.blocks(&block.children, inner);
            }
            BlockKind::CodeBlock(code) => {
                let text = code.text(self.src);
                self.code_lines(text.trim_end_matches('\n'));
            }
            BlockKind::MathBlock(math) => {
                let tex = math.text(self.src);
                let run = self.math_run(&tex, true);
                self.paragraph_xml(None, r#"<w:jc w:val="center"/>"#, &run);
            }
            BlockKind::Table(_) => self.table(block),
            BlockKind::ThematicBreak => self.paragraph_xml(
                None,
                r#"<w:pBdr><w:bottom w:val="single" w:sz="6" w:space="1" w:color="808080"/></w:pBdr>"#,
                "",
            ),
            BlockKind::PageBreak => {
                self.paragraph_xml(None, "", r#"<w:r><w:br w:type="page"/></w:r>"#);
            }
            BlockKind::HtmlBlock(html) => {
                if block.inlines.is_empty() {
                    let text = html.text(self.src);
                    for line in text.lines().filter(|l| !l.trim().is_empty()) {
                        let run = self.text_run(&RunFmt::default(), line);
                        self.paragraph_xml(None, "", &run);
                    }
                } else {
                    let inner = self.inlines(&block.inlines, &RunFmt::default());
                    self.paragraph_xml(None, "", &inner);
                }
            }
            BlockKind::FrontMatter { .. }
            | BlockKind::FootnoteDefinition { .. }
            | BlockKind::LinkReferenceDefinition(_) => {}
        }
    }

    fn code_lines(&mut self, text: &str) {
        for line in text.split('\n') {
            let run = self.text_run(&RunFmt::default(), line);
            self.paragraph_xml(Some("CodeBlock"), "", &run);
        }
    }

    fn list(&mut self, block: &'a Block, ctx: Ctx) {
        let BlockKind::List { ordered, start, .. } = &block.kind else {
            return;
        };
        let level = if ctx.in_list {
            (ctx.level + 1).min(8)
        } else {
            0
        };
        let instance = ordered.then(|| {
            self.ordered.push((*start, level));
            u32::try_from(self.ordered.len() - 1).unwrap_or(u32::MAX)
        });
        for item in &block.children {
            let BlockKind::ListItem { task, .. } = &item.kind else {
                continue;
            };
            let num_id = match (instance, task) {
                (_, Some(t)) if t.checked => 4,
                (_, Some(_)) => 3,
                (Some(i), None) => 100 + i,
                (None, None) => 1,
            };
            let inner = Ctx {
                quote: 0,
                in_list: true,
                level,
            };
            let mut numbered = false;
            for (index, child) in item.children.iter().enumerate() {
                if index == 0 && matches!(child.kind, BlockKind::Paragraph) {
                    let runs = self.inlines(&child.inlines, &RunFmt::default());
                    self.paragraph_xml(Some("ListParagraph"), &num_pr(num_id, level), &runs);
                    numbered = true;
                    continue;
                }
                if !numbered {
                    self.paragraph_xml(Some("ListParagraph"), &num_pr(num_id, level), "");
                    numbered = true;
                }
                self.block(child, inner);
            }
            if !numbered {
                self.paragraph_xml(Some("ListParagraph"), &num_pr(num_id, level), "");
            }
        }
    }

    fn table(&mut self, block: &Block) {
        let BlockKind::Table(table) = &block.kind else {
            return;
        };
        let columns = table.header.cells.len().max(1);
        let (page_width, _) = self.options.page.twips();
        let total = page_width.saturating_sub(self.options.margin_twips.saturating_mul(2));
        let column_width = (total / u32::try_from(columns).unwrap_or(1)).max(1);
        let border = |name: &str| {
            format!(r#"<w:{name} w:val="single" w:sz="4" w:space="0" w:color="7F7F7F"/>"#)
        };
        let mut xml = format!(
            r#"<w:tbl><w:tblPr><w:tblW w:w="{total}" w:type="dxa"/><w:tblBorders>{}{}{}{}{}{}</w:tblBorders><w:tblLayout w:type="fixed"/><w:tblCellMar><w:left w:w="100" w:type="dxa"/><w:right w:w="100" w:type="dxa"/></w:tblCellMar><w:tblLook w:val="0420" w:firstRow="1" w:lastRow="0" w:firstColumn="0" w:lastColumn="0" w:noHBand="0" w:noVBand="1"/></w:tblPr><w:tblGrid>"#,
            border("top"),
            border("left"),
            border("bottom"),
            border("right"),
            border("insideH"),
            border("insideV")
        );
        for _ in 0..columns {
            xml.push_str(&format!(r#"<w:gridCol w:w="{column_width}"/>"#));
        }
        xml.push_str("</w:tblGrid>");
        let rows = std::iter::once(&table.header).chain(table.rows.iter());
        for (row_index, row) in rows.enumerate() {
            xml.push_str("<w:tr><w:trPr><w:cantSplit/>");
            if row_index == 0 {
                xml.push_str("<w:tblHeader/>");
            }
            xml.push_str("</w:trPr>");
            for column in 0..columns {
                let shade = if row_index == 0 {
                    r#"<w:shd w:val="clear" w:color="auto" w:fill="D9E2F3"/>"#
                } else {
                    ""
                };
                let align = match table.alignments.get(column) {
                    Some(Alignment::Center) => "center",
                    Some(Alignment::Right) => "right",
                    _ => "left",
                };
                let style = if row_index == 0 {
                    r#"<w:pStyle w:val="TableHeader"/>"#
                } else {
                    ""
                };
                let runs = match row.cells.get(column) {
                    Some(cell) => self.inlines(&cell.inlines, &RunFmt::default()),
                    None => String::new(),
                };
                xml.push_str(&format!(
                    r#"<w:tc><w:tcPr><w:tcW w:w="{column_width}" w:type="dxa"/>{shade}</w:tcPr><w:p><w:pPr>{style}<w:spacing w:before="40" w:after="40"/><w:jc w:val="{align}"/></w:pPr>{runs}</w:p></w:tc>"#
                ));
            }
            xml.push_str("</w:tr>");
        }
        xml.push_str("</w:tbl>");
        self.body.push_str(&xml);
        self.body.push_str("<w:p/>");
    }
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

fn core_xml(options: &ExportOptions) -> String {
    let mut xml = format!(
        r#"{XML_DECL}<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">"#
    );
    if let Some(title) = &options.title {
        xml.push_str(&format!("<dc:title>{}</dc:title>", escape_xml(title)));
    }
    xml.push_str("<dc:creator>SS Writer</dc:creator>");
    if let Some(created) = &options.created {
        xml.push_str(&format!(
            r#"<dcterms:created xsi:type="dcterms:W3CDTF">{}</dcterms:created>"#,
            escape_xml(created)
        ));
    }
    if let Some(modified) = &options.modified {
        xml.push_str(&format!(
            r#"<dcterms:modified xsi:type="dcterms:W3CDTF">{}</dcterms:modified>"#,
            escape_xml(modified)
        ));
    }
    xml.push_str("</cp:coreProperties>");
    xml
}

/// Converts Markdown to a DOCX package.
pub fn export_docx(
    markdown: &str,
    options: &ExportOptions,
    resources: &mut Resources<'_>,
) -> Result<Vec<u8>, ExportError> {
    let document = parse_document(markdown);
    let mut writer = Writer::new(markdown, options, resources);
    for block in document.iter_blocks() {
        if let BlockKind::FootnoteDefinition { label } = &block.kind {
            writer.footnote_defs.insert(normalize_label(label), block);
        }
    }
    let mut used: HashMap<String, usize> = HashMap::new();
    for block in document.iter_blocks() {
        if matches!(block.kind, BlockKind::Heading { .. }) {
            let mut text = String::new();
            inline_text(markdown, &block.inlines, &mut text);
            let base = slugify(text.trim());
            let count = used.entry(base.clone()).or_insert(0);
            let slug = if *count == 0 {
                base.clone()
            } else {
                format!("{base}-{count}")
            };
            *count += 1;
            let name = format!("_ssw_h{}", writer.heading_bookmarks.len() + 1);
            writer.anchors.insert(slug, name.clone());
            writer.heading_bookmarks.insert(block.range.start, name);
        }
    }
    writer.blocks(&document.blocks, Ctx::default());
    let (page_w, page_h) = options.page.twips();
    let margin = options.margin_twips;
    let document_xml = format!(
        r#"{XML_DECL}<w:document {NS_DECLS}><w:body>{}<w:sectPr><w:pgSz w:w="{page_w}" w:h="{page_h}"/><w:pgMar w:top="{margin}" w:right="{margin}" w:bottom="{margin}" w:left="{margin}" w:header="708" w:footer="708" w:gutter="0"/></w:sectPr></w:body></w:document>"#,
        writer.body
    );
    let separator = r#"<w:footnote w:type="separator" w:id="-1"><w:p><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:r><w:separator/></w:r></w:p></w:footnote><w:footnote w:type="continuationSeparator" w:id="0"><w:p><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:r><w:continuationSeparator/></w:r></w:p></w:footnote>"#;
    let footnotes_xml = format!(
        r#"{XML_DECL}<w:footnotes {NS_DECLS}>{separator}{}</w:footnotes>"#,
        writer.footnotes.concat()
    );
    let mut root_rels = RelationshipsBuilder::new();
    root_rels.add(rel::OFFICE_DOCUMENT, "word/document.xml", false);
    root_rels.add(rel::CORE_PROPERTIES, "docProps/core.xml", false);
    root_rels.add(rel::EXTENDED_PROPERTIES, "docProps/app.xml", false);

    let app_xml = format!(
        r#"{XML_DECL}<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Application>SS Writer</Application></Properties>"#
    );
    // Parts with their own content type, in the order they are stored.
    let xml_parts = [
        (
            "docProps/core.xml",
            "application/vnd.openxmlformats-package.core-properties+xml".to_string(),
            core_xml(options),
        ),
        (
            "docProps/app.xml",
            "application/vnd.openxmlformats-officedocument.extended-properties+xml".to_string(),
            app_xml,
        ),
        (
            "word/document.xml",
            format!("{CT_PREFIX}.document.main+xml"),
            document_xml,
        ),
        (
            "word/styles.xml",
            format!("{CT_PREFIX}.styles+xml"),
            styles_xml(options, NS_DECLS),
        ),
        (
            "word/numbering.xml",
            format!("{CT_PREFIX}.numbering+xml"),
            numbering_xml(&writer.ordered, NS_DECLS),
        ),
        (
            "word/settings.xml",
            format!("{CT_PREFIX}.settings+xml"),
            settings_xml(NS_DECLS),
        ),
        (
            "word/footnotes.xml",
            format!("{CT_PREFIX}.footnotes+xml"),
            footnotes_xml,
        ),
    ];

    let mut types = ContentTypes::new();
    types.default_type(
        "rels",
        "application/vnd.openxmlformats-package.relationships+xml",
    );
    types.default_type("xml", "application/xml");
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Gif] {
        types.default_type(format.extension(), format.mime());
    }
    for (name, content_type, _) in &xml_parts {
        types.override_part(&format!("/{name}"), content_type);
    }

    let mut package = PackageWriter::new();
    package.add("[Content_Types].xml", types.to_xml());
    package.add("_rels/.rels", root_rels.to_xml());
    for (name, _, xml) in xml_parts {
        package.add(name, xml);
    }
    package.add("word/_rels/document.xml.rels", writer.rels.to_xml());
    for (name, bytes) in writer.media {
        package.add(&format!("word/media/{name}"), bytes);
    }
    package.finish().map_err(ExportError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ooxml::{Element, Ns, Package};

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    fn export_with(markdown: &str, options: &ExportOptions) -> Vec<u8> {
        let mut image = |path: &str| -> Option<Vec<u8>> {
            match path {
                "pic.png" => Some(png(20, 10)),
                "huge.png" => Some(png(2000, 1000)),
                "bad.png" => Some(b"garbage".to_vec()),
                _ => None,
            }
        };
        let mut math = |tex: &str, display: bool| -> Option<MathImage> {
            match tex.chars().next() {
                Some('x') => Some(MathImage {
                    png: png(30, 12),
                    width_px: 30,
                    height_px: if display { 20 } else { 12 },
                }),
                Some('z') => Some(MathImage {
                    png: b"not an image".to_vec(),
                    width_px: 1,
                    height_px: 1,
                }),
                _ => None,
            }
        };
        let mut resources = Resources {
            image: &mut image,
            math: &mut math,
        };
        export_docx(markdown, options, &mut resources).unwrap()
    }

    fn export(markdown: &str) -> Vec<u8> {
        export_with(markdown, &ExportOptions::default())
    }

    fn part(bytes: &[u8], name: &str) -> Element {
        Package::open(bytes)
            .unwrap()
            .read_xml(name)
            .unwrap()
            .unwrap()
    }

    fn doc(markdown: &str) -> Element {
        part(&export(markdown), "word/document.xml")
    }

    fn vals<'e>(root: &'e Element, name: &str) -> Vec<&'e str> {
        root.descendants(Ns::W, name)
            .iter()
            .filter_map(|e| e.attr_ns(Ns::W, "val"))
            .collect()
    }

    fn paragraphs(root: &Element) -> Vec<&Element> {
        root.descendants(Ns::W, "p")
    }

    fn style_of(p: &Element) -> Option<&str> {
        p.child(Ns::W, "pPr")?
            .child(Ns::W, "pStyle")?
            .attr_ns(Ns::W, "val")
    }

    #[test]
    fn page_sizes_and_default_options() {
        assert_eq!(PageSize::A4.twips(), (11906, 16838));
        assert_eq!(PageSize::Letter.twips(), (12240, 15840));
        let options = ExportOptions::default();
        assert_eq!(options.page, PageSize::A4);
        assert_eq!((options.margin_twips, options.font_half_points), (1440, 22));
        assert!(options.title.is_none() && !options.black_text);
        let mut image = |_: &str| None;
        let mut math = |_: &str, _: bool| None;
        let resources = Resources {
            image: &mut image,
            math: &mut math,
        };
        assert_eq!(format!("{resources:?}"), "Resources");
    }

    #[test]
    fn empty_document_has_only_a_section() {
        let document = doc("");
        let body = document.child(Ns::W, "body").unwrap();
        let names: Vec<&str> = body.elements().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["sectPr"]);
    }

    #[test]
    fn page_setup_follows_the_options() {
        let options = ExportOptions {
            page: PageSize::Letter,
            margin_twips: 720,
            font_half_points: 24,
            black_text: true,
            ..ExportOptions::default()
        };
        let bytes = export_with("# H\n", &options);
        let document = part(&bytes, "word/document.xml");
        let size = &document.descendants(Ns::W, "pgSz")[0];
        assert_eq!(size.attr_ns(Ns::W, "w"), Some("12240"));
        assert_eq!(size.attr_ns(Ns::W, "h"), Some("15840"));
        let margins = &document.descendants(Ns::W, "pgMar")[0];
        for side in ["top", "right", "bottom", "left"] {
            assert_eq!(margins.attr_ns(Ns::W, side), Some("720"), "{side}");
        }
        let styles = String::from_utf8(
            Package::open(&bytes)
                .unwrap()
                .read_part("word/styles.xml")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(styles.contains(r#"<w:sz w:val="24"/>"#));
        assert!(styles.contains("000000") && !styles.contains("1F3864"));
        let default_styles = String::from_utf8(
            Package::open(&export("# H\n"))
                .unwrap()
                .read_part("word/styles.xml")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(default_styles.contains("1F3864"));
    }

    #[test]
    fn inline_formats_map_to_run_properties() {
        let document = doc("***bi*** ~u~ ~~s~~ `c` [l](https://x.test/)\n");
        let property_names: Vec<Vec<&str>> = document
            .descendants(Ns::W, "rPr")
            .iter()
            .map(|r| r.elements().map(|e| e.name.as_str()).collect())
            .collect();
        assert_eq!(
            property_names,
            [
                vec!["b", "i"],
                vec!["u"],
                vec!["strike"],
                vec!["rStyle"],
                vec!["rStyle"]
            ]
        );
        assert_eq!(vals(&document, "rStyle"), ["InlineCode", "Hyperlink"]);
        assert_eq!(vals(&document, "u"), ["single"]);
    }

    #[test]
    fn highlight_colors_use_the_closest_word_highlight() {
        let document =
            doc("==y== =={red}r== =={green}g== =={blue}b== =={purple}p== =={orange}o==\n");
        assert_eq!(
            vals(&document, "highlight"),
            ["yellow", "red", "green", "cyan", "magenta"]
        );
        let shading = document.descendants(Ns::W, "shd");
        assert_eq!(shading.len(), 1);
        assert_eq!(shading[0].attr_ns(Ns::W, "fill"), Some("FFC000"));
    }

    #[test]
    fn breaks_entities_html_and_wikilinks() {
        let document = doc("a\\\nb<br>c\nd &amp; <span> [[Page]]\n");
        assert_eq!(document.descendants(Ns::W, "br").len(), 2);
        let text = document.text();
        assert!(text.contains("c d & <span> Page"), "{text}");
    }

    #[test]
    fn text_is_escaped_and_invalid_characters_dropped() {
        let document = doc("a < b & c > d \"q\" 'x' \u{1}end\n");
        assert_eq!(document.text(), "a < b & c > d \"q\" 'x' end");
    }

    #[test]
    fn list_numbering_ids_levels_and_tasks() {
        let document =
            doc("- a\n  - b\n- [ ] todo\n- [x] done\n\ntext\n\n3. x\n4. y\n\nmid\n\n1. z\n");
        assert_eq!(
            vals(&document, "numId"),
            ["1", "1", "3", "4", "100", "100", "101"]
        );
        assert_eq!(vals(&document, "ilvl"), ["0", "1", "0", "0", "0", "0", "0"]);
        let numbering = part(&export("3. x\n\nmid\n\n1. z\n"), "word/numbering.xml");
        let ids: Vec<&str> = numbering
            .descendants(Ns::W, "num")
            .iter()
            .filter_map(|n| n.attr_ns(Ns::W, "numId"))
            .collect();
        assert!(
            ids.contains(&"1") && ids.contains(&"100") && ids.contains(&"101"),
            "{ids:?}"
        );
    }

    #[test]
    fn list_items_with_extra_blocks_and_empty_items() {
        let document = doc("- a\n\n  b\n-\n- c\n");
        let list: Vec<(Option<&str>, bool)> = paragraphs(&document)
            .iter()
            .map(|p| (style_of(p), p.descendants(Ns::W, "numPr").len() == 1))
            .collect();
        assert_eq!(
            list,
            [
                (Some("ListParagraph"), true),
                (Some("ListParagraph"), false),
                (Some("ListParagraph"), true),
                (Some("ListParagraph"), true)
            ]
        );
        let indents = vals_attr(&document, "ind", "left");
        assert_eq!(indents, ["720"]);
    }

    fn vals_attr<'e>(root: &'e Element, name: &str, attr: &str) -> Vec<&'e str> {
        root.descendants(Ns::W, name)
            .iter()
            .filter_map(|e| e.attr_ns(Ns::W, attr))
            .collect()
    }

    #[test]
    fn quotes_and_callouts() {
        let document = doc("> a\n>\n> > b\n");
        let styles: Vec<Option<&str>> = paragraphs(&document).iter().map(|p| style_of(p)).collect();
        assert_eq!(styles, [Some("Quote"), Some("Quote")]);
        assert_eq!(vals_attr(&document, "ind", "left"), ["1134"]);
        assert_eq!(vals_attr(&document, "ind", "right"), ["567"]);
        for (keyword, label) in [
            ("NOTE", "Note"),
            ("TIP", "Tip"),
            ("IMPORTANT", "Important"),
            ("WARNING", "Warning"),
            ("CAUTION", "Caution"),
        ] {
            let callout = doc(&format!("> [!{keyword}]\n> body\n"));
            let text = callout.text();
            assert!(
                text.starts_with(label) && text.ends_with("body"),
                "{keyword}: {text}"
            );
            assert_eq!(vals(&callout, "pStyle"), ["Quote", "Quote"]);
        }
        assert!(
            doc("> [!NOTE] My title\n> body\n")
                .text()
                .starts_with("My title")
        );
    }

    #[test]
    fn code_blocks_rules_page_breaks_and_html_blocks() {
        let document =
            doc("```\na\n\nb\n```\n\n---\n\n<!-- pagebreak -->\n\n<div>\nhello\n</div>\n");
        let styles: Vec<Option<&str>> = paragraphs(&document).iter().map(|p| style_of(p)).collect();
        assert_eq!(
            styles.iter().filter(|s| **s == Some("CodeBlock")).count(),
            3
        );
        assert_eq!(document.descendants(Ns::W, "pBdr").len(), 1);
        let page_breaks = document
            .descendants(Ns::W, "br")
            .iter()
            .filter(|b| b.attr_ns(Ns::W, "type") == Some("page"))
            .count();
        assert_eq!(page_breaks, 1);
        let text = document.text();
        assert!(text.contains("<div>") && text.contains("hello") && text.contains("</div>"));
        assert!(!doc("---\ntitle: x\n---\n\ntext\n").text().contains("title"));
    }

    #[test]
    fn math_uses_rendered_images_or_falls_back_to_code() {
        let document = doc("$x^2$ $y$ $z$\n\n$$\nx\n$$\n\n$$\ny\n$$\n");
        let extents: Vec<(&str, &str)> = document
            .descendants(Ns::Wp, "extent")
            .iter()
            .map(|e| (e.attr("cx").unwrap(), e.attr("cy").unwrap()))
            .collect();
        assert_eq!(extents, [("285750", "114300"), ("285750", "190500")]);
        let alt: Vec<&str> = document
            .descendants(Ns::Wp, "docPr")
            .iter()
            .filter_map(|e| e.attr("descr"))
            .collect();
        assert_eq!(alt, ["x^2", "x"]);
        assert_eq!(
            vals(&document, "rStyle"),
            ["InlineCode", "InlineCode", "InlineCode"]
        );
        assert_eq!(vals(&document, "jc"), ["center", "center"]);
    }

    #[test]
    fn image_sizes_alt_text_and_fallbacks() {
        let document = doc(
            "![a](pic.png) ![b|50](pic.png) ![c|20x40](pic.png) ![d](huge.png) ![e](bad.png) ![](nope.png) ![f](nope.png)\n",
        );
        let extents: Vec<(&str, &str)> = document
            .descendants(Ns::Wp, "extent")
            .iter()
            .map(|e| (e.attr("cx").unwrap(), e.attr("cy").unwrap()))
            .collect();
        assert_eq!(
            extents,
            [
                ("190500", "95250"),
                ("476250", "238125"),
                ("190500", "381000"),
                ("5724525", "2857500")
            ]
        );
        let ids: Vec<&str> = document
            .descendants(Ns::Wp, "docPr")
            .iter()
            .map(|e| e.attr("id").unwrap())
            .collect();
        assert_eq!(ids, ["1", "2", "3", "4"]);
        let text = document.text();
        assert!(text.contains("e nope.png f"), "{text}");
        let bytes = export("![a](pic.png) ![b](pic.png)\n");
        let rels = part(&bytes, "word/_rels/document.xml.rels");
        let images: Vec<&str> = rels
            .elements()
            .filter(|r| r.attr("Type").is_some_and(|t| t.ends_with("/image")))
            .filter_map(|r| r.attr("Target"))
            .collect();
        assert_eq!(images, ["media/image1.png", "media/image2.png"]);
    }

    #[test]
    fn links_share_relationships_and_resolve_heading_anchors() {
        let bytes = export(
            "# A\n\n# A\n\n[x](https://x.test/) [y](https://x.test/) [h](#a-1) [n](#nope)\n",
        );
        let document = part(&bytes, "word/document.xml");
        let links = document.descendants(Ns::W, "hyperlink");
        assert_eq!(links[0].attr_ns(Ns::R, "id"), links[1].attr_ns(Ns::R, "id"));
        assert_eq!(links[2].attr_ns(Ns::W, "anchor"), Some("_ssw_h2"));
        assert_eq!(links[3].attr_ns(Ns::W, "anchor"), Some("nope"));
        let names: Vec<&str> = document
            .descendants(Ns::W, "bookmarkStart")
            .iter()
            .filter_map(|b| b.attr_ns(Ns::W, "name"))
            .collect();
        assert_eq!(names, ["_ssw_h1", "_ssw_h2"]);
        let rels = part(&bytes, "word/_rels/document.xml.rels");
        let external = rels
            .elements()
            .filter(|r| r.attr("TargetMode") == Some("External"))
            .count();
        assert_eq!(external, 1);
    }

    #[test]
    fn tables_pad_short_rows_and_style_the_header() {
        let document = doc("| a | b |\n|:-:|--|\n| 1 |\n");
        let rows = document.descendants(Ns::W, "tr");
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.elements().filter(|e| e.name == "tc").count(), 2);
        }
        assert_eq!(vals(&document, "pStyle"), ["TableHeader", "TableHeader"]);
        let widths: Vec<&str> = vals_attr(&document, "gridCol", "w");
        assert_eq!(widths, ["4513", "4513"]);
        assert_eq!(vals(rows[0], "jc"), ["center", "left"]);
        assert_eq!(rows[0].descendants(Ns::W, "shd").len(), 2);
        assert!(rows[1].descendants(Ns::W, "shd").is_empty());
        assert_eq!(rows[1].descendants(Ns::W, "t").len(), 1);
        let body = document.child(Ns::W, "body").unwrap();
        let names: Vec<&str> = body.elements().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["tbl", "p", "sectPr"]);
    }

    #[test]
    fn footnotes_keep_multiple_paragraphs() {
        let bytes = export("x[^1]\n\n[^1]: first\n\n    second\n");
        let notes = part(&bytes, "word/footnotes.xml");
        let real: Vec<&Element> = notes
            .elements()
            .filter(|n| n.attr_ns(Ns::W, "type").is_none())
            .collect();
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].descendants(Ns::W, "p").len(), 2);
        assert_eq!(real[0].descendants(Ns::W, "footnoteRef").len(), 1);
        assert!(real[0].text().contains("first") && real[0].text().contains("second"));
        assert_eq!(vals(real[0], "pStyle"), ["FootnoteText", "FootnoteText"]);
    }

    #[test]
    fn package_failures_are_reported_as_export_errors() {
        let error = ExportError::from(PackageError::Io("disk gone".to_string()));
        assert_eq!(error.to_string(), "package I/O failure: disk gone");
        assert_eq!(
            error,
            ExportError::Package(PackageError::Io("disk gone".to_string()))
        );
    }

    #[test]
    fn package_parts_are_declared_in_content_types() {
        let bytes = export("![a](pic.png)\n");
        let types = part(&bytes, "[Content_Types].xml");
        let overrides: Vec<&str> = types
            .elements()
            .filter(|e| e.name == "Override")
            .filter_map(|e| e.attr("PartName"))
            .collect();
        for name in [
            "/docProps/core.xml",
            "/docProps/app.xml",
            "/word/document.xml",
            "/word/styles.xml",
            "/word/numbering.xml",
            "/word/settings.xml",
            "/word/footnotes.xml",
        ] {
            assert!(overrides.contains(&name), "{name}");
        }
        let defaults: Vec<&str> = types
            .elements()
            .filter(|e| e.name == "Default")
            .filter_map(|e| e.attr("Extension"))
            .collect();
        assert_eq!(defaults, ["rels", "xml", "png", "jpeg", "gif"]);
        let root = part(&bytes, "_rels/.rels");
        let targets: Vec<&str> = root.elements().filter_map(|r| r.attr("Target")).collect();
        assert_eq!(
            targets,
            ["word/document.xml", "docProps/core.xml", "docProps/app.xml"]
        );
    }
}
