//! Text primitives: byte-offset ranges, the rope-backed [`Buffer`] and UTF-16 conversion.
//!
//! Every position in this module is a UTF-8 byte offset on a char boundary. Rope char
//! indices never leave this module; UTF-16 offsets exist only inside [`Utf16Map`], which is
//! used at the DirectWrite/TSF boundary.

use std::borrow::Cow;
use std::fmt;
use std::ops::Range;

use ropey::Rope;
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

/// Half-open range of UTF-8 byte offsets. Both ends lie on char boundaries of the text the
/// range refers to, and `start <= end`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

impl TextRange {
    /// Range `[start, end)`. Callers uphold `start <= end`.
    pub const fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end);
        Self { start, end }
    }

    /// Empty range at `offset`.
    pub const fn at(offset: usize) -> Self {
        Self {
            start: offset,
            end: offset,
        }
    }

    pub const fn len(self) -> usize {
        self.end - self.start
    }

    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// `start <= offset < end`.
    pub const fn contains(self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }

    /// `start <= offset <= end` (inclusive bounds: a caret at either edge touches the range).
    pub const fn touches(self, offset: usize) -> bool {
        self.start <= offset && offset <= self.end
    }

    /// True when `other` lies completely inside `self` (an empty `other` at an edge counts).
    pub const fn contains_range(self, other: TextRange) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    /// True when the ranges share at least one byte.
    pub const fn intersects(self, other: TextRange) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Shared part of two ranges, if they share at least one byte.
    pub fn intersection(self, other: TextRange) -> Option<TextRange> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        (start < end).then_some(TextRange { start, end })
    }

    /// Smallest range containing both.
    pub fn cover(self, other: TextRange) -> TextRange {
        TextRange {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// The same range moved by `delta` bytes; `None` when it would leave `usize`.
    pub fn shifted(self, delta: isize) -> Option<TextRange> {
        Some(TextRange {
            start: self.start.checked_add_signed(delta)?,
            end: self.end.checked_add_signed(delta)?,
        })
    }

    pub const fn as_range(self) -> Range<usize> {
        self.start..self.end
    }

    /// The addressed part of `text`; `None` when out of bounds or not on char boundaries.
    pub fn get(self, text: &str) -> Option<&str> {
        text.get(self.start..self.end)
    }
}

impl From<Range<usize>> for TextRange {
    fn from(r: Range<usize>) -> Self {
        TextRange::new(r.start, r.end)
    }
}

impl From<TextRange> for Range<usize> {
    fn from(r: TextRange) -> Self {
        r.start..r.end
    }
}

/// A byte offset or range that cannot be applied to a text.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TextError {
    #[error("offset {offset} is past the end of the text (length {len})")]
    OutOfBounds { offset: usize, len: usize },
    #[error("offset {offset} is not on a UTF-8 character boundary")]
    NotCharBoundary { offset: usize },
    #[error("range start {start} is after its end {end}")]
    InvertedRange { start: usize, end: usize },
}

/// Document text. Stored as a rope; the API only speaks UTF-8 byte offsets.
///
/// Lines end at `\n` (a preceding `\r` belongs to the terminator). Text is kept with `\n`
/// endings internally (spec 4.2), so no other Unicode line separator is recognized.
#[derive(Clone, Debug, Default)]
pub struct Buffer {
    rope: Rope,
}

