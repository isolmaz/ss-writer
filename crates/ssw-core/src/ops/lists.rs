//! List structure: renumbering, list type conversion, indent/outdent and marker removal.

use crate::edit::{Assoc, Edit, Selection, Transaction};
use crate::markdown::ast::{self, Block, BlockKind};
use crate::markdown::parse_document;
use crate::settings::ListIndent;
use crate::text::TextRange;

use super::blocks::line_is_protected;
use super::{Ctx, list_marker_len, task_box_len};

/// A list item with the list and item it sits in.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ItemRef<'a> {
    pub item: &'a Block,
    pub list: &'a Block,
    pub parent: Option<&'a Block>,
    /// Position among the list's items.
    pub index: usize,
}

impl ItemRef<'_> {
    /// The bullet or `1.` marker.
    pub fn marker(&self) -> TextRange {
        self.item.markers[0]
    }

    pub fn checkbox(&self) -> Option<ast::Task> {
        self.item.task()
    }
}

/// Every list item of the document in source order (parents before their children).
pub(crate) fn all_items(ast: &ast::Document) -> Vec<ItemRef<'_>> {
    fn walk<'a>(blocks: &'a [Block], parent: Option<&'a Block>, out: &mut Vec<ItemRef<'a>>) {
        for block in blocks {
            if matches!(block.kind, BlockKind::List { .. }) {
                for (index, item) in block.children.iter().enumerate() {
                    out.push(ItemRef {
                        item,
                        list: block,
                        parent,
                        index,
                    });
                    walk(&item.children, Some(item), out);
                }
            } else {
                walk(&block.children, parent, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&ast.blocks, None, &mut out);
    out
}

/// The innermost list item whose range touches `offset`.
pub(crate) fn item_at(ast: &ast::Document, offset: usize) -> Option<ItemRef<'_>> {
    let path: Vec<&Block> = ast
        .path_at(offset)
        .into_iter()
        .filter_map(ast::NodeRef::as_block)
        .collect();
    let pos = path
        .iter()
        .rposition(|b| matches!(b.kind, BlockKind::ListItem { .. }))?;
    let item = path[pos];
    // A list item is always a child of its list, which precedes it in the path.
    let list = path[pos - 1];
    let parent = path[..pos - 1]
        .iter()
        .rev()
        .find(|b| matches!(b.kind, BlockKind::ListItem { .. }))
        .copied();
    let index = list
        .children
        .iter()
        .position(|c| std::ptr::eq(c, item))
        .expect("an item is one of its list's children");
    Some(ItemRef {
        item,
        list,
        parent,
        index,
    })
}

/// Like [`item_at`], also for a caret behind the trailing blanks of an item's last line
/// (`- ‸`), which lie outside the item's range.
pub(crate) fn item_around<'a>(ctx: Ctx<'a>, offset: usize) -> Option<ItemRef<'a>> {
    item_at(ctx.ast, offset).or_else(|| {
        let line = ctx.line_range(ctx.line_of(offset));
        let end = line.start + ctx.slice(line).trim_end().len();
        if end < offset {
            item_at(ctx.ast, end)
        } else {
            None
        }
    })
}

/// Offset of the item's text: after the marker, the task box and the spaces behind them.
pub(crate) fn item_content_start(ctx: Ctx<'_>, item: &ItemRef<'_>) -> usize {
    let after = item
        .checkbox()
        .map_or(item.marker().end, |t| t.checkbox.end);
    let line_end = ctx.line_range(ctx.line_of(after)).end;
    let rest = ctx.slice(TextRange::new(after, line_end));
    after + rest.len() - rest.trim_start_matches([' ', '\t']).len()
}

/// Byte length of the quote prefix (`> `, `> > `) of `line`; indentation behind it is not
/// counted.
pub(crate) fn quote_prefix_len(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut pos = 0;
    let mut probe = 0;
    loop {
        while matches!(bytes.get(probe), Some(b' ' | b'\t')) {
            probe += 1;
        }
        if bytes.get(probe) != Some(&b'>') {
            return pos;
        }
        probe += 1;
        if matches!(bytes.get(probe), Some(b' ' | b'\t')) {
            probe += 1;
        }
        pos = probe;
    }
}

/// Offset where list indentation of `line` starts: after its quote prefix.
fn indent_origin(ctx: Ctx<'_>, line: usize) -> usize {
    let range = ctx.line_range(line);
    range.start + quote_prefix_len(&ctx.slice(range))
}

/// Indentation characters in front of the item's marker (behind any quote prefix).
fn indent_chars(ctx: Ctx<'_>, item: &Block) -> usize {
    let marker = item.markers[0];
    marker.start - indent_origin(ctx, ctx.line_of(marker.start))
}

