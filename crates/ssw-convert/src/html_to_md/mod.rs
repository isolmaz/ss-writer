//! HTML → Markdown for paste (spec §2.3): browsers, Microsoft Word (`mso-list` paragraphs),
//! Google Docs, code copied from editors.
//!
//! The HTML is parsed with html5ever and walked once. Block elements become [`Block`]s, inline
//! content becomes [`Inline`] runs that carry their formatting (bold/italic/underline/strike/
//! highlight/code/link) from inline `style` attributes and tags; [`render`] turns both into
//! Markdown. Word's flat list paragraphs are regrouped into nested lists by [`Sink`].

mod dom;
mod gdocs;
mod render;
mod word;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use markup5ever_rcdom::{Handle, NodeData};

use crate::assets::{extension_for_mime, is_external_url, sniff_image_mime};
use crate::import::{Asset, ImportResult, LossKind, fit_table};
use dom::{Style, attr, children, has_class, tag, text_content, text_of};
use render::{
    Align, Block, Fmt, Inline, Item, List, Table, code_span, destination, escape_inline,
    escape_line_starts, fenced, render_document, render_inline, render_paragraphs,
};

/// Elements nested deeper than this are flattened to their text (keeps the walk's stack bounded).
const MAX_DEPTH: usize = 100;
/// Largest `colspan`/`rowspan` honoured (the HTML standard clamps `colspan` to 1000 too).
const MAX_SPAN: usize = 1000;

/// How HTML found on the clipboard should be pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlPaste {
    /// Code copied from an editor (or any single monospace block): insert as plain text, inside
    /// a fenced block when the caret is not in one (see [`fenced_code`]).
    PlainText(String),
    /// Rich content converted to Markdown; images that were embedded as `data:` URIs are in
    /// `assets`.
    Markdown(ImportResult),
}

/// Decides how to paste `html` and converts it.
///
/// `vscode_editor_data` is true when the clipboard also carries the `vscode-editor-data` format.
/// Images with `data:` URIs become assets named `image-N.ext` inside `asset_dir` (a relative
/// directory such as `assets`; empty for none).
pub fn convert_html_paste(html: &str, vscode_editor_data: bool, asset_dir: &str) -> HtmlPaste {
    let document = dom::parse(html);
    let code = extract_code(&document);
    if vscode_editor_data || (code.ink && code.mono && code.plain) {
        return HtmlPaste::PlainText(code.into_text());
    }
    HtmlPaste::Markdown(convert(&document, asset_dir))
}

/// Converts HTML to Markdown without the code-block detection of [`convert_html_paste`].
pub fn html_to_markdown(html: &str, asset_dir: &str) -> ImportResult {
    convert(&dom::parse(html), asset_dir)
}

/// Wraps `text` in a fenced code block (the fence outgrows any backtick run in the text).
pub fn fenced_code(text: &str) -> String {
    fenced(text, "")
}

fn convert(document: &Handle, asset_dir: &str) -> ImportResult {
    let mut converter = Converter {
        result: ImportResult::default(),
        asset_dir: asset_dir.trim_end_matches('/').to_string(),
        images: 0,
        gdocs: gdocs::detect(document),
        depth: 0,
        in_cell: false,
        cells_used: 0,
    };
    let mut sink = Sink::default();
    let mut acc = Vec::new();
    converter.walk(document, &mut sink, &mut acc, &Fmt::default());
    converter.flush(&mut acc, &mut sink);
    converter.result.markdown = render_document(&sink.finish());
    converter.result
}

// ---------------------------------------------------------------------------------------------
// Block collection
// ---------------------------------------------------------------------------------------------

/// Where a flat (Word / Google Docs) list item belongs.
#[derive(Debug)]
struct ListSpec {
    level: usize,
    /// Items with different keys at the same level are separate lists.
    key: Option<String>,
    ordered: bool,
    paren: bool,
    start: u64,
}

#[derive(Debug)]
struct OpenList {
    level: usize,
    key: Option<String>,
    list: List,
}

/// Collects blocks and regroups flat list items into nested lists. Any non-list block ends the
/// lists that are still open.
#[derive(Debug, Default)]
struct Sink {
    blocks: Vec<Block>,
    open: Vec<OpenList>,
}

impl Sink {
    fn push(&mut self, block: Block) {
        self.close_lists();
        if block == Block::PageBreak && self.blocks.last() == Some(&Block::PageBreak) {
            return;
        }
        self.blocks.push(block);
    }

    fn push_item(&mut self, spec: ListSpec, item: Item) {
        while let Some(deeper) = self.open.pop_if(|o| o.level > spec.level) {
            self.close_level(deeper);
        }
        let same_list = self.open.last().is_some_and(|o| {
            o.level == spec.level
                && o.key == spec.key
                && o.list.ordered == spec.ordered
                && o.list.paren == spec.paren
        });
        if !same_list {
            if let Some(same_level) = self.open.pop_if(|o| o.level == spec.level) {
                self.close_level(same_level);
            }
            self.open.push(OpenList {
                level: spec.level,
                key: spec.key,
                list: List {
                    ordered: spec.ordered,
                    start: spec.start,
                    paren: spec.paren,
                    items: Vec::new(),
                },
            });
        }
        let top = self.open.last_mut().expect("the item's list is open");
        top.list.items.push(item);
    }

    /// Closes the innermost open list: it nests in the last item of the list below, or ends the
    /// run of lists.
    fn close_level(&mut self, closed: OpenList) {
        let block = Block::List(closed.list);
        match self
            .open
            .last_mut()
            .and_then(|parent| parent.list.items.last_mut())
        {
            Some(item) => item.blocks.push(block),
            None => self.blocks.push(block),
        }
    }

    fn close_lists(&mut self) {
        while let Some(innermost) = self.open.pop() {
            self.close_level(innermost);
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.close_lists();
        self.blocks
    }
}

// ---------------------------------------------------------------------------------------------
// The converter
// ---------------------------------------------------------------------------------------------

struct Converter {
    result: ImportResult,
    asset_dir: String,
    images: usize,
    gdocs: bool,
    depth: usize,
    /// Inside a table cell: line breaks become `<br>` and `|` is escaped.
    in_cell: bool,
    /// Cells of the tables kept so far, padding included.
    cells_used: usize,
}

impl Converter {
    fn walk_children(&mut self, node: &Handle, sink: &mut Sink, acc: &mut Vec<Inline>, fmt: &Fmt) {
        for child in children(node) {
            self.walk(&child, sink, acc, fmt);
        }
    }

    fn walk(&mut self, node: &Handle, sink: &mut Sink, acc: &mut Vec<Inline>, fmt: &Fmt) {
        match &node.data {
            NodeData::Text { contents } => self.text(&contents.borrow(), acc, fmt),
            NodeData::Element { .. } if self.depth >= MAX_DEPTH => {
                self.text(&text_content(node), acc, fmt);
            }
            NodeData::Element { .. } => {
                self.depth += 1;
                self.element(node, sink, acc, fmt);
                self.depth -= 1;
            }
            NodeData::Document => self.walk_children(node, sink, acc, fmt),
            _ => {}
        }
    }

