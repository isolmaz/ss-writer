//! Block structure pass: container/leaf recognition line by line (the CommonMark reference
//! algorithm), producing an arena of nodes with exact source ranges.

use crate::markdown::ast::{
    Alignment, Fence, FrontMatterFormat, LinkReferenceDefinition, ListDelimiter, VerbatimLine,
};
use crate::markdown::definitions::{Definitions, LinkDefinition, normalize_label};
use crate::text::TextRange;

use super::scan;
use super::source::Source;

#[derive(Clone, Debug)]
pub(super) struct ListData {
    pub ordered: bool,
    pub delimiter: ListDelimiter,
    pub start: u32,
    pub marker_offset: usize,
    pub padding: usize,
    pub marker: TextRange,
}

#[derive(Clone, Debug)]
pub(super) enum NodeKind {
    Document,
    List(ListData),
    Item(ListData),
    Quote,
    Footnote {
        label: String,
    },
    Heading {
        level: u8,
        marker: TextRange,
        content: TextRange,
        close: Option<TextRange>,
    },
    Paragraph,
    Code {
        fence: Option<Fence>,
        fence_indent: usize,
        info: Option<TextRange>,
    },
    Math {
        open: TextRange,
        close: Option<TextRange>,
    },
    Html(u8),
    Thematic,
    PageBreak,
    FrontMatter {
        format: FrontMatterFormat,
        open: TextRange,
        close: TextRange,
    },
    Table {
        alignments: Vec<Alignment>,
    },
    Def(LinkReferenceDefinition),
}

#[derive(Clone, Debug)]
pub(super) struct Node {
    pub kind: NodeKind,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub open: bool,
    pub start: usize,
    pub line: usize,
    pub markers: Vec<TextRange>,
    /// Content lines of leaf blocks (paragraph, heading text, code, math, HTML, table rows).
    pub lines: Vec<VerbatimLine>,
    pub last_line_blank: bool,
    pub tight: bool,
    /// Set when a paragraph was fully consumed by definitions or turned into a table.
    pub removed: bool,
}

impl Node {
    fn new(kind: NodeKind, parent: Option<usize>, start: usize, line: usize) -> Node {
        Node {
            kind,
            parent,
            children: Vec::new(),
            open: true,
            start,
            line,
            markers: Vec::new(),
            lines: Vec::new(),
            last_line_blank: false,
            tight: true,
            removed: false,
        }
    }
}

enum Cont {
    Matched,
    Failed,
    LineDone,
}

enum Start {
    No,
    Container,
    Leaf,
}

/// Result of the block pass.
pub(super) struct BlockTree {
    pub nodes: Vec<Node>,
    pub defs: Definitions,
}

pub(super) struct BlockParser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    nodes: Vec<Node>,
    tip: usize,
    old_tip: usize,
    last_matched: usize,
    all_closed: bool,
    line_end: usize,
    line_no: usize,
    offset: usize,
    column: usize,
    next_nonspace: usize,
    next_nonspace_column: usize,
    indent: usize,
    blank: bool,
    partially_consumed_tab: bool,
    skip_line: bool,
    defs: Definitions,
}

/// Runs the block pass over `src`.
pub(super) fn parse_blocks(src: &str) -> BlockTree {
    let mut p = BlockParser {
        src,
        bytes: src.as_bytes(),
        nodes: vec![Node::new(NodeKind::Document, None, 0, 0)],
        tip: 0,
        old_tip: 0,
        last_matched: 0,
        all_closed: true,
        line_end: 0,
        line_no: 0,
        offset: 0,
        column: 0,
        next_nonspace: 0,
        next_nonspace_column: 0,
        indent: 0,
        blank: false,
        partially_consumed_tab: false,
        skip_line: false,
        defs: Definitions::new(),
    };
    p.run();
    p.collect_footnotes();
    BlockTree {
        nodes: p.nodes,
        defs: p.defs,
    }
}

fn is_blank_text(s: &str) -> bool {
    s.bytes().all(|c| c == b' ' || c == b'\t')
}

impl<'a> BlockParser<'a> {
    fn run(&mut self) {
        let mut pos = self.front_matter();
        let len = self.bytes.len();
        while pos < len {
            let nl = self.bytes[pos..].iter().position(|&c| c == b'\n');
            let (mut end, next) = match nl {
                Some(n) => (pos + n, pos + n + 1),
                None => (len, len),
            };
            if nl.is_some() && end > pos && self.bytes[end - 1] == b'\r' {
                end -= 1;
            }
            self.line_no += 1;
            self.incorporate_line(pos, end);
            pos = next;
        }
        while self.tip != 0 {
            self.finalize(self.tip);
        }
        self.nodes[0].open = false;
    }

    /// Detects YAML/TOML front matter at the start of the document; returns the offset where
    /// ordinary line processing starts.
    fn front_matter(&mut self) -> usize {
        let b = self.bytes;
        let first_end = b.iter().position(|&c| c == b'\n').unwrap_or(b.len());
        let first = trim_cr_end(&self.src[..first_end]);
        let (format, fence_text) = match first.trim_end_matches([' ', '\t']) {
            "---" => (FrontMatterFormat::Yaml, "---"),
            "+++" => (FrontMatterFormat::Toml, "+++"),
            _ => return 0,
        };
        let open = TextRange::new(0, first.trim_end_matches([' ', '\t']).len());
        let mut pos = first_end + 1;
        let mut line_no = 1;
        while pos <= b.len() && first_end < b.len() {
            line_no += 1;
            let end = b[pos..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |n| pos + n);
            let text = trim_cr_end(&self.src[pos..end]);
            let trimmed = text.trim_end_matches([' ', '\t']);
            let closes =
                trimmed == fence_text || (format == FrontMatterFormat::Yaml && trimmed == "...");
            if closes {
                let close = TextRange::new(pos, pos + trimmed.len());
                let mut node = Node::new(
                    NodeKind::FrontMatter {
                        format,
                        open,
                        close,
                    },
                    Some(0),
                    0,
                    1,
                );
                node.open = false;
                node.markers = vec![open, close];
                let id = self.nodes.len();
                self.nodes.push(node);
                self.nodes[0].children.push(id);
                self.line_no = line_no;
                return (end + 1).min(b.len());
            }
            if end >= b.len() {
                break;
            }
            pos = end + 1;
        }
        0
    }

