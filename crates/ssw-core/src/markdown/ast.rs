//! The Markdown tree shared by the editor, exporters and importers.
//!
//! Conventions that hold for every node:
//!
//! * `range` is the full source extent of the node, including its syntax markers. Offsets are
//!   UTF-8 byte offsets into the parsed text.
//! * `markers` lists the exact syntax characters of the node (`**`, `[`, `](url)`, `>`, `#`,
//!   fences, pipes, …) in ascending order. Spaces that merely separate a marker from the
//!   content are not part of a marker. These are the ranges the live renderer hides or dims.
//! * `content` is the part a reader sees as the node's own text (inside the markers). For
//!   container nodes it is the span of the children.
//! * Siblings never overlap and are sorted by position.

use smallvec::SmallVec;

use crate::text::TextRange;

use super::definitions::Definitions;

/// Syntax marker ranges of a node.
pub type Markers = SmallVec<[TextRange; 4]>;

/// The markers `ranges` (in source order); spills to the heap past the inline capacity.
pub fn marks(ranges: &[TextRange]) -> Markers {
    Markers::from_slice(ranges)
}

/// A parsed document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    /// Top-level blocks in source order.
    pub blocks: Vec<Block>,
    /// Link reference and footnote definitions plus label lookups.
    pub definitions: Definitions,
    /// Length in bytes of the parsed text.
    pub len: usize,
}

/// A block-level node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub range: TextRange,
    pub content: TextRange,
    pub markers: Markers,
    pub kind: BlockKind,
    /// Child blocks (containers only).
    pub children: Vec<Block>,
    /// Inline content of paragraphs and headings; for an [`BlockKind::HtmlBlock`] that is a
    /// lone `<img>` tag, the single [`InlineKind::Image`]. Table cells carry their own.
    pub inlines: Vec<Inline>,
}

/// What a [`Block`] is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockKind {
    /// ATX heading. `marker` is the opening `#…` run; an optional closing run is the second
    /// entry of `markers`. `content` excludes surrounding spaces and the closing run.
    Heading {
        level: u8,
        marker: TextRange,
    },
    Paragraph,
    /// List container; children are [`BlockKind::ListItem`] blocks.
    List {
        ordered: bool,
        /// Number of the first item (0 for bullet lists).
        start: u32,
        delimiter: ListDelimiter,
        /// `false` when any two direct children are separated by a blank line.
        tight: bool,
    },
    /// `marker` is the bullet or `1.` / `1)` (without trailing spaces). The task checkbox
    /// range, when present, is also listed in `markers` after the marker.
    ListItem {
        marker: TextRange,
        task: Option<Task>,
    },
    /// `markers` holds one `>` per source line.
    BlockQuote,
    /// `> [!NOTE] Title`. `markers` holds the `>` of every line and the `[!NOTE]` tag.
    Callout {
        kind: CalloutKind,
        /// The `[!KIND]` tag.
        tag: TextRange,
        /// Optional title text after the tag on the first line.
        title: Option<TextRange>,
    },
    CodeBlock(CodeBlock),
    MathBlock(MathBlock),
    Table(Table),
    ThematicBreak,
    /// `---` (YAML) or `+++` (TOML) block at the very start of the document. `markers` holds
    /// the opening and closing fence lines; `content` is the text between them.
    FrontMatter {
        format: FrontMatterFormat,
    },
    HtmlBlock(HtmlBlock),
    /// `[^label]: …` container; `markers` holds the `[^label]:` prefix. `label` is the label
    /// as written.
    FootnoteDefinition {
        label: String,
    },
    /// `[label]: dest "title"` — kept as a block so it can be dimmed/edited; the resolved
    /// data also lives in [`Definitions`].
    LinkReferenceDefinition(LinkReferenceDefinition),
    /// `<!-- pagebreak -->` alone on a line.
    PageBreak,
}

/// Bullet character or ordered-list delimiter. Lists with different delimiters are different
/// lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListDelimiter {
    /// `-`
    Dash,
    /// `*`
    Star,
    /// `+`
    Plus,
    /// `1.`
    Period,
    /// `1)`
    Paren,
}

