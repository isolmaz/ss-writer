//! Paste: URLs over selections, link labels, code blocks and Markdown inserted into containers.

use crate::edit::{Edit, Selection, Transaction};
use crate::markdown::ast::InlineKind;
use crate::text::TextRange;

use super::format::{escape_brackets, link_destination, longest_backtick_run};
use super::lists::{bare_item, finish};
use super::typing::continuation_of;
use super::{Ctx, container_prefix_len, list_marker_len};

const URL_SCHEMES: [&str; 6] = [
    "http://", "https://", "ftp://", "ftps://", "mailto:", "file://",
];

/// True when `text` (surrounding whitespace ignored) is a single URL.
pub fn is_url(text: &str) -> bool {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    !text.chars().any(char::is_whitespace)
        && URL_SCHEMES
            .iter()
            .any(|s| lower.starts_with(s) && lower.len() > s.len())
}

/// Replaces the selection with `text` exactly, the caret ending behind it.
fn replace_literal(ctx: Ctx<'_>, text: &str) -> Transaction {
    let range = ctx.selection.range();
    let caret = range.start + text.len();
    ctx.transaction(vec![Edit::replace(range, text)], Selection::caret(caret))
}

/// Plain-text paste (`Ctrl+Shift+V`): the text is inserted as it is.
pub fn paste_plain(ctx: Ctx<'_>, text: &str) -> Transaction {
    replace_literal(ctx, text)
}

/// Pastes clipboard text. A URL over a selection becomes `[selection](url)`; over the label of
/// a link, or inside a code or math block, the text is inserted literally.
pub fn paste_text(ctx: Ctx<'_>, text: &str) -> Transaction {
    let range = ctx.selection.range();
    let selected = ctx.slice(range);
    let wraps = is_url(text)
        && !range.is_empty()
        && !selected.contains('\n')
        && !in_link_label(ctx, range)
        && !ctx.in_verbatim_block(range.start);
    if !wraps {
        return replace_literal(ctx, text);
    }
    let link = format!(
        "[{}]({})",
        escape_brackets(&selected),
        link_destination(text.trim())
    );
    let caret = range.start + link.len();
    ctx.transaction(vec![Edit::replace(range, link)], Selection::caret(caret))
}

/// True when `range` lies inside the text (label) of an inline link.
fn in_link_label(ctx: Ctx<'_>, range: TextRange) -> bool {
    ctx.ast
        .path_at(range.start)
        .into_iter()
        .filter_map(|n| n.as_inline())
        .any(|node| matches!(node.kind, InlineKind::Link(_)) && node.content.contains_range(range))
}

/// Pastes Markdown that was converted from rich content. Inside a quote or list item, every
/// line after the first gets the container prefix of the caret line; pasting a list onto an empty
/// item replaces that item. Code and math blocks take the text literally. Ordered lists are
/// renumbered.
pub fn paste_markdown(ctx: Ctx<'_>, markdown: &str) -> Transaction {
    let range = ctx.selection.range();
    if ctx.in_verbatim_block(range.start) {
        return replace_literal(ctx, markdown);
    }
    let line = ctx.line_range(ctx.line_of(range.start));
    let line_text = ctx.slice(line);
    let before = range.start - line.start;
    let mut start = range.start;
    let mut prefix = continuation_of(&line_text[..container_prefix_len(&line_text).min(before)]);
    if range.is_empty()
        && list_marker_len(markdown).is_some()
        && let Some(bare) = bare_item(ctx, range.start)
        && range.start == bare.line.end
    {
        start = bare.marker;
        prefix = continuation_of(&ctx.slice(TextRange::new(line.start, start)));
    }
    let mut text = String::new();
    for (i, piece) in markdown.split('\n').enumerate() {
        if i > 0 {
            text.push('\n');
            text.push_str(if piece.is_empty() {
                prefix.trim_end()
            } else {
                &prefix
            });
        }
        text.push_str(piece);
    }
    let caret = start + text.len();
    let edit = Edit::replace(TextRange::new(start, range.end), text);
    finish(ctx, vec![edit], Selection::caret(caret))
}