    fn text(&mut self, raw: &str, acc: &mut Vec<Inline>, fmt: &Fmt) {
        let collapsed = collapse_space(raw);
        if !collapsed.is_empty() {
            acc.push(Inline::Text(collapsed, fmt.clone()));
        }
    }

    fn element(&mut self, node: &Handle, sink: &mut Sink, acc: &mut Vec<Inline>, parent: &Fmt) {
        let name = tag(node).unwrap_or_default();
        let style = Style::of(node);
        if SKIPPED.contains(&name) || is_hidden(node, &style) || word::is_marker(&style) {
            return;
        }
        if matches!(name, "sup" | "sub")
            || matches!(style.get("vertical-align"), Some("super" | "sub"))
        {
            self.result.record_loss(LossKind::SubSuperscript, 1, None);
        }
        let fmt = self.derive_fmt(node, name, &style, parent);
        let block = matches!(
            name,
            "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "table"
        );
        if block && forced_break(&style, "page-break-before", "break-before") {
            self.page_break(sink, acc);
        }
        match name {
            "br" => {
                if forced_break(&style, "page-break-before", "break-before") {
                    self.page_break(sink, acc);
                } else if !has_class(node, "Apple-interchange-newline") {
                    acc.push(Inline::Break);
                }
            }
            "img" => self.image(node, acc, &fmt),
            "hr" => {
                self.flush(acc, sink);
                sink.push(Block::Rule);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.heading(node, heading_level(name), sink, acc, &fmt);
            }
            "p" => self.paragraph(node, &style, sink, acc, &fmt),
            "ul" | "ol" => self.list(node, name == "ol", sink, acc, &fmt),
            "pre" => self.code_block(node, sink, acc),
            "blockquote" => self.quote(node, sink, acc, &fmt),
            "table" => self.table(node, sink, acc, &fmt),
            _ if is_container(name) => {
                self.flush(acc, sink);
                self.walk_children(node, sink, acc, &fmt);
                self.flush(acc, sink);
            }
            _ => self.walk_children(node, sink, acc, &fmt),
        }
        if block && forced_break(&style, "page-break-after", "break-after") {
            self.page_break(sink, acc);
        }
    }

    fn page_break(&mut self, sink: &mut Sink, acc: &mut Vec<Inline>) {
        self.flush(acc, sink);
        sink.push(Block::PageBreak);
    }

    /// Turns pending inline content into paragraph blocks.
    fn flush(&mut self, acc: &mut Vec<Inline>, sink: &mut Sink) {
        for text in render_paragraphs(&std::mem::take(acc), self.in_cell) {
            sink.push(Block::Paragraph(escape_line_starts(&text)));
        }
    }

    /// Formatting of an element: tags and inline CSS on top of what the parent already has.
    fn derive_fmt(&self, node: &Handle, name: &str, style: &Style, parent: &Fmt) -> Fmt {
        let mut fmt = parent.clone();
        match style.get("font-weight").and_then(bold_weight) {
            Some(bold) => fmt.bold = bold,
            None => fmt.bold |= matches!(name, "b" | "strong"),
        }
        match style.get("font-style") {
            Some("italic" | "oblique") => fmt.italic = true,
            Some("normal") => fmt.italic = false,
            _ => fmt.italic |= matches!(name, "i" | "em" | "cite"),
        }
        let decorations = [
            style.get("text-decoration"),
            style.get("text-decoration-line"),
        ];
        let decorated = |kind: &str| {
            decorations
                .iter()
                .flatten()
                .any(|d| d.split_whitespace().any(|t| t == kind))
        };
        fmt.underline |= decorated("underline") || matches!(name, "u" | "ins");
        fmt.strike |= decorated("line-through") || matches!(name, "s" | "strike" | "del");
        fmt.highlight |= name == "mark"
            || word::is_highlight(style)
            || (self.gdocs && gdocs::is_highlight(style));
        match style.get("font-family") {
            Some(family) => fmt.code = is_monospace(family),
            None => fmt.code |= matches!(name, "code" | "kbd" | "samp" | "tt"),
        }
        let href = if name == "a" {
            attr(node, "href")
        } else {
            None
        };
        if let Some(url) = href
            .map(|h| h.trim().to_string())
            .filter(|h| is_safe_href(h))
        {
            fmt.link = Some(url);
        }
        // Links are underlined by nature; Google Docs and browsers add the decoration explicitly.
        if fmt.link.is_some() {
            fmt.underline = false;
        }
        fmt
    }

    // ----- headings and paragraphs -----

    fn heading(
        &mut self,
        node: &Handle,
        level: u8,
        sink: &mut Sink,
        acc: &mut Vec<Inline>,
        fmt: &Fmt,
    ) {
        self.flush(acc, sink);
        let mut inner = Vec::new();
        self.walk_children(node, sink, &mut inner, fmt);
        self.push_heading(level, inner, sink);
    }

    /// Heading text never keeps bold (headings are bold already) or line breaks.
    fn push_heading(&self, level: u8, inner: Vec<Inline>, sink: &mut Sink) {
        let items: Vec<Inline> = inner
            .into_iter()
            .map(|item| match item {
                Inline::Text(text, mut fmt) => {
                    fmt.bold = false;
                    Inline::Text(text, fmt)
                }
                Inline::Break => Inline::Text(" ".to_string(), Fmt::default()),
                image => image,
            })
            .collect();
        let text = render_inline(&items, self.in_cell);
        if !text.is_empty() {
            sink.push(Block::Heading(level, text));
        }
    }

    fn paragraph(
        &mut self,
        node: &Handle,
        style: &Style,
        sink: &mut Sink,
        acc: &mut Vec<Inline>,
        fmt: &Fmt,
    ) {
        self.flush(acc, sink);
        let mut inner = Vec::new();
        self.walk_children(node, sink, &mut inner, fmt);
        if let Some(info) = word::list_info(style) {
            self.word_list_item(node, info, &inner, sink);
        } else if let Some(level) = word::heading_level(&attr(node, "class").unwrap_or_default()) {
            self.push_heading(level, inner, sink);
        } else {
            self.flush(&mut inner, sink);
        }
    }

    /// A Word list paragraph: marker text decides bullet/ordered, `mso-list` the level.
    fn word_list_item(
        &mut self,
        node: &Handle,
        info: word::ListInfo,
        inner: &[Inline],
        sink: &mut Sink,
    ) {
        let paragraphs = render_paragraphs(inner, self.in_cell);
        if paragraphs.is_empty() {
            return;
        }
        let marker = word::parse_marker(&word::find_marker(node).unwrap_or_default());
        if marker.lettered {
            self.result
                .record_loss(LossKind::ListNumberingStyle, 1, None);
        }
        let blocks = paragraphs
            .iter()
            .map(|p| Block::Paragraph(escape_line_starts(p)))
            .collect();
        let spec = ListSpec {
            level: info.level,
            key: Some(info.key),
            ordered: marker.ordered,
            paren: marker.paren,
            start: marker.start,
        };
        sink.push_item(spec, Item { task: None, blocks });
    }

    // ----- lists -----