    fn peek(&self, i: usize) -> Option<u8> {
        if i < self.line_end {
            Some(self.bytes[i])
        } else {
            None
        }
    }

    fn kind(&self, id: usize) -> &NodeKind {
        &self.nodes[id].kind
    }

    fn find_next_nonspace(&mut self) {
        let mut i = self.offset;
        let mut cols = self.column;
        while i < self.line_end {
            match self.bytes[i] {
                b' ' => {
                    i += 1;
                    cols += 1;
                }
                b'\t' => {
                    i += 1;
                    cols += 4 - (cols % 4);
                }
                _ => break,
            }
        }
        self.blank = i >= self.line_end;
        self.next_nonspace = i;
        self.next_nonspace_column = cols;
        self.indent = cols - self.column;
    }

    fn advance_next_nonspace(&mut self) {
        self.offset = self.next_nonspace;
        self.column = self.next_nonspace_column;
        self.partially_consumed_tab = false;
    }

    /// Advances over `count` columns; a tab wider than what is left is consumed partially.
    /// (Markers are never tabs, so advancing over them in columns is the same as in bytes.)
    fn advance_offset(&mut self, mut count: usize) {
        while count > 0 && self.offset < self.line_end {
            if self.bytes[self.offset] == b'\t' {
                let chars_to_tab = 4 - (self.column % 4);
                self.partially_consumed_tab = chars_to_tab > count;
                let advance = chars_to_tab.min(count);
                self.column += advance;
                if !self.partially_consumed_tab {
                    self.offset += 1;
                }
                count -= advance;
            } else {
                self.partially_consumed_tab = false;
                self.offset += 1;
                self.column += 1;
                count -= 1;
            }
        }
    }

    fn accepts_lines(&self, id: usize) -> bool {
        matches!(
            self.kind(id),
            NodeKind::Paragraph
                | NodeKind::Code { .. }
                | NodeKind::Html(_)
                | NodeKind::Math { .. }
                | NodeKind::Table { .. }
        )
    }

    fn can_contain(&self, parent: usize, child: &NodeKind) -> bool {
        match self.kind(parent) {
            NodeKind::Document
            | NodeKind::Quote
            | NodeKind::Item(_)
            | NodeKind::Footnote { .. } => !matches!(child, NodeKind::Item(_)),
            NodeKind::List(_) => matches!(child, NodeKind::Item(_)),
            _ => false,
        }
    }

    fn add_child(&mut self, kind: NodeKind, start: usize) -> usize {
        while !self.can_contain(self.tip, &kind) {
            self.finalize(self.tip);
        }
        let id = self.nodes.len();
        let node = Node::new(kind, Some(self.tip), start, self.line_no);
        self.nodes.push(node);
        self.nodes[self.tip].children.push(id);
        self.tip = id;
        id
    }

    fn close_unmatched(&mut self) {
        if !self.all_closed {
            while self.old_tip != self.last_matched {
                let parent = self.nodes[self.old_tip].parent.unwrap_or(0);
                self.finalize(self.old_tip);
                self.old_tip = parent;
            }
            self.all_closed = true;
        }
    }

    fn incorporate_line(&mut self, line_start: usize, line_end: usize) {
        self.line_end = line_end;
        self.offset = line_start;
        self.column = 0;
        self.blank = false;
        self.partially_consumed_tab = false;
        self.skip_line = false;
        self.old_tip = self.tip;

        let mut all_matched = true;
        let mut container = 0usize;
        while let Some(&last) = self.nodes[container].children.last() {
            if !self.nodes[last].open {
                break;
            }
            container = last;
            self.find_next_nonspace();
            match self.continue_block(container) {
                Cont::Matched => {}
                Cont::Failed => all_matched = false,
                Cont::LineDone => return,
            }
            if !all_matched {
                container = self.nodes[container].parent.unwrap_or(0);
                break;
            }
        }
        self.all_closed = container == self.old_tip;
        self.last_matched = container;

        let mut matched_leaf = !matches!(
            self.kind(container),
            NodeKind::Paragraph | NodeKind::Table { .. }
        ) && self.accepts_lines(container);
        while !matched_leaf {
            self.find_next_nonspace();
            match self.try_starts(container) {
                Start::No => {
                    self.advance_next_nonspace();
                    break;
                }
                Start::Container => container = self.tip,
                Start::Leaf => {
                    container = self.tip;
                    matched_leaf = true;
                }
            }
        }

        if !self.all_closed && !self.blank && matches!(self.kind(self.tip), NodeKind::Paragraph) {
            self.add_line();
        } else {
            self.close_unmatched();
            if self.blank
                && let Some(&lc) = self.nodes[container].children.last()
            {
                self.nodes[lc].last_line_blank = true;
            }
            let fenced_code = matches!(self.kind(container), NodeKind::Code { fence: Some(_), .. });
            let item_like = matches!(
                self.kind(container),
                NodeKind::Item(_) | NodeKind::Footnote { .. }
            );
            let last_line_blank = self.blank
                && !(matches!(self.kind(container), NodeKind::Quote)
                    || fenced_code
                    || (item_like
                        && self.nodes[container].children.is_empty()
                        && self.nodes[container].line == self.line_no));
            let mut cur = Some(container);
            while let Some(c) = cur {
                self.nodes[c].last_line_blank = last_line_blank;
                cur = self.nodes[c].parent;
            }
            if self.accepts_lines(container) {
                if !self.skip_line {
                    self.add_line();
                }
                if let NodeKind::Html(kind) = *self.kind(container)
                    && (1..=5).contains(&kind)
                    && self.html_end_condition(kind)
                {
                    self.finalize(container);
                }
            } else if self.offset < self.line_end && !self.blank {
                self.add_child(NodeKind::Paragraph, self.next_nonspace);
                self.advance_next_nonspace();
                self.add_line();
            }
        }
    }

