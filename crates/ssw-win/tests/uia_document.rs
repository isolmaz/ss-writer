//! The Text pattern's document model: UTF-8 ⇄ UTF-16 conversion and unit navigation over a
//! text source. Pure logic, no UI Automation client involved.

use std::ops::Range;

use ssw_ui::geometry::{Point, Rect};
use ssw_win::uia::TextSource;
use ssw_win::uia::text_pattern::{Document, Unit};
use windows::Win32::UI::Accessibility::{
    TextUnit_Character, TextUnit_Document, TextUnit_Format, TextUnit_Line, TextUnit_Page,
    TextUnit_Paragraph, TextUnit_Word,
};

/// A source whose visual lines are `width` bytes wide; only the line geometry matters here.
struct Lines {
    width: usize,
}

impl TextSource for Lines {
    fn text(&self) -> String {
        String::new()
    }

    fn bounding_rects(&self, _range: Range<usize>) -> Vec<Rect> {
        Vec::new()
    }

    fn offset_from_point(&self, _point: Point) -> Option<usize> {
        None
    }

    fn line_range(&self, offset: usize) -> Range<usize> {
        let start = offset / self.width * self.width;
        start..start + self.width
    }

    fn select(&self, _range: Range<usize>) {}

    fn scroll_into_view(&self, _range: Range<usize>, _align_to_top: bool) {}
}

#[test]
fn uia_document_converts_between_utf8_and_utf16() {
    // 'a', 'é' (2 bytes), '😀' (4 bytes, 2 units), 'b'.
    let document = Document::new("a\u{e9}\u{1F600}b".to_owned());
    assert_eq!(document.len16(), 5);
    assert_eq!(document.to16(0), 0);
    assert_eq!(document.to16(1), 1);
    assert_eq!(document.to16(3), 2);
    assert_eq!(document.to16(7), 4);
    assert_eq!(document.to16(8), 5);
    // A byte offset inside a char rounds down.
    assert_eq!(document.to16(2), 1);
    assert_eq!(document.to_byte(0), 0);
    assert_eq!(document.to_byte(2), 3);
    // Inside the surrogate pair rounds down to the char.
    assert_eq!(document.to_byte(3), 3);
    assert_eq!(document.to_byte(4), 7);
    assert_eq!(document.to_byte(99), 8);
    assert_eq!(document.slice16(1..4), "\u{e9}\u{1F600}");
    // A reversed range (end before start) is empty, not a panic.
    let (start, end) = (4, 1);
    assert_eq!(document.slice16(Range { start, end }), "");
}

#[test]
fn uia_document_enclosing_units() {
    let document = Document::new("Hello, world.\nSecond line".to_owned());
    let source = Lines { width: 8 };
    let table = [
        (Unit::Character, 1..1, 1..2),
        (Unit::Character, 1..3, 1..3),
        (Unit::Word, 1..1, 0..5),
        (Unit::Word, 5..5, 5..7),
        // Punctuation is its own word: "world" ends where "." starts, and the "." word
        // takes the line break up to the next word.
        (Unit::Word, 7..7, 7..12),
        (Unit::Word, 12..12, 12..14),
        (Unit::Paragraph, 3..3, 0..14),
        (Unit::Paragraph, 20..20, 14..25),
        (Unit::Document, 3..4, 0..25),
        (Unit::Line, 3..3, 0..8),
        (Unit::Line, 9..9, 8..16),
    ];
    for (unit, range, expected) in table {
        assert_eq!(
            document.enclosing(unit, &source, range.clone()),
            expected,
            "{unit:?} {range:?}"
        );
    }
    // The empty document has one empty unit.
    let empty = Document::new(String::new());
    assert_eq!(empty.enclosing(Unit::Word, &source, 0..0), 0..0);
    assert_eq!(empty.step(Unit::Word, &source, 0, 3), (0, 0));
}

#[test]
fn uia_document_steps_by_units() {
    let document = Document::new("ab cd\nef".to_owned());
    let source = Lines { width: 8 };
    assert_eq!(document.step(Unit::Character, &source, 0, 3), (3, 3));
    assert_eq!(document.step(Unit::Character, &source, 3, -2), (1, -2));
    assert_eq!(document.step(Unit::Character, &source, 6, 10), (8, 2));
    assert_eq!(document.step(Unit::Character, &source, 2, -10), (0, -2));
    assert_eq!(document.step(Unit::Word, &source, 0, 1), (3, 1));
    assert_eq!(document.step(Unit::Word, &source, 0, 2), (6, 2));
    assert_eq!(document.step(Unit::Paragraph, &source, 0, 1), (6, 1));
    assert_eq!(document.step(Unit::Paragraph, &source, 8, -1), (6, -1));
    assert_eq!(document.step(Unit::Document, &source, 0, 1), (8, 1));
    assert_eq!(document.step(Unit::Line, &source, 0, 1), (8, 1));
    assert_eq!(document.step(Unit::Character, &source, 2, 0), (2, 0));
}

#[test]
fn uia_a_source_whose_lines_do_not_advance_makes_the_rest_one_line() {
    // A zero-width line range never reaches the end of the text: stop at the first start
    // instead of looping, and treat everything as one line.
    let document = Document::new("abc\ndef".to_owned());
    let source = Lines { width: 1 };
    // Sanity check of the fixture: one-byte lines advance and split the text per byte.
    assert_eq!(document.enclosing(Unit::Line, &source, 2..2), 2..3);
    let stuck = Stuck;
    assert_eq!(document.enclosing(Unit::Line, &stuck, 1..1), 0..7);
    assert_eq!(document.step(Unit::Line, &stuck, 5, -1), (0, -1));
}

/// A source whose line range never contains the offset it was asked about.
struct Stuck;

impl TextSource for Stuck {
    fn text(&self) -> String {
        String::new()
    }

    fn bounding_rects(&self, _range: Range<usize>) -> Vec<Rect> {
        Vec::new()
    }

    fn offset_from_point(&self, _point: Point) -> Option<usize> {
        None
    }

    fn line_range(&self, offset: usize) -> Range<usize> {
        offset..offset
    }

    fn select(&self, _range: Range<usize>) {}

    fn scroll_into_view(&self, _range: Range<usize>, _align_to_top: bool) {}
}

#[test]
fn uia_unit_mapping() {
    assert_eq!(Unit::from_uia(TextUnit_Character), Unit::Character);
    assert_eq!(Unit::from_uia(TextUnit_Word), Unit::Word);
    assert_eq!(Unit::from_uia(TextUnit_Line), Unit::Line);
    assert_eq!(Unit::from_uia(TextUnit_Paragraph), Unit::Paragraph);
    // Pages and formats have no meaning for the editor: they are the whole document.
    assert_eq!(Unit::from_uia(TextUnit_Page), Unit::Document);
    assert_eq!(Unit::from_uia(TextUnit_Format), Unit::Document);
    assert_eq!(Unit::from_uia(TextUnit_Document), Unit::Document);
}
