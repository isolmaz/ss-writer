//! Markdown → plain text: markers stripped, list markers kept as text bullets, tables as
//! aligned columns.
//!
//! Layout: blocks are separated by a blank line; list items are indented two spaces per
//! nesting level behind their `- ` / `1. ` / `[ ] ` prefix; block quotes and callout bodies are
//! indented by two spaces (a callout starts with its title); code and math keep their source
//! lines; footnote references appear as `[label]` and each definition as `[label] text`.

use ssw_core::markdown::ast::{Alignment, Block, BlockKind, Inline, InlineKind, Table};
use ssw_core::markdown::{parse_document, unescape_markdown};

/// Plain-text rendering of Markdown source.
pub fn markdown_to_text(src: &str) -> String {
    let doc = parse_document(src);
    let body = join_blocks(&doc.blocks, src, "\n\n");
    let trimmed = body.trim_matches('\n');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

/// Appends the reader-visible text of `inlines`; both kinds of line break become `soft`.
/// Footnote references are written as `[label]` only when `footnote_refs` is set.
pub(crate) fn inline_plain(
    inlines: &[Inline],
    src: &str,
    soft: &str,
    footnote_refs: bool,
    out: &mut String,
) {
    for n in inlines {
        match &n.kind {
            InlineKind::Text | InlineKind::RawHtml => out.push_str(n.range.get(src).unwrap_or("")),
            InlineKind::Escape | InlineKind::InlineMath | InlineKind::WikiLink(_) => {
                out.push_str(n.content.get(src).unwrap_or(""));
            }
            InlineKind::Entity { decoded } => out.push_str(decoded),
            InlineKind::Code { text } => out.push_str(text),
            InlineKind::Emphasis
            | InlineKind::Strong
            | InlineKind::Underline
            | InlineKind::Strikethrough
            | InlineKind::Highlight { .. }
            | InlineKind::Link(_) => inline_plain(&n.children, src, soft, footnote_refs, out),
            InlineKind::Image(img) => {
                if n.children.is_empty() {
                    out.push_str(&unescape_markdown(img.alt.get(src).unwrap_or("")));
                } else {
                    inline_plain(&n.children, src, soft, footnote_refs, out);
                }
            }
            InlineKind::FootnoteRef { label } => {
                if footnote_refs {
                    out.push('[');
                    out.push_str(label);
                    out.push(']');
                }
            }
            InlineKind::HardBreak | InlineKind::SoftBreak => out.push_str(soft),
        }
    }
}

fn plain(inlines: &[Inline], src: &str) -> String {
    let mut s = String::new();
    inline_plain(inlines, src, "\n", true, &mut s);
    s
}

/// Renders each block and joins the non-empty results with `sep`.
fn join_blocks(blocks: &[Block], src: &str, sep: &str) -> String {
    let parts: Vec<String> = blocks
        .iter()
        .map(|b| block_text(b, src))
        .filter(|s| !s.is_empty())
        .collect();
    parts.join(sep)
}

fn prefix_lines(text: &str, first: &str, rest: &str) -> String {
    let lines: Vec<String> = text
        .split('\n')
        .enumerate()
        .map(|(i, l)| {
            let line = format!("{}{l}", if i == 0 { first } else { rest });
            if l.is_empty() {
                line.trim_end().to_string()
            } else {
                line
            }
        })
        .collect();
    lines.join("\n")
}

fn block_text(b: &Block, src: &str) -> String {
    match &b.kind {
        BlockKind::Heading { .. } | BlockKind::Paragraph => plain(&b.inlines, src),
        BlockKind::List {
            ordered,
            start,
            delimiter,
            tight,
        } => {
            let sep = if *tight { "\n" } else { "\n\n" };
            let mut items = Vec::new();
            for (i, item) in b.children.iter().enumerate() {
                let mut marker = if *ordered {
                    format!("{}{} ", u64::from(*start) + i as u64, delimiter.as_char())
                } else {
                    "- ".to_string()
                };
                if let BlockKind::ListItem { task: Some(t), .. } = &item.kind {
                    marker.push_str(if t.checked { "[x] " } else { "[ ] " });
                }
                let body = join_blocks(&item.children, src, sep);
                let indent = " ".repeat(marker.chars().count());
                items.push(prefix_lines(&body, &marker, &indent));
            }
            items.join(sep)
        }
        BlockKind::ListItem { .. } | BlockKind::BlockQuote => {
            prefix_lines(&join_blocks(&b.children, src, "\n\n"), "  ", "  ")
        }
        BlockKind::Callout { kind, title, .. } => {
            let head = match title.and_then(|t| t.get(src)) {
                Some(t) => t.to_string(),
                None => kind.keyword().to_string(),
            };
            let body = join_blocks(&b.children, src, "\n\n");
            if body.is_empty() {
                head
            } else {
                format!("{head}\n{}", prefix_lines(&body, "  ", "  "))
            }
        }
        BlockKind::CodeBlock(cb) => cb.text(src).trim_end_matches('\n').to_string(),
        BlockKind::MathBlock(m) => m.text(src),
        BlockKind::Table(t) => table_text(t, src),
        BlockKind::ThematicBreak => "---".to_string(),
        BlockKind::HtmlBlock(h) => {
            if b.inlines.is_empty() {
                h.text(src).trim_end_matches('\n').to_string()
            } else {
                plain(&b.inlines, src)
            }
        }
        BlockKind::FootnoteDefinition { label } => {
            let body = join_blocks(&b.children, src, "\n\n");
            prefix_lines(&body, &format!("[{label}] "), "    ")
        }
        BlockKind::FrontMatter { .. }
        | BlockKind::LinkReferenceDefinition(_)
        | BlockKind::PageBreak => String::new(),
    }
}

fn table_text(t: &Table, src: &str) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row in std::iter::once(&t.header).chain(t.rows.iter()) {
        rows.push(
            row.cells
                .iter()
                .map(|c| {
                    let mut s = String::new();
                    inline_plain(&c.inlines, src, " ", true, &mut s);
                    s
                })
                .collect(),
        );
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![1usize; cols];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let format_row = |cells: &[String]| -> String {
        let mut line = String::new();
        for (i, width) in widths.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            let cell = cells.get(i).map(String::as_str).unwrap_or("");
            let pad = width - cell.chars().count();
            match t.alignments.get(i) {
                Some(Alignment::Right) => {
                    line.push_str(&" ".repeat(pad));
                    line.push_str(cell);
                }
                Some(Alignment::Center) => {
                    let left = pad / 2;
                    line.push_str(&" ".repeat(left));
                    line.push_str(cell);
                    line.push_str(&" ".repeat(pad - left));
                }
                _ => {
                    line.push_str(cell);
                    line.push_str(&" ".repeat(pad));
                }
            }
        }
        line.trim_end().to_string()
    };
    let mut lines = Vec::with_capacity(rows.len() + 1);
    for (i, row) in rows.iter().enumerate() {
        lines.push(format_row(row));
        if i == 0 {
            let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            lines.push(rule.join("  "));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_table() {
        let table: &[(&str, &str)] = &[
            ("", ""),
            ("\n\n", ""),
            (
                "# Title\n\nSome **bold**, *it*, ~u~, ~~s~~, ==h==, `c`.",
                "Title\n\nSome bold, it, u, s, h, c.\n",
            ),
            (
                "a\\*b &amp; [l](http://x) ![alt *t*](p.png) ![a](p.png)",
                "a*b & l alt t a\n",
            ),
            (
                "[[Note|Alias]] and [[Other]] x[^1] $e=mc^2$ <b>\n\n[^1]: the note",
                "Alias and Other x[1] e=mc^2 <b>\n\n[1] the note\n",
            ),
            ("one  \ntwo\nthree", "one\ntwo\nthree\n"),
            ("- a\n- b\n  - c", "- a\n- b\n  - c\n"),
            ("1. x\n2. y", "1. x\n2. y\n"),
            ("3) x\n4) y", "3) x\n4) y\n"),
            ("- [ ] open\n- [x] done", "- [ ] open\n- [x] done\n"),
            ("- a\n\n- b", "- a\n\n- b\n"),
            ("- ", "-\n"),
            ("> quote\n> more", "  quote\n  more\n"),
            ("> [!NOTE]\n> body", "NOTE\n  body\n"),
            ("> [!TIP] Custom\n> body", "Custom\n  body\n"),
            ("> [!CAUTION]", "CAUTION\n"),
            ("```rust\nfn x() {}\n\n```", "fn x() {}\n"),
            ("$$\na\nb\n$$", "a\nb\n"),
            ("a\n\n---\n\nb", "a\n\n---\n\nb\n"),
            ("---\ntitle: t\n---\nx", "x\n"),
            ("x\n\n<!-- pagebreak -->\n\n[r]: http://e", "x\n"),
            ("<div>raw</div>", "<div>raw</div>\n"),
            ("<img src=\"a.png\" alt=\"pic\">", "pic\n"),
        ];
        for (md, expected) in table {
            assert_eq!(markdown_to_text(md), *expected, "{md:?}");
        }
    }

    #[test]
    fn a_list_item_rendered_on_its_own_is_indented_like_a_quote() {
        let md = "- a\n- b";
        let doc = parse_document(md);
        assert_eq!(block_text(&doc.blocks[0].children[1], md), "  b");
    }

    #[test]
    fn tables_are_aligned_with_alignment_rules() {
        let md = "| Name | Qty | Mid |\n|:--|--:|:-:|\n| apple | 3 | x |\n| kiwi | 12345 | yy |";
        assert_eq!(
            markdown_to_text(md),
            "Name     Qty  Mid\n-----  -----  ---\napple      3   x\nkiwi   12345  yy\n"
        );
    }

    #[test]
    fn short_rows_are_padded_and_cells_use_plain_text() {
        let md = "| a | b |\n|---|---|\n| **x** |";
        assert_eq!(markdown_to_text(md), "a  b\n-  -\nx\n");
    }

    #[test]
    fn heading_text_uses_given_soft_break_and_hides_footnote_refs() {
        let doc = parse_document("a[^1]\nb\n\n[^1]: n");
        let mut s = String::new();
        inline_plain(
            &doc.blocks[0].inlines,
            "a[^1]\nb\n\n[^1]: n",
            " ",
            false,
            &mut s,
        );
        assert_eq!(s, "a b");
    }
}
