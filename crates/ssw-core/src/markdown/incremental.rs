//! Incremental Markdown parsing: [`IncrementalParser`] keeps a [`Document`] up to date across
//! edits and always equals `parse_document(text)` for the current text.
//!
//! # Algorithm
//!
//! The reference block pass is a left-to-right line parser whose state at a *clean boundary*
//! (a blank line after a block that a blank line closes) is the same as the state of a fresh
//! parser. An edit is therefore handled as follows:
//!
//! 1. **Start.** The window starts at the line after the nearest blank line before the edit that
//!    lies between two top-level blocks of the old tree and follows a *sealed* block. Lists,
//!    footnotes, indented code and unterminated verbatim blocks stay open across blank lines and
//!    can swallow or merge with later text, so they are never sealed. The text before the window
//!    is untouched, so the old blocks before it stay valid.
//! 2. **End.** The window ends after the next blank line that starts behind the edit and again
//!    lies between two old top-level blocks (so the old text has a blank line there too),
//!    provided the *new* parse of the window ends with a sealed block.
//!    Otherwise the window grows (doubling) until the new and old block structure
//!    re-synchronize or the end of the document is reached. Everything after the window is
//!    byte-identical text in an equivalent parser state, so those blocks are reused with their
//!    ranges shifted by the length delta.
//! 3. **Window parse.** The window text is parsed with [`parse_document`]. A leading blank line
//!    keeps front matter from being recognized away from the document start. Link reference and
//!    footnote definitions that live outside the window are supplied as synthetic definition
//!    blocks in front of the window text so references inside the window
//!    resolve exactly as they do in the whole document. Each synthetic block is closed by the
//!    blank line behind it (a footnote has no content, because one with content would stay open
//!    and take indented lines of the window for continuations), so the window itself is parsed
//!    in the state of a fresh parser.
//! 4. **Front matter** is the only construct that looks ahead across blank lines. When its
//!    presence or extent may differ between old and new text the window starts at offset 0 and
//!    covers the whole front matter.
//! 5. **Definitions.** Every top-level block keeps the definitions it contains and the labels it
//!    looked up (offsets relative to the block). The document-wide
//!    [`Definitions`] are rebuilt from them; blocks outside the window that looked up a label
//!    whose resolution changed are parsed again the same way.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::edit::Edit;
use crate::text::TextRange;

use super::ast::{
    Block, BlockKind, Document, ImageKind, Inline, InlineKind, LinkKind, TableRow, VerbatimLine,
};
use super::definitions::{
    Definitions, LabelKey, LabelKind, LabelUse, LinkDefinition, normalize_label,
};
use super::parse_document;

/// What [`IncrementalParser::apply`] re-parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reparse {
    /// Range in the new text covered by the re-parsed top-level blocks (empty, at the edit, when
    /// the edit removed blocks without producing new ones).
    pub changed: TextRange,
    /// Indices (in the tree before the edit) of the top-level blocks that were replaced.
    pub blocks_replaced: Range<usize>,
    /// Number of top-level blocks that took their place (starting at `blocks_replaced.start`).
    pub blocks_inserted: usize,
    /// Indices (in the tree after the edit) of other top-level blocks whose inline content was
    /// parsed again because a link reference or footnote definition they look up changed.
    pub refreshed: Vec<usize>,
}

/// A [`Document`] that follows text edits without re-parsing the whole text.
#[derive(Clone, Debug)]
pub struct IncrementalParser {
    doc: Document,
    /// One entry per top-level block of `doc`.
    infos: Vec<BlockInfo>,
    /// Every definition of the document by label.
    index: DefIndex,
}

impl IncrementalParser {
    /// Parses `text` completely.
    pub fn new(text: &str) -> Self {
        let doc = parse_document(text);
        let infos = collect_infos(&doc.blocks, doc.definitions.uses());
        let index = DefIndex::build(&infos);
        Self { doc, infos, index }
    }

    /// The current tree; equal to `parse_document` of the current text.
    pub fn document(&self) -> &Document {
        &self.doc
    }