/// Column of the item's text relative to the quote prefix, in characters.
fn content_column(ctx: Ctx<'_>, item: &Block) -> usize {
    let marker = item.markers[0];
    let line = ctx.line_of(marker.start);
    let after = marker.end;
    let line_end = ctx.line_range(line).end;
    let rest = ctx.slice(TextRange::new(after, line_end));
    let spaces = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    let spacing = if spaces == 0 || spaces > 4 { 1 } else { spaces };
    after + spacing - indent_origin(ctx, line)
}

/// First and last line of the item.
fn item_lines(ctx: Ctx<'_>, item: &ItemRef<'_>) -> (usize, usize) {
    (
        ctx.line_of(item.item.range.start),
        ctx.line_of(item.item.range.end),
    )
}

/// Replaces the number of every ordered list item by its position (the first item keeps its
/// number). `text` is the text `ast` was parsed from.
pub(crate) fn renumber_edits(ast: &ast::Document, text: &str) -> Vec<Edit> {
    let mut edits = Vec::new();
    for block in ast.iter_blocks() {
        let BlockKind::List {
            ordered: true,
            start,
            ..
        } = block.kind
        else {
            continue;
        };
        for (i, item) in block.children.iter().enumerate() {
            let marker = item.markers[0];
            let digits = TextRange::new(marker.start, marker.end - 1);
            let want = (u64::from(start) + i as u64).to_string();
            if digits.get(text) != Some(want.as_str()) {
                edits.push(Edit::replace(digits, want));
            }
        }
    }
    edits.sort_by_key(|e| e.range.start);
    edits
}

/// Renumbers all ordered lists of the document; `None` when they are already consistent.
pub fn renumber_lists(ctx: Ctx<'_>) -> Option<Transaction> {
    let text = ctx.slice(TextRange::new(0, ctx.buffer.len()));
    let edits = renumber_edits(ctx.ast, &text);
    (!edits.is_empty()).then(|| ctx.transaction_mapped(edits))
}

/// The smallest range covering `edits` and the top-level blocks they touch, when one of those
/// blocks holds an ordered list.
fn renumber_window(ctx: Ctx<'_>, edits: &[Edit]) -> Option<TextRange> {
    let mut window: Option<TextRange> = None;
    let mut has_ordered = false;
    for edit in edits {
        let mut range = edit.range;
        let touched = ctx
            .ast
            .blocks
            .iter()
            .filter(|b| b.range.start <= edit.range.end && edit.range.start <= b.range.end);
        for block in touched {
            range = range.cover(block.range);
            has_ordered |= block
                .descendants()
                .any(|b| matches!(b.kind, BlockKind::List { ordered: true, .. }));
        }
        window = Some(window.map_or(range, |w| w.cover(range)));
    }
    if has_ordered { window } else { None }
}

/// Lengths of the common prefix and suffix of `a` and `b` (char boundaries in both, not
/// overlapping).
fn common_affixes(a: &str, b: &str) -> (usize, usize) {
    let mut prefix = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    while !(a.is_char_boundary(prefix) && b.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let max_suffix = a.len().min(b.len()) - prefix;
    let mut suffix = a
        .bytes()
        .rev()
        .zip(b.bytes().rev())
        .take(max_suffix)
        .take_while(|(x, y)| x == y)
        .count();
    while !(a.is_char_boundary(a.len() - suffix) && b.is_char_boundary(b.len() - suffix)) {
        suffix -= 1;
    }
    (prefix, suffix)
}

/// Builds the transaction for `edits`, renumbering the ordered lists they touch so the result
/// is one edit. `after` is the selection in the coordinates of the text after `edits`.
pub(crate) fn finish(ctx: Ctx<'_>, edits: Vec<Edit>, after: Selection) -> Transaction {
    let Some(window) = renumber_window(ctx, &edits) else {
        return ctx.transaction(edits, after);
    };
    let old = ctx.slice(window).into_owned();
    let mut text = old.clone();
    for edit in edits.iter().rev() {
        text.replace_range(
            edit.range.start - window.start..edit.range.end - window.start,
            &edit.text,
        );
    }
    let renumber = renumber_edits(&parse_document(&text), &text);
    let mut renumbered = text.clone();
    for edit in renumber.iter().rev() {
        renumbered.replace_range(edit.range.as_range(), &edit.text);
    }
    let shifted: Vec<Edit> = renumber
        .into_iter()
        .map(|e| {
            let range = TextRange::new(e.range.start + window.start, e.range.end + window.start);
            Edit::replace(range, e.text)
        })
        .collect();
    let phase_two = Transaction::new(shifted, after, after);
    let after = Selection::new(
        phase_two.map_offset(after.anchor, Assoc::After),
        phase_two.map_offset(after.head, Assoc::After),
    );
    if renumbered == old {
        return ctx.transaction(Vec::new(), after);
    }
    let (prefix, suffix) = common_affixes(&old, &renumbered);
    let edit = Edit::replace(
        TextRange::new(window.start + prefix, window.end - suffix),
        renumbered[prefix..renumbered.len() - suffix].to_owned(),
    );
    ctx.transaction(vec![edit], after)
}

/// Like [`finish`], with the selection after the edits being the current selection mapped
/// through them.
pub(crate) fn finish_mapped(ctx: Ctx<'_>, edits: Vec<Edit>) -> Transaction {
    let draft = Transaction::new(edits.clone(), ctx.selection, ctx.selection);
    let after = ctx.selection.map_through(&draft);
    finish(ctx, edits, after)
}

/// The kind of list [`toggle_list`] produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    Bullet,
    Numbered,
    Task,
}

