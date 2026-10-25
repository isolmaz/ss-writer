//! ODT → Markdown (`ImportResult`).
//!
//! `content.xml` is walked once. Block elements become [`Block`]s, inline content becomes
//! [`Inline`]s carrying the formatting of the style chain (bold, italic, underline, strike,
//! highlight, monospace); the Markdown text itself is produced by the emitter shared with the
//! `.doc` importer. Whatever Markdown cannot express is counted in the loss report.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::assets::picture_extension;
use crate::doc::md::{self, Align, Block, Ctx, Fmt, Inline};
use crate::doc::sniff;
use crate::import::{Asset, ImportError, ImportResult, LossKind, MAX_CELLS, MAX_COLUMNS};

use super::package::{Package, PackageError};
use super::slug::unique_slug;
use super::styles::{ListKind, ParaInfo, ParaKind, Styles, TextProps};
use super::xml::{self, Element, Node};

const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
/// Largest `table:number-*-repeated` count that is expanded.
const MAX_REPEAT: usize = 64;
/// Largest number of spaces a single `text:s` stands for.
const MAX_SPACES: usize = 1000;
const UNSUPPORTED_IMAGES: [&str; 5] = ["emf", "wmf", "svm", "pict", "eps"];
const PAGE_FIELDS: [&str; 3] = [
    "text:page-number",
    "text:page-count",
    "text:page-continuation",
];
const INDEXES: [&str; 7] = [
    "text:table-of-content",
    "text:illustration-index",
    "text:table-index",
    "text:object-index",
    "text:user-index",
    "text:alphabetical-index",
    "text:bibliography",
];

/// Imports an ODT file. `stem` names the extracted-asset folder (`<stem>_assets/image-N.ext`).
pub fn import_odt(bytes: &[u8], stem: &str) -> Result<ImportResult, ImportError> {
    if !bytes.starts_with(b"PK") {
        return Err(wrong_format(bytes));
    }
    import_package(&mut Package::open(bytes)?, stem)
}

fn import_package(package: &mut Package<'_>, stem: &str) -> Result<ImportResult, ImportError> {
    check_kind(package)?;
    if let Some(manifest) = package.read("META-INF/manifest.xml")?
        && contains_bytes(&manifest, b"encryption-data")
    {
        return Err(ImportError::Encrypted);
    }
    let content = package
        .read("content.xml")?
        .ok_or_else(|| ImportError::Corrupt("the package has no content.xml".into()))?;
    let content =
        xml::parse(&content).map_err(|e| ImportError::Corrupt(format!("content.xml: {e}")))?;
    if content.name != "office:document-content" {
        return Err(ImportError::Corrupt(
            "content.xml is not an OpenDocument content part".into(),
        ));
    }
    let Some(body) = content.child("office:body") else {
        return Err(ImportError::Corrupt(
            "content.xml has no document body".into(),
        ));
    };
    let Some(text) = body.child("office:text") else {
        return Err(match body.elements().next().map(|e| e.name.as_str()) {
            Some("office:spreadsheet") => wrong_zip("ODS"),
            Some("office:presentation") => wrong_zip("ODP"),
            Some("office:drawing") => wrong_zip("ODG"),
            _ => ImportError::Corrupt("content.xml has no text document body".into()),
        });
    };
    let mut corrupt = 0;
    let styles_root = match package.read("styles.xml") {
        Ok(Some(part)) => match xml::parse(&part) {
            Ok(root) => Some(root),
            Err(_) => {
                corrupt += 1;
                None
            }
        },
        Ok(None) => None,
        Err(PackageError::Corrupt(_)) => {
            corrupt += 1;
            None
        }
        Err(e @ PackageError::TooLarge(_)) => return Err(e.into()),
    };
    let styles = Styles::parse(&content, styles_root.as_ref());
    let mut converter = Converter::new(&styles, package, stem);
    converter
        .res
        .record_loss(LossKind::CorruptParts, corrupt, None);
    converter
        .res
        .record_loss(LossKind::HeadersFooters, styles.header_footers, None);
    converter
        .res
        .record_loss(LossKind::Columns, styles.column_pages, None);
    Ok(converter.run(text))
}

fn wrong_zip(detected: &'static str) -> ImportError {
    ImportError::WrongFormat {
        expected: "ODT",
        detected,
    }
}

/// The error for data that is not a ZIP package at all.
fn wrong_format(bytes: &[u8]) -> ImportError {
    let detected = if bytes.starts_with(&OLE_MAGIC) {
        "DOC"
    } else {
        sniff::sniff(bytes).unwrap_or("unknown")
    };
    wrong_zip(detected)
}

/// Refuses packages that are another kind of document.
fn check_kind(package: &mut Package<'_>) -> Result<(), ImportError> {
    let mime = package
        .read("mimetype")?
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .unwrap_or_default();
    if mime.starts_with("application/vnd.oasis.opendocument.text") {
        return Ok(());
    }
    if let Some(kind) = mime.strip_prefix("application/vnd.oasis.opendocument.") {
        return Err(wrong_zip(match kind.split('-').next().unwrap_or(kind) {
            "spreadsheet" => "ODS",
            "presentation" => "ODP",
            "graphics" => "ODG",
            _ => "OpenDocument",
        }));
    }
    if mime == "application/epub+zip" {
        return Err(wrong_zip("EPUB"));
    }
    if package.contains("content.xml") {
        return Ok(());
    }
    for (part, detected) in [
        ("word/document.xml", "DOCX"),
        ("xl/workbook.xml", "XLSX"),
        ("ppt/presentation.xml", "PPTX"),
    ] {
        if package.contains(part) {
            return Err(wrong_zip(detected));
        }
    }
    Err(ImportError::Corrupt(
        "the package has no content.xml".into(),
    ))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn repeat_count(attr: Option<&str>) -> usize {
    attr.and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_REPEAT)
}

fn span_count(attr: Option<&str>) -> usize {
    attr.and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
}

/// `7.726cm`, `3in`, `100pt`, `50px` → CSS pixels at 96 dpi.
fn length_to_px(value: &str) -> Option<u32> {
    let value = value.trim();
    let split = value.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))?;
    let (number, unit) = value.split_at(split);
    let number: f64 = number.parse().ok()?;
    let per_unit = match unit.trim() {
        "in" => 96.0,
        "cm" => 96.0 / 2.54,
        "mm" => 96.0 / 25.4,
        "pt" => 96.0 / 72.0,
        "pc" => 16.0,
        "px" => 1.0,
        _ => return None,
    };
    let px = (number * per_unit).round();
    (1.0..=20_000.0).contains(&px).then_some(px as u32)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|&b| hex(b)),
                bytes.get(i + 2).and_then(|&b| hex(b)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// File extension for the well-known image signatures.
fn sniff_extension(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if b.starts_with(&[0xFF, 0xD8]) {
        Some("jpg")
    } else if b.starts_with(b"GIF8") {
        Some("gif")
    } else if b.starts_with(b"BM") {
        Some("bmp")
    } else if b.starts_with(b"II*\0") || b.starts_with(b"MM\0*") {
        Some("tif")
    } else if b.starts_with(b"RIFF") && b.get(8..12) == Some(&b"WEBP"[..]) {
        Some("webp")
    } else if b.starts_with(&[0xD7, 0xCD, 0xC6, 0x9A]) {
        Some("wmf")
    } else if b.get(40..44) == Some(&b" EMF"[..]) {
        Some("emf")
    } else if String::from_utf8_lossy(&b[..b.len().min(512)])
        .to_ascii_lowercase()
        .contains("<svg")
    {
        Some("svg")
    } else {
        None
    }
}

/// Extension of an extracted image: what the bytes are, else what the name says.
fn image_extension(href: &str, bytes: &[u8]) -> String {
    if let Some(sniffed) = sniff_extension(bytes) {
        return sniffed.to_string();
    }
    let named = href
        .rsplit('/')
        .next()
        .and_then(|file| file.rsplit_once('.'))
        .map_or(String::new(), |(_, ext)| ext.to_ascii_lowercase());
    match picture_extension(&named) {
        "jpeg" => "jpg",
        known => known,
    }
    .to_string()
}

/// True for image destinations that are not parts of the package.
fn is_external(href: &str) -> bool {
    href.contains(':') || href.starts_with("../") || href.starts_with('/')
}

/// Text of an element the way a reader sees it: notes and comments excluded, whitespace
/// normalized.
fn plain_text(el: &Element) -> String {
    fn collect(el: &Element, out: &mut String) {
        for node in &el.children {
            match node {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) => match e.name.as_str() {
                    "text:note" | "office:annotation" | "text:ruby-text" => {}
                    "text:s" | "text:tab" | "text:line-break" => out.push(' '),
                    _ => collect(e, out),
                },
            }
        }
    }
    let mut raw = String::new();
    collect(el, &mut raw);
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn push_break(out: &mut Vec<Block>) {
    if !matches!(out.last(), Some(Block::PageBreak)) {
        out.push(Block::PageBreak);
    }
}

/// The verbatim text of a code paragraph.
fn code_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(t, _) | Inline::Raw(t) => out.push_str(t),
            Inline::Break => out.push('\n'),
            Inline::Link { kids, .. } => out.push_str(&code_text(kids)),
        }
    }
    out.trim_end().to_string()
}

/// A trailing `#` would be read as the closing sequence of the ATX heading.
fn escape_heading(text: String) -> String {
    match text.strip_suffix('#') {
        Some(rest) => format!("{rest}\\#"),
        None => text,
    }
}

/// The cells of a table with the alignment of each cell's text.
#[derive(Debug, Default)]
struct Rows {
    cells: Vec<Vec<String>>,
    align: Vec<Vec<Align>>,
}