    /// Brings the tree in line with `new_text`, the text after applying `edits` (sorted,
    /// disjoint, in coordinates of the previous text) to the previous text.
    ///
    /// Edits that do not describe the length change between the previous and the new text, or
    /// that reach past the previous text, make the parser start over with a full parse.
    pub fn apply(&mut self, new_text: &str, edits: &[Edit]) -> Reparse {
        let old_len = self.doc.len;
        let new_len = new_text.len();
        if edits.is_empty() && new_len == old_len {
            return Reparse {
                changed: TextRange::at(0),
                blocks_replaced: 0..0,
                blocks_inserted: 0,
                refreshed: Vec::new(),
            };
        }
        let Some((start, old_end)) = edit_span(edits, old_len, new_len) else {
            return self.rebuild(new_text);
        };
        let delta = new_len as isize - old_len as isize;
        let new_end = old_end.saturating_add_signed(delta);

        let front_old = self
            .doc
            .blocks
            .first()
            .filter(|block| matches!(block.kind, BlockKind::FrontMatter { .. }))
            .map(|block| block.range.end);
        let front_new = front_matter_end(new_text);
        let front_touched = match (front_old, front_new) {
            (None, None) => false,
            (Some(old), Some(new)) => old != new || start <= line_end(new_text, new),
            _ => true,
        };
        let (ws, first) = if front_touched {
            (0, 0)
        } else {
            self.window_start(new_text, start)
        };
        let mut from = new_end.max(if front_touched {
            front_new.unwrap_or(0)
        } else {
            0
        });
        let (past, region) = loop {
            let (end, past) = self.window_end(new_text, from, delta);
            let outside = Outside {
                index: &self.index,
                replaced: first..past,
            };
            let region = parse_region(new_text, ws, end, &outside, end != new_len);
            if region.sealed || end == new_len {
                break (past, region);
            }
            from = end + (end - ws);
        };

        let changed = match (region.blocks.first(), region.blocks.last()) {
            (Some(head), Some(tail)) => TextRange::new(head.range.start, tail.range.end),
            _ => TextRange::at(start),
        };
        let inserted = region.blocks.len();
        replace_range(&mut self.doc.blocks, first..past, region.blocks);
        replace_range(&mut self.infos, first..past, region.infos);
        if delta != 0 {
            for block in &mut self.doc.blocks[first + inserted..] {
                shift_block(block, delta);
            }
        }
        self.doc.len = new_len;
        self.index = DefIndex::build(&self.infos);
        let old_defs = std::mem::take(&mut self.doc.definitions);
        let mut defs = build_definitions(&self.doc.blocks, &self.infos);
        let changed_labels = old_defs.changed_labels(&defs);
        let mut refreshed = Vec::new();
        if !changed_labels.is_empty() {
            let window = first..first + inserted;
            let mut covered = 0usize;
            for range in defs.blocks_affected_by(&changed_labels) {
                // Lookups are recorded by `parse_document`, so some block always contains one.
                let last = self.doc.blocks.len().saturating_sub(1);
                let idx = self
                    .doc
                    .blocks
                    .partition_point(|block| block.range.end < range.end)
                    .min(last);
                if window.contains(&idx) || idx < covered {
                    continue;
                }
                let done = self.refresh(new_text, idx);
                covered = done.end;
                refreshed.extend(done);
            }
            defs = build_definitions(&self.doc.blocks, &self.infos);
        }
        self.doc.definitions = defs;
        refreshed.sort_unstable();
        refreshed.dedup();
        Reparse {
            changed,
            blocks_replaced: first..past,
            blocks_inserted: inserted,
            refreshed,
        }
    }

    /// Replaces everything by a full parse of `new_text`.
    fn rebuild(&mut self, new_text: &str) -> Reparse {
        let replaced = 0..self.doc.blocks.len();
        *self = Self::new(new_text);
        Reparse {
            changed: TextRange::new(0, new_text.len()),
            blocks_replaced: replaced,
            blocks_inserted: self.doc.blocks.len(),
            refreshed: Vec::new(),
        }
    }

    /// Start (a line start) of the window for an edit beginning at `start`, and the number of
    /// old top-level blocks before it.
    fn window_start(&self, text: &str, start: usize) -> (usize, usize) {
        let bytes = text.as_bytes();
        let mut line = line_start(bytes, start);
        while line > 0 {
            let prev = line_start(bytes, line - 1);
            if is_blank(&bytes[prev..line - 1]) {
                let before = self.doc.blocks.partition_point(|b| b.range.start < line);
                let clean = before
                    .checked_sub(1)
                    .map(|p| &self.doc.blocks[p])
                    .is_none_or(|b| b.range.end <= prev && is_sealed(b));
                if clean {
                    return (line, before);
                }
            }
            line = prev;
        }
        (0, 0)
    }

