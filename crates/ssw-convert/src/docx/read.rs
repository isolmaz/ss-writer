//! DOCX → Markdown (`ImportResult`).

use std::collections::HashMap;

use ssw_core::markdown::ast::HighlightColor;

use crate::import::{Asset, ImportError, ImportResult, LossKind};
use crate::omml;
use crate::ooxml::{
    Container, EMU_PER_PX, Element, Ns, Package, PackageError, Relationships, on_off,
    parse_xml_salvage, rels_path_for, resolve_target, sniff_container,
};

use super::numbering::{Counters, Kind, Numbering};
use super::styles::{NumRef, RunProps, Styles, outline_to_level, read_num_pr};

// ---------------------------------------------------------------------------------------------
// Inline model and Markdown rendering
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Fmt {
    bold: bool,
    italic: bool,
    strike: bool,
    underline: bool,
    highlight: Option<HighlightColor>,
    code: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Inl {
    Text {
        text: String,
        fmt: Fmt,
    },
    Break,
    PageBreak,
    Rule,
    Link {
        url: String,
        children: Vec<Inl>,
    },
    Image {
        alt: String,
        dest: String,
        width: Option<u32>,
    },
    NoteRef(String),
    Math {
        tex: String,
        display: bool,
    },
    Raw(String),
}

/// Marks from the outermost to the innermost wrapper.
const MARK_LEVELS: usize = 5;

fn mark_open(fmt: &Fmt, level: usize) -> Option<String> {
    match level {
        0 => fmt.highlight.map(|c| {
            if c == HighlightColor::Yellow {
                "==".to_string()
            } else {
                format!("=={{{}}}", c.name())
            }
        }),
        1 => fmt.strike.then(|| "~~".to_string()),
        2 => fmt.underline.then(|| "~".to_string()),
        3 => fmt.bold.then(|| "**".to_string()),
        _ => fmt.italic.then(|| "*".to_string()),
    }
}

fn mark_close(level: usize) -> &'static str {
    match level {
        0 => "==",
        1 => "~~",
        2 => "~",
        3 => "**",
        _ => "*",
    }
}

fn is_space(c: char) -> bool {
    c == ' ' || c == '\u{a0}'
}

fn wrap_mark(open: &str, close: &str, inner: &str) -> String {
    let core = inner.trim_matches(is_space);
    if core.is_empty() {
        return inner.to_string();
    }
    let start = inner.len() - inner.trim_start_matches(is_space).len();
    let end = inner.trim_end_matches(is_space).len();
    format!("{}{open}{core}{close}{}", &inner[..start], &inner[end..])
}

fn entity_like(rest: &[char]) -> bool {
    match rest.first() {
        Some('#') => rest
            .get(1)
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, 'x' | 'X')),
        Some(c) if c.is_ascii_alphanumeric() => {
            let name_len = rest
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric())
                .count();
            name_len <= 32 && rest.get(name_len) == Some(&';')
        }
        _ => false,
    }
}

/// Escapes characters that would be read as inline Markdown syntax.
fn escape_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1).copied();
        let escape = match c {
            '\\' | '`' | '*' | '[' | ']' | '~' | '$' => true,
            '_' => {
                !(prev.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric))
            }
            '<' => next.is_some_and(|n| n.is_ascii_alphabetic() || matches!(n, '/' | '!' | '?')),
            '=' => prev == Some('=') || next == Some('='),
            '&' => entity_like(&chars[i + 1..]),
            _ => false,
        };
        if escape {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn code_span(text: &str) -> String {
    let text = text.replace('\n', " ");
    if text.is_empty() {
        return String::new();
    }
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest + 1);
    if text.starts_with(['`', ' ']) || text.ends_with(['`', ' ']) {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// Link destination, in angle brackets when it contains characters that end a bare destination.
fn md_dest(dest: &str) -> String {
    if !dest.is_empty() && !dest.contains([' ', '(', ')', '<', '>', '\n', '\t']) {
        return dest.to_string();
    }
    let encoded: String = dest
        .chars()
        .map(|c| match c {
            '<' => "%3C".to_string(),
            '>' => "%3E".to_string(),
            '\n' | '\t' => "%20".to_string(),
            other => other.to_string(),
        })
        .collect();
    format!("<{encoded}>")
}

fn render_atom(inl: &Inl) -> String {
    match inl {
        Inl::Text { .. } | Inl::PageBreak | Inl::Rule => String::new(),
        Inl::Break => "\\\n".to_string(),
        Inl::Link { url, children } => {
            format!("[{}]({})", render_inlines(children), md_dest(url))
        }
        Inl::Image { alt, dest, width } => {
            let alt = alt.replace(['\n', '\r'], " ");
            let alt = alt
                .replace('\\', "\\\\")
                .replace('[', "\\[")
                .replace(']', "\\]")
                .replace('|', "\\|");
            match width {
                Some(w) => format!("![{alt}|{w}]({})", md_dest(dest)),
                None => format!("![{alt}]({})", md_dest(dest)),
            }
        }
        Inl::NoteRef(label) => format!("[^{label}]"),
        Inl::Math { tex, display } => {
            let tex = tex.trim();
            if tex.is_empty() {
                String::new()
            } else if *display {
                format!("$${tex}$$")
            } else {
                format!("${tex}$")
            }
        }
        Inl::Raw(raw) => raw.clone(),
    }
}

fn render_group(items: &[(Fmt, String, bool)], level: usize) -> String {
    if level == MARK_LEVELS {
        return items
            .iter()
            .map(|(fmt, text, is_text)| match (*is_text, fmt.code) {
                (true, true) => code_span(text),
                (true, false) => escape_text(text),
                (false, _) => text.clone(),
            })
            .collect();
    }
    let mut out = String::new();
    let mut i = 0;
    while i < items.len() {
        let key = mark_open(&items[i].0, level);
        let mut j = i + 1;
        while j < items.len() && mark_open(&items[j].0, level) == key {
            j += 1;
        }
        let inner = render_group(&items[i..j], level + 1);
        match key {
            Some(open) => out.push_str(&wrap_mark(&open, mark_close(level), &inner)),
            None => out.push_str(&inner),
        }
        i = j;
    }
    out
}

/// Renders inline nodes to Markdown: merges equal formatting, nests marks, moves boundary
/// whitespace outside the markers and escapes syntax characters.
fn render_inlines(inlines: &[Inl]) -> String {
    let mut merged: Vec<Inl> = Vec::new();
    for inl in inlines {
        if let Inl::Text { text, fmt } = inl
            && let Some(Inl::Text {
                text: previous,
                fmt: previous_fmt,
            }) = merged.last_mut()
            && previous_fmt == fmt
        {
            previous.push_str(text);
            continue;
        }
        merged.push(inl.clone());
    }
    let items: Vec<(Fmt, String, bool)> = merged
        .iter()
        .map(|inl| match inl {
            Inl::Text { text, fmt } => (fmt.clone(), text.clone(), true),
            other => (Fmt::default(), render_atom(other), false),
        })
        .collect();
    render_group(&items, 0)
}

/// Escapes block-level syntax at the start of one paragraph line.
fn escape_line_start(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let first = chars.first().copied();
    let after = |n: usize| chars.get(n).copied();
    let blank_or_end = |n: usize| after(n).is_none_or(|c| c == ' ');
    let hashes = chars.iter().take_while(|c| **c == '#').count();
    let digits = chars.iter().take_while(|c| c.is_ascii_digit()).count();
    let escape_first = match first {
        Some('#') => hashes <= 6 && blank_or_end(hashes),
        Some('>') => true,
        Some('-' | '+') => blank_or_end(1) || line.starts_with("---"),
        _ => false,
    };
    if escape_first {
        return format!("\\{line}");
    }
    if (1..=9).contains(&digits)
        && matches!(after(digits), Some('.' | ')'))
        && blank_or_end(digits + 1)
    {
        let head: String = chars[..digits].iter().collect();
        let tail: String = chars[digits..].iter().collect();
        return format!("{head}\\{tail}");
    }
    line.to_string()
}

/// Trims every line of a rendered paragraph and escapes block syntax at line starts.
fn escape_lines(rendered: &str) -> String {
    rendered
        .split('\n')
        .map(|line| escape_line_start(line.trim_matches(is_space)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Hard breaks become spaces (headings, table cells).
fn flatten_breaks(rendered: &str) -> String {
    rendered.replace("\\\n", " ").replace('\n', " ")
}

fn escape_heading(text: &str) -> String {
    match text.strip_suffix('#') {
        Some(rest) => format!("{rest}\\#"),
        None => text.to_string(),
    }
}

/// GitHub-style heading anchor.
pub(super) fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            slug.push(c);
        } else if c.is_whitespace() {
            slug.push('-');
        }
    }
    slug
}

fn plain_code_text(inlines: &[Inl]) -> String {
    let mut out = String::new();
    for inl in inlines {
        match inl {
            Inl::Text { text, .. } => out.push_str(text),
            Inl::Break => out.push('\n'),
            _ => {}
        }
    }
    out
}

fn has_visible(inlines: &[Inl]) -> bool {
    inlines.iter().any(|i| match i {
        Inl::Text { text, .. } => !text.trim().is_empty(),
        Inl::PageBreak | Inl::Break => false,
        _ => true,
    })
}

// ---------------------------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    Heading(u8, String),
    Para(String),
    Item(String),
    Quote(String),
    Code(String),
    Math(String),
    Table(String),
    Rule,
    PageBreak,
}

/// Appends a block, merging adjacent code lines and quote paragraphs.
fn push_block(out: &mut Vec<Block>, block: Block) {
    match (out.last_mut(), block) {
        (Some(Block::Code(previous)), Block::Code(text)) => {
            previous.push('\n');
            previous.push_str(&text);
        }
        (Some(Block::Quote(previous)), Block::Quote(text)) => {
            previous.push_str("\n\n");
            previous.push_str(&text);
        }
        (_, block) => out.push(block),
    }
}

fn render_blocks(blocks: &[Block]) -> String {
    let mut out = String::new();
    let mut previous: Option<&Block> = None;
    for block in blocks {
        let text = match block {
            Block::Heading(level, text) => format!("{} {text}", "#".repeat(usize::from(*level))),
            Block::Para(text) | Block::Item(text) | Block::Table(text) => text.clone(),
            Block::Quote(text) => text
                .split('\n')
                .map(|line| {
                    if line.is_empty() {
                        ">".to_string()
                    } else {
                        format!("> {line}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Block::Code(text) => {
                let mut fence = "```".to_string();
                while text.contains(&fence) {
                    fence.push('`');
                }
                format!("{fence}\n{text}\n{fence}")
            }
            Block::Math(tex) => format!("$$\n{tex}\n$$"),
            Block::Rule => {
                if out.is_empty() {
                    "***".to_string()
                } else {
                    "---".to_string()
                }
            }
            Block::PageBreak => "<!-- pagebreak -->".to_string(),
        };
        if let Some(prev) = previous {
            let tight = matches!((prev, block), (Block::Item(_), Block::Item(_)));
            out.push_str(if tight { "\n" } else { "\n\n" });
        }
        out.push_str(&text);
        previous = Some(block);
    }
    out
}

enum Piece {
    Inlines(Vec<Inl>),
    PageBreak,
    Rule,
    DisplayMath(String),
}

/// Splits a paragraph at page breaks, rules and display equations.
fn split_inlines(inlines: Vec<Inl>) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut current = Vec::new();
    for inl in inlines {
        let special = match &inl {
            Inl::PageBreak => Some(Piece::PageBreak),
            Inl::Rule => Some(Piece::Rule),
            Inl::Math { tex, display: true } => Some(Piece::DisplayMath(tex.trim().to_string())),
            _ => None,
        };
        match special {
            Some(piece) => {
                pieces.push(Piece::Inlines(std::mem::take(&mut current)));
                pieces.push(piece);
            }
            None => current.push(inl),
        }
    }
    pieces.push(Piece::Inlines(current));
    pieces
}

// ---------------------------------------------------------------------------------------------
// Converter
// ---------------------------------------------------------------------------------------------

/// Everything the converter needs besides the document body.
#[derive(Default)]
struct Setup {
    rels: Relationships,
    /// Name of the main document part; relationship targets are relative to it.
    main_part: String,
    styles: Styles,
    numbering: Numbering,
    footnotes: HashMap<String, Element>,
    endnotes: HashMap<String, Element>,
    /// Relationships of the footnotes / endnotes parts (hyperlinks and images inside notes).
    footnote_rels: Relationships,
    endnote_rels: Relationships,
    stem: String,
}

struct Field {
    instr: String,
    in_result: bool,
    para: usize,
    result_start: usize,
}

type Media<'m> = &'m mut dyn FnMut(&str) -> Option<Vec<u8>>;

struct Converter<'m> {
    setup: Setup,
    media: Media<'m>,
    result: ImportResult,
    counters: Counters,
    anchors: HashMap<String, String>,
    images: HashMap<String, String>,
    image_count: usize,
    note_count: usize,
    note_defs: Vec<String>,
    fields: Vec<Field>,
    para_seq: usize,
    pending: Vec<Block>,
    list_run: usize,
    last_list: Option<String>,
    list_widths: Vec<usize>,
    in_cell: bool,
}

fn val(el: &Element) -> Option<&str> {
    el.attr("val")
}

fn choice(alternate: &Element) -> Option<&Element> {
    alternate
        .child(Ns::Mc, "Choice")
        .or_else(|| alternate.child(Ns::Mc, "Fallback"))
}

/// Outermost `w:txbxContent` elements below `el`.
fn text_boxes<'a>(el: &'a Element, out: &mut Vec<&'a Element>) {
    for child in el.elements() {
        if child.is(Ns::W, "txbxContent") {
            out.push(child);
        } else {
            text_boxes(child, out);
        }
    }
}

fn hyperlink_target(instr: &str) -> Option<String> {
    let mut quoted = Vec::new();
    let mut rest = instr;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { break };
        quoted.push(&after[..end]);
        rest = &after[end + 1..];
    }
    let target = if instr.contains("\\l") {
        let anchor = quoted.last().copied().unwrap_or("");
        let url = if quoted.len() >= 2 { quoted[0] } else { "" };
        format!("{url}#{anchor}")
    } else if let Some(url) = quoted.first() {
        (*url).to_string()
    } else {
        instr.split_whitespace().nth(1).unwrap_or("").to_string()
    };
    (!target.is_empty() && target != "#").then_some(target)
}

