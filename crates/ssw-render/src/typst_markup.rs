//! Markdown tree → Typst markup for paged export (PDF, PNG, JPEG, print layout).
//!
//! [`markdown_to_typst`] turns a parsed [`Document`] into the source of one Typst document: a
//! preamble (page, fonts, show rules, a few helper functions), then the content. All document
//! text goes through [`escape_markup`] / [`string_literal`], so no Markdown text can change the
//! structure of the Typst source. Inline constructs are written as function calls ended with
//! `;` (`#strong[…];`) so the text that follows can never be read as part of the call, which
//! also makes them work inside words.
//!
//! Content that cannot be typeset degrades instead of failing the export: formulas MiTeX or
//! Typst reject appear as their source in the error color, images that are missing, outside the
//! document directory or of an unsupported type appear as their alt text, and diagrams that
//! cannot be drawn appear as a code block with a note.

use std::collections::HashMap;

use ssw_core::markdown::ast::{
    Alignment, Block, BlockKind, CalloutKind, CodeBlock, Document, HighlightColor, HtmlBlock,
    Image, Inline, InlineKind, ListDelimiter, MathBlock, Table, TableRow, Task,
};
use ssw_core::markdown::definitions::normalize_label;
use ssw_core::settings::PageSize;

use crate::export::ExportOptions;
use crate::fonts::{MONO_FAMILY, TEXT_FAMILY};
use crate::math::{export_expression, typst_import};
use crate::svg_to_scene::svg_to_scene;

/// Note shown above the source of a diagram that cannot be drawn.
const UNSUPPORTED_DIAGRAM: &str = "Unsupported diagram";

/// File name stem of diagram images in [`TypstDocument::files`].
const DIAGRAM_FILE_PREFIX: &str = "ssw-diagram-";

/// Image file extensions Typst can decode.
const IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "svg", "webp"];

/// URL schemes that become links in the exported document.
const LINK_SCHEMES: [&str; 5] = ["http", "https", "mailto", "tel", "ftp"];

/// CSS pixels to points.
const PX_TO_PT: f32 = 0.75;

/// Supplies the SVG of a diagram (Mermaid) for export.
///
/// Closures `Fn(&str) -> Option<String>` implement it.
pub trait DiagramSvg {
    /// SVG markup for the diagram `source`, or `None` when it cannot be drawn.
    fn svg(&self, source: &str) -> Option<String>;
}

impl<F> DiagramSvg for F
where
    F: Fn(&str) -> Option<String>,
{
    fn svg(&self, source: &str) -> Option<String> {
        self(source)
    }
}

/// A [`DiagramSvg`] that draws nothing: diagrams are exported as code blocks.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDiagrams;

impl DiagramSvg for NoDiagrams {
    fn svg(&self, _source: &str) -> Option<String> {
        None
    }
}

/// The Typst source of a document and the virtual files it refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypstDocument {
    /// Typst markup for the main file.
    pub source: String,
    /// In-memory files (diagram SVGs) as `(path, bytes)`; the paths are relative to the
    /// project root and are referenced by the source.
    pub files: Vec<(String, Vec<u8>)>,
}

/// Converts `doc` (parsed from `src`) to Typst.
///
/// `image_exists` tells whether an image destination (percent-decoded, `/`-separated) can be
/// read by the Typst world; destinations it rejects are shown as alt text.
pub fn markdown_to_typst(
    src: &str,
    doc: &Document,
    options: &ExportOptions,
    diagrams: &dyn DiagramSvg,
    image_exists: &dyn Fn(&str) -> bool,
) -> TypstDocument {
    let palette = if options.black_text {
        &BLACK
    } else if options.dark() {
        &DARK
    } else {
        &COLOR
    };
    let mut footnote_defs = HashMap::new();
    for block in doc.iter_blocks() {
        if let BlockKind::FootnoteDefinition { label } = &block.kind {
            footnote_defs.entry(normalize_label(label)).or_insert(block);
        }
    }
    let mut converter = Converter {
        src,
        diagrams,
        image_exists,
        palette,
        text_width_pt: text_width_pt(options),
        footnote_defs,
        footnote_ids: HashMap::new(),
        footnotes_open: Vec::new(),
        files: Vec::new(),
        uses_math: false,
    };
    let body = converter.blocks(&doc.blocks);
    let title = visible_title(options);

    let mut source = preamble(options, palette);
    if converter.uses_math {
        source.push_str(&typst_import());
    }
    if let Some(title) = title {
        source.push_str(&format!("#ssw-title[{}]\n\n", escape_markup(title)));
    }
    if body.is_empty() && title.is_none() {
        // Typst makes no page for a document without content.
        source.push_str("#box()");
    }
    source.push_str(&body);
    source.push('\n');
    TypstDocument {
        source,
        files: converter.files,
    }
}

/// The title to print at the top of the document.
fn visible_title(options: &ExportOptions) -> Option<&str> {
    options
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| options.include_title && !title.is_empty())
}

/// Escapes `text` so that Typst markup shows exactly these characters.
///
/// Every character that can start markup anywhere (`\ # $ * _ ~ ` < @ [ ] / + =`) is escaped,
/// plus `-` unless a letter follows (dashes, `-?`, minus signs, list markers) and the `.` of a
/// leading number, indentation allowed (numbered list marker). Control characters other than
/// tab and line feed are dropped.
pub fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    // Number of digits at the very start of `text`, while no other character was seen.
    let mut leading_digits = Some(0usize);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' | '#' | '$' | '*' | '_' | '~' | '`' | '<' | '@' | '[' | ']' | '/' | '+' | '=' => {
                out.push('\\');
                out.push(c);
            }
            '-' if !chars.peek().is_some_and(|next| next.is_alphabetic()) => {
                out.push_str("\\-");
            }
            '.' if leading_digits.is_some_and(|digits| digits > 0) => out.push_str("\\."),
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
        leading_digits = match leading_digits {
            Some(digits) if c.is_ascii_digit() => Some(digits + 1),
            // Indentation does not stop a line from being a numbered list item.
            Some(0) if c == ' ' || c == '\t' => Some(0),
            _ => None,
        };
    }
    out
}

/// `text` as a Typst string literal, quotes included.
pub fn string_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Colors of the exported document.
struct Palette {
    /// Whether callouts get their own accent colors.
    colored: bool,
    /// Fill of every page; `None` leaves the paper as it is.
    page: Option<&'static str>,
    text: &'static str,
    link: &'static str,
    accent: &'static str,
    quote_text: &'static str,
    quote_bar: &'static str,
    code_bg: &'static str,
    inline_bg: &'static str,
    rule: &'static str,
    zebra: &'static str,
    error: &'static str,
    dim: &'static str,
    /// The Typst expression of a callout's fill; `accent` is the callout's color.
    callout_fill: &'static str,
    /// Accent colors of a note, tip, important, warning and caution callout.
    callouts: [&'static str; 5],
    /// Highlight backgrounds: yellow (also the default), red, orange, green, blue, purple.
    highlights: [&'static str; 6],
}

impl Palette {
    /// The accent color of a callout of `kind`.
    fn callout_accent(&self, kind: CalloutKind) -> &'static str {
        if !self.colored {
            return self.accent;
        }
        self.callouts[match kind {
            CalloutKind::Note => 0,
            CalloutKind::Tip => 1,
            CalloutKind::Important => 2,
            CalloutKind::Warning => 3,
            CalloutKind::Caution => 4,
        }]
    }

    /// The background of a highlight of `color`.
    fn highlight(&self, color: Option<HighlightColor>) -> &'static str {
        self.highlights[match color {
            None | Some(HighlightColor::Yellow) => 0,
            Some(HighlightColor::Red) => 1,
            Some(HighlightColor::Orange) => 2,
            Some(HighlightColor::Green) => 3,
            Some(HighlightColor::Blue) => 4,
            Some(HighlightColor::Purple) => 5,
        }]
    }
}