    fn list(
        &mut self,
        node: &Handle,
        ordered: bool,
        sink: &mut Sink,
        acc: &mut Vec<Inline>,
        fmt: &Fmt,
    ) {
        self.flush(acc, sink);
        if matches!(attr(node, "type").as_deref(), Some("a" | "A" | "i" | "I")) {
            self.result
                .record_loss(LossKind::ListNumberingStyle, 1, None);
        }
        let start = list_start(node);
        if gdocs::has_levels(node) {
            self.list_flat(node, ordered, sink, fmt);
            return;
        }
        let mut items = Vec::new();
        for child in children(node) {
            match tag(&child) {
                Some("li") => items.extend(self.list_item(&child, fmt)),
                Some("ul" | "ol") => {
                    // Nested list written as a child of the list: it belongs to the item before.
                    let mut inner = Sink::default();
                    self.walk(&child, &mut inner, &mut Vec::new(), fmt);
                    let blocks = inner.finish();
                    match items.last_mut() {
                        Some(last) => last.blocks.extend(blocks),
                        None => items.push(Item { task: None, blocks }),
                    }
                }
                _ => {}
            }
        }
        if !items.is_empty() {
            sink.push(Block::List(List {
                ordered,
                start,
                paren: false,
                items,
            }));
        }
    }

    /// Lists whose items declare their nesting level (`aria-level`, Google Docs): the items are
    /// placed by level, wherever the `ul`/`ol` elements sit.
    fn list_flat(&mut self, node: &Handle, ordered: bool, sink: &mut Sink, fmt: &Fmt) {
        for child in children(node) {
            match tag(&child) {
                Some("li") => {
                    let level = gdocs::aria_level(&child).unwrap_or(1) - 1;
                    if let Some(item) = self.list_item(&child, fmt) {
                        let spec = ListSpec {
                            level,
                            key: None,
                            ordered,
                            paren: false,
                            start: list_start(node),
                        };
                        sink.push_item(spec, item);
                    }
                }
                Some(inner @ ("ul" | "ol")) => self.list_flat(&child, inner == "ol", sink, fmt),
                _ => {}
            }
        }
    }

    /// One list item; `None` for items without content.
    fn list_item(&mut self, node: &Handle, fmt: &Fmt) -> Option<Item> {
        let style = Style::of(node);
        let fmt = self.derive_fmt(node, "li", &style, fmt);
        let mut inner = Sink::default();
        let mut acc = Vec::new();
        self.walk_children(node, &mut inner, &mut acc, &fmt);
        self.flush(&mut acc, &mut inner);
        let item = Item {
            task: find_checkbox(node, 0),
            blocks: inner.finish(),
        };
        (!item.blocks.is_empty() || item.task.is_some()).then_some(item)
    }

    // ----- other blocks -----

    fn code_block(&mut self, node: &Handle, sink: &mut Sink, acc: &mut Vec<Inline>) {
        self.flush(acc, sink);
        let text = pre_text(node);
        let text = text.trim_end_matches('\n');
        if !text.trim().is_empty() {
            sink.push(Block::Code {
                lang: code_language(node),
                text: text.to_string(),
            });
        }
    }

    fn quote(&mut self, node: &Handle, sink: &mut Sink, acc: &mut Vec<Inline>, fmt: &Fmt) {
        self.flush(acc, sink);
        let mut inner = Sink::default();
        let mut inner_acc = Vec::new();
        self.walk_children(node, &mut inner, &mut inner_acc, fmt);
        self.flush(&mut inner_acc, &mut inner);
        let blocks = inner.finish();
        if !blocks.is_empty() {
            sink.push(Block::Quote(blocks));
        }
    }

    // ----- tables -----

    fn table(&mut self, node: &Handle, sink: &mut Sink, acc: &mut Vec<Inline>, fmt: &Fmt) {
        self.flush(acc, sink);
        let rows: Vec<Vec<Handle>> = table_rows(node)
            .iter()
            .map(row_cells)
            .filter(|cells| !cells.is_empty())
            .collect();
        match rows.iter().flatten().collect::<Vec<_>>().as_slice() {
            [] => return,
            // A table with a single cell is page layout: keep its content.
            [only] => {
                self.walk_children(only, sink, acc, fmt);
                self.flush(acc, sink);
                return;
            }
            _ => {}
        }
        let mut grid: Vec<Vec<String>> = Vec::new();
        let mut align: Vec<Align> = Vec::new();
        // Rows still covered by a `rowspan` from above, per column.
        let mut covered: Vec<usize> = Vec::new();
        let mut merged = 0;
        for cells in &rows {
            let mut line: Vec<String> = Vec::new();
            let mut column = fill_covered(&mut line, &mut covered, 0);
            for cell in cells {
                let style = Style::of(cell);
                let colspan = span(cell, "colspan");
                let rowspan = span(cell, "rowspan");
                merged += usize::from(colspan > 1 || rowspan > 1);
                let text = self.cell_text(cell, &style, fmt);
                line.push(text);
                line.extend(std::iter::repeat_n(String::new(), colspan - 1));
                if align.len() < column + colspan {
                    align.resize(column + colspan, Align::None);
                }
                if align[column] == Align::None {
                    align[column] = cell_align(cell, &style);
                }
                if covered.len() < column + colspan {
                    covered.resize(column + colspan, 0);
                }
                covered[column..column + colspan].fill(rowspan - 1);
                column = fill_covered(&mut line, &mut covered, column + colspan);
            }
            grid.push(line);
        }
        self.result.record_loss(LossKind::MergedCells, merged, None);
        if fit_table(&mut grid, &mut self.cells_used) {
            self.result.record_loss(LossKind::CorruptParts, 1, None);
        }
        let columns = grid.iter().map(Vec::len).max().unwrap_or(0);
        for line in &mut grid {
            line.resize(columns, String::new());
        }
        align.resize(columns, Align::None);
        if grid.iter().flatten().any(|cell| !cell.is_empty()) {
            sink.push(Block::Table(Table { align, rows: grid }));
        }
    }

    /// Cell content as one line of Markdown (paragraphs joined by `<br>`).
    fn cell_text(&mut self, cell: &Handle, style: &Style, fmt: &Fmt) -> String {
        let fmt = self.derive_fmt(cell, tag(cell).unwrap_or_default(), style, fmt);
        let outer = std::mem::replace(&mut self.in_cell, true);
        let mut inner = Sink::default();
        let mut acc = Vec::new();
        self.walk_children(cell, &mut inner, &mut acc, &fmt);
        self.flush(&mut acc, &mut inner);
        self.in_cell = outer;
        let mut parts = Vec::new();
        self.flatten(&inner.finish(), &mut parts);
        if parts.len() > 1 {
            self.result
                .record_loss(LossKind::MultiParagraphCells, 1, None);
        }
        parts.join("<br>")
    }