    /// Whether the rest of the line ends an HTML block of kind 1..=5 (the only kinds with an
    /// end condition; kind 5 is the CDATA section).
    fn html_end_condition(&self, kind: u8) -> bool {
        let rest = &self.bytes[self.offset.min(self.line_end)..self.line_end];
        let lower: Vec<u8> = rest.to_ascii_lowercase();
        let contains = |needle: &[u8]| lower.windows(needle.len()).any(|w| w == needle);
        match kind {
            1 => {
                contains(b"</script>")
                    || contains(b"</pre>")
                    || contains(b"</style>")
                    || contains(b"</textarea>")
            }
            2 => contains(b"-->"),
            3 => contains(b"?>"),
            4 => contains(b">"),
            _ => contains(b"]]>"),
        }
    }

    fn add_line(&mut self) {
        let mut start = self.offset;
        let mut pad = 0u8;
        if self.partially_consumed_tab {
            start += 1;
            pad = (4 - (self.column % 4)) as u8;
        }
        let start = start.min(self.line_end);
        let tip = self.tip;
        self.nodes[tip].lines.push(VerbatimLine {
            range: TextRange::new(start, self.line_end),
            pad,
        });
    }

    fn continue_block(&mut self, id: usize) -> Cont {
        match self.kind(id).clone() {
            NodeKind::Document | NodeKind::List(_) => Cont::Matched,
            NodeKind::Quote => {
                if self.indent < 4 && self.peek(self.next_nonspace) == Some(b'>') {
                    let gt = self.next_nonspace;
                    self.advance_next_nonspace();
                    self.advance_offset(1);
                    self.nodes[id].markers.push(TextRange::new(gt, gt + 1));
                    if matches!(self.peek(self.offset), Some(b' ' | b'\t')) {
                        self.advance_offset(1);
                    }
                    Cont::Matched
                } else {
                    Cont::Failed
                }
            }
            NodeKind::Item(data) => self.continue_item(id, data.marker_offset, data.padding),
            NodeKind::Footnote { .. } => self.continue_item(id, 0, 4),
            NodeKind::Heading { .. }
            | NodeKind::Thematic
            | NodeKind::PageBreak
            | NodeKind::FrontMatter { .. }
            | NodeKind::Def(_) => Cont::Failed,
            NodeKind::Code {
                fence: Some(fence),
                fence_indent,
                info,
            } => {
                if self.indent <= 3 && self.peek(self.next_nonspace) == Some(fence.ch as u8) {
                    let mut i = self.next_nonspace;
                    while self.peek(i) == Some(fence.ch as u8) {
                        i += 1;
                    }
                    let run = i - self.next_nonspace;
                    if run >= fence.len && is_blank_text(&self.src[i..self.line_end]) {
                        let close = TextRange::new(self.next_nonspace, i);
                        self.nodes[id].kind = NodeKind::Code {
                            fence: Some(Fence {
                                close: Some(close),
                                ..fence
                            }),
                            fence_indent,
                            info,
                        };
                        self.nodes[id].markers.push(close);
                        self.finalize(id);
                        return Cont::LineDone;
                    }
                }
                let mut left = fence_indent;
                while left > 0 && matches!(self.peek(self.offset), Some(b' ' | b'\t')) {
                    self.advance_offset(1);
                    left -= 1;
                }
                Cont::Matched
            }
            NodeKind::Code { fence: None, .. } => {
                if self.indent >= 4 {
                    self.advance_offset(4);
                    Cont::Matched
                } else if self.blank {
                    self.advance_next_nonspace();
                    Cont::Matched
                } else {
                    Cont::Failed
                }
            }
            NodeKind::Html(kind) => {
                if self.blank && (kind == 6 || kind == 7) {
                    Cont::Failed
                } else {
                    Cont::Matched
                }
            }
            NodeKind::Paragraph | NodeKind::Table { .. } => {
                if self.blank {
                    Cont::Failed
                } else {
                    Cont::Matched
                }
            }
            NodeKind::Math { open, .. } => self.continue_math(id, open),
        }
    }

    fn continue_item(&mut self, id: usize, marker_offset: usize, padding: usize) -> Cont {
        if self.blank {
            if self.nodes[id].children.is_empty() {
                Cont::Failed
            } else {
                self.advance_next_nonspace();
                Cont::Matched
            }
        } else if self.indent >= marker_offset + padding {
            self.advance_offset(marker_offset + padding);
            Cont::Matched
        } else {
            Cont::Failed
        }
    }

    fn continue_math(&mut self, id: usize, open: TextRange) -> Cont {
        let line = &self.src[self.offset.min(self.line_end)..self.line_end];
        let trimmed = line.trim_end_matches([' ', '\t']);
        if trimmed.ends_with("$$") && !trimmed.ends_with("\\$$") {
            let close_start = self.offset + trimmed.len() - 2;
            let before = &self.src[self.offset..close_start];
            if !is_blank_text(before) {
                self.nodes[id].lines.push(VerbatimLine {
                    range: TextRange::new(self.offset, close_start),
                    pad: 0,
                });
            }
            let close_range = TextRange::new(close_start, close_start + 2);
            self.nodes[id].kind = NodeKind::Math {
                open,
                close: Some(close_range),
            };
            self.nodes[id].markers.push(close_range);
            self.finalize(id);
            Cont::LineDone
        } else {
            Cont::Matched
        }
    }