/// Light callouts and highlights, shared by the light and the black palette.
const LIGHT_CALLOUT_FILL: &str = "accent.lighten(92%)";
const LIGHT_HIGHLIGHTS: [&str; 6] = [
    "#fff3a3", "#ffc9c9", "#ffd8a8", "#b2f2bb", "#a5d8ff", "#d0bfff",
];

const COLOR: Palette = Palette {
    colored: true,
    page: None,
    text: "#1f2328",
    link: "#0969da",
    accent: "#0969da",
    quote_text: "#57606a",
    quote_bar: "#d0d7de",
    code_bg: "#f6f8fa",
    inline_bg: "#eff1f3",
    rule: "#bfc6cf",
    zebra: "#f6f8fa",
    error: "#cf222e",
    dim: "#6e7781",
    callout_fill: LIGHT_CALLOUT_FILL,
    callouts: ["#0969da", "#1a7f37", "#8250df", "#9a6700", "#cf222e"],
    highlights: LIGHT_HIGHLIGHTS,
};

/// The colors of the editor's dark theme (`ssw_ui::theme::Theme::dark`) on dark pages: the
/// surface `#272727` under `#e4e4e4` text, its code, table and callout colors.
const DARK: Palette = Palette {
    colored: true,
    page: Some("#272727"),
    text: "#e4e4e4",
    link: "#4493f8",
    accent: "#4493f8",
    quote_text: "#9e9e9e",
    quote_bar: "#3d4045",
    code_bg: "#303134",
    inline_bg: "#38393d",
    rule: "#3d4045",
    zebra: "#2c2c2c",
    error: "#ff99a4",
    dim: "#9e9e9e",
    callout_fill: "color.mix((accent, 16%), (rgb(\"#272727\"), 84%))",
    callouts: ["#4493f8", "#3fb950", "#a371f7", "#d29922", "#f85149"],
    highlights: [
        "#5e5420", "#6b2a2a", "#6b4620", "#2f5a25", "#254a75", "#5a2f66",
    ],
};

/// Everything black or gray (the "black text" export option).
const BLACK: Palette = Palette {
    colored: false,
    page: None,
    text: "#000000",
    link: "#000000",
    accent: "#000000",
    quote_text: "#000000",
    quote_bar: "#808080",
    code_bg: "#f2f2f2",
    inline_bg: "#ececec",
    rule: "#808080",
    zebra: "#f2f2f2",
    error: "#000000",
    dim: "#404040",
    callout_fill: LIGHT_CALLOUT_FILL,
    callouts: ["#000000"; 5],
    highlights: LIGHT_HIGHLIGHTS,
};

fn callout_label(kind: CalloutKind) -> &'static str {
    match kind {
        CalloutKind::Note => "Note",
        CalloutKind::Tip => "Tip",
        CalloutKind::Important => "Important",
        CalloutKind::Warning => "Warning",
        CalloutKind::Caution => "Caution",
    }
}

/// Typst paper name of a page size.
fn paper(page: PageSize) -> &'static str {
    match page {
        PageSize::A4 => "a4",
        PageSize::Letter => "us-letter",
    }
}

/// Width of the text column in points.
fn text_width_pt(options: &ExportOptions) -> f32 {
    let page_mm = match options.page {
        PageSize::A4 => 210.0,
        PageSize::Letter => 215.9,
    };
    let text_mm = (page_mm - 2.0 * options.margin_mm()).max(10.0);
    text_mm * 72.0 / 25.4
}

/// A number for Typst source: at most two decimals, no exponent.
fn number(value: f32) -> String {
    format!("{}", (value * 100.0).round() / 100.0)
}

/// Page setup, text and show rules, and the helper functions the content refers to.
fn preamble(options: &ExportOptions, palette: &Palette) -> String {
    let document = visible_title(options)
        .or(options
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty()))
        .map(|title| format!("#set document(title: {})\n", string_literal(title)))
        .unwrap_or_default();
    let numbering = if options.page_numbers {
        ", numbering: \"1\", number-align: center"
    } else {
        ""
    };
    let link_rule = if options.black_text {
        "#show link: underline".to_owned()
    } else {
        format!("#show link: set text(fill: rgb(\"{}\"))", palette.link)
    };
    let page_fill = palette
        .page
        .map(|color| format!(", fill: rgb(\"{color}\")"))
        .unwrap_or_default();
    // The document title is user text: it is substituted last so that it is never scanned for
    // placeholders.
    let replacements = [
        ("@PAPER@", paper(options.page).to_owned()),
        ("@MARGIN@", number(options.margin_mm())),
        ("@NUMBERING@", numbering.to_owned()),
        ("@PAGE_FILL@", page_fill),
        ("@CALLOUT_FILL@", palette.callout_fill.to_owned()),
        ("@FONT@", TEXT_FAMILY.to_owned()),
        ("@MONO@", MONO_FAMILY.to_owned()),
        ("@SIZE@", number(options.base_font_pt())),
        ("@LINK_RULE@", link_rule),
        ("@TEXT@", palette.text.to_owned()),
        ("@QUOTE@", palette.quote_text.to_owned()),
        ("@BAR@", palette.quote_bar.to_owned()),
        ("@CODE_BG@", palette.code_bg.to_owned()),
        ("@INLINE_BG@", palette.inline_bg.to_owned()),
        ("@RULE@", palette.rule.to_owned()),
        ("@ZEBRA@", palette.zebra.to_owned()),
        ("@ERROR@", palette.error.to_owned()),
        ("@DIM@", palette.dim.to_owned()),
        ("@DOCUMENT@", document),
    ];
    let mut out = PREAMBLE.to_owned();
    for (placeholder, value) in replacements {
        out = out.replace(placeholder, &value);
    }
    out
}