/// Marker structure of one source line.
struct LineParts {
    /// Bytes of indentation and quote prefix in front of the marker.
    outer: usize,
    /// Bytes of the marker, when the line starts a list item.
    marker: Option<usize>,
    ordered: bool,
    number: u64,
    /// `Some(checked)` for a task box.
    task: Option<bool>,
    /// Byte offset of the task box in the line.
    task_start: Option<usize>,
    /// Bytes up to the item text (marker, task box and spaces included).
    prefix_end: usize,
    /// First character of the marker.
    marker_char: char,
}

fn parse_line(text: &str) -> LineParts {
    let bytes = text.as_bytes();
    let mut outer = 0;
    loop {
        while matches!(bytes.get(outer), Some(b' ' | b'\t')) {
            outer += 1;
        }
        if bytes.get(outer) != Some(&b'>') {
            break;
        }
        outer += 1;
        if matches!(bytes.get(outer), Some(b' ' | b'\t')) {
            outer += 1;
        }
    }
    let skip = |mut pos: usize| {
        while matches!(bytes.get(pos), Some(b' ' | b'\t')) {
            pos += 1;
        }
        pos
    };
    let Some(len) = list_marker_len(&text[outer..]) else {
        return LineParts {
            outer,
            marker: None,
            ordered: false,
            number: 0,
            task: None,
            task_start: None,
            prefix_end: outer,
            marker_char: '-',
        };
    };
    let marker_text = &text[outer..outer + len];
    let ordered = marker_text.as_bytes()[0].is_ascii_digit();
    let number = marker_text[..len - 1].parse().unwrap_or(0);
    let mut prefix_end = skip(outer + len);
    let mut task = None;
    let mut task_start = None;
    if let Some(box_len) = task_box_len(&text[prefix_end..]) {
        task = Some(bytes[prefix_end + 1] != b' ');
        task_start = Some(prefix_end);
        prefix_end = skip(prefix_end + box_len);
    }
    LineParts {
        outer,
        marker: Some(len),
        ordered,
        number: if ordered { number } else { 0 },
        task,
        task_start,
        prefix_end,
        marker_char: marker_text.chars().next().unwrap_or('-'),
    }
}

impl LineParts {
    fn is_kind(&self, kind: ListKind) -> bool {
        match kind {
            ListKind::Bullet => self.marker.is_some() && !self.ordered && self.task.is_none(),
            ListKind::Numbered => self.ordered,
            ListKind::Task => self.marker.is_some() && !self.ordered && self.task.is_some(),
        }
    }
}

/// A line that holds a list marker (and a task box) and no text: an empty list item as the user
/// means it. The parser reads such a line as an item, except right after the line of another
/// item, where it is paragraph text of that item (an empty item cannot interrupt a paragraph:
/// `- a` followed by `  - ` has no nested item). A bare line of that kind is a *phantom*.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BareItem {
    /// The line without its terminator.
    pub line: TextRange,
    /// Start of the marker, behind indentation and quote prefix.
    pub marker: usize,
    /// Start of the task box, when the line has one.
    pub task: Option<usize>,
    /// True when the parser does not see an item on the line.
    pub phantom: bool,
}

/// The bare item on the line holding `offset`. `None` for any other line, for lines of code,
/// math, HTML and tables, for a bare marker after a plain paragraph (that stays text), and for
/// an item that has content on later lines (`-` followed by an indented line).
pub(crate) fn bare_item(ctx: Ctx<'_>, offset: usize) -> Option<BareItem> {
    let line_index = ctx.line_of(offset);
    let line = ctx.line_range(line_index);
    let text = ctx.slice(line);
    let parts = parse_line(&text);
    if parts.marker.is_none()
        || parts.prefix_end != text.len()
        || line_is_protected(ctx, line_index)
    {
        return None;
    }
    let marker = line.start + parts.outer;
    let phantom = match all_items(ctx.ast)
        .into_iter()
        .find(|item| item.marker().start == marker)
    {
        Some(item) if ctx.line_of(item.item.range.end) == line_index => false,
        Some(_) => return None,
        None => {
            let follows_item = line_index
                .checked_sub(1)
                .is_some_and(|prev| item_at(ctx.ast, ctx.line_range(prev).end).is_some());
            if !follows_item {
                return None;
            }
            true
        }
    };
    Some(BareItem {
        line,
        marker,
        task: parts.task_start.map(|start| line.start + start),
        phantom,
    })
}

