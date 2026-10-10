//! Verbatim source lines (code, math and Mermaid source, an image's markup) shown as one text
//! layout, with the map between the displayed text and the document.
//!
//! The lines of a block nested in a quote or a list are not contiguous in the document (the
//! container prefixes lie between them), so the display text is built from the lines and mapped
//! back segment by segment.

use ssw_core::markdown::ast::VerbatimLine;
use ssw_core::text::TextRange;

use crate::canvas::Canvas;
use crate::geometry::{Color, Point, Rect};
use crate::text::{TextLayout, TextStyle};

use crate::editor::block::BlockCx;
use crate::editor::config::Composition;
use crate::editor::display::{CompositionDisplay, Display, DisplayMap, Segment, SegmentKind};

/// One displayed line of source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Row {
    /// The source text of the line (without its terminator and container prefix).
    pub src: TextRange,
    /// Spaces displayed before the text that are not in the source (a partly consumed tab).
    pub pad: u8,
}

impl Row {
    /// The row of a parsed verbatim line.
    pub fn of(line: &VerbatimLine) -> Self {
        Self {
            src: line.range,
            pad: line.pad,
        }
    }
}

/// Where a row ended up in the display text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RowSpan {
    /// Display offset of the row's first byte (its pad spaces).
    pub disp_start: usize,
    /// Byte length of the row's text in the display text without a composition.
    pub text_len: usize,
    /// The composition inserted in this row: offset inside the row text and its length.
    pub composition: Option<(usize, usize)>,
}

/// The display text of a block of verbatim rows.
#[derive(Debug)]
pub(super) struct SourceDisplay {
    /// Text, map, composition (no styles yet).
    pub display: Display,
    /// One span per row, in order.
    pub spans: Vec<RowSpan>,
}

/// Builds the display of `rows` (joined by line breaks) for a block covering `block`. Source
/// before the first and after the last row (fences, delimiters) is hidden. `composition` is
/// displayed when it lies in a row.
pub(super) fn display_of(
    block: TextRange,
    rows: &[Row],
    buffer: &ssw_core::text::Buffer,
    composition: Option<&Composition>,
) -> SourceDisplay {
    let mut text = String::new();
    let mut segments: Vec<Segment> = Vec::new();
    let mut spans = Vec::with_capacity(rows.len());
    let mut shown_composition = None;
    let mut cursor = block.start;
    for (index, row) in rows.iter().enumerate() {
        if index == 0 {
            if cursor < row.src.start {
                segments.push(Segment {
                    src: TextRange::new(cursor, row.src.start),
                    disp: TextRange::at(0),
                    kind: SegmentKind::Hidden,
                });
            }
        } else {
            let at = text.len();
            text.push('\n');
            segments.push(Segment {
                src: TextRange::new(cursor, row.src.start),
                disp: TextRange::new(at, at + 1),
                kind: SegmentKind::Atomic,
            });
        }
        let disp_start = text.len();
        if row.pad > 0 {
            text.push_str(&" ".repeat(usize::from(row.pad)));
            segments.push(Segment {
                src: TextRange::at(row.src.start),
                disp: TextRange::new(disp_start, text.len()),
                kind: SegmentKind::Atomic,
            });
        }
        let content = buffer.slice(row.src);
        let split = composition.filter(|comp| {
            row.src.touches(comp.at) && content.is_char_boundary(comp.at - row.src.start)
        });
        let split_at = split.map_or(row.src.end, |comp| comp.at);
        let mut text_composition = None;
        push_shown(
            &mut text,
            &mut segments,
            &content,
            row.src,
            row.src.start,
            split_at,
        );
        if let Some(comp) = split {
            let start = text.len();
            text_composition = Some((start - disp_start, comp.text.len()));
            text.push_str(&comp.text);
            segments.push(Segment {
                src: TextRange::at(comp.at),
                disp: TextRange::new(start, text.len()),
                kind: SegmentKind::Composition,
            });
            shown_composition = Some(CompositionDisplay {
                range: start..text.len(),
                caret: start + floor_boundary(&comp.text, comp.caret),
                attrs: comp
                    .attrs
                    .iter()
                    .map(|attr| (start + attr.range.start..start + attr.range.end, attr.style))
                    .collect(),
            });
        }
        push_shown(
            &mut text,
            &mut segments,
            &content,
            row.src,
            split_at,
            row.src.end,
        );
        spans.push(RowSpan {
            disp_start,
            text_len: text.len() - disp_start - text_composition.map_or(0, |(_, len)| len),
            composition: text_composition,
        });
        cursor = row.src.end;
    }
    if cursor < block.end {
        segments.push(Segment {
            src: TextRange::new(cursor, block.end),
            disp: TextRange::at(text.len()),
            kind: SegmentKind::Hidden,
        });
    }
    let len = text.len();
    SourceDisplay {
        display: Display {
            text,
            map: DisplayMap::from_segments(segments, block, len),
            styles: Vec::new(),
            objects: Vec::new(),
            object_draws: Vec::new(),
            pills: Vec::new(),
            links: Vec::new(),
            composition: shown_composition,
        },
        spans,
    }
}