    // ---------------------------------------------------------------- block starts

    fn try_starts(&mut self, container: usize) -> Start {
        let in_paragraph = matches!(self.kind(container), NodeKind::Paragraph);
        let lazy_paragraph =
            !self.all_closed && !self.blank && matches!(self.kind(self.tip), NodeKind::Paragraph);
        if !self.indented() {
            if self.start_block_quote() {
                return Start::Container;
            }
            if self.start_atx_heading() {
                return Start::Leaf;
            }
            if self.start_fence() {
                return Start::Leaf;
            }
            if self.start_page_break() {
                return Start::Leaf;
            }
            if self.start_html(in_paragraph, lazy_paragraph) {
                return Start::Leaf;
            }
            if self.start_thematic_break() {
                return Start::Leaf;
            }
            if !in_paragraph && self.start_footnote() {
                return Start::Container;
            }
            if self.start_math() {
                return Start::Leaf;
            }
        }
        if (!self.indented() || matches!(self.kind(container), NodeKind::List(_)))
            && self.start_list_item(container)
        {
            return Start::Container;
        }
        if self.indented()
            && !matches!(
                self.kind(self.tip),
                NodeKind::Paragraph | NodeKind::Table { .. }
            )
            && !self.blank
        {
            self.advance_offset(4);
            self.close_unmatched();
            let start = self.offset;
            self.add_child(
                NodeKind::Code {
                    fence: None,
                    fence_indent: 0,
                    info: None,
                },
                start,
            );
            return Start::Leaf;
        }
        if !self.indented() && self.start_table(container) {
            return Start::Leaf;
        }
        Start::No
    }

    fn indented(&self) -> bool {
        self.indent >= 4
    }

    fn start_block_quote(&mut self) -> bool {
        if self.peek(self.next_nonspace) != Some(b'>') {
            return false;
        }
        let gt = self.next_nonspace;
        self.advance_next_nonspace();
        self.advance_offset(1);
        if matches!(self.peek(self.offset), Some(b' ' | b'\t')) {
            self.advance_offset(1);
        }
        self.close_unmatched();
        let id = self.add_child(NodeKind::Quote, gt);
        self.nodes[id].markers.push(TextRange::new(gt, gt + 1));
        true
    }

    fn start_atx_heading(&mut self) -> bool {
        let ns = self.next_nonspace;
        let mut i = ns;
        while self.peek(i) == Some(b'#') {
            i += 1;
        }
        let level = i - ns;
        if level == 0 || level > 6 || !matches!(self.peek(i), None | Some(b' ' | b'\t')) {
            return false;
        }
        let marker = TextRange::new(ns, i);
        let mut cs = i;
        while matches!(self.peek(cs), Some(b' ' | b'\t')) {
            cs += 1;
        }
        let rest = &self.src[cs..self.line_end];
        let trimmed_len = rest.trim_end_matches([' ', '\t']).len();
        let mut content_end = cs + trimmed_len;
        let mut close = None;
        let hashes = self.src[cs..content_end]
            .bytes()
            .rev()
            .take_while(|&c| c == b'#')
            .count();
        if hashes > 0 {
            let run_start = content_end - hashes;
            if run_start == cs || matches!(self.bytes[run_start - 1], b' ' | b'\t') {
                close = Some(TextRange::new(run_start, content_end));
                content_end = cs + self.src[cs..run_start].trim_end_matches([' ', '\t']).len();
            }
        }
        self.close_unmatched();
        let id = self.add_child(
            NodeKind::Heading {
                level: level as u8,
                marker,
                content: TextRange::new(cs.min(content_end), content_end),
                close,
            },
            ns,
        );
        self.nodes[id].markers.push(marker);
        if let Some(c) = close {
            self.nodes[id].markers.push(c);
        }
        if content_end > cs {
            self.nodes[id].lines.push(VerbatimLine {
                range: TextRange::new(cs, content_end),
                pad: 0,
            });
        }
        self.offset = self.line_end;
        true
    }

    fn start_fence(&mut self) -> bool {
        let ns = self.next_nonspace;
        let Some(ch) = self.peek(ns).filter(|c| matches!(c, b'`' | b'~')) else {
            return false;
        };
        let mut i = ns;
        while self.peek(i) == Some(ch) {
            i += 1;
        }
        let len = i - ns;
        if len < 3 {
            return false;
        }
        if ch == b'`' && self.src[i..self.line_end].contains('`') {
            return false;
        }
        let rest = &self.src[i..self.line_end];
        let lead = rest.len() - rest.trim_start_matches([' ', '\t']).len();
        let info_text = rest.trim_matches([' ', '\t']);
        let info =
            (!info_text.is_empty()).then(|| TextRange::new(i + lead, i + lead + info_text.len()));
        let fence_indent = self.indent;
        self.close_unmatched();
        let fence = Fence {
            ch: char::from(ch),
            len,
            open: TextRange::new(ns, i),
            close: None,
        };
        let id = self.add_child(
            NodeKind::Code {
                fence: Some(fence),
                fence_indent,
                info,
            },
            ns,
        );
        self.nodes[id].markers.push(TextRange::new(ns, i));
        self.offset = self.line_end;
        self.skip_line = true;
        true
    }

    fn start_page_break(&mut self) -> bool {
        let ns = self.next_nonspace;
        let line = &self.src[ns..self.line_end];
        let t = line.trim_end_matches([' ', '\t']);
        let Some(inner) = t.strip_prefix("<!--").and_then(|r| r.strip_suffix("-->")) else {
            return false;
        };
        if !inner
            .trim_matches([' ', '\t'])
            .eq_ignore_ascii_case("pagebreak")
        {
            return false;
        }
        self.close_unmatched();
        let range = TextRange::new(ns, ns + t.len());
        let id = self.add_child(NodeKind::PageBreak, ns);
        self.nodes[id].markers.push(range);
        self.offset = self.line_end;
        true
    }