    /// End (a line start, or the text length) of the window and the number of old top-level
    /// blocks inside the window: the first blank line that starts strictly behind `from` and
    /// lies between two old top-level blocks. `delta` is the length change of the edit.
    ///
    /// The blank line must start strictly behind the end of the edit. A line that starts exactly
    /// there may only be the tail of an old line (an edit ending in a newline inside a line), so
    /// the old text has no blank line at that place and no old block boundary proves anything:
    /// `[foo]: /url` followed by `    text` is a definition and a paragraph that continues it,
    /// and the paragraph is indented code once a blank line is typed in between.
    fn window_end(&self, text: &str, from: usize, delta: isize) -> (usize, usize) {
        let bytes = text.as_bytes();
        let len = bytes.len();
        let mut line = line_start_after(bytes, from);
        while line < len {
            let Some(nl) = bytes[line..].iter().position(|&c| c == b'\n') else {
                break;
            };
            let next = line + nl + 1;
            if is_blank(&bytes[line..next - 1]) {
                let old_line = line.saturating_add_signed(-delta);
                let old_next = next.saturating_add_signed(-delta);
                let inside = self
                    .doc
                    .blocks
                    .partition_point(|b| b.range.start < old_next);
                let gap = inside
                    .checked_sub(1)
                    .is_none_or(|p| self.doc.blocks[p].range.end <= old_line);
                if gap {
                    return (next, inside);
                }
            }
            line = next;
        }
        (len, self.doc.blocks.len())
    }

    /// Parses the top-level block `idx` (and the blocks needed to reach a clean start) again
    /// with the current definitions. Returns the indices of the blocks that were parsed.
    fn refresh(&mut self, text: &str, idx: usize) -> Range<usize> {
        let range = self.doc.blocks[idx].range;
        let (ws, first) = self.window_start(text, range.start);
        let we = line_end(text, range.end).saturating_add(1).min(text.len());
        let outside = Outside {
            index: &self.index,
            replaced: first..idx + 1,
        };
        let region = parse_region(text, ws, we, &outside, false);
        let count = region.blocks.len();
        replace_range(&mut self.doc.blocks, first..idx + 1, region.blocks);
        replace_range(&mut self.infos, first..idx + 1, region.infos);
        first..first + count
    }
}

/// Outer edit range `(start, end)` in the old text, or `None` when `edits` cannot describe the
/// change from a text of `old_len` bytes to one of `new_len` bytes.
fn edit_span(edits: &[Edit], old_len: usize, new_len: usize) -> Option<(usize, usize)> {
    let start = edits.iter().map(|e| e.range.start).min()?;
    let end = edits.iter().map(|e| e.range.end).max()?;
    (end <= old_len && end + new_len >= start + old_len).then_some((start, end))
}

fn replace_range<T>(items: &mut Vec<T>, range: Range<usize>, with: Vec<T>) {
    items.splice(range, with).for_each(drop);
}

// ------------------------------------------------------------------------------ text helpers

/// Start of the line containing `pos`.
fn line_start(bytes: &[u8], pos: usize) -> usize {
    bytes[..pos]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1)
}