/// The preamble template; `@NAME@` placeholders are filled by [`preamble`].
const PREAMBLE: &str = r#"@DOCUMENT@#set page(paper: "@PAPER@", margin: @MARGIN@mm@NUMBERING@@PAGE_FILL@)
#set text(font: ("@FONT@", "@MONO@"), size: @SIZE@pt, fill: rgb("@TEXT@"))
#set par(leading: 0.65em, spacing: 1em, justify: false)
#set list(indent: 0.3em, body-indent: 0.5em, marker: ([•], [–]))
#set enum(indent: 0.3em, body-indent: 0.5em)
#set table(stroke: 0.5pt + rgb("@RULE@"), inset: 6pt, fill: (_, y) => if y > 0 and calc.even(y) { rgb("@ZEBRA@") })
#show table.cell.where(y: 0): set text(weight: "bold")
#show heading: set block(above: 1.4em, below: 0.7em)
#show heading.where(level: 1): set text(size: 1.75em)
#show heading.where(level: 2): set text(size: 1.45em)
#show heading.where(level: 3): set text(size: 1.25em)
#show heading.where(level: 4): set text(size: 1.1em)
@LINK_RULE@
#show raw: set text(font: ("@MONO@", "@FONT@"), size: 0.9em)
#show raw.where(block: false): box.with(fill: rgb("@INLINE_BG@"), inset: (x: 3pt, y: 0pt), outset: (y: 3pt), radius: 2pt)
#show raw.where(block: true): block.with(fill: rgb("@CODE_BG@"), inset: 8pt, radius: 3pt, width: 100%)
#let ssw-quote(body) = block(width: 100%, inset: (left: 12pt, y: 2pt), stroke: (left: 2.5pt + rgb("@BAR@")))[
  #set text(fill: rgb("@QUOTE@"))
  #body
]
#let ssw-callout(accent, title, body) = block(width: 100%, fill: @CALLOUT_FILL@, stroke: (left: 3pt + accent), inset: (left: 12pt, right: 10pt, y: 8pt))[
  #text(weight: "bold", fill: accent)[#title]
  #parbreak()
  #body
]
#let ssw-check(done, accent) = box(width: 0.85em, height: 0.85em, baseline: 0.1em, radius: 0.15em, stroke: 0.07em + accent, fill: if done { accent } else { none })
#let ssw-error(body) = text(fill: rgb("@ERROR@"), body)
#let ssw-dim(body) = text(fill: rgb("@DIM@"), body)
#let ssw-html(source, block: false) = text(fill: rgb("@DIM@"), raw(source, block: block))
#let ssw-title(body) = align(center, block(below: 1.2em, text(size: 1.9em, weight: "bold", body)))
"#;

struct Converter<'a> {
    src: &'a str,
    diagrams: &'a dyn DiagramSvg,
    image_exists: &'a dyn Fn(&str) -> bool,
    palette: &'static Palette,
    text_width_pt: f32,
    /// Footnote definition blocks by normalized label.
    footnote_defs: HashMap<String, &'a Block>,
    /// Numbers of the footnotes written so far, by normalized label.
    footnote_ids: HashMap<String, usize>,
    /// Labels of the footnotes whose body is being written.
    footnotes_open: Vec<String>,
    files: Vec<(String, Vec<u8>)>,
    uses_math: bool,
}

