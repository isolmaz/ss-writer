//! Block-level formatting: headings, quotes, callouts, code and math blocks, dividers, page
//! breaks and footnotes.

use std::collections::HashSet;

use crate::edit::{Edit, Selection, Transaction};
use crate::markdown::ast::{Block, BlockKind, CalloutKind, InlineKind, NodeRef, iter_inlines};
use crate::text::TextRange;

use super::Ctx;
use super::format::longest_backtick_run;

/// A heading found in the tree.
struct HeadingInfo {
    range: TextRange,
    level: u8,
    marker: TextRange,
    content: TextRange,
    closing: Option<TextRange>,
}

fn headings(ctx: Ctx<'_>) -> Vec<HeadingInfo> {
    ctx.ast
        .iter_blocks()
        .filter_map(|b| match &b.kind {
            BlockKind::Heading { level, marker } => Some(HeadingInfo {
                range: b.range,
                level: *level,
                marker: *marker,
                content: b.content,
                closing: b.markers.get(1).copied(),
            }),
            _ => None,
        })
        .collect()
}

/// Innermost blocks around the selection head, innermost first.
fn enclosing_blocks<'a>(ctx: Ctx<'a>) -> impl Iterator<Item = &'a Block> {
    ctx.ast
        .path_at(ctx.selection.head)
        .into_iter()
        .rev()
        .filter_map(NodeRef::as_block)
}

/// True when line-prefix operations must not touch `line` (code, math, HTML, tables).
pub(crate) fn line_is_protected(ctx: Ctx<'_>, line: usize) -> bool {
    let start = ctx.line_range(line).start;
    ctx.in_verbatim_block(start)
        || ctx
            .ast
            .path_at(start)
            .into_iter()
            .any(|n| matches!(n.as_block().map(|b| &b.kind), Some(BlockKind::Table(_))))
}

/// Lines a heading command applies to: the selected lines without blank ones (a lone blank
/// caret line is allowed) and without protected ones.
fn heading_lines(ctx: Ctx<'_>) -> Vec<usize> {
    let (first, last) = ctx.selected_lines();
    (first..=last)
        .filter(|&l| (first == last || !ctx.is_blank_line(l)) && !line_is_protected(ctx, l))
        .collect()
}

fn heading_on_line<'a>(
    list: &'a [HeadingInfo],
    ctx: Ctx<'_>,
    line: usize,
) -> Option<&'a HeadingInfo> {
    let lr = ctx.line_range(line);
    list.iter()
        .find(|h| lr.start <= h.range.start && h.range.start <= lr.end)
}

fn clear_heading_edits(h: &HeadingInfo) -> Vec<Edit> {
    let mut edits = vec![Edit::delete(TextRange::new(
        h.marker.start,
        h.content.start,
    ))];
    if let Some(closing) = h.closing {
        edits.push(Edit::delete(TextRange::new(h.content.end, closing.end)));
    }
    edits
}

/// Makes the selected lines headings of `level` (clamped to 1–6); when they all already are,
/// turns them back into paragraphs. `None` when no line qualifies (code, tables, …).
pub fn toggle_heading(ctx: Ctx<'_>, level: u8) -> Option<Transaction> {
    let level = level.clamp(1, 6);
    let lines = heading_lines(ctx);
    if lines.is_empty() {
        return None;
    }
    let list = headings(ctx);
    let found: Vec<(usize, Option<&HeadingInfo>)> = lines
        .iter()
        .map(|&l| (l, heading_on_line(&list, ctx, l)))
        .collect();
    let all_same = found
        .iter()
        .all(|(_, h)| h.is_some_and(|h| h.level == level));
    let hashes = "#".repeat(usize::from(level));
    let mut edits = Vec::new();
    for (line, heading) in found {
        match heading {
            Some(h) if all_same => edits.extend(clear_heading_edits(h)),
            Some(h) => edits.push(Edit::replace(h.marker, hashes.clone())),
            None => edits.push(Edit::insert(ctx.content_start(line), format!("{hashes} "))),
        }
    }
    Some(ctx.transaction_mapped(edits))
}

/// Turns the selected headings into paragraphs. `None` when no selected line is a heading.
pub fn clear_heading(ctx: Ctx<'_>) -> Option<Transaction> {
    let list = headings(ctx);
    let edits: Vec<Edit> = heading_lines(ctx)
        .into_iter()
        .filter_map(|l| heading_on_line(&list, ctx, l))
        .flat_map(clear_heading_edits)
        .collect();
    (!edits.is_empty()).then(|| ctx.transaction_mapped(edits))
}