const UNSUPPORTED_IMAGE_EXTENSIONS: [&str; 6] = ["emf", "wmf", "emz", "wmz", "pict", "wdp"];
const VML_SHAPES: [&str; 9] = [
    "shape",
    "rect",
    "roundrect",
    "oval",
    "line",
    "curve",
    "polyline",
    "group",
    "arc",
];

impl<'m> Converter<'m> {
    fn new(setup: Setup, media: Media<'m>) -> Self {
        Converter {
            setup,
            media,
            result: ImportResult::default(),
            counters: Counters::default(),
            anchors: HashMap::new(),
            images: HashMap::new(),
            image_count: 0,
            note_count: 0,
            note_defs: Vec::new(),
            fields: Vec::new(),
            para_seq: 0,
            pending: Vec::new(),
            list_run: 0,
            last_list: None,
            list_widths: Vec::new(),
            in_cell: false,
        }
    }

    fn loss(&mut self, kind: LossKind, count: usize) {
        self.result.record_loss(kind, count, None);
    }

    /// Converts the `w:document` element.
    fn run(mut self, document: &Element) -> ImportResult {
        self.scan_anchors(document);
        let mut blocks = Vec::new();
        if let Some(body) = document.child(Ns::W, "body") {
            self.blocks(body, &mut blocks);
            if let Some(sect) = body.child(Ns::W, "sectPr") {
                self.columns(sect);
            }
        }
        let mut markdown = render_blocks(&blocks);
        if !self.note_defs.is_empty() {
            if !markdown.is_empty() {
                markdown.push_str("\n\n");
            }
            markdown.push_str(&self.note_defs.join("\n\n"));
        }
        if !markdown.is_empty() {
            markdown.push('\n');
        }
        self.result.markdown = markdown;
        self.result
    }

    fn columns(&mut self, sect: &Element) {
        let columns = sect
            .child(Ns::W, "cols")
            .and_then(|c| c.attr_ns(Ns::W, "num"))
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(1);
        if columns > 1 {
            self.loss(LossKind::Columns, 1);
        }
    }

    /// Maps bookmark names inside headings to the heading's anchor.
    fn scan_anchors(&mut self, document: &Element) {
        let mut used: HashMap<String, usize> = HashMap::new();
        for p in document.descendants(Ns::W, "p") {
            let ppr = p.child(Ns::W, "pPr");
            let style = ppr
                .and_then(|x| x.child(Ns::W, "pStyle"))
                .and_then(val)
                .unwrap_or("");
            let outline = ppr
                .and_then(|x| x.child(Ns::W, "outlineLvl"))
                .and_then(val)
                .and_then(|v| v.parse::<u8>().ok());
            let level = match outline {
                Some(o) => outline_to_level(o),
                None => self.setup.styles.heading_level(style),
            };
            if level.is_none() {
                continue;
            }
            let marks = p.descendants(Ns::W, "bookmarkStart");
            if marks.is_empty() {
                continue;
            }
            let text: String = p.descendants(Ns::W, "t").iter().map(|t| t.text()).collect();
            let base = slugify(text.trim());
            let count = used.entry(base.clone()).or_insert(0);
            let slug = if *count == 0 {
                base.clone()
            } else {
                format!("{base}-{count}")
            };
            *count += 1;
            for mark in marks {
                if let Some(name) = mark.attr_ns(Ns::W, "name") {
                    self.anchors.insert(name.to_string(), slug.clone());
                }
            }
        }
    }

    fn anchor_url(&self, anchor: &str) -> String {
        let slug = self.anchors.get(anchor).map_or(anchor, String::as_str);
        format!("#{slug}")
    }

    /// Rewrites `#bookmark` destinations to heading anchors.
    fn resolve_url(&self, url: String) -> String {
        match url.strip_prefix('#') {
            Some(anchor) => self.anchor_url(anchor),
            None => url,
        }
    }

    // ---- blocks ------------------------------------------------------------------------------

    fn blocks(&mut self, parent: &Element, out: &mut Vec<Block>) {
        for child in parent.elements() {
            match (child.ns, child.name.as_str()) {
                (Ns::W, "p") => self.paragraph(child, out),
                (Ns::W, "tbl") => self.table(child, out),
                (Ns::W, "sdt") => {
                    if let Some(content) = child.child(Ns::W, "sdtContent") {
                        self.blocks(content, out);
                    }
                }
                (Ns::W, "sdtContent" | "customXml" | "smartTag") => self.blocks(child, out),
                (Ns::W, "ins" | "moveTo") => {
                    self.loss(LossKind::TrackedChanges, 1);
                    self.blocks(child, out);
                }
                (Ns::W, "del" | "moveFrom") => self.loss(LossKind::TrackedChanges, 1),
                (Ns::Mc, "AlternateContent") => {
                    if let Some(chosen) = choice(child) {
                        self.blocks(chosen, out);
                    }
                }
                _ => {}
            }
        }
    }

    fn section_break(&mut self, sect: &Element) {
        self.loss(LossKind::SectionBreaks, 1);
        self.columns(sect);
    }

    fn paragraph(&mut self, p: &Element, out: &mut Vec<Block>) {
        let ppr = p.child(Ns::W, "pPr");
        let style = ppr
            .and_then(|x| x.child(Ns::W, "pStyle"))
            .and_then(val)
            .unwrap_or("")
            .to_string();
        let direct_num = ppr.and_then(read_num_pr);
        let outline = ppr
            .and_then(|x| x.child(Ns::W, "outlineLvl"))
            .and_then(val)
            .and_then(|v| v.parse::<u8>().ok());
        let justification = ppr.and_then(|x| x.child(Ns::W, "jc")).and_then(val);
        let page_break_before = ppr
            .and_then(|x| x.child(Ns::W, "pageBreakBefore"))
            .is_some_and(|e| on_off(val(e)));
        if page_break_before {
            push_block(out, Block::PageBreak);
        }
        if let Some(x) = ppr {
            if x.child(Ns::W, "pPrChange").is_some() {
                self.loss(LossKind::TrackedChanges, 1);
            }
            if let Some(sect) = x.child(Ns::W, "sectPr") {
                self.section_break(sect);
            }
        }
        self.para_seq += 1;
        let mut inlines = Vec::new();
        self.inline_children(p, &mut inlines, false);
        let pending = std::mem::take(&mut self.pending);

        let level = match outline {
            Some(o) => outline_to_level(o),
            None => self.setup.styles.heading_level(&style),
        };
        let all_code = !inlines.is_empty()
            && inlines
                .iter()
                .all(|i| matches!(i, Inl::Text { fmt, .. } if fmt.code));
        let is_code = self.setup.styles.is_code_paragraph(&style) || all_code;
        let is_quote = self.setup.styles.is_quote_paragraph(&style);
        let num = match direct_num {
            Some(n) => n,
            None => self.setup.styles.num_ref(&style).flatten(),
        };
        if !self.in_cell
            && matches!(justification, Some("center" | "right" | "end"))
            && has_visible(&inlines)
        {
            self.loss(LossKind::Alignment, 1);
        }
        let inlines = Self::display_single_math(inlines);

        let mut first_text = true;
        for piece in split_inlines(inlines) {
            match piece {
                Piece::PageBreak => push_block(out, Block::PageBreak),
                Piece::Rule => push_block(out, Block::Rule),
                Piece::DisplayMath(tex) => push_block(out, Block::Math(tex)),
                Piece::Inlines(list) => {
                    if !has_visible(&list) {
                        continue;
                    }
                    if is_code {
                        push_block(out, Block::Code(plain_code_text(&list)));
                        continue;
                    }
                    let rendered = render_inlines(&list);
                    if let Some(level) = level {
                        let text = flatten_breaks(&rendered);
                        let text = escape_heading(text.trim_matches(is_space));
                        push_block(out, Block::Heading(level, text));
                        first_text = false;
                        continue;
                    }
                    let text = escape_lines(&rendered);
                    if text.is_empty() {
                        continue;
                    }
                    if is_quote {
                        push_block(out, Block::Quote(text));
                    } else if let Some(item) = self.list_item(num.as_ref(), &text, out, first_text)
                    {
                        push_block(out, item);
                    } else {
                        push_block(out, Block::Para(text));
                    }
                    first_text = false;
                }
            }
        }
        for block in pending {
            push_block(out, block);
        }
    }