impl Buffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_text(text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
        }
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn is_empty(&self) -> bool {
        self.rope.len_bytes() == 0
    }

    /// Number of lines. An empty buffer has one (empty) line; a trailing `\n` starts a new
    /// empty last line.
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// True when `offset` is `len()` or the start of a UTF-8 sequence.
    pub fn is_char_boundary(&self, offset: usize) -> bool {
        let len = self.len();
        offset == len || (offset < len && self.rope.byte(offset) & 0xC0 != 0x80)
    }

    /// Validates `range`: ordered, inside the buffer and on char boundaries.
    pub fn check_range(&self, range: TextRange) -> Result<(), TextError> {
        if range.start > range.end {
            return Err(TextError::InvertedRange {
                start: range.start,
                end: range.end,
            });
        }
        self.check_offset(range.start)?;
        self.check_offset(range.end)
    }

    /// Validates a single offset.
    pub fn check_offset(&self, offset: usize) -> Result<(), TextError> {
        let len = self.len();
        if offset > len {
            return Err(TextError::OutOfBounds { offset, len });
        }
        if !self.is_char_boundary(offset) {
            return Err(TextError::NotCharBoundary { offset });
        }
        Ok(())
    }

    /// Largest char boundary `<= offset` (clamped to `len()`).
    pub fn floor_char_boundary(&self, offset: usize) -> usize {
        let mut o = offset.min(self.len());
        while o > 0 && !self.is_char_boundary(o) {
            o -= 1;
        }
        o
    }

    /// Smallest char boundary `>= offset` (clamped to `len()`).
    pub fn ceil_char_boundary(&self, offset: usize) -> usize {
        let len = self.len();
        let mut o = offset.min(len);
        while o < len && !self.is_char_boundary(o) {
            o += 1;
        }
        o
    }

    /// The text in `range`. Borrowed when the range lies inside one rope chunk.
    ///
    /// Out-of-bounds ends are clamped to the buffer and ends inside a character are moved
    /// outwards to the nearest boundary, so a stale range can never panic the UI. Use
    /// [`Buffer::check_range`] to detect invalid ranges.
    pub fn slice(&self, range: TextRange) -> Cow<'_, str> {
        let start = self.floor_char_boundary(range.start);
        let end = self.ceil_char_boundary(range.end).max(start);
        // `start <= end <= len` and both ends are char boundaries, so the slice always exists.
        let slice = self.rope.byte_slice(start..end);
        match slice.as_str() {
            Some(s) => Cow::Borrowed(s),
            None => Cow::Owned(slice.to_string()),
        }
    }

    /// The character starting at `offset`, if any.
    pub fn char_at(&self, offset: usize) -> Option<char> {
        if offset >= self.len() || !self.is_char_boundary(offset) {
            return None;
        }
        Some(self.rope.char(self.rope.byte_to_char(offset)))
    }

    /// Line index containing `offset` (clamped to the last line).
    pub fn line_of(&self, offset: usize) -> usize {
        self.rope.byte_to_line(offset.min(self.len()))
    }

    /// Content range of `line` without its terminator (`\n` or `\r\n`). Lines past the end
    /// resolve to the last line.
    pub fn line_range(&self, line: usize) -> TextRange {
        let full = self.line_range_with_terminator(line);
        let mut end = full.end;
        if end > full.start && self.rope.byte(end - 1) == b'\n' {
            end -= 1;
            if end > full.start && self.rope.byte(end - 1) == b'\r' {
                end -= 1;
            }
        }
        TextRange::new(full.start, end)
    }

    /// Range of `line` including its terminator.
    pub fn line_range_with_terminator(&self, line: usize) -> TextRange {
        let line = line.min(self.rope.len_lines() - 1);
        let start = self.rope.line_to_byte(line);
        let end = if line + 1 < self.rope.len_lines() {
            self.rope.line_to_byte(line + 1)
        } else {
            self.len()
        };
        TextRange::new(start, end)
    }

    /// Inserts `text` at `offset`.
    pub fn insert(&mut self, offset: usize, text: &str) -> Result<(), TextError> {
        self.check_offset(offset)?;
        let char_idx = self.rope.byte_to_char(offset);
        self.rope.insert(char_idx, text);
        Ok(())
    }

    /// Removes `range`.
    pub fn delete(&mut self, range: TextRange) -> Result<(), TextError> {
        self.check_range(range)?;
        let start = self.rope.byte_to_char(range.start);
        let end = self.rope.byte_to_char(range.end);
        self.rope.remove(start..end);
        Ok(())
    }

    /// Replaces `range` with `text`. The buffer is untouched when `range` is invalid.
    pub fn replace(&mut self, range: TextRange, text: &str) -> Result<(), TextError> {
        self.check_range(range)?;
        let start = self.rope.byte_to_char(range.start);
        let end = self.rope.byte_to_char(range.end);
        self.rope.remove(start..end);
        self.rope.insert(start, text);
        Ok(())
    }

    /// All text as consecutive chunks.
    pub fn chunks(&self) -> impl Iterator<Item = &str> + '_ {
        self.rope.chunks()
    }

    /// The text of `range` as consecutive chunks (same clamping as [`Buffer::slice`]).
    pub fn chunks_in(&self, range: TextRange) -> impl Iterator<Item = &str> + '_ {
        let start = self.floor_char_boundary(range.start);
        let end = self.ceil_char_boundary(range.end).max(start);
        self.rope
            .get_byte_slice(start..end)
            .into_iter()
            .flat_map(|s| s.chunks())
    }

    /// Start of the next grapheme cluster after `offset`; crosses line terminators.
    pub fn next_grapheme_boundary(&self, offset: usize) -> usize {
        let offset = offset.min(self.len());
        let line = self.line_of(offset);
        let content = self.line_range(line);
        if offset >= content.end {
            return self.line_range_with_terminator(line).end.max(offset);
        }
        let text = self.slice(content);
        let rel = offset - content.start;
        // `rel < text.len()`, so the last cluster always ends past `rel`.
        let end = text
            .grapheme_indices(true)
            .map(|(i, g)| i + g.len())
            .find(|&end| end > rel)
            .unwrap_or(text.len());
        content.start + end
    }

    /// Start of the grapheme cluster ending at or before `offset`; crosses line terminators.
    pub fn prev_grapheme_boundary(&self, offset: usize) -> usize {
        let offset = offset.min(self.len());
        if offset == 0 {
            return 0;
        }
        let line = self.line_of(offset);
        let content = self.line_range(line);
        if offset <= content.start {
            // `offset > 0` and line 0 starts at 0, so there is a previous line.
            return self.line_range(line - 1).end;
        }
        if offset > content.end {
            return content.end;
        }
        let text = self.slice(TextRange::new(content.start, offset));
        let mut prev = 0;
        for (i, _) in text.grapheme_indices(true) {
            prev = i;
        }
        content.start + prev
    }

    /// The Unicode word-boundary segment (word, whitespace run or punctuation) containing
    /// `offset`, on its line. At a line end the last segment of the line is returned; an
    /// empty line yields an empty range.
    pub fn word_range_at(&self, offset: usize) -> TextRange {
        let offset = offset.min(self.len());
        let content = self.line_range(self.line_of(offset));
        let text = self.slice(content);
        let rel = offset.saturating_sub(content.start).min(text.len());
        let mut last = TextRange::at(content.start);
        for (i, w) in text.split_word_bound_indices() {
            let seg = TextRange::new(content.start + i, content.start + i + w.len());
            if i <= rel && rel < i + w.len() {
                return seg;
            }
            last = seg;
        }
        last
    }

    /// Start of the next word after `offset` (Ctrl+Right). Stops at the end of an empty line.
    pub fn next_word_boundary(&self, offset: usize) -> usize {
        let len = self.len();
        let offset = offset.min(len);
        let line = self.line_of(offset);
        let content = self.line_range(line);
        let text = self.slice(content);
        let rel = offset.saturating_sub(content.start);
        if offset < content.end {
            if let Some(start) = word_starts(&text).into_iter().find(|&s| s > rel) {
                return content.start + start;
            }
            return content.end;
        }
        let next_line = self.line_range_with_terminator(line).end;
        if next_line <= offset {
            return len;
        }
        let next = self.line_range(self.line_of(next_line));
        let next_text = self.slice(next);
        match word_starts(&next_text).first() {
            Some(&s) => next.start + s,
            None => next.end,
        }
    }

    /// Start of the word before `offset` (Ctrl+Left).
    pub fn prev_word_boundary(&self, offset: usize) -> usize {
        let offset = offset.min(self.len());
        if offset == 0 {
            return 0;
        }
        let line = self.line_of(offset);
        let content = self.line_range(line);
        if offset <= content.start {
            let prev = self.line_range(line - 1);
            let text = self.slice(prev);
            return match word_starts(&text).last() {
                Some(&s) => prev.start + s,
                None => prev.start,
            };
        }
        let text = self.slice(content);
        let rel = (offset - content.start).min(text.len());
        match word_starts(&text).into_iter().rev().find(|&s| s < rel) {
            Some(s) => content.start + s,
            None => content.start,
        }
    }
}