/// Smallest line start `> pos` (the text length when there is none).
fn line_start_after(bytes: &[u8], pos: usize) -> usize {
    let pos = pos.min(bytes.len());
    bytes[pos..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(bytes.len(), |i| pos + i + 1)
}

/// Offset of the `\n` ending the line containing `pos` (the text length for the last line).
fn line_end(text: &str, pos: usize) -> usize {
    text.as_bytes()[pos..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(text.len(), |i| pos + i)
}

/// Whether a line (without its `\n`) is blank: only spaces, tabs and one `\r` before the `\n`.
fn is_blank(line: &[u8]) -> bool {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    line.iter().all(|&c| c == b' ' || c == b'\t')
}

/// End offset of the front matter block of `text` (the end of its closing fence), found the way
/// the reference parser finds it.
fn front_matter_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let first_end = bytes
        .iter()
        .position(|&c| c == b'\n')
        .unwrap_or(bytes.len());
    let first = trim_line(&text[..first_end]);
    let (fence, yaml) = match first {
        "---" => ("---", true),
        "+++" => ("+++", false),
        _ => return None,
    };
    if first_end >= bytes.len() {
        return None;
    }
    let mut pos = first_end + 1;
    loop {
        let end = line_end(text, pos);
        let line = trim_line(&text[pos..end]);
        if line == fence || (yaml && line == "...") {
            return Some(pos + line.len());
        }
        if end >= bytes.len() {
            return None;
        }
        pos = end + 1;
    }
}

fn trim_line(line: &str) -> &str {
    line.strip_suffix('\r')
        .unwrap_or(line)
        .trim_end_matches([' ', '\t'])
}

/// Whether a blank line directly behind `block` closes it for good, so that the parser state
/// after the blank line is the state of a fresh parser.
fn is_sealed(block: &Block) -> bool {
    match &block.kind {
        BlockKind::List { .. } | BlockKind::FootnoteDefinition { .. } => false,
        BlockKind::CodeBlock(code) => code.fence.is_some_and(|fence| fence.close.is_some()),
        BlockKind::MathBlock(math) => math.closed,
        BlockKind::HtmlBlock(html) => html.kind >= 6,
        _ => true,
    }
}

// ------------------------------------------------------------------------------- definitions

/// A label lookup of a top-level block; offsets are relative to the start of the block.
#[derive(Clone, Debug)]
struct InfoUse {
    kind: LabelKind,
    label: String,
    range: TextRange,
}

/// A footnote definition inside a top-level block; offsets are relative to the block start.
#[derive(Clone, Debug)]
struct InfoNote {
    label: String,
    range: TextRange,
}

/// The definitions a top-level block contains and the labels it looked up, so the document-wide
/// tables can be rebuilt without touching the other blocks.
#[derive(Clone, Debug, Default)]
struct BlockInfo {
    /// Link definitions in source order, duplicates included.
    links: Vec<LinkDefinition>,
    /// Footnote definitions in source order, duplicates included.
    notes: Vec<InfoNote>,
    /// Lookups in source order.
    uses: Vec<InfoUse>,
}

impl BlockInfo {
    /// Definitions inside `top`; lookups are added by [`collect_infos`].
    fn of(top: &Block) -> Self {
        let mut info = BlockInfo::default();
        for block in top.descendants() {
            match &block.kind {
                BlockKind::LinkReferenceDefinition(def) => info.links.push(LinkDefinition {
                    label: normalize_label(&def.label),
                    dest: def.dest.clone(),
                    title: def.title.clone(),
                }),
                BlockKind::FootnoteDefinition { label } => {
                    let range = footnote_range(block);
                    info.notes.push(InfoNote {
                        label: normalize_label(label),
                        range: TextRange::new(
                            range.start.saturating_sub(top.range.start),
                            range.end.saturating_sub(top.range.start),
                        ),
                    });
                }
                _ => {}
            }
        }
        info
    }

    /// Keys of the definitions this block contains.
    fn defined(&self) -> impl Iterator<Item = LabelKey> + '_ {
        let links = self
            .links
            .iter()
            .map(|d| (LabelKind::Link, d.label.clone()));
        let notes = self
            .notes
            .iter()
            .map(|n| (LabelKind::Footnote, n.label.clone()));
        links.chain(notes)
    }
}

/// Range the reference parser records for a footnote definition: from its start to the end of
/// the marker or of the last child, whichever is later.
fn footnote_range(block: &Block) -> TextRange {
    let marker_end = block.markers.last().map_or(block.range.start, |m| m.end);
    let end = block
        .children
        .iter()
        .map(raw_end)
        .fold(marker_end, usize::max);
    TextRange::new(block.range.start, end)
}

/// End of `block` as the block pass sees it. It differs from `block.range.end` only for
/// containers: the task checkbox is added to the range afterwards (a callout's range already
/// covers its tag line, see the callout arm).
fn raw_end(block: &Block) -> usize {
    let own = |end: usize| block.children.iter().map(raw_end).fold(end, usize::max);
    match &block.kind {
        BlockKind::List { .. } => own(block.range.start),
        BlockKind::ListItem { marker, .. } => own(block.range.start.max(marker.end)),
        BlockKind::BlockQuote | BlockKind::FootnoteDefinition { .. } => {
            let markers = block.markers.iter().map(|m| m.end);
            own(markers.fold(block.range.start, usize::max))
        }
        BlockKind::Callout { tag, title, .. } => {
            // The tag line is the first line of a paragraph in the block pass: it reaches to the
            // end of the title (or the tag) when no further line of that paragraph (a child
            // now) reaches further.
            let tag_line = title.map_or(tag.end, |title| title.end);
            let markers = block.markers.iter().map(|m| m.end);
            own(markers.fold(block.range.start.max(tag_line), usize::max))
        }
        _ => block.range.end,
    }
}