impl ListDelimiter {
    pub const fn as_char(self) -> char {
        match self {
            ListDelimiter::Dash => '-',
            ListDelimiter::Star => '*',
            ListDelimiter::Plus => '+',
            ListDelimiter::Period => '.',
            ListDelimiter::Paren => ')',
        }
    }
}

/// `[ ]` / `[x]` at the start of a list item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Task {
    pub checked: bool,
    /// The `[ ]` / `[x]` characters.
    pub checkbox: TextRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CalloutKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl CalloutKind {
    pub const ALL: [CalloutKind; 5] = [
        CalloutKind::Note,
        CalloutKind::Tip,
        CalloutKind::Important,
        CalloutKind::Warning,
        CalloutKind::Caution,
    ];

    /// Upper-case keyword as written in `[!NOTE]`.
    pub const fn keyword(self) -> &'static str {
        match self {
            CalloutKind::Note => "NOTE",
            CalloutKind::Tip => "TIP",
            CalloutKind::Important => "IMPORTANT",
            CalloutKind::Warning => "WARNING",
            CalloutKind::Caution => "CAUTION",
        }
    }

    /// Case-insensitive keyword lookup.
    pub fn from_keyword(word: &str) -> Option<CalloutKind> {
        CalloutKind::ALL
            .into_iter()
            .find(|k| k.keyword().eq_ignore_ascii_case(word))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontMatterFormat {
    Yaml,
    Toml,
}

/// A source line of verbatim block content (code, math, HTML).
///
/// `range` excludes the line terminator and any container prefix (`>`, list indentation).
/// `pad` is the number of leading spaces that belong to the line but are not in the source:
/// a tab that was only partially consumed by container indentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerbatimLine {
    pub range: TextRange,
    pub pad: u8,
}

/// Text of verbatim `lines` read from `src`, each line followed by `\n`.
pub fn verbatim_text(lines: &[VerbatimLine], src: &str) -> String {
    let mut out = String::new();
    for line in lines {
        for _ in 0..line.pad {
            out.push(' ');
        }
        out.push_str(line.range.get(src).unwrap_or(""));
        out.push('\n');
    }
    out
}

/// Opening (and closing) fence of a fenced code block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence {
    /// `` ` `` or `~`.
    pub ch: char,
    /// Length of the opening fence run.
    pub len: usize,
    pub open: TextRange,
    /// `None` when the block ran to the end of its container without a closing fence.
    pub close: Option<TextRange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeBlock {
    /// `None` for an indented code block.
    pub fence: Option<Fence>,
    /// Info string (trimmed, still escaped as written) of a fenced block.
    pub info: Option<TextRange>,
    /// Content lines (the lines between the fences). `Block::content` spans them.
    pub lines: Vec<VerbatimLine>,
}

impl CodeBlock {
    /// Literal code text: every line followed by `\n`.
    pub fn text(&self, src: &str) -> String {
        verbatim_text(&self.lines, src)
    }

    /// First word of the info string.
    pub fn language<'a>(&self, src: &'a str) -> Option<&'a str> {
        let info = self.info?.get(src)?;
        info.split_whitespace().next()
    }
}

/// `$$ … $$` display math. `Block::markers` holds the opening and closing `$$`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MathBlock {
    /// Content lines between the delimiters (a single-line `$$x$$` yields one line).
    pub lines: Vec<VerbatimLine>,
    /// `false` when the closing `$$` is missing.
    pub closed: bool,
}