/// The lines a quote-like command applies to: the whole paragraph around a caret, otherwise
/// the selected lines.
fn target_lines(ctx: Ctx<'_>) -> (usize, usize) {
    if ctx.selection.is_caret()
        && let Some(block) = ctx.ast.block_at(ctx.selection.head)
        && matches!(block.kind, BlockKind::Paragraph)
    {
        return (ctx.line_of(block.range.start), ctx.line_of(block.range.end));
    }
    ctx.selected_lines()
}

/// Adds a quote level to the selected lines, or removes one when every non-blank line is
/// already quoted. `None` for a caret inside code or math.
pub fn toggle_quote(ctx: Ctx<'_>) -> Option<Transaction> {
    if ctx.selection.is_caret() && ctx.in_verbatim_block(ctx.selection.head) {
        return None;
    }
    // A callout is one unit: its tag line without the body (or the body without the tag line)
    // is no callout any more.
    let callout = ctx.selection.is_caret().then(|| {
        enclosing_blocks(ctx)
            .find(|b| matches!(b.kind, BlockKind::BlockQuote | BlockKind::Callout { .. }))
            .filter(|b| matches!(b.kind, BlockKind::Callout { .. }))
    });
    let callout = callout.flatten();
    let (first, last) = match callout {
        Some(block) => (ctx.line_of(block.range.start), ctx.line_of(block.range.end)),
        None => target_lines(ctx),
    };
    let non_blank: Vec<usize> = (first..=last).filter(|&l| !ctx.is_blank_line(l)).collect();
    let is_quoted = |l: usize| ctx.line_text(l).trim_start().starts_with('>');
    let remove = !non_blank.is_empty() && non_blank.iter().all(|&l| is_quoted(l));
    let mut edits = Vec::new();
    if remove {
        // The tag goes with the quote: a line holding only the tag disappears, a title stays
        // as a line of its own.
        let tag_line = callout.and_then(|block| match &block.kind {
            BlockKind::Callout { tag, title, .. } => Some((ctx.line_of(tag.start), *tag, *title)),
            _ => None,
        });
        for line in non_blank {
            if let Some((tag_at, tag, title)) = tag_line
                && line == tag_at
            {
                match title {
                    None => {
                        edits.push(delete_line(ctx, tag.start));
                        continue;
                    }
                    Some(title) => edits.push(Edit::delete(TextRange::new(tag.start, title.start))),
                }
            }
            let text = ctx.line_text(line);
            let indent = text.len() - text.trim_start().len();
            let extra = usize::from(text[indent + 1..].starts_with(' '));
            let at = ctx.line_range(line).start + indent;
            edits.push(Edit::delete(TextRange::new(at, at + 1 + extra)));
        }
        edits.sort_by_key(|e| e.range.start);
    } else {
        for line in first..=last {
            edits.push(Edit::insert(
                ctx.line_range(line).start,
                quote_prefix(ctx, line, first == last),
            ));
        }
    }
    Some(ctx.transaction_mapped(edits))
}

