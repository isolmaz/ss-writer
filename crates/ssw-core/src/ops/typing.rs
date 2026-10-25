//! Typing behaviors: Enter (list, quote and callout continuation, fence auto-close), Shift+Enter,
//! text insertion and the deletion keys.

use crate::edit::{Edit, Selection, Transaction};
use crate::markdown::ast::BlockKind;
use crate::text::TextRange;

use super::lists::{
    backspace_at_item_start, bare_item, finish, item_around, item_content_start, quote_prefix_len,
};
use super::{Ctx, container_prefix_len};

/// Replaces the selection with `text` (a typed character, an IME result); the caret ends
/// after it. Ordered lists the edit touches are renumbered.
pub fn insert_text(ctx: Ctx<'_>, text: &str) -> Transaction {
    let range = ctx.selection.range();
    let caret = range.start + text.len();
    finish(
        ctx,
        vec![Edit::replace(range, text)],
        Selection::caret(caret),
    )
}

/// Backspace: deletes the selection, removes a list marker the caret sits behind, or deletes the
/// grapheme before the caret. `None` at the start of the document. Ordered lists the edit
/// touches are renumbered.
pub fn backspace(ctx: Ctx<'_>) -> Option<Transaction> {
    if !ctx.selection.is_caret() {
        return Some(delete_selection(ctx));
    }
    if let Some(tx) = backspace_at_item_start(ctx) {
        return Some(tx);
    }
    let head = ctx.selection.head;
    let previous = ctx.buffer.prev_grapheme_boundary(head);
    (previous < head).then(|| {
        finish(
            ctx,
            vec![Edit::delete(TextRange::new(previous, head))],
            Selection::caret(previous),
        )
    })
}

/// Delete: deletes the selection or the grapheme after the caret. `None` at the end of the
/// document. Ordered lists the edit touches are renumbered.
pub fn delete_forward(ctx: Ctx<'_>) -> Option<Transaction> {
    if !ctx.selection.is_caret() {
        return Some(delete_selection(ctx));
    }
    let head = ctx.selection.head;
    let next = ctx.buffer.next_grapheme_boundary(head);
    (next > head).then(|| {
        finish(
            ctx,
            vec![Edit::delete(TextRange::new(head, next))],
            Selection::caret(head),
        )
    })
}

fn delete_selection(ctx: Ctx<'_>) -> Transaction {
    let range = ctx.selection.range();
    finish(
        ctx,
        vec![Edit::delete(range)],
        Selection::caret(range.start),
    )
}

/// Keeps quote markers and blanks, turns every other character into a blank: the prefix that
/// continues a container on the next line.
pub(crate) fn continuation_of(prefix: &str) -> String {
    prefix
        .chars()
        .map(|c| {
            if matches!(c, '>' | ' ' | '\t') {
                c
            } else {
                ' '
            }
        })
        .collect()
}

/// Leading blanks and quote markers of `text`.
fn blank_prefix_len(text: &str) -> usize {
    text.len() - text.trim_start_matches([' ', '\t', '>']).len()
}

/// Enter. Continues lists, tasks, quotes and callouts, leaves them on an empty item or line,
/// closes an opening ```` ``` ```` / `$$` line, keeps the indentation inside code, and
/// otherwise inserts a line break. A selection is replaced.
pub fn enter(ctx: Ctx<'_>) -> Transaction {
    let sel = ctx.selection.range();
    let line_range = ctx.line_range(ctx.line_of(sel.start));
    let line = ctx.slice(line_range);
    if sel.is_empty()
        && sel.start == line_range.end
        && let Some(tx) = auto_close(ctx, &line, line_range)
    {
        return tx;
    }
    if let Some(tx) = exit_bare_item(ctx, sel) {
        return tx;
    }
    if let Some(tx) = enter_in_item(ctx, sel) {
        return tx;
    }
    if let Some(tx) = enter_in_quote(ctx, sel, line_range, &line) {
        return tx;
    }
    let upto = &line[..sel.start - line_range.start];
    let blanks = upto.len() - upto.trim_start_matches([' ', '\t']).len();
    let text = format!("\n{}", &upto[..blanks]);
    let caret = sel.start + text.len();
    ctx.transaction(vec![Edit::replace(sel, text)], Selection::caret(caret))
}

