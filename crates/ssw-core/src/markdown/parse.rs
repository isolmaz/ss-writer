//! The reference Markdown parser: [`parse_document`].
//!
//! Two passes: [`block`] recognizes the block structure line by line and collects link
//! reference and footnote definitions; the inline pass then parses every leaf block's text with
//! those definitions available. Both passes keep exact source ranges.

mod block;
mod inline;
mod scan;
mod source;
#[cfg(test)]
mod tests;

use crate::text::TextRange;

use super::ast::{
    Block, BlockKind, CalloutKind, CodeBlock, Document, HtmlBlock, Inline, InlineKind, Markers,
    MathBlock, Table, TableCell, TableRow, Task, marks,
};
use super::definitions::Definitions;
use block::{Node, NodeKind};
use source::Source;

/// Parses `src` into a [`Document`]. Offsets in the tree are UTF-8 byte offsets into `src`.
pub fn parse_document(src: &str) -> Document {
    let tree = block::parse_blocks(src);
    let mut conv = Converter {
        src,
        nodes: tree.nodes,
        defs: tree.defs,
    };
    let blocks = conv.children(0);
    Document {
        blocks,
        definitions: conv.defs,
        len: src.len(),
    }
}

/// Resolves backslash escapes and character references in `text` (the way link destinations,
/// titles and info strings are interpreted).
pub fn unescape_markdown(text: &str) -> String {
    scan::unescape(text)
}

struct Converter<'a> {
    src: &'a str,
    nodes: Vec<Node>,
    defs: Definitions,
}

fn span(blocks: &[Block], fallback: usize) -> TextRange {
    match (blocks.first(), blocks.last()) {
        (Some(first), Some(last)) => {
            TextRange::new(first.range.start, last.range.end.max(first.range.start))
        }
        _ => TextRange::at(fallback),
    }
}

fn lines_span(lines: &[crate::markdown::ast::VerbatimLine], fallback: usize) -> TextRange {
    match (lines.first(), lines.last()) {
        (Some(first), Some(last)) => {
            TextRange::new(first.range.start, last.range.end.max(first.range.start))
        }
        _ => TextRange::at(fallback),
    }
}

