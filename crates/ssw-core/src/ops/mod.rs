//! Editing operations.
//!
//! Every operation is a pure function over a [`Ctx`] (text, tree, primary selection) plus its
//! arguments (settings values, strings to insert) and returns the [`Transaction`] to apply —
//! `None` when the operation does not apply at that position. The caller applies it with
//! [`crate::document::Document::apply`] (or `apply_typing` for keystrokes).

use std::borrow::Cow;

use crate::edit::{Edit, Selection, Transaction};
use crate::markdown::ast::{self, BlockKind};
use crate::text::{Buffer, TextRange};

pub mod blocks;
pub mod format;
pub mod lists;
pub mod movement;
pub mod paste;
pub mod table;
pub mod tasks;
pub mod typing;

/// What an operation sees: the text, its parsed tree and the primary selection.
#[derive(Clone, Copy, Debug)]
pub struct Ctx<'a> {
    pub buffer: &'a Buffer,
    pub ast: &'a ast::Document,
    pub selection: Selection,
}

impl<'a> Ctx<'a> {
    pub fn new(buffer: &'a Buffer, ast: &'a ast::Document, selection: Selection) -> Self {
        Self {
            buffer,
            ast,
            selection,
        }
    }

    /// The text of `range`.
    pub fn slice(&self, range: TextRange) -> Cow<'a, str> {
        self.buffer.slice(range)
    }

    /// Line index containing `offset`.
    pub fn line_of(&self, offset: usize) -> usize {
        self.buffer.line_of(offset)
    }

    /// Content range of `line` (without its terminator).
    pub fn line_range(&self, line: usize) -> TextRange {
        self.buffer.line_range(line)
    }

    /// Content of `line` (without its terminator).
    pub fn line_text(&self, line: usize) -> Cow<'a, str> {
        self.slice(self.buffer.line_range(line))
    }

    /// First and last line touched by the selection. A non-empty selection that ends at the
    /// very start of a line does not include that line.
    pub fn selected_lines(&self) -> (usize, usize) {
        let range = self.selection.range();
        let first = self.line_of(range.start);
        let mut last = self.line_of(range.end);
        if !range.is_empty() && last > first && self.line_range(last).start == range.end {
            last -= 1;
        }
        (first, last)
    }

    /// Offset of the first character of `line` after block-container prefixes (indentation,
    /// quote markers, list markers and task boxes).
    pub fn content_start(&self, line: usize) -> usize {
        let range = self.line_range(line);
        range.start + container_prefix_len(&self.slice(range))
    }

    /// True when `offset` lies inside a block whose text is verbatim (code, math, front
    /// matter, HTML): inline formatting and line-prefix operations do not apply there.
    pub fn in_verbatim_block(&self, offset: usize) -> bool {
        self.ast.path_at(offset).into_iter().any(|node| {
            matches!(
                node.as_block().map(|b| &b.kind),
                Some(
                    BlockKind::CodeBlock(_)
                        | BlockKind::MathBlock(_)
                        | BlockKind::FrontMatter { .. }
                        | BlockKind::HtmlBlock(_)
                )
            )
        })
    }

    /// `true` when `line` is empty or only whitespace.
    pub fn is_blank_line(&self, line: usize) -> bool {
        self.line_text(line).trim().is_empty()
    }

    /// Transaction from this context's selection to `after`.
    pub fn transaction(&self, edits: Vec<Edit>, after: Selection) -> Transaction {
        Transaction::new(edits, self.selection, after)
    }

    /// Transaction whose resulting selection is the current one mapped through the edits.
    pub fn transaction_mapped(&self, edits: Vec<Edit>) -> Transaction {
        let draft = Transaction::new(edits, self.selection, self.selection);
        let after = self.selection.map_through(&draft);
        Transaction {
            selection_after: after,
            ..draft
        }
    }
}

/// Byte length of the container prefix of `line`: leading whitespace, `>` markers (with one
/// optional following space), list markers and task boxes, repeated for nested containers.
pub fn container_prefix_len(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut pos = 0;
    loop {
        pos = skip_blanks(bytes, pos);
        match bytes.get(pos) {
            Some(b'>') => {
                pos += 1;
                if matches!(bytes.get(pos), Some(b' ' | b'\t')) {
                    pos += 1;
                }
            }
            _ => match list_marker_len(&line[pos..]) {
                Some(len) => {
                    pos = skip_blanks(bytes, pos + len);
                    if let Some(task) = task_box_len(&line[pos..]) {
                        pos = skip_blanks(bytes, pos + task);
                    }
                }
                None => return pos,
            },
        }
    }
}

fn skip_blanks(bytes: &[u8], mut pos: usize) -> usize {
    while matches!(bytes.get(pos), Some(b' ' | b'\t')) {
        pos += 1;
    }
    pos
}

