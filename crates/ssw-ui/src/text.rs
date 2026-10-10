//! Styled text layout contract and a deterministic test engine.
//!
//! Every offset at this API is a UTF-8 byte offset into the laid-out text. `ssw-win`
//! implements [`TextLayoutEngine`] with DirectWrite (converting to UTF-16 internally); tests
//! use [`MonoTextEngine`].

use std::any::Any;
use std::ops::Range;
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;

use crate::geometry::{Color, Point, Rect, Size};

/// A font weight on the CSS/OpenType 1..=1000 scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FontWeight(pub u16);

impl FontWeight {
    pub const NORMAL: FontWeight = FontWeight(400);
    pub const MEDIUM: FontWeight = FontWeight(500);
    pub const SEMIBOLD: FontWeight = FontWeight(600);
    pub const BOLD: FontWeight = FontWeight(700);
}

/// The style of a run of text.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    /// Font family name.
    pub family: Arc<str>,
    /// Font size in DIPs.
    pub size: f32,
    pub weight: FontWeight,
    pub italic: bool,
    /// Text color; `None` uses the color passed to `Canvas::draw_text_layout`, so a layout can
    /// be redrawn dimmed or re-themed without being rebuilt.
    pub color: Option<Color>,
    pub underline: bool,
    pub strikethrough: bool,
    /// Background drawn behind the run (highlight pills, inline code).
    pub background: Option<Color>,
    /// Vertical offset of the glyphs from the baseline in DIPs; positive moves up
    /// (superscript), negative moves down.
    pub baseline_shift: f32,
}

impl TextStyle {
    /// A regular, upright, undecorated style.
    #[must_use]
    pub fn new(family: impl Into<Arc<str>>, size: f32) -> Self {
        Self {
            family: family.into(),
            size,
            weight: FontWeight::NORMAL,
            italic: false,
            color: None,
            underline: false,
            strikethrough: false,
            background: None,
            baseline_shift: 0.0,
        }
    }

    #[must_use]
    pub fn with_weight(mut self, weight: FontWeight) -> Self {
        self.weight = weight;
        self
    }

    #[must_use]
    pub fn with_italic(mut self, italic: bool) -> Self {
        self.italic = italic;
        self
    }

    #[must_use]
    pub fn with_color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    #[must_use]
    pub fn with_underline(mut self, underline: bool) -> Self {
        self.underline = underline;
        self
    }

    #[must_use]
    pub fn with_strikethrough(mut self, strikethrough: bool) -> Self {
        self.strikethrough = strikethrough;
        self
    }

    #[must_use]
    pub fn with_background(mut self, background: Color) -> Self {
        self.background = Some(background);
        self
    }

    #[must_use]
    pub fn with_baseline_shift(mut self, shift: f32) -> Self {
        self.baseline_shift = shift;
        self
    }
}

/// A style applied to a byte range of the text. Ranges are applied in order: where ranges
/// overlap the later one wins, and text covered by no range uses the base style.
#[derive(Debug, Clone, PartialEq)]
pub struct StyledRange {
    /// UTF-8 byte offsets, on char boundaries.
    pub range: Range<usize>,
    pub style: TextStyle,
}

/// An object laid out inline with the text (inline image, math, checkbox). It occupies the
/// grapheme that starts at `at` (conventionally a U+FFFC placeholder): that grapheme's advance
/// becomes `size.width`, and the line grows to fit the object. The caller paints the object at
/// [`TextLayout::inline_object_rect`]. An `at` that is not the start of a grapheme is ignored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InlineObject {
    /// Byte offset of the placeholder grapheme.
    pub at: usize,
    pub size: Size,
    /// Distance from the top of the object to its baseline.
    pub baseline: f32,
    /// Caller-chosen key used to look the object up after layout.
    pub id: u64,
}

/// Vertical metrics of one laid-out line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineMetrics {
    /// Top of the line relative to the top of the layout.
    pub top: f32,
    pub height: f32,
    /// Y of the baseline relative to the top of the layout (`top + ascent`).
    pub baseline: f32,
    /// Byte offset of the first character of the line.
    pub start: usize,
    /// Byte offset one past the line, including its hard line break; the next line starts here.
    pub end: usize,
    /// Byte length of the hard line break that ends the line (0 for soft-wrapped last lines).
    pub newline_len: usize,
}

impl LineMetrics {
    /// End of the line's content, before any hard line break (where `End` puts the caret).
    #[must_use]
    pub fn content_end(&self) -> usize {
        self.end - self.newline_len
    }
}

/// The result of hit testing a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitTest {
    /// Byte offset of the start of the hit cluster.
    pub offset: usize,
    /// The point is in the trailing half of the cluster (the caret belongs after it).
    pub trailing: bool,
    /// The point lies within the text's bounds.
    pub inside: bool,
    /// Byte length of the hit cluster (0 for the end of the text).
    pub length: usize,
}