impl Converter<'_> {
    fn children(&mut self, parent: usize) -> Vec<Block> {
        let ids = self.nodes[parent].children.clone();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if self.nodes[id].removed {
                continue;
            }
            out.push(self.convert(id));
        }
        out
    }

    fn block(
        range: TextRange,
        content: TextRange,
        markers: Markers,
        kind: BlockKind,
        children: Vec<Block>,
        inlines: Vec<Inline>,
    ) -> Block {
        Block {
            range,
            content,
            markers,
            kind,
            children,
            inlines,
        }
    }

    fn inlines_of_lines(
        &mut self,
        lines: &[crate::markdown::ast::VerbatimLine],
        owner: TextRange,
    ) -> Vec<Inline> {
        if lines.is_empty() {
            return Vec::new();
        }
        let ranges: Vec<TextRange> = lines.iter().map(|l| l.range).collect();
        let source = Source::from_lines(self.src, &ranges);
        inline::parse_inlines(&mut self.defs, owner, &source, false)
    }

    fn convert(&mut self, id: usize) -> Block {
        let kind = self.nodes[id].kind.clone();
        let start = self.nodes[id].start;
        let markers: Markers = self.nodes[id].markers.iter().copied().collect();
        match kind {
            NodeKind::List(data) => {
                let children = self.children(id);
                let range = span(&children, start);
                Self::block(
                    range,
                    range,
                    Markers::new(),
                    BlockKind::List {
                        ordered: data.ordered,
                        start: data.start,
                        delimiter: data.delimiter,
                        tight: self.nodes[id].tight,
                    },
                    children,
                    Vec::new(),
                )
            }
            NodeKind::Item(data) => self.convert_item(id, data.marker),
            NodeKind::Quote => self.convert_quote(id),
            NodeKind::Footnote { label } => {
                let children = self.children(id);
                let marker_end = markers.last().map_or(start, |m| m.end);
                let content = span(&children, marker_end);
                let end = content.end.max(marker_end);
                Self::block(
                    TextRange::new(start, end),
                    content,
                    markers,
                    BlockKind::FootnoteDefinition { label },
                    children,
                    Vec::new(),
                )
            }
            NodeKind::Heading {
                level,
                marker,
                content,
                close,
            } => {
                let end = close.map_or(content.end, |c| c.end).max(marker.end);
                let range = TextRange::new(start, end);
                let lines = self.nodes[id].lines.clone();
                let inlines = self.inlines_of_lines(&lines, range);
                Self::block(
                    range,
                    content,
                    markers,
                    BlockKind::Heading { level, marker },
                    Vec::new(),
                    inlines,
                )
            }
            // The root node has no lines and starts at 0: an empty paragraph at offset 0.
            NodeKind::Document | NodeKind::Paragraph => {
                let lines = self.nodes[id].lines.clone();
                let range = lines_span(&lines, start);
                let inlines = self.inlines_of_lines(&lines, range);
                Self::block(
                    range,
                    range,
                    Markers::new(),
                    BlockKind::Paragraph,
                    Vec::new(),
                    inlines,
                )
            }
            NodeKind::Code { fence, info, .. } => {
                let lines = self.nodes[id].lines.clone();
                let (range, content) = match fence {
                    Some(f) => {
                        let open_end = info.map_or(f.open.end, |i| i.end);
                        let content = lines_span(&lines, open_end);
                        let end = f.close.map_or_else(|| content.end.max(open_end), |c| c.end);
                        (TextRange::new(f.open.start, end), content)
                    }
                    None => {
                        let content = lines_span(&lines, start);
                        let s = content.start.min(start);
                        (TextRange::new(s, content.end.max(s)), content)
                    }
                };
                Self::block(
                    range,
                    content,
                    markers,
                    BlockKind::CodeBlock(CodeBlock { fence, info, lines }),
                    Vec::new(),
                    Vec::new(),
                )
            }
            NodeKind::Math { open, close } => {
                let lines = self.nodes[id].lines.clone();
                let content = lines_span(&lines, open.end);
                let end = close.map_or(content.end.max(open.end), |c| c.end);
                Self::block(
                    TextRange::new(open.start, end),
                    content,
                    markers,
                    BlockKind::MathBlock(MathBlock {
                        lines,
                        closed: close.is_some(),
                    }),
                    Vec::new(),
                    Vec::new(),
                )
            }
            NodeKind::Html(kind) => {
                let lines = self.nodes[id].lines.clone();
                let content = lines_span(&lines, start);
                let range = TextRange::new(start.min(content.start), content.end.max(start));
                let mut inlines = Vec::new();
                if lines.len() == 1 {
                    let trimmed = block::trim_range(self.src, lines[0].range);
                    let parsed = self.inlines_of_lines(
                        &[crate::markdown::ast::VerbatimLine {
                            range: trimmed,
                            pad: 0,
                        }],
                        range,
                    );
                    if parsed.len() == 1 && matches!(parsed[0].kind, InlineKind::Image(_)) {
                        inlines = parsed;
                    }
                }
                Self::block(
                    range,
                    content,
                    Markers::new(),
                    BlockKind::HtmlBlock(HtmlBlock { kind, lines }),
                    Vec::new(),
                    inlines,
                )
            }
            NodeKind::Thematic => {
                let r = markers.first().copied().unwrap_or(TextRange::at(start));
                Self::block(
                    r,
                    r,
                    markers,
                    BlockKind::ThematicBreak,
                    Vec::new(),
                    Vec::new(),
                )
            }
            NodeKind::PageBreak => {
                let r = markers.first().copied().unwrap_or(TextRange::at(start));
                Self::block(r, r, markers, BlockKind::PageBreak, Vec::new(), Vec::new())
            }
            NodeKind::FrontMatter {
                format,
                open,
                close,
            } => {
                let after_open = if self.src[open.end..].starts_with("\r\n") {
                    open.end + 2
                } else {
                    open.end + 1
                };
                let content = TextRange::new(after_open.min(close.start), close.start);
                Self::block(
                    TextRange::new(0, close.end),
                    content,
                    markers,
                    BlockKind::FrontMatter { format },
                    Vec::new(),
                    Vec::new(),
                )
            }
            NodeKind::Table { alignments } => self.convert_table(id, alignments, markers),
            NodeKind::Def(def) => {
                let r = self.nodes[id].lines[0].range;
                let content_start = def.dest_range.start.min(r.end);
                Self::block(
                    r,
                    TextRange::new(content_start, r.end),
                    markers,
                    BlockKind::LinkReferenceDefinition(def),
                    Vec::new(),
                    Vec::new(),
                )
            }
        }
    }

    fn convert_item(&mut self, id: usize, marker: TextRange) -> Block {
        let mut task = None;
        let first_child = self.nodes[id].children.first().copied();
        if let Some(pid) = first_child {
            task = self.detect_task(pid);
        }
        let children = self.children(id);
        let content = span(&children, marker.end);
        let mut markers = marks(&[marker]);
        if let Some(t) = &task {
            markers.push(t.checkbox);
        }
        let end = content
            .end
            .max(marker.end)
            .max(task.map_or(0, |t| t.checkbox.end));
        let content_start = content.start.max(marker.end);
        Self::block(
            TextRange::new(marker.start, end),
            TextRange::new(content_start.min(end), end),
            markers,
            BlockKind::ListItem { marker, task },
            children,
            Vec::new(),
        )
    }

    /// Recognizes `[ ]` / `[x]` at the start of the first paragraph of an item and removes it
    /// from the paragraph text.
    fn detect_task(&mut self, pid: usize) -> Option<Task> {
        if !matches!(self.nodes[pid].kind, NodeKind::Paragraph) || self.nodes[pid].removed {
            return None;
        }
        // A paragraph that is not removed always has its first line.
        let line = self.nodes[pid].lines[0];
        let text = line.range.get(self.src).unwrap_or("");
        let b = text.as_bytes();
        if b.len() < 4 || b[0] != b'[' || b[2] != b']' || !matches!(b[3], b' ' | b'\t') {
            return None;
        }
        let checked = match b[1] {
            b' ' => false,
            b'x' | b'X' => true,
            _ => return None,
        };
        let rest = &text[3..];
        let skipped = rest.len() - rest.trim_start_matches([' ', '\t']).len();
        let new_start = line.range.start + 3 + skipped;
        // The paragraph is finalized (last line trimmed), so a lone line with a 4th
        // whitespace byte always has text after the checkbox.
        let checkbox = TextRange::new(line.range.start, line.range.start + 3);
        if new_start >= line.range.end {
            self.nodes[pid].lines.remove(0);
        } else {
            self.nodes[pid].lines[0].range.start = new_start;
        }
        let first = self.nodes[pid].lines[0].range.start;
        self.nodes[pid].start = first;
        Some(Task { checked, checkbox })
    }

    fn convert_quote(&mut self, id: usize) -> Block {
        let start = self.nodes[id].start;
        let mut markers: Vec<TextRange> = self.nodes[id].markers.clone();
        let mut callout = None;
        if let Some(&pid) = self.nodes[id].children.first() {
            callout = self.detect_callout(pid);
        }
        let children = self.children(id);
        let range_end = span(&children, start)
            .end
            .max(markers.iter().map(|m| m.end).max().unwrap_or(start))
            .max(callout.as_ref().map_or(0, |(_, tag, title)| {
                title.map_or(tag.end, |title| title.end)
            }));
        let range = TextRange::new(start, range_end);
        if let Some((kind, tag, title)) = callout {
            markers.push(tag);
            markers.sort();
            let content_start = title.map_or(tag.end, |t| t.end);
            return Self::block(
                range,
                TextRange::new(content_start.min(range.end), range.end),
                markers.into_iter().collect(),
                BlockKind::Callout { kind, tag, title },
                children,
                Vec::new(),
            );
        }
        let content = span(&children, start);
        Self::block(
            range,
            content,
            markers.into_iter().collect(),
            BlockKind::BlockQuote,
            children,
            Vec::new(),
        )
    }

    /// `[!NOTE] Title` as the first line of a block quote's first paragraph.
    fn detect_callout(
        &mut self,
        pid: usize,
    ) -> Option<(CalloutKind, TextRange, Option<TextRange>)> {
        if !matches!(self.nodes[pid].kind, NodeKind::Paragraph) || self.nodes[pid].removed {
            return None;
        }
        // A paragraph that is not removed always has its first line.
        let line = self.nodes[pid].lines[0];
        let text = line.range.get(self.src).unwrap_or("");
        let rest = text.strip_prefix("[!")?;
        let close = rest.find(']')?;
        let kind = CalloutKind::from_keyword(&rest[..close])?;
        let tag = TextRange::new(line.range.start, line.range.start + 2 + close + 1);
        let after = &text[2 + close + 1..];
        let lead = after.len() - after.trim_start_matches([' ', '\t']).len();
        let title_text = after.trim_matches([' ', '\t']);
        let title = (!title_text.is_empty())
            .then(|| TextRange::new(tag.end + lead, tag.end + lead + title_text.len()));
        if !after.is_empty() && lead == 0 {
            return None;
        }
        self.nodes[pid].lines.remove(0);
        if self.nodes[pid].lines.is_empty() {
            self.nodes[pid].removed = true;
        } else {
            let first = self.nodes[pid].lines[0].range.start;
            self.nodes[pid].start = first;
        }
        Some((kind, tag, title))
    }

    fn convert_table(
        &mut self,
        id: usize,
        alignments: Vec<crate::markdown::ast::Alignment>,
        markers: Markers,
    ) -> Block {
        let lines = self.nodes[id].lines.clone();
        let start = lines[0].range.start;
        let end = lines.last().map_or(start, |l| l.range.end);
        let range = TextRange::new(start, end);
        let delimiter = lines[1].range;
        let header = self.convert_row(lines[0].range, range);
        let rows = lines[2..]
            .iter()
            .map(|l| self.convert_row(block::trim_range(self.src, l.range), range))
            .collect();
        Self::block(
            range,
            range,
            markers,
            BlockKind::Table(Table {
                alignments,
                header,
                delimiter,
                rows,
            }),
            Vec::new(),
            Vec::new(),
        )
    }

    fn convert_row(&mut self, row: TextRange, owner: TextRange) -> TableRow {
        let text = row.get(self.src).unwrap_or("");
        let split = block::split_row(text, row.start);
        let mut cells = Vec::with_capacity(split.cells.len());
        for (padded, content) in split.cells {
            let inlines = if content.is_empty() {
                Vec::new()
            } else {
                let source = Source::from_lines(self.src, &[content]);
                inline::parse_inlines(&mut self.defs, owner, &source, true)
            };
            cells.push(TableCell {
                range: padded,
                content,
                inlines,
            });
        }
        TableRow {
            range: row,
            pipes: split.pipes,
            cells,
        }
    }
}