/// Per-block infos for `blocks`; `uses` (document order, coordinates of `blocks`) are handed to
/// the block that contains them.
fn collect_infos(blocks: &[Block], uses: &[LabelUse]) -> Vec<BlockInfo> {
    let mut infos: Vec<BlockInfo> = blocks.iter().map(BlockInfo::of).collect();
    for used in uses {
        let at = blocks.partition_point(|b| b.range.end < used.block.end);
        if let (Some(block), Some(info)) = (blocks.get(at), infos.get_mut(at)) {
            let origin = block.range.start;
            info.uses.push(InfoUse {
                kind: used.kind,
                label: used.label.clone(),
                range: TextRange::new(
                    used.block.start.saturating_sub(origin),
                    used.block.end.saturating_sub(origin),
                ),
            });
        }
    }
    infos
}

fn absolute(origin: usize, range: TextRange) -> TextRange {
    TextRange::new(origin + range.start, origin + range.end)
}

/// The document-wide tables for `blocks`, in the order the reference parser fills them.
fn build_definitions(blocks: &[Block], infos: &[BlockInfo]) -> Definitions {
    let mut defs = Definitions::new();
    for (block, info) in blocks.iter().zip(infos) {
        let origin = block.range.start;
        for link in &info.links {
            defs.define_link(link.clone());
        }
        for note in &info.notes {
            defs.define_footnote(&note.label, absolute(origin, note.range));
        }
    }
    for (block, info) in blocks.iter().zip(infos) {
        let origin = block.range.start;
        for used in &info.uses {
            defs.record_use(absolute(origin, used.range), used.kind, used.label.clone());
        }
    }
    defs
}

/// What a definition looks like, enough to write it down again.
#[derive(Clone, Debug)]
enum Def {
    Link(LinkDefinition),
    Note,
}

impl Def {
    /// Source of a definition block for the normalized `label` that resolves like the original.
    fn source(&self, label: &str) -> String {
        match self {
            // The spaces keep a label that starts with `^` from reading as a footnote.
            Def::Link(link) => {
                let mut out = format!("[ {label} ]: <{}>", escape(&link.dest, &['<', '>']));
                if let Some(title) = &link.title {
                    out.push_str(" \"");
                    out.push_str(&escape(title, &['"']));
                    out.push('"');
                }
                out
            }
            // No content: a footnote that already has content stays open across blank lines and
            // would take an indented first line of the window for a continuation (and swallow
            // the lines after it lazily). An empty one is closed by the blank line behind it.
            Def::Note => format!("[^{label}]:"),
        }
    }
}

/// Escapes `text` so that resolving backslash escapes and entities yields `text` again.
fn escape(text: &str, specials: &[char]) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '&' => out.push_str("&amp;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            c if specials.contains(&c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// Every definition of the document by label; entries are ordered by top-level block index.
#[derive(Clone, Debug, Default)]
struct DefIndex(HashMap<LabelKey, Vec<(usize, Def)>>);

impl DefIndex {
    fn build(infos: &[BlockInfo]) -> Self {
        let mut map: HashMap<LabelKey, Vec<(usize, Def)>> = HashMap::new();
        for (idx, info) in infos.iter().enumerate() {
            for link in &info.links {
                let key = (LabelKind::Link, link.label.clone());
                map.entry(key)
                    .or_default()
                    .push((idx, Def::Link(link.clone())));
            }
            for note in &info.notes {
                let key = (LabelKind::Footnote, note.label.clone());
                map.entry(key).or_default().push((idx, Def::Note));
            }
        }
        Self(map)
    }
}

/// The definitions outside a window of top-level blocks.
struct Outside<'a> {
    index: &'a DefIndex,
    /// Top-level block indices (in the tree `index` was built from) the window replaces.
    replaced: Range<usize>,
}

