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
        match self.rope.get_byte_slice(start..end) {
            Some(slice) => match slice.as_str() {
                Some(s) => Cow::Borrowed(s),
                None => Cow::Owned(slice.to_string()),
            },
            None => Cow::Borrowed(""),
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
        for (i, g) in text.grapheme_indices(true) {
            let end = i + g.len();
            if end > rel {
                return content.start + end;
            }
        }
        content.end
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
            return if line == 0 {
                0
            } else {
                self.line_range(line - 1).end
            };
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