    fn html_block_kind(&self, in_paragraph: bool, lazy_paragraph: bool) -> Option<u8> {
        let ns = self.next_nonspace;
        let s = &self.bytes[ns..self.line_end];
        if s.first() != Some(&b'<') {
            return None;
        }
        let lower = s.to_ascii_lowercase();
        let after = |n: usize| lower.get(n).copied();
        let tag_boundary = |n: usize| matches!(after(n), None | Some(b' ' | b'\t' | b'>' | 0x0c));
        for name in ["script", "pre", "style", "textarea"] {
            if lower[1..].starts_with(name.as_bytes()) && tag_boundary(1 + name.len()) {
                return Some(1);
            }
        }
        if lower.starts_with(b"<!--") {
            return Some(2);
        }
        if lower.starts_with(b"<?") {
            return Some(3);
        }
        if lower.starts_with(b"<!") && lower.get(2).is_some_and(u8::is_ascii_alphabetic) {
            return Some(4);
        }
        if lower.starts_with(b"<![cdata[") {
            return Some(5);
        }
        let name_start = if lower.starts_with(b"</") { 2 } else { 1 };
        let name_len = lower[name_start..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        let name = &lower[name_start..name_start + name_len];
        if BLOCK_TAGS
            .binary_search_by(|t| t.as_bytes().cmp(name))
            .is_ok()
        {
            let end = name_start + name_len;
            let boundary = match after(end) {
                None | Some(b' ' | b'\t' | 0x0c | b'>') => true,
                Some(b'/') => after(end + 1) == Some(b'>'),
                _ => false,
            };
            if boundary {
                return Some(6);
            }
        }
        if !in_paragraph && !lazy_paragraph {
            let end = if s.get(1) == Some(&b'/') {
                scan::closing_tag_end(s, 0)
            } else {
                scan::open_tag_end(s, 0)
            };
            if let Some(end) = end
                && s[end..].iter().all(|&c| matches!(c, b' ' | b'\t'))
            {
                return Some(7);
            }
        }
        None
    }

    fn start_html(&mut self, in_paragraph: bool, lazy_paragraph: bool) -> bool {
        let Some(kind) = self.html_block_kind(in_paragraph, lazy_paragraph) else {
            return false;
        };
        self.close_unmatched();
        let start = self.offset;
        self.add_child(NodeKind::Html(kind), start);
        true
    }

    fn start_thematic_break(&mut self) -> bool {
        let ns = self.next_nonspace;
        let Some(ch) = self.peek(ns).filter(|c| matches!(c, b'*' | b'-' | b'_')) else {
            return false;
        };
        let mut count = 0;
        let mut i = ns;
        while i < self.line_end {
            let c = self.bytes[i];
            if c == ch {
                count += 1;
            } else if c != b' ' && c != b'\t' {
                return false;
            }
            i += 1;
        }
        if count < 3 {
            return false;
        }
        let end = ns
            + self.src[ns..self.line_end]
                .trim_end_matches([' ', '\t'])
                .len();
        self.close_unmatched();
        let id = self.add_child(NodeKind::Thematic, ns);
        self.nodes[id].markers.push(TextRange::new(ns, end));
        self.offset = self.line_end;
        true
    }

    fn start_footnote(&mut self) -> bool {
        let ns = self.next_nonspace;
        let s = &self.src[ns..self.line_end];
        let Some(rest) = s.strip_prefix("[^") else {
            return false;
        };
        let Some(close) = rest.find(']') else {
            return false;
        };
        let label = &rest[..close];
        if label.is_empty() || label.contains(|c: char| c.is_whitespace() || c == '[') {
            return false;
        }
        if rest.as_bytes().get(close + 1) != Some(&b':') {
            return false;
        }
        let marker = TextRange::new(ns, ns + 2 + close + 2);
        let label = label.to_string();
        self.advance_next_nonspace();
        self.advance_offset(marker.len());
        let mut spaces = 0;
        while matches!(self.peek(self.offset + spaces), Some(b' ' | b'\t')) && spaces < 1 {
            spaces += 1;
        }
        self.advance_offset(spaces);
        self.close_unmatched();
        let id = self.add_child(NodeKind::Footnote { label }, ns);
        self.nodes[id].markers.push(marker);
        true
    }

    fn start_math(&mut self) -> bool {
        let ns = self.next_nonspace;
        let s = &self.src[ns..self.line_end];
        if !s.starts_with("$$") {
            return false;
        }
        let rest = &s[2..];
        let trimmed = rest.trim_end_matches([' ', '\t']);
        let open = TextRange::new(ns, ns + 2);
        if let Some(pos) = trimmed.find("$$") {
            if pos + 2 != trimmed.len() {
                return false;
            }
            let content_start = ns + 2;
            let content_end = ns + 2 + pos;
            self.close_unmatched();
            let id = self.add_child(
                NodeKind::Math {
                    open,
                    close: Some(TextRange::new(content_end, content_end + 2)),
                },
                ns,
            );
            self.nodes[id].markers.push(open);
            self.nodes[id]
                .markers
                .push(TextRange::new(content_end, content_end + 2));
            if !is_blank_text(&self.src[content_start..content_end]) {
                self.nodes[id].lines.push(VerbatimLine {
                    range: TextRange::new(content_start, content_end),
                    pad: 0,
                });
            }
            self.offset = self.line_end;
            self.skip_line = true;
            self.finalize(id);
            return true;
        }
        self.close_unmatched();
        let id = self.add_child(NodeKind::Math { open, close: None }, ns);
        self.nodes[id].markers.push(open);
        if !trimmed.trim_start_matches([' ', '\t']).is_empty() {
            let lead = trimmed.len() - trimmed.trim_start_matches([' ', '\t']).len();
            self.nodes[id].lines.push(VerbatimLine {
                range: TextRange::new(ns + 2 + lead, ns + 2 + trimmed.len()),
                pad: 0,
            });
        }
        self.offset = self.line_end;
        self.skip_line = true;
        true
    }

    fn start_list_item(&mut self, container: usize) -> bool {
        let marker_start = self.next_nonspace;
        let Some(data) = self.parse_list_marker(container) else {
            return false;
        };
        self.close_unmatched();
        let matches_list = match (self.kind(self.tip), self.kind(container)) {
            (NodeKind::List(_), NodeKind::List(prev)) => {
                prev.ordered == data.ordered && prev.delimiter == data.delimiter
            }
            _ => false,
        };
        if !matches_list {
            self.add_child(NodeKind::List(data.clone()), marker_start);
        }
        let marker = data.marker;
        let id = self.add_child(NodeKind::Item(data), marker_start);
        self.nodes[id].markers.push(marker);
        true
    }

    fn parse_list_marker(&mut self, container: usize) -> Option<ListData> {
        if self.indent >= 4 {
            return None;
        }
        let ns = self.next_nonspace;
        let c = self.peek(ns)?;
        let (ordered, delimiter, start, len) = if matches!(c, b'*' | b'+' | b'-') {
            let d = match c {
                b'*' => ListDelimiter::Star,
                b'+' => ListDelimiter::Plus,
                _ => ListDelimiter::Dash,
            };
            (false, d, 0u32, 1usize)
        } else if c.is_ascii_digit() {
            let mut i = ns;
            while self.peek(i).is_some_and(|d| d.is_ascii_digit()) {
                i += 1;
            }
            let digits = i - ns;
            if digits > 9 {
                return None;
            }
            let d = match self.peek(i) {
                Some(b'.') => ListDelimiter::Period,
                Some(b')') => ListDelimiter::Paren,
                _ => return None,
            };
            // At most nine digits, so the number fits in a `u32`.
            let number = self.bytes[ns..i]
                .iter()
                .fold(0u32, |n, d| n * 10 + u32::from(d - b'0'));
            if matches!(self.kind(container), NodeKind::Paragraph) && number != 1 {
                return None;
            }
            (true, d, number, digits + 1)
        } else {
            return None;
        };
        let after = ns + len;
        if !matches!(self.peek(after), None | Some(b' ' | b'\t')) {
            return None;
        }
        if matches!(self.kind(container), NodeKind::Paragraph)
            && is_blank_text(&self.src[after.min(self.line_end)..self.line_end])
        {
            return None;
        }
        let marker_offset = self.indent;
        self.advance_next_nonspace();
        self.advance_offset(len);
        let spaces_start_col = self.column;
        let spaces_start_offset = self.offset;
        loop {
            self.advance_offset(1);
            if !(self.column - spaces_start_col < 5
                && matches!(self.peek(self.offset), Some(b' ' | b'\t')))
            {
                break;
            }
        }
        let blank_item = self.peek(self.offset).is_none();
        let spaces_after = self.column - spaces_start_col;
        let padding;
        if !(1..5).contains(&spaces_after) || blank_item {
            padding = len + 1;
            self.column = spaces_start_col;
            self.offset = spaces_start_offset;
            self.partially_consumed_tab = false;
            if matches!(self.peek(self.offset), Some(b' ' | b'\t')) {
                self.advance_offset(1);
            }
        } else {
            padding = len + spaces_after;
        }
        Some(ListData {
            ordered,
            delimiter,
            start,
            marker_offset,
            padding,
            marker: TextRange::new(ns, ns + len),
        })
    }

    // ---------------------------------------------------------------- tables

    fn start_table(&mut self, container: usize) -> bool {
        if !matches!(self.kind(container), NodeKind::Paragraph) {
            return false;
        }
        let ns = self.next_nonspace;
        let delim_text = &self.src[ns..self.line_end];
        let Some(alignments) = parse_delimiter_row(delim_text) else {
            return false;
        };
        // An open paragraph always has at least the line that created it.
        let header = *self.nodes[container]
            .lines
            .last()
            .expect("open paragraph has a line");
        let header_text = header.range.get(self.src).unwrap_or("");
        if split_row(header_text, header.range.start).cells.len() != alignments.len() {
            return false;
        }
        if !header_text.contains('|') && !delim_text.contains('|') {
            return false;
        }
        self.close_unmatched();
        let header_trimmed = trim_range(self.src, header.range);
        let delim_end = ns + delim_text.trim_end_matches([' ', '\t']).len();
        let delim_range = TextRange::new(ns, delim_end);
        let only_header = self.nodes[container].lines.len() == 1;
        let id = if only_header {
            let node = &mut self.nodes[container];
            node.kind = NodeKind::Table { alignments };
            node.lines.clear();
            node.start = header_trimmed.start;
            container
        } else {
            self.nodes[container].lines.pop();
            self.finalize(container);
            self.add_child(NodeKind::Table { alignments }, header_trimmed.start)
        };
        self.nodes[id].lines.push(VerbatimLine {
            range: header_trimmed,
            pad: 0,
        });
        self.nodes[id].lines.push(VerbatimLine {
            range: delim_range,
            pad: 0,
        });
        self.nodes[id].markers.push(delim_range);
        self.nodes[id].open = true;
        self.tip = id;
        self.offset = self.line_end;
        self.skip_line = true;
        true
    }

    // ---------------------------------------------------------------- finalization

    fn finalize(&mut self, id: usize) {
        self.nodes[id].open = false;
        self.tip = self.nodes[id].parent.unwrap_or(0);
        match self.kind(id).clone() {
            NodeKind::Paragraph => self.finalize_paragraph(id),
            NodeKind::Code { fence: None, .. } => {
                let src = self.src;
                let node = &mut self.nodes[id];
                while node.lines.last().is_some_and(|l| node_line_blank(src, l)) {
                    node.lines.pop();
                }
            }
            NodeKind::Html(_) => {
                let src = self.src;
                let node = &mut self.nodes[id];
                while node.lines.last().is_some_and(|l| node_line_blank(src, l)) {
                    node.lines.pop();
                }
            }
            NodeKind::List(_) => self.finalize_list(id),
            _ => {}
        }
    }

    fn ends_with_blank_line(&self, mut id: usize) -> bool {
        loop {
            let node = &self.nodes[id];
            if node.last_line_blank {
                return true;
            }
            if matches!(
                node.kind,
                NodeKind::List(_) | NodeKind::Item(_) | NodeKind::Footnote { .. }
            ) {
                match node.children.last() {
                    Some(&last) => id = last,
                    None => return false,
                }
            } else {
                return false;
            }
        }
    }

    fn finalize_list(&mut self, id: usize) {
        let items = self.nodes[id].children.clone();
        let mut tight = true;
        'outer: for (n, &item) in items.iter().enumerate() {
            let has_next = n + 1 < items.len();
            if self.ends_with_blank_line(item) && has_next {
                tight = false;
                break;
            }
            let subs = self.nodes[item].children.clone();
            for (m, &sub) in subs.iter().enumerate() {
                if self.ends_with_blank_line(sub) && (has_next || m + 1 < subs.len()) {
                    tight = false;
                    break 'outer;
                }
            }
        }
        self.nodes[id].tight = tight;
    }