/// Shift+Enter: a line break inside the paragraph that keeps the container prefix (quote
/// markers; list indentation aligned to the item text) and starts no new item.
pub fn shift_enter(ctx: Ctx<'_>) -> Transaction {
    let sel = ctx.selection.range();
    let line_range = ctx.line_range(ctx.line_of(sel.start));
    let line = ctx.slice(line_range);
    let before = sel.start - line_range.start;
    let prefix_len = if ctx.in_verbatim_block(sel.start) {
        blank_prefix_len(&line)
    } else {
        container_prefix_len(&line)
    };
    let text = format!("\n{}", continuation_of(&line[..prefix_len.min(before)]));
    let caret = sel.start + text.len();
    ctx.transaction(vec![Edit::replace(sel, text)], Selection::caret(caret))
}

/// Enter at the end of an opening fence or `$$` line whose block is not closed: inserts an empty
/// line and the closing delimiter.
fn auto_close(ctx: Ctx<'_>, line: &str, line_range: TextRange) -> Option<Transaction> {
    let rest = &line[container_prefix_len(line)..];
    let opens = rest.starts_with("```") || rest.starts_with("~~~") || rest.trim_end() == "$$";
    if !opens {
        return None;
    }
    let line_index = ctx.line_of(line_range.start);
    let closing = ctx
        .ast
        .path_at(line_range.end)
        .into_iter()
        .filter_map(|n| n.as_block())
        .find_map(|block| match &block.kind {
            BlockKind::CodeBlock(code) => {
                let fence = code.fence?;
                let opens_here = ctx.line_of(fence.open.start) == line_index;
                (opens_here && fence.close.is_none())
                    .then(|| fence.ch.to_string().repeat(fence.len))
            }
            BlockKind::MathBlock(math) => {
                let opens_here = ctx.line_of(block.markers[0].start) == line_index;
                (opens_here && !math.closed).then(|| "$$".to_owned())
            }
            _ => None,
        })?;
    let prefix = continuation_of(&line[..container_prefix_len(line)]);
    let text = format!("\n{prefix}\n{prefix}{closing}");
    let caret = line_range.end + 1 + prefix.len();
    Some(ctx.transaction(
        vec![Edit::insert(line_range.end, text)],
        Selection::caret(caret),
    ))
}

/// Enter on a line that holds only a list marker and task box (an empty item, also one the
/// parser reads as text after another item's line): leaves the list by removing the marker, the
/// task box and the indentation in front of them (quote prefixes stay).
fn exit_bare_item(ctx: Ctx<'_>, sel: TextRange) -> Option<Transaction> {
    if !sel.is_empty() {
        return None;
    }
    let bare = bare_item(ctx, sel.start)?;
    let before = ctx.slice(TextRange::new(bare.line.start, bare.marker));
    let from = if before.trim().is_empty() {
        bare.line.start
    } else {
        bare.marker
    };
    let edits = vec![Edit::delete(TextRange::new(from, bare.line.end))];
    Some(finish(ctx, edits, Selection::caret(from)))
}

