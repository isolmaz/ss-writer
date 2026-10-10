//! The UIA Text pattern for the editor document.
//!
//! UIA text ranges are UTF-16 offsets; the editor works in UTF-8 byte offsets. [`Document`]
//! converts between them and implements unit navigation (character, word, line, paragraph,
//! document) on UTF-16 positions; [`TextRangeProvider`] wraps it as `ITextRangeProvider`.
//! Rectangles, line breaks, selection and scrolling come from the registered
//! [`TextSource`](super::TextSource).

// The `windows` crate names its UIA identifiers `TextUnit_Character` and so on; matching on them
// is the idiomatic way to dispatch.
#![allow(non_upper_case_globals)]

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER};
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    IRawElementProviderSimple, ITextProvider_Impl, ITextRangeProvider, ITextRangeProvider_Impl,
    SupportedTextSelection, SupportedTextSelection_Single, TextPatternRangeEndpoint,
    TextPatternRangeEndpoint_Start, TextUnit, TextUnit_Character, TextUnit_Line,
    TextUnit_Paragraph, TextUnit_Word, UIA_TEXTATTRIBUTE_ID, UiaPoint,
};
use windows::core::{AsImpl, BOOL, BSTR, ComObject, Error, Ref, implement};

use ssw_ui::geometry::Point;
use ssw_ui::widget::WidgetId;

use super::patterns::safe_array_of;
use super::provider::{ElementProvider_Impl, not_supported};
use super::{Shared, TextSource, none, safe_array_empty_unknown, safe_array_f64};

/// The unit a range is navigated by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Character,
    Word,
    Line,
    Paragraph,
    Document,
}

impl Unit {
    /// Maps a UIA unit; formats and pages are treated as the whole document.
    #[must_use]
    pub fn from_uia(unit: TextUnit) -> Self {
        match unit {
            TextUnit_Character => Unit::Character,
            TextUnit_Word => Unit::Word,
            TextUnit_Line => Unit::Line,
            TextUnit_Paragraph => Unit::Paragraph,
            _ => Unit::Document,
        }
    }
}