    fn finalize_paragraph(&mut self, id: usize) {
        let ranges: Vec<TextRange> = self.nodes[id].lines.iter().map(|l| l.range).collect();
        let source = Source::from_lines(self.src, &ranges);
        let b = source.buf.as_bytes();
        let mut pos = 0usize;
        let mut defs = Vec::new();
        while b.get(pos) == Some(&b'[') {
            let Some((def, end)) = parse_reference(&source, pos) else {
                break;
            };
            defs.push(def);
            pos = end;
        }
        if !defs.is_empty() {
            let consumed_lines = source.line_starts.partition_point(|&s| s < pos);
            let parent = self.nodes[id].parent.unwrap_or(0);
            for (range, markers, def) in defs {
                let start = range.start;
                self.defs.define_link(LinkDefinition {
                    label: normalize_label(&def.label),
                    dest: def.dest.clone(),
                    title: def.title.clone(),
                });
                let mut node =
                    Node::new(NodeKind::Def(def), Some(parent), start, self.nodes[id].line);
                node.open = false;
                node.markers = markers;
                node.lines.push(VerbatimLine { range, pad: 0 });
                let def_id = self.nodes.len();
                self.nodes.push(node);
                let at = self.nodes[parent]
                    .children
                    .iter()
                    .position(|&c| c == id)
                    .unwrap_or(0);
                self.nodes[parent].children.insert(at, def_id);
            }
            self.nodes[id].lines.drain(..consumed_lines);
            if self.nodes[id].lines.is_empty() {
                self.nodes[parent].children.retain(|&c| c != id);
                self.nodes[id].removed = true;
                return;
            }
        }
        // A paragraph is only ever finalized with a line: it starts with one, a table takes at
        // most the lines before its delimiter row and leaves one, and the block above returns
        // when the definitions take them all.
        let src = self.src;
        let node = &mut self.nodes[id];
        let last = node.lines.last_mut().expect("paragraph has a line");
        let trimmed = trim_range(src, last.range);
        last.range = TextRange::new(last.range.start, trimmed.end.max(last.range.start));
        node.start = node.lines[0].range.start;
    }