    /// Reduces blocks to lines of inline Markdown for a table cell.
    fn flatten(&mut self, blocks: &[Block], parts: &mut Vec<String>) {
        for block in blocks {
            match block {
                Block::Heading(_, text) | Block::Paragraph(text) => parts.push(text.clone()),
                Block::Code { text, .. } => {
                    parts.push(code_span(&text.replace('\n', " ").replace('|', "\\|")));
                }
                Block::Quote(inner) => self.flatten(inner, parts),
                Block::List(list) => {
                    for item in &list.items {
                        let mut lines = Vec::new();
                        self.flatten(&item.blocks, &mut lines);
                        parts.push(format!("- {}", lines.join(" ")));
                    }
                }
                Block::Table(table) => {
                    self.result.record_loss(LossKind::NestedTables, 1, None);
                    let cells = table.rows.iter().flatten().filter(|c| !c.is_empty());
                    parts.push(cells.cloned().collect::<Vec<_>>().join(" "));
                }
                Block::Rule | Block::PageBreak => {}
            }
        }
    }

    // ----- images -----

    fn image(&mut self, node: &Handle, acc: &mut Vec<Inline>, fmt: &Fmt) {
        let src = attr(node, "src").unwrap_or_default();
        let Some(dest) = self.image_destination(src.trim()) else {
            return;
        };
        let alt = collapse_space(&attr(node, "alt").unwrap_or_default());
        let size = match (pixels(node, "width"), pixels(node, "height")) {
            (Some(w), Some(h)) => format!("|{w}x{h}"),
            (Some(w), None) => format!("|{w}"),
            _ => String::new(),
        };
        let markdown = format!(
            "![{}{size}]({})",
            escape_inline(alt.trim(), false),
            destination(&dest)
        );
        acc.push(Inline::Image {
            markdown,
            link: fmt.link.clone(),
        });
    }

    /// Where the image goes in the Markdown: `data:` URIs are extracted to assets, remote and
    /// relative sources are kept, local-machine and opaque schemes (`file:`, `cid:`, `blob:`) are
    /// dropped and reported.
    fn image_destination(&mut self, src: &str) -> Option<String> {
        if src.is_empty() {
            return None;
        }
        if let Some(data) = src.strip_prefix("data:") {
            return self.data_image(data);
        }
        if !is_external_url(src) {
            return Some(src.to_string());
        }
        let lower = src.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return Some(src.to_string());
        }
        self.unsupported_image()
    }

    fn data_image(&mut self, data: &str) -> Option<String> {
        let (meta, payload) = data.split_once(',').unwrap_or_default();
        let mut parameters = meta.split(';');
        let mime = parameters
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let is_base64 = parameters.any(|p| p.trim().eq_ignore_ascii_case("base64"));
        let compact: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = if is_base64 {
            STANDARD.decode(compact).ok()
        } else {
            None
        };
        let extension = bytes
            .as_deref()
            .and_then(|bytes| match extension_for_mime(&mime) {
                "bin" => sniff_image_mime(bytes).map(extension_for_mime),
                known => Some(known),
            });
        let (Some(bytes), Some(extension)) = (bytes, extension) else {
            return self.unsupported_image();
        };
        self.images += 1;
        let name = format!("image-{}.{extension}", self.images);
        let relative_path = if self.asset_dir.is_empty() {
            name
        } else {
            format!("{}/{name}", self.asset_dir)
        };
        self.result.assets.push(Asset {
            relative_path: relative_path.clone(),
            bytes,
        });
        Some(relative_path)
    }

    fn unsupported_image(&mut self) -> Option<String> {
        self.result
            .record_loss(LossKind::UnsupportedImages, 1, None);
        None
    }
}

/// Elements whose content is never part of the document text.
const SKIPPED: [&str; 27] = [
    "script", "style", "head", "title", "meta", "link", "base", "template", "noscript", "svg",
    "canvas", "iframe", "video", "audio", "object", "embed", "param", "source", "track", "select",
    "button", "textarea", "input", "colgroup", "col", "map", "datalist",
];

fn is_container(name: &str) -> bool {
    matches!(
        name,
        "html"
            | "body"
            | "div"
            | "section"
            | "article"
            | "aside"
            | "header"
            | "footer"
            | "nav"
            | "main"
            | "figure"
            | "figcaption"
            | "form"
            | "fieldset"
            | "address"
            | "center"
            | "details"
            | "summary"
            | "dl"
            | "dt"
            | "dd"
            | "li"
            | "hgroup"
            | "menu"
            | "dir"
            | "legend"
    )
}

fn is_hidden(node: &Handle, style: &Style) -> bool {
    style.get("display") == Some("none")
        || style.get("mso-hide") == Some("all")
        || attr(node, "hidden").is_some()
}

/// `page-break-before: always` and the modern `break-before: page` (and the `after` forms).
fn forced_break(style: &Style, legacy: &str, modern: &str) -> bool {
    [style.get(legacy), style.get(modern)]
        .into_iter()
        .flatten()
        .any(|v| matches!(v, "always" | "page" | "left" | "right" | "recto" | "verso"))
}

fn heading_level(name: &str) -> u8 {
    name.strip_prefix('h')
        .and_then(|d| d.parse().ok())
        .unwrap_or(1)
}

/// `Some(true)` for bold weights, `Some(false)` for normal ones, `None` for `inherit` & co.
fn bold_weight(value: &str) -> Option<bool> {
    match value {
        "bold" | "bolder" => Some(true),
        "normal" | "lighter" => Some(false),
        number => number.parse::<u32>().ok().map(|w| w >= 600),
    }
}

/// Whether the first family of a `font-family` list is a monospace font.
fn is_monospace(family: &str) -> bool {
    const MONOSPACE: [&str; 10] = [
        "courier",
        "consolas",
        "menlo",
        "monaco",
        "lucida console",
        "fira code",
        "source code",
        "jetbrains",
        "cascadia",
        "inconsolata",
    ];
    let first = family
        .split(',')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches(['"', '\'']);
    (first.contains("mono") && !first.contains("monotype"))
        || MONOSPACE.iter().any(|m| first.contains(*m))
}

/// Links that cannot be followed or that would run script are dropped.
fn is_safe_href(href: &str) -> bool {
    let lower = href.to_ascii_lowercase();
    !lower.is_empty() && !lower.starts_with("javascript:") && !lower.starts_with("vbscript:")
}

/// Collapses runs of whitespace (including no-break spaces) to one space.
fn collapse_space(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = true;
        } else {
            if space {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    if space {
        out.push(' ');
    }
    out
}

fn pixels(node: &Handle, name: &str) -> Option<u32> {
    let value = attr(node, name)?;
    value
        .trim()
        .trim_end_matches("px")
        .parse()
        .ok()
        .filter(|n| *n > 0)
}

fn list_start(node: &Handle) -> u64 {
    attr(node, "start")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1)
}

/// `Some(checked)` when the item starts with a checkbox (GitHub task lists).
fn find_checkbox(node: &Handle, depth: usize) -> Option<bool> {
    for child in children(node) {
        if text_of(&child).is_some_and(|t| t.trim().is_empty()) {
            continue;
        }
        return match tag(&child) {
            Some("input")
                if attr(&child, "type").is_some_and(|t| t.eq_ignore_ascii_case("checkbox")) =>
            {
                Some(attr(&child, "checked").is_some())
            }
            Some("p" | "label" | "div" | "span") if depth < 2 => find_checkbox(&child, depth + 1),
            _ => None,
        };
    }
    None
}