impl HitTest {
    /// The caret insertion offset the hit maps to.
    #[must_use]
    pub fn insertion_offset(&self) -> usize {
        self.offset + if self.trailing { self.length } else { 0 }
    }
}

/// A laid-out block of text.
///
/// A caret position is `(offset, trailing)` of a cluster: `trailing == true` puts the caret
/// after the cluster that starts at `offset`. The same insertion offset can therefore be drawn
/// at the end of a wrapped line (`(previous cluster, true)`) or at the start of the next line
/// (`(offset, false)`).
pub trait TextLayout {
    /// The laid-out text.
    fn text(&self) -> &str;
    /// Size of the text's bounding box (trailing whitespace excluded from the width).
    fn size(&self) -> Size;
    /// Number of lines; always at least one.
    fn line_count(&self) -> usize;
    /// Metrics of line `index`, or `None` when out of range.
    fn line_metrics(&self, index: usize) -> Option<LineMetrics>;
    /// Hit tests a point in layout coordinates; points outside clamp to the nearest cluster.
    fn hit_test_point(&self, point: Point) -> HitTest;
    /// The caret rectangle (zero width, line height) for the caret position `(offset,
    /// trailing)`. Offsets past the end clamp to the end of the text.
    fn caret_rect(&self, offset: usize, trailing: bool) -> Rect;
    /// One rectangle per line covered by `range`, in line order. A range that covers a hard
    /// line break extends its rectangle on that line so the break is visible.
    fn range_rects(&self, range: Range<usize>) -> Vec<Rect>;
    /// Where the inline object `id` was placed, in layout coordinates.
    fn inline_object_rect(&self, id: u64) -> Option<Rect>;
    /// Downcast hook so a platform canvas can recover its own layout type.
    fn as_any(&self) -> &dyn Any;

    /// Index of the line the insertion offset belongs to: the last line starting at or before
    /// `offset`.
    fn line_for_offset(&self, offset: usize) -> usize {
        (0..self.line_count())
            .rev()
            .find(|&index| {
                self.line_metrics(index)
                    .is_some_and(|line| line.start <= offset)
            })
            .unwrap_or(0)
    }
}

/// Creates [`TextLayout`]s.
pub trait TextLayoutEngine {
    /// Lays out `text` with `base` style and `ranges` overrides, wrapping at `max_width`
    /// (`f32::INFINITY` for no wrapping). `objects` reserve inline space.
    fn layout(
        &self,
        text: &str,
        base: &TextStyle,
        ranges: &[StyledRange],
        objects: &[InlineObject],
        max_width: f32,
    ) -> Box<dyn TextLayout>;
}

/// Line height of [`MonoTextEngine`] as a multiple of the font size.
const MONO_LINE_HEIGHT_FACTOR: f32 = 1.2;

/// A deterministic engine for tests: every grapheme cluster advances by the same width
/// regardless of script or style, lines break after spaces (words longer than the available
/// width break by grapheme), and a line is `size * 1.2` tall with the baseline `size` below its
/// top. Only the size of the base style and inline objects affect metrics; `ranges` are
/// ignored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonoTextEngine {
    advance: f32,
}

impl MonoTextEngine {
    /// An engine whose grapheme advance is `advance` DIPs.
    #[must_use]
    pub const fn new(advance: f32) -> Self {
        Self { advance }
    }
}

impl Default for MonoTextEngine {
    fn default() -> Self {
        Self::new(8.0)
    }
}