/// Byte offsets (within `line`) where a non-whitespace word-boundary segment starts.
fn word_starts(line: &str) -> Vec<usize> {
    line.split_word_bound_indices()
        .filter(|(_, w)| !w.chars().all(char::is_whitespace))
        .map(|(i, _)| i)
        .collect()
}

impl fmt::Display for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.rope.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

/// One non-ASCII scalar of the mapped string: its extent in both encodings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WideChar {
    byte_start: usize,
    byte_end: usize,
    utf16_start: usize,
    utf16_end: usize,
}

/// Byte offset <-> UTF-16 code unit offset conversion for one block's string.
///
/// ASCII-only strings (the common case) need no table. Otherwise one entry per non-ASCII
/// scalar is stored and both directions are `O(log n)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Utf16Map {
    byte_len: usize,
    utf16_len: usize,
    wide: Vec<WideChar>,
}

impl Utf16Map {
    pub fn new(text: &str) -> Self {
        let mut wide = Vec::new();
        let mut utf16 = 0usize;
        for (i, c) in text.char_indices() {
            let units = c.len_utf16();
            if !c.is_ascii() {
                wide.push(WideChar {
                    byte_start: i,
                    byte_end: i + c.len_utf8(),
                    utf16_start: utf16,
                    utf16_end: utf16 + units,
                });
            }
            utf16 += units;
        }
        Self {
            byte_len: text.len(),
            utf16_len: utf16,
            wide,
        }
    }