    /// A paragraph consisting only of one inline equation is a display equation.
    fn display_single_math(inlines: Vec<Inl>) -> Vec<Inl> {
        let visible = inlines
            .iter()
            .filter(|i| has_visible(std::slice::from_ref(i)))
            .count();
        if visible != 1 {
            return inlines;
        }
        inlines
            .into_iter()
            .map(|inl| match inl {
                Inl::Math { tex, .. } => Inl::Math { tex, display: true },
                other => other,
            })
            .collect()
    }

    /// The list item block for `text` when the paragraph is a list item.
    fn list_item(
        &mut self,
        num: Option<&NumRef>,
        text: &str,
        out: &[Block],
        first_text: bool,
    ) -> Option<Block> {
        let num = num.filter(|_| first_text)?;
        let level = self.setup.numbering.level(&num.num_id, num.level)?;
        if level.kind == Kind::None {
            return None;
        }
        let previous_is_item = matches!(out.last(), Some(Block::Item(_)));
        if !previous_is_item || self.last_list.as_deref() != Some(num.num_id.as_str()) {
            self.list_run += 1;
        }
        self.last_list = Some(num.num_id.clone());
        let parity = (self.list_run + 1) % 2;
        let base = match level.kind {
            Kind::Ordered => {
                if !level.decimal {
                    self.loss(LossKind::ListNumberingStyle, 1);
                }
                let number = self.counters.next(&num.num_id, num.level, level.start);
                let delimiter = if parity == 0 { '.' } else { ')' };
                format!("{number}{delimiter}")
            }
            _ => if parity == 0 { "-" } else { "*" }.to_string(),
        };
        let marker = match level.task() {
            Some(true) => format!("{base} [x]"),
            Some(false) => format!("{base} [ ]"),
            None => base.clone(),
        };
        let depth = usize::from(num.level);
        self.list_widths.truncate(depth);
        while self.list_widths.len() < depth {
            self.list_widths.push(2);
        }
        let indent: usize = self.list_widths.iter().sum();
        let width = base.chars().count() + 1;
        self.list_widths.push(width);
        let pad = " ".repeat(indent);
        let continuation = " ".repeat(indent + width);
        let mut lines = text.split('\n');
        let mut item = format!("{pad}{marker} {}", lines.next().unwrap_or(""));
        for line in lines {
            item.push('\n');
            item.push_str(&continuation);
            item.push_str(line);
        }
        Some(Block::Item(item))
    }

    // ---- inline content ------------------------------------------------------------------------

    fn in_instruction(&self) -> bool {
        self.fields.last().is_some_and(|f| !f.in_result)
    }

    fn inline_children(&mut self, parent: &Element, out: &mut Vec<Inl>, in_link: bool) {
        for child in parent.elements() {
            match (child.ns, child.name.as_str()) {
                (Ns::W, "r") => self.run_element(child, out, in_link),
                (Ns::W, "hyperlink") => self.hyperlink(child, out),
                (Ns::W, "fldSimple") => self.simple_field(child, out, in_link),
                (Ns::W, "ins" | "moveTo") => {
                    self.loss(LossKind::TrackedChanges, 1);
                    self.inline_children(child, out, in_link);
                }
                (Ns::W, "del" | "moveFrom") => self.loss(LossKind::TrackedChanges, 1),
                (Ns::W, "smartTag" | "customXml" | "bdo" | "dir" | "sdtContent") => {
                    self.inline_children(child, out, in_link);
                }
                (Ns::W, "sdt") => {
                    if let Some(content) = child.child(Ns::W, "sdtContent") {
                        self.inline_children(content, out, in_link);
                    }
                }
                (Ns::M, "oMathPara") => self.math(child, true, out),
                (Ns::M, "oMath") => self.math(child, false, out),
                (Ns::Mc, "AlternateContent") => {
                    if let Some(chosen) = choice(child) {
                        self.inline_children(chosen, out, in_link);
                    }
                }
                _ => {}
            }
        }
    }

    fn run_element(&mut self, r: &Element, out: &mut Vec<Inl>, in_link: bool) {
        let mut props = RunProps::default();
        if let Some(rpr) = r.child(Ns::W, "rPr") {
            let direct = RunProps::from_rpr(rpr);
            if let Some(style) = &direct.style {
                props.overlay(&self.setup.styles.char_props(style));
            }
            props.overlay(&direct);
            if rpr.child(Ns::W, "rPrChange").is_some() {
                self.loss(LossKind::TrackedChanges, 1);
            }
        }
        self.run_content(r, &props, out, in_link);
    }

    fn emit_text(&mut self, text: &mut String, fmt: &Fmt, props: &RunProps, out: &mut Vec<Inl>) {
        if text.is_empty() {
            return;
        }
        let raw = std::mem::take(text);
        let cleaned = raw.replace(['\n', '\r', '\t'], " ");
        if props.sup_sub == Some(true) {
            self.loss(LossKind::SubSuperscript, 1);
        }
        if props.colored || props.shaded {
            self.loss(LossKind::FontsAndColors, 1);
        }
        out.push(Inl::Text {
            text: cleaned,
            fmt: fmt.clone(),
        });
    }

    fn run_content(&mut self, el: &Element, props: &RunProps, out: &mut Vec<Inl>, in_link: bool) {
        if props.hidden == Some(true) {
            if !el.descendants(Ns::W, "t").is_empty() {
                self.loss(LossKind::HiddenText, 1);
            }
            return;
        }
        let fmt = Fmt {
            bold: props.bold == Some(true),
            italic: props.italic == Some(true),
            strike: props.strike == Some(true),
            underline: props.underline == Some(true) && !in_link,
            highlight: props.highlight,
            code: props.mono,
        };
        let mut text = String::new();
        for child in el.elements() {
            match (child.ns, child.name.as_str()) {
                (Ns::W, "t") => {
                    if !self.in_instruction() {
                        text.push_str(&child.text());
                    }
                }
                (Ns::W, "tab" | "ptab") => text.push(' '),
                (Ns::W, "noBreakHyphen") => text.push('-'),
                (Ns::W, "br" | "cr") => {
                    self.emit_text(&mut text, &fmt, props, out);
                    match child.attr_ns(Ns::W, "type") {
                        Some("page") => out.push(Inl::PageBreak),
                        Some("column") => {}
                        _ => out.push(Inl::Break),
                    }
                }
                (Ns::W, "instrText") => {
                    if let Some(field) = self.fields.last_mut()
                        && !field.in_result
                    {
                        field.instr.push_str(&child.text());
                    }
                }
                (Ns::W, "fldChar") => {
                    self.emit_text(&mut text, &fmt, props, out);
                    self.field_char(child, out);
                }
                (Ns::W, "sym") => self.loss(LossKind::SymbolFontCharacters, 1),
                (Ns::W, "footnoteReference") => {
                    self.emit_text(&mut text, &fmt, props, out);
                    self.note_ref(child, false, out);
                }
                (Ns::W, "endnoteReference") => {
                    self.emit_text(&mut text, &fmt, props, out);
                    self.note_ref(child, true, out);
                }
                (Ns::W, "drawing") => {
                    self.emit_text(&mut text, &fmt, props, out);
                    self.drawing(child, out);
                }
                (Ns::W, "pict" | "object") => {
                    self.emit_text(&mut text, &fmt, props, out);
                    self.pict(child, out);
                }
                (Ns::W, "ruby") => {
                    self.emit_text(&mut text, &fmt, props, out);
                    if let Some(base) = child.child(Ns::W, "rubyBase") {
                        for run in base.elements().filter(|e| e.is(Ns::W, "r")) {
                            self.run_element(run, out, in_link);
                        }
                    }
                }
                (Ns::Mc, "AlternateContent") => {
                    self.emit_text(&mut text, &fmt, props, out);
                    if let Some(chosen) = choice(child) {
                        self.run_content(chosen, props, out, in_link);
                    }
                }
                _ => {}
            }
        }
        self.emit_text(&mut text, &fmt, props, out);
    }

    fn field_char(&mut self, el: &Element, out: &mut Vec<Inl>) {
        match el.attr_ns(Ns::W, "fldCharType") {
            Some("begin") => self.fields.push(Field {
                instr: String::new(),
                in_result: false,
                para: self.para_seq,
                result_start: out.len(),
            }),
            Some("separate") => {
                if let Some(field) = self.fields.last_mut() {
                    field.in_result = true;
                    field.result_start = out.len();
                }
            }
            Some("end") => {
                if let Some(field) = self.fields.pop() {
                    self.finish_field(&field, out);
                }
            }
            _ => {}
        }
    }

    fn finish_field(&mut self, field: &Field, out: &mut Vec<Inl>) {
        let instr = field.instr.trim();
        let kind = instr
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if kind == "HYPERLINK" {
            if field.in_result
                && field.para == self.para_seq
                && field.result_start <= out.len()
                && let Some(url) = hyperlink_target(instr)
            {
                let children = out.split_off(field.result_start);
                if !children.is_empty() {
                    let url = self.resolve_url(url);
                    out.push(Inl::Link { url, children });
                }
            }
        } else if !kind.is_empty() {
            self.result.record_loss(LossKind::Fields, 1, Some(kind));
        }
    }

    fn simple_field(&mut self, el: &Element, out: &mut Vec<Inl>, in_link: bool) {
        let instr = el.attr_ns(Ns::W, "instr").unwrap_or("").trim().to_string();
        let kind = instr
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let mut children = Vec::new();
        self.inline_children(el, &mut children, in_link || kind == "HYPERLINK");
        if kind == "HYPERLINK"
            && let Some(url) = hyperlink_target(&instr)
            && !children.is_empty()
        {
            let url = self.resolve_url(url);
            out.push(Inl::Link { url, children });
            return;
        }
        if kind != "HYPERLINK" && !kind.is_empty() {
            self.result.record_loss(LossKind::Fields, 1, Some(kind));
        }
        out.extend(children);
    }