/// The phantom item on the only line the selection touches.
fn lone_phantom(ctx: Ctx<'_>) -> Option<BareItem> {
    let (first, last) = ctx.selected_lines();
    if first != last {
        return None;
    }
    bare_item(ctx, ctx.line_range(first).start).filter(|bare| bare.phantom)
}

/// Turns the selected lines into list items of `kind`, or back into plain lines when they all
/// already are of that kind. Ordered lists are renumbered. `None` inside code, math, HTML or
/// tables.
pub fn toggle_list(ctx: Ctx<'_>, kind: ListKind) -> Option<Transaction> {
    let (first, last) = ctx.selected_lines();
    let lines: Vec<usize> = (first..=last)
        .filter(|&l| (first == last || !ctx.is_blank_line(l)) && !line_is_protected(ctx, l))
        .collect();
    if lines.is_empty() {
        return None;
    }
    let parts: Vec<LineParts> = lines
        .iter()
        .map(|&l| parse_line(&ctx.line_text(l)))
        .collect();
    let remove = parts.iter().all(|p| p.is_kind(kind));
    let mut number = if parts[0].ordered { parts[0].number } else { 1 };
    let mut edits = Vec::new();
    for (&line, part) in lines.iter().zip(&parts) {
        let start = ctx.line_range(line).start;
        let range = TextRange::new(start + part.outer, start + part.prefix_end);
        if remove {
            edits.push(Edit::delete(range));
            continue;
        }
        let marker = match kind {
            ListKind::Numbered => {
                number += 1;
                format!("{}.", number - 1)
            }
            _ => part.marker_char.to_string(),
        };
        let marker = if kind != ListKind::Numbered && part.ordered {
            "-".to_owned()
        } else {
            marker
        };
        let task = match (kind, part.task) {
            (ListKind::Task, Some(true)) => "[x] ",
            (ListKind::Task, _) => "[ ] ",
            _ => "",
        };
        edits.push(Edit::replace(range, format!("{marker} {task}")));
    }
    Some(finish_mapped(ctx, edits))
}