/// A document's text with UTF-16 ⇄ UTF-8 offset conversion and unit navigation.
#[derive(Debug, Clone)]
pub struct Document {
    text: String,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Document {
    #[must_use]
    pub fn new(text: String) -> Self {
        Self { text }
    }

    /// Length in UTF-16 code units.
    #[must_use]
    pub fn len16(&self) -> usize {
        self.text.encode_utf16().count()
    }

    /// The UTF-16 offset of a byte offset (rounded down to a char boundary).
    #[must_use]
    pub fn to16(&self, byte: usize) -> usize {
        self.text
            .char_indices()
            .take_while(|(index, c)| index + c.len_utf8() <= byte)
            .map(|(_, c)| c.len_utf16())
            .sum()
    }

    /// The byte offset of a UTF-16 offset (rounded down to a char boundary; clamped to the end).
    #[must_use]
    pub fn to_byte(&self, utf16: usize) -> usize {
        let mut units = 0;
        for (index, c) in self.text.char_indices() {
            if units + c.len_utf16() > utf16 {
                return index;
            }
            units += c.len_utf16();
        }
        self.text.len()
    }

    /// The text of a UTF-16 range.
    #[must_use]
    pub fn slice16(&self, range: Range<usize>) -> &str {
        let start = self.to_byte(range.start);
        let end = self.to_byte(range.end).max(start);
        &self.text[start..end]
    }

    /// UTF-16 offsets where a unit starts, ascending, all `< len16` (or `[0]` for no text).
    fn unit_starts(&self, unit: Unit, source: &dyn TextSource) -> Vec<usize> {
        let len = self.len16();
        if len == 0 {
            return vec![0];
        }
        // Every char with its UTF-16 offset; only the char-based units need it.
        let chars = || -> Vec<(usize, char)> {
            let mut units = 0;
            self.text
                .chars()
                .map(|c| {
                    let at = units;
                    units += c.len_utf16();
                    (at, c)
                })
                .collect()
        };
        match unit {
            Unit::Document => vec![0],
            Unit::Character => chars().iter().map(|(at, _)| *at).collect(),
            Unit::Word => {
                let chars = chars();
                chars
                    .iter()
                    .enumerate()
                    .filter(|(index, (_, c))| {
                        *index == 0 || {
                            let previous = chars[*index - 1].1;
                            !c.is_whitespace()
                                && (previous.is_whitespace()
                                    || is_word_char(previous) != is_word_char(*c))
                        }
                    })
                    .map(|(_, (at, _))| *at)
                    .collect()
            }
            Unit::Paragraph => {
                let mut starts = vec![0];
                starts.extend(
                    chars()
                        .iter()
                        .filter(|(_, c)| *c == '\n')
                        .map(|(at, _)| at + 1)
                        .filter(|start| *start < len),
                );
                starts
            }
            Unit::Line => {
                let mut starts = Vec::new();
                let mut byte = 0;
                while byte < self.text.len() {
                    starts.push(self.to16(byte));
                    let line = source.line_range(byte);
                    // A line range that does not advance would loop forever; treat the rest as
                    // one line.
                    if line.end <= byte {
                        break;
                    }
                    byte = self.text.floor_char_boundary(line.end);
                }
                starts
            }
        }
    }

    /// The unit containing `[start, end)`: the largest unit start ≤ `start` to the smallest
    /// unit boundary ≥ `end` (the unit end of `start` for an empty range).
    #[must_use]
    pub fn enclosing(
        &self,
        unit: Unit,
        source: &dyn TextSource,
        range: Range<usize>,
    ) -> Range<usize> {
        let len = self.len16();
        let starts = self.unit_starts(unit, source);
        let start = starts
            .iter()
            .rev()
            .copied()
            .find(|s| *s <= range.start)
            .unwrap_or(0);
        let next_boundary =
            |after: usize| starts.iter().copied().find(|s| *s > after).unwrap_or(len);
        let end = if range.end <= range.start {
            next_boundary(start)
        } else {
            starts
                .iter()
                .copied()
                .find(|s| *s >= range.end)
                .unwrap_or(len)
        };
        start..end
    }

    /// Steps `position` by `count` unit boundaries (positive forward). Returns the new position
    /// and the signed number of units moved. The end of the text counts as a boundary.
    #[must_use]
    pub fn step(
        &self,
        unit: Unit,
        source: &dyn TextSource,
        position: usize,
        count: i32,
    ) -> (usize, i32) {
        let len = self.len16();
        let mut positions = self.unit_starts(unit, source);
        if positions.last() != Some(&len) {
            positions.push(len);
        }
        let mut at = position;
        let mut moved = 0;
        while moved < count {
            match positions.iter().copied().find(|p| *p > at) {
                Some(next) => at = next,
                None => break,
            }
            moved += 1;
        }
        while moved > count {
            match positions.iter().rev().copied().find(|p| *p < at) {
                Some(previous) => at = previous,
                None => break,
            }
            moved -= 1;
        }
        (at, moved)
    }
}

/// A UIA text range over a document node. Offsets are UTF-16 code units; UIA calls arrive on
/// several threads, so they are atomics.
#[implement(ITextRangeProvider)]
pub(crate) struct TextRangeProvider {
    shared: Arc<Shared>,
    id: WidgetId,
    /// The text source of the document node, looked up once when the range is created.
    source: Arc<dyn TextSource>,
    start: AtomicUsize,
    end: AtomicUsize,
}

impl TextRangeProvider {
    fn create(
        shared: &Arc<Shared>,
        id: WidgetId,
        source: &Arc<dyn TextSource>,
        range: Range<usize>,
    ) -> ITextRangeProvider {
        ComObject::new(Self {
            shared: shared.clone(),
            id,
            source: source.clone(),
            start: AtomicUsize::new(range.start),
            end: AtomicUsize::new(range.end),
        })
        .to_interface()
    }

    fn range(&self) -> Range<usize> {
        self.start.load(Relaxed)..self.end.load(Relaxed)
    }

    fn set(&self, range: Range<usize>) {
        self.start.store(range.start, Relaxed);
        self.end.store(range.end, Relaxed);
    }
}

impl TextRangeProvider_Impl {
    /// The current document text and the source it came from.
    fn context(&self) -> (Document, Arc<dyn TextSource>) {
        (Document::new(self.source.text()), self.source.clone())
    }

    fn byte_range(&self, document: &Document) -> Range<usize> {
        document.to_byte(self.start.load(Relaxed))..document.to_byte(self.end.load(Relaxed))
    }

    /// The range of another range object.
    fn peer(&self, other: &Ref<ITextRangeProvider>) -> windows::core::Result<Range<usize>> {
        let other = other.as_ref().ok_or_else(|| Error::from(E_POINTER))?;
        // SAFETY: UIA only passes back range objects this provider created, and
        // `TextRangeProvider` is the only type behind `ITextRangeProvider` here.
        let other: &TextRangeProvider = unsafe { AsImpl::<TextRangeProvider>::as_impl(other) };
        Ok(other.range())
    }
}

fn endpoint_value(range: &Range<usize>, endpoint: TextPatternRangeEndpoint) -> usize {
    if endpoint == TextPatternRangeEndpoint_Start {
        range.start
    } else {
        range.end
    }
}

impl ITextRangeProvider_Impl for TextRangeProvider_Impl {
    fn Clone(&self) -> windows::core::Result<ITextRangeProvider> {
        Ok(TextRangeProvider::create(
            &self.shared,
            self.id,
            &self.source,
            self.range(),
        ))
    }