    /// Length of the mapped string in bytes.
    pub fn byte_len(&self) -> usize {
        self.byte_len
    }

    /// Length of the mapped string in UTF-16 code units.
    pub fn utf16_len(&self) -> usize {
        self.utf16_len
    }

    /// `(byte, utf16)` of the end of the last scalar that ends at or before `byte`.
    fn base_for_byte(&self, byte: usize) -> (usize, (usize, usize)) {
        let idx = self.wide.partition_point(|w| w.byte_end <= byte);
        match idx.checked_sub(1) {
            Some(i) => (idx, (self.wide[i].byte_end, self.wide[i].utf16_end)),
            None => (idx, (0, 0)),
        }
    }

    /// `(byte, utf16)` of the end of the last scalar that ends at or before `utf16`.
    fn base_for_utf16(&self, utf16: usize) -> (usize, (usize, usize)) {
        let idx = self.wide.partition_point(|w| w.utf16_end <= utf16);
        match idx.checked_sub(1) {
            Some(i) => (idx, (self.wide[i].byte_end, self.wide[i].utf16_end)),
            None => (idx, (0, 0)),
        }
    }

    /// UTF-16 offset of `byte`; `None` when past the end or inside a scalar.
    pub fn byte_to_utf16(&self, byte: usize) -> Option<usize> {
        if byte > self.byte_len {
            return None;
        }
        let (idx, (base_byte, base_utf16)) = self.base_for_byte(byte);
        if self
            .wide
            .get(idx)
            .is_some_and(|next| byte > next.byte_start)
        {
            return None;
        }
        Some(base_utf16 + (byte - base_byte))
    }

    /// Like [`Utf16Map::byte_to_utf16`] but a `byte` inside a scalar maps to the scalar's start
    /// and a `byte` past the end to the end.
    pub fn byte_to_utf16_floor(&self, byte: usize) -> usize {
        let byte = byte.min(self.byte_len);
        let (idx, (base_byte, base_utf16)) = self.base_for_byte(byte);
        let byte = match self.wide.get(idx) {
            Some(next) => byte.min(next.byte_start),
            None => byte,
        };
        base_utf16 + (byte - base_byte)
    }

    /// Byte offset of UTF-16 offset `utf16`; `None` when past the end or between the two
    /// units of a surrogate pair.
    pub fn utf16_to_byte(&self, utf16: usize) -> Option<usize> {
        if utf16 > self.utf16_len {
            return None;
        }
        let (idx, (base_byte, base_utf16)) = self.base_for_utf16(utf16);
        if self
            .wide
            .get(idx)
            .is_some_and(|next| utf16 > next.utf16_start)
        {
            return None;
        }
        Some(base_byte + (utf16 - base_utf16))
    }

    /// Like [`Utf16Map::utf16_to_byte`] but an offset between surrogates maps to the start of
    /// the pair and an offset past the end to the end.
    pub fn utf16_to_byte_floor(&self, utf16: usize) -> usize {
        let utf16 = utf16.min(self.utf16_len);
        let (idx, (base_byte, base_utf16)) = self.base_for_utf16(utf16);
        let utf16 = match self.wide.get(idx) {
            Some(next) => utf16.min(next.utf16_start),
            None => utf16,
        };
        base_byte + (utf16 - base_utf16)
    }

    /// UTF-16 range for a byte range.
    pub fn range_to_utf16(&self, range: TextRange) -> Option<Range<usize>> {
        Some(self.byte_to_utf16(range.start)?..self.byte_to_utf16(range.end)?)
    }

    /// Byte range for a UTF-16 range.
    pub fn range_to_bytes(&self, range: Range<usize>) -> Option<TextRange> {
        let start = self.utf16_to_byte(range.start)?;
        let end = self.utf16_to_byte(range.end)?;
        (start <= end).then_some(TextRange { start, end })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: usize, end: usize) -> TextRange {
        TextRange::new(start, end)
    }

    fn units(start: usize, end: usize) -> Range<usize> {
        start..end
    }

