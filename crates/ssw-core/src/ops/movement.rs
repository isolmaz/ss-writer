//! Caret movement and selection extension over the text buffer.
//!
//! Positions are byte offsets. Words follow Unicode word segmentation. Which source offsets a
//! caret may occupy next to hidden markers is a view concern and not decided here.

use crate::edit::Selection;
use crate::text::{Buffer, TextRange};

/// A caret movement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Movement {
    /// Left arrow: one grapheme back.
    CharBackward,
    /// Right arrow: one grapheme forward.
    CharForward,
    /// Ctrl+Left: start of the previous word.
    WordBackward,
    /// Ctrl+Right: start of the next word.
    WordForward,
    /// Home: start of the line.
    LineStart,
    /// End: end of the line.
    LineEnd,
    /// Ctrl+Up: start of the paragraph, or of the previous one when already there.
    ParagraphBackward,
    /// Ctrl+Down: start of the next paragraph (the document end after the last one).
    ParagraphForward,
    /// Ctrl+Home.
    DocumentStart,
    /// Ctrl+End.
    DocumentEnd,
}

fn is_blank(buffer: &Buffer, line: usize) -> bool {
    buffer.slice(buffer.line_range(line)).trim().is_empty()
}

/// First line of the paragraph (run of non-blank lines) containing `line`.
fn paragraph_first_line(buffer: &Buffer, mut line: usize) -> usize {
    while line > 0 && !is_blank(buffer, line - 1) {
        line -= 1;
    }
    line
}

/// Last line of the paragraph (run of non-blank lines) containing `line`.
fn paragraph_last_line(buffer: &Buffer, mut line: usize) -> usize {
    while line + 1 < buffer.line_count() && !is_blank(buffer, line + 1) {
        line += 1;
    }
    line
}

/// The offset `movement` leads to from `from`.
pub fn target(buffer: &Buffer, from: usize, movement: Movement) -> usize {
    let from = from.min(buffer.len());
    let line = buffer.line_of(from);
    match movement {
        Movement::CharBackward => buffer.prev_grapheme_boundary(from),
        Movement::CharForward => buffer.next_grapheme_boundary(from),
        Movement::WordBackward => buffer.prev_word_boundary(from),
        Movement::WordForward => buffer.next_word_boundary(from),
        Movement::LineStart => buffer.line_range(line).start,
        Movement::LineEnd => buffer.line_range(line).end,
        Movement::DocumentStart => 0,
        Movement::DocumentEnd => buffer.len(),
        Movement::ParagraphBackward => {
            if from > buffer.line_range(line).start && !is_blank(buffer, line) {
                return buffer.line_range(paragraph_first_line(buffer, line)).start;
            }
            let mut l = line;
            while l > 0 {
                l -= 1;
                if !is_blank(buffer, l) {
                    return buffer.line_range(paragraph_first_line(buffer, l)).start;
                }
            }
            0
        }
        Movement::ParagraphForward => {
            let mut l = paragraph_last_line(buffer, line);
            if is_blank(buffer, line) {
                l = line;
            }
            while l + 1 < buffer.line_count() {
                l += 1;
                if !is_blank(buffer, l) {
                    return buffer.line_range(l).start;
                }
            }
            buffer.len()
        }
    }
}

/// The selection after `movement`. Without `extend`, a range collapses first: left/right arrows
/// go to its start/end, every other movement starts from the caret end. With `extend` the
/// caret end moves and the anchor stays.
pub fn move_selection(
    buffer: &Buffer,
    selection: Selection,
    movement: Movement,
    extend: bool,
) -> Selection {
    if extend {
        return selection.with_head(target(buffer, selection.head, movement));
    }
    if !selection.is_caret() {
        match movement {
            Movement::CharBackward => return Selection::caret(selection.start()),
            Movement::CharForward => return Selection::caret(selection.end()),
            _ => {}
        }
    }
    Selection::caret(target(buffer, selection.head, movement))
}