    fn Compare(&self, range: Ref<ITextRangeProvider>) -> windows::core::Result<BOOL> {
        Ok(BOOL::from(self.peer(&range)? == self.range()))
    }

    fn CompareEndpoints(
        &self,
        endpoint: TextPatternRangeEndpoint,
        target: Ref<ITextRangeProvider>,
        target_endpoint: TextPatternRangeEndpoint,
    ) -> windows::core::Result<i32> {
        let ours = endpoint_value(&self.range(), endpoint);
        let theirs = endpoint_value(&self.peer(&target)?, target_endpoint);
        Ok(match ours.cmp(&theirs) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        })
    }

    fn ExpandToEnclosingUnit(&self, unit: TextUnit) -> windows::core::Result<()> {
        let (document, source) = self.context();
        let range = document.enclosing(Unit::from_uia(unit), source.as_ref(), self.range());
        self.set(range);
        Ok(())
    }

    fn FindAttribute(
        &self,
        _attribute: UIA_TEXTATTRIBUTE_ID,
        _value: &VARIANT,
        _backward: BOOL,
    ) -> windows::core::Result<ITextRangeProvider> {
        Err(not_supported())
    }

    fn FindText(
        &self,
        text: &BSTR,
        backward: BOOL,
        ignore_case: BOOL,
    ) -> windows::core::Result<ITextRangeProvider> {
        let (document, _) = self.context();
        let needle = text.to_string();
        if needle.is_empty() {
            return Err(Error::from(E_INVALIDARG));
        }
        let haystack = document.slice16(self.range()).to_owned();
        let fold = |s: &str| {
            if ignore_case.as_bool() {
                s.to_lowercase()
            } else {
                s.to_owned()
            }
        };
        let (haystack_folded, needle_folded) = (fold(&haystack), fold(&needle));
        // Lowercasing can change byte lengths, so match on the folded text only when the case
        // is ignored and the lengths agree; otherwise match exactly.
        let found = if haystack_folded.len() == haystack.len() {
            if backward.as_bool() {
                haystack_folded.rfind(&needle_folded)
            } else {
                haystack_folded.find(&needle_folded)
            }
        } else if backward.as_bool() {
            haystack.rfind(&needle)
        } else {
            haystack.find(&needle)
        };
        match found {
            Some(byte) => {
                let base = self.range().start;
                let start = base + haystack[..byte].encode_utf16().count();
                let end = start + needle.encode_utf16().count();
                Ok(TextRangeProvider::create(
                    &self.shared,
                    self.id,
                    &self.source,
                    start..end,
                ))
            }
            None => Err(none()),
        }
    }

    fn GetAttributeValue(
        &self,
        _attribute: UIA_TEXTATTRIBUTE_ID,
    ) -> windows::core::Result<VARIANT> {
        Err(not_supported())
    }

    fn GetBoundingRectangles(&self) -> windows::core::Result<*mut SAFEARRAY> {
        let (document, source) = self.context();
        let mut values = Vec::new();
        for rect in source.bounding_rects(self.byte_range(&document)) {
            let (left, top, width, height) = self.shared.to_screen(rect);
            values.extend([left, top, width, height]);
        }
        safe_array_f64(&values)
    }

    fn GetEnclosingElement(&self) -> windows::core::Result<IRawElementProviderSimple> {
        self.shared.node(self.id)?;
        Ok(self.shared.provider(self.id))
    }

    fn GetText(&self, max_length: i32) -> windows::core::Result<BSTR> {
        let (document, _) = self.context();
        let text = document.slice16(self.range());
        let limit = match max_length {
            -1 => usize::MAX,
            n => usize::try_from(n).map_err(|_| Error::from(E_INVALIDARG))?,
        };
        let clipped: String = {
            let mut units = 0;
            text.chars()
                .take_while(|c| {
                    units += c.len_utf16();
                    units <= limit
                })
                .collect()
        };
        Ok(BSTR::from(clipped.as_str()))
    }

    fn Move(&self, unit: TextUnit, count: i32) -> windows::core::Result<i32> {
        let (document, source) = self.context();
        let unit = Unit::from_uia(unit);
        let current = document.enclosing(unit, source.as_ref(), self.range());
        let (start, moved) = if count == 0 {
            (current.start, 0)
        } else {
            let (position, moved) = document.step(unit, source.as_ref(), current.start, count);
            // Moving forward onto the end of the text stays on the last unit.
            let position = if position >= document.len16() && document.len16() > 0 {
                document
                    .enclosing(
                        unit,
                        source.as_ref(),
                        document.len16() - 1..document.len16(),
                    )
                    .start
            } else {
                position
            };
            (position, moved)
        };
        self.set(document.enclosing(unit, source.as_ref(), start..start));
        Ok(moved)
    }