    fn hyperlink(&mut self, el: &Element, out: &mut Vec<Inl>) {
        let mut children = Vec::new();
        self.inline_children(el, &mut children, true);
        let anchor = el.attr_ns(Ns::W, "anchor");
        let external = el
            .attr_ns(Ns::R, "id")
            .and_then(|id| self.setup.rels.get(id))
            .map(|rel| rel.target.clone());
        let url = match (external, anchor) {
            (Some(target), Some(anchor)) => Some(format!("{target}#{anchor}")),
            (Some(target), None) => Some(target),
            (None, Some(anchor)) => Some(self.anchor_url(anchor)),
            (None, None) => None,
        };
        match url {
            Some(url) if !children.is_empty() => out.push(Inl::Link { url, children }),
            _ => out.extend(children),
        }
    }

    fn math(&mut self, el: &Element, display: bool, out: &mut Vec<Inl>) {
        match omml::to_latex(el) {
            Ok(tex) => {
                if !tex.trim().is_empty() {
                    out.push(Inl::Math { tex, display });
                }
            }
            Err(_) => {
                self.loss(LossKind::UnsupportedEquations, 1);
                out.push(Inl::Raw("[equation]".to_string()));
            }
        }
    }

    // ---- notes ---------------------------------------------------------------------------------

    fn note_ref(&mut self, el: &Element, endnote: bool, out: &mut Vec<Inl>) {
        let Some(id) = el.attr_ns(Ns::W, "id") else {
            return;
        };
        let note = if endnote {
            self.setup.endnotes.get(id)
        } else {
            self.setup.footnotes.get(id)
        }
        .cloned();
        self.note_count += 1;
        let label = self.note_count.to_string();
        out.push(Inl::NoteRef(label.clone()));
        let body = note
            .map(|n| self.note_text(&n, endnote))
            .unwrap_or_default();
        self.note_defs
            .push(format!("[^{label}]: {body}").trim_end().to_string());
    }

    fn note_text(&mut self, note: &Element, endnote: bool) -> String {
        let notes_rels = std::mem::take(if endnote {
            &mut self.setup.endnote_rels
        } else {
            &mut self.setup.footnote_rels
        });
        let saved_rels = std::mem::replace(&mut self.setup.rels, notes_rels);
        let saved_fields = std::mem::take(&mut self.fields);
        let saved_seq = self.para_seq;
        let saved_pending = std::mem::take(&mut self.pending);
        let saved_cell = std::mem::replace(&mut self.in_cell, true);
        let mut paragraphs = Vec::new();
        for p in note.descendants(Ns::W, "p") {
            self.para_seq += 1;
            let mut inlines = Vec::new();
            self.inline_children(p, &mut inlines, false);
            let text = flatten_breaks(&render_inlines(&inlines));
            let text = text.trim_matches(is_space);
            if !text.is_empty() {
                paragraphs.push(text.to_string());
            }
        }
        self.fields = saved_fields;
        self.para_seq = saved_seq;
        self.pending = saved_pending;
        self.in_cell = saved_cell;
        let notes_rels = std::mem::replace(&mut self.setup.rels, saved_rels);
        if endnote {
            self.setup.endnote_rels = notes_rels;
        } else {
            self.setup.footnote_rels = notes_rels;
        }
        paragraphs.join("\n\n    ")
    }

    // ---- drawings ------------------------------------------------------------------------------

    fn extract_image(&mut self, part: &str) -> Option<String> {
        if let Some(path) = self.images.get(part) {
            return Some(path.clone());
        }
        let Some(bytes) = (self.media)(part) else {
            self.loss(LossKind::CorruptParts, 1);
            return None;
        };
        let ext = part
            .rsplit_once('.')
            .map_or("bin", |(_, e)| e)
            .to_ascii_lowercase();
        let ext = if ext == "jpeg" {
            "jpg".to_string()
        } else {
            ext
        };
        if UNSUPPORTED_IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            self.loss(LossKind::UnsupportedImages, 1);
        }
        self.image_count += 1;
        let path = format!(
            "{}_assets/image-{}.{ext}",
            self.setup.stem, self.image_count
        );
        self.result.assets.push(Asset {
            relative_path: path.clone(),
            bytes,
        });
        self.images.insert(part.to_string(), path.clone());
        Some(path)
    }

    fn image_inline(
        &mut self,
        embed: Option<&str>,
        link: Option<&str>,
        alt: &str,
        width: Option<u32>,
    ) -> Option<Inl> {
        let id = embed.or(link)?;
        let Some(rel) = self.setup.rels.get(id).cloned() else {
            self.loss(LossKind::CorruptParts, 1);
            return None;
        };
        let dest = if rel.external {
            rel.target
        } else {
            let part = resolve_target(&self.setup.main_part, &rel.target);
            self.extract_image(&part)?
        };
        Some(Inl::Image {
            alt: alt.to_string(),
            dest,
            width,
        })
    }

    fn text_box_blocks(&mut self, boxes: &[&Element]) {
        self.loss(LossKind::TextBoxes, boxes.len());
        let saved = std::mem::take(&mut self.pending);
        let mut blocks = Vec::new();
        for text_box in boxes {
            self.blocks(text_box, &mut blocks);
        }
        self.pending = saved;
        self.pending.extend(blocks);
    }

    fn drawing(&mut self, drawing: &Element, out: &mut Vec<Inl>) {
        let blips = drawing.descendants(Ns::A, "blip");
        let mut boxes = Vec::new();
        text_boxes(drawing, &mut boxes);
        let frame = drawing
            .child(Ns::Wp, "inline")
            .or_else(|| drawing.child(Ns::Wp, "anchor"));
        let properties = frame.and_then(|f| f.child(Ns::Wp, "docPr"));
        let alt = properties
            .and_then(|p| {
                p.attr_ns(Ns::None, "descr")
                    .or_else(|| p.attr_ns(Ns::None, "title"))
            })
            .unwrap_or("");
        let width = if blips.len() == 1 {
            frame
                .and_then(|f| f.child(Ns::Wp, "extent"))
                .and_then(|e| e.attr_ns(Ns::None, "cx"))
                .and_then(|cx| cx.parse::<u64>().ok())
                .map(|cx| cx / EMU_PER_PX)
                .filter(|w| *w > 0)
                .and_then(|w| u32::try_from(w).ok())
        } else {
            None
        };
        for blip in &blips {
            let embed = blip.attr_ns(Ns::R, "embed");
            let link = blip.attr_ns(Ns::R, "link");
            if let Some(image) = self.image_inline(embed, link, alt, width) {
                out.push(image);
            }
        }
        if !boxes.is_empty() {
            self.text_box_blocks(&boxes);
        }
        let referenced = blips.iter().any(|blip| {
            blip.attr_ns(Ns::R, "embed").is_some() || blip.attr_ns(Ns::R, "link").is_some()
        });
        if !referenced && boxes.is_empty() {
            self.loss(LossKind::VectorShapes, 1);
        }
    }

    fn pict(&mut self, pict: &Element, out: &mut Vec<Inl>) {
        let images = pict.descendants(Ns::V, "imagedata");
        let mut boxes = Vec::new();
        text_boxes(pict, &mut boxes);
        for image in &images {
            let embed = image
                .attr_ns(Ns::R, "id")
                .or_else(|| image.attr_ns(Ns::O, "relid"));
            let alt = image.attr_ns(Ns::O, "title").unwrap_or("");
            if let Some(inline) = self.image_inline(embed, None, alt, None) {
                out.push(inline);
            }
        }
        if !boxes.is_empty() {
            self.text_box_blocks(&boxes);
        }
        if images.is_empty() && boxes.is_empty() {
            let rule = pict
                .descendants(Ns::V, "rect")
                .iter()
                .any(|r| r.attr_ns(Ns::O, "hr") == Some("t"));
            if rule {
                out.push(Inl::Rule);
            } else if VML_SHAPES
                .iter()
                .any(|shape| !pict.descendants(Ns::V, shape).is_empty())
            {
                self.loss(LossKind::VectorShapes, 1);
            }
        }
    }

    // ---- tables --------------------------------------------------------------------------------

    fn collect_cell(&mut self, parent: &Element, parts: &mut Vec<String>) {
        for child in parent.elements() {
            match (child.ns, child.name.as_str()) {
                (Ns::W, "p") => {
                    self.para_seq += 1;
                    let mut inlines = Vec::new();
                    self.inline_children(child, &mut inlines, false);
                    self.pending.clear();
                    let text = flatten_breaks(&render_inlines(&inlines));
                    let text = text.trim_matches(is_space);
                    if !text.is_empty() {
                        parts.push(text.to_string());
                    }
                }
                (Ns::W, "tbl") => {
                    self.loss(LossKind::NestedTables, 1);
                    for row in child.elements().filter(|e| e.is(Ns::W, "tr")) {
                        for cell in row.elements().filter(|e| e.is(Ns::W, "tc")) {
                            self.collect_cell(cell, parts);
                        }
                    }
                }
                (Ns::W, "sdt") => {
                    if let Some(content) = child.child(Ns::W, "sdtContent") {
                        self.collect_cell(content, parts);
                    }
                }
                (Ns::W, "sdtContent" | "ins" | "moveTo") => self.collect_cell(child, parts),
                _ => {}
            }
        }
    }

    fn cell_text(&mut self, cell: &Element) -> String {
        let saved = std::mem::replace(&mut self.in_cell, true);
        let mut parts = Vec::new();
        self.collect_cell(cell, &mut parts);
        self.in_cell = saved;
        if parts.len() > 1 {
            self.loss(LossKind::MultiParagraphCells, 1);
        }
        parts.join(" ")
    }

    fn table(&mut self, table: &Element, out: &mut Vec<Block>) {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut aligns: Vec<Vec<&'static str>> = Vec::new();
        let mut merged = 0;
        for row in table.elements().filter(|e| e.is(Ns::W, "tr")) {
            let mut cells = Vec::new();
            let mut row_aligns = Vec::new();
            for cell in row.elements().filter(|e| e.is(Ns::W, "tc")) {
                let properties = cell.child(Ns::W, "tcPr");
                let span = properties
                    .and_then(|p| p.child(Ns::W, "gridSpan"))
                    .and_then(val)
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(1)
                    .clamp(1, 64);
                let vertical = properties.and_then(|p| p.child(Ns::W, "vMerge"));
                let continued = vertical.is_some_and(|v| val(v).is_none_or(|s| s != "restart"));
                let horizontal = properties.is_some_and(|p| p.child(Ns::W, "hMerge").is_some());
                if span > 1 || vertical.is_some() || horizontal {
                    merged += 1;
                }
                let text = if continued || horizontal {
                    String::new()
                } else {
                    self.cell_text(cell)
                };
                let align = match cell
                    .child(Ns::W, "p")
                    .and_then(|p| p.child(Ns::W, "pPr"))
                    .and_then(|p| p.child(Ns::W, "jc"))
                    .and_then(val)
                {
                    Some("center") => ":---:",
                    Some("right" | "end") => "---:",
                    _ => "---",
                };
                cells.push(text);
                row_aligns.push(align);
                for _ in 1..span {
                    cells.push(String::new());
                    row_aligns.push(align);
                }
            }
            rows.push(cells);
            aligns.push(row_aligns);
        }
        self.loss(LossKind::MergedCells, merged);
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let align_row = aligns.get(1).or_else(|| aligns.first());
        let mut lines = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let cells: Vec<String> = (0..columns)
                .map(|c| {
                    let text = row.get(c).map_or("", String::as_str).replace('|', "\\|");
                    format!(" {text} ")
                })
                .collect();
            lines.push(format!("|{}|", cells.join("|")));
            if index == 0 {
                let marks: Vec<String> = (0..columns)
                    .map(|c| {
                        let mark = align_row.and_then(|a| a.get(c)).copied().unwrap_or("---");
                        format!(" {mark} ")
                    })
                    .collect();
                lines.push(format!("|{}|", marks.join("|")));
            }
        }
        push_block(out, Block::Table(lines.join("\n")));
    }
}