impl MathBlock {
    /// TeX source: lines joined with `\n` (no trailing newline).
    pub fn text(&self, src: &str) -> String {
        let mut text = verbatim_text(&self.lines, src);
        if text.ends_with('\n') {
            text.pop();
        }
        text
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HtmlBlock {
    /// CommonMark HTML block type 1..=7.
    pub kind: u8,
    pub lines: Vec<VerbatimLine>,
}

impl HtmlBlock {
    /// Raw HTML: every line followed by `\n`.
    pub fn text(&self, src: &str) -> String {
        verbatim_text(&self.lines, src)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkReferenceDefinition {
    /// Label as written (without normalization).
    pub label: String,
    pub label_range: TextRange,
    /// Destination with escapes/entities resolved.
    pub dest: String,
    pub dest_range: TextRange,
    pub title: Option<String>,
    pub title_range: Option<TextRange>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Alignment {
    None,
    Left,
    Center,
    Right,
}

/// GFM pipe table. `Block::markers` holds the delimiter row range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    pub alignments: Vec<Alignment>,
    pub header: TableRow,
    pub delimiter: TextRange,
    /// Body rows. A row may have fewer or more cells than `alignments`.
    pub rows: Vec<TableRow>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableRow {
    pub range: TextRange,
    /// Unescaped `|` separators, including optional leading/trailing pipes.
    pub pipes: Vec<TextRange>,
    pub cells: Vec<TableCell>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableCell {
    /// Space between the neighbouring pipes (padding included).
    pub range: TextRange,
    /// Cell source text without padding.
    pub content: TextRange,
    pub inlines: Vec<Inline>,
}

/// An inline-level node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inline {
    pub range: TextRange,
    pub content: TextRange,
    pub markers: Markers,
    pub kind: InlineKind,
    pub children: Vec<Inline>,
}

/// What an [`Inline`] is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InlineKind {
    /// Plain text; its characters are exactly the source text of `range`. Never contains a
    /// line break.
    Text,
    /// `\*`: `markers` holds the backslash, `content` the escaped character.
    Escape,
    /// `&amp;`, `&#35;`; `decoded` is the resolved text.
    Entity {
        decoded: String,
    },
    /// `*x*` / `_x_`.
    Emphasis,
    /// `**x**` / `__x__`.
    Strong,
    /// `~x~`.
    Underline,
    /// `~~x~~`.
    Strikethrough,
    /// `==x==` or `=={red}x==` (the `{red}` belongs to the opening marker).
    Highlight {
        color: Option<HighlightColor>,
    },
    /// `` `x` ``; `text` is the normalized code text (line endings as spaces, one surrounding
    /// space pair stripped). `markers` holds both backtick runs.
    Code {
        text: String,
    },
    /// Children are the link text.
    Link(Link),
    /// Children are the alt text nodes (without the `|300x200` size suffix).
    Image(Image),
    WikiLink(WikiLink),
    /// `[^label]` that resolves to a footnote definition. `label` as written.
    FootnoteRef {
        label: String,
    },
    /// `$x$`; `content` is the TeX source.
    InlineMath,
    /// Backslash or two-plus spaces before a line ending; `markers` holds the backslash or
    /// spaces. `range` includes the line ending.
    HardBreak,
    /// A line ending inside a paragraph (preceding trailing space included).
    SoftBreak,
    /// Inline HTML tag, comment, processing instruction, declaration or CDATA.
    RawHtml,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HighlightColor {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl HighlightColor {
    pub const ALL: [HighlightColor; 6] = [
        HighlightColor::Red,
        HighlightColor::Orange,
        HighlightColor::Yellow,
        HighlightColor::Green,
        HighlightColor::Blue,
        HighlightColor::Purple,
    ];

    /// Lower-case name as written in `=={red}`.
    pub const fn name(self) -> &'static str {
        match self {
            HighlightColor::Red => "red",
            HighlightColor::Orange => "orange",
            HighlightColor::Yellow => "yellow",
            HighlightColor::Green => "green",
            HighlightColor::Blue => "blue",
            HighlightColor::Purple => "purple",
        }
    }

    pub fn from_name(name: &str) -> Option<HighlightColor> {
        HighlightColor::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// How a reference link/image names its definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReferenceForm {
    /// `[text][label]`
    Full,
    /// `[label][]`
    Collapsed,
    /// `[label]`
    Shortcut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// `[text](dest "title")`. `markers` = `[` and the closing `](…)` part.
    Inline {
        dest_range: TextRange,
        title_range: Option<TextRange>,
    },
    /// `[text][label]`, `[label][]`, `[label]`. `label` is normalized.
    Reference {
        label: String,
        form: ReferenceForm,
        label_range: TextRange,
    },
    /// `<https://x>` (`angle`) or a bare GFM extended autolink. `markers` holds `<` and `>`
    /// for the angle form and is empty otherwise.
    Autolink { angle: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// Resolved destination (escapes and entities resolved; `mailto:` added for e-mail
    /// autolinks, `http://` for `www.` autolinks).
    pub dest: String,
    pub title: Option<String>,
    pub kind: LinkKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageKind {
    /// `![alt|300](dest "title")`
    Inline {
        dest_range: TextRange,
        title_range: Option<TextRange>,
    },
    /// `![alt][label]`, `![label][]`, `![label]`
    Reference {
        label: String,
        form: ReferenceForm,
        label_range: TextRange,
    },
    /// `<img src="…" width="…" height="…">`; `range` is the whole tag.
    Html,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub dest: String,
    pub title: Option<String>,
    /// Alt text source range, without the `|300x200` size suffix. For [`ImageKind::Html`] the
    /// value of the `alt` attribute (empty range when absent).
    pub alt: TextRange,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub kind: ImageKind,
}

/// `[[target]]`, `[[target|alias]]`, `[[#heading]]`, `[[target#heading]]`.
///
/// `markers` holds the opening `[[`, the alias separator `|` (when present) and the closing
/// `]]`. `content` is the text a reader sees: the alias when present, otherwise the
/// target/heading text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WikiLink {
    /// Target document name (empty for `[[#heading]]`).
    pub target: String,
    pub heading: Option<String>,
    pub alias: Option<String>,
    pub target_range: TextRange,
    pub heading_range: Option<TextRange>,
    pub alias_range: Option<TextRange>,
}

/// Reference to any node of the tree.
#[derive(Clone, Copy, Debug)]
pub enum NodeRef<'a> {
    Block(&'a Block),
    TableRow(&'a TableRow),
    TableCell(&'a TableCell),
    Inline(&'a Inline),
}

impl<'a> NodeRef<'a> {
    pub fn range(self) -> TextRange {
        match self {
            NodeRef::Block(b) => b.range,
            NodeRef::TableRow(r) => r.range,
            NodeRef::TableCell(c) => c.range,
            NodeRef::Inline(i) => i.range,
        }
    }

    /// Syntax marker ranges (table rows list their pipes, table cells have none).
    pub fn markers(self) -> &'a [TextRange] {
        match self {
            NodeRef::Block(b) => &b.markers,
            NodeRef::TableRow(r) => &r.pipes,
            NodeRef::TableCell(_) => &[],
            NodeRef::Inline(i) => &i.markers,
        }
    }

    pub fn as_block(self) -> Option<&'a Block> {
        match self {
            NodeRef::Block(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_inline(self) -> Option<&'a Inline> {
        match self {
            NodeRef::Inline(i) => Some(i),
            _ => None,
        }
    }
}

/// Picks the item containing `offset`; at a boundary between two items the one ending there
/// is used only when no item starts there. Items must be sorted and disjoint.
fn pick<T>(items: &[T], offset: usize, range_of: impl Fn(&T) -> TextRange) -> Option<&T> {
    let idx = items.partition_point(|item| range_of(item).end <= offset);
    if let Some(item) = items.get(idx)
        && range_of(item).start <= offset
    {
        return Some(item);
    }
    // `partition_point` never exceeds `items.len()`, so the predecessor of a non-zero index exists.
    let prev = &items[idx.checked_sub(1)?];
    (range_of(prev).end == offset).then_some(prev)
}

impl Document {
    /// All blocks, parents before children, in source order.
    pub fn iter_blocks(&self) -> BlockIter<'_> {
        BlockIter {
            stack: self.blocks.iter().rev().collect(),
        }
    }

    /// Chain of nodes from the top-level block down to the innermost node whose range touches
    /// `offset` (inclusive bounds). Empty when `offset` is outside every block.
    pub fn path_at(&self, offset: usize) -> Vec<NodeRef<'_>> {
        let mut path = Vec::new();
        let Some(mut block) = pick(&self.blocks, offset, |b| b.range) else {
            return path;
        };
        loop {
            path.push(NodeRef::Block(block));
            if let Some(child) = pick(&block.children, offset, |b| b.range) {
                block = child;
                continue;
            }
            if let BlockKind::Table(table) = &block.kind {
                push_table_path(table, offset, &mut path);
            } else {
                push_inline_path(&block.inlines, offset, &mut path);
            }
            return path;
        }
    }

    /// Innermost node touching `offset`.
    pub fn innermost_at(&self, offset: usize) -> Option<NodeRef<'_>> {
        self.path_at(offset).pop()
    }

    /// Innermost block touching `offset`.
    pub fn block_at(&self, offset: usize) -> Option<&Block> {
        self.path_at(offset)
            .into_iter()
            .rev()
            .find_map(NodeRef::as_block)
    }
}

fn push_inline_path<'a>(inlines: &'a [Inline], offset: usize, path: &mut Vec<NodeRef<'a>>) {
    let mut level = inlines;
    while let Some(node) = pick(level, offset, |i| i.range) {
        path.push(NodeRef::Inline(node));
        level = &node.children;
    }
}

fn push_table_path<'a>(table: &'a Table, offset: usize, path: &mut Vec<NodeRef<'a>>) {
    let row = if offset <= table.header.range.end {
        Some(&table.header)
    } else {
        pick(&table.rows, offset, |r| r.range)
    };
    let Some(row) = row else { return };
    path.push(NodeRef::TableRow(row));
    if let Some(cell) = pick(&row.cells, offset, |c| c.range) {
        path.push(NodeRef::TableCell(cell));
        push_inline_path(&cell.inlines, offset, path);
    }
}

impl Block {
    /// This block and all descendant blocks, parents before children.
    pub fn descendants(&self) -> BlockIter<'_> {
        BlockIter { stack: vec![self] }
    }

    /// Inline sequences owned directly by this block: its own `inlines`, or one per table
    /// cell (header first).
    pub fn inline_groups(&self) -> Vec<&[Inline]> {
        match &self.kind {
            BlockKind::Table(table) => table
                .header
                .cells
                .iter()
                .chain(table.rows.iter().flat_map(|r| r.cells.iter()))
                .map(|c| c.inlines.as_slice())
                .collect(),
            _ if self.inlines.is_empty() => Vec::new(),
            _ => vec![self.inlines.as_slice()],
        }
    }

    /// The task checkbox of a list item; `None` for plain items and every other block.
    pub fn task(&self) -> Option<Task> {
        match self.kind {
            BlockKind::ListItem { task, .. } => task,
            _ => None,
        }
    }

    /// Heading level for headings.
    pub fn heading_level(&self) -> Option<u8> {
        match self.kind {
            BlockKind::Heading { level, .. } => Some(level),
            _ => None,
        }
    }
}

impl InlineKind {
    /// The color of a highlight; `None` for an uncolored highlight and for every other kind.
    pub fn highlight_color(&self) -> Option<HighlightColor> {
        match self {
            InlineKind::Highlight { color } => *color,
            _ => None,
        }
    }
}

impl Inline {
    /// This node and all descendants, parents before children.
    pub fn descendants(&self) -> InlineIter<'_> {
        InlineIter { stack: vec![self] }
    }
}

/// Pre-order iterator over all inlines in `inlines` and their descendants.
pub fn iter_inlines(inlines: &[Inline]) -> InlineIter<'_> {
    InlineIter {
        stack: inlines.iter().rev().collect(),
    }
}

/// Pre-order block iterator.
#[derive(Clone, Debug)]
pub struct BlockIter<'a> {
    stack: Vec<&'a Block>,
}

impl<'a> Iterator for BlockIter<'a> {
    type Item = &'a Block;

    fn next(&mut self) -> Option<&'a Block> {
        let block = self.stack.pop()?;
        self.stack.extend(block.children.iter().rev());
        Some(block)
    }
}

/// Pre-order inline iterator.
#[derive(Clone, Debug)]
pub struct InlineIter<'a> {
    stack: Vec<&'a Inline>,
}

impl<'a> Iterator for InlineIter<'a> {
    type Item = &'a Inline;

    fn next(&mut self) -> Option<&'a Inline> {
        let node = self.stack.pop()?;
        self.stack.extend(node.children.iter().rev());
        Some(node)
    }
}