/// Pastes source code (VS Code, a lone monospace block; spec §2.3). Inside a code or math block
/// the text goes in literally; elsewhere it becomes a fenced code block of its own, the fence
/// longer than any backtick run in the code. Text that follows the selection on its line moves
/// below the block. The caret ends behind the closing fence.
pub fn paste_code(ctx: Ctx<'_>, code: &str) -> Transaction {
    let range = ctx.selection.range();
    if ctx.in_verbatim_block(range.start) {
        return replace_literal(ctx, code);
    }
    let code = code.trim_end_matches('\n');
    let fence = "`".repeat((longest_backtick_run(code) + 1).max(3));
    let line_start = ctx.line_range(ctx.line_of(range.start)).start;
    let before = ctx.slice(TextRange::new(line_start, range.start));
    let after = ctx.slice(TextRange::new(
        range.end,
        ctx.line_range(ctx.line_of(range.end)).end,
    ));
    let container = container_prefix_len(&before);
    let prefix = continuation_of(&before[..container]);
    let mut text = String::new();
    if !before[container..].trim().is_empty() {
        // The fence needs a line of its own.
        text.push('\n');
        text.push_str(&prefix);
    }
    text.push_str(&fence);
    for piece in code.split('\n') {
        text.push('\n');
        text.push_str(if piece.is_empty() {
            prefix.trim_end()
        } else {
            &prefix
        });
        text.push_str(piece);
    }
    text.push('\n');
    text.push_str(&prefix);
    text.push_str(&fence);
    let caret = range.start + text.len();
    if !after.trim().is_empty() {
        text.push('\n');
        text.push_str(&prefix);
    }
    ctx.transaction(vec![Edit::replace(range, text)], Selection::caret(caret))
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn url_detection() {
        for url in [
            "https://a.b",
            "http://a.b/c?d=1#e",
            " https://a.b\n",
            "HTTPS://A.B",
            "mailto:x@y.z",
            "ftp://h/f",
            "file:///c:/x",
        ] {
            assert!(is_url(url), "{url:?}");
        }
        for text in [
            "",
            "https://",
            "mailto:",
            "www.a.b",
            "a b",
            "see https://a.b",
            "https://a b",
            "text",
        ] {
            assert!(!is_url(text), "{text:?}");
        }
    }

    #[test]
    fn url_over_selection_becomes_a_link() {
        check("see «this» now", "see [this](https://a.b)‸ now", |c| {
            Some(paste_text(c, "https://a.b"))
        });
        check("«ışık»", "[ışık](https://a.b/ç)‸", |c| {
            Some(paste_text(c, "  https://a.b/ç\n"))
        });
        check("«a]b»", "[a\\]b](<https://x.y/a(b)>)‸", |c| {
            Some(paste_text(c, "https://x.y/a(b)"))
        });
    }

    #[test]
    fn url_is_literal_without_a_single_line_selection_or_in_special_places() {
        for (before, after, text) in [
            ("a ‸ b", "a https://u‸ b", "https://u"),
            ("«a\nb»", "https://u‸", "https://u"),
            ("«a»", "text‸", "text"),
            ("```\n«a»\n```", "```\nhttps://u‸\n```", "https://u"),
            ("$$\n«a»\n$$", "$$\nhttps://u‸\n$$", "https://u"),
        ] {
            check(before, after, |c| Some(paste_text(c, text)));
        }
    }

    #[test]
    fn pasting_over_a_link_label_replaces_the_label() {
        check("[«old»](http://x)", "[new‸](http://x)", |c| {
            Some(paste_text(c, "new"))
        });
        check("[«old»](http://x)", "[https://z‸](http://x)", |c| {
            Some(paste_text(c, "https://z"))
        });
        check("[o«l»d](http://x)", "[ohttps://z‸d](http://x)", |c| {
            Some(paste_text(c, "https://z"))
        });
        // The destination is not a label: the URL still wraps the selection.
        check("[a](«http://x»)", "[a]([http://x](https://z)‸)", |c| {
            Some(paste_text(c, "https://z"))
        });
    }

    #[test]
    fn plain_paste_is_literal() {
        check("«a»", "https://u‸", |c| {
            Some(paste_plain(c, "https://u"))
        });
        check("x‸", "x- a\n- b‸", |c| Some(paste_plain(c, "- a\n- b")));
    }

    #[test]
    fn markdown_paste_adapts_to_quotes_and_items() {
        for (before, md, after) in [
            ("a‸", "x\ny", "ax\ny‸"),
            ("- a‸", "x\ny", "- ax\n  y‸"),
            ("> a‸", "x\n\ny", "> ax\n>\n> y‸"),
            ("- [ ] a‸", "x\ny", "- [ ] ax\n      y‸"),
            ("- a\n  - b‸", "x\ny", "- a\n  - bx\n    y‸"),
            ("1. a‸", "x\ny", "1. ax\n   y‸"),
            ("‸", "# T\n\nbody", "# T\n\nbody‸"),
        ] {
            check(before, after, |c| Some(paste_markdown(c, md)));
        }
    }

    #[test]
    fn markdown_list_replaces_an_empty_item() {
        for (before, md, after) in [
            ("- ‸", "- x\n- y", "- x\n- y‸"),
            ("- a\n  - b\n  - ‸", "- x\n- y", "- a\n  - b\n  - x\n  - y‸"),
            // An empty item right under another item's line is text to the parser; it is still
            // an empty item to the user.
            ("- a\n  - ‸", "- x\n- y", "- a\n  - x\n  - y‸"),
            ("1. a\n   1. ‸", "1. x\n2. y", "1. a\n   1. x\n   2. y‸"),
            ("> - a\n>   - ‸", "- x\n- y", "> - a\n>   - x\n>   - y‸"),
            // Empty task items are replaced together with their box.
            ("- [x] a\n- [ ] ‸", "- x\n- y", "- [x] a\n- x\n- y‸"),
            ("- [ ] a\n  - [ ] ‸", "- x", "- [ ] a\n  - x‸"),
            ("- [ ] ‸", "1. x", "1. x‸"),
            // A marker under a plain paragraph is text: the pasted list stays inside it.
            ("text\n- ‸", "- x", "text\n- - x‸"),
            ("> - ‸", "- x\n\n- y", "> - x\n>\n> - y‸"),
            ("1. a\n2. ‸", "1. x\n2. y", "1. a\n2. x\n3. y‸"),
            // A non-list paste into an empty item stays inside the item.
            ("- ‸", "x\ny", "- x\n  y‸"),
            // A list pasted into a non-empty item is nested text, not a replacement.
            ("- a‸", "- x", "- a- x‸"),
        ] {
            check(before, after, |c| Some(paste_markdown(c, md)));
        }
    }

    #[test]
    fn code_paste_becomes_a_fenced_block() {
        for (before, code, after) in [
            (
                "‸",
                "let x = 1;\nlet y = 2;\n",
                "```\nlet x = 1;\nlet y = 2;\n```‸",
            ),
            ("‸", "x\n\n\n", "```\nx\n```‸"),
            ("‸", "a\n\nb", "```\na\n\nb\n```‸"),
            // A fence longer than any backtick run inside the code.
            ("‸", "```\nx", "````\n```\nx\n````‸"),
            ("«old»", "x", "```\nx\n```‸"),
            // The fence starts its own line.
            ("para‸", "x", "para\n```\nx\n```‸"),
            ("a ‸b", "x", "a \n```\nx\n```‸\nb"),
            // Container prefixes continue on every line.
            ("> ‸", "a\n\nb", "> ```\n> a\n>\n> b\n> ```‸"),
            ("- ‸", "x", "- ```\n  x\n  ```‸"),
            ("- text‸", "x", "- text\n  ```\n  x\n  ```‸"),
            ("1. ‸", "x", "1. ```\n   x\n   ```‸"),
        ] {
            check(before, after, |c| Some(paste_code(c, code)));
        }
    }

    #[test]
    fn code_paste_in_code_or_math_is_literal() {
        check("```\n‸\n```", "```\na\nb‸\n```", |c| {
            Some(paste_code(c, "a\nb"))
        });
        check("$$\n‸\n$$", "$$\nx\n‸\n$$", |c| {
            Some(paste_code(c, "x\n"))
        });
    }

    #[test]
    fn markdown_paste_in_code_is_literal() {
        check("```\n‸\n```", "```\na\nb‸\n```", |c| {
            Some(paste_markdown(c, "a\nb"))
        });
        check("«x»", "y‸", |c| Some(paste_markdown(c, "y")));
    }
}