/// Text of a `<pre>`: verbatim, `<br>` as newline.
fn pre_text(node: &Handle) -> String {
    let mut out = String::new();
    let mut stack = children(node);
    stack.reverse();
    while let Some(current) = stack.pop() {
        if let Some(text) = text_of(&current) {
            out.push_str(&text);
            continue;
        }
        match tag(&current) {
            Some("br") => out.push('\n'),
            Some("script" | "style") | None => {}
            Some(_) => stack.extend(children(&current).into_iter().rev()),
        }
    }
    out.replace("\r\n", "\n").replace('\r', "\n")
}

/// Language from `language-x` / `lang-x` / `highlight-source-x` classes on the `<pre>` or its
/// `<code>` child.
fn code_language(pre: &Handle) -> String {
    let mut candidates = vec![pre.clone()];
    candidates.extend(children(pre).into_iter().filter(|c| tag(c) == Some("code")));
    for node in candidates {
        let class = attr(&node, "class").unwrap_or_default();
        for token in class.split_whitespace() {
            for prefix in ["language-", "lang-", "highlight-source-"] {
                let lang = token.strip_prefix(prefix).filter(|l| {
                    !l.is_empty()
                        && l.chars().all(|c| {
                            c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '-' | '_' | '.')
                        })
                });
                if let Some(lang) = lang {
                    return lang.to_ascii_lowercase();
                }
            }
        }
    }
    String::new()
}

/// The rows of a table. The HTML parser puts every `<tr>` into a row group (`<tbody>` when the
/// markup has none), so rows are only looked for there.
fn table_rows(table: &Handle) -> Vec<Handle> {
    let mut rows = Vec::new();
    for group in children(table) {
        if matches!(tag(&group), Some("thead" | "tbody" | "tfoot")) {
            rows.extend(
                children(&group)
                    .into_iter()
                    .filter(|r| tag(r) == Some("tr")),
            );
        }
    }
    rows
}

fn row_cells(row: &Handle) -> Vec<Handle> {
    children(row)
        .into_iter()
        .filter(|c| matches!(tag(c), Some("td" | "th")))
        .collect()
}

/// `colspan`/`rowspan` as a count of at least 1.
fn span(cell: &Handle, name: &str) -> usize {
    attr(cell, name)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_SPAN)
}

fn cell_align(cell: &Handle, style: &Style) -> Align {
    let value = style
        .get("text-align")
        .map(str::to_string)
        .or_else(|| attr(cell, "align"));
    match value.map(|v| v.to_lowercase()).as_deref() {
        Some("center") => Align::Center,
        Some("right" | "end") => Align::Right,
        _ => Align::None,
    }
}

/// Emits empty cells for the columns from `column` on that a `rowspan` above still covers;
/// returns the first free column.
fn fill_covered(line: &mut Vec<String>, covered: &mut [usize], mut column: usize) -> usize {
    while covered.get(column).is_some_and(|rows| *rows > 0) {
        covered[column] -= 1;
        line.push(String::new());
        column += 1;
    }
    column
}

// ---------------------------------------------------------------------------------------------
// Single-monospace-block detection and plain-text extraction (VS Code & co.)
// ---------------------------------------------------------------------------------------------

/// The text of an HTML document read as lines of code: `div`/`p`/`pre` end a line, `<br>` is an
/// empty line when the line is already closed.
#[derive(Debug)]
struct CodeText {
    lines: Vec<String>,
    line: String,
    /// The current line has content.
    open: bool,
    /// Some text is not whitespace.
    ink: bool,
    /// All of that text is in a monospace font.
    mono: bool,
    /// The markup is only wrappers (`div`, `span`, `pre`, ...), no structure.
    plain: bool,
}

impl CodeText {
    fn end_line(&mut self) {
        self.lines.push(std::mem::take(&mut self.line));
        self.open = false;
    }

    fn block_edge(&mut self) {
        if self.open {
            self.end_line();
        }
    }

    fn text(&mut self, text: &str, mono: bool, pre: bool) {
        if pre {
            for (index, segment) in text.replace('\r', "").split('\n').enumerate() {
                if index > 0 {
                    self.end_line();
                }
                self.segment(segment, mono);
            }
        } else {
            let collapsed = collapse_space(text);
            let segment = if self.open {
                collapsed.as_str()
            } else {
                collapsed.trim_start()
            };
            self.segment(segment, mono);
        }
    }

    fn segment(&mut self, segment: &str, mono: bool) {
        if segment.is_empty() {
            return;
        }
        self.line.push_str(segment);
        self.open = true;
        if segment.chars().any(|c| !c.is_whitespace()) {
            self.ink = true;
            self.mono &= mono;
        }
    }

    /// The lines joined by newlines, without trailing empty lines.
    fn into_text(mut self) -> String {
        self.block_edge();
        while self.lines.last().is_some_and(String::is_empty) {
            self.lines.pop();
        }
        self.lines.join("\n")
    }
}

fn extract_code(document: &Handle) -> CodeText {
    let mut code = CodeText {
        lines: Vec::new(),
        line: String::new(),
        open: false,
        ink: false,
        mono: true,
        plain: true,
    };
    code_walk(document, false, false, 0, &mut code);
    code
}