    fn MoveEndpointByUnit(
        &self,
        endpoint: TextPatternRangeEndpoint,
        unit: TextUnit,
        count: i32,
    ) -> windows::core::Result<i32> {
        let (document, source) = self.context();
        let range = self.range();
        let from = endpoint_value(&range, endpoint);
        let (to, moved) = document.step(Unit::from_uia(unit), source.as_ref(), from, count);
        let moved_range = if endpoint == TextPatternRangeEndpoint_Start {
            to..range.end.max(to)
        } else {
            range.start.min(to)..to
        };
        self.set(moved_range);
        Ok(moved)
    }

    fn MoveEndpointByRange(
        &self,
        endpoint: TextPatternRangeEndpoint,
        target: Ref<ITextRangeProvider>,
        target_endpoint: TextPatternRangeEndpoint,
    ) -> windows::core::Result<()> {
        let to = endpoint_value(&self.peer(&target)?, target_endpoint);
        let range = self.range();
        let moved_range = if endpoint == TextPatternRangeEndpoint_Start {
            to..range.end.max(to)
        } else {
            range.start.min(to)..to
        };
        self.set(moved_range);
        Ok(())
    }

    fn Select(&self) -> windows::core::Result<()> {
        let (document, source) = self.context();
        source.select(self.byte_range(&document));
        Ok(())
    }

    fn AddToSelection(&self) -> windows::core::Result<()> {
        Err(not_supported())
    }

    fn RemoveFromSelection(&self) -> windows::core::Result<()> {
        Err(not_supported())
    }

    fn ScrollIntoView(&self, align_to_top: BOOL) -> windows::core::Result<()> {
        let (document, source) = self.context();
        source.scroll_into_view(self.byte_range(&document), align_to_top.as_bool());
        Ok(())
    }

    fn GetChildren(&self) -> windows::core::Result<*mut SAFEARRAY> {
        safe_array_empty_unknown()
    }
}

impl ElementProvider_Impl {
    fn text_document(&self) -> windows::core::Result<(Document, Arc<dyn TextSource>)> {
        let source = self.shared.text_source(self.id)?;
        Ok((Document::new(source.text()), source))
    }

    fn text_ranges(&self, ranges: &[Range<usize>]) -> windows::core::Result<*mut SAFEARRAY> {
        let (document, source) = self.text_document()?;
        let providers: Vec<ITextRangeProvider> = ranges
            .iter()
            .map(|range| {
                TextRangeProvider::create(
                    &self.shared,
                    self.id,
                    &source,
                    document.to16(range.start)..document.to16(range.end),
                )
            })
            .collect();
        safe_array_of(&providers)
    }
}

impl ITextProvider_Impl for ElementProvider_Impl {
    fn GetSelection(&self) -> windows::core::Result<*mut SAFEARRAY> {
        let node = self.shared.node(self.id)?;
        let info = node.patterns.text.ok_or_else(not_supported)?;
        self.text_ranges(&info.selection)
    }

    fn GetVisibleRanges(&self) -> windows::core::Result<*mut SAFEARRAY> {
        let node = self.shared.node(self.id)?;
        let info = node.patterns.text.ok_or_else(not_supported)?;
        self.text_ranges(&[info.visible])
    }

    fn RangeFromChild(
        &self,
        _child: Ref<IRawElementProviderSimple>,
    ) -> windows::core::Result<ITextRangeProvider> {
        self.DocumentRange()
    }

    fn RangeFromPoint(&self, point: &UiaPoint) -> windows::core::Result<ITextRangeProvider> {
        let (document, source) = self.text_document()?;
        let window = self.shared.screen_to_window(point.x, point.y);
        let byte = source
            .offset_from_point(Point::new(window.x, window.y))
            .ok_or_else(none)?;
        let at = document.to16(byte);
        Ok(TextRangeProvider::create(
            &self.shared,
            self.id,
            &source,
            at..at,
        ))
    }

    fn DocumentRange(&self) -> windows::core::Result<ITextRangeProvider> {
        let (document, source) = self.text_document()?;
        Ok(TextRangeProvider::create(
            &self.shared,
            self.id,
            &source,
            0..document.len16(),
        ))
    }

    fn SupportedTextSelection(&self) -> windows::core::Result<SupportedTextSelection> {
        Ok(SupportedTextSelection_Single)
    }
}