impl Outside<'_> {
    /// Source of the definition that has to precede the window so `key` resolves inside it as
    /// it does in the whole document, if that definition lies outside the window. The first
    /// definition of a label wins: one before the window beats the window's own, one behind the
    /// window only matters when the window defines nothing under that label.
    fn external(&self, key: &LabelKey, window_defs: &HashSet<LabelKey>) -> Option<String> {
        let entries = self.index.0.get(key)?;
        let pick = match entries.first() {
            Some((block, def)) if *block < self.replaced.start => Some(def),
            _ if window_defs.contains(key) => None,
            _ => entries
                .iter()
                .find(|(block, _)| *block >= self.replaced.end)
                .map(|(_, def)| def),
        };
        pick.map(|def| def.source(&key.1))
    }
}

// -------------------------------------------------------------------------------- window parse

/// Result of parsing a window of the text.
struct Region {
    /// Top-level blocks with ranges in the coordinates of the whole text.
    blocks: Vec<Block>,
    infos: Vec<BlockInfo>,
    /// The last block is sealed (or there is none), see [`is_sealed`].
    sealed: bool,
}

/// Parses `text[ws..we]` as the middle part of the document. With `reject_unsealed` the caller
/// discards a region that is not sealed, so resolving outside definitions is skipped for it.
fn parse_region(
    text: &str,
    ws: usize,
    we: usize,
    outside: &Outside<'_>,
    reject_unsealed: bool,
) -> Region {
    let window = &text[ws..we];
    let mut externals: Vec<(LabelKey, String)> = Vec::new();
    loop {
        let (prefix, assembled) = assemble(ws, window, &externals);
        let mut doc = parse_document(&assembled);
        let limit = prefix + window.len();
        let head = doc.blocks.partition_point(|b| b.range.start < prefix);
        let tail = doc.blocks.partition_point(|b| b.range.start < limit);
        let infos = collect_infos(&doc.blocks[head..tail], doc.definitions.uses());
        let sealed = doc.blocks[head..tail].last().is_none_or(is_sealed);

        let mut needed: Vec<(LabelKey, String)> = Vec::new();
        if sealed || !reject_unsealed {
            let window_defs: HashSet<LabelKey> =
                infos.iter().flat_map(BlockInfo::defined).collect();
            let mut seen: HashSet<LabelKey> = HashSet::new();
            for used in doc.definitions.uses() {
                let key = (used.kind, used.label.clone());
                if externals.iter().any(|(known, _)| *known == key) || !seen.insert(key.clone()) {
                    continue;
                }
                if let Some(source) = outside.external(&key, &window_defs) {
                    needed.push((key, source));
                }
            }
        }
        if needed.is_empty() {
            let mut blocks: Vec<Block> = doc.blocks.drain(head..tail).collect();
            let shift = ws as isize - prefix as isize;
            if shift != 0 {
                for block in &mut blocks {
                    shift_block(block, shift);
                }
            }
            return Region {
                blocks,
                infos,
                sealed,
            };
        }
        externals.extend(needed);
    }
}

/// Text to parse for a window: the length of everything before the window text, and the text.
///
/// Away from the document start a blank line comes first so the window cannot be taken for
/// front matter, followed by the outside definitions (all sealed by blank lines). At the
/// document start the window must stay first; outside definitions can then only come from
/// behind the window, which ends in a blank line, so they are appended.
fn assemble(ws: usize, window: &str, externals: &[(LabelKey, String)]) -> (usize, String) {
    let mut defs = String::new();
    for (_, source) in externals {
        defs.push_str(source);
        defs.push_str("\n\n");
    }
    if ws == 0 {
        let mut text = String::with_capacity(window.len() + defs.len() + 1);
        text.push_str(window);
        if !defs.is_empty() {
            text.push('\n');
            text.push_str(&defs);
        }
        (0, text)
    } else {
        let mut text = String::with_capacity(window.len() + defs.len() + 1);
        text.push('\n');
        text.push_str(&defs);
        let prefix = text.len();
        text.push_str(window);
        (prefix, text)
    }
}

// ----------------------------------------------------------------------------------- shifting

fn sh(range: &mut TextRange, delta: isize) {
    range.start = range.start.saturating_add_signed(delta);
    range.end = range.end.saturating_add_signed(delta);
}

fn sh_opt(range: &mut Option<TextRange>, delta: isize) {
    if let Some(range) = range {
        sh(range, delta);
    }
}