fn push_shown(
    text: &mut String,
    segments: &mut Vec<Segment>,
    content: &str,
    row: TextRange,
    from: usize,
    to: usize,
) {
    if from >= to {
        return;
    }
    let start = text.len();
    text.push_str(
        content
            .get(from - row.start..to - row.start)
            .unwrap_or_default(),
    );
    segments.push(Segment {
        src: TextRange::new(from, to),
        disp: TextRange::new(start, text.len()),
        kind: SegmentKind::Shown,
    });
}

/// The largest char boundary of `text` at or before `at`.
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// A laid-out [`Display`]: the retained text of a verbatim block with the offset mapping the
/// layout trait needs. All coordinates are relative to the text's own origin.
pub(super) struct SourceText {
    pub layout: Box<dyn TextLayout>,
    pub display: Display,
    composition_at: Option<usize>,
}

impl SourceText {
    /// Lays `display` out with `base` (and its styles) in `width`.
    pub fn new(display: Display, base: &TextStyle, width: f32, cx: &BlockCx<'_>) -> Self {
        let layout = display.layout(cx.text, base, width);
        Self {
            layout,
            composition_at: cx
                .composition
                .filter(|_| display.composition.is_some())
                .map(|comp| comp.at),
            display,
        }
    }

    pub fn height(&self) -> f32 {
        self.layout.size().height
    }

    /// Draws the text (and the composition underline) with its origin at `origin`.
    pub fn paint(&self, canvas: &mut dyn Canvas, origin: Point, color: Color) {
        canvas.save();
        canvas.translate(origin.x, origin.y);
        canvas.draw_text_layout(self.layout.as_ref(), Point::ZERO, color);
        self.display
            .paint_front(self.layout.as_ref(), canvas, color);
        canvas.restore();
    }

    fn display_offset(&self, offset: usize) -> usize {
        match (&self.display.composition, self.composition_at) {
            (Some(composition), Some(at)) if at == offset => composition.caret,
            _ => self.display.map.source_to_display(offset),
        }
    }

    /// The source offset at text-local `point`.
    pub fn hit(&self, point: Point) -> usize {
        let hit = self.layout.hit_test_point(point);
        self.display.map.display_to_source(hit.insertion_offset())
    }

    /// The caret at source `offset`.
    pub fn caret(&self, offset: usize) -> Rect {
        self.layout.caret_rect(self.display_offset(offset), false)
    }

    /// One rectangle per line covered by `range`.
    pub fn selection(&self, range: TextRange) -> Vec<Rect> {
        let map = &self.display.map;
        self.layout
            .range_rects(map.source_to_display(range.start)..map.source_to_display(range.end))
    }

    /// The visual line containing `offset` as a source range.
    pub fn line_range(&self, offset: usize) -> TextRange {
        let map = &self.display.map;
        let display = self.display_offset(offset);
        // `line_for_offset` always names an existing line; the collapsed range is only the
        // contract's answer for a layout without one.
        self.layout
            .line_metrics(self.layout.line_for_offset(display))
            .map_or(TextRange::at(offset), |line| {
                let start = map.display_to_source(line.start);
                let end = map.display_to_source(line.content_end());
                TextRange::new(start, end.max(start))
            })
    }
}