fn flatten_table(rows: &[Vec<String>]) -> String {
    rows.iter()
        .flatten()
        .filter(|c| !c.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

/// One line of a table cell for a block of the cell.
fn cell_line(block: &Block) -> Option<String> {
    match block {
        Block::Para(t) | Block::Heading(_, t) | Block::Code(t) | Block::Quote(t) => Some(t.clone()),
        Block::Item { marker, text, .. } => {
            let joined = text
                .split('\n')
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("<br>");
            Some(format!("{marker} {joined}"))
        }
        _ => None,
    }
}

fn alt_text(frame: &Element) -> String {
    ["svg:desc", "svg:title"]
        .into_iter()
        .filter_map(|name| frame.child(name))
        .map(plain_text)
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}

/// Whether two lists that follow each other are meant to be separate lists in Markdown.
/// Numbered lists always look alike; bullet and task lists only when they are the same kind,
/// because a task list whose boxes differ is written as one list per box state.
fn same_family(a: ListKind, b: ListKind) -> bool {
    match (a, b) {
        (ListKind::Number { .. }, ListKind::Number { .. }) => true,
        _ => a == b,
    }
}

/// What a paragraph turns into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Plain,
    Heading(u8),
    Quote,
    Code,
    Rule,
}

/// The formatting a span's style adds to its context.
fn apply(fmt: Fmt, props: &TextProps, in_link: bool) -> Fmt {
    Fmt {
        bold: props.bold.unwrap_or(fmt.bold),
        italic: props.italic.unwrap_or(fmt.italic),
        strike: props.strike.unwrap_or(fmt.strike),
        // the underline of a link is link chrome, not user formatting
        underline: props.underline.map_or(fmt.underline, |u| u && !in_link),
        highlight: props.highlight.unwrap_or(fmt.highlight),
        code: props.mono.unwrap_or(fmt.code),
    }
}

/// Converter state that a nested block context (cell, note, text box) starts afresh.
struct Saved {
    ctx: Ctx,
    code: bool,
    header_row: bool,
    list_depth: usize,
    list_style: Option<String>,
    pending: Vec<Block>,
    last_space: bool,
}

struct Converter<'a, 'p> {
    styles: &'a Styles,
    package: &'a mut Package<'p>,
    stem: &'a str,
    res: ImportResult,
    /// How inline text is rendered: body, heading or table cell.
    ctx: Ctx,
    /// Inside a code paragraph: tabs stay tabs.
    code: bool,
    /// The cells of the table's first row are header cells.
    header_row: bool,
    /// The last character written was white space (ODF collapses runs of white space).
    last_space: bool,
    /// Something was written already: a page style on the first block is not a break.
    seen_content: bool,
    list_depth: usize,
    list_style: Option<String>,
    /// (list style, level) → number of the last item.
    counters: HashMap<(String, usize), u32>,
    /// Alternate bullet/number delimiters so that adjacent lists stay separate lists.
    parity: bool,
    /// The kind of the last top-level list.
    prev_kind: Option<ListKind>,
    /// Blocks found inside text boxes and shapes; they follow the current paragraph.
    pending: Vec<Block>,
    /// Package path (or `#binary-N`) → (asset path, unsupported format).
    images: HashMap<String, (String, bool)>,
    image_count: usize,
    note_count: usize,
    notes: Vec<String>,
    /// Bookmark names and heading texts → heading slug.
    anchors: HashMap<String, String>,
    cells_left: usize,
    truncated: bool,
    /// Alignment of the first paragraph with text in the table cell being read.
    cell_align: Option<Align>,
}

impl<'a, 'p> Converter<'a, 'p> {
    fn new(styles: &'a Styles, package: &'a mut Package<'p>, stem: &'a str) -> Self {
        Converter {
            styles,
            package,
            stem,
            res: ImportResult::default(),
            ctx: Ctx::Body,
            code: false,
            header_row: false,
            last_space: true,
            seen_content: false,
            list_depth: 0,
            list_style: None,
            counters: HashMap::new(),
            parity: false,
            prev_kind: None,
            pending: Vec::new(),
            images: HashMap::new(),
            image_count: 0,
            note_count: 0,
            notes: Vec::new(),
            anchors: HashMap::new(),
            cells_left: MAX_CELLS,
            truncated: false,
            cell_align: None,
        }
    }

    fn run(mut self, text: &Element) -> ImportResult {
        let mut used = HashMap::new();
        self.collect_anchors(text, &mut used);
        let mut blocks = Vec::new();
        self.blocks(text, &mut blocks);
        blocks.append(&mut self.pending);
        let lead = blocks
            .iter()
            .take_while(|b| matches!(b, Block::PageBreak))
            .count();
        blocks.drain(..lead);
        while matches!(blocks.last(), Some(Block::PageBreak)) {
            blocks.pop();
        }
        // The footnote definitions follow the body after a blank line.
        let body = md::blocks_to_string(&blocks);
        let notes = self.notes.concat();
        let mut markdown = [body.as_str(), notes.trim_end()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !markdown.is_empty() {
            markdown.push('\n');
        }
        self.res
            .record_loss(LossKind::CorruptParts, usize::from(self.truncated), None);
        self.res.markdown = markdown;
        self.res
    }

    // ---- anchors -----------------------------------------------------------------------------

    /// Maps bookmark names inside headings and heading texts to the heading's anchor.
    fn collect_anchors(&mut self, el: &Element, used: &mut HashMap<String, usize>) {
        for child in el.elements() {
            match child.name.as_str() {
                "text:tracked-changes" | "table:table" | "draw:frame" | "draw:g" | "text:note" => {}
                name => {
                    let heading = name == "text:h"
                        || (name == "text:p"
                            && self
                                .styles
                                .paragraph(child.attr("text:style-name"))
                                .heading
                                .is_some());
                    if heading {
                        let title = plain_text(child);
                        let slug = unique_slug(&title, used);
                        if !title.is_empty() {
                            self.anchors.entry(title).or_insert_with(|| slug.clone());
                        }
                        Self::bookmarks(child, &slug, &mut self.anchors);
                    } else {
                        self.collect_anchors(child, used);
                    }
                }
            }
        }
    }

    fn bookmarks(el: &Element, slug: &str, anchors: &mut HashMap<String, String>) {
        for child in el.elements() {
            if matches!(child.name.as_str(), "text:bookmark" | "text:bookmark-start")
                && let Some(name) = child.attr("text:name")
            {
                anchors
                    .entry(name.to_string())
                    .or_insert_with(|| slug.to_string());
            }
            Self::bookmarks(child, slug, anchors);
        }
    }

    fn resolve_anchor(&self, name: &str) -> Option<String> {
        let decoded = percent_decode(name);
        let key = decoded
            .rsplit_once('|')
            .map_or(decoded.as_str(), |(n, _)| n);
        let key = key.split_whitespace().collect::<Vec<_>>().join(" ");
        self.anchors.get(&key).map(|slug| format!("#{slug}"))
    }

    // ---- nested contexts ---------------------------------------------------------------------

    fn enter(&mut self, ctx: Ctx) -> Saved {
        Saved {
            ctx: std::mem::replace(&mut self.ctx, ctx),
            code: std::mem::replace(&mut self.code, false),
            header_row: std::mem::replace(&mut self.header_row, false),
            list_depth: std::mem::replace(&mut self.list_depth, 0),
            list_style: self.list_style.take(),
            pending: std::mem::take(&mut self.pending),
            last_space: self.last_space,
        }
    }

    fn leave(&mut self, saved: Saved) {
        self.ctx = saved.ctx;
        self.code = saved.code;
        self.header_row = saved.header_row;
        self.list_depth = saved.list_depth;
        self.list_style = saved.list_style;
        self.pending = saved.pending;
        self.last_space = saved.last_space;
    }

    /// The blocks of `el` converted in a fresh context.
    fn nested_blocks(&mut self, el: &Element, ctx: Ctx, header: bool) -> Vec<Block> {
        let saved = self.enter(ctx);
        self.header_row = header;
        let mut blocks = Vec::new();
        self.blocks(el, &mut blocks);
        blocks.append(&mut self.pending);
        self.leave(saved);
        blocks
    }

    // ---- blocks ------------------------------------------------------------------------------

    fn blocks(&mut self, parent: &Element, out: &mut Vec<Block>) {
        for child in parent.elements() {
            match child.name.as_str() {
                "text:p" | "text:h" => self.paragraph(child, out),
                "text:list" => self.list(child, out),
                "table:table" => self.table(child, out),
                "text:section" => self.section(child, out),
                "text:tracked-changes" => {
                    let regions = child
                        .elements()
                        .filter(|e| e.name == "text:changed-region")
                        .count();
                    self.res
                        .record_loss(LossKind::TrackedChanges, regions, None);
                }
                name if INDEXES.contains(&name) => self.res.record_loss(LossKind::Fields, 1, None),
                name if name.starts_with("draw:") => self.floating(child, out),
                _ => {}
            }
        }
    }

    fn section(&mut self, section: &Element, out: &mut Vec<Block>) {
        if section.attr("text:display") == Some("none") {
            self.res.record_loss(LossKind::HiddenText, 1, None);
            return;
        }
        if let Some(style) = section.attr("text:style-name")
            && self.styles.section_columns(style) > 1
        {
            self.res.record_loss(LossKind::Columns, 1, None);
        }
        self.blocks(section, out);
    }

    /// A drawing object anchored to the page: its content becomes a paragraph.
    fn floating(&mut self, el: &Element, out: &mut Vec<Block>) {
        let mut inlines = Vec::new();
        self.last_space = true;
        self.inline_element(el, Fmt::default(), false, &mut inlines);
        let text = md::render_inlines(&inlines, self.ctx);
        if !text.is_empty() {
            out.push(Block::Para(text));
        }
        out.append(&mut self.pending);
    }

    fn shape(&self, el: &Element, info: &ParaInfo) -> Shape {
        if self.ctx == Ctx::Cell {
            return Shape::Plain;
        }
        if el.name == "text:h" {
            let level = el
                .attr("text:outline-level")
                .and_then(|l| l.trim().parse::<u32>().ok())
                .filter(|&l| l >= 1)
                .or(info.heading.map(u32::from))
                .unwrap_or(1);
            return Shape::Heading(u8::try_from(level.min(6)).unwrap_or(6));
        }
        match (info.heading, info.kind) {
            (Some(level), _) => Shape::Heading(level),
            (None, ParaKind::Code) => Shape::Code,
            (None, ParaKind::Quote) => Shape::Quote,
            (None, ParaKind::Rule) => Shape::Rule,
            (None, ParaKind::Normal) => Shape::Plain,
        }
    }

    fn base_format(&self, props: &TextProps, shape: Shape) -> Fmt {
        Fmt {
            // a heading is bold anyway, and so is the header row of a table
            bold: props.bold == Some(true)
                && !matches!(shape, Shape::Heading(_))
                && !self.header_row,
            italic: props.italic == Some(true) && shape != Shape::Quote,
            strike: props.strike == Some(true),
            underline: props.underline == Some(true),
            highlight: props.highlight.flatten(),
            code: props.mono == Some(true),
        }
    }

    /// Counts the formatting of `props` that Markdown cannot carry, when `el` has text.
    fn note_text_losses(&mut self, props: &TextProps, el: &Element) {
        if (props.colored == Some(true) || props.sub_super == Some(true))
            && !el.text().trim().is_empty()
        {
            self.res.record_loss(
                LossKind::FontsAndColors,
                usize::from(props.colored == Some(true)),
                None,
            );
            self.res.record_loss(
                LossKind::SubSuperscript,
                usize::from(props.sub_super == Some(true)),
                None,
            );
        }
    }

    fn paragraph(&mut self, el: &Element, out: &mut Vec<Block>) {
        let info = self.styles.paragraph(el.attr("text:style-name"));
        let first = !std::mem::replace(&mut self.seen_content, true);
        let new_page_style = info.master_page && !first;
        self.res
            .record_loss(LossKind::SectionBreaks, usize::from(new_page_style), None);
        if info.break_before || new_page_style {
            push_break(out);
        }
        if info.text.hidden == Some(true) {
            let hidden = !el.text().trim().is_empty();
            self.res
                .record_loss(LossKind::HiddenText, usize::from(hidden), None);
        } else {
            self.paragraph_content(el, &info, out);
        }
        out.append(&mut self.pending);
        if info.break_after {
            push_break(out);
        }
    }

    fn paragraph_content(&mut self, el: &Element, info: &ParaInfo, out: &mut Vec<Block>) {
        let shape = self.shape(el, info);
        if shape == Shape::Plain {
            // the colors of heading, quote and code styles are chrome
            self.note_text_losses(&info.text, el);
        }
        let column_aligned = self.ctx == Ctx::Cell && info.halign != Align::None;
        if info.aligned && !column_aligned && !el.text().trim().is_empty() {
            self.res.record_loss(LossKind::Alignment, 1, None);
        }
        if shape == Shape::Rule {
            out.push(Block::Rule);
            return;
        }
        self.last_space = true;
        self.code = shape == Shape::Code;
        let fmt = self.base_format(&info.text, shape);
        let mut inlines = Vec::new();
        self.inline_children(el, fmt, false, &mut inlines);
        self.code = false;
        match shape {
            Shape::Code => match out.last_mut() {
                Some(Block::Code(code)) => {
                    code.push('\n');
                    code.push_str(&code_text(&inlines));
                }
                _ => {
                    let line = code_text(&inlines);
                    if !line.is_empty() {
                        out.push(Block::Code(line));
                    }
                }
            },
            Shape::Heading(level) => {
                let text = md::render_inlines(&inlines, Ctx::Heading);
                if !text.is_empty() {
                    out.push(Block::Heading(level, escape_heading(text)));
                }
            }
            Shape::Quote => {
                let text = md::render_inlines(&inlines, Ctx::Body);
                if !text.is_empty() {
                    match out.last_mut() {
                        Some(Block::Quote(quote)) => {
                            quote.push_str("\n\n");
                            quote.push_str(&text);
                        }
                        _ => out.push(Block::Quote(text)),
                    }
                }
            }
            Shape::Plain | Shape::Rule => {
                let text = md::render_inlines(&inlines, self.ctx);
                if !text.is_empty() {
                    self.cell_align.get_or_insert(info.halign);
                    out.push(Block::Para(text));
                }
            }
        }
    }

    // ---- lists -------------------------------------------------------------------------------

    fn list(&mut self, list: &Element, out: &mut Vec<Block>) {
        self.seen_content = true;
        let level = self.list_depth;
        let outer = self.list_style.clone();
        let style = list
            .attr("text:style-name")
            .map(str::to_string)
            .or_else(|| outer.clone());
        let kind = self.styles.list_level(style.as_deref(), level + 1);
        if level == 0 {
            let adjacent = matches!(out.last(), Some(Block::Item { .. }));
            self.parity = adjacent
                && self.prev_kind.is_some_and(|prev| same_family(prev, kind))
                && !self.parity;
            self.prev_kind = Some(kind);
        }
        let key = (style.clone().unwrap_or_default(), level);
        if let ListKind::Number { decimal, start } = kind {
            self.res
                .record_loss(LossKind::ListNumberingStyle, usize::from(!decimal), None);
            let resume = list.attr("text:continue-numbering") == Some("true")
                || list.attr("text:continue-list").is_some();
            if !(resume && self.counters.contains_key(&key)) {
                self.counters.insert(key.clone(), start.saturating_sub(1));
            }
        }
        self.list_depth = level + 1;
        self.list_style = style;
        for item in list
            .elements()
            .filter(|e| matches!(e.name.as_str(), "text:list-item" | "text:list-header"))
        {
            self.list_item(item, level, kind, &key, out);
        }
        self.list_depth = level;
        self.list_style = outer;
    }

    fn marker(&self, kind: ListKind, number: Option<u32>, level: usize) -> (String, &'static str) {
        let alternate = level == 0 && self.parity;
        let bullet = if alternate { "*" } else { "-" };
        match (kind, number) {
            (ListKind::Number { .. }, Some(n)) => {
                (format!("{n}{}", if alternate { ')' } else { '.' }), "")
            }
            (ListKind::Task(checked), _) => {
                (bullet.to_string(), if checked { "[x] " } else { "[ ] " })
            }
            _ => (bullet.to_string(), ""),
        }
    }

    fn list_item(
        &mut self,
        item: &Element,
        level: usize,
        kind: ListKind,
        key: &(String, usize),
        out: &mut Vec<Block>,
    ) {
        let number = match kind {
            ListKind::Number { .. } if item.name == "text:list-item" => {
                let n = item
                    .attr("text:start-value")
                    .and_then(|v| v.trim().parse::<u32>().ok())
                    .unwrap_or_else(|| {
                        self.counters
                            .get(key)
                            .copied()
                            .unwrap_or(0)
                            .saturating_add(1)
                    });
                self.counters.insert(key.clone(), n);
                Some(n)
            }
            _ => None,
        };
        let mut blocks = Vec::new();
        self.blocks(item, &mut blocks);
        let split = blocks
            .iter()
            .position(|b| matches!(b, Block::Item { .. }))
            .unwrap_or(blocks.len());
        let nested = blocks.split_off(split);
        let mut own = blocks;
        let lead = matches!(own.first(), Some(Block::PageBreak));
        let trail = own.len() > usize::from(lead) && matches!(own.last(), Some(Block::PageBreak));
        own.retain(|b| !matches!(b, Block::PageBreak));
        if lead {
            push_break(out);
        }
        let text = md::blocks_to_string(&own);
        if !text.is_empty() {
            let (marker, prefix) = self.marker(kind, number, level);
            out.push(Block::Item {
                level: u8::try_from(level).unwrap_or(u8::MAX),
                marker,
                text: format!("{prefix}{text}"),
            });
        }
        out.extend(nested);
        if trail {
            push_break(out);
        }
    }

    // ---- tables ------------------------------------------------------------------------------

    fn table(&mut self, table: &Element, out: &mut Vec<Block>) {
        let info = self.styles.table(table.attr("table:style-name"));
        let first = !std::mem::replace(&mut self.seen_content, true);
        let new_page_style = info.master_page && !first;
        self.res
            .record_loss(LossKind::SectionBreaks, usize::from(new_page_style), None);
        if info.break_before || new_page_style {
            push_break(out);
        }
        let mut rows = Rows::default();
        self.table_rows(table, &mut rows);
        if !rows.cells.is_empty() {
            if self.ctx == Ctx::Cell {
                self.res.record_loss(LossKind::NestedTables, 1, None);
                out.push(Block::Para(flatten_table(&rows.cells)));
            } else {
                let align = md::majority_alignments(&rows.cells, &rows.align);
                out.push(Block::Table {
                    rows: rows.cells,
                    align,
                });
            }
        }
        if info.break_after {
            push_break(out);
        }
    }

    fn table_rows(&mut self, el: &Element, rows: &mut Rows) {
        for child in el.elements() {
            match child.name.as_str() {
                "table:table-row" => self.table_row(child, rows),
                "table:table-header-rows" | "table:table-rows" | "table:table-row-group" => {
                    self.table_rows(child, rows)
                }
                _ => {}
            }
        }
    }

    fn table_row(&mut self, row: &Element, rows: &mut Rows) {
        if self.cells_left == 0 {
            self.truncated = true;
            return;
        }
        let header = rows.cells.is_empty();
        let mut cells: Vec<String> = Vec::new();
        let mut aligns: Vec<Align> = Vec::new();
        for cell in row.elements() {
            let covered = match cell.name.as_str() {
                "table:table-cell" => false,
                "table:covered-table-cell" => true,
                _ => continue,
            };
            if cells.len() >= MAX_COLUMNS {
                self.truncated = true;
                break;
            }
            let merged = span_count(cell.attr("table:number-columns-spanned")) > 1
                || span_count(cell.attr("table:number-rows-spanned")) > 1;
            self.res
                .record_loss(LossKind::MergedCells, usize::from(merged && !covered), None);
            let (text, align) = if covered {
                (String::new(), Align::None)
            } else {
                self.cell_text(cell, header)
            };
            let repeat = repeat_count(cell.attr("table:number-columns-repeated"))
                .min(MAX_COLUMNS - cells.len());
            cells.extend(std::iter::repeat_n(text, repeat));
            aligns.extend(std::iter::repeat_n(align, repeat));
        }
        let times = repeat_count(row.attr("table:number-rows-repeated"));
        self.cells_left = self
            .cells_left
            .saturating_sub(cells.len().saturating_mul(times).max(1));
        rows.cells.extend(std::iter::repeat_n(cells, times));
        rows.align.extend(std::iter::repeat_n(aligns, times));
    }

    /// The text of a cell and the alignment of its first paragraph with text.
    fn cell_text(&mut self, cell: &Element, header: bool) -> (String, Align) {
        let outer = self.cell_align.take();
        let blocks = self.nested_blocks(cell, Ctx::Cell, header);
        let lines: Vec<String> = blocks.iter().filter_map(cell_line).collect();
        self.res.record_loss(
            LossKind::MultiParagraphCells,
            usize::from(lines.len() > 1),
            None,
        );
        let align = std::mem::replace(&mut self.cell_align, outer).unwrap_or_default();
        (md::join_cell(lines), align)
    }

    // ---- inline content ----------------------------------------------------------------------

    fn inline_children(&mut self, el: &Element, fmt: Fmt, in_link: bool, out: &mut Vec<Inline>) {
        for node in &el.children {
            match node {
                Node::Text(t) => self.text(t, fmt, out),
                Node::Element(e) => self.inline_element(e, fmt, in_link, out),
            }
        }
    }

    /// Text with ODF white-space handling: runs of white space collapse to one space, also
    /// across element boundaries.
    fn text(&mut self, raw: &str, fmt: Fmt, out: &mut Vec<Inline>) {
        let mut text = String::with_capacity(raw.len());
        for c in raw.chars() {
            if matches!(c, ' ' | '\t' | '\r' | '\n') {
                if !self.last_space {
                    text.push(' ');
                    self.last_space = true;
                }
            } else {
                text.push(c);
                self.last_space = false;
            }
        }
        if !text.is_empty() {
            out.push(Inline::Text(text, fmt));
        }
    }

    fn inline_element(&mut self, e: &Element, fmt: Fmt, in_link: bool, out: &mut Vec<Inline>) {
        match e.name.as_str() {
            "text:span" => self.span(e, fmt, in_link, out),
            "text:a" | "draw:a" => self.link(e, fmt, out),
            "text:s" => {
                let count = e
                    .attr("text:c")
                    .and_then(|c| c.trim().parse::<usize>().ok())
                    .unwrap_or(1)
                    .min(MAX_SPACES);
                if count > 0 {
                    out.push(Inline::Text(" ".repeat(count), fmt));
                }
                self.last_space = true;
            }
            "text:tab" => {
                let tab = if self.code { "\t" } else { " " };
                out.push(Inline::Text(tab.to_string(), fmt));
                self.last_space = true;
            }
            "text:line-break" => {
                out.push(Inline::Break);
                self.last_space = true;
            }
            "text:note" => self.note(e, out),
            "office:annotation" => self.res.record_loss(LossKind::Comments, 1, None),
            "text:ruby" => {
                if let Some(base) = e.child("text:ruby-base") {
                    self.inline_children(base, fmt, in_link, out);
                }
            }
            name if PAGE_FIELDS.contains(&name) => self.res.record_loss(LossKind::Fields, 1, None),
            "draw:frame" => self.frame(e, out),
            "draw:g" => {
                for child in e.elements() {
                    self.inline_element(child, fmt, in_link, out);
                }
            }
            name if name.starts_with("draw:") || name.starts_with("dr3d:") => self.drawing_shape(e),
            name if name.starts_with("text:") => self.inline_children(e, fmt, in_link, out),
            _ => {}
        }
    }

    fn span(&mut self, e: &Element, fmt: Fmt, in_link: bool, out: &mut Vec<Inline>) {
        let props = e
            .attr("text:style-name")
            .map(|n| self.styles.text(n))
            .unwrap_or_default();
        if props.hidden == Some(true) {
            let hidden = !e.text().trim().is_empty();
            self.res
                .record_loss(LossKind::HiddenText, usize::from(hidden), None);
            return;
        }
        self.note_text_losses(&props, e);
        self.inline_children(e, apply(fmt, &props, in_link), in_link, out);
    }

    fn link(&mut self, e: &Element, fmt: Fmt, out: &mut Vec<Inline>) {
        let mut kids = Vec::new();
        self.inline_children(
            e,
            Fmt {
                underline: false,
                ..fmt
            },
            true,
            &mut kids,
        );
        let href = e.attr("xlink:href").unwrap_or("").trim();
        let target = if href.is_empty() {
            None
        } else if let Some(name) = href.strip_prefix('#') {
            let resolved = self.resolve_anchor(name);
            self.res
                .record_loss(LossKind::Fields, usize::from(resolved.is_none()), None);
            resolved
        } else {
            Some(href.to_string())
        };
        match target {
            Some(url) if !kids.is_empty() => out.push(Inline::Link { url, kids }),
            _ => out.extend(kids),
        }
    }

    fn note(&mut self, note: &Element, out: &mut Vec<Inline>) {
        let Some(body) = note.child("text:note-body") else {
            return;
        };
        self.note_count += 1;
        let label = self.note_count.to_string();
        let mut blocks = self.nested_blocks(body, Ctx::Body, false);
        blocks.retain(|b| !matches!(b, Block::PageBreak));
        self.notes.push(md::footnote_definition(
            &label,
            &md::blocks_to_string(&blocks),
        ));
        out.push(Inline::Raw(format!("[^{label}]")));
        self.last_space = false;
    }

    // ---- frames, images and shapes -----------------------------------------------------------

    fn frame(&mut self, frame: &Element, out: &mut Vec<Inline>) {
        if let Some(object) = frame
            .child("draw:object")
            .or_else(|| frame.child("draw:object-ole"))
        {
            self.object(object);
        } else if let Some(text_box) = frame.child("draw:text-box") {
            self.res.record_loss(LossKind::TextBoxes, 1, None);
            self.shape_text(text_box);
        } else if let Some(image) = frame.child("draw:image") {
            self.image(frame, image, out);
        } else {
            self.res.record_loss(LossKind::VectorShapes, 1, None);
        }
    }

    /// An embedded object: a formula (MathML) or any other OLE/chart object.
    fn object(&mut self, object: &Element) {
        let href = object
            .attr("xlink:href")
            .unwrap_or("")
            .trim_end_matches('/');
        let math = object.child("math:math").is_some()
            || (!href.is_empty()
                && self
                    .package
                    .read(&format!("{href}/content.xml"))
                    .ok()
                    .flatten()
                    .is_some_and(|part| contains_bytes(&part, b"MathML")));
        let kind = if math {
            LossKind::UnsupportedEquations
        } else {
            LossKind::VectorShapes
        };
        self.res.record_loss(kind, 1, None);
    }

    /// A drawing shape: the shape is lost, its text follows the paragraph.
    fn drawing_shape(&mut self, shape: &Element) {
        self.res.record_loss(LossKind::VectorShapes, 1, None);
        self.shape_text(shape);
    }

    fn shape_text(&mut self, el: &Element) {
        let blocks = self.nested_blocks(el, self.ctx, self.header_row);
        self.pending.extend(blocks);
    }

    fn image(&mut self, frame: &Element, image: &Element, out: &mut Vec<Inline>) {
        let alt = alt_text(frame);
        let width = frame.attr("svg:width").and_then(length_to_px);
        let href = image.attr("xlink:href").unwrap_or("").trim();
        let found = if let Some(data) = image.child("office:binary-data") {
            let clean: String = data.text().chars().filter(|c| !c.is_whitespace()).collect();
            let key = format!("#binary-{}", self.image_count + 1);
            STANDARD
                .decode(clean)
                .ok()
                .and_then(|bytes| self.asset(&key, Some(bytes)))
        } else if href.is_empty() {
            None
        } else {
            self.asset(href, None)
        };
        match found {
            Some((path, unsupported)) => {
                self.res
                    .record_loss(LossKind::UnsupportedImages, usize::from(unsupported), None);
                out.push(Inline::Raw(md::image_markdown(&alt, &path, width)));
                self.last_space = false;
            }
            None if is_external(href) => {
                out.push(Inline::Raw(md::image_markdown(&alt, href, width)));
                self.last_space = false;
            }
            None => self.res.record_loss(LossKind::CorruptParts, 1, None),
        }
    }

    /// The asset path of an image (and whether its format is unsupported), extracting the
    /// package part on first use. `data` holds bytes that are not in the package.
    fn asset(&mut self, href: &str, data: Option<Vec<u8>>) -> Option<(String, bool)> {
        if let Some(known) = self.images.get(href) {
            return Some(known.clone());
        }
        let bytes = match data {
            Some(bytes) => bytes,
            None => {
                let decoded = percent_decode(href);
                match self.package.read(href).ok().flatten() {
                    Some(bytes) => bytes,
                    None if decoded != href => self.package.read(&decoded).ok().flatten()?,
                    None => return None,
                }
            }
        };
        let ext = image_extension(href, &bytes);
        let unsupported = UNSUPPORTED_IMAGES.contains(&ext.as_str());
        self.image_count += 1;
        let path = format!("{}_assets/image-{}.{ext}", self.stem, self.image_count);
        self.res.assets.push(Asset {
            relative_path: path.clone(),
            bytes,
        });
        self.images
            .insert(href.to_string(), (path.clone(), unsupported));
        Some((path, unsupported))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;
    use crate::odt::package::tests::zip;

    const MIME: &[u8] = b"application/vnd.oasis.opendocument.text";

    type Losses<'a> = &'a [(LossKind, usize)];
    /// Label, automatic styles, body of `office:text`, expected Markdown, expected losses.
    type Case<'a> = (&'a str, &'a str, &'a str, &'a str, Losses<'a>);
    type Part<'a> = (&'a str, &'a [u8]);
    /// Label, body, extra package parts, expected Markdown, expected losses, extracted assets.
    type PartCase<'a> = (&'a str, &'a str, &'a [Part<'a>], &'a str, Losses<'a>, usize);

    fn content(auto: &str, body: &str) -> String {
        format!(
            "<office:document-content><office:automatic-styles>{auto}</office:automatic-styles><office:body><office:text>{body}</office:text></office:body></office:document-content>"
        )
    }

    fn convert_with(auto: &str, body: &str, parts: &[(&str, &[u8])]) -> ImportResult {
        let xml = content(auto, body);
        let mut files: Vec<(&str, &[u8])> =
            vec![("mimetype", MIME), ("content.xml", xml.as_bytes())];
        files.extend_from_slice(parts);
        import_odt(&zip(&files), "doc").unwrap()
    }

    fn convert(auto: &str, body: &str) -> ImportResult {
        convert_with(auto, body, &[])
    }

    fn losses_of(result: &ImportResult) -> Vec<(LossKind, usize)> {
        result
            .sorted_losses()
            .iter()
            .map(|l| (l.kind, l.count))
            .collect()
    }

    fn check(cases: &[Case<'_>]) {
        for (label, auto, body, markdown, losses) in cases {
            let result = convert(auto, body);
            assert_eq!(result.markdown, *markdown, "{label}");
            assert_eq!(losses_of(&result), *losses, "{label}");
        }
    }

    /// A package with stored (uncompressed) entries, so a test can damage one entry's bytes.
    fn stored(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, data) in files {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    /// Flips the first byte of the first occurrence of `needle` (an entry's data).
    fn damage(mut bytes: Vec<u8>, needle: &[u8]) -> Vec<u8> {
        let at = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap();
        bytes[at] ^= 0xFF;
        bytes
    }

    /// Start of the message of every [`ImportError::Corrupt`].
    const CORRUPT: &str = "the document is corrupt: ";

    /// The message of the error of an import that must fail.
    fn failure(result: Result<ImportResult, ImportError>) -> String {
        result.unwrap_err().to_string()
    }

    fn parse(xml: &str) -> Element {
        xml::parse(xml.as_bytes()).unwrap()
    }

    fn plain(s: &str) -> Inline {
        Inline::Text(s.to_string(), Fmt::default())
    }

    #[test]
    fn data_that_is_not_a_package_is_identified() {
        let ole = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0];
        let cases: [(&[u8], &str); 6] = [
            (b"hello world", "text"),
            (&[0u8; 64], "unknown"),
            (&ole, "DOC"),
            (b"{\\rtf1 x}", "RTF"),
            (b"%PDF-1.4", "PDF"),
            (b"<html></html>", "HTML"),
        ];
        for (bytes, detected) in cases {
            assert_eq!(
                import_odt(bytes, "x"),
                Err(ImportError::WrongFormat {
                    expected: "ODT",
                    detected
                })
            );
        }
        let message = failure(import_odt(b"PK\x03\x04junk", "x"));
        assert!(message.starts_with(CORRUPT), "{message}");
    }

    #[test]
    fn other_opendocument_and_office_packages_are_identified() {
        let mimes: [(&[u8], &str); 5] = [
            (b"application/vnd.oasis.opendocument.spreadsheet", "ODS"),
            (b"application/vnd.oasis.opendocument.presentation", "ODP"),
            (b"application/vnd.oasis.opendocument.graphics", "ODG"),
            (
                b"application/vnd.oasis.opendocument.formula",
                "OpenDocument",
            ),
            (b"application/epub+zip", "EPUB"),
        ];
        for (mime, detected) in mimes {
            let bytes = zip(&[("mimetype", mime)]);
            assert_eq!(import_odt(&bytes, "x"), Err(wrong_zip(detected)));
        }
        for (part, detected) in [
            ("word/document.xml", "DOCX"),
            ("xl/workbook.xml", "XLSX"),
            ("ppt/presentation.xml", "PPTX"),
        ] {
            assert_eq!(
                import_odt(&zip(&[(part, b"<x/>")]), "x"),
                Err(wrong_zip(detected))
            );
        }
        assert_eq!(
            import_odt(&zip(&[("a", b"")]), "x"),
            Err(ImportError::Corrupt(
                "the package has no content.xml".into()
            ))
        );
    }

    #[test]
    fn packages_without_a_text_mimetype_are_read_when_they_have_content() {
        let xml = content("", "<text:p>x</text:p>");
        let cases: [&[Part<'_>]; 3] = [
            &[("content.xml", xml.as_bytes())],
            &[("mimetype", b"text/plain"), ("content.xml", xml.as_bytes())],
            &[
                (
                    "mimetype",
                    b"application/vnd.oasis.opendocument.text-template",
                ),
                ("content.xml", xml.as_bytes()),
            ],
        ];
        for files in cases {
            assert_eq!(import_odt(&zip(files), "x").unwrap().markdown, "x\n");
        }
    }

    #[test]
    fn encrypted_manifests_are_refused() {
        let xml = content("", "<text:p>x</text:p>");
        let encrypted = b"<manifest:manifest><manifest:file-entry><manifest:encryption-data/></manifest:file-entry></manifest:manifest>";
        let plain_manifest = b"<manifest:manifest><manifest:file-entry/></manifest:manifest>";
        let build = |manifest: &[u8]| {
            zip(&[
                ("mimetype", MIME),
                ("META-INF/manifest.xml", manifest),
                ("content.xml", xml.as_bytes()),
            ])
        };
        assert_eq!(
            import_odt(&build(encrypted), "x"),
            Err(ImportError::Encrypted)
        );
        assert_eq!(
            import_odt(&build(plain_manifest), "x").unwrap().markdown,
            "x\n"
        );
    }

    #[test]
    fn unusable_content_parts_are_corrupt_or_another_document_type() {
        let corrupt = [
            ("<a", "content.xml: "),
            ("<root/>", "content.xml is not an OpenDocument content part"),
            (
                "<office:document-content/>",
                "content.xml has no document body",
            ),
            (
                "<office:document-content><office:body/></office:document-content>",
                "content.xml has no text document body",
            ),
        ];
        for (xml, reason) in corrupt {
            let bytes = zip(&[("mimetype", MIME), ("content.xml", xml.as_bytes())]);
            let message = failure(import_odt(&bytes, "x"));
            assert!(
                message.starts_with(&format!("{CORRUPT}{reason}")),
                "{xml}: {message}"
            );
        }
        assert_eq!(
            import_odt(&zip(&[("mimetype", MIME)]), "x"),
            Err(ImportError::Corrupt(
                "the package has no content.xml".into()
            ))
        );
        let body = |inner: &str| {
            format!(
                "<office:document-content><office:body>{inner}</office:body></office:document-content>"
            )
        };
        let cases = [
            ("<office:spreadsheet/>", Err(wrong_zip("ODS"))),
            ("<office:presentation/>", Err(wrong_zip("ODP"))),
            ("<office:drawing/>", Err(wrong_zip("ODG"))),
            (
                "<office:chart/>",
                Err(ImportError::Corrupt(
                    "content.xml has no text document body".into(),
                )),
            ),
        ];
        for (inner, want) in cases {
            let bytes = zip(&[("mimetype", MIME), ("content.xml", body(inner).as_bytes())]);
            assert_eq!(import_odt(&bytes, "x"), want, "{inner}");
        }
    }

    #[test]
    fn styles_xml_contributes_named_styles_page_furniture_and_damage() {
        let xml = content("", r#"<text:p text:style-name="Quotations">q</text:p>"#);
        let styles = r##"<office:document-styles><office:styles><style:style style:name="Quotations" style:family="paragraph"/></office:styles><office:automatic-styles><style:page-layout style:name="pm"><style:page-layout-properties><style:columns fo:column-count="2"/></style:page-layout-properties></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="S" style:page-layout-name="pm"><style:header><text:p>H</text:p></style:header></style:master-page></office:master-styles></office:document-styles>"##;
        let bytes = zip(&[
            ("mimetype", MIME),
            ("content.xml", xml.as_bytes()),
            ("styles.xml", styles.as_bytes()),
        ]);
        let result = import_odt(&bytes, "x").unwrap();
        assert_eq!(result.markdown, "> q\n");
        assert_eq!(
            losses_of(&result),
            [(LossKind::HeadersFooters, 1), (LossKind::Columns, 1)]
        );
        let broken = zip(&[
            ("mimetype", MIME),
            ("content.xml", xml.as_bytes()),
            ("styles.xml", b"<a"),
        ]);
        let result = import_odt(&broken, "x").unwrap();
        assert_eq!(result.markdown, "q\n");
        assert_eq!(losses_of(&result), [(LossKind::CorruptParts, 1)]);
    }

    #[test]
    fn damaged_entries_are_fatal_except_the_style_sheet() {
        let xml = content("", "<text:p>x</text:p>");
        let manifest = b"<manifest:manifest/>";
        let files: [(&str, &[u8]); 4] = [
            ("mimetype", MIME),
            ("META-INF/manifest.xml", manifest),
            ("content.xml", xml.as_bytes()),
            ("styles.xml", b"<office:document-styles/>"),
        ];
        let intact = stored(&files);
        assert_eq!(import_odt(&intact, "x").unwrap().markdown, "x\n");
        for needle in [
            &b"application/vnd"[..],
            b"<manifest:manifest/>",
            b"<office:document-content",
        ] {
            let message = failure(import_odt(&damage(intact.clone(), needle), "x"));
            let label = String::from_utf8_lossy(needle);
            assert!(message.starts_with(CORRUPT), "{label}: {message}");
        }
        let result = import_odt(&damage(intact, b"<office:document-styles/>"), "x").unwrap();
        assert_eq!(losses_of(&result), [(LossKind::CorruptParts, 1)]);
    }

    #[test]
    fn the_read_budget_is_enforced_while_reading_the_style_sheet() {
        let xml = content("", "<text:p>x</text:p>");
        let styles = b"<office:document-styles/>";
        let bytes = zip(&[
            ("mimetype", MIME),
            ("content.xml", xml.as_bytes()),
            ("styles.xml", styles),
        ]);
        let mut package = Package::open(&bytes).unwrap();
        package.exhaust_budget((MIME.len() + xml.len() + styles.len() - 1) as u64);
        assert_eq!(
            import_package(&mut package, "x"),
            Err(ImportError::TooLarge(format!(
                "more than {} bytes uncompressed",
                crate::odt::package::MAX_TOTAL
            )))
        );
    }

    #[test]
    fn repeat_counts_are_bounded() {
        let big = r#"<table:table><table:table-row table:number-rows-repeated="1000"><table:table-cell table:number-columns-repeated="1000"/></table:table-row></table:table>"#;
        let result = convert("", big);
        let first = result.markdown.lines().next().unwrap();
        assert_eq!(first.matches('|').count(), MAX_REPEAT + 1);
        assert_eq!(result.markdown.lines().count(), 2 + MAX_REPEAT - 1);
        let spaces = convert("", "<text:p>a<text:s text:c=\"5000\"/>b</text:p>");
        assert_eq!(spaces.markdown, format!("a{}b\n", " ".repeat(MAX_SPACES)));
    }

    #[test]
    fn rows_beyond_the_column_and_cell_limits_are_dropped() {
        let cells = "<table:table-cell table:number-columns-repeated=\"64\"/>".repeat(65);
        let result = convert(
            "",
            &format!("<table:table><table:table-row>{cells}</table:table-row></table:table>"),
        );
        assert_eq!(
            result.markdown.lines().next().unwrap().matches('|').count(),
            MAX_COLUMNS + 1
        );
        assert_eq!(losses_of(&result), [(LossKind::CorruptParts, 1)]);

        let styles = Styles::default();
        let bytes = zip(&[("a", b"")]);
        let mut package = Package::open(&bytes).unwrap();
        let mut converter = Converter::new(&styles, &mut package, "s");
        converter.cells_left = 0;
        let mut rows = Rows::default();
        converter.table_row(
            &parse("<table:table-row><table:table-cell/></table:table-row>"),
            &mut rows,
        );
        assert!(rows.cells.is_empty() && rows.align.is_empty() && converter.truncated);
    }

    #[test]
    fn helper_functions_parse_lengths_counts_and_escapes() {
        assert_eq!(
            [
                repeat_count(None),
                repeat_count(Some("3")),
                repeat_count(Some("100")),
                repeat_count(Some("0")),
                repeat_count(Some("x"))
            ],
            [1, 3, MAX_REPEAT, 1, 1]
        );
        assert_eq!(
            [
                span_count(None),
                span_count(Some(" 2 ")),
                span_count(Some("x"))
            ],
            [1, 2, 1]
        );
        let lengths = [
            ("7.726cm", Some(292)),
            ("3in", Some(288)),
            ("100pt", Some(133)),
            ("50px", Some(50)),
            ("10mm", Some(38)),
            ("6pc", Some(96)),
            (" 2in ", Some(192)),
            ("2 in", Some(192)),
            ("5", None),
            ("abc", None),
            ("1.5.5cm", None),
            ("0cm", None),
            ("999999px", None),
            ("-5px", None),
            ("3em", None),
        ];
        for (text, want) in lengths {
            assert_eq!(length_to_px(text), want, "{text}");
        }
        let decoded = [
            ("a%20b", "a b"),
            ("%zz", "%zz"),
            ("%4", "%4"),
            ("%C3%A9", "\u{e9}"),
            ("plain", "plain"),
            ("100%", "100%"),
        ];
        for (text, want) in decoded {
            assert_eq!(percent_decode(text), want, "{text}");
        }
        assert!(contains_bytes(b"abcdef", b"cd"));
        assert!(!contains_bytes(b"ab", b"abc"));
        assert_eq!(escape_heading("C#".into()), "C\\#");
        assert_eq!(escape_heading("C".into()), "C");
        for href in ["http://x/y.png", "../x.png", "/abs.png"] {
            assert!(is_external(href), "{href}");
        }
        assert!(!is_external("Pictures/a.png"));
    }

    #[test]
    fn image_formats_are_recognized_by_content_then_by_name() {
        let mut emf = vec![0u8; 40];
        emf.extend_from_slice(b" EMF");
        let cases: [(&[u8], Option<&str>); 13] = [
            (b"\x89PNGxx", Some("png")),
            (b"\xff\xd8x", Some("jpg")),
            (b"GIF89a", Some("gif")),
            (b"BMxx", Some("bmp")),
            (b"II*\0", Some("tif")),
            (b"MM\0*", Some("tif")),
            (b"RIFF\0\0\0\0WEBP", Some("webp")),
            (b"RIFF\0\0\0\0AVI ", None),
            (b"\xd7\xcd\xc6\x9a", Some("wmf")),
            (&emf, Some("emf")),
            (b"<?xml?><SVG xmlns='x'>", Some("svg")),
            (b"hello", None),
            (b"", None),
        ];
        for (bytes, want) in cases {
            assert_eq!(sniff_extension(bytes), want, "{bytes:?}");
        }
        let names = [
            ("Pictures/a.JPEG", "jpg"),
            ("a.dat", "bin"),
            ("evil.exe", "bin"),
            ("Pictures/x.TIFF", "tiff"),
            ("noext", "bin"),
            ("d.d/file", "bin"),
            ("a.toolongext", "bin"),
            ("a.", "bin"),
            ("a.p-g", "bin"),
        ];
        for (href, want) in names {
            assert_eq!(image_extension(href, b"zz"), want, "{href}");
        }
        assert_eq!(image_extension("a.txt", b"\x89PNGxx"), "png");
    }

    #[test]
    fn block_helpers_follow_their_contracts() {
        let el = parse(
            "<p>  a <text:note>n</text:note>b<office:annotation>c</office:annotation><text:ruby-text>r</text:ruby-text><text:s/>d<text:tab/>e<text:line-break/>f <text:span>g</text:span></p>",
        );
        assert_eq!(plain_text(&el), "a b d e f g");
        let mut blocks = Vec::new();
        push_break(&mut blocks);
        push_break(&mut blocks);
        assert_eq!(blocks, [Block::PageBreak]);
        blocks.push(Block::Para("x".into()));
        push_break(&mut blocks);
        assert_eq!(blocks.len(), 3);
        let inlines = [
            plain("a"),
            Inline::Raw("b".into()),
            Inline::Break,
            Inline::Link {
                url: "u".into(),
                kids: vec![plain("c")],
            },
            plain("  "),
        ];
        assert_eq!(code_text(&inlines), "ab\nc");
        assert_eq!(
            flatten_table(&[
                vec!["a".into(), String::new(), "b".into()],
                vec!["c".into()]
            ]),
            "a b c"
        );
        let lines = [
            (Block::Para("p".into()), Some("p")),
            (Block::Heading(1, "h".into()), Some("h")),
            (Block::Code("c".into()), Some("c")),
            (Block::Quote("q".into()), Some("q")),
            (
                Block::Item {
                    level: 0,
                    marker: "-".into(),
                    text: "a\n\nb".into(),
                },
                Some("- a<br>b"),
            ),
            (Block::PageBreak, None),
            (Block::Rule, None),
            (
                Block::Table {
                    rows: Vec::new(),
                    align: Vec::new(),
                },
                None,
            ),
        ];
        for (block, want) in lines {
            assert_eq!(cell_line(&block).as_deref(), want, "{block:?}");
        }
        let alts = [
            ("<f><svg:title>T</svg:title></f>", "T"),
            ("<f><svg:desc>D</svg:desc><svg:title>T</svg:title></f>", "D"),
            ("<f><svg:desc> </svg:desc><svg:title>T</svg:title></f>", "T"),
            ("<f/>", ""),
        ];
        for (xml, want) in alts {
            assert_eq!(alt_text(&parse(xml)), want, "{xml}");
        }
    }

    #[test]
    fn span_styles_override_the_context_and_links_swallow_underline() {
        let base = Fmt {
            bold: true,
            highlight: Some(ssw_core::markdown::ast::HighlightColor::Red),
            ..Fmt::default()
        };
        let props = TextProps {
            bold: Some(false),
            underline: Some(true),
            highlight: Some(None),
            mono: Some(true),
            ..TextProps::default()
        };
        let applied = apply(base, &props, false);
        assert_eq!(
            applied,
            Fmt {
                underline: true,
                code: true,
                ..Fmt::default()
            }
        );
        assert!(!apply(base, &props, true).underline);
        assert_eq!(apply(base, &TextProps::default(), false), base);
        assert!(same_family(
            ListKind::Number {
                decimal: true,
                start: 1
            },
            ListKind::Number {
                decimal: false,
                start: 9
            }
        ));
        assert!(same_family(ListKind::Bullet, ListKind::Bullet));
        assert!(!same_family(ListKind::Bullet, ListKind::Task(false)));
        assert!(!same_family(ListKind::Task(false), ListKind::Task(true)));
    }

    #[test]
    fn inline_cases() {
        check(&[
            (
                "whitespace",
                r##""##,
                r##"<text:p>  a   b 	 c
 d  </text:p>"##,
                "a b c d\n",
                &[],
            ),
            (
                "text:s count",
                r##""##,
                r##"<text:p>a<text:s/>b<text:s text:c="3"/>c<text:s text:c="x"/>d<text:s text:c="0"/>e</text:p>"##,
                "a b   c de\n",
                &[],
            ),
            (
                "tab/linebreak",
                r##""##,
                r##"<text:p>a<text:tab/>b<text:line-break/>c<text:line-break/> d</text:p>"##,
                "a b\\\nc\\\nd\n",
                &[],
            ),
            (
                "leading space element",
                r##""##,
                r##"<text:p><text:s/>x</text:p>"##,
                "x\n",
                &[],
            ),
            (
                "emphasis",
                r##"<style:style style:name="B" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style><style:style style:name="I" style:family="text"><style:text-properties fo:font-style="italic"/></style:style><style:style style:name="U" style:family="text"><style:text-properties style:text-underline-style="solid"/></style:style><style:style style:name="S" style:family="text"><style:text-properties style:text-line-through-style="solid"/></style:style><style:style style:name="Hy" style:family="text"><style:text-properties fo:background-color="#ffff00"/></style:style><style:style style:name="Hb" style:family="text"><style:text-properties fo:background-color="#c5ddfb"/></style:style><style:style style:name="Hn" style:family="text"><style:text-properties fo:background-color="transparent"/></style:style><style:style style:name="M" style:family="text"><style:text-properties style:font-name="Courier New"/></style:style>"##,
                r##"<text:p><text:span text:style-name="B">bold</text:span> <text:span text:style-name="I">it</text:span> <text:span text:style-name="U">und</text:span> <text:span text:style-name="S">str</text:span> <text:span text:style-name="Hy">yel</text:span> <text:span text:style-name="Hb">blu</text:span> <text:span text:style-name="Hn">none</text:span> <text:span text:style-name="M">code</text:span></text:p>"##,
                "**bold** *it* ~und~ ~~str~~ ==yel== =={blue}blu== none `code`\n",
                &[],
            ),
            (
                "nested",
                r##"<style:style style:name="B" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style><style:style style:name="I" style:family="text"><style:text-properties fo:font-style="italic"/></style:style><style:style style:name="Nob" style:family="text"><style:text-properties fo:font-weight="normal"/></style:style>"##,
                r##"<text:p><text:span text:style-name="B">a <text:span text:style-name="I">b</text:span><text:span text:style-name="Nob"> c</text:span></text:span> d</text:p>"##,
                "**a** ***b*** c d\n",
                &[],
            ),
            (
                "paragraph style emphasis",
                r##"<style:style style:name="PBold" style:family="paragraph"><style:text-properties fo:font-weight="bold" fo:font-style="italic"/></style:style><style:style style:name="Nob" style:family="text"><style:text-properties fo:font-weight="normal"/></style:style>"##,
                r##"<text:p text:style-name="PBold">x <text:span text:style-name="Nob">y</text:span></text:p>"##,
                "***x*** *y*\n",
                &[],
            ),
            (
                "hidden span",
                r##"<style:style style:name="Hid" style:family="text"><style:text-properties text:display="none"/></style:style>"##,
                r##"<text:p>a<text:span text:style-name="Hid">secret</text:span>b<text:span text:style-name="Hid">  </text:span>c</text:p>"##,
                "abc\n",
                &[(LossKind::HiddenText, 1)],
            ),
            (
                "colored/sub",
                r##"<style:style style:name="Col" style:family="text"><style:text-properties fo:color="#ff0000"/></style:style><style:style style:name="Sup" style:family="text"><style:text-properties style:text-position="super 58%"/></style:style>"##,
                r##"<text:p><text:span text:style-name="Col">red</text:span> <text:span text:style-name="Sup">up</text:span> <text:span text:style-name="Col">  </text:span></text:p>"##,
                "red up\n",
                &[(LossKind::FontsAndColors, 1), (LossKind::SubSuperscript, 1)],
            ),
            (
                "unknown span style",
                r##""##,
                r##"<text:p><text:span text:style-name="Nope">x</text:span></text:p>"##,
                "x\n",
                &[],
            ),
            (
                "span without style",
                r##""##,
                r##"<text:p><text:span>x</text:span></text:p>"##,
                "x\n",
                &[],
            ),
            (
                "escapes",
                r##""##,
                r##"<text:p>a*b_c [d] &lt;e&gt; `f` $g ~h~ i=j == k|l \ m &amp; n</text:p>"##,
                "a\\*b_c \\[d\\] \\<e\\> \\`f\\` \\$g \\~h\\~ i=j \\=\\= k|l \\\\ m & n\n",
                &[],
            ),
            (
                "generic text element",
                r##""##,
                r##"<text:p>a<text:date>2020</text:date> b<text:bookmark text:name="x"/>c<text:soft-page-break/>d<svg:title>no</svg:title></text:p>"##,
                "a2020 bcd\n",
                &[],
            ),
            (
                "ruby",
                r##""##,
                r##"<text:p><text:ruby><text:ruby-base>base</text:ruby-base><text:ruby-text>top</text:ruby-text></text:ruby>!</text:p>"##,
                "base!\n",
                &[],
            ),
            (
                "ruby without base",
                r##""##,
                r##"<text:p><text:ruby><text:ruby-text>top</text:ruby-text></text:ruby>!</text:p>"##,
                "!\n",
                &[],
            ),
            (
                "page fields",
                r##""##,
                r##"<text:p>p<text:page-number/>q<text:page-count/>r<text:page-continuation/></text:p>"##,
                "pqr\n",
                &[(LossKind::Fields, 3)],
            ),
            (
                "annotation",
                r##""##,
                r##"<text:p>a<office:annotation><text:p>note</text:p></office:annotation>b</text:p>"##,
                "ab\n",
                &[(LossKind::Comments, 1)],
            ),
            (
                "code span",
                r##"<style:style style:name="M" style:family="text"><style:text-properties style:font-name="Courier New"/></style:style>"##,
                r##"<text:p>run <text:span text:style-name="M">a`b</text:span> and <text:span text:style-name="M"> x </text:span></text:p>"##,
                "run ``a`b`` and `x`\n",
                &[],
            ),
            (
                "highlight blue across",
                r##"<style:style style:name="Hb" style:family="text"><style:text-properties fo:background-color="#c5ddfb"/></style:style>"##,
                r##"<text:p><text:span text:style-name="Hb">one two</text:span></text:p>"##,
                "=={blue}one two==\n",
                &[],
            ),
        ]);
    }

    #[test]
    fn paragraph_cases() {
        check(&[
            (
                "headings",
                r##"<style:style style:name="Title" style:family="paragraph"/><style:style style:name="Heading_20_2" style:family="paragraph"/>"##,
                r##"<text:h text:outline-level="1">One</text:h><text:h text:outline-level="2">Two</text:h><text:h text:outline-level="9">Seven</text:h><text:h text:outline-level="0">Zero</text:h><text:h>NoLevel</text:h><text:h text:style-name="Heading_20_2">By style</text:h><text:p text:style-name="Title">Title text</text:p><text:p text:style-name="Heading_20_2">Heading style para</text:p>"##,
                "# One\n\n## Two\n\n###### Seven\n\n# Zero\n\n# NoLevel\n\n## By style\n\n# Title text\n\n## Heading style para\n",
                &[],
            ),
            (
                "heading with bold span and hash",
                r##"<style:style style:name="B" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style>"##,
                r##"<text:h text:outline-level="1">C#</text:h><text:h text:outline-level="2">x <text:span text:style-name="B">b</text:span> y</text:h><text:h text:outline-level="1"></text:h><text:h text:outline-level="1">   </text:h>"##,
                "# C\\#\n\n## x **b** y\n",
                &[],
            ),
            (
                "heading line break",
                r##""##,
                r##"<text:h text:outline-level="1">a<text:line-break/>b</text:h>"##,
                "# a b\n",
                &[],
            ),
            (
                "quote merge",
                r##"<style:style style:name="Quotations" style:family="paragraph"><style:text-properties fo:font-style="italic"/></style:style><style:style style:name="I" style:family="text"><style:text-properties fo:font-style="italic"/></style:style>"##,
                r##"<text:p text:style-name="Quotations">one</text:p><text:p text:style-name="Quotations"></text:p><text:p text:style-name="Quotations">two <text:span text:style-name="I">em</text:span></text:p><text:p>plain</text:p><text:p text:style-name="Quotations">three</text:p>"##,
                "> one\n>\n> two *em*\n\nplain\n\n> three\n",
                &[],
            ),
            (
                "code merge",
                r##"<style:style style:name="Preformatted_20_Text" style:family="paragraph"/>"##,
                r##"<text:p text:style-name="Preformatted_20_Text">a</text:p><text:p text:style-name="Preformatted_20_Text"></text:p><text:p text:style-name="Preformatted_20_Text"><text:s text:c="2"/>b<text:tab/>c<text:line-break/>d</text:p><text:p>after</text:p><text:p text:style-name="Preformatted_20_Text"></text:p><text:p text:style-name="Preformatted_20_Text">x</text:p>"##,
                "```\na\n\n  b\tc\nd\n```\n\nafter\n\n```\nx\n```\n",
                &[],
            ),
            (
                "code with backticks",
                r##"<style:style style:name="Preformatted_20_Text" style:family="paragraph"/>"##,
                r##"<text:p text:style-name="Preformatted_20_Text">```</text:p>"##,
                "````\n```\n````\n",
                &[],
            ),
            (
                "code link",
                r##"<style:style style:name="Preformatted_20_Text" style:family="paragraph"/>"##,
                r##"<text:p text:style-name="Preformatted_20_Text"><text:a xlink:href="http://e.com">lnk</text:a> <text:note><text:note-body><text:p>n</text:p></text:note-body></text:note></text:p>"##,
                "```\nlnk [^1]\n```\n\n[^1]: n\n",
                &[],
            ),
            (
                "rule",
                r##"<style:style style:name="Horizontal_20_Line" style:family="paragraph"/><style:style style:name="PC" style:family="paragraph"><style:paragraph-properties fo:text-align="center"/></style:style>"##,
                r##"<text:p></text:p><text:p text:style-name="Horizontal_20_Line"></text:p><text:p text:style-name="Horizontal_20_Line">x</text:p><text:p>y</text:p>"##,
                "---\n\n---\n\ny\n",
                &[],
            ),
            (
                "breaks",
                r##"<style:style style:name="PB" style:family="paragraph"><style:paragraph-properties fo:break-before="page"/></style:style><style:style style:name="PA" style:family="paragraph"><style:paragraph-properties fo:break-after="page"/></style:style>"##,
                r##"<text:p>a</text:p><text:p text:style-name="PB">b</text:p><text:p text:style-name="PB">c</text:p><text:p text:style-name="PA">d</text:p><text:p text:style-name="PA">e</text:p>"##,
                "a\n\n<!-- pagebreak -->\n\nb\n\n<!-- pagebreak -->\n\nc\n\nd\n\n<!-- pagebreak -->\n\ne\n",
                &[],
            ),
            (
                "page break at edges",
                r##"<style:style style:name="PB" style:family="paragraph"><style:paragraph-properties fo:break-before="page"/></style:style><style:style style:name="PA" style:family="paragraph"><style:paragraph-properties fo:break-after="page"/></style:style>"##,
                r##"<text:p text:style-name="PB">x</text:p><text:p>y</text:p><text:p text:style-name="PA">z</text:p>"##,
                "x\n\ny\n\nz\n",
                &[],
            ),
            (
                "master page",
                r##"<style:style style:name="PM" style:family="paragraph" style:master-page-name="Standard"/>"##,
                r##"<text:p text:style-name="PM">a</text:p><text:p text:style-name="PM">b</text:p><text:p>c</text:p>"##,
                "a\n\n<!-- pagebreak -->\n\nb\n\nc\n",
                &[(LossKind::SectionBreaks, 1)],
            ),
            (
                "hidden paragraph",
                r##"<style:style style:name="PH" style:family="paragraph"><style:text-properties text:display="none"/></style:style>"##,
                r##"<text:p text:style-name="PH">gone</text:p><text:p text:style-name="PH">  </text:p><text:p>kept</text:p>"##,
                "kept\n",
                &[(LossKind::HiddenText, 1)],
            ),
            (
                "aligned paragraph",
                r##"<style:style style:name="PC" style:family="paragraph"><style:paragraph-properties fo:text-align="center"/></style:style>"##,
                r##"<text:p text:style-name="PC">centered</text:p><text:p text:style-name="PC">  </text:p><text:p>left</text:p>"##,
                "centered\n\nleft\n",
                &[(LossKind::Alignment, 1)],
            ),
            (
                "colored paragraph",
                r##"<style:style style:name="PR" style:family="paragraph"><style:text-properties fo:color="#ff0000" style:text-position="sub 58%"/></style:style>"##,
                r##"<text:p text:style-name="PR">red</text:p><text:p text:style-name="PR">  </text:p>"##,
                "red\n",
                &[(LossKind::FontsAndColors, 1), (LossKind::SubSuperscript, 1)],
            ),
            ("empty doc", r##""##, r##""##, "", &[]),
            (
                "only whitespace paragraphs",
                r##""##,
                r##"<text:p></text:p><text:p>   </text:p>"##,
                "",
                &[],
            ),
            (
                "paragraph starting like a list",
                r##""##,
                r##"<text:p>1. not a list</text:p><text:p>- dash</text:p><text:p># hash</text:p><text:p>---</text:p>"##,
                "1\\. not a list\n\n\\- dash\n\n\\# hash\n\n\\---\n",
                &[],
            ),
        ]);
    }

    #[test]
    fn lists_cases() {
        check(&[
            (
                "bullets",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style>"##,
                r##"<text:list text:style-name="LB"><text:list-item><text:p>one</text:p></text:list-item><text:list-item><text:p>two</text:p></text:list-item></text:list>"##,
                "- one\n- two\n",
                &[],
            ),
            (
                "numbered",
                r##"<text:list-style style:name="LN"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-number text:level="2" style:num-format="a"/></text:list-style>"##,
                r##"<text:list text:style-name="LN"><text:list-item><text:p>one</text:p></text:list-item><text:list-item><text:p>two</text:p></text:list-item><text:list-item><text:p>three</text:p></text:list-item></text:list>"##,
                "1. one\n2. two\n3. three\n",
                &[],
            ),
            (
                "start value on style",
                r##"<text:list-style style:name="LS"><text:list-level-style-number text:level="1" style:num-format="1" text:start-value="5"/></text:list-style>"##,
                r##"<text:list text:style-name="LS"><text:list-item><text:p>a</text:p></text:list-item><text:list-item><text:p>b</text:p></text:list-item></text:list>"##,
                "5. a\n6. b\n",
                &[],
            ),
            (
                "start value on item",
                r##"<text:list-style style:name="LN"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-number text:level="2" style:num-format="a"/></text:list-style>"##,
                r##"<text:list text:style-name="LN"><text:list-item text:start-value="7"><text:p>a</text:p></text:list-item><text:list-item><text:p>b</text:p></text:list-item><text:list-item text:start-value="x"><text:p>c</text:p></text:list-item></text:list>"##,
                "7. a\n8. b\n9. c\n",
                &[],
            ),
            (
                "letters",
                r##"<text:list-style style:name="LN"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-number text:level="2" style:num-format="a"/></text:list-style>"##,
                r##"<text:list text:style-name="LN"><text:list-item><text:p>top</text:p><text:list text:style-name="LN"><text:list-item><text:p>sub1</text:p></text:list-item><text:list-item><text:p>sub2</text:p></text:list-item></text:list></text:list-item><text:list-item><text:p>two</text:p></text:list-item></text:list>"##,
                "1. top\n    1. sub1\n    2. sub2\n2. two\n",
                &[(LossKind::ListNumberingStyle, 1)],
            ),
            (
                "nested without style",
                r##"<text:list-style style:name="LN"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-number text:level="2" style:num-format="a"/></text:list-style>"##,
                r##"<text:list text:style-name="LN"><text:list-item><text:p>a</text:p><text:list><text:list-item><text:p>b</text:p></text:list-item></text:list></text:list-item></text:list>"##,
                "1. a\n    1. b\n",
                &[(LossKind::ListNumberingStyle, 1)],
            ),
            (
                "unknown list style",
                r##""##,
                r##"<text:list text:style-name="Nope"><text:list-item><text:p>a</text:p></text:list-item><text:list-item><text:p>b</text:p></text:list-item></text:list>"##,
                "- a\n- b\n",
                &[],
            ),
            (
                "no list style",
                r##""##,
                r##"<text:list><text:list-item><text:p>a</text:p></text:list-item><text:list-item><text:p>b</text:p></text:list-item></text:list>"##,
                "- a\n- b\n",
                &[],
            ),
            (
                "tasks",
                r##"<text:list-style style:name="LT"><text:list-level-style-bullet text:level="1" text:bullet-char="&#9744;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9745;"/></text:list-style>"##,
                r##"<text:list text:style-name="LT"><text:list-item><text:p>open</text:p></text:list-item><text:list-item><text:p>done</text:p><text:list><text:list-item><text:p>sub</text:p></text:list-item></text:list></text:list-item></text:list>"##,
                "- [ ] open\n- [ ] done\n    - [x] sub\n",
                &[],
            ),
            (
                "list header",
                r##"<text:list-style style:name="LN"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-number text:level="2" style:num-format="a"/></text:list-style>"##,
                r##"<text:list text:style-name="LN"><text:list-header><text:p>Head</text:p></text:list-header><text:list-item><text:p>one</text:p></text:list-item><text:list-item><text:p>two</text:p></text:list-item></text:list>"##,
                "- Head\n1. one\n2. two\n",
                &[],
            ),
            (
                "two bullet lists",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style>"##,
                r##"<text:list text:style-name="LB"><text:list-item><text:p>a</text:p></text:list-item></text:list><text:list text:style-name="LB"><text:list-item><text:p>b</text:p></text:list-item></text:list><text:list text:style-name="LB"><text:list-item><text:p>c</text:p></text:list-item></text:list>"##,
                "- a\n* b\n- c\n",
                &[],
            ),
            (
                "two numbered lists",
                r##"<text:list-style style:name="LN"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-number text:level="2" style:num-format="a"/></text:list-style>"##,
                r##"<text:list text:style-name="LN"><text:list-item><text:p>a</text:p></text:list-item></text:list><text:list text:style-name="LN"><text:list-item><text:p>b</text:p></text:list-item></text:list><text:list text:style-name="LN"><text:list-item><text:p>c</text:p></text:list-item></text:list>"##,
                "1. a\n1) b\n1. c\n",
                &[],
            ),
            (
                "bullet then numbered",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style><text:list-style style:name="LN"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-number text:level="2" style:num-format="a"/></text:list-style>"##,
                r##"<text:list text:style-name="LB"><text:list-item><text:p>a</text:p></text:list-item></text:list><text:list text:style-name="LN"><text:list-item><text:p>b</text:p></text:list-item></text:list><text:list text:style-name="LB"><text:list-item><text:p>c</text:p></text:list-item></text:list>"##,
                "- a\n1. b\n- c\n",
                &[],
            ),
            (
                "separated lists",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style>"##,
                r##"<text:list text:style-name="LB"><text:list-item><text:p>a</text:p></text:list-item></text:list><text:p>between</text:p><text:list text:style-name="LB"><text:list-item><text:p>b</text:p></text:list-item></text:list>"##,
                "- a\n\nbetween\n\n- b\n",
                &[],
            ),
            (
                "task flavours",
                r##"<text:list-style style:name="LT"><text:list-level-style-bullet text:level="1" text:bullet-char="&#9744;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9745;"/></text:list-style>"##,
                r##"<text:list text:style-name="LT"><text:list-item><text:p>a</text:p></text:list-item></text:list><text:list text:style-name="LT"><text:list-item><text:p>b</text:p></text:list-item></text:list>"##,
                "- [ ] a\n* [ ] b\n",
                &[],
            ),
            (
                "bullet then task",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style><text:list-style style:name="LT"><text:list-level-style-bullet text:level="1" text:bullet-char="&#9744;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9745;"/></text:list-style>"##,
                r##"<text:list text:style-name="LB"><text:list-item><text:p>a</text:p></text:list-item></text:list><text:list text:style-name="LT"><text:list-item><text:p>b</text:p></text:list-item></text:list>"##,
                "- a\n- [ ] b\n",
                &[],
            ),
            (
                "continue numbering",
                r##"<text:list-style style:name="LN"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-number text:level="2" style:num-format="a"/></text:list-style>"##,
                r##"<text:list text:style-name="LN"><text:list-item><text:p>a</text:p></text:list-item><text:list-item><text:p>b</text:p></text:list-item></text:list><text:p>x</text:p><text:list text:style-name="LN" text:continue-numbering="true"><text:list-item><text:p>c</text:p></text:list-item></text:list><text:p>y</text:p><text:list text:style-name="LN" text:continue-list="id1"><text:list-item><text:p>d</text:p></text:list-item></text:list><text:p>z</text:p><text:list text:style-name="LN"><text:list-item><text:p>e</text:p></text:list-item></text:list>"##,
                "1. a\n2. b\n\nx\n\n3. c\n\ny\n\n4. d\n\nz\n\n1. e\n",
                &[],
            ),
            (
                "continue without previous",
                r##"<text:list-style style:name="LN"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-number text:level="2" style:num-format="a"/></text:list-style>"##,
                r##"<text:list text:style-name="LN" text:continue-numbering="true"><text:list-item><text:p>a</text:p></text:list-item></text:list>"##,
                "1. a\n",
                &[],
            ),
            (
                "multi paragraph item",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style>"##,
                r##"<text:list text:style-name="LB"><text:list-item><text:p>first</text:p><text:p>second</text:p></text:list-item></text:list>"##,
                "- first\n\n  second\n",
                &[],
            ),
            (
                "item with only nested",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style>"##,
                r##"<text:list text:style-name="LB"><text:list-item><text:list text:style-name="LB"><text:list-item><text:p>deep</text:p></text:list-item></text:list></text:list-item></text:list>"##,
                "    - deep\n",
                &[],
            ),
            (
                "empty items",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style>"##,
                r##"<text:list text:style-name="LB"><text:list-item><text:p></text:p></text:list-item><text:list-item></text:list-item><text:list-item><text:p>x</text:p></text:list-item></text:list>"##,
                "- x\n",
                &[],
            ),
            (
                "page breaks in items",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style><style:style style:name="PB" style:family="paragraph"><style:paragraph-properties fo:break-before="page"/></style:style><style:style style:name="PA" style:family="paragraph"><style:paragraph-properties fo:break-after="page"/></style:style>"##,
                r##"<text:list text:style-name="LB"><text:list-item><text:p text:style-name="PB">a</text:p></text:list-item><text:list-item><text:p>b</text:p><text:p text:style-name="PA">c</text:p></text:list-item><text:list-item><text:p text:style-name="PA">d</text:p></text:list-item></text:list>"##,
                "- a\n- b\n\n  c\n\n<!-- pagebreak -->\n\n- d\n",
                &[],
            ),
            (
                "list in paragraph flow",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style>"##,
                r##"<text:p>before</text:p><text:list text:style-name="LB"><text:list-item><text:p>a</text:p></text:list-item></text:list><text:p>after</text:p>"##,
                "before\n\n- a\n\nafter\n",
                &[],
            ),
            (
                "nested lists without text at outer levels",
                r##""##,
                r##"<text:list><text:list-item><text:list><text:list-item><text:list><text:list-item><text:p>deep</text:p></text:list-item></text:list></text:list-item></text:list></text:list-item></text:list>"##,
                "        - deep\n",
                &[],
            ),
            (
                "item with heading",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style>"##,
                r##"<text:list text:style-name="LB"><text:list-item><text:h text:outline-level="2">Head</text:h></text:list-item></text:list>"##,
                "- ## Head\n",
                &[],
            ),
            (
                "item with table",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style>"##,
                r##"<text:list text:style-name="LB"><text:list-item><text:p>t</text:p><table:table><table:table-row><table:table-cell><text:p>c</text:p></table:table-cell></table:table-row></table:table></text:list-item></text:list>"##,
                "- t\n\n  | c |\n  | --- |\n",
                &[],
            ),
        ]);
    }

    #[test]
    fn tables_cases() {
        check(&[
            (
                "simple",
                r##"<style:style style:name="B" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style>"##,
                r##"<table:table><table:table-column/><table:table-row><table:table-cell><text:p>h1</text:p></table:table-cell><table:table-cell><text:p><text:span text:style-name="B">h2</text:span></text:p></table:table-cell></table:table-row><table:table-row><table:table-cell><text:p>a</text:p></table:table-cell><table:table-cell><text:p><text:span text:style-name="B">b</text:span></text:p></table:table-cell></table:table-row></table:table>"##,
                "| h1 | **h2** |\n| --- | --- |\n| a | **b** |\n",
                &[],
            ),
            (
                "groups",
                r##""##,
                r##"<table:table><table:table-column/><table:table-header-rows><table:table-row><table:table-cell><text:p>h</text:p></table:table-cell></table:table-row></table:table-header-rows><table:table-rows><table:table-row><table:table-cell><text:p>r1</text:p></table:table-cell></table:table-row></table:table-rows><table:table-row-group><table:table-row><table:table-cell><text:p>r2</text:p></table:table-cell></table:table-row></table:table-row-group><table:table-columns><table:table-column/></table:table-columns><table:other/></table:table>"##,
                "| h |\n| --- |\n| r1 |\n| r2 |\n",
                &[],
            ),
            (
                "spans and covered",
                r##""##,
                r##"<table:table><table:table-column/><table:table-row><table:table-cell table:number-columns-spanned="2"><text:p>wide</text:p></table:table-cell><table:covered-table-cell></table:covered-table-cell><table:table-cell><text:p>x</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell table:number-rows-spanned="2"><text:p>tall</text:p></table:table-cell><table:table-cell table:number-columns-spanned="x"><text:p>b</text:p></table:table-cell><table:table-cell table:number-columns-spanned="1"><text:p>c</text:p></table:table-cell></table:table-row><table:table-row><table:covered-table-cell table:number-rows-spanned="2"></table:covered-table-cell><table:table-cell><text:p>d</text:p></table:table-cell><table:table-cell><text:p>e</text:p></table:table-cell></table:table-row></table:table>"##,
                "| wide |  | x |\n| --- | --- | --- |\n| tall | b | c |\n|  | d | e |\n",
                &[(LossKind::MergedCells, 2)],
            ),
            (
                "multi paragraph cell",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style>"##,
                r##"<table:table><table:table-column/><table:table-row><table:table-cell><text:p>a</text:p><text:p>b</text:p><text:list text:style-name="LB"><text:list-item><text:p>i1</text:p></text:list-item><text:list-item><text:p>i2</text:p></text:list-item></text:list><table:table><table:table-row><table:table-cell><text:p>n1</text:p></table:table-cell><table:table-cell><text:p>n2</text:p></table:table-cell></table:table-row></table:table><text:p></text:p></table:table-cell></table:table-row></table:table>"##,
                "| a<br>b<br>- i1<br>- i2<br>n1 n2 |\n| --- |\n",
                &[
                    (LossKind::NestedTables, 1),
                    (LossKind::MultiParagraphCells, 1),
                ],
            ),
            (
                "nested table",
                r##""##,
                r##"<table:table><table:table-column/><table:table-row><table:table-cell><table:table><table:table-column/><table:table-row><table:table-cell><text:p>in1</text:p></table:table-cell><table:table-cell><text:p>in2</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell><text:p>in3</text:p></table:table-cell><table:table-cell></table:table-cell></table:table-row></table:table><text:p>tail</text:p></table:table-cell></table:table-row></table:table>"##,
                "| in1 in2 in3<br>tail |\n| --- |\n",
                &[
                    (LossKind::NestedTables, 1),
                    (LossKind::MultiParagraphCells, 1),
                ],
            ),
            (
                "empty tables",
                r##""##,
                r##"<table:table><table:table-column/></table:table><table:table><table:table-column/><table:table-row></table:table-row></table:table><text:p>after</text:p>"##,
                "|  |\n| --- |\n\nafter\n",
                &[],
            ),
            (
                "cell with pipes breaks and code",
                r##"<style:style style:name="M" style:family="text"><style:text-properties style:font-name="Courier New"/></style:style><style:style style:name="Preformatted_20_Text" style:family="paragraph"/><style:style style:name="Quotations" style:family="paragraph"/><style:style style:name="PB" style:family="paragraph"><style:paragraph-properties fo:break-before="page"/></style:style>"##,
                r##"<table:table><table:table-column/><table:table-row><table:table-cell><text:p>a|b<text:line-break/>c <text:span text:style-name="M">d|e</text:span></text:p><text:p text:style-name="Preformatted_20_Text">pre</text:p><text:p text:style-name="Quotations">q</text:p><text:p text:style-name="PB">x</text:p></table:table-cell></table:table-row></table:table>"##,
                "| a\\|b<br>c `d\\|e`<br>pre<br>q<br>x |\n| --- |\n",
                &[(LossKind::MultiParagraphCells, 1)],
            ),
            (
                "header cell bold dropped",
                r##"<style:style style:name="B" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style><style:style style:name="PBold" style:family="paragraph"><style:text-properties fo:font-weight="bold" fo:font-style="italic"/></style:style>"##,
                r##"<table:table><table:table-column/><table:table-row><table:table-cell><text:p text:style-name="PBold">hb</text:p></table:table-cell><table:table-cell><text:p><text:span text:style-name="B">sp</text:span> t</text:p></table:table-cell></table:table-row></table:table>"##,
                "| *hb* | **sp** t |\n| --- | --- |\n",
                &[],
            ),
            (
                "table styles",
                r##"<style:style style:name="TB" style:family="table"><style:table-properties fo:break-before="page"/></style:style><style:style style:name="TA" style:family="table"><style:table-properties fo:break-after="page"/></style:style><style:style style:name="TM" style:family="table" style:master-page-name="Standard"/>"##,
                r##"<text:p>a</text:p><table:table table:style-name="TB"><table:table-column/><table:table-row><table:table-cell><text:p>1</text:p></table:table-cell></table:table-row></table:table><table:table table:style-name="TA"><table:table-column/><table:table-row><table:table-cell><text:p>2</text:p></table:table-cell></table:table-row></table:table><table:table table:style-name="TM"><table:table-column/><table:table-row><table:table-cell><text:p>3</text:p></table:table-cell></table:table-row></table:table><text:p>z</text:p>"##,
                "a\n\n<!-- pagebreak -->\n\n| 1 |\n| --- |\n\n| 2 |\n| --- |\n\n<!-- pagebreak -->\n\n| 3 |\n| --- |\n\nz\n",
                &[(LossKind::SectionBreaks, 1)],
            ),
            (
                "table first with master page",
                r##"<style:style style:name="TM" style:family="table" style:master-page-name="Standard"/>"##,
                r##"<table:table table:style-name="TM"><table:table-column/><table:table-row><table:table-cell><text:p>1</text:p></table:table-cell></table:table-row></table:table>"##,
                "| 1 |\n| --- |\n",
                &[],
            ),
            (
                "heading and break in cell",
                r##"<style:style style:name="PB" style:family="paragraph"><style:paragraph-properties fo:break-before="page"/></style:style>"##,
                r##"<table:table><table:table-column/><table:table-row><table:table-cell><text:h text:outline-level="1">h</text:h><text:p text:style-name="PB">t</text:p><text:p text:style-name="Quotations">q</text:p></table:table-cell></table:table-row></table:table>"##,
                "| h<br>t<br>q |\n| --- |\n",
                &[(LossKind::MultiParagraphCells, 1)],
            ),
            (
                "short rows",
                r##""##,
                r##"<table:table><table:table-column/><table:table-row><table:table-cell><text:p>a</text:p></table:table-cell><table:table-cell><text:p>b</text:p></table:table-cell><table:table-cell><text:p>c</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell><text:p>d</text:p></table:table-cell></table:table-row></table:table>"##,
                "| a | b | c |\n| --- | --- | --- |\n| d |  |  |\n",
                &[],
            ),
            (
                "unknown row children",
                r##""##,
                r##"<table:table><table:table-column/><table:table-row><table:table-cell><text:p>a</text:p></table:table-cell><table:unknown/></table:table-row></table:table>"##,
                "| a |\n| --- |\n",
                &[],
            ),
        ]);
    }

    #[test]
    fn misc_cases() {
        check(&[
            (
                "sections",
                r##"<style:style style:name="Sec2" style:family="section"><style:section-properties><style:columns fo:column-count="2"/></style:section-properties></style:style><style:style style:name="Sec1" style:family="section"><style:section-properties><style:columns fo:column-count="1"/></style:section-properties></style:style>"##,
                r##"<text:section text:style-name="Sec2"><text:p>in</text:p></text:section><text:section text:display="none"><text:p>hidden</text:p></text:section><text:section text:style-name="Sec1"><text:p>one</text:p></text:section><text:section><text:section><text:p>nested</text:p></text:section></text:section><text:section text:style-name="Missing"><text:p>nostyle</text:p></text:section>"##,
                "in\n\none\n\nnested\n\nnostyle\n",
                &[(LossKind::Columns, 1), (LossKind::HiddenText, 1)],
            ),
            (
                "tracked changes",
                r##""##,
                r##"<text:tracked-changes><text:changed-region><text:insertion/></text:changed-region><text:changed-region><text:deletion><text:p>deleted</text:p></text:deletion></text:changed-region><text:other/></text:tracked-changes><text:p>kept</text:p>"##,
                "kept\n",
                &[(LossKind::TrackedChanges, 2)],
            ),
            (
                "indexes",
                r##""##,
                r##"<text:table-of-content><text:index-body><text:p>toc</text:p></text:index-body></text:table-of-content><text:illustration-index/><text:table-index/><text:object-index/><text:user-index/><text:alphabetical-index/><text:bibliography/><text:p>kept</text:p>"##,
                "kept\n",
                &[(LossKind::Fields, 7)],
            ),
            (
                "unknown block elements",
                r##""##,
                r##"<text:sequence-decls/><office:forms/><text:unknown><text:p>not read</text:p></text:unknown><text:p>p</text:p>"##,
                "p\n",
                &[],
            ),
            (
                "links",
                r##"<style:style style:name="U" style:family="text"><style:text-properties style:text-underline-style="solid"/></style:style><style:style style:name="B" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style>"##,
                r##"<text:p><text:a xlink:href="http://e.com/a_b">site</text:a> <text:a xlink:href="  ">blank</text:a> <text:a xlink:href="http://x.org"></text:a> <text:a xlink:href="">none </text:a>end<text:a xlink:href="http://u">under <text:span text:style-name="U">u</text:span><text:span text:style-name="B">b</text:span></text:a></text:p>"##,
                "[site](http://e.com/a_b) blank none end[under u**b**](http://u)\n",
                &[],
            ),
            (
                "draw link",
                r##""##,
                r##"<text:p><draw:a xlink:href="http://d.org">drawn</draw:a></text:p>"##,
                "[drawn](http://d.org)\n",
                &[],
            ),
            (
                "link underline suppressed",
                r##"<style:style style:name="U" style:family="text"><style:text-properties style:text-underline-style="solid"/></style:style>"##,
                r##"<text:p><text:span text:style-name="U">plain u </text:span><text:a xlink:href="http://e.com">x</text:a></text:p>"##,
                "~plain u~ [x](http://e.com)\n",
                &[],
            ),
            (
                "anchors",
                r##"<style:style style:name="Heading_20_2" style:family="paragraph"/>"##,
                r##"<text:h text:outline-level="1">Intro</text:h><text:p>see <text:a xlink:href="#Intro|outline">intro</text:a> <text:a xlink:href="#Intro">same</text:a> <text:a xlink:href="#Missing">lost</text:a> <text:a xlink:href="#Sec%20Two">two</text:a> <text:a xlink:href="#bm1">bm</text:a> <text:a xlink:href="#Intro%7Cregion">pct</text:a></text:p><text:h text:outline-level="2">Sec Two</text:h><text:h text:outline-level="1">Intro</text:h><text:p><text:bookmark text:name="bm0"/>x</text:p><text:h text:outline-level="2"><text:bookmark-start text:name="bm1"/>With  bookmark<text:bookmark-end text:name="bm1"/></text:h><text:p text:style-name="Heading_20_2">Styled heading</text:p>"##,
                "# Intro\n\nsee [intro](#intro) [same](#intro) lost [two](#sec-two) [bm](#with-bookmark) [pct](#intro)\n\n## Sec Two\n\n# Intro\n\nx\n\n## With bookmark\n\n## Styled heading\n",
                &[(LossKind::Fields, 1)],
            ),
            (
                "anchors in nested structures",
                r##"<style:style style:name="Heading_20_2" style:family="paragraph"/>"##,
                r##"<text:list><text:list-item><text:h text:outline-level="2">Listed</text:h></text:list-item></text:list><text:section><text:h text:outline-level="2">Sectioned</text:h></text:section><table:table><table:table-column/><table:table-row><table:table-cell><text:h text:outline-level="2">Hidden in table</text:h></table:table-cell></table:table-row></table:table><text:p><text:a xlink:href="#Listed">a</text:a><text:a xlink:href="#Sectioned">b</text:a><text:a xlink:href="#Hidden in table">c</text:a></text:p>"##,
                "- ## Listed\n\n## Sectioned\n\n| Hidden in table |\n| --- |\n\n[a](#listed)[b](#sectioned)c\n",
                &[(LossKind::Fields, 1)],
            ),
            (
                "empty heading anchors",
                r##""##,
                r##"<text:h text:outline-level="1"></text:h><text:p><text:a xlink:href="#x">y</text:a></text:p>"##,
                "y\n",
                &[(LossKind::Fields, 1)],
            ),
            (
                "footnotes",
                r##""##,
                r##"<text:p>one<text:note text:note-class="footnote"><text:note-citation>1</text:note-citation><text:note-body><text:p>first note</text:p><text:p>second para</text:p></text:note-body></text:note> two<text:note text:note-class="endnote"><text:note-body><text:p text:style-name="PB">end <text:s/>note</text:p></text:note-body></text:note> three<text:note><text:note-citation>x</text:note-citation></text:note></text:p>"##,
                "one[^1] two[^2] three\n\n[^1]: first note\n\n    second para\n[^2]: end  note\n",
                &[],
            ),
            (
                "footnote with list",
                r##"<text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-bullet text:level="2" text:bullet-char="&#9702;"/></text:list-style>"##,
                r##"<text:p>x<text:note><text:note-body><text:list text:style-name="LB"><text:list-item><text:p>l1</text:p></text:list-item><text:list-item><text:p>l2</text:p></text:list-item></text:list></text:note-body></text:note></text:p>"##,
                "x[^1]\n\n[^1]: - l1\n    - l2\n",
                &[],
            ),
            (
                "empty footnote",
                r##""##,
                r##"<text:p>x<text:note><text:note-body/></text:note></text:p>"##,
                "x[^1]\n\n[^1]:\n",
                &[],
            ),
        ]);
    }

    #[test]
    fn frames_cases() {
        check(&[
            (
                "text box inline",
                r##""##,
                r##"<text:p>before<draw:frame><draw:text-box><text:p>boxed</text:p></draw:text-box></draw:frame>after</text:p>"##,
                "beforeafter\n\nboxed\n",
                &[(LossKind::TextBoxes, 1)],
            ),
            (
                "object math inline",
                r##""##,
                r##"<text:p>x<draw:frame><draw:object><math:math/></draw:object></draw:frame>y</text:p>"##,
                "xy\n",
                &[(LossKind::UnsupportedEquations, 1)],
            ),
            (
                "object without part",
                r##""##,
                r##"<text:p><draw:frame><draw:object xlink:href="./Obj"/></draw:frame><draw:frame><draw:object-ole/></draw:frame></text:p>"##,
                "",
                &[(LossKind::VectorShapes, 2)],
            ),
            (
                "empty frame",
                r##""##,
                r##"<text:p>a<draw:frame/>b</text:p>"##,
                "ab\n",
                &[(LossKind::VectorShapes, 1)],
            ),
            (
                "group",
                r##""##,
                r##"<text:p><draw:g><draw:frame/><draw:custom-shape><text:p>in group</text:p></draw:custom-shape></draw:g></text:p>"##,
                "in group\n",
                &[(LossKind::VectorShapes, 2)],
            ),
            (
                "3d scene",
                r##""##,
                r##"<text:p><dr3d:scene/>x</text:p>"##,
                "x\n",
                &[(LossKind::VectorShapes, 1)],
            ),
            (
                "shape text follows paragraph",
                r##""##,
                r##"<text:p>a<draw:custom-shape><text:p>shape text</text:p></draw:custom-shape>b</text:p><text:p>next</text:p>"##,
                "ab\n\nshape text\n\nnext\n",
                &[(LossKind::VectorShapes, 1)],
            ),
            (
                "floating",
                r##""##,
                r##"<draw:frame><draw:text-box><text:p>boxed</text:p></draw:text-box></draw:frame><draw:line/><text:p>p</text:p>"##,
                "boxed\n\np\n",
                &[(LossKind::TextBoxes, 1), (LossKind::VectorShapes, 1)],
            ),
            (
                "floating empty frame",
                r##""##,
                r##"<draw:frame/><text:p>p</text:p>"##,
                "p\n",
                &[(LossKind::VectorShapes, 1)],
            ),
        ]);
    }

    #[test]
    fn cases_with_package_parts() {
        let cases: &[PartCase<'_>] = &[
            ("floating image", r##"<draw:frame><draw:image xlink:href="Pictures/a.png"/></draw:frame>"##, &[("Pictures/a.png", b"\x89PNG\x0d\x0a\x1a\x0a1234")], "![](doc_assets/image-1.png)\n", &[], 1),
            ("objects with parts", r##"<text:p><draw:frame><draw:object xlink:href="./Obj1/"/></draw:frame><draw:frame><draw:object xlink:href="Obj2"/></draw:frame>x</text:p>"##, &[("Obj1/content.xml", b"<x>MathML</x>"), ("Obj2/content.xml", b"<x>chart</x>")], "x\n", &[(LossKind::VectorShapes, 1), (LossKind::UnsupportedEquations, 1)], 0),
            ("image", r##"<text:p>a <draw:frame svg:width="2in"><draw:image xlink:href="Pictures/a.png"/><svg:desc>alt [x]</svg:desc><svg:title>t</svg:title></draw:frame> b</text:p>"##, &[("Pictures/a.png", b"\x89PNG\x0d\x0a\x1a\x0a1234")], "a ![alt \\[x\\]|192](doc_assets/image-1.png) b\n", &[], 1),
            ("title alt, jpeg by magic, twice", r##"<text:p><draw:frame svg:width="1cm"><draw:image xlink:href="Pictures/b.dat"/><svg:title>The  title</svg:title></draw:frame><draw:frame><draw:image xlink:href="Pictures/b.dat"/></draw:frame><draw:frame><draw:image xlink:href="./Pictures/c.jpeg"/></draw:frame></text:p>"##, &[("Pictures/b.dat", b"\xff\xd8\xff\xe0abcd"), ("Pictures/c.jpeg", b"plain bytes")], "![The title|38](doc_assets/image-1.jpg)![](doc_assets/image-1.jpg)![](doc_assets/image-2.jpg)\n", &[], 2),
            ("unnamed ext", r##"<text:p><draw:frame><draw:image xlink:href="Pictures/x.dat"/></draw:frame><draw:frame><draw:image xlink:href="Pictures/noext"/></draw:frame><draw:frame><draw:image xlink:href="Pictures/x.toolongext"/></draw:frame><draw:frame><draw:image xlink:href="Pictures/y.p-g"/></draw:frame></text:p>"##, &[("Pictures/x.dat", b"plain bytes"), ("Pictures/noext", b"plain bytes"), ("Pictures/x.toolongext", b"plain bytes"), ("Pictures/y.p-g", b"plain bytes")], "![](doc_assets/image-1.bin)![](doc_assets/image-2.bin)![](doc_assets/image-3.bin)![](doc_assets/image-4.bin)\n", &[], 4),
            ("listed and unlisted names", r##"<text:p><draw:frame><draw:image xlink:href="Pictures/x.exe"/></draw:frame><draw:frame><draw:image xlink:href="Pictures/y.PNG"/></draw:frame></text:p>"##, &[("Pictures/x.exe", b"plain bytes"), ("Pictures/y.PNG", b"plain bytes")], "![](doc_assets/image-1.bin)![](doc_assets/image-2.png)\n", &[], 2),
            ("emf", r##"<text:p><draw:frame svg:width="100pt"><draw:image xlink:href="Pictures/e.emf"/></draw:frame></text:p>"##, &[("Pictures/e.emf", b"\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00 EMF")], "![|133](doc_assets/image-1.emf)\n", &[(LossKind::UnsupportedImages, 1)], 1),
            ("missing and external", r##"<text:p><draw:frame><draw:image xlink:href="Pictures/none.png"/></draw:frame> <draw:frame svg:width="50px"><draw:image xlink:href="http://e.com/x.png"/><svg:desc>ext</svg:desc></draw:frame> <draw:frame><draw:image xlink:href="../up/x.png"/></draw:frame> <draw:frame><draw:image xlink:href="/abs/x.png"/></draw:frame> <draw:frame><draw:image xlink:href=""/></draw:frame><draw:frame><draw:image xlink:href="Pictures/gone%20too.png"/></draw:frame></text:p>"##, &[], "![ext|50](http://e.com/x.png) ![](../up/x.png) ![](/abs/x.png)\n", &[(LossKind::CorruptParts, 3)], 0),
            ("percent", r##"<text:p><draw:frame><draw:image xlink:href="Pictures/a%20b.png"/></draw:frame><draw:frame><draw:image xlink:href="Pictures/a%20b.png"/></draw:frame></text:p>"##, &[("Pictures/a b.png", b"\x89PNG\x0d\x0a\x1a\x0a1234")], "![](doc_assets/image-1.png)![](doc_assets/image-1.png)\n", &[], 1),
            ("binary", r##"<text:p><draw:frame><draw:image><office:binary-data>iVBO
 Rw0K</office:binary-data></draw:image></draw:frame><draw:frame><draw:image><office:binary-data>!!!notbase64</office:binary-data></draw:image></draw:frame><draw:frame><draw:image><office:binary-data>iVBO Rw0K</office:binary-data></draw:image></draw:frame></text:p>"##, &[], "![](doc_assets/image-1.png)![](doc_assets/image-2.png)\n", &[(LossKind::CorruptParts, 1)], 2),
        ];
        for (label, body, parts, md, losses, assets) in cases {
            let result = convert_with("", body, parts);
            assert_eq!(result.markdown, *md, "{label}");
            assert_eq!(losses_of(&result), *losses, "{label}");
            assert_eq!(result.assets.len(), *assets, "{label}");
        }
    }

    #[test]
    fn column_alignment_is_the_majority_of_the_cells() {
        let auto = r##"<style:style style:name="PC" style:family="paragraph"><style:paragraph-properties fo:text-align="center"/></style:style><style:style style:name="PE" style:family="paragraph"><style:paragraph-properties fo:text-align="end"/></style:style><style:style style:name="PS" style:family="paragraph"><style:paragraph-properties fo:text-align="start"/></style:style><style:style style:name="PJ" style:family="paragraph"><style:paragraph-properties fo:text-align="justify"/></style:style>"##;
        let cell = |style: &str, text: &str| {
            format!(
                r#"<table:table-cell><text:p text:style-name="{style}">{text}</text:p></table:table-cell>"#
            )
        };
        let row =
            |cells: [String; 4]| format!("<table:table-row>{}</table:table-row>", cells.concat());
        let body = format!(
            "<table:table>{}{}{}</table:table>",
            row([
                cell("PC", "h1"),
                cell("PC", "h2"),
                cell("PE", "h3"),
                cell("PS", "h4")
            ]),
            row([
                cell("PC", "a"),
                "<table:table-cell><text:p>c</text:p></table:table-cell>".into(),
                cell("PE", "e"),
                cell("PS", "x")
            ]),
            row([
                cell("PC", "b"),
                "<table:table-cell><text:p>d</text:p></table:table-cell>".into(),
                cell("PJ", "f"),
                "<table:table-cell/>".into()
            ]),
        );
        let result = convert(auto, &body);
        assert_eq!(
            result.markdown,
            "| h1 | h2 | h3 | h4 |\n| :-: | --- | --: | :-- |\n| a | c | e | x |\n| b | d | f |  |\n"
        );
        assert_eq!(
            losses_of(&result),
            [(LossKind::Alignment, 1)],
            "only the justified cell cannot be a column alignment"
        );
        let outside = convert(auto, r#"<text:p text:style-name="PC">centered</text:p>"#);
        assert_eq!(losses_of(&outside), [(LossKind::Alignment, 1)]);
        let nested = format!(
            "<table:table><table:table-row><table:table-cell>{body}{}</table:table-cell></table:table-row></table:table>",
            r#"<text:p text:style-name="PE">after</text:p>"#
        );
        let result = convert(auto, &nested);
        assert!(
            result
                .markdown
                .starts_with("| h1 h2 h3 h4 a c e x b d f<br>after |\n| --: |"),
            "{}",
            result.markdown
        );
    }

    #[test]
    fn repeated_rows_and_columns_expand() {
        let body = r#"<table:table><table:table-row><table:table-cell table:number-columns-repeated="2"><text:p>a</text:p></table:table-cell></table:table-row><table:table-row table:number-rows-repeated="2"><table:table-cell><text:p>r</text:p></table:table-cell></table:table-row></table:table>"#;
        assert_eq!(
            convert("", body).markdown,
            "| a | a |\n| --- | --- |\n| r |  |\n| r |  |\n"
        );
    }

    #[test]
    fn package_images_keep_alt_text_and_width() {
        let png = b"\x89PNG\r\n\x1a\n1234";
        let body = r#"<text:p><draw:frame svg:width="2in"><draw:image xlink:href="Pictures/a.png"/><svg:desc>alt</svg:desc></draw:frame></text:p>"#;
        let result = convert_with("", body, &[("Pictures/a.png", png)]);
        assert_eq!(result.markdown, "![alt|192](doc_assets/image-1.png)\n");
        assert_eq!(
            result.assets,
            [Asset {
                relative_path: "doc_assets/image-1.png".into(),
                bytes: png.to_vec()
            }]
        );
    }
}