    /// Registers footnote definitions in document order (first definition of a label wins).
    fn collect_footnotes(&mut self) {
        let mut order = Vec::new();
        let mut stack = vec![0usize];
        while let Some(id) = stack.pop() {
            for &c in self.nodes[id].children.iter().rev() {
                stack.push(c);
            }
            if let NodeKind::Footnote { label } = &self.nodes[id].kind {
                order.push((id, label.clone()));
            }
        }
        for (id, label) in order {
            let range = self.footnote_range(id);
            self.defs.define_footnote(&label, range);
        }
    }

    fn footnote_range(&self, id: usize) -> TextRange {
        let node = &self.nodes[id];
        let marker_end = node.markers.last().map_or(node.start, |m| m.end);
        let mut end = marker_end;
        for &c in &node.children {
            end = end.max(node_end(&self.nodes, c));
        }
        TextRange::new(node.start, end)
    }
}

fn node_line_blank(src: &str, line: &VerbatimLine) -> bool {
    is_blank_text(line.range.get(src).unwrap_or(""))
}

fn trim_cr_end(s: &str) -> &str {
    s.strip_suffix('\r').unwrap_or(s)
}

/// Range of `range` without surrounding spaces/tabs.
pub(super) fn trim_range(src: &str, range: TextRange) -> TextRange {
    let text = range.get(src).unwrap_or("");
    let lead = text.len() - text.trim_start_matches([' ', '\t']).len();
    let trimmed = text.trim_matches([' ', '\t']);
    TextRange::new(range.start + lead, range.start + lead + trimmed.len())
}