impl Converter<'_> {
    fn slice(&self, range: ssw_core::text::TextRange) -> &str {
        range.get(self.src).unwrap_or_default()
    }

    /// Blocks separated by blank lines; blocks without output are left out.
    fn blocks(&mut self, blocks: &[Block]) -> String {
        let parts: Vec<String> = blocks
            .iter()
            .map(|block| self.block(block))
            .filter(|part| !part.is_empty())
            .collect();
        parts.join("\n\n")
    }

    fn block(&mut self, block: &Block) -> String {
        match &block.kind {
            BlockKind::Heading { level, .. } => format!(
                "#heading(level: {})[{}]",
                (*level).clamp(1, 6),
                self.inlines(&block.inlines)
            ),
            BlockKind::Paragraph => self.paragraph(&block.inlines),
            BlockKind::List {
                ordered,
                start,
                delimiter,
                tight,
            } => self.list(block, *ordered, *start, *delimiter, *tight),
            BlockKind::ListItem { .. } => self.blocks(&block.children),
            BlockKind::BlockQuote => format!("#ssw-quote[\n{}\n]", self.blocks(&block.children)),
            BlockKind::Callout { kind, title, .. } => {
                let title = title
                    .and_then(|range| range.get(self.src))
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .unwrap_or(callout_label(*kind));
                format!(
                    "#ssw-callout(rgb({}), {})[\n{}\n]",
                    string_literal(self.palette.callout_accent(*kind)),
                    string_literal(title),
                    self.blocks(&block.children)
                )
            }
            BlockKind::CodeBlock(code) => self.code_block(code),
            BlockKind::MathBlock(math) => self.math_block(math),
            BlockKind::Table(table) => self.table(table),
            BlockKind::ThematicBreak => format!(
                "#line(length: 100%, stroke: 0.5pt + rgb({}))",
                string_literal(self.palette.rule)
            ),
            BlockKind::FrontMatter { .. }
            | BlockKind::FootnoteDefinition { .. }
            | BlockKind::LinkReferenceDefinition(_) => String::new(),
            BlockKind::HtmlBlock(html) => self.html_block(block, html),
            BlockKind::PageBreak => "#pagebreak()".to_owned(),
        }
    }

    fn paragraph(&mut self, inlines: &[Inline]) -> String {
        if let [only] = inlines
            && let InlineKind::Image(image) = &only.kind
        {
            return self.image(image, &only.children, true);
        }
        self.inlines(inlines)
    }

    fn list(
        &mut self,
        block: &Block,
        ordered: bool,
        start: u32,
        delimiter: ListDelimiter,
        tight: bool,
    ) -> String {
        let has_tasks = block.children.iter().any(|item| item_task(item).is_some());
        let bodies: Vec<String> = block
            .children
            .iter()
            .map(|item| self.blocks(&item.children))
            .collect();
        let mut out = String::new();
        if has_tasks {
            out.push_str("#grid(columns: (auto, 1fr), column-gutter: 0.6em, row-gutter: 0.8em,\n");
            for (index, (item, body)) in block.children.iter().zip(&bodies).enumerate() {
                let marker = match item_task(item) {
                    Some(task) => format!(
                        "ssw-check({}, rgb({}))",
                        task.checked,
                        string_literal(self.palette.accent)
                    ),
                    None if ordered => {
                        // `start` and the item count are far below `u32::MAX`.
                        let ordinal = u64::from(start) + index as u64;
                        string_literal(&format!("{ordinal}{}", ordered_suffix(delimiter)))
                    }
                    None => "[•]".to_owned(),
                };
                out.push_str(&format!("{marker}, [{body}],\n"));
            }
        } else if ordered {
            out.push_str(&format!(
                "#enum(tight: {tight}, start: {start}, numbering: \"1{}\",\n",
                ordered_suffix(delimiter)
            ));
            for body in &bodies {
                out.push_str(&format!("[{body}],\n"));
            }
        } else {
            out.push_str(&format!("#list(tight: {tight},\n"));
            for body in &bodies {
                out.push_str(&format!("[{body}],\n"));
            }
        }
        out.push(')');
        out
    }

    fn code_block(&mut self, code: &CodeBlock) -> String {
        let mut text = code.text(self.src);
        if text.ends_with('\n') {
            text.pop();
        }
        let language = code.language(self.src);
        if language.is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid")) {
            let svg = self
                .diagrams
                .svg(&text)
                .filter(|svg| svg_to_scene(svg.as_bytes()).is_ok());
            return match svg {
                Some(svg) => {
                    let name = format!("{DIAGRAM_FILE_PREFIX}{}.svg", self.files.len());
                    let image = format!("#align(center, image({}))", string_literal(&name));
                    self.files.push((name, svg.into_bytes()));
                    image
                }
                None => format!(
                    "#ssw-dim[{UNSUPPORTED_DIAGRAM}]\n\n{}",
                    raw_block(&text, language)
                ),
            };
        }
        raw_block(&text, language)
    }

    fn math_block(&mut self, math: &MathBlock) -> String {
        let latex = math.text(self.src);
        match export_expression(&latex, true) {
            Ok(expression) => {
                self.uses_math = true;
                format!("#{expression}")
            }
            Err(_) => format!("#ssw-error(raw({}, block: true))", string_literal(&latex)),
        }
    }

    fn table(&mut self, table: &Table) -> String {
        let columns = table.header.cells.len().max(1);
        let aligns: String = (0..columns)
            .map(|column| match table.alignments.get(column) {
                Some(Alignment::Left) => "left,",
                Some(Alignment::Center) => "center,",
                Some(Alignment::Right) => "right,",
                Some(Alignment::None) | None => "auto,",
            })
            .collect::<Vec<_>>()
            .join(" ");
        let mut out = format!("#table(columns: {columns}, align: ({aligns}),\n");
        out.push_str(&format!(
            "table.header({}),\n",
            self.row_cells(&table.header, columns)
        ));
        for row in &table.rows {
            out.push_str(&format!("{},\n", self.row_cells(row, columns)));
        }
        out.push(')');
        out
    }

    /// Exactly `columns` content blocks for the cells of `row` (padded or truncated).
    fn row_cells(&mut self, row: &TableRow, columns: usize) -> String {
        let mut cells: Vec<String> = row
            .cells
            .iter()
            .take(columns)
            .map(|cell| format!("[{}]", self.inlines(&cell.inlines)))
            .collect();
        cells.resize(columns, "[]".to_owned());
        cells.join(", ")
    }

    fn html_block(&mut self, block: &Block, html: &HtmlBlock) -> String {
        if !block.inlines.is_empty() {
            return self.paragraph(&block.inlines);
        }
        let mut text = html.text(self.src);
        if text.ends_with('\n') {
            text.pop();
        }
        format!("#ssw-html({}, block: true)", string_literal(&text))
    }

    fn inlines(&mut self, inlines: &[Inline]) -> String {
        let mut out = String::new();
        for inline in inlines {
            self.inline(inline, &mut out);
        }
        out
    }

    /// `#name(args)[children];`
    fn wrap(&mut self, call: &str, node: &Inline, out: &mut String) {
        let body = self.inlines(&node.children);
        out.push_str(&format!("#{call}[{body}];"));
    }

    fn inline(&mut self, node: &Inline, out: &mut String) {
        match &node.kind {
            InlineKind::Text => out.push_str(&escape_markup(self.slice(node.range))),
            InlineKind::Escape => out.push_str(&escape_markup(self.slice(node.content))),
            InlineKind::Entity { decoded } => out.push_str(&escape_markup(decoded)),
            InlineKind::Emphasis => self.wrap("emph", node, out),
            InlineKind::Strong => self.wrap("strong", node, out),
            InlineKind::Underline => self.wrap("underline", node, out),
            InlineKind::Strikethrough => self.wrap("strike", node, out),
            InlineKind::Highlight { color } => {
                let call = format!(
                    "highlight(fill: rgb({}), radius: 2pt, extent: 1pt)",
                    string_literal(self.palette.highlight(*color))
                );
                self.wrap(&call, node, out);
            }
            InlineKind::Code { text } => {
                out.push_str(&format!("#raw({});", string_literal(text)));
            }
            InlineKind::Link(link) => match linkable(&link.dest) {
                Some(dest) if node.children.is_empty() => {
                    out.push_str(&format!("#link({});", string_literal(dest)));
                }
                Some(dest) => {
                    let call = format!("link({})", string_literal(dest));
                    self.wrap(&call, node, out);
                }
                None => out.push_str(&self.inlines(&node.children)),
            },
            InlineKind::Image(image) => {
                let rendered = self.image(image, &node.children, false);
                out.push_str(&rendered);
            }
            InlineKind::WikiLink(wiki) => {
                let text = match (&wiki.alias, &wiki.heading) {
                    (Some(alias), _) => alias.clone(),
                    (None, Some(heading)) if wiki.target.is_empty() => heading.clone(),
                    (None, Some(heading)) => format!("{}#{heading}", wiki.target),
                    (None, None) => wiki.target.clone(),
                };
                out.push_str(&escape_markup(&text));
            }
            InlineKind::FootnoteRef { label } => self.footnote(label, out),
            InlineKind::InlineMath => {
                let latex = self.slice(node.content).to_owned();
                match export_expression(&latex, false) {
                    Ok(expression) => {
                        self.uses_math = true;
                        out.push_str(&format!("#{expression};"));
                    }
                    Err(_) => {
                        out.push_str(&format!("#ssw-error(raw({}));", string_literal(&latex)));
                    }
                }
            }
            InlineKind::HardBreak => out.push_str("\\\n"),
            InlineKind::SoftBreak => out.push('\n'),
            InlineKind::RawHtml => {
                out.push_str(&format!(
                    "#ssw-html({});",
                    string_literal(self.slice(node.range))
                ));
            }
        }
    }

    fn footnote(&mut self, label: &str, out: &mut String) {
        let key = normalize_label(label);
        let open = self.footnotes_open.contains(&key);
        if !open && let Some(id) = self.footnote_ids.get(&key) {
            out.push_str(&format!("#footnote(<ssw-fn-{id}>);"));
            return;
        }
        let Some(def) = self.footnote_defs.get(&key).copied().filter(|_| !open) else {
            // No definition, or a footnote that refers to itself: keep the reference as text.
            out.push_str(&escape_markup(&format!("[^{label}]")));
            return;
        };
        let id = self.footnote_ids.len();
        self.footnote_ids.insert(key.clone(), id);
        self.footnotes_open.push(key);
        let body = self.blocks(&def.children);
        self.footnotes_open.pop();
        out.push_str(&format!("#footnote[{body}]<ssw-fn-{id}>"));
    }

    /// An image as a block (alone in its paragraph) or inline. Images that cannot be shown
    /// become their alt text (or file name) in italics.
    fn image(&mut self, image: &Image, children: &[Inline], block: bool) -> String {
        let mut alt = plain_text(children, self.src);
        if alt.is_empty() {
            alt = self.slice(image.alt).to_owned();
        }
        let dest = normalize_dest(&image.dest);
        if !is_supported_image(&dest) || !(self.image_exists)(&dest) {
            let shown = if alt.is_empty() { &image.dest } else { &alt };
            return format!("#emph[{}];", escape_markup(shown));
        }
        let mut args = vec![string_literal(&dest)];
        if !alt.is_empty() {
            args.push(format!("alt: {}", string_literal(&alt)));
        }
        let max = self.text_width_pt;
        match (image.width, image.height) {
            (Some(width), Some(height)) => {
                let width = width as f32 * PX_TO_PT;
                let height = height as f32 * PX_TO_PT;
                let scale = (max / width).min(1.0);
                args.push(format!("width: {}pt", number(width * scale)));
                args.push(format!("height: {}pt", number(height * scale)));
                args.push("fit: \"stretch\"".to_owned());
            }
            (Some(width), None) => {
                args.push(format!(
                    "width: {}pt",
                    number((width as f32 * PX_TO_PT).min(max))
                ));
            }
            (None, Some(height)) => {
                args.push(format!("height: {}pt", number(height as f32 * PX_TO_PT)));
            }
            (None, None) => {}
        }
        let call = format!("image({})", args.join(", "));
        if block {
            format!("#align(center, {call})")
        } else {
            format!("#box({call});")
        }
    }
}