/// Enter in the text of a list item: a new item with the next marker.
fn enter_in_item(ctx: Ctx<'_>, sel: TextRange) -> Option<Transaction> {
    let item = item_around(ctx, sel.start)?;
    let in_text = item.item.children.first().is_none_or(|first| {
        matches!(first.kind, BlockKind::Paragraph | BlockKind::Heading { .. })
            && sel.start <= first.range.end
    });
    if !in_text {
        return None;
    }
    let marker = item.marker();
    let marker_line = ctx.line_range(ctx.line_of(marker.start));
    let on_marker_line = ctx.line_of(sel.start) == ctx.line_of(marker.start);
    let content_start = item_content_start(ctx, &item);
    let start = if on_marker_line {
        sel.start.max(content_start)
    } else {
        sel.start
    };
    let end = sel.end.max(start);
    let indent = ctx.slice(TextRange::new(marker_line.start, marker.start));
    let marker_text = ctx.slice(marker);
    let next_marker = if marker_text.as_bytes()[0].is_ascii_digit() {
        let (digits, delimiter) = marker_text.split_at(marker_text.len() - 1);
        let number: u64 = digits.parse().unwrap_or(0);
        format!("{}{delimiter}", number + 1)
    } else {
        marker_text.into_owned()
    };
    let after_marker = ctx.slice(TextRange::new(marker.end, marker_line.end));
    let spacing_len = after_marker.len() - after_marker.trim_start_matches([' ', '\t']).len();
    let spacing = if spacing_len == 0 {
        " "
    } else {
        &after_marker[..spacing_len]
    };
    let task = if item.checkbox().is_some() {
        "[ ] "
    } else {
        ""
    };
    let text = format!("\n{indent}{next_marker}{spacing}{task}");
    let caret = start + text.len();
    let edit = Edit::replace(TextRange::new(start, end), text);
    Some(finish(ctx, vec![edit], Selection::caret(caret)))
}