/// Largest source offset covered by node `id` (markers, lines and children).
pub(super) fn node_end(nodes: &[Node], id: usize) -> usize {
    let node = &nodes[id];
    let mut end = node.start;
    for m in &node.markers {
        end = end.max(m.end);
    }
    for l in &node.lines {
        end = end.max(l.range.end);
    }
    if let NodeKind::Heading { content, .. } = &node.kind {
        end = end.max(content.end);
    }
    if let NodeKind::Code { info: Some(i), .. } = &node.kind {
        end = end.max(i.end);
    }
    for &c in &node.children {
        end = end.max(node_end(nodes, c));
    }
    end
}

const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

// -------------------------------------------------------------------- tables

/// One row of a table split at unescaped pipes.
pub(super) struct SplitRow {
    /// Unescaped pipe ranges.
    pub pipes: Vec<TextRange>,
    /// `(padded cell range, trimmed content range)`.
    pub cells: Vec<(TextRange, TextRange)>,
}

/// Splits one table row (text starting at document offset `base`).
pub(super) fn split_row(text: &str, base: usize) -> SplitRow {
    let b = text.as_bytes();
    let trimmed_end = text.trim_end_matches([' ', '\t']).len();
    let lead = text.len() - text.trim_start_matches([' ', '\t']).len();
    let mut pipes = Vec::new();
    let mut i = lead;
    while i < trimmed_end {
        match b[i] {
            b'\\' if i + 1 < trimmed_end => i += 2,
            b'|' => {
                pipes.push(i);
                i += 1;
            }
            _ => i += 1,
        }
    }
    let mut cells = Vec::new();
    let mut cell_start = lead;
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    let leading_pipe = pipes.first() == Some(&lead);
    let trailing_pipe = pipes.last().is_some_and(|&p| p + 1 == trimmed_end)
        && pipes.len() > usize::from(leading_pipe);
    for (n, &p) in pipes.iter().enumerate() {
        if n == 0 && leading_pipe {
            cell_start = p + 1;
            continue;
        }
        bounds.push((cell_start, p));
        cell_start = p + 1;
    }
    if !trailing_pipe && cell_start <= trimmed_end {
        bounds.push((cell_start, trimmed_end));
    }
    for (s, e) in bounds {
        let padded = TextRange::new(base + s, base + e);
        let seg = &text[s..e];
        let l = seg.len() - seg.trim_start_matches([' ', '\t']).len();
        let content_len = seg.trim_matches([' ', '\t']).len();
        let content = TextRange::new(base + s + l, base + s + l + content_len);
        cells.push((padded, content));
    }
    SplitRow {
        pipes: pipes
            .iter()
            .map(|&p| TextRange::new(base + p, base + p + 1))
            .collect(),
        cells,
    }
}

/// Alignments of a table delimiter row, or `None` when `text` is not one.
fn parse_delimiter_row(text: &str) -> Option<Vec<Alignment>> {
    let row = split_row(text, 0);
    let mut out = Vec::with_capacity(row.cells.len());
    for (_, content) in &row.cells {
        // `split_row` cut the cells out of `text`, so the range is always valid.
        let c = &text[content.start..content.end];
        let left = c.starts_with(':');
        let right = c.ends_with(':') && c.len() > 1;
        let dashes = c.trim_start_matches(':').trim_end_matches(':');
        if dashes.is_empty() || !dashes.bytes().all(|d| d == b'-') {
            return None;
        }
        out.push(match (left, right) {
            (true, true) => Alignment::Center,
            (true, false) => Alignment::Left,
            (false, true) => Alignment::Right,
            (false, false) => Alignment::None,
        });
    }
    // Outside the cells `split_row` leaves only pipes and the leading and trailing blanks, and
    // every cell was checked above, so nothing else can be left over.
    (!out.is_empty()).then_some(out)
}

// ------------------------------------------------------- link reference definitions

type DefMatch = (TextRange, Vec<TextRange>, LinkReferenceDefinition);

/// Parses one link reference definition at `pos` of `source.buf`. Returns the definition
/// (range, markers, data) and the buffer position after its line ending.
fn parse_reference(source: &Source, pos: usize) -> Option<(DefMatch, usize)> {
    let b = source.buf.as_bytes();
    let label_end = scan::link_label_end(b, pos)?;
    if b.get(label_end) != Some(&b':') {
        return None;
    }
    let label_text = &source.buf[pos + 1..label_end - 1];
    if normalize_label(label_text).is_empty() {
        return None;
    }
    let dest_start = scan::skip_spaces_and_newline(b, label_end + 1);
    let ((ds, de), dest_end) = scan::link_destination(b, dest_start)?;
    if b.get(dest_start) != Some(&b'<') && ds == de {
        return None;
    }
    let after_dest = dest_end;
    let line_end_after = |p: usize| -> Option<usize> {
        let mut i = p;
        while matches!(b.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        match b.get(i) {
            None => Some(b.len()),
            Some(b'\n') => Some(i + 1),
            _ => None,
        }
    };
    let title_start = scan::skip_spaces_and_newline(b, after_dest);
    let mut title = None;
    if title_start != after_dest
        && let Some(((ts, te), tend)) = scan::link_title(b, title_start)
        && let Some(next) = line_end_after(tend)
    {
        title = Some((ts, te, tend, next));
    }
    let (def_end_pos, next, title_info) = match title {
        Some((ts, te, tend, next)) => (tend, next, Some((ts, te))),
        None => (after_dest, line_end_after(after_dest)?, None),
    };
    let dest_raw = &source.buf[ds..de];
    let range = source.range(pos, def_end_pos);
    let marker = source.range(pos, label_end + 1);
    let def = LinkReferenceDefinition {
        label: label_text.to_string(),
        label_range: source.range(pos + 1, label_end - 1),
        dest: scan::unescape(dest_raw),
        dest_range: source.range(ds, de),
        title: title_info.map(|(ts, te)| scan::unescape(&source.buf[ts..te])),
        title_range: title_info.map(|(ts, te)| source.range(ts, te)),
    };
    Some(((range, vec![marker], def), next))
}