/// The word (or whitespace / punctuation run) at `offset`: double-click selection.
pub fn select_word(buffer: &Buffer, offset: usize) -> TextRange {
    buffer.word_range_at(offset)
}

/// The paragraph at `offset`: the run of non-blank lines, without the last line break; a blank
/// line selects itself. Triple-click selection.
pub fn select_paragraph(buffer: &Buffer, offset: usize) -> TextRange {
    let line = buffer.line_of(offset.min(buffer.len()));
    if is_blank(buffer, line) {
        return buffer.line_range(line);
    }
    TextRange::new(
        buffer.line_range(paragraph_first_line(buffer, line)).start,
        buffer.line_range(paragraph_last_line(buffer, line)).end,
    )
}

/// The whole document.
pub fn select_all(buffer: &Buffer) -> Selection {
    Selection::new(0, buffer.len())
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    fn moved(before: &str, movement: Movement, extend: bool) -> String {
        let (text, selection) = parse_marked(before);
        let buffer = Buffer::from_text(&text);
        render_marked(&text, move_selection(&buffer, selection, movement, extend))
    }

    #[test]
    fn character_movement_uses_graphemes() {
        for (before, movement, after) in [
            ("a‸bc", Movement::CharForward, "ab‸c"),
            ("a‸bc", Movement::CharBackward, "‸abc"),
            ("‸abc", Movement::CharBackward, "‸abc"),
            ("abc‸", Movement::CharForward, "abc‸"),
            ("İ‸ı", Movement::CharBackward, "‸İı"),
            ("a‸👨‍👩‍👧b", Movement::CharForward, "a👨‍👩‍👧‸b"),
            ("e\u{301}‸x", Movement::CharBackward, "‸e\u{301}x"),
            ("a‸\nb", Movement::CharForward, "a\n‸b"),
            ("a\n‸b", Movement::CharBackward, "a‸\nb"),
            ("a‸\r\nb", Movement::CharForward, "a\r\n‸b"),
        ] {
            assert_eq!(moved(before, movement, false), after, "{before:?}");
        }
    }

    #[test]
    fn arrows_collapse_a_selection_to_its_edges() {
        assert_eq!(moved("a«bc»d", Movement::CharBackward, false), "a‸bcd");
        assert_eq!(moved("a«bc»d", Movement::CharForward, false), "abc‸d");
        assert_eq!(moved("a»bc«d", Movement::CharBackward, false), "a‸bcd");
        assert_eq!(moved("a»bc«d", Movement::CharForward, false), "abc‸d");
        // Other movements start from the caret end.
        assert_eq!(moved("a«bc»d", Movement::LineEnd, false), "abcd‸");
        assert_eq!(moved("a»bc«d", Movement::LineStart, false), "‸abcd");
    }

    #[test]
    fn extending_keeps_the_anchor() {
        for (before, movement, after) in [
            ("a‸bc", Movement::CharForward, "a«b»c"),
            ("ab‸c", Movement::CharBackward, "a»b«c"),
            ("a«b»c", Movement::CharForward, "a«bc»"),
            ("a«b»c", Movement::CharBackward, "a‸bc"),
            ("a»b«c", Movement::DocumentStart, "»ab«c"),
            ("a‸b c", Movement::WordForward, "a«b »c"),
            ("a‸bc", Movement::DocumentEnd, "a«bc»"),
        ] {
            assert_eq!(moved(before, movement, true), after, "{before:?}");
        }
    }

    #[test]
    fn word_movement_uses_unicode_segmentation() {
        for (before, movement, after) in [
            (
                "hello ‸world foo",
                Movement::WordForward,
                "hello world ‸foo",
            ),
            ("hello world‸", Movement::WordBackward, "hello ‸world"),
            ("merhaba dünya‸", Movement::WordBackward, "merhaba ‸dünya"),
            ("‸ışık güneş", Movement::WordForward, "ışık ‸güneş"),
            ("a‸", Movement::WordForward, "a‸"),
            ("‸a", Movement::WordBackward, "‸a"),
            ("a\n‸b", Movement::WordBackward, "‸a\nb"),
            ("a‸\nb c", Movement::WordForward, "a\n‸b c"),
        ] {
            assert_eq!(moved(before, movement, false), after, "{before:?}");
        }
    }

    #[test]
    fn line_and_document_movement() {
        for (before, movement, after) in [
            ("ab\ncd‸e", Movement::LineStart, "ab\n‸cde"),
            ("ab\nc‸de\nf", Movement::LineEnd, "ab\ncde‸\nf"),
            ("ab\r\nc‸d", Movement::LineStart, "ab\r\n‸cd"),
            ("a‸b\r\ncd", Movement::LineEnd, "ab‸\r\ncd"),
            ("a\nb‸c", Movement::DocumentStart, "‸a\nbc"),
            ("a‸b\nc", Movement::DocumentEnd, "ab\nc‸"),
            ("‸", Movement::LineEnd, "‸"),
        ] {
            assert_eq!(moved(before, movement, false), after, "{before:?}");
        }
    }

    #[test]
    fn paragraph_movement() {
        let cases = [
            // Backward: to the paragraph start, then to the previous paragraph's start.
            (
                "p1a\np1b\n\np2‸\n\n\np3",
                Movement::ParagraphBackward,
                "p1a\np1b\n\n‸p2\n\n\np3",
            ),
            (
                "p1a\np1b\n\n‸p2\n\n\np3",
                Movement::ParagraphBackward,
                "‸p1a\np1b\n\np2\n\n\np3",
            ),
            (
                "p1a\np1b\n\np2\n\n‸\np3",
                Movement::ParagraphBackward,
                "p1a\np1b\n\n‸p2\n\n\np3",
            ),
            (
                "p1a\n‸p1b\n\np2\n\n\np3",
                Movement::ParagraphBackward,
                "‸p1a\np1b\n\np2\n\n\np3",
            ),
            ("‸p1a", Movement::ParagraphBackward, "‸p1a"),
            ("‸\n\nx", Movement::ParagraphBackward, "‸\n\nx"),
            ("\n\n‸x", Movement::ParagraphBackward, "‸\n\nx"),
            // Forward: to the next paragraph start; the document end after the last.
            (
                "p‸1a\np1b\n\np2\n\n\np3",
                Movement::ParagraphForward,
                "p1a\np1b\n\n‸p2\n\n\np3",
            ),
            (
                "p1a\np1b\n\np‸2\n\n\np3",
                Movement::ParagraphForward,
                "p1a\np1b\n\np2\n\n\n‸p3",
            ),
            (
                "p1a\np1b\n\np2\n\n\np‸3",
                Movement::ParagraphForward,
                "p1a\np1b\n\np2\n\n\np3‸",
            ),
            (
                "p1a\np1b\n‸\np2\n\n\np3",
                Movement::ParagraphForward,
                "p1a\np1b\n\n‸p2\n\n\np3",
            ),
            ("a‸\n\n", Movement::ParagraphForward, "a\n\n‸"),
        ];
        for (before, movement, after) in cases {
            assert_eq!(moved(before, movement, false), after, "{before:?}");
        }
    }

    #[test]
    fn selection_helpers() {
        let buffer = Buffer::from_text("hello wörld\n\nsecond\nparagraph\n");
        assert_eq!(select_word(&buffer, 9), TextRange::new(6, 12));
        assert_eq!(select_word(&buffer, 5), TextRange::new(5, 6));
        assert_eq!(select_paragraph(&buffer, 3), TextRange::new(0, 12));
        assert_eq!(select_paragraph(&buffer, 13), TextRange::new(13, 13));
        assert_eq!(select_paragraph(&buffer, 20), TextRange::new(14, 30));
        assert_eq!(select_paragraph(&buffer, 999), TextRange::new(31, 31));
        assert_eq!(select_all(&buffer), Selection::new(0, buffer.len()));
    }

    #[test]
    fn target_clamps_offsets_past_the_end() {
        let buffer = Buffer::from_text("ab");
        assert_eq!(target(&buffer, 99, Movement::CharBackward), 1);
    }
}