// ---------------------------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------------------------

fn package_error(error: PackageError) -> ImportError {
    match error {
        PackageError::TooLarge(detail) => ImportError::TooLarge(detail),
        other => ImportError::Corrupt(other.to_string()),
    }
}

fn wrong_zip(package: &mut Package<'_>) -> ImportError {
    let mimetype = package
        .read_part("mimetype")
        .ok()
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let detected = if mimetype.starts_with("application/vnd.oasis.opendocument") {
        "an OpenDocument document"
    } else if package.contains("xl/workbook.xml") {
        "a spreadsheet"
    } else if package.contains("ppt/presentation.xml") {
        "a presentation"
    } else {
        return ImportError::Corrupt("the package has no main document part".to_string());
    };
    ImportError::WrongFormat {
        expected: "DOCX",
        detected,
    }
}

/// Reads and parses an XML part; damaged parts are salvaged and reported.
fn load_xml(package: &mut Package<'_>, name: &str, result: &mut ImportResult) -> Option<Element> {
    let bytes = match package.read_part(name) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return None,
        Err(_) => {
            result.record_loss(LossKind::CorruptParts, 1, Some(name.to_string()));
            return None;
        }
    };
    match parse_xml_salvage(&bytes) {
        Some((root, damaged)) => {
            if damaged {
                result.record_loss(LossKind::CorruptParts, 1, Some(name.to_string()));
            }
            Some(root)
        }
        None => {
            result.record_loss(LossKind::CorruptParts, 1, Some(name.to_string()));
            None
        }
    }
}

/// Reads a `.rels` part; a missing part is empty, an unreadable one is reported.
fn load_relationships(
    package: &mut Package<'_>,
    name: &str,
    result: &mut ImportResult,
) -> Relationships {
    let parsed = match package.read_part(name) {
        Ok(None) => return Relationships::default(),
        Ok(Some(bytes)) => Relationships::parse(&bytes).ok(),
        Err(_) => None,
    };
    parsed.unwrap_or_else(|| {
        result.record_loss(LossKind::CorruptParts, 1, Some(name.to_string()));
        Relationships::default()
    })
}

/// Loads a footnotes/endnotes part and the relationships of that part.
fn load_notes(
    package: &mut Package<'_>,
    part: Option<String>,
    tag: &str,
    result: &mut ImportResult,
) -> (HashMap<String, Element>, Relationships) {
    let Some(part) = part else {
        return (HashMap::new(), Relationships::default());
    };
    let notes = load_xml(package, &part, result)
        .map(|root| notes_by_id(&root, tag))
        .unwrap_or_default();
    let rels = load_relationships(package, &rels_path_for(&part), result);
    (notes, rels)
}

fn notes_by_id(root: &Element, tag: &str) -> HashMap<String, Element> {
    root.elements()
        .filter(|e| e.is(Ns::W, tag))
        .filter(|e| matches!(e.attr_ns(Ns::W, "type"), None | Some("normal")))
        .filter_map(|e| Some((e.attr_ns(Ns::W, "id")?.to_string(), e.clone())))
        .collect()
}

fn has_content(root: &Element) -> bool {
    !root.text().trim().is_empty()
        || !root.descendants(Ns::A, "blip").is_empty()
        || !root.descendants(Ns::V, "imagedata").is_empty()
}