/// Enter in a quote or callout line: continues the quote prefix, or leaves one quote level when
/// the line has no text.
fn enter_in_quote(
    ctx: Ctx<'_>,
    sel: TextRange,
    line_range: TextRange,
    line: &str,
) -> Option<Transaction> {
    let prefix_len = quote_prefix_len(line);
    if prefix_len == 0 || sel.start < line_range.start + prefix_len {
        return None;
    }
    let (prefix, rest) = line.split_at(prefix_len);
    if rest.trim().is_empty() && sel.is_empty() && !ctx.in_verbatim_block(sel.start) {
        let trimmed = prefix.trim_end();
        let outer = &trimmed[..trimmed.len() - 1];
        let caret = line_range.start + outer.len();
        return Some(ctx.transaction(
            vec![Edit::replace(line_range, outer)],
            Selection::caret(caret),
        ));
    }
    let text = format!("\n{prefix}");
    let caret = sel.start + text.len();
    Some(ctx.transaction(vec![Edit::replace(sel, text)], Selection::caret(caret)))
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    fn enter_op(c: Ctx<'_>) -> Option<Transaction> {
        Some(enter(c))
    }

    #[test]
    fn enter_closes_nothing_after_a_fence_look_alike_in_indented_code() {
        // Indented code is no fence: only the indentation is kept.
        check("    ```‸", "    ```\n    ‸", enter_op);
    }

    #[test]
    fn enter_in_an_item_whose_first_block_is_not_text_adds_no_item() {
        check("- > a‸", "- > a\n‸", enter_op);
    }

    #[test]
    fn enter_continues_bullets_and_tasks() {
        for (before, after) in [
            ("- a‸", "- a\n- ‸"),
            ("* a‸", "* a\n* ‸"),
            ("+ a‸", "+ a\n+ ‸"),
            ("- a‸b", "- a\n- ‸b"),
            ("-   a‸", "-   a\n-   ‸"),
            ("- [ ] a‸", "- [ ] a\n- [ ] ‸"),
            ("- [x] a‸", "- [x] a\n- [ ] ‸"),
            ("- a\n  b‸", "- a\n  b\n- ‸"),
            ("- a\n  - b‸", "- a\n  - b\n  - ‸"),
            ("> - a‸", "> - a\n> - ‸"),
            ("- ‸a", "- \n- ‸a"),
            ("«- a»", "- \n- ‸"),
        ] {
            check(before, after, enter_op);
        }
    }

    #[test]
    fn enter_continues_and_renumbers_ordered_lists() {
        for (before, after) in [
            ("1. a‸", "1. a\n2. ‸"),
            ("1) a‸", "1) a\n2) ‸"),
            ("9. a‸", "9. a\n10. ‸"),
            ("1. a‸\n2. b", "1. a\n2. ‸\n3. b"),
            ("1. a‸\n2. b\n3. c", "1. a\n2. ‸\n3. b\n4. c"),
            (
                "1. a\n   1. x‸\n   2. y\n2. b",
                "1. a\n   1. x\n   2. ‸\n   3. y\n2. b",
            ),
            ("1. [ ] a‸", "1. [ ] a\n2. [ ] ‸"),
        ] {
            check(before, after, enter_op);
        }
    }

    #[test]
    fn enter_on_empty_item_leaves_the_list() {
        for (before, after) in [
            ("- a\n- ‸", "- a\n‸"),
            ("- ‸", "‸"),
            ("1. a\n2. ‸", "1. a\n‸"),
            ("- a\n  - b\n  - ‸", "- a\n  - b\n‸"),
            ("> - a\n> - ‸", "> - a\n> ‸"),
            ("1. a\n2. ‸\n3. c", "1. a\n‸\n2. c"),
            // The empty item right under another item's line is paragraph text to the parser
            // (an empty item cannot interrupt a paragraph): Enter still leaves the list.
            ("- a\n  - ‸", "- a\n‸"),
            ("1. a\n   1. ‸", "1. a\n‸"),
            ("- a\n  - b\n    - ‸", "- a\n  - b\n‸"),
            ("> - a\n>   - ‸", "> - a\n>   ‸"),
            // Task items: an empty one at any level.
            ("- [ ] ‸", "‸"),
            ("- [x] a\n- [ ] ‸", "- [x] a\n‸"),
            ("- [x] a\n- [ ]‸", "- [x] a\n‸"),
            ("- [ ] a\n  - [ ] ‸", "- [ ] a\n‸"),
            ("1. [ ] a\n2. [ ] ‸", "1. [ ] a\n‸"),
            ("- a\n  - [ ] ‸", "- a\n‸"),
            // Blanks behind the marker do not matter.
            ("- a\n- ‸  ", "- a\n‸"),
            ("- a\n  - ‸  ", "- a\n‸"),
        ] {
            check(before, after, enter_op);
        }
    }

    #[test]
    fn a_marker_after_a_plain_paragraph_or_with_content_below_is_not_an_empty_item() {
        for (before, after) in [
            // A bare marker under a paragraph is text, not a list.
            ("text\n- ‸", "text\n- \n‸"),
            // The item continues on the next line: Enter splits as in any item.
            ("-‸\n  foo", "-\n- ‸\n  foo"),
            // A selection is replaced, not an exit.
            ("- a\n  - «x»", "- a\n  - \n  - ‸"),
        ] {
            check(before, after, enter_op);
        }
    }

    #[test]
    fn enter_continues_and_leaves_quotes_and_callouts() {
        for (before, after) in [
            ("> a‸", "> a\n> ‸"),
            (">a‸", ">a\n>‸"),
            ("> > a‸", "> > a\n> > ‸"),
            ("> [!NOTE] T‸", "> [!NOTE] T\n> ‸"),
            ("> [!NOTE]\n> body‸", "> [!NOTE]\n> body\n> ‸"),
            ("> a\n> ‸", "> a\n‸"),
            ("> > ‸", "> ‸"),
            ("> ‸", "‸"),
            ("‸> a", "\n‸> a"),
        ] {
            check(before, after, enter_op);
        }
    }

    #[test]
    fn enter_closes_fences_and_math() {
        for (before, after) in [
            ("```‸", "```\n‸\n```"),
            ("```rust‸", "```rust\n‸\n```"),
            ("~~~‸", "~~~\n‸\n~~~"),
            ("````‸", "````\n‸\n````"),
            ("$$‸", "$$\n‸\n$$"),
            ("- ```‸", "- ```\n  ‸\n  ```"),
            ("> ```‸", "> ```\n> ‸\n> ```"),
            ("```‸\ntext", "```\n‸\n```\ntext"),
            ("a\n\n```‸", "a\n\n```\n‸\n```"),
        ] {
            check(before, after, enter_op);
        }
    }

    #[test]
    fn enter_inside_closed_blocks_is_a_plain_break() {
        for (before, after) in [
            ("```\nx\n```‸", "```\nx\n```\n‸"),
            ("```‸rust\nx\n```", "```\n‸rust\nx\n```"),
            ("```\n  co‸de\n```", "```\n  co\n  ‸de\n```"),
            ("$$\nx‸\n$$", "$$\nx\n‸\n$$"),
            ("$$x‸$$", "$$x\n‸$$"),
        ] {
            check(before, after, enter_op);
        }
    }

    #[test]
    fn enter_elsewhere_inserts_a_line_break() {
        for (before, after) in [
            ("ab‸cd", "ab\n‸cd"),
            ("«ab»c", "\n‸c"),
            ("# Title‸", "# Title\n‸"),
            ("İşık‸", "İşık\n‸"),
            ("  ab‸", "  ab\n  ‸"),
            ("‸", "\n‸"),
        ] {
            check(before, after, enter_op);
        }
    }

    #[test]
    fn enter_in_second_paragraph_of_an_item_keeps_indentation() {
        check("- a\n\n  b‸", "- a\n\n  b\n  ‸", enter_op);
    }

    #[test]
    fn shift_enter_keeps_the_container_without_a_new_item() {
        for (before, after) in [
            ("- a‸", "- a\n  ‸"),
            ("1. a‸", "1. a\n   ‸"),
            ("- [ ] a‸", "- [ ] a\n      ‸"),
            ("> a‸", "> a\n> ‸"),
            ("a‸", "a\n‸"),
            ("- a\n  b‸", "- a\n  b\n  ‸"),
            ("- a‸b", "- a\n  ‸b"),
            ("‸- a", "\n‸- a"),
            ("```\n  x‸\n```", "```\n  x\n  ‸\n```"),
            ("«ab»", "\n‸"),
        ] {
            check(before, after, |c| Some(shift_enter(c)));
        }
    }

    #[test]
    fn insert_text_replaces_selection() {
        check("a‸", "ax‸", |c| Some(insert_text(c, "x")));
        check("a«bc»d", "aZ‸d", |c| Some(insert_text(c, "Z")));
        check("‸", "İ‸", |c| Some(insert_text(c, "İ")));
    }

    #[test]
    fn backspace_deletes_graphemes_and_selections() {
        for (before, after) in [
            ("ab‸", "a‸"),
            ("a‸b", "‸b"),
            ("«ab»c", "‸c"),
            ("»ab«c", "‸c"),
            ("a👨‍👩‍👧‸", "a‸"),
            ("İ‸", "‸"),
            ("e\u{301}‸", "‸"),
            ("a\n‸b", "a‸b"),
            ("a\r\n‸b", "a‸b"),
            ("- ‸a", "‸a"),
            ("- [ ] ‸a", "- ‸a"),
            ("1. a\n2. ‸b\n3. c", "1. a\n‸b\n2. c"),
            ("- a‸", "- ‸"),
            // Empty items, also under another item's line (text to the parser).
            ("- a\n  - ‸", "- a\n  ‸"),
            ("- [ ] ‸", "- ‸"),
            ("- a\n  -‸", "- a\n  ‸"),
        ] {
            check(before, after, backspace);
        }
        check_none("‸ab", backspace);
        check_none("‸", backspace);
    }

    #[test]
    fn delete_forward_deletes_graphemes_and_selections() {
        for (before, after) in [
            ("‸ab", "‸b"),
            ("a‸b", "a‸"),
            ("«ab»c", "‸c"),
            ("‸👨‍👩‍👧a", "‸a"),
            ("a‸\nb", "a‸b"),
            ("a‸\r\nb", "a‸b"),
            ("‸İ", "‸"),
        ] {
            check(before, after, delete_forward);
        }
        check_none("ab‸", delete_forward);
        check_none("‸", delete_forward);
    }

    #[test]
    fn edits_that_change_the_item_count_renumber_the_list() {
        // A deleted or cut item, a joined line, a typed marker: the numbers follow.
        check("1. a\n«2. b\n»3. c", "1. a\n‸2. c", delete_forward);
        check("1. a\n«2. b\n»3. c", "1. a\n‸2. c", backspace);
        check("1. a‸\n2. b\n3. c", "1. a‸2. b\n2. c", delete_forward);
        check("1. a\n2. b‸\n3. c", "1. a\n2. b‸3. c", delete_forward);
        check("1. a\n2‸\n3. b", "1. a\n2.‸\n3. b", |c| {
            Some(insert_text(c, "."))
        });
        check("1. a\n2.‸\n3. b", "1. a\n2. ‸\n3. b", |c| {
            Some(insert_text(c, " "))
        });
        // The first item keeps its number: it starts the list.
        check("1‸. a\n2. b", "10‸. a\n11. b", |c| {
            Some(insert_text(c, "0"))
        });
        // Outside ordered lists the edit is the plain one.
        check("- a\n«- b\n»- c", "- a\n‸- c", delete_forward);
    }

    #[test]
    fn deleting_what_separates_two_ordered_lists_renumbers_the_second() {
        check(
            "1. a\n\n«text»\n\n3. b",
            "1. a\n\n‸\n\n2. b",
            delete_forward,
        );
        check(
            "1. a\n\n«text»\n\n3. b\n4. c",
            "1. a\n\n‸\n\n2. b\n3. c",
            backspace,
        );
        // A paragraph between a list and a bullet list merges nothing.
        check("1. a\n\n«text»\n\n- b", "1. a\n\n‸\n\n- b", delete_forward);
    }

    #[test]
    fn keystroke_transactions_classify_as_typing() {
        use crate::undo::{TypingKind, typing_kind};
        let keystroke = |before: &str, op: fn(Ctx<'_>) -> Option<Transaction>| {
            let (text, selection) = parse_marked(before);
            let buffer = crate::text::Buffer::from_text(&text);
            let ast = crate::markdown::parse_document(&text);
            op(Ctx::new(&buffer, &ast, selection)).expect("applies")
        };
        let kind = |before: &str, op: fn(Ctx<'_>) -> Option<Transaction>| {
            typing_kind(&keystroke(before, op))
        };
        assert_eq!(kind("ab‸", backspace), Some(TypingKind::DeleteBackward));
        assert_eq!(kind("a‸b", delete_forward), Some(TypingKind::DeleteForward));
        assert_eq!(kind("«ab»", backspace), None);

        // In an ordered list item the edit stays the one the key made, at the caret, also
        // between repeated letters: undo only groups keystrokes it classifies as typing.
        let tx = keystroke("1. h‸llo", |c| Some(insert_text(c, "l")));
        assert_eq!(tx.edits, [Edit::insert(4, "l")]);
        assert_eq!(tx.selection_after, Selection::caret(5));
        assert_eq!(typing_kind(&tx), Some(TypingKind::Insert));
        let tx = keystroke("1. hel‸lo", backspace);
        assert_eq!(tx.edits, [Edit::delete(TextRange::new(5, 6))]);
        assert_eq!(tx.selection_after, Selection::caret(5));
        assert_eq!(typing_kind(&tx), Some(TypingKind::DeleteBackward));
        let tx = keystroke("1. he‸llo", delete_forward);
        assert_eq!(tx.edits, [Edit::delete(TextRange::new(5, 6))]);
        assert_eq!(tx.selection_after, Selection::caret(5));
        assert_eq!(typing_kind(&tx), Some(TypingKind::DeleteForward));
    }
}