    fn is_borrowed(text: Cow<'_, str>) -> bool {
        matches!(text, Cow::Borrowed(_))
    }

    /// A formatting target that takes `budget` bytes and then refuses every write.
    struct Refusing {
        budget: usize,
        taken: String,
    }

    impl fmt::Write for Refusing {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            if s.len() > self.budget {
                return Err(fmt::Error);
            }
            self.budget -= s.len();
            self.taken.push_str(s);
            Ok(())
        }
    }

    #[test]
    fn displaying_a_buffer_reports_a_target_that_refuses_its_text() {
        use std::fmt::Write as _;

        let buffer = Buffer::from_text("héllo");
        let mut roomy = Refusing {
            budget: 100,
            taken: String::new(),
        };
        assert_eq!(write!(roomy, "{buffer}"), Ok(()));
        assert_eq!(roomy.taken, "héllo");

        let mut full = Refusing {
            budget: 2,
            taken: String::new(),
        };
        assert_eq!(write!(full, "{buffer}"), Err(fmt::Error));
        assert_eq!(full.taken, "");
    }

    #[test]
    fn range_predicates_distinguish_open_and_closed_ends() {
        let range = r(2, 5);
        assert_eq!((range.len(), range.is_empty()), (3, false));
        assert_eq!(
            (TextRange::at(4).len(), TextRange::at(4).is_empty()),
            (0, true)
        );

        assert!(!range.contains(1));
        assert!(range.contains(2));
        assert!(range.contains(4));
        assert!(!range.contains(5));

        assert!(!range.touches(1));
        assert!(range.touches(2));
        assert!(range.touches(5));
        assert!(!range.touches(6));

        assert!(range.contains_range(r(2, 5)));
        assert!(range.contains_range(r(5, 5)));
        assert!(!range.contains_range(r(1, 3)));
        assert!(!range.contains_range(r(4, 6)));

        assert!(range.intersects(r(4, 9)));
        assert!(!range.intersects(r(5, 9)));
        assert!(!range.intersects(r(0, 2)));
    }

    #[test]
    fn range_set_operations_and_conversions() {
        let range = r(2, 5);
        assert_eq!(range.intersection(r(4, 9)), Some(r(4, 5)));
        assert_eq!(range.intersection(r(5, 9)), None);
        assert_eq!(range.cover(r(0, 3)), r(0, 5));
        assert_eq!(range.cover(r(7, 8)), r(2, 8));

        assert_eq!(range.shifted(3), Some(r(5, 8)));
        assert_eq!(range.shifted(-2), Some(r(0, 3)));
        assert_eq!(range.shifted(-3), None, "start would underflow");
        assert_eq!(r(0, usize::MAX).shifted(1), None, "end would overflow");

        assert_eq!(range.as_range(), 2..5);
        assert_eq!(TextRange::from(2..5), range);
        let back: Range<usize> = range.into();
        assert_eq!(back, 2..5);

        assert_eq!(r(0, 1).get("héllo"), Some("h"));
        assert_eq!(r(0, 2).get("héllo"), None, "ends inside é");
        assert_eq!(r(0, 99).get("héllo"), None, "past the end");
    }

    #[test]
    fn empty_buffer_has_one_empty_line() {
        let buffer = Buffer::new();
        assert!(buffer.is_empty());
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.line_count(), 1);
        assert_eq!(buffer.line_range(0), r(0, 0));
        assert_eq!(buffer.to_string(), "");
        assert!(!Buffer::from_text("x").is_empty());
    }

    #[test]
    fn range_and_offset_validation_reports_the_failure_kind() {
        let buffer = Buffer::from_text("aé");
        assert_eq!(buffer.check_range(r(0, 3)), Ok(()));
        assert_eq!(
            buffer.check_range(TextRange { start: 3, end: 1 }),
            Err(TextError::InvertedRange { start: 3, end: 1 })
        );
        assert_eq!(
            buffer.check_range(r(0, 9)),
            Err(TextError::OutOfBounds { offset: 9, len: 3 })
        );
        assert_eq!(
            buffer.check_range(r(2, 3)),
            Err(TextError::NotCharBoundary { offset: 2 })
        );
        assert_eq!(buffer.check_offset(1), Ok(()));
        assert_eq!(buffer.check_offset(3), Ok(()));
        assert_eq!(
            buffer.check_offset(4),
            Err(TextError::OutOfBounds { offset: 4, len: 3 })
        );
        assert_eq!(
            buffer.check_offset(2),
            Err(TextError::NotCharBoundary { offset: 2 })
        );
        assert!(buffer.is_char_boundary(0));
        assert!(buffer.is_char_boundary(3));
        assert!(!buffer.is_char_boundary(2));
        assert!(!buffer.is_char_boundary(4));
    }

    #[test]
    fn char_boundary_clamping_moves_in_both_directions() {
        let buffer = Buffer::from_text("a😀");
        assert_eq!(buffer.len(), 5);
        assert_eq!(buffer.floor_char_boundary(3), 1);
        assert_eq!(buffer.ceil_char_boundary(3), 5);
        assert_eq!(buffer.floor_char_boundary(1), 1);
        assert_eq!(buffer.ceil_char_boundary(1), 1);
        assert_eq!(buffer.floor_char_boundary(99), 5);
        assert_eq!(buffer.ceil_char_boundary(99), 5);
    }

    #[test]
    fn slice_clamps_and_widens_to_char_boundaries() {
        let buffer = Buffer::from_text("héllo");
        assert_eq!(buffer.slice(r(0, 1)), "h");
        assert!(is_borrowed(buffer.slice(r(0, 1))));
        assert_eq!(buffer.slice(r(2, 4)), "él", "start moves back into é");
        assert_eq!(buffer.slice(r(1, 2)), "é", "end moves forward past é");
        assert_eq!(buffer.slice(r(3, 99)), "llo");
        assert_eq!(buffer.slice(r(50, 99)), "");
        assert_eq!(buffer.slice(TextRange { start: 4, end: 2 }), "");
    }

    #[test]
    fn slice_across_rope_chunks_is_owned_and_complete() {
        let text = "x".repeat(8000) + "é" + &"y".repeat(8000);
        let buffer = Buffer::from_text(&text);
        let whole = buffer.slice(r(0, buffer.len()));
        assert!(!is_borrowed(whole.clone()));
        assert_eq!(whole, text);
        let middle = buffer.slice(r(7990, 8020));
        assert_eq!(middle, &text[7990..8020]);
    }

    #[test]
    fn char_at_reads_whole_scalars() {
        let buffer = Buffer::from_text("aé😀");
        assert_eq!(buffer.char_at(0), Some('a'));
        assert_eq!(buffer.char_at(1), Some('é'));
        assert_eq!(buffer.char_at(3), Some('😀'));
        assert_eq!(buffer.char_at(2), None, "inside é");
        assert_eq!(buffer.char_at(7), None, "at the end");
        assert_eq!(buffer.char_at(99), None, "past the end");
    }

    #[test]
    fn lines_end_at_lf_and_crlf_only() {
        let buffer = Buffer::from_text("ab\r\ncd\n");
        assert_eq!(buffer.line_count(), 3);
        assert_eq!(buffer.line_range(0), r(0, 2));
        assert_eq!(buffer.line_range_with_terminator(0), r(0, 4));
        assert_eq!(buffer.line_range(1), r(4, 6));
        assert_eq!(buffer.line_range_with_terminator(1), r(4, 7));
        assert_eq!(buffer.line_range(2), r(7, 7));
        assert_eq!(
            buffer.line_range(99),
            r(7, 7),
            "past the end resolves to the last line"
        );
        assert_eq!(buffer.line_range_with_terminator(99), r(7, 7));
        assert_eq!(
            [0, 2, 3, 4, 7, 100].map(|offset| buffer.line_of(offset)),
            [0, 0, 0, 1, 2, 2]
        );

        let lone_cr = Buffer::from_text("a\rb");
        assert_eq!(lone_cr.line_count(), 1);
        assert_eq!(lone_cr.line_range(0), r(0, 3));
    }

    #[test]
    fn insert_delete_and_replace_validate_before_mutating() {
        let mut buffer = Buffer::from_text("héllo");
        assert_eq!(buffer.insert(3, "XY"), Ok(()));
        assert_eq!(buffer.to_string(), "héXYllo");
        assert_eq!(
            buffer.insert(2, ".."),
            Err(TextError::NotCharBoundary { offset: 2 })
        );
        assert_eq!(
            buffer.insert(9, "x"),
            Err(TextError::OutOfBounds { offset: 9, len: 8 })
        );
        assert_eq!(buffer.to_string(), "héXYllo");

        assert_eq!(buffer.delete(r(3, 5)), Ok(()));
        assert_eq!(buffer.to_string(), "héllo");
        assert_eq!(
            buffer.delete(TextRange { start: 4, end: 2 }),
            Err(TextError::InvertedRange { start: 4, end: 2 })
        );
        assert_eq!(
            buffer.delete(r(0, 99)),
            Err(TextError::OutOfBounds { offset: 99, len: 6 })
        );
        assert_eq!(buffer.to_string(), "héllo");

        assert_eq!(buffer.replace(r(1, 3), "e"), Ok(()));
        assert_eq!(buffer.to_string(), "hello");
        assert_eq!(
            buffer.replace(r(2, 9), "!"),
            Err(TextError::OutOfBounds { offset: 9, len: 5 })
        );
        assert_eq!(buffer.to_string(), "hello");
    }

    #[test]
    fn chunk_iterators_reassemble_the_text() {
        let text = "ab".repeat(3000) + "é" + &"cd".repeat(3000);
        let buffer = Buffer::from_text(&text);
        assert!(
            buffer.chunks().count() > 1,
            "the text spans several rope chunks"
        );
        assert_eq!(buffer.chunks().collect::<String>(), text);

        let range = r(5990, 6020);
        assert_eq!(
            buffer.chunks_in(range).collect::<String>(),
            &text[5990..6020]
        );
        assert_eq!(
            buffer.chunks_in(r(0, 99_999)).collect::<String>(),
            text,
            "an end past the buffer is clamped"
        );
        let accent = Buffer::from_text("aéb");
        assert_eq!(
            accent.chunks_in(r(2, 2)).collect::<String>(),
            "é",
            "an empty range inside é widens to the whole scalar"
        );
        assert_eq!(accent.chunks_in(r(3, 3)).collect::<String>(), "");
        assert_eq!(accent.chunks_in(r(1, 3)).collect::<String>(), "é");
    }

    #[test]
    fn grapheme_steps_cross_clusters_and_terminators() {
        let buffer = Buffer::from_text("e\u{301}x\ny");
        assert_eq!(
            [0, 3, 4, 5, 6, 100].map(|offset| buffer.next_grapheme_boundary(offset)),
            [3, 4, 5, 6, 6, 6]
        );
        assert_eq!(
            [0, 3, 4, 5, 6, 100].map(|offset| buffer.prev_grapheme_boundary(offset)),
            [0, 0, 3, 4, 5, 5]
        );

        let crlf = Buffer::from_text("a\r\nb");
        assert_eq!(crlf.next_grapheme_boundary(1), 3, "CRLF is one step");
        assert_eq!(crlf.next_grapheme_boundary(2), 3, "from inside CRLF");
        assert_eq!(crlf.prev_grapheme_boundary(2), 1, "from inside CRLF");
        assert_eq!(
            crlf.prev_grapheme_boundary(3),
            1,
            "from the next line start"
        );
    }

    #[test]
    fn word_range_at_returns_the_segment_under_the_offset() {
        let buffer = Buffer::from_text("hello world, foo");
        assert_eq!(buffer.word_range_at(0), r(0, 5));
        assert_eq!(buffer.word_range_at(3), r(0, 5));
        assert_eq!(buffer.word_range_at(5), r(5, 6), "whitespace is a segment");
        assert_eq!(buffer.word_range_at(6), r(6, 11));
        assert_eq!(
            buffer.word_range_at(11),
            r(11, 12),
            "punctuation is a segment"
        );
        assert_eq!(
            buffer.word_range_at(16),
            r(13, 16),
            "line end gives the last segment"
        );
        assert_eq!(buffer.word_range_at(100), r(13, 16));

        let lines = Buffer::from_text("ab cd\n\nef");
        assert_eq!(
            lines.word_range_at(6),
            r(6, 6),
            "an empty line yields an empty range"
        );
        assert_eq!(lines.word_range_at(8), r(7, 9));
    }

    #[test]
    fn next_word_boundary_skips_to_the_next_word_start() {
        let buffer = Buffer::from_text("hello world, foo");
        assert_eq!(
            [0, 3, 6, 11, 13].map(|offset| buffer.next_word_boundary(offset)),
            [6, 6, 11, 13, 16]
        );
        assert_eq!(buffer.next_word_boundary(16), 16, "end of the document");
        assert_eq!(buffer.next_word_boundary(100), 16);

        let trailing = Buffer::from_text("hello   ");
        assert_eq!(trailing.next_word_boundary(2), 8, "no later word: line end");

        let blank = Buffer::from_text("ab\n\ncd");
        assert_eq!(
            blank.next_word_boundary(2),
            3,
            "next line is empty: stop at its end"
        );
        assert_eq!(
            blank.next_word_boundary(3),
            4,
            "from the empty line to the next word"
        );

        let spaces = Buffer::from_text("ab\n  \ncd");
        assert_eq!(spaces.next_word_boundary(2), 5, "whitespace-only next line");
    }

    #[test]
    fn prev_word_boundary_skips_to_the_previous_word_start() {
        let buffer = Buffer::from_text("hello world, foo");
        assert_eq!(
            [16, 13, 11, 6, 1, 0].map(|offset| buffer.prev_word_boundary(offset)),
            [13, 11, 6, 0, 0, 0]
        );

        let indented = Buffer::from_text("  ab");
        assert_eq!(
            indented.prev_word_boundary(2),
            0,
            "no earlier word start on the line"
        );
        assert_eq!(indented.prev_word_boundary(1), 0);

        let lines = Buffer::from_text("ab cd\nef");
        assert_eq!(
            lines.prev_word_boundary(6),
            3,
            "last word of the previous line"
        );
        let blank = Buffer::from_text("ab cd\n\nef");
        assert_eq!(blank.prev_word_boundary(7), 6, "previous line has no words");
        let spaces = Buffer::from_text("  \nab");
        assert_eq!(
            spaces.prev_word_boundary(3),
            0,
            "previous line is only whitespace"
        );
    }

    #[test]
    fn utf16_map_of_ascii_is_the_identity() {
        let map = Utf16Map::new("abc");
        assert_eq!((map.byte_len(), map.utf16_len()), (3, 3));
        assert_eq!(map.byte_to_utf16(3), Some(3));
        assert_eq!(map.byte_to_utf16(4), None);
        assert_eq!(map.utf16_to_byte(3), Some(3));
        assert_eq!(map.utf16_to_byte(4), None);
        assert_eq!(map.byte_to_utf16_floor(9), 3);
        assert_eq!(map.utf16_to_byte_floor(9), 3);
    }

    #[test]
    fn utf16_map_converts_around_a_surrogate_pair() {
        // a: byte 0 / unit 0; 😀: bytes 1..5 / units 1..3; b: byte 5 / unit 3.
        let map = Utf16Map::new("a😀b");
        assert_eq!((map.byte_len(), map.utf16_len()), (6, 4));
        assert_eq!(
            [0, 1, 2, 5, 6, 7].map(|byte| map.byte_to_utf16(byte)),
            [Some(0), Some(1), None, Some(3), Some(4), None]
        );
        assert_eq!(
            [0, 1, 2, 3, 4, 5].map(|unit| map.utf16_to_byte(unit)),
            [Some(0), Some(1), None, Some(5), Some(6), None]
        );
        assert_eq!(
            [0, 1, 2, 3, 5, 6, 100].map(|byte| map.byte_to_utf16_floor(byte)),
            [0, 1, 1, 1, 3, 4, 4]
        );
        assert_eq!(
            [0, 1, 2, 3, 4, 9].map(|unit| map.utf16_to_byte_floor(unit)),
            [0, 1, 1, 5, 6, 6]
        );
    }

    #[test]
    fn utf16_map_handles_consecutive_wide_scalars() {
        // é: bytes 0..2 / unit 0; 😀: bytes 2..6 / units 1..3; x: byte 6 / unit 3.
        let map = Utf16Map::new("é😀x");
        assert_eq!((map.byte_len(), map.utf16_len()), (7, 4));
        assert_eq!(
            [1, 2, 4, 6, 7].map(|byte| map.byte_to_utf16(byte)),
            [None, Some(1), None, Some(3), Some(4)]
        );
        assert_eq!(
            [1, 2, 3, 4].map(|unit| map.utf16_to_byte(unit)),
            [Some(2), None, Some(6), Some(7)]
        );
        assert_eq!([1, 4].map(|byte| map.byte_to_utf16_floor(byte)), [0, 1]);
    }

    #[test]
    fn utf16_ranges_convert_only_when_both_ends_do() {
        let map = Utf16Map::new("a😀b");
        assert_eq!(map.range_to_utf16(r(0, 5)), Some(0..3));
        assert_eq!(map.range_to_utf16(r(2, 5)), None, "start inside the scalar");
        assert_eq!(map.range_to_utf16(r(0, 2)), None, "end inside the scalar");

        assert_eq!(map.range_to_bytes(0..3), Some(r(0, 5)));
        assert_eq!(map.range_to_bytes(2..3), None, "start between surrogates");
        assert_eq!(map.range_to_bytes(0..2), None, "end between surrogates");
        assert_eq!(map.range_to_bytes(units(3, 1)), None, "reversed range");
    }
}