/// Imports a DOCX file. `stem` names the extracted-asset folder (`<stem>_assets/image-N.ext`).
pub fn import_docx(bytes: &[u8], stem: &str) -> Result<ImportResult, ImportError> {
    match sniff_container(bytes) {
        Container::Zip => {}
        Container::OleEncrypted => return Err(ImportError::Encrypted),
        other => {
            return Err(ImportError::WrongFormat {
                expected: "DOCX",
                detected: other.describe(),
            });
        }
    }
    let mut package = Package::open(bytes).map_err(package_error)?;
    let main = match package.main_part() {
        Ok(Some(main)) if package.contains(&main) => main,
        _ if package.contains("word/document.xml") => "word/document.xml".to_string(),
        _ => return Err(wrong_zip(&mut package)),
    };
    let mut result = ImportResult::default();
    let Some(document) = load_xml(&mut package, &main, &mut result) else {
        return Err(ImportError::Corrupt(
            "the main document part is empty or not XML".to_string(),
        ));
    };
    let rels = load_relationships(&mut package, &rels_path_for(&main), &mut result);
    let part_of = |kind: &str| {
        rels.of_kind(kind)
            .find(|r| !r.external)
            .map(|r| resolve_target(&main, &r.target))
    };
    let styles = part_of("styles")
        .and_then(|name| load_xml(&mut package, &name, &mut result))
        .map(|root| Styles::parse(&root))
        .unwrap_or_default();
    let numbering = part_of("numbering")
        .and_then(|name| load_xml(&mut package, &name, &mut result))
        .map(|root| Numbering::parse(&root, &styles))
        .unwrap_or_default();
    let (footnotes, footnote_rels) =
        load_notes(&mut package, part_of("footnotes"), "footnote", &mut result);
    let (endnotes, endnote_rels) =
        load_notes(&mut package, part_of("endnotes"), "endnote", &mut result);
    if let Some(root) = part_of("comments").and_then(|n| load_xml(&mut package, &n, &mut result)) {
        let count = root.elements().filter(|e| e.is(Ns::W, "comment")).count();
        result.record_loss(LossKind::Comments, count, None);
    }
    let furniture: Vec<String> = rels
        .of_kind("header")
        .chain(rels.of_kind("footer"))
        .filter(|r| !r.external)
        .map(|r| resolve_target(&main, &r.target))
        .collect();
    for name in furniture {
        if let Some(root) = load_xml(&mut package, &name, &mut result)
            && has_content(&root)
        {
            result.record_loss(LossKind::HeadersFooters, 1, None);
        }
    }
    let setup = Setup {
        rels,
        main_part: main,
        styles,
        numbering,
        footnotes,
        endnotes,
        footnote_rels,
        endnote_rels,
        stem: stem.to_string(),
    };
    let mut media = |name: &str| package.read_part(name).ok().flatten();
    let mut converter = Converter::new(setup, &mut media);
    converter.result = result;
    Ok(converter.run(&document))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ooxml::parse_xml;

    const NS: &str = concat!(
        r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" "#,
        r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" "#,
        r#"xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math" "#,
        r#"xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" "#,
        r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
        r#"xmlns:v="urn:schemas-microsoft-com:vml" "#,
        r#"xmlns:o="urn:schemas-microsoft-com:office:office" "#,
        r#"xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#
    );

    #[derive(Default)]
    struct Fixture {
        styles: &'static str,
        numbering: &'static str,
        rels: &'static str,
        footnotes: &'static str,
        media: Vec<(&'static str, Vec<u8>)>,
    }

    fn convert_with(f: &Fixture, body: &str) -> ImportResult {
        let doc =
            parse_xml(format!("<w:document {NS}><w:body>{body}</w:body></w:document>").as_bytes())
                .unwrap();
        let styles = Styles::parse(
            &parse_xml(format!("<w:styles {NS}>{}</w:styles>", f.styles).as_bytes()).unwrap(),
        );
        let numbering = Numbering::parse(
            &parse_xml(format!("<w:numbering {NS}>{}</w:numbering>", f.numbering).as_bytes())
                .unwrap(),
            &styles,
        );
        let rels = Relationships::parse(
            format!(
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{}</Relationships>"#,
                f.rels
            )
            .as_bytes(),
        )
        .unwrap();
        let notes =
            parse_xml(format!("<w:footnotes {NS}>{}</w:footnotes>", f.footnotes).as_bytes())
                .unwrap();
        let setup = Setup {
            rels,
            main_part: "word/document.xml".to_string(),
            styles,
            numbering,
            footnotes: notes_by_id(&notes, "footnote"),
            endnotes: notes_by_id(&notes, "endnote"),
            footnote_rels: Relationships::default(),
            endnote_rels: Relationships::default(),
            stem: "doc".to_string(),
        };
        let media = f.media.clone();
        let mut lookup = move |name: &str| {
            media
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, bytes)| bytes.clone())
        };
        Converter::new(setup, &mut lookup).run(&doc)
    }

    fn md(body: &str) -> String {
        convert_with(&Fixture::default(), body).markdown
    }

    fn p(runs: &str) -> String {
        format!("<w:p>{runs}</w:p>")
    }

    fn r(text: &str) -> String {
        format!("<w:r><w:t xml:space=\"preserve\">{text}</w:t></w:r>")
    }

    fn rp(props: &str, text: &str) -> String {
        format!("<w:r><w:rPr>{props}</w:rPr><w:t xml:space=\"preserve\">{text}</w:t></w:r>")
    }

    fn kinds(result: &ImportResult) -> Vec<(LossKind, usize)> {
        result
            .sorted_losses()
            .iter()
            .map(|l| (l.kind, l.count))
            .collect()
    }

    #[test]
    fn inline_formatting_nests_and_moves_whitespace_outside_markers() {
        let cases = [
            (rp("<w:b/>", "bold "), "**bold**\n"),
            (
                format!("{}{}", rp("<w:b/>", "a"), rp("<w:b/><w:i/>", "b")),
                "**a*b***\n",
            ),
            (rp("<w:i/>", "x"), "*x*\n"),
            (rp("<w:strike/>", "x"), "~~x~~\n"),
            (rp("<w:u w:val=\"single\"/>", "x"), "~x~\n"),
            (rp("<w:highlight w:val=\"yellow\"/>", "x"), "==x==\n"),
            (rp("<w:highlight w:val=\"red\"/>", "x"), "=={red}x==\n"),
            (
                format!(
                    "{}{}",
                    r("x "),
                    rp("<w:rFonts w:ascii=\"Consolas\"/>", "a`b")
                ),
                "x ``a`b``\n",
            ),
            (
                format!(
                    "{}{}",
                    r("x"),
                    rp("<w:rFonts w:ascii=\"Consolas\"/>", " pad ")
                ),
                "x`  pad  `\n",
            ),
            (
                rp("<w:rFonts w:ascii=\"Consolas\"/>", "a`b"),
                "```\na`b\n```\n",
            ),
            (rp("<w:b/>", "   "), ""),
            // Boundary whitespace moves outside the markers, and both runs' spaces are kept
            // (Word shows two spaces; Markdown rendering collapses them, the text stays faithful).
            (format!("{}{}", r("a "), rp("<w:b/>", " b")), "a  **b**\n"),
            (r("1. not a list"), "1\\. not a list\n"),
            (r("# not a heading"), "\\# not a heading\n"),
            (r("- dash"), "\\- dash\n"),
            (r("---"), "\\---\n"),
            (r("> quote"), "\\> quote\n"),
            // The run text is XML-escaped here; the document text is `... <g> ==`.
            (
                r("*a* _b_ [c] `d` ~e~ $f$ &lt;g&gt; =="),
                "\\*a\\* \\_b\\_ \\[c\\] \\`d\\` \\~e\\~ \\$f\\$ \\<g> \\=\\=\n",
            ),
            // Literal text `a & b &amp; &#35; &x; < 3`: only entity-like ampersands are escaped.
            (
                r("snake_case a &amp; b &amp;amp; &amp;#35; &amp;x; &lt; 3"),
                "snake_case a & b \\&amp; \\&#35; \\&x; < 3\n",
            ),
        ];
        for (body, want) in cases {
            assert_eq!(md(&p(&body)), want, "{body}");
        }
    }

    #[test]
    fn character_styles_and_hidden_text() {
        let fixture = Fixture {
            styles: r#"<w:style w:type="character" w:styleId="Strong"><w:name w:val="Strong"/><w:rPr><w:b/></w:rPr></w:style>
<w:style w:type="character" w:styleId="Hyperlink"><w:name w:val="Hyperlink"/><w:rPr><w:u w:val="single"/></w:rPr></w:style>"#,
            ..Fixture::default()
        };
        let body = p(&format!(
            "{}{}{}",
            "<w:r><w:rPr><w:rStyle w:val=\"Strong\"/></w:rPr><w:t>S</w:t></w:r>",
            "<w:r><w:rPr><w:vanish/></w:rPr><w:t>hidden</w:t></w:r>",
            "<w:r><w:rPr><w:vanish/></w:rPr></w:r>"
        ));
        let result = convert_with(&fixture, &body);
        assert_eq!(result.markdown, "**S**\n");
        assert_eq!(kinds(&result), [(LossKind::HiddenText, 1)]);
    }

    #[test]
    fn loss_accounting_for_run_and_paragraph_properties() {
        let body = format!(
            "{}{}{}{}",
            p(&rp("<w:vertAlign w:val=\"superscript\"/>", "x")),
            p(&rp("<w:color w:val=\"FF0000\"/>", "y")),
            "<w:p><w:pPr><w:jc w:val=\"center\"/></w:pPr><w:r><w:t>c</w:t></w:r></w:p>",
            "<w:p><w:pPr><w:jc w:val=\"center\"/></w:pPr></w:p>"
        );
        let result = convert_with(&Fixture::default(), &body);
        assert_eq!(result.markdown, "x\n\ny\n\nc\n");
        assert_eq!(
            kinds(&result),
            [
                (LossKind::Alignment, 1),
                (LossKind::FontsAndColors, 1),
                (LossKind::SubSuperscript, 1)
            ]
        );
    }

    #[test]
    fn headings_quotes_code_and_rules() {
        let fixture = Fixture {
            styles: r#"<w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/></w:style>
<w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/></w:style>
<w:style w:type="paragraph" w:styleId="Code"><w:name w:val="Source Code"/></w:style>"#,
            ..Fixture::default()
        };
        let body = format!(
            "{}{}{}{}{}{}{}{}",
            "<w:p><w:pPr><w:pStyle w:val=\"Heading2\"/></w:pPr><w:r><w:t>Title #</w:t></w:r><w:r><w:br/></w:r><w:r><w:t>more</w:t></w:r></w:p>",
            "<w:p><w:pPr><w:outlineLvl w:val=\"0\"/></w:pPr><w:r><w:t>Direct</w:t></w:r></w:p>",
            "<w:p><w:pPr><w:outlineLvl w:val=\"9\"/></w:pPr><w:r><w:t>Body</w:t></w:r></w:p>",
            "<w:p><w:pPr><w:pStyle w:val=\"Quote\"/></w:pPr><w:r><w:t>q1</w:t></w:r></w:p>",
            "<w:p><w:pPr><w:pStyle w:val=\"Quote\"/></w:pPr><w:r><w:t>q2</w:t></w:r></w:p>",
            "<w:p><w:pPr><w:pStyle w:val=\"Code\"/></w:pPr><w:r><w:t>a()</w:t></w:r><w:r><w:br/></w:r><w:r><w:t>```</w:t></w:r></w:p>",
            "<w:p><w:pPr><w:pStyle w:val=\"Code\"/></w:pPr><w:r><w:t>b()</w:t></w:r></w:p>",
            "<w:p><w:r><w:pict><v:rect o:hr=\"t\"/></w:pict></w:r></w:p>"
        );
        assert_eq!(
            convert_with(&fixture, &body).markdown,
            "## Title # more\n\n# Direct\n\nBody\n\n> q1\n>\n> q2\n\n````\na()\n```\nb()\n````\n\n---\n"
        );
        assert_eq!(
            md(&format!(
                "{}{}",
                p(&r("x")),
                "<w:p><w:r><w:pict><v:rect o:hr=\"t\"/></w:pict></w:r></w:p>"
            )),
            "x\n\n---\n"
        );
    }

    #[test]
    fn lists_numbering_nesting_tasks_and_styles() {
        let fixture = Fixture {
            styles: r#"<w:style w:type="paragraph" w:styleId="LB"><w:name w:val="List Bullet"/><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr></w:style>"#,
            numbering: r#"<w:abstractNum w:abstractNumId="0">
<w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/><w:lvlText w:val="o"/></w:lvl>
<w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/><w:lvlText w:val="&#x2610;"/></w:lvl>
<w:lvl w:ilvl="2"><w:numFmt w:val="bullet"/><w:lvlText w:val="&#x2611;"/></w:lvl></w:abstractNum>
<w:abstractNum w:abstractNumId="1">
<w:lvl w:ilvl="0"><w:start w:val="4"/><w:numFmt w:val="decimal"/></w:lvl>
<w:lvl w:ilvl="1"><w:numFmt w:val="lowerLetter"/></w:lvl>
<w:lvl w:ilvl="2"><w:numFmt w:val="none"/></w:lvl></w:abstractNum>
<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
<w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>"#,
            ..Fixture::default()
        };
        let item = |num: &str, lvl: u8, text: &str| {
            format!(
                "<w:p><w:pPr><w:numPr><w:ilvl w:val=\"{lvl}\"/><w:numId w:val=\"{num}\"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"
            )
        };
        let body = format!(
            "{}{}{}{}{}{}{}{}{}{}{}",
            item("1", 0, "bullet"),
            item("1", 1, "open task"),
            item("1", 2, "done task"),
            "<w:p><w:pPr><w:pStyle w:val=\"LB\"/></w:pPr><w:r><w:t>styled</w:t></w:r></w:p>",
            p(&r("break")),
            item("2", 0, "one"),
            item("2", 1, "sub"),
            item("2", 2, "plain"),
            item("2", 0, "two"),
            "<w:p><w:pPr><w:numPr><w:numId w:val=\"0\"/></w:numPr></w:pPr><w:r><w:t>no list</w:t></w:r></w:p>",
            item("9", 0, "unknown list")
        );
        let result = convert_with(&fixture, &body);
        assert_eq!(
            result.markdown,
            "- bullet\n  - [ ] open task\n    - [x] done task\n- styled\n\nbreak\n\n4) one\n   1) sub\n\nplain\n\n5. two\n\nno list\n\nunknown list\n"
        );
        assert_eq!(kinds(&result), [(LossKind::ListNumberingStyle, 1)]);
    }

    #[test]
    fn adjacent_lists_use_different_markers() {
        let fixture = Fixture {
            numbering: r#"<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum>
<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="0"/></w:num>"#,
            ..Fixture::default()
        };
        let item = |num: &str, text: &str| {
            format!(
                "<w:p><w:pPr><w:numPr><w:numId w:val=\"{num}\"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"
            )
        };
        let body = format!("{}{}{}", item("1", "a"), item("1", "b"), item("2", "c"));
        assert_eq!(convert_with(&fixture, &body).markdown, "- a\n- b\n* c\n");
    }

    #[test]
    fn hyperlinks_anchors_and_fields() {
        let fixture = Fixture {
            styles: r#"<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style>"#,
            rels: r#"<Relationship Id="rId5" Type="http://x/hyperlink" Target="https://e.com/a b" TargetMode="External"/>"#,
            ..Fixture::default()
        };
        let body = format!(
            "{}{}{}{}{}{}{}{}{}",
            "<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr><w:bookmarkStart w:id=\"1\" w:name=\"_Toc1\"/><w:r><w:t>My Title</w:t></w:r></w:p>",
            "<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr><w:bookmarkStart w:id=\"2\" w:name=\"_Toc2\"/><w:r><w:t>My Title</w:t></w:r></w:p>",
            "<w:p><w:hyperlink r:id=\"rId5\"><w:r><w:rPr><w:u w:val=\"single\"/></w:rPr><w:t>site</w:t></w:r></w:hyperlink><w:hyperlink r:id=\"rId5\" w:anchor=\"frag\"><w:r><w:t>both</w:t></w:r></w:hyperlink></w:p>",
            "<w:p><w:hyperlink w:anchor=\"_Toc2\"><w:r><w:t>jump</w:t></w:r></w:hyperlink><w:hyperlink w:anchor=\"nope\"><w:r><w:t>dangling</w:t></w:r></w:hyperlink></w:p>",
            "<w:p><w:hyperlink><w:r><w:t>plain</w:t></w:r></w:hyperlink><w:hyperlink w:anchor=\"x\"></w:hyperlink></w:p>",
            "<w:p><w:r><w:fldChar w:fldCharType=\"begin\"/></w:r><w:r><w:instrText> HYPERLINK \"https://f.org\" </w:instrText></w:r><w:r><w:fldChar w:fldCharType=\"separate\"/></w:r><w:r><w:t>field link</w:t></w:r><w:r><w:fldChar w:fldCharType=\"end\"/></w:r></w:p>",
            "<w:p><w:fldSimple w:instr=\" HYPERLINK \\l &quot;_Toc1&quot; \"><w:r><w:t>simple</w:t></w:r></w:fldSimple><w:fldSimple w:instr=\" PAGE \"><w:r><w:t>7</w:t></w:r></w:fldSimple><w:fldSimple w:instr=\"HYPERLINK\"><w:r><w:t>bare</w:t></w:r></w:fldSimple><w:fldSimple><w:r><w:t>nofield</w:t></w:r></w:fldSimple></w:p>",
            "<w:p><w:r><w:fldChar w:fldCharType=\"begin\"/></w:r><w:r><w:instrText>TOC \\o</w:instrText></w:r><w:r><w:fldChar w:fldCharType=\"separate\"/></w:r><w:r><w:t>toc text</w:t></w:r><w:r><w:fldChar w:fldCharType=\"end\"/></w:r><w:r><w:fldChar w:fldCharType=\"weird\"/></w:r></w:p>",
            "<w:p><w:r><w:fldChar w:fldCharType=\"begin\"/></w:r><w:r><w:instrText>HYPERLINK \"https://g.org\"</w:instrText></w:r><w:r><w:t>instr only</w:t></w:r><w:r><w:fldChar w:fldCharType=\"end\"/></w:r><w:r><w:fldChar w:fldCharType=\"separate\"/></w:r><w:r><w:fldChar w:fldCharType=\"end\"/></w:r></w:p>"
        );
        let result = convert_with(&fixture, &body);
        assert_eq!(
            result.markdown,
            "# My Title\n\n# My Title\n\n[site](<https://e.com/a b>)[both](<https://e.com/a b#frag>)\n\n[jump](#my-title-1)[dangling](#nope)\n\nplain\n\n[field link](https://f.org)\n\n[simple](#my-title)7barenofield\n\ntoc text\n"
        );
        // Only PAGE and TOC are lost; every HYPERLINK form keeps its text and an
        // instruction-less `fldSimple` is not a field.
        assert_eq!(kinds(&result), [(LossKind::Fields, 2)]);
    }

    #[test]
    fn breaks_tabs_symbols_and_tracked_changes() {
        let body = format!(
            "{}{}{}{}{}",
            p(
                "<w:r><w:t>a</w:t><w:tab/><w:t>b</w:t><w:br/><w:t>c</w:t><w:noBreakHyphen/><w:softHyphen/><w:sym w:font=\"Symbol\" w:char=\"F0B7\"/><w:br w:type=\"column\"/><w:t>d</w:t></w:r>"
            ),
            p("<w:r><w:t>before</w:t><w:br w:type=\"page\"/><w:t>after</w:t></w:r>"),
            "<w:p><w:pPr><w:pageBreakBefore/></w:pPr><w:r><w:t>new page</w:t></w:r></w:p>",
            p(&format!(
                "{}{}{}{}",
                "<w:ins><w:r><w:t>inserted</w:t></w:r></w:ins>",
                "<w:del><w:r><w:delText>gone</w:delText></w:r></w:del>",
                "<w:moveFrom><w:r><w:t>moved</w:t></w:r></w:moveFrom><w:moveTo><w:r><w:rPr><w:rPrChange/></w:rPr><w:t>here</w:t></w:r></w:moveTo>",
                "<w:smartTag><w:r><w:t>S</w:t></w:r></w:smartTag><w:sdt><w:sdtContent><w:r><w:t>T</w:t></w:r></w:sdtContent></w:sdt><w:proofErr/>"
            )),
            "<w:ins><w:p><w:r><w:t>block ins</w:t></w:r></w:p></w:ins><w:del><w:p><w:r><w:t>block del</w:t></w:r></w:p></w:del>"
        );
        let result = convert_with(&Fixture::default(), &body);
        assert_eq!(
            result.markdown,
            "a b\\\nc-d\n\nbefore\n\n<!-- pagebreak -->\n\nafter\n\n<!-- pagebreak -->\n\nnew page\n\ninsertedhereST\n\nblock ins\n"
        );
        assert_eq!(
            kinds(&result),
            [
                (LossKind::TrackedChanges, 7),
                (LossKind::SymbolFontCharacters, 1),
            ]
        );
    }

    #[test]
    fn tables_merges_nesting_and_alignment() {
        let cell =
            |props: &str, paras: &str| format!("<w:tc><w:tcPr>{props}</w:tcPr>{paras}</w:tc>");
        let para = |jc: &str, text: &str| {
            format!("<w:p><w:pPr><w:jc w:val=\"{jc}\"/></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>")
        };
        let nested =
            "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>inner</w:t></w:r></w:p></w:tc></w:tr></w:tbl>";
        let table = format!(
            "<w:tbl><w:tr>{}{}{}</w:tr><w:tr>{}{}{}</w:tr><w:tr>{}{}</w:tr><w:tr>{}</w:tr></w:tbl>",
            cell("", &para("left", "H1")),
            cell("<w:gridSpan w:val=\"2\"/>", &para("left", "H2")),
            cell("", &para("left", "H4")),
            cell("<w:vMerge w:val=\"restart\"/>", &para("center", "a|b")),
            cell("", &para("right", "c")),
            cell("", &format!("{}{}", para("left", "p1"), para("left", "p2"))),
            cell("<w:vMerge/>", &para("left", "ignored")),
            cell("<w:hMerge/>", &para("left", "ignored")),
            cell("", &format!("{}{}", para("left", "n"), nested))
        );
        let result = convert_with(&Fixture::default(), &table);
        assert_eq!(
            result.markdown,
            "| H1 | H2 |  | H4 |\n| :---: | ---: | --- | --- |\n| a\\|b | c | p1 p2 |  |\n|  |  |  |  |\n| n inner |  |  |  |\n"
        );
        assert_eq!(
            kinds(&result),
            [
                (LossKind::MergedCells, 4),
                (LossKind::NestedTables, 1),
                (LossKind::MultiParagraphCells, 2)
            ]
        );
        assert_eq!(md("<w:tbl/>"), "");
        assert_eq!(md("<w:tbl><w:tr/></w:tbl>"), "");
    }

    #[test]
    fn footnotes_endnotes_and_comments_in_order() {
        let fixture = Fixture {
            footnotes: r#"<w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>
<w:footnote w:id="1"><w:p><w:r><w:footnoteRef/></w:r><w:r><w:t xml:space="preserve"> First </w:t></w:r></w:p><w:p><w:r><w:t>Second para</w:t></w:r></w:p></w:footnote>
<w:endnote w:id="2"><w:p><w:r><w:t>An end note</w:t></w:r></w:p></w:endnote>
<w:footnote w:id="3"/>"#,
            ..Fixture::default()
        };
        let body = format!(
            "{}{}{}",
            p(&format!(
                "{}{}{}",
                r("Text"),
                "<w:r><w:footnoteReference w:id=\"1\"/></w:r>",
                "<w:r><w:endnoteReference w:id=\"2\"/></w:r>"
            )),
            p(
                "<w:r><w:footnoteReference w:id=\"3\"/></w:r><w:r><w:footnoteReference w:id=\"99\"/></w:r><w:r><w:footnoteReference/></w:r>"
            ),
            "<w:p/>"
        );
        let result = convert_with(&fixture, &body);
        assert_eq!(
            result.markdown,
            "Text[^1][^2]\n\n[^3][^4]\n\n[^1]: First\n\n    Second para\n\n[^2]: An end note\n\n[^3]:\n\n[^4]:\n"
        );
    }

    #[test]
    fn equations_become_latex_or_placeholders() {
        let frac = "<m:f><m:num><m:r><m:t>a</m:t></m:r></m:num><m:den><m:r><m:t>b</m:t></m:r></m:den></m:f>";
        let body = format!(
            "{}{}{}{}{}",
            p(&format!(
                "{}<m:oMath>{frac}</m:oMath>{}",
                r("see "),
                r(" here")
            )),
            p(&format!("<m:oMath>{frac}</m:oMath>")),
            p(&format!(
                "{}<m:oMathPara><m:oMath>{frac}</m:oMath></m:oMathPara>{}",
                r("x"),
                r("y")
            )),
            p("<m:oMath><m:zigzag/></m:oMath>"),
            p("<m:oMath><m:ctrlPr/></m:oMath>")
        );
        let result = convert_with(&Fixture::default(), &body);
        assert_eq!(
            result.markdown,
            "see $\\frac{a}{b}$ here\n\n$$\n\\frac{a}{b}\n$$\n\nx\n\n$$\n\\frac{a}{b}\n$$\n\ny\n\n[equation]\n"
        );
        assert_eq!(kinds(&result), [(LossKind::UnsupportedEquations, 1)]);
    }

    fn drawing(extent: &str, blip: &str, descr: &str) -> String {
        format!(
            "<w:r><w:drawing><wp:inline><wp:extent {extent}/><wp:docPr id=\"1\" name=\"Picture 1\" {descr}/><a:graphic><a:graphicData><a:blip {blip}/></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"
        )
    }

    #[test]
    fn images_are_extracted_once_and_losses_reported() {
        let fixture = Fixture {
            rels: r#"<Relationship Id="rId1" Type="http://x/image" Target="media/image1.jpeg"/>
<Relationship Id="rId2" Type="http://x/image" Target="media/pic.emf"/>
<Relationship Id="rId3" Type="http://x/image" Target="media/missing.png"/>
<Relationship Id="rId4" Type="http://x/image" Target="https://cdn.example.com/x.png" TargetMode="External"/>"#,
            media: vec![
                ("word/media/image1.jpeg", vec![1, 2]),
                ("word/media/pic.emf", vec![3]),
            ],
            ..Fixture::default()
        };
        let body = p(&format!(
            "{}{}{}{}{}{}{}",
            drawing(
                "cx=\"1905000\" cy=\"10\"",
                "r:embed=\"rId1\"",
                "descr=\"A | [cat]\""
            ),
            drawing("cx=\"0\"", "r:embed=\"rId1\"", "title=\"T\""),
            drawing("cx=\"x\"", "r:embed=\"rId2\"", ""),
            drawing("", "r:embed=\"rId3\"", ""),
            drawing("", "r:embed=\"rId9\"", ""),
            drawing("", "r:link=\"rId4\"", ""),
            drawing("", "", "")
        ));
        let result = convert_with(&fixture, &body);
        assert_eq!(
            result.markdown,
            "![A \\| \\[cat\\]|200](doc_assets/image-1.jpg)![T](doc_assets/image-1.jpg)![](doc_assets/image-2.emf)![](https://cdn.example.com/x.png)\n"
        );
        assert_eq!(result.assets.len(), 2);
        assert_eq!(result.assets[0].relative_path, "doc_assets/image-1.jpg");
        assert_eq!(result.assets[0].bytes, [1, 2]);
        assert_eq!(
            kinds(&result),
            [
                (LossKind::VectorShapes, 1),
                (LossKind::UnsupportedImages, 1),
                (LossKind::CorruptParts, 2)
            ]
        );
    }

    #[test]
    fn text_boxes_vml_and_alternate_content() {
        let fixture = Fixture {
            rels: r#"<Relationship Id="rId1" Type="http://x/image" Target="media/i.png"/>"#,
            media: vec![("word/media/i.png", vec![9])],
            ..Fixture::default()
        };
        let txbx = "<w:txbxContent><w:p><w:r><w:t>boxed</w:t></w:r></w:p></w:txbxContent>";
        let body = format!(
            "{}{}{}{}{}{}",
            p(&format!(
                "{}<w:r><mc:AlternateContent><mc:Choice><w:drawing><wp:anchor><a:graphic>{txbx}</a:graphic></wp:anchor></w:drawing></mc:Choice><mc:Fallback><w:pict><v:shape><v:textbox>{txbx}</v:textbox></v:shape></w:pict></mc:Fallback></mc:AlternateContent></w:r>",
                r("host")
            )),
            p(
                "<w:r><w:pict><v:shape><v:imagedata r:id=\"rId1\" o:title=\"vml\"/></v:shape></w:pict></w:r>"
            ),
            p(
                "<w:r><w:pict><v:shape><v:textbox><w:txbxContent><w:p><w:r><w:drawing><wp:inline><a:graphic><a:graphicData/></a:graphic></wp:inline></w:drawing></w:r></w:p></w:txbxContent></v:textbox></v:shape></w:pict></w:r>"
            ),
            p("<w:r><w:pict><v:oval/></w:pict></w:r>"),
            p("<w:r><w:pict><v:shapetype/></w:pict></w:r>"),
            p(
                "<w:r><w:object><v:shape><v:imagedata o:relid=\"rId1\"/></v:shape></w:object></w:r><mc:AlternateContent/>"
            )
        );
        let result = convert_with(&fixture, &body);
        assert_eq!(
            result.markdown,
            "host\n\nboxed\n\n![vml](doc_assets/image-1.png)\n\n![](doc_assets/image-1.png)\n"
        );
        assert_eq!(
            kinds(&result),
            [(LossKind::TextBoxes, 2), (LossKind::VectorShapes, 2)]
        );
    }

    #[test]
    fn ruby_keeps_the_base_text_and_sections_report_columns() {
        let body = format!(
            "{}{}",
            p(
                "<w:r><w:ruby><w:rt><w:r><w:t>guide</w:t></w:r></w:rt><w:rubyBase><w:r><w:t>base</w:t></w:r></w:rubyBase></w:ruby></w:r>"
            ),
            "<w:p><w:pPr><w:sectPr><w:cols w:num=\"2\"/></w:sectPr></w:pPr></w:p><w:sectPr><w:cols w:num=\"3\"/></w:sectPr>"
        );
        let result = convert_with(&Fixture::default(), &body);
        assert_eq!(result.markdown, "base\n");
        assert_eq!(
            kinds(&result),
            [(LossKind::Columns, 2), (LossKind::SectionBreaks, 1)]
        );
    }

    #[test]
    fn content_controls_and_alternate_content_at_block_level() {
        let body = format!(
            "<w:sdt><w:sdtContent>{}</w:sdtContent></w:sdt><w:sdt/><mc:AlternateContent><mc:Choice>{}</mc:Choice></mc:AlternateContent><mc:AlternateContent/><w:customXml>{}</w:customXml>",
            p(&r("sdt")),
            p(&r("choice")),
            p(&r("custom"))
        );
        assert_eq!(md(&body), "sdt\n\nchoice\n\ncustom\n");
    }

    #[test]
    fn rendering_helpers() {
        assert_eq!(code_span(""), "");
        assert_eq!(code_span("a\nb"), "`a b`");
        assert_eq!(md_dest("a/b.png"), "a/b.png");
        assert_eq!(md_dest("a b(1).png"), "<a b(1).png>");
        assert_eq!(md_dest(""), "<>");
        assert_eq!(md_dest("x<y>\tz"), "<x%3Cy%3E%20z>");
        assert_eq!(slugify("Hello, World! A_b-c"), "hello-world-a_b-c");
        assert_eq!(escape_heading("a #"), "a \\#");
        assert_eq!(flatten_breaks("a\\\nb\nc"), "a b c");
        assert_eq!(wrap_mark("**", "**", "  "), "  ");
        assert_eq!(
            hyperlink_target(" HYPERLINK \"u\" \\l \"a\""),
            Some("u#a".to_string())
        );
        assert_eq!(
            hyperlink_target("HYPERLINK \\l \"a\""),
            Some("#a".to_string())
        );
        assert_eq!(
            hyperlink_target("HYPERLINK mailto:x"),
            Some("mailto:x".to_string())
        );
        assert_eq!(hyperlink_target("HYPERLINK"), None);
        assert_eq!(
            hyperlink_target("HYPERLINK \"unterminated"),
            Some("\"unterminated".to_string())
        );
        assert_eq!(escape_line_start("7)"), "7\\)");
        assert_eq!(escape_line_start("1234567890. x"), "1234567890. x");
        assert_eq!(escape_line_start("####### x"), "####### x");
        assert_eq!(escape_line_start("+x"), "+x");
        assert_eq!(escape_line_start("1.x"), "1.x");
        assert_eq!(
            render_blocks(&[Block::Math("x".into()), Block::Para("a".into())]),
            "$$\nx\n$$\n\na"
        );
        assert!(has_visible(&[Inl::Raw("x".into())]));
        assert!(!has_visible(&[Inl::Break]));
    }

    fn zip(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = crate::ooxml::PackageWriter::new();
        for (name, content) in parts {
            writer.add(name, content.as_bytes());
        }
        writer.finish().unwrap()
    }

    #[test]
    fn rejects_files_that_are_not_docx() {
        assert_eq!(
            import_docx(b"{\\rtf1 hi}", "x").unwrap_err(),
            ImportError::WrongFormat {
                expected: "DOCX",
                detected: "an RTF document"
            }
        );
        assert_eq!(
            import_docx(b"", "x").unwrap_err(),
            ImportError::WrongFormat {
                expected: "DOCX",
                detected: "an unknown file type"
            }
        );
        assert!(matches!(
            import_docx(b"PK\x03\x04junk", "x"),
            Err(ImportError::Corrupt(_))
        ));
        let odt = zip(&[("mimetype", "application/vnd.oasis.opendocument.text")]);
        assert_eq!(
            import_docx(&odt, "x").unwrap_err(),
            ImportError::WrongFormat {
                expected: "DOCX",
                detected: "an OpenDocument document"
            }
        );
        let xlsx = zip(&[("xl/workbook.xml", "<x/>")]);
        assert_eq!(
            import_docx(&xlsx, "x").unwrap_err(),
            ImportError::WrongFormat {
                expected: "DOCX",
                detected: "a spreadsheet"
            }
        );
        let pptx = zip(&[("ppt/presentation.xml", "<x/>")]);
        assert_eq!(
            import_docx(&pptx, "x").unwrap_err(),
            ImportError::WrongFormat {
                expected: "DOCX",
                detected: "a presentation"
            }
        );
        let empty = zip(&[("readme.txt", "hi")]);
        assert!(matches!(
            import_docx(&empty, "x"),
            Err(ImportError::Corrupt(_))
        ));
        let not_xml = zip(&[("word/document.xml", "   ")]);
        assert!(matches!(
            import_docx(&not_xml, "x"),
            Err(ImportError::Corrupt(_))
        ));
    }

    #[test]
    fn imports_a_package_with_all_parts() {
        let w = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://x/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://x/comments" Target="comments.xml"/><Relationship Id="rId3" Type="http://x/header" Target="header1.xml"/><Relationship Id="rId4" Type="http://x/footer" Target="footer1.xml"/><Relationship Id="rId5" Type="http://x/header" Target="missing.xml"/><Relationship Id="rId6" Type="http://x/numbering" Target="numbering.xml"/><Relationship Id="rId7" Type="http://x/footnotes" Target="footnotes.xml"/><Relationship Id="rId8" Type="http://x/endnotes" Target="endnotes.xml"/></Relationships>"#;
        let root = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://x/officeDocument" Target="/word/document.xml"/></Relationships>"#;
        let document = format!(
            "<w:document {w}><w:body><w:p><w:r><w:t>Hello</w:t></w:r></w:p></w:body></w:document>"
        );
        let comments =
            format!("<w:comments {w}><w:comment w:id=\"0\"/><w:comment w:id=\"1\"/></w:comments>");
        let header = format!("<w:hdr {w}><w:p><w:r><w:t>Head</w:t></w:r></w:p></w:hdr>");
        let footer = format!("<w:ftr {w}><w:p/></w:ftr>");
        let styles = format!("<w:styles {w}/>");
        let numbering = "<w:numbering";
        let notes = format!("<w:footnotes {w}/>");
        let bytes = zip(&[
            ("_rels/.rels", root),
            ("word/document.xml", &document),
            ("word/_rels/document.xml.rels", rels),
            ("word/comments.xml", &comments),
            ("word/header1.xml", &header),
            ("word/footer1.xml", &footer),
            ("word/styles.xml", &styles),
            ("word/numbering.xml", numbering),
            ("word/footnotes.xml", &notes),
            ("word/endnotes.xml", &notes),
        ]);
        let result = import_docx(&bytes, "report").unwrap();
        assert_eq!(result.markdown, "Hello\n");
        assert_eq!(
            kinds(&result),
            [
                (LossKind::HeadersFooters, 1),
                (LossKind::Comments, 2),
                (LossKind::CorruptParts, 1)
            ]
        );
        // Without relationships the main part falls back to word/document.xml.
        let bare = zip(&[("word/document.xml", &document)]);
        assert_eq!(import_docx(&bare, "x").unwrap().markdown, "Hello\n");
    }

    #[test]
    fn truncated_main_part_is_salvaged_and_reported() {
        let w = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
        let document = format!("<w:document {w}><w:body><w:p><w:r><w:t>Kept");
        let bytes = zip(&[("word/document.xml", &document)]);
        let result = import_docx(&bytes, "x").unwrap();
        assert_eq!(result.markdown, "Kept\n");
        assert_eq!(kinds(&result), [(LossKind::CorruptParts, 1)]);
    }
}