/// The task marker of a list item.
fn item_task(item: &Block) -> Option<Task> {
    match &item.kind {
        BlockKind::ListItem { task, .. } => *task,
        _ => None,
    }
}

/// Delimiter after the number of an ordered list item.
fn ordered_suffix(delimiter: ListDelimiter) -> char {
    if delimiter == ListDelimiter::Paren {
        ')'
    } else {
        '.'
    }
}

fn raw_block(text: &str, language: Option<&str>) -> String {
    match language {
        Some(language) => format!(
            "#raw({}, block: true, lang: {})",
            string_literal(text),
            string_literal(language)
        ),
        None => format!("#raw({}, block: true)", string_literal(text)),
    }
}

/// The visible text of `inlines` without any formatting.
fn plain_text(inlines: &[Inline], src: &str) -> String {
    let mut out = String::new();
    for inline in inlines {
        match &inline.kind {
            InlineKind::Text => out.push_str(inline.range.get(src).unwrap_or_default()),
            InlineKind::Escape | InlineKind::InlineMath => {
                out.push_str(inline.content.get(src).unwrap_or_default());
            }
            InlineKind::Entity { decoded } => out.push_str(decoded),
            InlineKind::Code { text } => out.push_str(text),
            InlineKind::SoftBreak | InlineKind::HardBreak => out.push(' '),
            InlineKind::WikiLink(wiki) => {
                out.push_str(wiki.alias.as_deref().unwrap_or(&wiki.target));
            }
            InlineKind::FootnoteRef { .. } | InlineKind::RawHtml => {}
            InlineKind::Emphasis
            | InlineKind::Strong
            | InlineKind::Underline
            | InlineKind::Strikethrough
            | InlineKind::Highlight { .. }
            | InlineKind::Link(_)
            | InlineKind::Image(_) => out.push_str(&plain_text(&inline.children, src)),
        }
    }
    out
}

/// The destination when it is an external link Typst can store (`http`, `mailto`, …).
fn linkable(dest: &str) -> Option<&str> {
    let dest = dest.trim();
    let (scheme, rest) = dest.split_once(':')?;
    let known = LINK_SCHEMES
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(scheme));
    (known && !rest.is_empty()).then_some(dest)
}

/// Image destination as a project-relative path: percent-decoded, `/` separators.
fn normalize_dest(dest: &str) -> String {
    percent_decode(dest.trim()).replace('\\', "/")
}

fn is_supported_image(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, extension)| {
        IMAGE_EXTENSIONS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(extension))
    })
}

/// Decodes `%XX` escapes; text that does not decode to UTF-8 is returned unchanged.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

#[cfg(test)]
mod tests {
    use ssw_core::markdown::parse_document;

    use super::*;

    fn convert(markdown: &str) -> TypstDocument {
        convert_with(markdown, &ExportOptions::default(), &NoDiagrams, &|_| false)
    }

    fn convert_with(
        markdown: &str,
        options: &ExportOptions,
        diagrams: &dyn DiagramSvg,
        image_exists: &dyn Fn(&str) -> bool,
    ) -> TypstDocument {
        markdown_to_typst(
            markdown,
            &parse_document(markdown),
            options,
            diagrams,
            image_exists,
        )
    }

    /// The generated content after the preamble.
    fn body(markdown: &str) -> String {
        content(&convert(markdown).source)
    }

    fn content(source: &str) -> String {
        let marker = "#let ssw-title(body)";
        let start = source.find(marker).expect("preamble");
        let rest = &source[start..];
        let end = rest.find('\n').expect("line end");
        rest[end + 1..].trim_end().to_owned()
    }