impl TextLayoutEngine for MonoTextEngine {
    fn layout(
        &self,
        text: &str,
        base: &TextStyle,
        _ranges: &[StyledRange],
        objects: &[InlineObject],
        max_width: f32,
    ) -> Box<dyn TextLayout> {
        Box::new(MonoLayout::build(
            text,
            base.size,
            self.advance,
            objects,
            max_width,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClusterKind {
    Glyph,
    Space,
    Newline,
}

fn classify(grapheme: &str) -> ClusterKind {
    match grapheme {
        "\n" | "\r\n" | "\r" => ClusterKind::Newline,
        _ if grapheme.chars().next().is_some_and(char::is_whitespace) => ClusterKind::Space,
        _ => ClusterKind::Glyph,
    }
}

#[derive(Debug, Clone, Copy)]
struct Cluster {
    start: usize,
    end: usize,
    kind: ClusterKind,
    width: f32,
    object: Option<InlineObject>,
    /// X of the cluster within its line.
    x: f32,
    line: usize,
}

#[derive(Debug, Clone, Copy)]
struct Line {
    /// Cluster index range `first..last`.
    first: usize,
    last: usize,
    start: usize,
    end: usize,
    newline_len: usize,
    top: f32,
    height: f32,
    ascent: f32,
    /// Width without trailing spaces.
    visible_width: f32,
    /// X after the last cluster (trailing spaces included).
    end_x: f32,
}

#[derive(Debug)]
struct MonoLayout {
    text: String,
    size: Size,
    advance: f32,
    clusters: Vec<Cluster>,
    /// Never empty: `build` always produces at least one line.
    lines: Vec<Line>,
    objects: Vec<(u64, Rect)>,
}

impl MonoLayout {
    fn build(
        text: &str,
        font_size: f32,
        advance: f32,
        objects: &[InlineObject],
        max_width: f32,
    ) -> Self {
        let line_height = font_size * MONO_LINE_HEIGHT_FACTOR;
        let base_ascent = font_size;
        let base_descent = line_height - font_size;

        let mut clusters: Vec<Cluster> = text
            .grapheme_indices(true)
            .map(|(start, grapheme)| {
                let kind = classify(grapheme);
                let object = if kind == ClusterKind::Newline {
                    None
                } else {
                    objects.iter().find(|object| object.at == start).copied()
                };
                let width = match (kind, object) {
                    (ClusterKind::Newline, _) => 0.0,
                    (_, Some(object)) => object.size.width,
                    (_, None) => advance,
                };
                Cluster {
                    start,
                    end: start + grapheme.len(),
                    kind,
                    width,
                    object,
                    x: 0.0,
                    line: 0,
                }
            })
            .collect();

        // Break into lines: cluster index ranges `first..last`.
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut first = 0;
        let mut x = 0.0_f32;
        let mut i = 0;
        while i < clusters.len() {
            match clusters[i].kind {
                ClusterKind::Newline => {
                    i += 1;
                    ranges.push((first, i));
                    first = i;
                    x = 0.0;
                }
                ClusterKind::Space => {
                    // Spaces hang past the margin; they never trigger a break.
                    x += clusters[i].width;
                    i += 1;
                }
                ClusterKind::Glyph => {
                    let word_end = clusters[i..]
                        .iter()
                        .position(|cluster| cluster.kind != ClusterKind::Glyph)
                        .map_or(clusters.len(), |offset| i + offset);
                    let word_width: f32 = clusters[i..word_end]
                        .iter()
                        .map(|cluster| cluster.width)
                        .sum();
                    if first < i && x + word_width > max_width {
                        ranges.push((first, i));
                        first = i;
                        x = 0.0;
                    }
                    // A word wider than the line breaks by grapheme (at least one per line).
                    for (k, cluster) in clusters.iter().enumerate().take(word_end).skip(i) {
                        if first < k && x + cluster.width > max_width {
                            ranges.push((first, k));
                            first = k;
                            x = 0.0;
                        }
                        x += cluster.width;
                    }
                    i = word_end;
                }
            }
        }
        ranges.push((first, clusters.len()));

        // Position clusters and measure lines.
        let mut lines = Vec::with_capacity(ranges.len());
        let mut placed_objects = Vec::new();
        let mut top = 0.0_f32;
        for (index, &(first, last)) in ranges.iter().enumerate() {
            let mut ascent = base_ascent;
            let mut descent = base_descent;
            let mut x = 0.0_f32;
            for cluster in &mut clusters[first..last] {
                cluster.x = x;
                cluster.line = index;
                x += cluster.width;
                if let Some(object) = cluster.object {
                    ascent = ascent.max(object.baseline);
                    descent = descent.max(object.size.height - object.baseline);
                }
            }
            let slice = &clusters[first..last];
            for cluster in slice {
                if let Some(object) = cluster.object {
                    placed_objects.push((
                        object.id,
                        Rect::new(
                            cluster.x,
                            top + ascent - object.baseline,
                            object.size.width,
                            object.size.height,
                        ),
                    ));
                }
            }
            let start = slice.first().map_or(text.len(), |cluster| cluster.start);
            let end = slice.last().map_or(start, |cluster| cluster.end);
            let newline_len = slice
                .last()
                .filter(|cluster| cluster.kind == ClusterKind::Newline)
                .map_or(0, |cluster| cluster.end - cluster.start);
            let visible_width = slice
                .iter()
                .rev()
                .find(|cluster| cluster.kind == ClusterKind::Glyph)
                .map_or(0.0, |cluster| cluster.x + cluster.width);
            let height = ascent + descent;
            lines.push(Line {
                first,
                last,
                start,
                end,
                newline_len,
                top,
                height,
                ascent,
                visible_width,
                end_x: x,
            });
            top += height;
        }

        let width = lines
            .iter()
            .map(|line| line.visible_width)
            .fold(0.0, f32::max);
        Self {
            text: text.to_owned(),
            size: Size::new(width, top),
            advance,
            clusters,
            lines,
            objects: placed_objects,
        }
    }

    /// The line and x of a caret position (see [`TextLayout`]).
    fn caret_position(&self, offset: usize, trailing: bool) -> (usize, f32) {
        if offset >= self.text.len() {
            // `lines` is never empty.
            let last = self.lines.len() - 1;
            return (last, self.lines[last].end_x);
        }
        // The cluster containing `offset`; one exists because the last cluster ends at
        // `text.len()`.
        let index = self
            .clusters
            .partition_point(|cluster| cluster.end <= offset);
        let cluster = &self.clusters[index];
        match (trailing, cluster.kind) {
            (false, _) => (cluster.line, cluster.x),
            // After a hard line break the caret is at the start of the next line, which
            // always exists.
            (true, ClusterKind::Newline) => (cluster.line + 1, 0.0),
            (true, _) => (cluster.line, cluster.x + cluster.width),
        }
    }
}

impl TextLayout for MonoLayout {
    fn text(&self) -> &str {
        &self.text
    }

    fn size(&self) -> Size {
        self.size
    }

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn line_metrics(&self, index: usize) -> Option<LineMetrics> {
        self.lines.get(index).map(|line| LineMetrics {
            top: line.top,
            height: line.height,
            baseline: line.top + line.ascent,
            start: line.start,
            end: line.end,
            newline_len: line.newline_len,
        })
    }

    fn hit_test_point(&self, point: Point) -> HitTest {
        let line_index = self
            .lines
            .iter()
            .rposition(|line| line.top <= point.y)
            .unwrap_or(0);
        let line = &self.lines[line_index];
        let inside = point.x >= 0.0
            && point.x < line.visible_width
            && point.y >= 0.0
            && point.y < self.size.height;
        let content = &self.clusters[line.first..line.last];
        let hit = |cluster: &Cluster, trailing: bool| HitTest {
            offset: cluster.start,
            trailing,
            inside,
            length: cluster.end - cluster.start,
        };
        if let Some(cluster) = content.iter().find(|cluster| {
            cluster.kind != ClusterKind::Newline && point.x < cluster.x + cluster.width
        }) {
            return hit(cluster, point.x >= cluster.x + cluster.width / 2.0);
        }
        // Past the end of the line's content.
        match content.last() {
            // Before the hard line break.
            Some(last) if last.kind == ClusterKind::Newline => hit(last, false),
            // After the last cluster of a wrapped line.
            Some(last) => hit(last, true),
            // The empty last line.
            None => HitTest {
                offset: line.start,
                trailing: false,
                inside,
                length: 0,
            },
        }
    }

    fn caret_rect(&self, offset: usize, trailing: bool) -> Rect {
        let (line_index, x) = self.caret_position(offset, trailing);
        let line = &self.lines[line_index];
        Rect::new(x, line.top, 0.0, line.height)
    }

    fn range_rects(&self, range: Range<usize>) -> Vec<Rect> {
        let mut rects = Vec::new();
        if range.start >= range.end {
            return rects;
        }
        for line in &self.lines {
            if line.end <= range.start {
                continue;
            }
            if line.start >= range.end {
                break;
            }
            let mut span: Option<(f32, f32)> = None;
            for cluster in &self.clusters[line.first..line.last] {
                if cluster.start < range.end && cluster.end > range.start {
                    let width = if cluster.kind == ClusterKind::Newline {
                        self.advance
                    } else {
                        cluster.width
                    };
                    let from = span.map_or(cluster.x, |(from, _)| from);
                    span = Some((from, cluster.x + width));
                }
            }
            if let Some((from, to)) = span {
                rects.push(Rect::new(from, line.top, to - from, line.height));
            }
        }
        rects
    }

    fn inline_object_rect(&self, id: u64) -> Option<Rect> {
        self.objects
            .iter()
            .find(|(object, _)| *object == id)
            .map(|(_, rect)| *rect)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAMILY: &str = "Mono";

    fn engine() -> MonoTextEngine {
        MonoTextEngine::new(10.0)
    }

    fn style() -> TextStyle {
        TextStyle::new(FAMILY, 10.0)
    }

    fn layout(text: &str, max_width: f32) -> Box<dyn TextLayout> {
        engine().layout(text, &style(), &[], &[], max_width)
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    fn rect_close(a: Rect, b: Rect) -> bool {
        close(a.x, b.x) && close(a.y, b.y) && close(a.width, b.width) && close(a.height, b.height)
    }

    #[test]
    fn style_builders_set_every_field() {
        let base = TextStyle::new("Segoe UI", 16.0);
        assert_eq!(&*base.family, "Segoe UI");
        assert_eq!(base.weight, FontWeight::NORMAL);
        assert!(!base.italic && !base.underline && !base.strikethrough);
        assert_eq!(
            (base.color, base.background, base.baseline_shift),
            (None, None, 0.0)
        );

        let red = Color::from_rgb_u32(0xFF0000);
        let styled = base
            .with_weight(FontWeight::BOLD)
            .with_italic(true)
            .with_color(red)
            .with_underline(true)
            .with_strikethrough(true)
            .with_background(Color::WHITE)
            .with_baseline_shift(4.0);
        assert_eq!(styled.weight, FontWeight(700));
        assert!(styled.italic && styled.underline && styled.strikethrough);
        assert_eq!(styled.color, Some(red));
        assert_eq!(styled.background, Some(Color::WHITE));
        assert_eq!(styled.baseline_shift, 4.0);
        assert!(FontWeight::MEDIUM < FontWeight::SEMIBOLD);
        assert!(FontWeight::SEMIBOLD < FontWeight::BOLD);
        let range = StyledRange {
            range: 0..3,
            style: styled.clone(),
        };
        assert_eq!(range.clone(), range);
    }

    #[test]
    fn empty_text_has_one_line() {
        let l = layout("", 100.0);
        assert_eq!(l.line_count(), 1);
        assert_eq!(l.text(), "");
        assert!(close(l.size().width, 0.0) && close(l.size().height, 12.0));
        assert!(rect_close(
            l.caret_rect(0, false),
            Rect::new(0.0, 0.0, 0.0, 12.0)
        ));
        assert!(rect_close(
            l.caret_rect(7, true),
            Rect::new(0.0, 0.0, 0.0, 12.0)
        ));
        let hit = l.hit_test_point(Point::new(3.0, 3.0));
        assert_eq!(
            (hit.offset, hit.trailing, hit.inside, hit.length),
            (0, false, false, 0)
        );
        assert!(l.range_rects(0..0).is_empty());
        let line = l.line_metrics(0).expect("one line");
        assert_eq!(
            (line.start, line.end, line.newline_len, line.content_end()),
            (0, 0, 0, 0)
        );
        assert!(l.line_metrics(1).is_none());
    }

    #[test]
    fn wraps_after_spaces_and_reports_line_metrics() {
        let l = layout("hello world foo", 100.0);
        assert_eq!(l.line_count(), 2);
        let first = l.line_metrics(0).expect("line 0");
        assert_eq!((first.start, first.end, first.newline_len), (0, 6, 0));
        assert!(close(first.top, 0.0) && close(first.height, 12.0) && close(first.baseline, 10.0));
        let second = l.line_metrics(1).expect("line 1");
        assert_eq!((second.start, second.end), (6, 15));
        assert!(close(second.top, 12.0) && close(second.baseline, 22.0));
        // Width excludes the hanging space of line 0; line 1 is "world foo" = 90.
        let size = l.size();
        assert!(
            close(size.width, 90.0) && close(size.height, 24.0),
            "{size:?}"
        );
        assert_eq!(l.line_for_offset(0), 0);
        assert_eq!(l.line_for_offset(5), 0);
        assert_eq!(l.line_for_offset(6), 1);
        assert_eq!(l.line_for_offset(15), 1);
        assert_eq!(l.line_for_offset(500), 1);
    }

    #[test]
    fn hit_testing_resolves_cluster_side_and_clamps() {
        let l = layout("hello world foo", 100.0);
        let leading = l.hit_test_point(Point::new(4.0, 5.0));
        assert_eq!(
            (
                leading.offset,
                leading.trailing,
                leading.inside,
                leading.length
            ),
            (0, false, true, 1)
        );
        let trailing = l.hit_test_point(Point::new(6.0, 5.0));
        assert_eq!((trailing.offset, trailing.trailing), (0, true));
        assert_eq!(trailing.insertion_offset(), 1);
        assert_eq!(leading.insertion_offset(), 0);

        // Right of a wrapped line: after its hanging space, i.e. the start of the next line.
        let past_wrap = l.hit_test_point(Point::new(500.0, 5.0));
        assert_eq!(
            (past_wrap.offset, past_wrap.trailing, past_wrap.inside),
            (5, true, false)
        );
        assert_eq!(past_wrap.insertion_offset(), 6);
        // Left of a line clamps to its first cluster.
        let left = l.hit_test_point(Point::new(-10.0, 30.0));
        assert_eq!((left.offset, left.trailing, left.inside), (6, false, false));
        // Below the layout clamps to the last line.
        let below = l.hit_test_point(Point::new(10.0, 100.0));
        assert_eq!(
            (below.offset, below.trailing, below.inside),
            (7, false, false)
        );
        // Exactly on a cluster boundary belongs to the next cluster.
        let boundary = l.hit_test_point(Point::new(10.0, 5.0));
        assert_eq!((boundary.offset, boundary.trailing), (1, false));
        // Above the layout clamps to the first line.
        let above = l.hit_test_point(Point::new(15.0, -5.0));
        assert_eq!(
            (above.offset, above.trailing, above.inside),
            (1, true, false)
        );
    }

    #[test]
    fn caret_rects_follow_the_clusters() {
        let l = layout("hello world foo", 100.0);
        assert!(rect_close(
            l.caret_rect(0, false),
            Rect::new(0.0, 0.0, 0.0, 12.0)
        ));
        assert!(rect_close(
            l.caret_rect(3, false),
            Rect::new(30.0, 0.0, 0.0, 12.0)
        ));
        assert!(rect_close(
            l.caret_rect(3, true),
            Rect::new(40.0, 0.0, 0.0, 12.0)
        ));
        // End of line 0 (after the hanging space) versus start of line 1.
        assert!(rect_close(
            l.caret_rect(5, true),
            Rect::new(60.0, 0.0, 0.0, 12.0)
        ));
        assert!(rect_close(
            l.caret_rect(6, false),
            Rect::new(0.0, 12.0, 0.0, 12.0)
        ));
        // Past the end clamps to the end of the last line.
        assert!(rect_close(
            l.caret_rect(15, false),
            Rect::new(90.0, 12.0, 0.0, 12.0)
        ));
        assert!(rect_close(
            l.caret_rect(999, true),
            Rect::new(90.0, 12.0, 0.0, 12.0)
        ));
    }

    #[test]
    fn range_rects_split_per_line() {
        let l = layout("hello world foo", 100.0);
        // "lo wor": "lo " on line 0, "wor" on line 1.
        let rects = l.range_rects(3..9);
        assert_eq!(rects.len(), 2);
        assert!(
            rect_close(rects[0], Rect::new(30.0, 0.0, 30.0, 12.0)),
            "{:?}",
            rects[0]
        );
        assert!(
            rect_close(rects[1], Rect::new(0.0, 12.0, 30.0, 12.0)),
            "{:?}",
            rects[1]
        );
        // A range on the second line only skips the first.
        let second = l.range_rects(7..9);
        assert_eq!(second.len(), 1);
        assert!(rect_close(second[0], Rect::new(10.0, 12.0, 20.0, 12.0)));
        // Empty and reversed ranges select nothing.
        assert!(l.range_rects(4..4).is_empty());
        assert!(
            l.range_rects(std::ops::Range { start: 9, end: 3 })
                .is_empty()
        );
        // A range outside the text selects nothing.
        assert!(l.range_rects(100..200).is_empty());
    }

    #[test]
    fn turkish_text_uses_one_advance_per_letter_with_byte_offsets() {
        // Every letter is two bytes in UTF-8.
        let text = "ğüşıöç";
        assert_eq!(text.len(), 12);
        let l = layout(text, f32::INFINITY);
        assert_eq!(l.line_count(), 1);
        assert!(close(l.size().width, 60.0));
        assert!(close(l.caret_rect(4, false).x, 20.0));
        assert!(close(l.caret_rect(4, true).x, 30.0));
        // An offset inside a cluster snaps to that cluster.
        assert!(close(l.caret_rect(5, false).x, 20.0));
        assert!(close(l.caret_rect(12, false).x, 60.0));
        let hit = l.hit_test_point(Point::new(25.0, 5.0));
        assert_eq!((hit.offset, hit.trailing, hit.length), (4, true, 2));
        assert_eq!(hit.insertion_offset(), 6);
        let before_mid = l.hit_test_point(Point::new(24.0, 5.0));
        assert_eq!((before_mid.offset, before_mid.trailing), (4, false));
        let rects = l.range_rects(2..8);
        assert_eq!(rects.len(), 1);
        assert!(rect_close(rects[0], Rect::new(10.0, 0.0, 30.0, 12.0)));

        // Dotted capital İ and a decomposed i + combining dot are one letter each.
        let dotted = "İi\u{307}";
        assert_eq!(dotted.len(), 5);
        let d = layout(dotted, f32::INFINITY);
        assert!(close(d.size().width, 20.0));
        assert!(close(d.caret_rect(2, false).x, 10.0));
        assert!(close(d.caret_rect(3, false).x, 10.0));
        assert!(close(d.caret_rect(5, false).x, 20.0));
    }

    #[test]
    fn emoji_sequences_are_single_clusters() {
        // Thumbs up + skin tone (8 bytes) between ASCII letters.
        let l = layout("a👍🏽b", f32::INFINITY);
        assert_eq!("a👍🏽b".len(), 10);
        assert!(close(l.size().width, 30.0));
        let hit = l.hit_test_point(Point::new(12.0, 5.0));
        assert_eq!((hit.offset, hit.trailing, hit.length), (1, false, 8));
        assert!(close(l.caret_rect(9, false).x, 20.0));
        let rects = l.range_rects(1..9);
        assert_eq!(rects.len(), 1);
        assert!(rect_close(rects[0], Rect::new(10.0, 0.0, 10.0, 12.0)));

        // A ZWJ family (18 bytes) and a flag (8 bytes) are one cluster each.
        let text = "👨\u{200D}👩\u{200D}👧🇹🇷x";
        assert_eq!(text.len(), 27);
        let l = layout(text, f32::INFINITY);
        assert!(close(l.size().width, 30.0));
        let flag = l.hit_test_point(Point::new(15.0, 5.0));
        assert_eq!((flag.offset, flag.length), (18, 8));
        assert!(close(l.caret_rect(26, false).x, 20.0));
        assert!(close(l.caret_rect(27, false).x, 30.0));
    }

    #[test]
    fn cjk_without_spaces_wraps_by_grapheme() {
        let text = "漢字かな";
        assert_eq!(text.len(), 12);
        let l = layout(text, 25.0);
        assert_eq!(l.line_count(), 2);
        let first = l.line_metrics(0).expect("line 0");
        let second = l.line_metrics(1).expect("line 1");
        assert_eq!((first.start, first.end), (0, 6));
        assert_eq!((second.start, second.end), (6, 12));
        assert!(close(l.size().width, 20.0));
        let hit = l.hit_test_point(Point::new(15.0, 15.0));
        assert_eq!((hit.offset, hit.trailing, hit.length), (9, true, 3));
        assert!(rect_close(
            l.caret_rect(6, false),
            Rect::new(0.0, 12.0, 0.0, 12.0)
        ));
        // A line fits at least one grapheme even when the width is zero.
        let narrow = layout("ab", 0.0);
        assert_eq!(narrow.line_count(), 2);
    }

    #[test]
    fn long_words_break_by_grapheme_and_keep_trailing_spaces() {
        let l = layout("abcdefghij klm", 45.0);
        let lines: Vec<(usize, usize)> = (0..l.line_count())
            .map(|i| {
                let m = l.line_metrics(i).expect("line");
                (m.start, m.end)
            })
            .collect();
        assert_eq!(lines, vec![(0, 4), (4, 8), (8, 11), (11, 14)]);

        // Trailing spaces hang: they widen the caret range but not the reported width.
        let spaced = layout("hi   ", 100.0);
        assert!(close(spaced.size().width, 20.0));
        assert!(close(spaced.caret_rect(5, false).x, 50.0));
        // A space-only prefix followed by a word that does not fit ends its own line.
        let lead = layout("   wide", 40.0);
        assert_eq!(lead.line_count(), 2);
    }

    #[test]
    fn hard_line_breaks_form_lines_with_and_without_content() {
        let l = layout("ab\ncd\n", f32::INFINITY);
        assert_eq!(l.line_count(), 3);
        assert!(close(l.size().width, 20.0));
        assert!(close(l.size().height, 36.0));
        let first = l.line_metrics(0).expect("line 0");
        assert_eq!(
            (
                first.start,
                first.end,
                first.newline_len,
                first.content_end()
            ),
            (0, 3, 1, 2)
        );
        let last = l.line_metrics(2).expect("line 2");
        assert_eq!((last.start, last.end, last.newline_len), (6, 6, 0));

        assert!(rect_close(
            l.caret_rect(2, false),
            Rect::new(20.0, 0.0, 0.0, 12.0)
        ));
        // Trailing on the line break is the start of the next line.
        assert!(rect_close(
            l.caret_rect(2, true),
            Rect::new(0.0, 12.0, 0.0, 12.0)
        ));
        assert!(rect_close(
            l.caret_rect(6, false),
            Rect::new(0.0, 24.0, 0.0, 12.0)
        ));

        let past = l.hit_test_point(Point::new(100.0, 5.0));
        assert_eq!(
            (past.offset, past.trailing, past.inside, past.length),
            (2, false, false, 1)
        );
        let second_line = l.hit_test_point(Point::new(4.0, 17.0));
        assert_eq!((second_line.offset, second_line.trailing), (3, false));
        let empty_last = l.hit_test_point(Point::new(5.0, 30.0));
        assert_eq!(
            (empty_last.offset, empty_last.trailing, empty_last.length),
            (6, false, 0)
        );
        assert_eq!(l.line_for_offset(6), 2);

        // Selecting across the breaks: the break itself is visible as one extra advance.
        let rects = l.range_rects(1..5);
        assert_eq!(rects.len(), 2);
        assert!(
            rect_close(rects[0], Rect::new(10.0, 0.0, 20.0, 12.0)),
            "{:?}",
            rects[0]
        );
        assert!(
            rect_close(rects[1], Rect::new(0.0, 12.0, 20.0, 12.0)),
            "{:?}",
            rects[1]
        );
        // An empty line in the middle is selectable through its break.
        let blank = layout("a\n\nb", f32::INFINITY);
        assert_eq!(blank.line_count(), 3);
        let rects = blank.range_rects(2..3);
        assert_eq!(rects.len(), 1);
        assert!(rect_close(rects[0], Rect::new(0.0, 12.0, 10.0, 12.0)));
    }

    #[test]
    fn crlf_is_one_line_break() {
        let l = layout("a\r\nb", f32::INFINITY);
        assert_eq!(l.line_count(), 2);
        let first = l.line_metrics(0).expect("line 0");
        assert_eq!((first.end, first.newline_len), (3, 2));
        assert!(close(l.caret_rect(1, false).x, 10.0));
        assert!(rect_close(
            l.caret_rect(1, true),
            Rect::new(0.0, 12.0, 0.0, 12.0)
        ));
        let past = l.hit_test_point(Point::new(100.0, 5.0));
        assert_eq!((past.offset, past.length), (1, 2));
        let lone_cr = layout("a\rb", f32::INFINITY);
        assert_eq!(lone_cr.line_count(), 2);
    }

    #[test]
    fn inline_objects_reserve_width_and_grow_the_line() {
        let text = "a\u{FFFC}b";
        let tall = InlineObject {
            at: 1,
            size: Size::new(20.0, 30.0),
            baseline: 25.0,
            id: 7,
        };
        let l = engine().layout(text, &style(), &[], &[tall], f32::INFINITY);
        // Ascent 25 (object) + descent 5 (object below baseline).
        let line = l.line_metrics(0).expect("line 0");
        assert!(
            close(line.height, 30.0) && close(line.baseline, 25.0),
            "{line:?}"
        );
        assert!(close(l.size().width, 40.0));
        assert!(close(l.caret_rect(4, false).x, 30.0));
        assert_eq!(
            l.inline_object_rect(7),
            Some(Rect::new(10.0, 0.0, 20.0, 30.0))
        );
        assert_eq!(l.inline_object_rect(8), None);

        // A small object sits on the baseline inside the normal line box.
        let small = InlineObject {
            at: 1,
            size: Size::new(10.0, 8.0),
            baseline: 6.0,
            id: 1,
        };
        let l = engine().layout(text, &style(), &[], &[small], f32::INFINITY);
        let line = l.line_metrics(0).expect("line 0");
        assert!(
            close(line.height, 12.0) && close(line.baseline, 10.0),
            "{line:?}"
        );
        assert_eq!(
            l.inline_object_rect(1),
            Some(Rect::new(10.0, 4.0, 10.0, 8.0))
        );

        // Objects that do not start a grapheme (or sit on a line break) are ignored.
        let stray = InlineObject {
            at: 2,
            size: Size::new(50.0, 50.0),
            baseline: 40.0,
            id: 2,
        };
        let on_break = InlineObject {
            at: 1,
            size: Size::new(50.0, 50.0),
            baseline: 40.0,
            id: 3,
        };
        let l = engine().layout(text, &style(), &[], &[stray], f32::INFINITY);
        assert_eq!(l.inline_object_rect(2), None);
        assert!(close(l.size().width, 30.0));
        let l = engine().layout("a\nb", &style(), &[], &[on_break], f32::INFINITY);
        assert_eq!(l.inline_object_rect(3), None);
    }

    #[test]
    fn line_height_follows_the_base_font_size_and_engine_default_advance() {
        let big = TextStyle::new(FAMILY, 20.0);
        let l = engine().layout("a\nb", &big, &[], &[], f32::INFINITY);
        assert!(close(l.size().height, 48.0));
        let default_engine = MonoTextEngine::default();
        let l = default_engine.layout("ab", &style(), &[], &[], f32::INFINITY);
        assert!(close(l.size().width, 16.0));
        // Ranges are accepted but do not change the mono metrics.
        let ranges = [StyledRange {
            range: 0..1,
            style: style().with_underline(true),
        }];
        let l = default_engine.layout("ab", &style(), &ranges, &[], f32::INFINITY);
        assert!(close(l.size().width, 16.0));
    }

    #[test]
    fn layouts_can_be_downcast_by_platform_code() {
        let l = layout("x", 10.0);
        assert!(l.as_any().downcast_ref::<MonoLayout>().is_some());
        assert!(l.as_any().downcast_ref::<String>().is_none());
    }
}