fn sh_lines(lines: &mut [VerbatimLine], delta: isize) {
    for line in lines {
        sh(&mut line.range, delta);
    }
}

/// Moves `block` and everything inside it by `delta` bytes.
fn shift_block(block: &mut Block, delta: isize) {
    sh(&mut block.range, delta);
    sh(&mut block.content, delta);
    for marker in &mut block.markers {
        sh(marker, delta);
    }
    match &mut block.kind {
        BlockKind::Heading { marker, .. } => sh(marker, delta),
        BlockKind::ListItem { marker, task } => {
            sh(marker, delta);
            if let Some(task) = task {
                sh(&mut task.checkbox, delta);
            }
        }
        BlockKind::Callout { tag, title, .. } => {
            sh(tag, delta);
            sh_opt(title, delta);
        }
        BlockKind::CodeBlock(code) => {
            if let Some(fence) = &mut code.fence {
                sh(&mut fence.open, delta);
                sh_opt(&mut fence.close, delta);
            }
            sh_opt(&mut code.info, delta);
            sh_lines(&mut code.lines, delta);
        }
        BlockKind::MathBlock(math) => sh_lines(&mut math.lines, delta),
        BlockKind::HtmlBlock(html) => sh_lines(&mut html.lines, delta),
        BlockKind::Table(table) => {
            shift_row(&mut table.header, delta);
            sh(&mut table.delimiter, delta);
            for row in &mut table.rows {
                shift_row(row, delta);
            }
        }
        BlockKind::LinkReferenceDefinition(def) => {
            sh(&mut def.label_range, delta);
            sh(&mut def.dest_range, delta);
            sh_opt(&mut def.title_range, delta);
        }
        BlockKind::Paragraph
        | BlockKind::List { .. }
        | BlockKind::BlockQuote
        | BlockKind::ThematicBreak
        | BlockKind::FrontMatter { .. }
        | BlockKind::FootnoteDefinition { .. }
        | BlockKind::PageBreak => {}
    }
    for child in &mut block.children {
        shift_block(child, delta);
    }
    shift_inlines(&mut block.inlines, delta);
}

fn shift_row(row: &mut TableRow, delta: isize) {
    sh(&mut row.range, delta);
    for pipe in &mut row.pipes {
        sh(pipe, delta);
    }
    for cell in &mut row.cells {
        sh(&mut cell.range, delta);
        sh(&mut cell.content, delta);
        shift_inlines(&mut cell.inlines, delta);
    }
}

fn shift_inlines(inlines: &mut [Inline], delta: isize) {
    for inline in inlines {
        shift_inline(inline, delta);
    }
}

fn shift_inline(inline: &mut Inline, delta: isize) {
    sh(&mut inline.range, delta);
    sh(&mut inline.content, delta);
    for marker in &mut inline.markers {
        sh(marker, delta);
    }
    match &mut inline.kind {
        InlineKind::Link(link) => match &mut link.kind {
            LinkKind::Inline {
                dest_range,
                title_range,
            } => {
                sh(dest_range, delta);
                sh_opt(title_range, delta);
            }
            LinkKind::Reference { label_range, .. } => sh(label_range, delta),
            LinkKind::Autolink { .. } => {}
        },
        InlineKind::Image(image) => {
            sh(&mut image.alt, delta);
            match &mut image.kind {
                ImageKind::Inline {
                    dest_range,
                    title_range,
                } => {
                    sh(dest_range, delta);
                    sh_opt(title_range, delta);
                }
                ImageKind::Reference { label_range, .. } => sh(label_range, delta),
                ImageKind::Html => {}
            }
        }
        InlineKind::WikiLink(wiki) => {
            sh(&mut wiki.target_range, delta);
            sh_opt(&mut wiki.heading_range, delta);
            sh_opt(&mut wiki.alias_range, delta);
        }
        InlineKind::Text
        | InlineKind::Escape
        | InlineKind::Entity { .. }
        | InlineKind::Emphasis
        | InlineKind::Strong
        | InlineKind::Underline
        | InlineKind::Strikethrough
        | InlineKind::Highlight { .. }
        | InlineKind::Code { .. }
        | InlineKind::FootnoteRef { .. }
        | InlineKind::InlineMath
        | InlineKind::HardBreak
        | InlineKind::SoftBreak
        | InlineKind::RawHtml => {}
    }
    shift_inlines(&mut inline.children, delta);
}