/// Items the selection works on: items whose marker line is selected, or for a caret the
/// innermost item around it; nested ones that are inside another selected item are dropped.
fn selected_roots<'a>(ctx: Ctx<'a>, items: &[ItemRef<'a>]) -> Vec<ItemRef<'a>> {
    let (first, last) = ctx.selected_lines();
    let mut touched: Vec<ItemRef<'a>> = items
        .iter()
        .filter(|i| (first..=last).contains(&ctx.line_of(i.marker().start)))
        .copied()
        .collect();
    if touched.is_empty() {
        let around = items.iter().rev().find(|i| {
            let (a, b) = item_lines(ctx, i);
            a <= first && first <= b
        });
        touched.extend(around.copied());
    }
    let ranges: Vec<TextRange> = touched.iter().map(|i| i.item.range).collect();
    touched
        .into_iter()
        .filter(|i| {
            ranges
                .iter()
                .filter(|r| **r != i.item.range)
                .all(|r| !r.contains_range(i.item.range))
        })
        .collect()
}

/// Indents the selected list items (with their children) one level under the previous
/// sibling. The step is `setting`, but never less than what is needed to nest under the
/// sibling's text. Items without a previous sibling cannot be nested and are left alone.
pub fn indent_items(ctx: Ctx<'_>, setting: ListIndent) -> Option<Transaction> {
    // A phantom item sits under the item above it and has no sibling to nest under; the item
    // whose text it is must not move instead.
    if lone_phantom(ctx).is_some() {
        return None;
    }
    let items = all_items(ctx.ast);
    let mut edits = Vec::new();
    for root in selected_roots(ctx, &items) {
        let Some(previous) = root
            .index
            .checked_sub(1)
            .and_then(|i| root.list.children.get(i))
        else {
            continue;
        };
        let align = content_column(ctx, previous).saturating_sub(indent_chars(ctx, root.item));
        let indent = match setting {
            ListIndent::Align => " ".repeat(align.max(1)),
            ListIndent::Two => " ".repeat(align.max(2)),
            ListIndent::Four => " ".repeat(align.max(4)),
            ListIndent::Tab => "\t".to_owned(),
        };
        let (a, b) = item_lines(ctx, &root);
        for line in (a..=b).filter(|&l| !ctx.is_blank_line(l)) {
            edits.push(Edit::insert(indent_origin(ctx, line), indent.clone()));
        }
        // The first item of a new nested ordered list starts at 1.
        let joins_sublist = matches!(
            previous.children.last().map(|c| &c.kind),
            Some(BlockKind::List { ordered: true, .. })
        );
        let marker = root.marker();
        if matches!(root.list.kind, BlockKind::List { ordered: true, .. }) && !joins_sublist {
            edits.push(Edit::replace(
                TextRange::new(marker.start, marker.end - 1),
                "1",
            ));
        }
    }
    edits.sort_by_key(|e| (e.range.start, e.range.end));
    (!edits.is_empty()).then(|| finish_mapped(ctx, edits))
}

/// Moves the selected list items (with their children) out one level, to the level of the
/// item they sit in. Top-level items lose their indentation. `None` when nothing can move.
pub fn outdent_items(ctx: Ctx<'_>) -> Option<Transaction> {
    if let Some(bare) = lone_phantom(ctx) {
        return outdent_phantom(ctx, &bare);
    }
    let items = all_items(ctx.ast);
    let mut edits = Vec::new();
    for root in selected_roots(ctx, &items) {
        let own = indent_chars(ctx, root.item);
        let target = root.parent.map_or(0, |parent| indent_chars(ctx, parent));
        let amount = own.saturating_sub(target);
        if amount == 0 {
            continue;
        }
        let (a, b) = item_lines(ctx, &root);
        for line in (a..=b).filter(|&l| !ctx.is_blank_line(l)) {
            let origin = indent_origin(ctx, line);
            let text = ctx.slice(TextRange::new(origin, ctx.line_range(line).end));
            let blanks = text.len() - text.trim_start_matches([' ', '\t']).len();
            let remove = amount.min(blanks);
            if remove > 0 {
                edits.push(Edit::delete(TextRange::new(origin, origin + remove)));
            }
        }
        // The item joins the list of its parent: a bullet between numbered items (or the
        // other way round) would split that list in three.
        if let Some(parent) = root.parent {
            let own = ctx.slice(root.marker());
            let parents = ctx.slice(parent.markers[0]);
            let own_ordered = own.as_bytes()[0].is_ascii_digit();
            let parent_ordered = parents.as_bytes()[0].is_ascii_digit();
            if own_ordered != parent_ordered {
                let marker = if parent_ordered {
                    format!("1{}", &parents[parents.len() - 1..])
                } else {
                    parents.into_owned()
                };
                edits.push(Edit::replace(root.marker(), marker));
            }
        }
    }
    edits.sort_by_key(|e| (e.range.start, e.range.end));
    (!edits.is_empty()).then(|| finish_mapped(ctx, edits))
}

/// True when the selection's head lies in a list item (Tab then indents instead of inserting
/// a tab character).
#[must_use]
pub fn in_item(ctx: Ctx<'_>) -> bool {
    item_around(ctx, ctx.selection.head).is_some()
}

/// Moves a phantom item out to the level of the item it sits in (it then follows that item as
/// a sibling and is a real item). `None` when it is not indented further than that item.
fn outdent_phantom(ctx: Ctx<'_>, bare: &BareItem) -> Option<Transaction> {
    let origin = indent_origin(ctx, ctx.line_of(bare.marker));
    let own = bare.marker - origin;
    let target = item_at(ctx.ast, bare.marker).map_or(0, |parent| indent_chars(ctx, parent.item));
    let amount = own.saturating_sub(target);
    (amount > 0).then(|| {
        finish_mapped(
            ctx,
            vec![Edit::delete(TextRange::new(origin, origin + amount))],
        )
    })
}

/// Backspace with the caret right behind a list marker (or task box): removes the task box
/// first, then the marker, keeping the item text. Works at the end of a bare marker line (a
/// marker and task box without text) too. Ordered lists are renumbered. `None` when the caret is not at the start
/// of an item's text.
pub fn backspace_at_item_start(ctx: Ctx<'_>) -> Option<Transaction> {
    if !ctx.selection.is_caret() {
        return None;
    }
    let caret = ctx.selection.head;
    if let Some(bare) = bare_item(ctx, caret)
        && caret == bare.line.end
    {
        let start = bare.task.unwrap_or(bare.marker);
        let edits = vec![Edit::delete(TextRange::new(start, caret))];
        return Some(finish(ctx, edits, Selection::caret(start)));
    }
    let item = item_around(ctx, caret)?;
    let on_marker_line = ctx.line_of(item.marker().start) == ctx.line_of(caret);
    if !on_marker_line || caret != item_content_start(ctx, &item) {
        return None;
    }
    let start = item
        .checkbox()
        .map_or(item.marker().start, |t| t.checkbox.start);
    let edits = vec![Edit::delete(TextRange::new(start, caret))];
    Some(finish(ctx, edits, Selection::caret(start)))
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn toggle_list_adds_markers() {
        for (kind, before, after) in [
            (ListKind::Bullet, "a‸", "- a‸"),
            (ListKind::Bullet, "«a\nb»", "- «a\n- b»"),
            (ListKind::Numbered, "«a\nb\nc»", "1. «a\n2. b\n3. c»"),
            (ListKind::Task, "a‸", "- [ ] a‸"),
            (ListKind::Bullet, "‸", "- ‸"),
            (ListKind::Bullet, "> a‸", "> - a‸"),
            (ListKind::Numbered, "«a\n\nb»", "1. «a\n\n2. b»"),
            (ListKind::Bullet, "  a‸", "  - a‸"),
        ] {
            check(before, after, |c| toggle_list(c, kind));
        }
    }

    #[test]
    fn toggle_list_removes_markers_when_all_lines_match() {
        for (kind, before, after) in [
            (ListKind::Bullet, "- a‸", "a‸"),
            (ListKind::Bullet, "«- a\n- b»", "«a\nb»"),
            (ListKind::Numbered, "«1. a\n2. b»", "«a\nb»"),
            (ListKind::Task, "- [ ] a‸", "a‸"),
            (ListKind::Task, "«- [x] a\n- [ ] b»", "«a\nb»"),
        ] {
            check(before, after, |c| toggle_list(c, kind));
        }
    }

    #[test]
    fn toggle_list_converts_between_kinds() {
        for (kind, before, after) in [
            (ListKind::Numbered, "«- a\n- b»", "«1. a\n2. b»"),
            (ListKind::Bullet, "1. a‸", "- a‸"),
            (ListKind::Task, "- a‸", "- [ ] a‸"),
            (ListKind::Bullet, "- [ ] a‸", "- a‸"),
            (ListKind::Numbered, "«3. a\nb»", "«3. a\n4. b»"),
            (ListKind::Task, "«- [x] a\nb»", "«- [x] a\n- [ ] b»"),
            (ListKind::Task, "* a‸", "* [ ] a‸"),
            (ListKind::Task, "1. a‸", "- [ ] a‸"),
        ] {
            check(before, after, |c| toggle_list(c, kind));
        }
    }

    #[test]
    fn toggle_list_skips_protected_lines() {
        check_none("```\nco‸de\n```", |c| toggle_list(c, ListKind::Bullet));
        check_none("«\n\n»", |c| toggle_list(c, ListKind::Bullet));
    }

    #[test]
    fn toggle_list_renumbers_neighbours() {
        check("1. a\n2. b‸\n3. c", "1. a\n- b‸\n3. c", |c| {
            toggle_list(c, ListKind::Bullet)
        });
        check("1. a\nb‸\n2. c", "1. a\n2. b‸\n3. c", |c| {
            toggle_list(c, ListKind::Numbered)
        });
    }

    #[test]
    fn indent_nests_under_the_previous_sibling() {
        fn align(c: Ctx<'_>) -> Option<Transaction> {
            indent_items(c, ListIndent::Align)
        }
        check("- a\n- b‸", "- a\n  - b‸", align);
        check("- a\n- b‸\n  - c", "- a\n  - b‸\n    - c", align);
        check("- a\n«- b\n- c»", "- a\n  «- b\n  - c»", align);
        check("> - a\n> - b‸", "> - a\n>   - b‸", align);
        check("- a\n- b\n  cont‸", "- a\n  - b\n    cont‸", align);
        check("- a\n\n- b\n\n  para‸", "- a\n\n  - b\n\n    para‸", align);
        check("1. a\n2. b‸", "1. a\n   1. b‸", align);
        check("1. a\n   1. x\n2. b‸", "1. a\n   1. x\n   2. b‸", align);
        check_none("- a‸", align);
        check_none("plain‸", align);
    }

    #[test]
    fn indent_aligns_with_the_text_of_the_previous_sibling() {
        fn align(c: Ctx<'_>) -> Option<Transaction> {
            indent_items(c, ListIndent::Align)
        }
        // Up to four spaces behind the marker belong to it.
        check("-   a\n- b‸", "-   a\n    - b‸", align);
        // Five or more spaces start an indented code block and an empty item has no text:
        // both count as a single space.
        check("-      a\n- b‸", "-      a\n  - b‸", align);
        check("-\n- b‸", "-\n  - b‸", align);
    }

    #[test]
    fn empty_items_indent_like_any_other() {
        fn align(c: Ctx<'_>) -> Option<Transaction> {
            indent_items(c, ListIndent::Align)
        }
        check("- a\n- ‸", "- a\n  - ‸", align);
        check("- a\n  - b\n  - ‸", "- a\n  - b\n    - ‸", align);
        check("1. a\n2. ‸", "1. a\n   1. ‸", align);
        check("- [ ] a\n- [ ] ‸", "- [ ] a\n  - [ ] ‸", align);
        // The empty item under another item's line is that item's paragraph text for the
        // parser: it has no sibling to nest under, and the item above must not move instead.
        check_none("- a\n  - ‸", align);
        check_none("- x\n- a\n  - ‸", align);
        check_none("- a\n  - b\n    - ‸", align);
        check_none("1. a\n   1. ‸", align);
    }

    #[test]
    fn indent_unit_follows_the_setting() {
        check("- a\n- b‸", "- a\n  - b‸", |c| {
            indent_items(c, ListIndent::Two)
        });
        check("- a\n- b‸", "- a\n    - b‸", |c| {
            indent_items(c, ListIndent::Four)
        });
        check("- a\n- b‸", "- a\n\t- b‸", |c| {
            indent_items(c, ListIndent::Tab)
        });
        // Never less than needed to nest under an ordered item's text.
        check("1. a\n2. b‸", "1. a\n   1. b‸", |c| {
            indent_items(c, ListIndent::Two)
        });
        check("1. a\n2. b‸", "1. a\n\t1. b‸", |c| {
            indent_items(c, ListIndent::Tab)
        });
    }

    #[test]
    fn outdent_moves_to_the_parent_level() {
        check("- a\n  - b‸", "- a\n- b‸", outdent_items);
        check("- a\n  - b‸\n    - c", "- a\n- b‸\n  - c", outdent_items);
        check("- a\n\t- b‸", "- a\n- b‸", outdent_items);
        check(" - a‸", "- a‸", outdent_items);
        check("> - a\n>   - b‸", "> - a\n> - b‸", outdent_items);
        // A lazy continuation line has no indentation to give up.
        check("- a\n  - b‸\nlazy", "- a\n- b‸\nlazy", outdent_items);
        check(
            "- a\n  - b\n  - c\n«    - d\n- e»",
            "- a\n  - b\n  - c\n«  - d\n- e»",
            outdent_items,
        );
        check_none("- a‸", outdent_items);
        check_none("plain‸", outdent_items);
    }

    #[test]
    fn empty_items_outdent_like_any_other() {
        for (before, after) in [
            // Real empty items.
            ("- a\n  - b\n  - ‸", "- a\n  - b\n- ‸"),
            ("- [ ] a\n  - [ ] ‸", "- [ ] a\n- [ ] ‸"),
            // Empty items under another item's line are text to the parser: they move out to
            // the level of that item and become its siblings.
            ("- a\n  - ‸", "- a\n- ‸"),
            ("- a\n  - b\n    - ‸", "- a\n  - b\n  - ‸"),
            ("> - a\n>   - ‸", "> - a\n> - ‸"),
            ("1. a\n   1. ‸", "1. a\n2. ‸"),
            ("- a\n  - ‸  ", "- a\n- ‸  "),
        ] {
            check(before, after, outdent_items);
        }
        // Already at the level of the item above.
        check_none("- a\n- ‸", outdent_items);
        check_none("text\n- ‸", outdent_items);
    }

    #[test]
    fn indent_then_outdent_round_trips_ordered_numbers() {
        check("1. a\n2. b\n3. c‸", "1. a\n2. b\n   1. c‸", |c| {
            indent_items(c, ListIndent::Align)
        });
        check("1. a\n   1. b‸\n2. c", "1. a\n2. b‸\n3. c", outdent_items);
    }

    #[test]
    fn outdent_takes_the_marker_kind_of_the_list_it_joins() {
        check("1. a\n   - b‸\n2. c", "1. a\n2. b‸\n3. c", outdent_items);
        check("1) a\n   - [ ] b‸", "1) a\n2) [ ] b‸", outdent_items);
        check("- a\n  1. b‸\n- c", "- a\n- b‸\n- c", outdent_items);
        check(
            "* a\n  1. b‸\n     1. d",
            "* a\n* b‸\n   1. d",
            outdent_items,
        );
        // A top-level item has no list to join.
        check("  1. a‸", "1. a‸", outdent_items);
    }

    #[test]
    fn in_item_follows_the_caret() {
        let (text, selection) = parse_marked("- a‸\n\nplain");
        let ast = parse_document(&text);
        let buffer = crate::text::Buffer::from_text(&text);
        assert!(in_item(Ctx::new(&buffer, &ast, selection)));
        assert!(!in_item(Ctx::new(&buffer, &ast, Selection::caret(8))));
    }

    #[test]
    fn backspace_removes_task_box_then_marker() {
        check("- ‸a", "‸a", backspace_at_item_start);
        check("- ‸", "‸", backspace_at_item_start);
        check("a\n\n- x\n- ‸", "a\n\n- x\n‸", backspace_at_item_start);
        check("- [ ] ‸a", "- ‸a", backspace_at_item_start);
        check("- [x] ‸a", "- ‸a", backspace_at_item_start);
        check("- a\n  - ‸b", "- a\n  ‸b", backspace_at_item_start);
        check(
            "1. a\n2. ‸b\n3. c",
            "1. a\n‸b\n2. c",
            backspace_at_item_start,
        );
        check_none("- a‸", backspace_at_item_start);
        check_none("- «a»", backspace_at_item_start);
        check_none("plain‸", backspace_at_item_start);
        check_none("- a\n  co‸nt", backspace_at_item_start);
    }

    #[test]
    fn backspace_on_an_empty_item_removes_box_then_marker() {
        for (before, after) in [
            // Empty items under another item's line are text to the parser.
            ("- a\n  - ‸", "- a\n  ‸"),
            ("1. a\n   1. ‸", "1. a\n   ‸"),
            ("> - a\n>   - ‸", "> - a\n>   ‸"),
            ("- a\n  -   ‸", "- a\n  ‸"),
            // Empty task items: the box first, then the marker.
            ("- [ ] ‸", "- ‸"),
            ("- [x] a\n- [ ] ‸", "- [x] a\n- ‸"),
            ("- [ ] a\n  - [ ] ‸", "- [ ] a\n  - ‸"),
            ("- a\n  - [ ] ‸", "- a\n  - ‸"),
            ("1. a\n2. [ ] ‸", "1. a\n2. ‸"),
            ("1. a\n2. ‸", "1. a\n‸"),
        ] {
            check(before, after, backspace_at_item_start);
        }
        // A caret inside the blanks behind the marker is not at the item start.
        check_none("- a\n  -‸  ", backspace_at_item_start);
        // A bare marker under a paragraph is plain text.
        check_none("text\n- ‸", backspace_at_item_start);
    }

    fn bare_of(marked: &str) -> Option<BareItem> {
        let (text, selection) = parse_marked(marked);
        let buffer = crate::text::Buffer::from_text(&text);
        let ast = parse_document(&text);
        bare_item(Ctx::new(&buffer, &ast, selection), selection.head)
    }

    #[test]
    fn bare_items_are_empty_items_even_when_the_parser_sees_text() {
        let real = bare_of("- ‸").expect("an empty item");
        assert_eq!(
            (real.line, real.marker, real.task, real.phantom),
            (TextRange::new(0, 2), 0, None, false)
        );
        let after_blank = bare_of("- a\n\n- ‸").expect("an empty item");
        assert_eq!((after_blank.marker, after_blank.phantom), (5, false));
        let task = bare_of("- [ ] ‸").expect("an empty task item");
        assert_eq!(
            (task.line, task.marker, task.task, task.phantom),
            (TextRange::new(0, 6), 0, Some(2), false)
        );
        let phantom = bare_of("- a\n  - ‸").expect("a phantom item");
        assert_eq!(
            (phantom.line, phantom.marker, phantom.task, phantom.phantom),
            (TextRange::new(4, 8), 6, None, true)
        );
        let nested_task = bare_of("- a\n  - [ ] ‸").expect("an empty task item");
        assert_eq!((nested_task.marker, nested_task.task), (6, Some(8)));
        // Not bare: text behind the marker, plain text, a marker under a paragraph, an item
        // that continues below, protected blocks.
        for marked in [
            "- a\n  - x‸",
            "plain‸",
            "text\n- ‸",
            "-‸\n  foo",
            "```\n- ‸\n```",
            "- a\n  - [ ] x‸",
        ] {
            assert!(bare_of(marked).is_none(), "{marked:?}");
        }
    }

    #[test]
    fn renumber_whole_document() {
        for (before, after) in [
            ("1. a\n1. b\n1. c‸", "1. a\n2. b\n3. c‸"),
            ("3. a\n7. b‸", "3. a\n4. b‸"),
            (
                "1. a\n   1. x\n   1. y\n1. b‸",
                "1. a\n   1. x\n   2. y\n2. b‸",
            ),
            ("9. a\n9. b\n9. c‸", "9. a\n10. b\n11. c‸"),
            ("1) a\n1) b‸", "1) a\n2) b‸"),
        ] {
            check(before, after, renumber_lists);
        }
        check_none("1. a\n2. b‸", renumber_lists);
        check_none("- a\n- b‸", renumber_lists);
    }

    #[test]
    fn affix_helpers_respect_char_boundaries() {
        assert_eq!(common_affixes("abcd", "abXd"), (2, 1));
        assert_eq!(common_affixes("same", "same"), (4, 0));
        // 'ı' (C4 B1) and 'İ' (C4 B0) share a first byte.
        assert_eq!(common_affixes("aı", "aİ"), (1, 0));
        assert_eq!(common_affixes("ıa", "İa"), (0, 1));
    }

    #[test]
    fn quote_prefix_lengths() {
        for (line, len) in [
            ("a", 0),
            ("> a", 2),
            (">a", 1),
            ("> > a", 4),
            ("  > a", 4),
            ("> - a", 2),
            ("", 0),
        ] {
            assert_eq!(quote_prefix_len(line), len, "{line:?}");
        }
    }

    #[test]
    fn common_affixes_never_split_a_character() {
        assert_eq!(common_affixes("1. é\n", "1. ©\n"), (3, 1));
        assert_eq!(common_affixes("é", "ê"), (0, 0));
        assert_eq!(common_affixes("1. a\n2. b", "1. a\n3. b"), (5, 3));
    }

    #[test]
    fn finish_without_a_net_change_yields_an_empty_transaction() {
        check("1. a\n2. b‸", "1. a\n2. b‸", |c| {
            let end = Selection::caret(c.buffer.len());
            Some(finish(c, vec![Edit::insert(0, "")], end))
        });
    }
}