/// `> ` for text lines (and a lone blank line), `>` for blank lines inside a range.
fn quote_prefix(ctx: Ctx<'_>, line: usize, single: bool) -> &'static str {
    if ctx.is_blank_line(line) && !single {
        ">"
    } else {
        "> "
    }
}

/// Sets the callout kind of the quote around the caret: changes the kind, removes the tag when
/// the kind is already set, turns a plain quote into a callout, or quotes the selected lines as
/// a callout.
pub fn set_callout(ctx: Ctx<'_>, kind: CalloutKind) -> Option<Transaction> {
    let quote = enclosing_blocks(ctx)
        .find(|b| matches!(b.kind, BlockKind::BlockQuote | BlockKind::Callout { .. }));
    let tag = format!("[!{}]", kind.keyword());
    match quote {
        Some(Block {
            kind:
                BlockKind::Callout {
                    kind: current,
                    tag: old_tag,
                    title,
                },
            ..
        }) => {
            let edit = if *current == kind {
                let end = title.map_or(old_tag.end, |t| t.start);
                Edit::delete(TextRange::new(old_tag.start, end))
            } else {
                Edit::replace(*old_tag, tag)
            };
            Some(ctx.transaction_mapped(vec![edit]))
        }
        Some(block) => {
            let marker = block.markers[0];
            let line_start = ctx.line_range(ctx.line_of(marker.start)).start;
            let prefix = ctx.slice(TextRange::new(line_start, marker.end));
            let header = format!("{prefix} {tag}\n");
            Some(ctx.transaction_mapped(vec![Edit::insert(line_start, header)]))
        }
        None => {
            if ctx.selection.is_caret() && ctx.in_verbatim_block(ctx.selection.head) {
                return None;
            }
            let (first, last) = target_lines(ctx);
            let mut edits = Vec::new();
            for line in first..=last {
                let prefix = quote_prefix(ctx, line, first == last);
                let text = if line == first {
                    format!("> {tag}\n{prefix}")
                } else {
                    prefix.to_owned()
                };
                edits.push(Edit::insert(ctx.line_range(line).start, text));
            }
            Some(ctx.transaction_mapped(edits))
        }
    }
}

/// Deletes the line holding `offset` with its terminator; the last line (which has none) takes
/// the terminator of the line before it instead.
fn delete_line(ctx: Ctx<'_>, offset: usize) -> Edit {
    let line = ctx.line_of(offset);
    let full = ctx.buffer.line_range_with_terminator(line);
    if line > 0 && full.end == ctx.buffer.len() && full.end == ctx.line_range(line).end {
        Edit::delete(TextRange::new(ctx.line_range(line - 1).end, full.end))
    } else {
        Edit::delete(full)
    }
}

/// Sorts deletions and joins the ones that overlap.
fn merge_deletes(mut edits: Vec<Edit>) -> Vec<Edit> {
    edits.sort_by_key(|e| e.range.start);
    let mut merged: Vec<Edit> = Vec::with_capacity(edits.len());
    for edit in edits {
        match merged.last_mut() {
            Some(last) if edit.range.start < last.range.end => {
                last.range.end = last.range.end.max(edit.range.end);
            }
            _ => merged.push(edit),
        }
    }
    merged
}

/// Wraps lines `first..=last` in `fence` lines, or inserts an empty fenced block with the caret
/// inside when the selection is a caret on a blank line.
fn wrap_lines(ctx: Ctx<'_>, fence: &str) -> Option<Transaction> {
    if ctx.selection.is_caret() && ctx.in_verbatim_block(ctx.selection.head) {
        return None;
    }
    let (first, last) = target_lines(ctx);
    if ctx.selection.is_caret() && ctx.is_blank_line(first) {
        let block = format!("{fence}\n\n{fence}");
        return Some(insert_block(ctx, &block, Some(fence.len() + 1)));
    }
    let start = ctx.line_range(first).start;
    let end = ctx.line_range(last).end;
    let edits = vec![
        Edit::insert(start, format!("{fence}\n")),
        Edit::insert(end, format!("\n{fence}")),
    ];
    Some(ctx.transaction_mapped(edits))
}

/// Wraps the selected lines (or the paragraph at the caret) in a fenced code block, or removes
/// the fences when the caret is inside one. `None` inside other verbatim blocks and indented
/// code.
pub fn toggle_code_block(ctx: Ctx<'_>) -> Option<Transaction> {
    let code = enclosing_blocks(ctx).find_map(|b| match &b.kind {
        BlockKind::CodeBlock(code) => Some(code),
        _ => None,
    });
    if let Some(code) = code {
        let fence = code.fence?;
        let mut edits = vec![delete_line(ctx, fence.open.start)];
        if let Some(close) = fence.close {
            edits.push(delete_line(ctx, close.start));
        }
        return Some(ctx.transaction_mapped(merge_deletes(edits)));
    }
    let (first, last) = target_lines(ctx);
    let text = ctx.slice(TextRange::new(
        ctx.line_range(first).start,
        ctx.line_range(last).end,
    ));
    let fence = "`".repeat((longest_backtick_run(&text) + 1).max(3));
    wrap_lines(ctx, &fence)
}

/// Wraps the selected lines (or the paragraph at the caret) in `$$` lines, or removes the
/// delimiters when the caret is inside a math block.
pub fn toggle_math_block(ctx: Ctx<'_>) -> Option<Transaction> {
    let math = enclosing_blocks(ctx).find(|b| matches!(b.kind, BlockKind::MathBlock(_)));
    if let Some(block) = math {
        let edits = block
            .markers
            .iter()
            .map(|m| {
                if ctx.line_text(ctx.line_of(m.start)).trim() == "$$" {
                    delete_line(ctx, m.start)
                } else {
                    Edit::delete(*m)
                }
            })
            .collect();
        return Some(ctx.transaction_mapped(merge_deletes(edits)));
    }
    wrap_lines(ctx, "$$")
}

/// Inserts `block` as its own block at the selection: replacing a blank caret line or after the
/// current line, with blank lines around it. The caret goes `caret_in_block` bytes into the
/// block, or onto the line after it.
fn insert_block(ctx: Ctx<'_>, block: &str, caret_in_block: Option<usize>) -> Transaction {
    let line = ctx.line_of(ctx.selection.end());
    let lr = ctx.line_range(line);
    let (region, prefix) = if ctx.is_blank_line(line) {
        let separated = line == 0 || ctx.is_blank_line(line - 1);
        (lr, if separated { "" } else { "\n" })
    } else {
        (TextRange::at(lr.end), "\n\n")
    };
    let suffix = if region.end == ctx.buffer.len() || !ctx.is_blank_line(line + 1) {
        "\n"
    } else {
        ""
    };
    let caret = region.start + prefix.len() + caret_in_block.unwrap_or(block.len() + 1);
    let edit = Edit::replace(region, format!("{prefix}{block}{suffix}"));
    ctx.transaction(vec![edit], Selection::caret(caret))
}

/// Inserts a thematic break as its own block.
pub fn insert_divider(ctx: Ctx<'_>) -> Transaction {
    // `---` at the very start could open front matter, so use `***` there.
    let at_start = ctx.line_of(ctx.selection.end()) == 0 && ctx.is_blank_line(0);
    insert_block(ctx, if at_start { "***" } else { "---" }, None)
}

/// Inserts `<!-- pagebreak -->` as its own block.
pub fn insert_page_break(ctx: Ctx<'_>) -> Transaction {
    insert_block(ctx, "<!-- pagebreak -->", None)
}

/// The smallest positive number not used as a footnote label.
fn next_footnote_label(ctx: Ctx<'_>) -> u64 {
    let mut used: HashSet<u64> = HashSet::new();
    for block in ctx.ast.iter_blocks() {
        if let BlockKind::FootnoteDefinition { label } = &block.kind {
            used.extend(label.parse::<u64>());
        }
        for group in block.inline_groups() {
            for inline in iter_inlines(group) {
                if let InlineKind::FootnoteRef { label } = &inline.kind {
                    used.extend(label.parse::<u64>());
                }
            }
        }
    }
    (1..).find(|n| !used.contains(n)).unwrap_or(1)
}

/// Inserts a footnote reference `[^n]` after the selection and appends the definition
/// `[^n]: ` at the end of the document, with the caret after it.
pub fn insert_footnote(ctx: Ctx<'_>) -> Transaction {
    let label = next_footnote_label(ctx);
    let reference = format!("[^{label}]");
    let at = ctx.selection.end();
    let len = ctx.buffer.len();
    let tail = ctx.slice(TextRange::new(len.saturating_sub(2), len));
    let separator = if at == len || !tail.ends_with('\n') {
        "\n\n"
    } else if tail.ends_with("\n\n") {
        ""
    } else {
        "\n"
    };
    let definition = format!("{separator}[^{label}]: ");
    let caret = len + reference.len() + definition.len();
    let edits = if at == len {
        vec![Edit::insert(len, format!("{reference}{definition}"))]
    } else {
        vec![Edit::insert(at, reference), Edit::insert(len, definition)]
    };
    ctx.transaction(edits, Selection::caret(caret))
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn heading_set_change_and_toggle_off() {
        for (level, before, after) in [
            (1, "Title‸", "# Title‸"),
            (2, "Ti‸tle", "## Ti‸tle"),
            (3, "‸Title", "### ‸Title"),
            (1, "## Ti‸tle", "# Ti‸tle"),
            (2, "## Ti‸tle", "Ti‸tle"),
            (6, "###### x‸", "x‸"),
            (1, "# Title‸ #", "Title‸"),
            (7, "x‸", "###### x‸"),
            (0, "x‸", "# x‸"),
            (1, "‸", "# ‸"),
            (2, "- item‸", "- ## item‸"),
            (2, "> quote‸", "> ## quote‸"),
            (1, "- [ ] task‸", "- [ ] # task‸"),
        ] {
            check(before, after, |c| toggle_heading(c, level));
        }
    }

    #[test]
    fn heading_over_several_lines() {
        check("«a\n\nb»", "# «a\n\n# b»", |c| toggle_heading(c, 1));
        check("«# a\n## b»", "«# a\n# b»", |c| toggle_heading(c, 1));
        check("«# a\n# b»", "«a\nb»", |c| toggle_heading(c, 1));
        check("«# a\nb»", "«# a\n# b»", |c| toggle_heading(c, 1));
    }

    #[test]
    fn heading_skips_code_and_tables() {
        check_none("```\nco‸de\n```", |c| toggle_heading(c, 1));
        check_none("| a | b |\n|---|---|\n| ‸1 | 2 |", |c| {
            toggle_heading(c, 1)
        });
        check_none("$$\nx‸\n$$", |c| toggle_heading(c, 1));
        check_none("«\n\n»", |c| toggle_heading(c, 1));
    }

    #[test]
    fn clear_heading_turns_headings_into_paragraphs() {
        check("## Ti‸tle", "Ti‸tle", clear_heading);
        check("«# a\nb\n## c»", "«a\nb\nc»", clear_heading);
        check_none("plain‸", clear_heading);
    }

    #[test]
    fn quote_toggle() {
        check("a‸", "> a‸", toggle_quote);
        check("> a‸", "a‸", toggle_quote);
        check(">a‸", "a‸", toggle_quote);
        check("  > a‸", "  a‸", toggle_quote);
        check("«a\nb»", "> «a\n> b»", toggle_quote);
        check("«a\n\nb»", "> «a\n>\n> b»", toggle_quote);
        check("«> a\n> b»", "«a\nb»", toggle_quote);
        check("«> a\n\n> b»", "«a\n\nb»", toggle_quote);
        check("«> a\nb»", "> «> a\n> b»", toggle_quote);
        check("‸", "> ‸", toggle_quote);
        check("> > a‸", "> a‸", toggle_quote);
        check("- a‸", "> - a‸", toggle_quote);
        check_none("```\nco‸de\n```", toggle_quote);
    }

    #[test]
    fn quote_toggle_in_a_callout_removes_the_whole_callout() {
        // Removing the quote of one line would leave a tag without a body (or a lazy
        // continuation of the tag line).
        check("> [!NOTE]\n> no‸te", "no‸te", toggle_quote);
        check("> [!TIP] Title\n> a\n> b‸", "Title\na\nb‸", toggle_quote);
        check("> [!NOTE]\n> ‸a\n>\n> b", "‸a\n\nb", toggle_quote);
        check("x\n> [!NOTE]‸", "x‸", toggle_quote);
        // A plain quote keeps working paragraph by paragraph.
        check("> a\n>\n> b‸", "> a\n>\nb‸", toggle_quote);
    }

    #[test]
    fn quote_wraps_the_whole_paragraph_at_the_caret() {
        check("one\nt‸wo\n\nthree", "> one\n> t‸wo\n\nthree", toggle_quote);
    }

    #[test]
    fn callout_kinds() {
        check("> [!NOTE]\n> b‸", "> [!TIP]\n> b‸", |c| {
            set_callout(c, CalloutKind::Tip)
        });
        check("> [!NOTE] Ti‸tle", "> [!WARNING] Ti‸tle", |c| {
            set_callout(c, CalloutKind::Warning)
        });
        // Same kind again: the tag (and the space before the title) goes away.
        check("> [!NOTE] Ti‸tle\n> body", "> Ti‸tle\n> body", |c| {
            set_callout(c, CalloutKind::Note)
        });
        check("> [!NOTE]‸\n> body", "> ‸\n> body", |c| {
            set_callout(c, CalloutKind::Note)
        });
        // Plain quote becomes a callout.
        check("> te‸xt", "> [!IMPORTANT]\n> te‸xt", |c| {
            set_callout(c, CalloutKind::Important)
        });
        check("> > in‸ner", "> > [!CAUTION]\n> > in‸ner", |c| {
            set_callout(c, CalloutKind::Caution)
        });
    }

    #[test]
    fn callout_from_plain_lines() {
        check("te‸xt", "> [!NOTE]\n> te‸xt", |c| {
            set_callout(c, CalloutKind::Note)
        });
        check("«a\nb»", "> [!TIP]\n> «a\n> b»", |c| {
            set_callout(c, CalloutKind::Tip)
        });
        check("«a\n\nb»", "> [!TIP]\n> «a\n>\n> b»", |c| {
            set_callout(c, CalloutKind::Tip)
        });
        check("‸", "> [!NOTE]\n> ‸", |c| {
            set_callout(c, CalloutKind::Note)
        });
        check_none("```\nco‸de\n```", |c| set_callout(c, CalloutKind::Note));
    }

    #[test]
    fn code_block_wrap_and_unwrap() {
        check("«a\nb»", "```\n«a\nb»\n```", toggle_code_block);
        check(
            "li‸ne one\nline two\n\nnext",
            "```\nli‸ne one\nline two\n```\n\nnext",
            toggle_code_block,
        );
        check("# He‸ad", "```\n# He‸ad\n```", toggle_code_block);
        check("```rust\nco‸de\n```", "co‸de", toggle_code_block);
        check("a\n\n```\nx‸\n```\n\nb", "a\n\nx‸\n\nb", toggle_code_block);
        check("```\nunclosed‸", "unclosed‸", toggle_code_block);
        // Both fence lines touch: their deletions merge.
        check("```\n‸```", "‸", toggle_code_block);
        check_none("    indented‸", toggle_code_block);
        check_none("$$\nx‸\n$$", toggle_code_block);
    }

    #[test]
    fn code_block_fence_outgrows_backticks_in_the_text() {
        check("«a ``` b»", "````\n«a ``` b»\n````", toggle_code_block);
    }

    #[test]
    fn code_block_inserted_on_blank_lines() {
        check("‸", "```\n‸\n```\n", toggle_code_block);
        check("a\n‸\nb", "a\n\n```\n‸\n```\n\nb", toggle_code_block);
        check("a\n\n‸", "a\n\n```\n‸\n```\n", toggle_code_block);
    }

    #[test]
    fn math_block_wrap_and_unwrap() {
        check("«x+y»", "$$\n«x+y»\n$$", toggle_math_block);
        check("a\n\n$$\nx‸\n$$\n\nb", "a\n\nx‸\n\nb", toggle_math_block);
        check("$$x‸$$", "x‸", toggle_math_block);
        check("‸", "$$\n‸\n$$\n", toggle_math_block);
        check("$$\nx‸", "x‸", toggle_math_block);
        check_none("```\nx‸\n```", toggle_math_block);
    }

    #[test]
    fn divider_and_page_break_are_separate_blocks() {
        for (before, after) in [
            ("text‸", "text\n\n---\n‸"),
            ("a‸\n\nb", "a\n\n---\n‸\nb"),
            ("a‸\nb", "a\n\n---\n‸\nb"),
            ("a\n‸\nb", "a\n\n---\n‸\nb"),
            ("a\n  ‸\nb", "a\n\n---\n‸\nb"),
            ("a\n\n‸", "a\n\n---\n‸"),
            ("‸", "***\n‸"),
            ("‸\ntext", "***\n‸\ntext"),
        ] {
            check(before, after, |c| Some(insert_divider(c)));
        }
        check("a‸", "a\n\n<!-- pagebreak -->\n‸", |c| {
            Some(insert_page_break(c))
        });
        // A divider after selected text goes after the last selected line.
        check("«a»\nb", "a\n\n---\n‸\nb", |c| Some(insert_divider(c)));
    }

    #[test]
    fn footnote_gets_next_free_label_and_definition() {
        check("Hello‸ world.\n", "Hello[^1] world.\n\n[^1]: ‸", |c| {
            Some(insert_footnote(c))
        });
        check("Hello‸", "Hello[^1]\n\n[^1]: ‸", |c| {
            Some(insert_footnote(c))
        });
        check("‸", "[^1]\n\n[^1]: ‸", |c| Some(insert_footnote(c)));
        check(
            "a[^1] b‸\n\n[^1]: x\n",
            "a[^1] b[^2]\n\n[^1]: x\n\n[^2]: ‸",
            |c| Some(insert_footnote(c)),
        );
        check("a‸\n\n", "a[^1]\n\n[^1]: ‸", |c| {
            Some(insert_footnote(c))
        });
        check(
            "a[^1][^3]‸\n\n[^1]: x\n\n[^3]: y",
            "a[^1][^3][^2]\n\n[^1]: x\n\n[^3]: y\n\n[^2]: ‸",
            |c| Some(insert_footnote(c)),
        );
        // Non-numeric labels do not count.
        check(
            "a[^n]‸\n\n[^n]: x\n",
            "a[^n][^1]\n\n[^n]: x\n\n[^1]: ‸",
            |c| Some(insert_footnote(c)),
        );
        // The reference goes after a selection.
        check("«word» x\n", "word[^1] x\n\n[^1]: ‸", |c| {
            Some(insert_footnote(c))
        });
    }
}