fn code_walk(node: &Handle, mono: bool, pre: bool, depth: usize, code: &mut CodeText) {
    if let Some(text) = text_of(node) {
        code.text(&text, mono, pre);
        return;
    }
    let Some(name) = tag(node) else {
        // The document node itself; comments and doctypes have no children.
        for child in children(node) {
            code_walk(&child, mono, pre, depth, code);
        }
        return;
    };
    if SKIPPED.contains(&name) {
        return;
    }
    if name == "br" {
        if !has_class(node, "Apple-interchange-newline") {
            code.end_line();
        }
        return;
    }
    const WRAPPERS: [&str; 15] = [
        "html", "body", "div", "span", "p", "pre", "code", "tt", "kbd", "samp", "b", "i", "em",
        "strong", "font",
    ];
    code.plain &= WRAPPERS.contains(&name);
    let style = Style::of(node);
    let mono = match style.get("font-family") {
        Some(family) => is_monospace(family),
        None => mono || matches!(name, "pre" | "code" | "tt" | "kbd" | "samp"),
    };
    let pre = style
        .get("white-space")
        .map_or(pre || name == "pre", |w| w.starts_with("pre"));
    let block = matches!(name, "div" | "p" | "pre");
    if block {
        code.block_edge();
    }
    if depth >= MAX_DEPTH {
        code.plain = false;
        code.text(&text_content(node), mono, pre);
    } else {
        for child in children(node) {
            code_walk(&child, mono, pre, depth + 1, code);
        }
    }
    if block {
        code.block_edge();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(html: &str) -> String {
        html_to_markdown(html, "assets").markdown
    }

    fn kinds(html: &str) -> Vec<(LossKind, usize)> {
        html_to_markdown(html, "assets")
            .sorted_losses()
            .iter()
            .map(|l| (l.kind, l.count))
            .collect()
    }

    #[test]
    fn emphasis_next_to_punctuation_parses_back() {
        use crate::doc::md::probe::{expect, runs};
        // (html, markdown, runs of equal emphasis the editor's parser finds in the markdown)
        type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);
        let table: [Case; 12] = [
            (
                "<p>x<b> bold </b>y</p>",
                "x **bold** y\n",
                &[("x ", ""), ("bold", "b"), (" y", "")],
            ),
            (
                "<p>a<b>bold.</b>b</p>",
                "a**bold**.b\n",
                &[("a", ""), ("bold", "b"), (".b", "")],
            ),
            (
                "<p>a<b>(bold)</b>b</p>",
                "a(**bold**)b\n",
                &[("a(", ""), ("bold", "b"), (")b", "")],
            ),
            (
                "<p>a<b>b</b>c</p>",
                "a**b**c\n",
                &[("a", ""), ("b", "b"), ("c", "")],
            ),
            (
                "<p>a<i>b</i>c</p>",
                "a*b*c\n",
                &[("a", ""), ("b", "i"), ("c", "")],
            ),
            ("<p>a<b>--</b>b</p>", "a--b\n", &[("a--b", "")]),
            (
                "<p><b>Note:</b>text</p>",
                "**Note**:text\n",
                &[("Note", "b"), (":text", "")],
            ),
            (
                "<p>(<b>bold</b>)</p>",
                "(**bold**)\n",
                &[("(", ""), ("bold", "b"), (")", "")],
            ),
            (
                "<p>a<b><i>b.</i></b>c</p>",
                "a***b***.c\n",
                &[("a", ""), ("b", "bi"), (".c", "")],
            ),
            // marks close before a hard break instead of staying open into the next line, where a
            // closing delimiter at the line start would not read as one
            (
                "<p><b>a<br></b>c</p>",
                "**a**\\\nc\n",
                &[("a", "b"), ("\nc", "")],
            ),
            (
                "<p><b>a<br>b</b></p>",
                "**a**\\\n**b**\n",
                &[("a", "b"), ("\n", ""), ("b", "b")],
            ),
            // white space after the punctuation already makes the closing delimiter valid
            (
                "<p><b>a:</b> <a href=\"u\">b</a></p>",
                "**a:** [b](u)\n",
                &[("a:", "b"), (" b", "")],
            ),
        ];
        for (html, want, spans) in table {
            let markdown = md(html);
            assert_eq!(markdown, want, "{html}");
            assert_eq!(runs(&markdown), expect(spans), "{html}");
        }
    }

    #[test]
    fn markdown_table() {
        let table = [
            (
                "<h2>Title <b>bold</b></h2><p>a <i>b</i> <u>c</u> <s>d</s> <mark>e</mark> <code>f</code></p>",
                "## Title bold\n\na *b* ~c~ ~~d~~ ==e== `f`\n",
            ),
            (
                "<span style=\"font-weight:700\">B</span><span style=\"font-weight:normal\">n</span>\
                 <b style=\"font-weight:400\">x</b><a href=\"u\"><span style=\"text-decoration:underline\">L</span></a>\
                 <span style=\"font-style:italic\">I</span><i style=\"font-style:normal\">N</i>\
                 <span style=\"text-decoration-line:line-through\">S</span>\
                 <span style=\"font-family:'Courier New'\">C</span>\
                 <span style=\"font-family:Arial\"><code>K</code></span>",
                "**B**nx[L](u)*I*N~~S~~`CK`\n",
            ),
            (
                "<p>See <a href=\"https://x.org/a b\">the <b>site</b></a>, <a href=\"javascript:x()\">bad</a> \
                 <a>anchor</a> <img src=\"https://x.org/i.png\" alt=\"A [pic]\" width=\"10\" height=\"20\">\
                 <img src=\"pic.png\" width=\"5px\"><img src=\"file:///C:/x.png\"><img src=\"cid:1\"><img></p>",
                "See [the **site**](https://x.org/a%20b), bad anchor ![A \\[pic\\]|10x20](https://x.org/i.png)![|5](pic.png)\n",
            ),
            (
                "<ul><li>a<ul><li>b</li></ul></li><li><input type=checkbox checked> done</li>\
                 <li><input type=checkbox> todo</li><li></li></ul>\
                 <ol start=\"3\" type=\"a\"><li>x</li><ul><li>y</li></ul></ol>",
                "- a\n  - b\n- [x] done\n- [ ] todo\n\n3. x\n   - y\n",
            ),
            (
                "<ul><li><p><input type=\"checkbox\" checked>x</p></li>\
                 <li><label><span><input type=checkbox></span></label>y</li><li><b>z</b></li>\
                 <li><div><div><div><input type=\"checkbox\"></div></div></div>w</li></ul>",
                "- [x] x\n- [ ] y\n- **z**\n- w\n",
            ),
            (
                "<ul><li> \n<input type=checkbox checked>x</li></ul>\
                 <pre class=\"hljs lang-py\">x = 1</pre>\
                 <p style=\"page-break-after:always\">a</p><p style=\"page-break-before:always\">b</p>",
                "- [x] x\n\n```py\nx = 1\n```\n\na\n\n<!-- pagebreak -->\n\nb\n",
            ),
            (
                "<ul>text<li>a</li><p>x</p></ul><ul><ul><li>n</li></ul></ul>",
                "- a\n\n* - n\n",
            ),
            (
                "<pre><code class=\"language-Rust\">fn main() {\n    println!(\"x\");\n}\n</code></pre>\
                 <blockquote><p>q1</p><p>q2<br>q3</p></blockquote><hr><pre>   </pre><blockquote></blockquote>",
                "```rust\nfn main() {\n    println!(\"x\");\n}\n```\n\n> q1\n>\n> q2\\\n> q3\n\n---\n",
            ),
            (
                "<p>1. not a list</p><p># not heading</p><p>a*b_c_ [x] $5 &amp;amp;</p>",
                "1\\. not a list\n\n\\# not heading\n\na\\*b_c\\_ \\[x\\] \\$5 \\&amp;\n",
            ),
            (
                "<div>a<br><br>b<br>c</div><p>  spaced   out&nbsp;&nbsp;text  </p><h3>a<br>b<img src=\"x.png\"></h3>",
                "a\n\nb\\\nc\n\nspaced out text\n\n### a b![](x.png)\n",
            ),
            (
                "<p>a</p><p style=\"page-break-before:always\">b</p><br style=\"page-break-before:always\">\
                 <div style=\"break-after:page\">c</div><p style=\"display:none\">h</p><script>x</script>\
                 <p>E=mc<sup>2</sup> H<sub>2</sub>O <span style=\"vertical-align:super\">s</span></p>",
                "a\n\n<!-- pagebreak -->\n\nb\n\n<!-- pagebreak -->\n\nc\n\n<!-- pagebreak -->\n\nE=mc2 H2O s\n",
            ),
            (
                "<dl><dt>t</dt><dd>d</dd></dl><li>stray</li><center><font color=red>c</font></center>",
                "t\n\nd\n\nstray\n\nc\n",
            ),
            (
                "<b id=\"docs-internal-guid-1\" style=\"font-weight:normal\"><ul><li aria-level=\"1\"><p role=\"presentation\">\
                 <span>One</span></p></li><ul><li aria-level=\"2\"><p><span style=\"background-color:#ffff00\">Two</span>\
                 </p></li></ul><li aria-level=\"1\"><p><span>Three</span></p></li></ul>\
                 <ol><li aria-level=\"2\"><p>flat</p></li></ol><ul><li aria-level=\"1\"></li>junk</ul></b>",
                "- One\n  - ==Two==\n- Three\n  1. flat\n",
            ),
            (
                "<p class=MsoTitle>Doc <b>title</b></p><p class=MsoHeading2>Sub</p>\
                 <p class=MsoListParagraph style='mso-list:l0 level1 lfo1'><span style='mso-list:Ignore'>o</span>A</p>\
                 <p class=MsoListParagraph style='mso-list:l0 level2 lfo1'><span style='mso-list:Ignore'>o</span>B</p>\
                 <p>plain</p><p class=MsoListParagraph style='mso-list:l0 level1 lfo1'>no marker</p>\
                 <p style='mso-list:l0 level1 lfo1'><o:p>&nbsp;</o:p></p>",
                "# Doc title\n\n## Sub\n\n- A\n  - B\n\nplain\n\n- no marker\n",
            ),
            (
                "<p><span style='mso-highlight:yellow'>hl</span> <span style='mso-highlight:auto'>no</span></p>",
                "==hl== no\n",
            ),
            (
                "<p>x</p><div hidden>h</div><p style='mso-hide:all'>h</p><svg>s</svg>",
                "x\n",
            ),
            ("   ", ""),
        ];
        for (html, expected) in table {
            assert_eq!(md(html), expected, "{html}");
        }
    }

    #[test]
    fn table_grids_beyond_the_column_and_cell_limits_are_cut() {
        use crate::import::{MAX_CELLS, MAX_COLUMNS};
        let cell = "<td>x</td>";
        let wide = format!("<tr>{}</tr>", cell.repeat(MAX_COLUMNS + 1));
        let narrow = format!("<tr>{cell}</tr>").repeat(MAX_CELLS / MAX_COLUMNS + 6);
        let result = html_to_markdown(&format!("<table>{wide}{narrow}</table>"), "assets");
        let lines: Vec<&str> = result.markdown.lines().collect();
        assert_eq!(lines[0].matches('|').count(), MAX_COLUMNS + 1);
        assert_eq!(lines.len(), MAX_CELLS / MAX_COLUMNS + 1);
        let losses: Vec<(LossKind, usize)> =
            result.losses.iter().map(|l| (l.kind, l.count)).collect();
        assert_eq!(losses, [(LossKind::CorruptParts, 1)]);
    }

    #[test]
    fn tables() {
        let table = [
            (
                "<table><tr><th align=\"center\">H1</th><th style=\"text-align:right\">H2</th><th>H3</th></tr>\
                 <tr><td colspan=\"2\">a|b</td><td rowspan=\"2\">c</td></tr>\
                 <tr><td>d</td><td><p>e</p><p>f</p></td></tr></table>",
                "| H1 | H2 | H3 |\n|:---:|---:|---|\n| a\\|b |  | c |\n| d | e<br>f |  |\n",
            ),
            (
                "<table><thead><tr><td align=\"RIGHT\">h</td></tr></thead><tbody><tr><td>x</td></tr><tr></tr></tbody></table>",
                "| h |\n|---:|\n| x |\n",
            ),
            (
                "<table><tr><td>x</td><td><table><tr><td>i1</td><td>i2</td></tr></table></td></tr></table>",
                "| x | i1 i2 |\n|---|---|\n",
            ),
            ("<table><tr><td><p>only</p></td></tr></table>", "only\n"),
            (
                "<table></table><table><tr><td></td><td></td></tr></table>",
                "",
            ),
            (
                "<table><tr><td><ul><li>a</li><li>b</li></ul></td><td><pre>x|y\nz</pre></td><td><blockquote>q</blockquote><hr></td></tr></table>",
                "| - a<br>- b | `x\\|y z` | q |\n|---|---|---|\n",
            ),
        ];
        for (html, expected) in table {
            assert_eq!(md(html), expected, "{html}");
        }
        let merged = "<table><tr><td colspan=2>a</td><td>b</td></tr><tr><td rowspan=2>c</td><td>d</td><td>e</td></tr>\
                      <tr><td>f</td><td>g</td></tr></table>";
        assert_eq!(
            md(merged),
            "| a |  | b |\n|---|---|---|\n| c | d | e |\n|  | f | g |\n"
        );
        assert_eq!(kinds(merged), [(LossKind::MergedCells, 2)]);
    }

    #[test]
    fn row_groups_cell_content_and_alignment() {
        let table = [
            (
                "<table><thead><tr><th>h</th></tr></thead><tbody><tr><td>x</td></tr></tbody>\
                 <tfoot><tr><td>f</td></tr></tfoot></table>",
                "| h |\n|---|\n| x |\n| f |\n",
            ),
            (
                "<table><tr><td><h2>x</h2></td><td style=\"text-align:end\">y</td></tr></table>",
                "| x | y |\n|---|---:|\n",
            ),
        ];
        for (html, expected) in table {
            assert_eq!(md(html), expected, "{html}");
        }
    }

    #[test]
    fn non_content_nodes_and_empty_blocks_are_dropped() {
        let table = [
            // comments and doctypes are not content
            ("<!DOCTYPE html><p>a<!-- c -->b</p>", "ab\n"),
            // the placeholder break that browsers add after a copied selection is not a break
            ("<p>a<br class=\"Apple-interchange-newline\">b</p>", "ab\n"),
            (
                "<p>H<span style=\"vertical-align:sub\">2</span>O</p>",
                "H2O\n",
            ),
            ("<h1> </h1><h2><br></h2><p>x</p>", "x\n"),
            ("<ul></ul><ol><li></li></ol><p>x</p>", "x\n"),
            // a nested list beside the items of a list whose items declare their level
            (
                "<ul><li aria-level=\"1\">a</li><ol><li aria-level=\"2\">b</li></ol></ul>",
                "- a\n  1. b\n",
            ),
            // `<pre>` text: `<br>` is a newline, comments and scripts are not text
            (
                "<pre>a<br>b<!-- c --><script>x</script></pre>",
                "```\na\nb\n```\n",
            ),
            (
                "<pre class=\"language-c++\">x</pre><pre class=\"language-a/b\">y</pre>",
                "```c++\nx\n```\n\n```\ny\n```\n",
            ),
        ];
        for (html, expected) in table {
            assert_eq!(md(html), expected, "{html}");
        }
    }

    #[test]
    fn every_forced_break_value_starts_a_new_page() {
        for value in ["always", "page", "left", "right", "recto", "verso"] {
            let html = format!("<p>a</p><p style=\"break-before:{value}\">b</p>");
            assert_eq!(md(&html), "a\n\n<!-- pagebreak -->\n\nb\n", "{value}");
        }
        assert_eq!(
            md("<p>a</p><p style=\"break-before:avoid\">b</p>"),
            "a\n\nb\n"
        );
    }

    #[test]
    fn loss_table() {
        let table = [
            (
                "<table><tr><td><p>a</p><p>b</p></td><td><table><tr><td>i</td></tr><tr><td>j</td></tr></table></td></tr></table>",
                vec![
                    (LossKind::NestedTables, 1),
                    (LossKind::MultiParagraphCells, 1),
                ],
            ),
            (
                "<p>E=mc<sup>2</sup></p>",
                vec![(LossKind::SubSuperscript, 1)],
            ),
            (
                "<img src=\"file:///x.png\"><img src=\"cid:x\"><img src=\"data:image/png,raw\">",
                vec![(LossKind::UnsupportedImages, 3)],
            ),
            (
                "<ol type=\"i\"><li>a</li></ol>",
                vec![(LossKind::ListNumberingStyle, 1)],
            ),
            (
                "<ol type=\"I\"><li>a</li></ol><ol type=\"A\"><li>b</li></ol>",
                vec![(LossKind::ListNumberingStyle, 2)],
            ),
            ("<p>plain</p>", vec![]),
        ];
        for (html, expected) in table {
            assert_eq!(kinds(html), expected, "{html}");
        }
    }

    #[test]
    fn data_uri_images_become_assets() {
        let html = "<p><img src=\"data:image/png;base64,iVBORw0KGgo=\" alt=\"dot\">\
                    <img src=\"data:image/jpeg;base64,/9j/4A==\">\
                    <img src=\"data:application/octet-stream;base64,iVBORw0KGgo=\">\
                    <img src=\"data:image/png,raw\"><img src=\"data:text/plain;base64,aGk=\">\
                    <img src=\"data:image/png;base64,@@@\"><img src=\"data:image\"></p>";
        let result = html_to_markdown(html, "assets");
        assert_eq!(
            result.markdown,
            "![dot](assets/image-1.png)![](assets/image-2.jpg)![](assets/image-3.png)\n"
        );
        let paths: Vec<&str> = result
            .assets
            .iter()
            .map(|a| a.relative_path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "assets/image-1.png",
                "assets/image-2.jpg",
                "assets/image-3.png"
            ]
        );
        assert_eq!(result.assets[0].bytes, b"\x89PNG\r\n\x1a\n");
        assert_eq!(result.assets[1].bytes, [0xFF, 0xD8, 0xFF, 0xE0]);
        assert_eq!(result.losses.len(), 1);
        assert_eq!(
            (result.losses[0].kind, result.losses[0].count),
            (LossKind::UnsupportedImages, 4)
        );
        let flat = html_to_markdown("<img src=\"data:image/gif;base64,R0lGODlh\">", "");
        assert_eq!(flat.assets[0].relative_path, "image-1.gif");
        let nested = html_to_markdown("<img src=\"data:image/gif;base64,R0lGODlh\">", "a/b/");
        assert_eq!(nested.assets[0].relative_path, "a/b/image-1.gif");
    }

    #[test]
    fn code_and_plain_text_detection() {
        let plain = |html: &str, vscode: bool| match convert_html_paste(html, vscode, "") {
            HtmlPaste::PlainText(text) => Some(text),
            HtmlPaste::Markdown(_) => None,
        };
        let table = [
            ("<pre>a\n  b</pre>", false, Some("a\n  b")),
            (
                "<code>x<!-- c --><script>y</script></code>",
                false,
                Some("x"),
            ),
            (
                "<div style=\"font-family: Consolas; white-space: pre-wrap\">one\r\ntwo<br><br>three</div>\
                 <p style=\"font-family:Consolas\">  x  y </p><br><br>",
                false,
                Some("one\ntwo\n\nthree\nx y "),
            ),
            (
                "<pre style=\"white-space:normal\">a   b</pre>",
                false,
                Some("a b"),
            ),
            (
                "<pre>a<br>b</pre><br class=\"Apple-interchange-newline\">",
                false,
                Some("a\nb"),
            ),
            (
                "<p style=\"font-family:Consolas\">code</p><p>text</p>",
                false,
                None,
            ),
            (
                "<div style=\"font-family:monospace\">a<span style=\"font-family:Arial\">b</span></div>",
                false,
                None,
            ),
            (
                "<a href=\"x\" style=\"font-family:monospace\">x</a>",
                false,
                None,
            ),
            ("<pre>a</pre><ul><li>b</li></ul>", false, None),
            ("<p>plain</p>", false, None),
            ("<p>a</p><p>b</p>", true, Some("a\nb")),
            ("", true, Some("")),
            ("   ", false, None),
        ];
        for (html, vscode, expected) in table {
            assert_eq!(plain(html, vscode).as_deref(), expected, "{html}");
        }
        assert_eq!(
            md("<a href=\"x\" style=\"font-family:monospace\">x</a>"),
            "[`x`](x)\n"
        );
    }

    #[test]
    fn very_deep_nesting_is_flattened_not_overflowed() {
        let depth = 300;
        let html = format!("{}deep{}", "<div>".repeat(depth), "</div>".repeat(depth));
        assert_eq!(md(&html), "deep\n");
        // markup flattened past the depth limit that holds no text adds nothing
        let empty = format!("{}{}", "<div>".repeat(depth), "</div>".repeat(depth));
        assert_eq!(md(&empty), "");
        let code = format!(
            "<code>{}t{}</code>",
            "<span style=\"font-family:monospace\">".repeat(depth),
            "</span>".repeat(depth)
        );
        assert!(
            matches!(
                convert_html_paste(&code, false, ""),
                HtmlPaste::Markdown(result) if result.markdown.contains('t')
            ),
            "deep markup is converted, not left as plain text"
        );
    }

    #[test]
    fn helper_tables() {
        for (value, expected) in [
            ("bold", Some(true)),
            ("bolder", Some(true)),
            ("700", Some(true)),
            ("600", Some(true)),
            ("599", Some(false)),
            ("400", Some(false)),
            ("normal", Some(false)),
            ("lighter", Some(false)),
            ("inherit", None),
        ] {
            assert_eq!(bold_weight(value), expected, "{value}");
        }
        for (family, expected) in [
            ("menlo, monaco, \"courier new\", monospace", true),
            ("\"courier new\", serif", true),
            ("ui-monospace, sfmono-regular", true),
            ("'jetbrains mono'", true),
            ("monotype corsiva", false),
            ("arial, monospace", false),
            ("", false),
        ] {
            assert_eq!(is_monospace(family), expected, "{family}");
        }
        for (href, expected) in [
            ("https://x.org", true),
            ("#a", true),
            ("JavaScript:x()", false),
            ("vbscript:x", false),
            ("", false),
        ] {
            assert_eq!(is_safe_href(href), expected, "{href}");
        }
        assert_eq!(collapse_space("  a \n\t b\u{a0}\u{a0}c  "), " a b c ");
        assert_eq!(collapse_space(""), "");
        assert_eq!(heading_level("h4"), 4);
    }
}