    #[test]
    fn markup_escaping_table() {
        let table = [
            ("plain text", "plain text"),
            ("", ""),
            (r"a\b", r"a\\b"),
            ("#heading $x$", r"\#heading \$x\$"),
            ("*b* _i_ ~n", r"\*b\* \_i\_ \~n"),
            ("`raw`", r"\`raw\`"),
            ("<label> @ref", r"\<label> \@ref"),
            ("[x](y) [z]", r"\[x\](y) \[z\]"),
            ("a/b // c /* d */", r"a\/b \/\/ c \/\* d \*\/"),
            ("= not heading", r"\= not heading"),
            ("+ not item", r"\+ not item"),
            ("- not item", r"\- not item"),
            ("-", r"\-"),
            ("a -- b --- c", r"a \-\- b \-\-\- c"),
            ("-1 and -? and -x", r"\-1 and \-? and -x"),
            ("state-of-the-art", "state-of-the-art"),
            ("1. not numbered", r"1\. not numbered"),
            ("12. x", r"12\. x"),
            (" 1. indented", r" 1\. indented"),
            ("  \t12. x", "  \t12\\. x"),
            (" a 1. not leading", " a 1. not leading"),
            ("v1.2 and 3.14", "v1.2 and 3.14"),
            (".5 and a.", ".5 and a."),
            ("1.2", r"1\.2"),
            ("https://x.org/a?b=c", r"https:\/\/x.org\/a?b\=c"),
            ("quotes \"a\" 'b'", "quotes \"a\" 'b'"),
            ("tab\tand\nnewline", "tab\tand\nnewline"),
            ("nul\u{0}bell\u{7}cr\r", "nulbellcr"),
            ("ünïcödé ✓ 日本", "ünïcödé ✓ 日本"),
            ("{braces} (parens) & | ^ % :", "{braces} (parens) & | ^ % :"),
        ];
        for (input, expected) in table {
            assert_eq!(escape_markup(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn string_literal_table() {
        let table = [
            ("", "\"\""),
            ("plain", "\"plain\""),
            ("say \"hi\"", r#""say \"hi\"""#),
            (r"back\slash", r#""back\\slash""#),
            ("two\nlines\r\ttab", r#""two\nlines\r\ttab""#),
            ("bell\u{7}nul\u{0}", r#""bell\u{7}nul\u{0}""#),
            ("日本 $x$ #y", "\"日本 $x$ #y\""),
        ];
        for (input, expected) in table {
            assert_eq!(string_literal(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn percent_decoding_table() {
        let table = [
            ("a%20b.png", "a b.png"),
            ("%E2%9C%93.png", "✓.png"),
            ("100%.png", "100%.png"),
            ("%zz", "%zz"),
            ("%+1", "%+1"),
            ("%", "%"),
            ("%2", "%2"),
            ("%ff.png", "%ff.png"),
            ("plain.png", "plain.png"),
        ];
        for (input, expected) in table {
            assert_eq!(percent_decode(input), expected, "input {input:?}");
        }
        assert_eq!(normalize_dest("  img\\a%20b.png "), "img/a b.png");
    }

    #[test]
    fn link_and_image_classification() {
        let links = [
            ("https://example.com/a", Some("https://example.com/a")),
            ("HTTP://example.com", Some("HTTP://example.com")),
            ("mailto:me@example.com", Some("mailto:me@example.com")),
            (" tel:+123 ", Some("tel:+123")),
            ("ftp://host/file", Some("ftp://host/file")),
            ("javascript:alert(1)", None),
            ("data:text/plain,hi", None),
            ("https:", None),
            ("docs/other.md", None),
            ("#section", None),
            ("C:\\x\\y.md", None),
            ("", None),
        ];
        for (dest, expected) in links {
            assert_eq!(linkable(dest), expected, "{dest:?}");
        }
        for ok in ["a.png", "a/b.JPG", "x.jpeg", "x.gif", "d.svg", "w.webp"] {
            assert!(is_supported_image(ok), "{ok}");
        }
        for bad in ["a.bmp", "a.pdf", "noext", "a.png.txt", ""] {
            assert!(!is_supported_image(bad), "{bad}");
        }
    }

    #[test]
    fn preamble_follows_the_options() {
        let source = convert("x").source;
        assert!(source.contains(
            "#set page(paper: \"a4\", margin: 20mm, numbering: \"1\", number-align: center)"
        ));
        assert!(source.contains("size: 11pt, fill: rgb(\"#1f2328\")"));
        assert!(source.contains("#show link: set text(fill: rgb(\"#0969da\"))"));
        assert!(!source.contains("#set document("));
        assert!(!source.contains('@'), "unreplaced placeholder in {source}");

        let options = ExportOptions {
            page: PageSize::Letter,
            margin_mm: 12.346,
            base_font_pt: 9.5,
            page_numbers: false,
            black_text: true,
            light_theme: true,
            include_title: false,
            title: Some("  My \"Doc\"  ".to_owned()),
        };
        let source = convert_with("x", &options, &NoDiagrams, &|_| false).source;
        assert!(
            source.starts_with("#set document(title: \"My \\\"Doc\\\"\")\n"),
            "{source}"
        );
        assert!(source.contains("#set page(paper: \"us-letter\", margin: 12.35mm)\n"));
        assert!(source.contains("size: 9.5pt, fill: rgb(\"#000000\")"));
        assert!(source.contains("#show link: underline\n"));
        assert!(!source.contains("ssw-title["), "{source}");
    }

    #[test]
    fn the_dark_theme_fills_the_pages_and_recolors_the_document() {
        let dark = ExportOptions {
            light_theme: false,
            ..ExportOptions::default()
        };
        let source = convert_with("x", &dark, &NoDiagrams, &|_| false).source;
        assert!(
            source.contains(
                "#set page(paper: \"a4\", margin: 20mm, numbering: \"1\", number-align: center, \
                 fill: rgb(\"#272727\"))"
            ),
            "{source}"
        );
        assert!(
            source.contains("size: 11pt, fill: rgb(\"#e4e4e4\")"),
            "{source}"
        );
        assert!(source.contains("#show link: set text(fill: rgb(\"#4493f8\"))"));
        assert!(source.contains("fill: color.mix((accent, 16%), (rgb(\"#272727\"), 84%))"));
        assert!(
            !source.contains("lighten") && !source.contains('@'),
            "{source}"
        );

        // The light theme keeps the paper as it is.
        let light = convert("x").source;
        assert!(!light.contains("fill: rgb(\"#272727\")"), "{light}");
        assert!(light.contains("fill: accent.lighten(92%)"));

        // Black ink needs light paper: black text wins over the dark theme.
        let black = ExportOptions {
            light_theme: false,
            black_text: true,
            ..ExportOptions::default()
        };
        let source = convert_with("x", &black, &NoDiagrams, &|_| false).source;
        assert!(
            !source.contains("#272727") && !source.contains("#e4e4e4"),
            "{source}"
        );
        assert!(source.contains("size: 11pt, fill: rgb(\"#000000\")"));
    }

    #[test]
    fn dark_callouts_and_highlights_keep_their_text_readable() {
        let dark = ExportOptions {
            light_theme: false,
            ..ExportOptions::default()
        };
        let markdown = "> [!TIP]\n> x\n\n==a== =={red}b== =={orange}c== =={green}d== \
                        =={blue}e== =={purple}f==";
        let source = convert_with(markdown, &dark, &NoDiagrams, &|_| false).source;
        assert!(
            source.contains("#ssw-callout(rgb(\"#3fb950\"), \"Tip\")"),
            "{source}"
        );
        for color in [
            "#5e5420", "#6b2a2a", "#6b4620", "#2f5a25", "#254a75", "#5a2f66",
        ] {
            let call = format!("highlight(fill: rgb(\"{color}\"), radius: 2pt, extent: 1pt)");
            assert!(source.contains(&call), "{color} in {source}");
        }
        for kind in ["NOTE", "TIP", "IMPORTANT", "WARNING", "CAUTION"] {
            let source =
                convert_with(&format!("> [!{kind}]\n> x"), &dark, &NoDiagrams, &|_| false).source;
            let accent = source
                .split("#ssw-callout(rgb(\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .expect("a callout");
            assert!(DARK.callouts.contains(&accent), "{kind}: {accent}");
        }
    }

    #[test]
    fn title_is_printed_only_when_requested() {
        let mut options = ExportOptions {
            title: Some("A *title*".to_owned()),
            ..ExportOptions::default()
        };
        let source = convert_with("text", &options, &NoDiagrams, &|_| false).source;
        assert!(
            source.contains("#ssw-title[A \\*title\\*]\n\ntext\n"),
            "{source}"
        );
        options.include_title = false;
        let source = convert_with("text", &options, &NoDiagrams, &|_| false).source;
        assert!(!source.contains("#ssw-title["));
        assert!(source.contains("#set document(title: \"A *title*\")"));
        options.include_title = true;
        options.title = Some("   ".to_owned());
        let source = convert_with("text", &options, &NoDiagrams, &|_| false).source;
        assert!(!source.contains("#ssw-title[") && !source.contains("#set document("));
    }

    #[test]
    fn empty_documents_still_get_content() {
        assert_eq!(body(""), "#box()");
        assert_eq!(body("---\ntitle: x\n---\n"), "#box()");
        let options = ExportOptions {
            title: Some("T".to_owned()),
            ..ExportOptions::default()
        };
        let source = convert_with("", &options, &NoDiagrams, &|_| false).source;
        assert!(!source.contains("#box()"));
        assert!(source.ends_with("#ssw-title[T]\n\n\n"), "{source:?}");
    }

    #[test]
    fn inline_formatting_markup() {
        let table = [
            (
                "plain *em* and **strong**",
                "plain #emph[em]; and #strong[strong];",
            ),
            ("~under~ ~~gone~~", "#underline[under]; #strike[gone];"),
            (
                "==mark== =={red}r== =={orange}o== =={yellow}y== =={green}g== =={blue}b== =={purple}p==",
                "#highlight(fill: rgb(\"#fff3a3\"), radius: 2pt, extent: 1pt)[mark]; \
                 #highlight(fill: rgb(\"#ffc9c9\"), radius: 2pt, extent: 1pt)[r]; \
                 #highlight(fill: rgb(\"#ffd8a8\"), radius: 2pt, extent: 1pt)[o]; \
                 #highlight(fill: rgb(\"#fff3a3\"), radius: 2pt, extent: 1pt)[y]; \
                 #highlight(fill: rgb(\"#b2f2bb\"), radius: 2pt, extent: 1pt)[g]; \
                 #highlight(fill: rgb(\"#a5d8ff\"), radius: 2pt, extent: 1pt)[b]; \
                 #highlight(fill: rgb(\"#d0bfff\"), radius: 2pt, extent: 1pt)[p];",
            ),
            ("`a \"b\" #c`", "#raw(\"a \\\"b\\\" #c\");"),
            ("a\\*b &amp; &#35; c", r"a\*b & \# c"),
            ("line one\nline two", "line one\nline two"),
            (
                "hard  \nbreak and back\\\nslash",
                "hard\\\nbreak and back\\\nslash",
            ),
            ("a<br>b", "a#ssw-html(\"<br>\");b"),
            (
                "[[Page]] [[Page|alias]] [[#Head]] [[Page#Head]]",
                "Page alias Head Page\\#Head",
            ),
        ];
        for (markdown, expected) in table {
            assert_eq!(body(markdown), expected, "{markdown:?}");
        }
    }

    #[test]
    fn links_become_typst_links_only_for_external_urls() {
        let table = [
            ("[text](https://x.org)", "#link(\"https://x.org\")[text];"),
            (
                "[**b** t](mailto:a@b.c \"title\")",
                "#link(\"mailto:a@b.c\")[#strong[b]; t];",
            ),
            (
                "<https://x.org>",
                "#link(\"https://x.org\")[https:\\/\\/x.org];",
            ),
            ("[text](notes/other.md)", "text"),
            ("[text](#anchor)", "text"),
            ("[](https://x.org)", "#link(\"https://x.org\");"),
            (
                "[ref] and [ref2][ref]\n\n[ref]: https://ref.example/a \"T\"",
                "#link(\"https://ref.example/a\")[ref]; and #link(\"https://ref.example/a\")[ref2];",
            ),
        ];
        for (markdown, expected) in table {
            let got = body(markdown);
            assert!(
                got.starts_with(expected),
                "{markdown:?}\n  got      {got}\n  expected {expected}"
            );
        }
    }

    #[test]
    fn headings_paragraphs_and_breaks() {
        assert_eq!(
            body("# One\n\npara *a*\n\n###### Six\n\n---\n\n<!-- pagebreak -->\n\nend"),
            "#heading(level: 1)[One]\n\npara #emph[a];\n\n#heading(level: 6)[Six]\n\n\
             #line(length: 100%, stroke: 0.5pt + rgb(\"#bfc6cf\"))\n\n#pagebreak()\n\nend"
        );
    }

    #[test]
    fn code_blocks_use_raw_strings() {
        assert_eq!(
            body("```rust title\nfn main() {}\n  \"x\"\n```"),
            "#raw(\"fn main() {}\\n  \\\"x\\\"\", block: true, lang: \"rust\")"
        );
        assert_eq!(body("```\n```"), "#raw(\"\", block: true)");
        assert_eq!(body("~~~\n#[x]\n~~~"), "#raw(\"#[x]\", block: true)");
    }

    #[test]
    fn lists_quotes_and_callouts() {
        assert_eq!(
            body("- a\n- b\n  - c"),
            "#list(tight: true,\n[a],\n[b\n\n#list(tight: true,\n[c],\n)],\n)"
        );
        assert_eq!(
            body("3) x\n4) y"),
            "#enum(tight: true, start: 3, numbering: \"1)\",\n[x],\n[y],\n)"
        );
        assert_eq!(
            body("1. x\n\n2. y"),
            "#enum(tight: false, start: 1, numbering: \"1.\",\n[x],\n[y],\n)"
        );
        assert_eq!(body("> quoted"), "#ssw-quote[\nquoted\n]");
        assert_eq!(
            body("> [!WARNING] Careful *now*\n> body"),
            "#ssw-callout(rgb(\"#9a6700\"), \"Careful *now*\")[\nbody\n]"
        );
        for (kind, label, color) in [
            ("NOTE", "Note", "#0969da"),
            ("TIP", "Tip", "#1a7f37"),
            ("IMPORTANT", "Important", "#8250df"),
            ("WARNING", "Warning", "#9a6700"),
            ("CAUTION", "Caution", "#cf222e"),
        ] {
            assert_eq!(
                body(&format!("> [!{kind}]\n> x")),
                format!("#ssw-callout(rgb(\"{color}\"), \"{label}\")[\nx\n]")
            );
        }
        let options = ExportOptions {
            black_text: true,
            ..ExportOptions::default()
        };
        let black = convert_with("> [!TIP]\n> x", &options, &NoDiagrams, &|_| false).source;
        assert!(black.contains("#ssw-callout(rgb(\"#000000\"), \"Tip\")"));
    }

    #[test]
    fn task_and_mixed_lists_use_a_grid() {
        assert_eq!(
            body("- [ ] open\n- [x] done\n- plain"),
            "#grid(columns: (auto, 1fr), column-gutter: 0.6em, row-gutter: 0.8em,\n\
             ssw-check(false, rgb(\"#0969da\")), [open],\n\
             ssw-check(true, rgb(\"#0969da\")), [done],\n\
             [•], [plain],\n)"
        );
        assert_eq!(
            body("5. [x] a\n6. b"),
            "#grid(columns: (auto, 1fr), column-gutter: 0.6em, row-gutter: 0.8em,\n\
             ssw-check(true, rgb(\"#0969da\")), [a],\n\
             \"6.\", [b],\n)"
        );
        assert_eq!(
            body("1) [ ] a\n2) b"),
            "#grid(columns: (auto, 1fr), column-gutter: 0.6em, row-gutter: 0.8em,\n\
             ssw-check(false, rgb(\"#0969da\")), [a],\n\
             \"2)\", [b],\n)"
        );
    }

    #[test]
    fn list_helpers_cover_non_items() {
        let doc = parse_document("- a\n\ntext");
        assert!(item_task(&doc.blocks[1]).is_none());
        let list = &doc.blocks[0];
        assert!(item_task(&list.children[0]).is_none());
        // A list item rendered on its own shows its content.
        let converter_doc = parse_document("- a *b*");
        let mut converter = Converter {
            src: "- a *b*",
            diagrams: &NoDiagrams,
            image_exists: &|_| false,
            palette: &COLOR,
            text_width_pt: 400.0,
            footnote_defs: HashMap::new(),
            footnote_ids: HashMap::new(),
            footnotes_open: Vec::new(),
            files: Vec::new(),
            uses_math: false,
        };
        assert_eq!(
            converter.block(&converter_doc.blocks[0].children[0]),
            "a #emph[b];"
        );
        assert_eq!(ordered_suffix(ListDelimiter::Paren), ')');
        assert_eq!(ordered_suffix(ListDelimiter::Period), '.');
    }

    #[test]
    fn tables_are_padded_to_the_header_width() {
        assert_eq!(
            body("| A | B | C |\n|:--|:-:|--:|\n| 1 | **2** |\n| x | y | z | extra |\n"),
            "#table(columns: 3, align: (left, center, right,),\n\
             table.header([A], [B], [C]),\n\
             [1], [#strong[2];], [],\n\
             [x], [y], [z],\n)"
        );
        assert_eq!(
            body("a | b\n-|-\n"),
            "#table(columns: 2, align: (auto, auto,),\ntable.header([a], [b]),\n)"
        );
    }

    #[test]
    fn math_is_checked_before_it_is_embedded() {
        let source =
            convert("inline $x^2$ and\n\n$$\\frac{a}{b}$$\n\n$\\xrightarrow{f}$ and $\\frac{$")
                .source;
        assert!(
            source.contains("#import \"mitex/mod.typ\": mitex-scope\n"),
            "{source}"
        );
        assert!(
            source.contains("#eval(\"")
                && source.contains(", mode: \"math\", scope: mitex-scope);"),
            "{source}"
        );
        assert!(
            source.contains("#eval(\"$ ") && source.contains(" $\", scope: mitex-scope)"),
            "{source}"
        );
        assert!(
            source.contains("#ssw-error(raw(\"\\\\xrightarrow{f}\"));"),
            "{source}"
        );
        assert!(
            source.contains("#ssw-error(raw(\"\\\\frac{\"));"),
            "{source}"
        );
        let broken = convert("$$\n\\begin{aligned}\n$$").source;
        assert!(
            broken.contains("#ssw-error(raw(\"\\\\begin{aligned}\", block: true))"),
            "{broken}"
        );
        assert!(!convert("no math").source.contains("mitex"));
    }

    #[test]
    fn images_need_a_supported_readable_file() {
        let exists = |dest: &str| dest == "pic.png" || dest == "my pic.png" || dest == "a.bmp";
        let options = ExportOptions::default();
        let md = |markdown: &str| {
            content(&convert_with(markdown, &options, &NoDiagrams, &exists).source)
        };
        assert_eq!(
            md("![alt *text*](pic.png)"),
            "#align(center, image(\"pic.png\", alt: \"alt text\"))"
        );
        assert_eq!(
            md("![](my%20pic.png)"),
            "#align(center, image(\"my pic.png\"))"
        );
        assert_eq!(
            md("see ![icon](pic.png) here"),
            "see #box(image(\"pic.png\", alt: \"icon\")); here"
        );
        assert_eq!(md("![gone](missing.png)"), "#emph[gone];");
        assert_eq!(md("![](missing.png)"), "#emph[missing.png];");
        assert_eq!(md("![bmp](a.bmp)"), "#emph[bmp];");
        assert_eq!(
            md("<img src=\"pic.png\" alt=\"html\" width=\"100\">"),
            "#align(center, image(\"pic.png\", alt: \"html\", width: 75pt))"
        );
    }

    #[test]
    fn image_sizes_are_scaled_to_the_text_width() {
        let exists = |_: &str| true;
        let options = ExportOptions::default();
        let md = |markdown: &str| {
            content(&convert_with(markdown, &options, &NoDiagrams, &exists).source)
        };
        let max = number(text_width_pt(&options));
        assert_eq!(
            md("![a|200](p.png)"),
            "#align(center, image(\"p.png\", alt: \"a\", width: 150pt))"
        );
        assert_eq!(
            md("![a|2000](p.png)"),
            format!("#align(center, image(\"p.png\", alt: \"a\", width: {max}pt))")
        );
        assert_eq!(
            md("![a|200x100](p.png)"),
            "#align(center, image(\"p.png\", alt: \"a\", width: 150pt, height: 75pt, fit: \"stretch\"))"
        );
        let scaled = md("![a|2000x1000](p.png)");
        assert!(
            scaled.contains(&format!("width: {max}pt, height: ")),
            "{scaled}"
        );
        let html = md("<img src=\"p.png\" height=\"40\">");
        assert!(html.contains("height: 30pt"), "{html}");
        let letter = ExportOptions {
            page: PageSize::Letter,
            margin_mm: 30.0,
            ..ExportOptions::default()
        };
        assert!(text_width_pt(&letter) < text_width_pt(&options));
        let tiny = ExportOptions {
            margin_mm: 500.0,
            ..ExportOptions::default()
        };
        assert!(text_width_pt(&tiny) > 0.0);
    }

    #[test]
    fn mermaid_diagrams_become_svg_files_or_a_noted_code_block() {
        let md = "```mermaid\ngraph TD\n  A-->B\n```\n\nafter\n\n```mermaid\nflowchart LR\n```";
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\"/></svg>";
        let seen = std::cell::RefCell::new(Vec::new());
        let diagrams = |source: &str| {
            seen.borrow_mut().push(source.to_owned());
            Some(svg.to_owned())
        };
        let doc = convert_with(md, &ExportOptions::default(), &diagrams, &|_| false);
        assert_eq!(*seen.borrow(), ["graph TD\n  A-->B", "flowchart LR"]);
        assert_eq!(
            content(&doc.source),
            "#align(center, image(\"ssw-diagram-0.svg\"))\n\nafter\n\n#align(center, image(\"ssw-diagram-1.svg\"))"
        );
        assert_eq!(doc.files.len(), 2);
        assert_eq!(
            doc.files[0],
            ("ssw-diagram-0.svg".to_owned(), svg.as_bytes().to_vec())
        );
        assert_eq!(doc.files[1].0, "ssw-diagram-1.svg");

        // Not drawable: no diagram, or an SVG that does not parse.
        let broken = |_: &str| Some("<svg".to_owned());
        let undrawable: [&dyn DiagramSvg; 2] = [&NoDiagrams, &broken];
        for diagrams in undrawable {
            let doc = convert_with(
                "```mermaid\nsankey-beta\n```",
                &ExportOptions::default(),
                diagrams,
                &|_| false,
            );
            assert!(doc.files.is_empty());
            assert_eq!(
                content(&doc.source),
                "#ssw-dim[Unsupported diagram]\n\n#raw(\"sankey-beta\", block: true, lang: \"mermaid\")"
            );
        }
        // Other languages never reach the diagram renderer.
        let requests = seen.borrow().len();
        let doc = convert_with(
            "```rust\nx\n```",
            &ExportOptions::default(),
            &diagrams,
            &|_| false,
        );
        assert_eq!(seen.borrow().len(), requests);
        assert!(doc.files.is_empty());
        assert_eq!(
            content(&doc.source),
            "#raw(\"x\", block: true, lang: \"rust\")"
        );
    }

    #[test]
    fn footnotes_are_written_once_and_referenced_afterwards() {
        let md = "A[^1] and B[^two] and again A[^1] and C[^missing]\n\n[^1]: First *note*\n\n[^two]: Second with [^1]\n\n    more\n";
        let got = body(md);
        assert!(got.starts_with("A#footnote[First #emph[note];]<ssw-fn-0> and B#footnote[Second with #footnote(<ssw-fn-0>);"), "{got}");
        assert!(
            got.contains("<ssw-fn-1> and again A#footnote(<ssw-fn-0>); and C"),
            "{got}"
        );
        // The definitions themselves are not part of the flow.
        assert!(!got.contains("First #emph[note];\n"), "{got}");
        let selfref = body("X[^a]\n\n[^a]: loops back to [^a]\n");
        assert_eq!(selfref, "X#footnote[loops back to \\[^a\\]]<ssw-fn-0>");
    }

    #[test]
    fn html_and_hidden_blocks() {
        assert_eq!(
            body("<div>\nhi *x*\n</div>"),
            "#ssw-html(\"<div>\\nhi *x*\\n</div>\", block: true)"
        );
        assert_eq!(body("[id]: https://x.org\n\ntext"), "text");
        assert_eq!(body("---\nt: 1\n---\n\nbody"), "body");
    }

    #[test]
    fn plain_text_flattens_inline_formatting() {
        let md = "x *a* **b** ~c~ ~~d~~ ==e== `f` [g](https://h.io) ![i](p.png) \\* &amp; $m$ [[W|al]] [[T]] <b>\nsoft  \nhard";
        let doc = parse_document(md);
        let text = plain_text(&doc.blocks[0].inlines, md);
        assert_eq!(text, "x a b c d e f g i * & m al T  soft hard");
    }
}