/// Length of a list marker (`-`, `+`, `*`, `1.`, `1)`) at the start of `text` that is followed
/// by a blank or the end of the text.
pub fn list_marker_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let len = match bytes.first()? {
        b'-' | b'+' | b'*' => 1,
        b'0'..=b'9' => {
            let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 9 || !matches!(bytes.get(digits), Some(b'.' | b')')) {
                return None;
            }
            digits + 1
        }
        _ => return None,
    };
    matches!(bytes.get(len), None | Some(b' ' | b'\t')).then_some(len)
}

/// Length (3) of a task box `[ ]` / `[x]` / `[X]` at the start of `text` followed by a blank or
/// the end of the text.
pub fn task_box_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let boxed = bytes.len() >= 3
        && bytes[0] == b'['
        && matches!(bytes[1], b' ' | b'x' | b'X')
        && bytes[2] == b']';
    (boxed && matches!(bytes.get(3), None | Some(b' ' | b'\t'))).then_some(3)
}

/// Marked-text helpers for the table-driven tests of this module tree.
///
/// A caret is written `‸`; a selection `«text»` (anchor first, caret at the end) or `»text«`
/// (reversed). The markers are not part of the document text.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::markdown::parse_document;

    pub const CARET: char = '‸';

    /// Splits marker characters from `marked`; returns the text and the selection.
    pub fn parse_marked(marked: &str) -> (String, Selection) {
        let mut text = String::new();
        let (mut caret, mut open, mut close) = (None, None, None);
        for ch in marked.chars() {
            match ch {
                CARET => caret = Some(text.len()),
                '«' => open = Some(text.len()),
                '»' => close = Some(text.len()),
                _ => text.push(ch),
            }
        }
        let selection = match (caret, open, close) {
            (Some(c), None, None) => Selection::caret(c),
            // `«…»`: anchor at the opening mark, caret at the closing one; `»…«` is reversed.
            (None, Some(open), Some(close)) => Selection::new(open, close),
            _ => panic!("marked text {marked:?} needs one caret or one selection"),
        };
        (text, selection)
    }

    /// Inverse of [`parse_marked`].
    pub fn render_marked(text: &str, sel: Selection) -> String {
        let mut out = String::new();
        let (a, b) = (sel.start(), sel.end());
        for (i, ch) in text.char_indices() {
            push_markers(&mut out, i, sel, a, b);
            out.push(ch);
        }
        push_markers(&mut out, text.len(), sel, a, b);
        out
    }

    fn push_markers(out: &mut String, at: usize, sel: Selection, a: usize, b: usize) {
        if sel.is_caret() {
            if at == a {
                out.push(CARET);
            }
            return;
        }
        let (first, second) = if sel.is_reversed() {
            ('»', '«')
        } else {
            ('«', '»')
        };
        if at == a {
            out.push(first);
        }
        if at == b {
            out.push(second);
        }
    }

    /// Runs `op` on the marked document `before` and returns the marked result, checking that
    /// the transaction records the original selection and that its inverse restores the text.
    pub fn run(before: &str, op: impl FnOnce(Ctx<'_>) -> Option<Transaction>) -> Option<String> {
        let (text, selection) = parse_marked(before);
        let mut buffer = Buffer::from_text(&text);
        let ast = parse_document(&text);
        let tx = op(Ctx::new(&buffer, &ast, selection))?;
        assert_eq!(
            tx.selection_before, selection,
            "selection_before of {before:?}"
        );
        let inverse = tx.apply(&mut buffer).expect("transaction applies");
        let result = render_marked(&buffer.to_string(), tx.selection_after);
        let mut restored = buffer.clone();
        inverse.apply(&mut restored).expect("inverse applies");
        assert_eq!(restored.to_string(), text, "undo of {before:?}");
        Some(result)
    }

    /// Asserts `op` turns `before` into `after`.
    pub fn check(before: &str, after: &str, op: impl FnOnce(Ctx<'_>) -> Option<Transaction>) {
        assert_eq!(run(before, op).as_deref(), Some(after), "from {before:?}");
    }

    /// Asserts `op` does not apply to `before`.
    pub fn check_none(before: &str, op: impl FnOnce(Ctx<'_>) -> Option<Transaction>) {
        assert_eq!(run(before, op), None, "from {before:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::markdown::parse_document;

    #[test]
    fn marked_text_round_trips() {
        for marked in ["a‸b", "‸", "«ab»c", "a»bc«", "x‸\ny"] {
            let (text, sel) = parse_marked(marked);
            assert_eq!(render_marked(&text, sel), marked);
        }
        let (text, sel) = parse_marked("a«bc»d");
        assert_eq!((text.as_str(), sel), ("abcd", Selection::new(1, 3)));
        let (_, sel) = parse_marked("a»bc«d");
        assert_eq!(sel, Selection::new(3, 1));
        let (text, sel) = parse_marked("İ‸ı");
        assert_eq!((text.as_str(), sel), ("İı", Selection::caret(2)));
    }

    #[test]
    #[should_panic(expected = "needs one caret or one selection")]
    fn marked_text_requires_a_marker() {
        parse_marked("plain");
    }

    #[test]
    fn container_prefix_lengths() {
        for (line, len) in [
            ("text", 0),
            ("  text", 2),
            ("- a", 2),
            ("* a", 2),
            ("+ a", 2),
            ("1. a", 3),
            ("12) a", 4),
            ("  - a", 4),
            ("- [ ] a", 6),
            ("- [x] a", 6),
            ("- [X] a", 6),
            ("> a", 2),
            (">a", 1),
            ("> > a", 4),
            ("> - a", 4),
            ("- - a", 4),
            ("> - [ ] a", 8),
            ("-", 1),
            ("-a", 0),
            ("*emph*", 0),
            ("1.a", 0),
            ("1234567890. a", 0),
            ("1 a", 0),
            ("- [y] a", 2),
            ("- [ ]a", 2),
            ("\t- a", 3),
            ("-\t[ ] a", 6),
        ] {
            assert_eq!(container_prefix_len(line), len, "{line:?}");
        }
    }

    #[test]
    fn marker_and_task_box_scanners() {
        assert_eq!(list_marker_len("- x"), Some(1));
        assert_eq!(list_marker_len("1) x"), Some(2));
        assert_eq!(list_marker_len("1.x"), None);
        assert_eq!(list_marker_len("x"), None);
        assert_eq!(list_marker_len(""), None);
        assert_eq!(task_box_len("[ ] x"), Some(3));
        assert_eq!(task_box_len("[x]"), Some(3));
        assert_eq!(task_box_len("[xx] x"), None);
        assert_eq!(task_box_len("[ ]x"), None);
        assert_eq!(task_box_len("[ "), None);
    }

    fn ctx_for(marked: &str, f: impl FnOnce(&Ctx<'_>)) {
        let (text, selection) = parse_marked(marked);
        let buffer = Buffer::from_text(&text);
        let ast = parse_document(&text);
        f(&Ctx::new(&buffer, &ast, selection));
    }

    #[test]
    fn selected_lines_exclude_a_line_only_touched_at_its_start() {
        ctx_for("«a\nb\n»c", |c| assert_eq!(c.selected_lines(), (0, 1)));
        ctx_for("«a\nb»\nc", |c| assert_eq!(c.selected_lines(), (0, 1)));
        ctx_for("a\n‸\nc", |c| assert_eq!(c.selected_lines(), (1, 1)));
        ctx_for("a\nb\n‸c", |c| assert_eq!(c.selected_lines(), (2, 2)));
        ctx_for("«a\n»b", |c| assert_eq!(c.selected_lines(), (0, 0)));
        ctx_for("»a\nb«", |c| assert_eq!(c.selected_lines(), (0, 1)));
    }

    #[test]
    fn line_helpers() {
        ctx_for("- [ ] a\n  \n> x‸", |c| {
            assert_eq!(c.line_of(9), 1);
            assert_eq!(c.line_text(0), "- [ ] a");
            assert_eq!(c.line_range(0), TextRange::new(0, 7));
            assert_eq!(c.content_start(0), 6);
            assert_eq!(c.content_start(2), 13);
            assert!(!c.is_blank_line(0));
            assert!(c.is_blank_line(1));
            assert_eq!(c.slice(TextRange::new(2, 5)), "[ ]");
        });
    }

    #[test]
    fn verbatim_blocks_are_detected() {
        ctx_for("```\nco‸de\n```\n\npara", |c| {
            assert!(c.in_verbatim_block(c.selection.head));
            assert!(!c.in_verbatim_block(c.buffer.len()));
        });
        ctx_for("$$\nx\n$$\n\n<div>\nh\n</div>\n‸", |c| {
            assert!(c.in_verbatim_block(4));
            assert!(c.in_verbatim_block(16));
        });
        ctx_for("---\na: 1\n---\ntext‸", |c| {
            assert!(c.in_verbatim_block(6));
            assert!(!c.in_verbatim_block(15));
        });
        ctx_for("# h‸\n\ntext", |c| {
            assert!(!c.in_verbatim_block(c.selection.head))
        });
    }

    #[test]
    fn transactions_record_selection() {
        ctx_for("ab«c»d", |c| {
            let tx = c.transaction_mapped(vec![Edit::insert(0, "XY")]);
            assert_eq!(tx.selection_before, Selection::new(2, 3));
            assert_eq!(tx.selection_after, Selection::new(4, 5));
            let tx = c.transaction(vec![], Selection::caret(1));
            assert_eq!(tx.selection_after, Selection::caret(1));
        });
    }
}
