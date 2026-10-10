//! The display text of a block and its map to the source.
//!
//! A paragraph or heading is shown as *display text*: the source text with the syntax markers
//! of inline elements removed (unless the caret or selection touches the element), entities
//! decoded and images/math replaced by one object placeholder. [`DisplayMap`] translates between
//! source offsets (what the document and the caret use) and display offsets (what the text
//! layout uses). [`build`] walks the inline tree and produces the display text, its styles and
//! the decorations the painter needs (pills, links, objects, IME composition).

use std::ops::Range;
use std::sync::Arc;

use ssw_core::markdown::ast::{self, Inline, InlineKind};
use ssw_core::text::TextRange;

use crate::canvas::Canvas;
use crate::geometry::{Color, CornerRadii, Point, Rect};
use crate::input::ImeAttrStyle;
use crate::text::{FontWeight, InlineObject, StyledRange, TextLayout, TextLayoutEngine, TextStyle};
use crate::theme::{HighlightColor, ThemeTokens};

use super::config::{Composition, LinkTarget};
use super::object::{InlineObjectProvider, ObjectKind, ObjectRequest, ObjectState, ObjectVisual};

/// The placeholder character an inline object occupies in the display text (3 UTF-8 bytes).
pub const OBJECT_PLACEHOLDER: char = '\u{FFFC}';

/// How a stretch of source text appears in the display text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Displayed one to one.
    Shown,
    /// Not displayed (a hidden marker); the display range is empty.
    Hidden,
    /// Displayed as a different text (decoded entity, object placeholder); the caret can only
    /// be at its ends.
    Atomic,
    /// IME composition text; the source range is empty and located at the insertion point.
    Composition,
}

/// A stretch of source text and where it appears in the display text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Source bytes.
    pub src: TextRange,
    /// Display bytes.
    pub disp: TextRange,
    /// How the stretch is displayed.
    pub kind: SegmentKind,
}

/// Source ↔ display offset translation of one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayMap {
    segments: Vec<Segment>,
    src: TextRange,
    len: usize,
}

impl DisplayMap {
    /// A map over `segments` (in source order), for blocks that build their display text from
    /// non-contiguous source lines (code and math inside quotes and lists). `src` is the source
    /// range covered and `len` the length of the display text.
    #[must_use]
    pub fn from_segments(segments: Vec<Segment>, src: TextRange, len: usize) -> Self {
        Self { segments, src, len }
    }

    /// The source range the map covers.
    #[must_use]
    pub fn source_range(&self) -> TextRange {
        self.src
    }

    /// Length of the display text in bytes.
    #[must_use]
    pub fn display_len(&self) -> usize {
        self.len
    }

    /// The segments in source order.
    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// The display offset a caret at source `offset` is drawn at. Offsets inside a hidden
    /// marker map to the display offset of the marker; offsets inside an atomic segment map
    /// to its nearer end.
    #[must_use]
    pub fn source_to_display(&self, offset: usize) -> usize {
        let offset = offset.clamp(self.src.start, self.src.end);
        for seg in &self.segments {
            match seg.kind {
                SegmentKind::Composition => {}
                SegmentKind::Hidden => {
                    if seg.src.touches(offset) {
                        return seg.disp.start;
                    }
                }
                SegmentKind::Shown => {
                    if seg.src.touches(offset) {
                        return seg.disp.start + (offset - seg.src.start);
                    }
                }
                SegmentKind::Atomic => {
                    if seg.src.touches(offset) {
                        let near_start = offset - seg.src.start <= seg.src.end - offset;
                        return if near_start {
                            seg.disp.start
                        } else {
                            seg.disp.end
                        };
                    }
                }
            }
        }
        self.len
    }

    /// The source offset a caret at display `offset` stands for (hit testing). At the border
    /// between two displayed segments the end of the earlier one wins, so a caret never lands
    /// inside a hidden marker that follows it; at display offset 0 it is the start of the first
    /// displayed segment.
    #[must_use]
    pub fn display_to_source(&self, offset: usize) -> usize {
        let offset = offset.min(self.len);
        let mut first_visible: Option<usize> = None;
        for seg in &self.segments {
            match seg.kind {
                SegmentKind::Hidden => {}
                SegmentKind::Shown => {
                    first_visible.get_or_insert(seg.src.start);
                    if seg.disp.start < offset && offset <= seg.disp.end {
                        return seg.src.start + (offset - seg.disp.start);
                    }
                }
                SegmentKind::Atomic => {
                    first_visible.get_or_insert(seg.src.start);
                    if seg.disp.start < offset && offset <= seg.disp.end {
                        let near_start = offset - seg.disp.start <= seg.disp.end - offset;
                        return if near_start {
                            seg.src.start
                        } else {
                            seg.src.end
                        };
                    }
                }
                SegmentKind::Composition => {
                    first_visible.get_or_insert(seg.src.start);
                    if seg.disp.start < offset && offset <= seg.disp.end {
                        return seg.src.start;
                    }
                }
            }
        }
        first_visible.unwrap_or(self.src.end)
    }
}

/// Which parts of a block are revealed: markers of elements touching a selection are shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reveal {
    all: bool,
    ranges: Vec<TextRange>,
}

impl Reveal {
    /// `all` reveals everything (Auto-hide Markdown off); `ranges` are the selections.
    #[must_use]
    pub fn new(all: bool, ranges: Vec<TextRange>) -> Self {
        Self { all, ranges }
    }

    /// True when `range` touches a selection with inclusive bounds (a caret at either edge
    /// counts) or everything is revealed.
    #[must_use]
    pub fn touches(&self, range: TextRange) -> bool {
        self.all || self.selection_touches(range)
    }

    /// True when `range` touches a selection with inclusive bounds, whatever `all` says.
    #[must_use]
    pub fn selection_touches(&self, range: TextRange) -> bool {
        self.ranges
            .iter()
            .any(|sel| sel.start <= range.end && range.start <= sel.end)
    }
}

/// What to do with source bytes no inline node covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapPolicy {
    /// Show them as plain text (whitespace the parser skipped).
    Raw,
    /// Hide them unless `reveal` is set (heading markers).
    Marker {
        /// Show them dimmed.
        reveal: bool,
    },
}

/// A rounded background behind a display range (highlight, inline code).
#[derive(Debug, Clone, PartialEq)]
pub struct Pill {
    /// Display bytes.
    pub disp: Range<usize>,
    /// Fill color.
    pub color: Color,
}

/// A clickable display range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    /// Display bytes.
    pub disp: Range<usize>,
    /// What the link points at.
    pub target: LinkTarget,
}

/// An object to draw at the position of an inline object placeholder.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectDraw {
    /// The id passed to the layout engine (`InlineObject::id`).
    pub id: u64,
    /// What to draw.
    pub visual: ObjectVisual,
}

/// The composition string inside the display text.
#[derive(Debug, Clone, PartialEq)]
pub struct CompositionDisplay {
    /// Display bytes of the composition string.
    pub range: Range<usize>,
    /// Caret as a display offset.
    pub caret: usize,
    /// Styled segments as display ranges.
    pub attrs: Vec<(Range<usize>, ImeAttrStyle)>,
}

/// Everything [`build`] produces for one block.
#[derive(Debug, Clone)]
pub struct Display {
    /// The text handed to the layout engine.
    pub text: String,
    /// Source ↔ display translation.
    pub map: DisplayMap,
    /// Style overrides over the base style.
    pub styles: Vec<StyledRange>,
    /// Inline object reservations for the layout engine.
    pub objects: Vec<InlineObject>,
    /// What to draw at each object.
    pub object_draws: Vec<ObjectDraw>,
    /// Rounded backgrounds.
    pub pills: Vec<Pill>,
    /// Clickable ranges.
    pub links: Vec<LinkSpan>,
    /// The IME composition, when it is displayed in this block.
    pub composition: Option<CompositionDisplay>,
}

impl Display {
    /// Lays the display text out with `base` and the collected styles and objects.
    #[must_use]
    pub fn layout(
        &self,
        engine: &dyn TextLayoutEngine,
        base: &TextStyle,
        width: f32,
    ) -> Box<dyn TextLayout> {
        engine.layout(&self.text, base, &self.styles, &self.objects, width)
    }

    /// The link under display offset `offset`, if any.
    #[must_use]
    pub fn link_at(&self, offset: usize) -> Option<&LinkSpan> {
        self.links
            .iter()
            .find(|link| link.disp.start <= offset && offset < link.disp.end)
    }

    /// Draws what goes behind the text: pills.
    pub fn paint_behind(&self, layout: &dyn TextLayout, canvas: &mut dyn Canvas) {
        for pill in &self.pills {
            for rect in layout.range_rects(pill.disp.clone()) {
                let rect = Rect::new(
                    rect.x - 2.0,
                    rect.y + rect.height * 0.1,
                    rect.width + 4.0,
                    rect.height * 0.8,
                );
                canvas.fill_rounded_rect(rect, CornerRadii::uniform(4.0), pill.color);
            }
        }
    }

    /// Draws what goes over the text: inline objects and the composition underline.
    pub fn paint_front(&self, layout: &dyn TextLayout, canvas: &mut dyn Canvas, color: Color) {
        for draw in &self.object_draws {
            let Some(rect) = layout.inline_object_rect(draw.id) else {
                continue;
            };
            match draw.visual {
                ObjectVisual::Scene(scene) => canvas.draw_scene(scene, rect),
                ObjectVisual::Image(image) => canvas.draw_image(image, rect, 1.0),
            }
        }
        if let Some(composition) = &self.composition {
            let mut segments: Vec<(Range<usize>, f32)> = composition
                .attrs
                .iter()
                .map(|(range, style)| (range.clone(), underline_width(*style)))
                .collect();
            if segments.is_empty() {
                segments.push((composition.range.clone(), 1.0));
            }
            for (range, width) in segments {
                for rect in layout.range_rects(range) {
                    let y = rect.bottom() - 1.0;
                    canvas.stroke_line(
                        Point::new(rect.x, y),
                        Point::new(rect.right(), y),
                        color,
                        width,
                        crate::canvas::Dash::Solid,
                    );
                }
            }
        }
    }
}

/// Underline thickness of an IME segment: the converted target is drawn thicker.
fn underline_width(style: ImeAttrStyle) -> f32 {
    match style {
        ImeAttrStyle::TargetConverted | ImeAttrStyle::TargetNotConverted => 2.0,
        _ => 1.0,
    }
}

/// The display of `comp` whose text starts at display offset `start`.
fn composition_display(comp: &Composition, start: usize) -> CompositionDisplay {
    CompositionDisplay {
        range: start..start + comp.text.len(),
        caret: start + floor_boundary(&comp.text, comp.caret),
        attrs: comp
            .attrs
            .iter()
            .map(|attr| (start + attr.range.start..start + attr.range.end, attr.style))
            .collect(),
    }
}

fn push_raw(
    text: &mut String,
    segments: &mut Vec<Segment>,
    src: &str,
    range: TextRange,
    from: usize,
    to: usize,
) {
    if from >= to {
        return;
    }
    let start = text.len();
    text.push_str(
        src.get(from - range.start..to - range.start)
            .unwrap_or_default(),
    );
    segments.push(Segment {
        src: TextRange::new(from, to),
        disp: TextRange::new(start, text.len()),
        kind: SegmentKind::Shown,
    });
}

/// Shows `src` (the text of `range`) unchanged, with `composition` inserted at its offset when
/// that lies inside `range`. Used by blocks without inline rendering.
#[must_use]
pub fn raw(src: &str, range: TextRange, composition: Option<&Composition>) -> Display {
    let split = composition
        .filter(|comp| range.touches(comp.at) && src.is_char_boundary(comp.at - range.start));
    let mut text = String::with_capacity(src.len());
    let mut segments = Vec::new();
    let mut shown_composition = None;
    let split_at = split.map_or(range.end, |comp| comp.at);
    push_raw(&mut text, &mut segments, src, range, range.start, split_at);
    if let Some(comp) = split {
        let start = text.len();
        text.push_str(&comp.text);
        segments.push(Segment {
            src: TextRange::at(comp.at),
            disp: TextRange::new(start, text.len()),
            kind: SegmentKind::Composition,
        });
        shown_composition = Some(composition_display(comp, start));
    }
    push_raw(&mut text, &mut segments, src, range, split_at, range.end);
    let len = text.len();
    Display {
        text,
        map: DisplayMap {
            segments,
            src: range,
            len,
        },
        styles: Vec::new(),
        objects: Vec::new(),
        object_draws: Vec::new(),
        pills: Vec::new(),
        links: Vec::new(),
        composition: shown_composition,
    }
}

/// Inputs of [`build`].
#[derive(Clone, Copy)]
pub struct DisplayParams<'a> {
    /// The source text of `range` (`src[0]` is the byte at `range.start`).
    pub src: &'a str,
    /// The block's source range.
    pub range: TextRange,
    /// The block's inline nodes.
    pub inlines: &'a [Inline],
    /// Theme colors.
    pub tokens: &'a ThemeTokens,
    /// Style of unformatted text of the block.
    pub base: &'a TextStyle,
    /// Code font family.
    pub mono_family: &'a Arc<str>,
    /// Which elements show their markers.
    pub reveal: &'a Reveal,
    /// What to do with bytes no node covers.
    pub gap: GapPolicy,
    /// Source of images and math.
    pub objects: &'a dyn InlineObjectProvider,
    /// Text column width, for object requests.
    pub max_width: f32,
    /// An IME composition to display inside this block.
    pub composition: Option<&'a Composition>,
}

impl std::fmt::Debug for DisplayParams<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisplayParams")
            .field("range", &self.range)
            .field("gap", &self.gap)
            .field("max_width", &self.max_width)
            .finish_non_exhaustive()
    }
}

/// Inherited inline formatting.
#[derive(Debug, Clone, Copy, Default)]
struct Fmt {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    mono: bool,
    link: bool,
    sup: bool,
    color: Option<Color>,
}

/// The theme highlight color of a Markdown highlight color; plain `==x==` is yellow.
fn theme_highlight(color: Option<ast::HighlightColor>) -> HighlightColor {
    match color {
        Some(ast::HighlightColor::Red) => HighlightColor::Red,
        Some(ast::HighlightColor::Orange) => HighlightColor::Orange,
        None | Some(ast::HighlightColor::Yellow) => HighlightColor::Yellow,
        Some(ast::HighlightColor::Green) => HighlightColor::Green,
        Some(ast::HighlightColor::Blue) => HighlightColor::Blue,
        Some(ast::HighlightColor::Purple) => HighlightColor::Purple,
    }
}

/// Largest char boundary of `text` not above `index`.
fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

struct Builder<'a> {
    p: DisplayParams<'a>,
    text: String,
    segments: Vec<Segment>,
    styles: Vec<StyledRange>,
    objects: Vec<InlineObject>,
    object_draws: Vec<ObjectDraw>,
    pills: Vec<Pill>,
    links: Vec<LinkSpan>,
    cursor: usize,
    composition: Option<CompositionDisplay>,
}

/// Builds the display text of one block.
#[must_use]
pub fn build(params: &DisplayParams<'_>) -> Display {
    let mut b = Builder {
        p: *params,
        text: String::new(),
        segments: Vec::new(),
        styles: Vec::new(),
        objects: Vec::new(),
        object_draws: Vec::new(),
        pills: Vec::new(),
        links: Vec::new(),
        cursor: params.range.start,
        composition: None,
    };
    b.walk_all(params.inlines, &Fmt::default(), params.gap);
    b.fill_to(params.range.end, params.gap);
    if b.composition.is_none()
        && let Some(comp) = params.composition
    {
        b.insert_composition(comp);
    }
    let len = b.text.len();
    Display {
        text: b.text,
        map: DisplayMap {
            segments: b.segments,
            src: params.range,
            len,
        },
        styles: b.styles,
        objects: b.objects,
        object_draws: b.object_draws,
        pills: b.pills,
        links: b.links,
        composition: b.composition,
    }
}

impl Builder<'_> {
    fn src_text(&self, range: TextRange) -> &str {
        let origin = self.p.range.start;
        range
            .start
            .checked_sub(origin)
            .zip(range.end.checked_sub(origin))
            .and_then(|(start, end)| self.p.src.get(start..end))
            .unwrap_or("")
    }

    fn style(&self, fmt: &Fmt) -> TextStyle {
        let base = self.p.base;
        let (family, size) = if fmt.mono {
            (self.p.mono_family.clone(), base.size * 0.9)
        } else if fmt.sup {
            (base.family.clone(), base.size * 0.75)
        } else {
            (base.family.clone(), base.size)
        };
        let mut style = TextStyle::new(family, size);
        style.weight = if fmt.bold {
            FontWeight::BOLD
        } else {
            base.weight
        };
        style.italic = fmt.italic || base.italic;
        style.underline = fmt.underline;
        style.strikethrough = fmt.strike;
        style.color = fmt.color.or(if fmt.link {
            Some(self.p.tokens.link)
        } else {
            base.color
        });
        if fmt.sup {
            style.baseline_shift = base.size * 0.35;
        }
        style
    }

    fn colored(&self, color: Color) -> TextStyle {
        self.style(&Fmt {
            color: Some(color),
            ..Fmt::default()
        })
    }

    fn dim(&self) -> TextStyle {
        self.colored(self.p.tokens.marker)
    }

    fn clip(&self, range: TextRange) -> Option<TextRange> {
        let start = range.start.max(self.cursor);
        (start < range.end).then(|| TextRange::new(start, range.end))
    }

    fn insert_composition(&mut self, comp: &Composition) {
        let start = self.text.len();
        self.text.push_str(&comp.text);
        let end = self.text.len();
        self.segments.push(Segment {
            src: TextRange::at(comp.at),
            disp: TextRange::new(start, end),
            kind: SegmentKind::Composition,
        });
        self.composition = Some(composition_display(comp, start));
    }

    fn push_shown(&mut self, range: TextRange, style: &TextStyle) -> Range<usize> {
        let start = self.text.len();
        let piece = self.src_text(range).to_owned();
        self.text.push_str(&piece);
        let end = self.text.len();
        self.segments.push(Segment {
            src: range,
            disp: TextRange::new(start, end),
            kind: SegmentKind::Shown,
        });
        if start < end && *style != *self.p.base {
            self.styles.push(StyledRange {
                range: start..end,
                style: style.clone(),
            });
        }
        self.cursor = range.end;
        start..end
    }

    /// Displays `range` one to one; returns its display range (a composition inserted inside
    /// it is part of the result).
    fn shown(&mut self, range: TextRange, style: &TextStyle) -> Range<usize> {
        let Some(range) = self.clip(range) else {
            let at = self.text.len();
            return at..at;
        };
        let start = self.text.len();
        match self.p.composition {
            Some(comp)
                if self.composition.is_none() && (range.start..=range.end).contains(&comp.at) =>
            {
                let at = comp.at;
                if range.start < at {
                    self.push_shown(TextRange::new(range.start, at), style);
                }
                self.insert_composition(comp);
                if at < range.end {
                    self.cursor = at;
                    self.push_shown(TextRange::new(at, range.end), style);
                }
                self.cursor = range.end;
            }
            _ => {
                self.push_shown(range, style);
            }
        }
        start..self.text.len()
    }

    fn hidden(&mut self, range: TextRange) {
        let Some(range) = self.clip(range) else {
            return;
        };
        let at = self.text.len();
        self.segments.push(Segment {
            src: range,
            disp: TextRange::at(at),
            kind: SegmentKind::Hidden,
        });
        self.cursor = range.end;
    }

    fn atomic(&mut self, range: TextRange, text: &str, style: &TextStyle) -> Range<usize> {
        let Some(range) = self.clip(range) else {
            let at = self.text.len();
            return at..at;
        };
        let start = self.text.len();
        self.text.push_str(text);
        let end = self.text.len();
        self.segments.push(Segment {
            src: range,
            disp: TextRange::new(start, end),
            kind: SegmentKind::Atomic,
        });
        if start < end && *style != *self.p.base {
            self.styles.push(StyledRange {
                range: start..end,
                style: style.clone(),
            });
        }
        self.cursor = range.end;
        start..end
    }

    fn marker(&mut self, range: TextRange, reveal: bool) {
        if reveal {
            let style = self.dim();
            self.shown(range, &style);
        } else {
            self.hidden(range);
        }
    }

    fn fill_to(&mut self, pos: usize, gap: GapPolicy) {
        if pos <= self.cursor {
            return;
        }
        let range = TextRange::new(self.cursor, pos);
        match gap {
            GapPolicy::Raw => {
                let style = self.style(&Fmt::default());
                self.shown(range, &style);
            }
            GapPolicy::Marker { reveal } => self.marker(range, reveal),
        }
    }

    fn walk_all(&mut self, nodes: &[Inline], fmt: &Fmt, gap: GapPolicy) {
        for node in nodes {
            self.fill_to(node.range.start, gap);
            self.walk(node, fmt);
        }
    }

    /// Shows only the node's content; everything else in its range is a marker.
    fn flat(&mut self, node: &Inline, style: &TextStyle, reveal: bool) -> Range<usize> {
        let content = TextRange::new(
            node.content.start.clamp(node.range.start, node.range.end),
            node.content.end.clamp(node.range.start, node.range.end),
        );
        let gap = GapPolicy::Marker { reveal };
        self.fill_to(content.start, gap);
        let shown = self.shown(content, style);
        self.fill_to(node.range.end, gap);
        shown
    }

    /// A node with an opening marker, children and closing markers. Returns the display
    /// range of the children.
    fn wrapper(&mut self, node: &Inline, fmt: &Fmt, reveal: bool) -> Range<usize> {
        let gap = GapPolicy::Marker { reveal };
        let (open, closers) = match node.markers.split_first() {
            Some((open, closers)) => (Some(*open), closers),
            None => (None, &[][..]),
        };
        if let Some(open) = open {
            self.fill_to(open.start, gap);
            self.marker(open, reveal);
        }
        let start = self.text.len();
        self.walk_all(&node.children, fmt, gap);
        let content_end = closers
            .first()
            .map_or(node.range.end, |closer| closer.start);
        self.fill_to(content_end, gap);
        let end = self.text.len();
        for closer in closers {
            self.fill_to(closer.start, gap);
            self.marker(*closer, reveal);
        }
        self.fill_to(node.range.end, gap);
        start..end
    }

    fn walk(&mut self, node: &Inline, fmt: &Fmt) {
        let reveal = self.p.reveal.touches(node.range);
        match &node.kind {
            InlineKind::Text => {
                let style = self.style(fmt);
                self.shown(node.range, &style);
            }
            InlineKind::Escape | InlineKind::HardBreak => {
                let style = self.style(fmt);
                self.flat(node, &style, reveal);
            }
            InlineKind::Entity { decoded } => {
                let style = self.style(fmt);
                if reveal {
                    self.shown(node.range, &style);
                } else {
                    self.atomic(node.range, decoded, &style);
                }
            }
            InlineKind::Emphasis => {
                self.wrapper(
                    node,
                    &Fmt {
                        italic: true,
                        ..*fmt
                    },
                    reveal,
                );
            }
            InlineKind::Strong => {
                self.wrapper(node, &Fmt { bold: true, ..*fmt }, reveal);
            }
            InlineKind::Underline => {
                self.wrapper(
                    node,
                    &Fmt {
                        underline: true,
                        ..*fmt
                    },
                    reveal,
                );
            }
            InlineKind::Strikethrough => {
                self.wrapper(
                    node,
                    &Fmt {
                        strike: true,
                        ..*fmt
                    },
                    reveal,
                );
            }
            InlineKind::Highlight { color } => {
                let style = self.p.tokens.highlight.get(theme_highlight(*color));
                let range = self.wrapper(
                    node,
                    &Fmt {
                        color: Some(style.text),
                        ..*fmt
                    },
                    reveal,
                );
                if range.start < range.end {
                    self.pills.push(Pill {
                        disp: range,
                        color: style.background,
                    });
                }
            }
            InlineKind::Code { .. } => {
                let style = self.style(&Fmt { mono: true, ..*fmt });
                // A code span's content is never empty: a closing run of backticks directly
                // after the opening one would just be a longer opening run.
                let range = self.flat(node, &style, reveal);
                self.pills.push(Pill {
                    disp: range,
                    color: self.p.tokens.code_bg,
                });
            }
            InlineKind::Link(link) => {
                let range = self.wrapper(node, &Fmt { link: true, ..*fmt }, reveal);
                if range.start < range.end {
                    self.links.push(LinkSpan {
                        disp: range,
                        target: LinkTarget::Url(link.dest.clone()),
                    });
                }
            }
            InlineKind::Image(image) => {
                let alt = self.src_text(image.alt).to_owned();
                let request = ObjectRequest {
                    kind: ObjectKind::Image,
                    source: &image.dest,
                    alt: &alt,
                    width: image.width,
                    height: image.height,
                    font_size: self.p.base.size,
                    max_width: self.p.max_width,
                    text_color: self.p.tokens.text,
                    range: node.range,
                };
                self.object(node, &request, fmt, reveal);
            }
            InlineKind::InlineMath => {
                let tex = self.src_text(node.content).to_owned();
                let request = ObjectRequest {
                    kind: ObjectKind::Math,
                    source: &tex,
                    alt: "",
                    width: None,
                    height: None,
                    font_size: self.p.base.size,
                    max_width: self.p.max_width,
                    text_color: self.p.tokens.text,
                    range: node.range,
                };
                self.object(node, &request, fmt, reveal);
            }
            InlineKind::WikiLink(wiki) => {
                let style = self.style(&Fmt { link: true, ..*fmt });
                let range = self.flat(node, &style, reveal);
                if range.start < range.end {
                    self.links.push(LinkSpan {
                        disp: range,
                        target: LinkTarget::Wiki {
                            target: wiki.target.clone(),
                            heading: wiki.heading.clone(),
                        },
                    });
                }
            }
            InlineKind::FootnoteRef { label } => {
                let style = if reveal {
                    self.style(fmt)
                } else {
                    self.style(&Fmt {
                        sup: true,
                        link: true,
                        ..*fmt
                    })
                };
                // The parser only accepts a non-empty label, which is the node's content.
                let range = self.flat(node, &style, reveal);
                self.links.push(LinkSpan {
                    disp: range,
                    target: LinkTarget::Footnote(label.clone()),
                });
            }
            InlineKind::SoftBreak => {
                let style = self.style(fmt);
                self.shown(node.range, &style);
            }
            InlineKind::RawHtml => {
                let style = self.dim();
                self.shown(node.range, &style);
            }
        }
    }

    /// An image or inline math: an object placeholder when ready, otherwise its source.
    fn object(&mut self, node: &Inline, request: &ObjectRequest<'_>, fmt: &Fmt, reveal: bool) {
        let mono = matches!(request.kind, ObjectKind::Math);
        let source_fmt = Fmt { mono, ..*fmt };
        if reveal {
            let style = self.style(&source_fmt);
            self.flat(node, &style, true);
            return;
        }
        match self.p.objects.object(request) {
            ObjectState::Ready(info) => {
                let style = self.style(fmt);
                let mut placeholder = [0u8; 4];
                let text: &str = OBJECT_PLACEHOLDER.encode_utf8(&mut placeholder);
                let range = self.atomic(node.range, text, &style);
                let id = self.object_draws.len() as u64;
                self.objects.push(InlineObject {
                    at: range.start,
                    size: info.size,
                    baseline: info.baseline,
                    id,
                });
                self.object_draws.push(ObjectDraw {
                    id,
                    visual: info.visual,
                });
            }
            ObjectState::Pending | ObjectState::Blocked => {
                let style = self.style(&Fmt {
                    color: Some(self.p.tokens.text_muted),
                    ..source_fmt
                });
                self.shown(node.range, &style);
            }
            ObjectState::Failed => {
                let style = self.style(&Fmt {
                    color: Some(self.p.tokens.error),
                    ..source_fmt
                });
                self.shown(node.range, &style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{DrawOp, RecordingCanvas};
    use crate::editor::object::{NoObjects, ObjectInfo};
    use crate::geometry::Size;
    use crate::text::MonoTextEngine;
    use crate::theme::{DEFAULT_ACCENT, Theme};
    use ssw_core::document::Document;
    use ssw_core::markdown::ast::BlockKind;

    struct Fixed(ObjectState);

    impl InlineObjectProvider for Fixed {
        fn object(&self, _request: &ObjectRequest<'_>) -> ObjectState {
            self.0
        }
    }

    fn display(
        source: &str,
        selection: Option<(usize, usize)>,
        provider: &dyn InlineObjectProvider,
        composition: Option<&Composition>,
    ) -> Display {
        let doc = Document::new(source);
        let block = &doc.ast().blocks[0];
        let theme = Theme::light(DEFAULT_ACCENT);
        let base = TextStyle::new("Body", 10.0);
        let mono: Arc<str> = Arc::from("Mono");
        let reveal = Reveal::new(
            false,
            selection
                .map(|(a, b)| vec![TextRange::new(a, b)])
                .unwrap_or_default(),
        );
        let text = doc.buffer().slice(block.range).into_owned();
        let gap = if matches!(block.kind, BlockKind::Heading { .. }) {
            GapPolicy::Marker {
                reveal: reveal.touches(block.range),
            }
        } else {
            GapPolicy::Raw
        };
        build(&DisplayParams {
            src: &text,
            range: block.range,
            inlines: &block.inlines,
            tokens: &theme.tokens,
            base: &base,
            mono_family: &mono,
            reveal: &reveal,
            gap,
            objects: provider,
            max_width: 400.0,
            composition,
        })
    }

    #[test]
    fn markers_hide_until_the_caret_touches_the_element() {
        let hidden = display("a **bold** b", Some((0, 0)), &NoObjects, None);
        assert_eq!(hidden.text, "a bold b");
        let touching = display("a **bold** b", Some((2, 2)), &NoObjects, None);
        assert_eq!(touching.text, "a **bold** b");
        let end = display("a **bold** b", Some((10, 10)), &NoObjects, None);
        assert_eq!(end.text, "a **bold** b", "the closing edge touches too");
        let past = display("a **bold** b", Some((11, 11)), &NoObjects, None);
        assert_eq!(past.text, "a bold b");
    }

    #[test]
    fn caret_positions_map_through_hidden_markers() {
        let shown = display("a **bold** b", Some((0, 0)), &NoObjects, None);
        let map = &shown.map;
        // source: a=0, ' '=1, '**'=2..4, bold=4..8, '**'=8..10, ' '=10
        assert_eq!(map.source_to_display(0), 0);
        assert_eq!(map.source_to_display(4), 2);
        assert_eq!(map.source_to_display(6), 4);
        assert_eq!(map.source_to_display(8), 6);
        assert_eq!(map.source_to_display(9), 6, "inside a hidden marker");
        assert_eq!(map.source_to_display(11), 7);
        assert_eq!(map.display_to_source(0), 0);
        assert_eq!(
            map.display_to_source(2),
            2,
            "the end of the earlier segment wins"
        );
        assert_eq!(map.display_to_source(4), 6);
        assert_eq!(map.display_to_source(6), 8, "before the closing marker");
        assert_eq!(map.display_to_source(8), 12);
        assert_eq!(map.display_len(), 8);
        assert_eq!(map.source_range(), TextRange::new(0, 12));
        assert!(!map.segments().is_empty());
    }

    #[test]
    fn heading_markers_show_only_on_the_caret_block() {
        let away = display("## Title", None, &NoObjects, None);
        assert_eq!(away.text, "Title");
        assert_eq!(away.map.display_to_source(0), 3);
        let on = display("## Title", Some((4, 4)), &NoObjects, None);
        assert_eq!(on.text, "## Title");
    }

    #[test]
    fn styles_pills_and_links_are_collected() {
        let d = display(
            "**b** ==h== `c` [t](http://x) [[Note]] x[^1]\n\n[^1]: n",
            None,
            &NoObjects,
            None,
        );
        assert_eq!(d.text, "b h c t Note x1");
        assert_eq!(d.pills.len(), 2);
        assert_eq!(d.pills[0].disp, 2..3);
        assert_eq!(d.pills[1].disp, 4..5);
        assert_eq!(d.links.len(), 3);
        assert_eq!(d.links[0].target, LinkTarget::Url("http://x".into()));
        assert_eq!(
            d.links[1].target,
            LinkTarget::Wiki {
                target: "Note".into(),
                heading: None,
            }
        );
        assert_eq!(d.links[2].target, LinkTarget::Footnote("1".into()));
        assert!(d.link_at(6).is_some());
        assert!(d.link_at(5).is_none());
        let bold = d
            .styles
            .iter()
            .find(|s| s.range == (0..1))
            .expect("bold run");
        assert_eq!(bold.style.weight, FontWeight::BOLD);
        let sup = d.styles.last().expect("superscript run");
        assert!(sup.style.baseline_shift > 0.0);
    }

    #[test]
    fn elements_without_visible_content_add_no_pills_or_links() {
        // An empty link text, an empty wiki alias and a highlight wrapping only an empty link
        // all hide their markers and leave nothing to colour or click.
        let link = display("a[](http://x)b", None, &NoObjects, None);
        assert_eq!(link.text, "ab");
        assert!(link.links.is_empty());
        let wiki = display("a[[Note|]]b", None, &NoObjects, None);
        assert_eq!(wiki.text, "ab");
        assert!(wiki.links.is_empty());
        let highlight = display("a ==[](http://x)== b", None, &NoObjects, None);
        assert_eq!(highlight.text, "a  b");
        assert!(highlight.pills.is_empty());
        assert!(highlight.links.is_empty());
    }

    #[test]
    fn entities_and_escapes_display_their_result() {
        let d = display("a &amp; \\* b", None, &NoObjects, None);
        assert_eq!(d.text, "a & * b");
        let revealed = display("a &amp; \\* b", Some((3, 3)), &NoObjects, None);
        assert_eq!(revealed.text, "a &amp; * b");
        assert_eq!(d.map.source_to_display(3), 2, "atomic: nearer end");
        assert_eq!(d.map.source_to_display(7), 3);
        assert_eq!(d.map.display_to_source(3), 7);
    }

    #[test]
    fn objects_become_placeholders_only_when_ready() {
        let info = ObjectInfo {
            visual: ObjectVisual::Scene(crate::canvas::SceneId(7)),
            size: Size::new(30.0, 20.0),
            baseline: 15.0,
        };
        let ready = display("a $x^2$ b", None, &Fixed(ObjectState::Ready(info)), None);
        assert_eq!(ready.text, "a \u{FFFC} b");
        assert_eq!(ready.objects.len(), 1);
        assert_eq!(ready.objects[0].at, 2);
        assert_eq!(ready.object_draws[0].visual, info.visual);
        let touched = display(
            "a $x^2$ b",
            Some((3, 3)),
            &Fixed(ObjectState::Ready(info)),
            None,
        );
        assert_eq!(touched.text, "a $x^2$ b");
        assert!(touched.objects.is_empty());
        let pending = display("a $x^2$ b", None, &NoObjects, None);
        assert_eq!(pending.text, "a $x^2$ b");
        let failed = display("a $x^2$ b", None, &Fixed(ObjectState::Failed), None);
        assert_eq!(failed.text, "a $x^2$ b");
        let tokens = Theme::light(DEFAULT_ACCENT).tokens;
        assert_eq!(failed.styles[0].style.color, Some(tokens.error));
        assert_eq!(pending.styles[0].style.color, Some(tokens.text_muted));
        let image = display(
            "![alt|20](p.png)",
            None,
            &Fixed(ObjectState::Ready(info)),
            None,
        );
        assert_eq!(image.text, "\u{FFFC}");
    }

    #[test]
    fn composition_is_inserted_at_its_source_offset() {
        let comp = Composition {
            at: 3,
            text: "かな".into(),
            caret: 3,
            attrs: vec![crate::input::ImeAttr {
                range: 0..3,
                style: ImeAttrStyle::TargetConverted,
            }],
        };
        let d = display("abcdef", Some((3, 3)), &NoObjects, Some(&comp));
        assert_eq!(d.text, "abcかなdef");
        let shown = d.composition.as_ref().expect("composition");
        assert_eq!(shown.range, 3..9);
        assert_eq!(shown.caret, 6);
        assert_eq!(shown.attrs, vec![(3..6, ImeAttrStyle::TargetConverted)]);
        assert_eq!(d.map.source_to_display(4), 10);
        assert_eq!(d.map.display_to_source(10), 4);
        assert_eq!(d.map.display_to_source(4), 3, "inside the composition");
        let at_end = Composition {
            at: 6,
            text: "x".into(),
            caret: 1,
            attrs: vec![],
        };
        let end = display("abcdef", Some((6, 6)), &NoObjects, Some(&at_end));
        assert_eq!(end.text, "abcdefx");
        let empty = display("# ", Some((1, 1)), &NoObjects, Some(&at_end));
        assert!(empty.composition.is_some());
    }

    #[test]
    fn decorations_are_painted_behind_and_over_the_text() {
        let info = ObjectInfo {
            visual: ObjectVisual::Image(crate::canvas::ImageId(3)),
            size: Size::new(30.0, 20.0),
            baseline: 15.0,
        };
        let comp = Composition {
            at: 0,
            text: "ab".into(),
            caret: 1,
            attrs: vec![],
        };
        let d = display(
            "`c` ![i](p.png)",
            None,
            &Fixed(ObjectState::Ready(info)),
            Some(&comp),
        );
        let engine = MonoTextEngine::new(10.0);
        let layout = d.layout(&engine, &TextStyle::new("Body", 10.0), 400.0);
        let mut canvas = RecordingCanvas::new();
        d.paint_behind(layout.as_ref(), &mut canvas);
        d.paint_front(layout.as_ref(), &mut canvas, Color::BLACK);
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::FillRoundedRect { .. }))
        );
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::Image { .. })));
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::StrokeLine { .. }))
        );
    }

    #[test]
    fn an_object_the_layout_did_not_place_is_not_drawn() {
        let info = ObjectInfo {
            visual: ObjectVisual::Scene(crate::canvas::SceneId(7)),
            size: Size::new(30.0, 20.0),
            baseline: 15.0,
        };
        let d = display("a $x^2$ b", None, &Fixed(ObjectState::Ready(info)), None);
        assert_eq!(d.object_draws.len(), 1);
        // A layout built without the reservations knows no rectangle for the object.
        let engine = MonoTextEngine::new(10.0);
        let layout = engine.layout(&d.text, &TextStyle::new("Body", 10.0), &[], &[], 400.0);
        let mut canvas = RecordingCanvas::new();
        d.paint_front(layout.as_ref(), &mut canvas, Color::BLACK);
        assert_eq!(canvas.ops().count(), 0);
    }

    #[test]
    fn underline_strikethrough_and_colored_highlights_style_their_content() {
        let d = display("~u~ ~~s~~", None, &NoObjects, None);
        assert_eq!(d.text, "u s");
        let underline = d
            .styles
            .iter()
            .find(|s| s.range == (0..1))
            .expect("underline run");
        assert!(underline.style.underline && !underline.style.strikethrough);
        let strike = d
            .styles
            .iter()
            .find(|s| s.range == (2..3))
            .expect("strikethrough run");
        assert!(strike.style.strikethrough && !strike.style.underline);

        let source = "=={red}a== =={orange}b== =={green}c== =={blue}d== =={purple}e== ==f==";
        let d = display(source, None, &NoObjects, None);
        assert_eq!(d.text, "a b c d e f");
        let tokens = Theme::light(DEFAULT_ACCENT).tokens;
        let expected = [
            HighlightColor::Red,
            HighlightColor::Orange,
            HighlightColor::Green,
            HighlightColor::Blue,
            HighlightColor::Purple,
            HighlightColor::Yellow,
        ]
        .map(|color| tokens.highlight.get(color).background);
        let pills: Vec<Color> = d.pills.iter().map(|pill| pill.color).collect();
        assert_eq!(pills, expected);
    }

    #[test]
    fn a_touched_footnote_reference_shows_its_source_unraised() {
        let source = "x[^1]\n\n[^1]: n";
        let away = display(source, None, &NoObjects, None);
        assert_eq!(away.text, "x1");
        assert!(away.styles.iter().any(|s| s.style.baseline_shift > 0.0));
        let touched = display(source, Some((3, 3)), &NoObjects, None);
        assert_eq!(touched.text, "x[^1]");
        assert!(touched.styles.iter().all(|s| s.style.baseline_shift == 0.0));
        assert_eq!(touched.links[0].target, LinkTarget::Footnote("1".into()));
    }

    #[test]
    fn raw_text_takes_a_composition_at_its_offset() {
        let comp = Composition {
            at: 13,
            text: "かな".into(),
            caret: 4,
            attrs: vec![],
        };
        let d = raw("abcdef", TextRange::new(10, 16), Some(&comp));
        assert_eq!(d.text, "abcかなdef");
        let shown = d.composition.as_ref().expect("composition");
        assert_eq!(shown.range, 3..9);
        assert_eq!(shown.caret, 6, "a caret inside a character floors to it");
        assert_eq!(d.map.source_to_display(13), 3, "before the composition");
        assert_eq!(d.map.source_to_display(14), 10);
        assert_eq!(d.map.display_to_source(10), 14);

        let elsewhere = Composition { at: 99, ..comp };
        let d = raw("abcdef", TextRange::new(10, 16), Some(&elsewhere));
        assert_eq!(d.text, "abcdef");
        assert!(d.composition.is_none());
    }

    #[test]
    fn an_atomic_segment_maps_display_offsets_to_its_nearer_source_end() {
        let segment = |src: (usize, usize), disp: (usize, usize), kind| Segment {
            src: TextRange::new(src.0, src.1),
            disp: TextRange::new(disp.0, disp.1),
            kind,
        };
        let map = DisplayMap::from_segments(
            vec![
                segment((0, 2), (0, 2), SegmentKind::Shown),
                segment((2, 8), (2, 6), SegmentKind::Atomic),
                segment((8, 10), (6, 8), SegmentKind::Shown),
            ],
            TextRange::new(0, 10),
            8,
        );
        assert_eq!(map.display_to_source(3), 2, "nearer the start");
        assert_eq!(map.display_to_source(5), 8, "nearer the end");
        assert_eq!(map.display_to_source(7), 9, "past the atomic segment");
    }

    #[test]
    fn nodes_overlapping_text_already_shown_add_nothing() {
        let doc = Document::new("abcdef");
        let block = &doc.ast().blocks[0];
        let text = doc.buffer().slice(block.range).into_owned();
        let node = |kind, range: (usize, usize), markers: &[(usize, usize)]| Inline {
            kind,
            range: TextRange::new(range.0, range.1),
            content: TextRange::new(range.0, range.1),
            markers: markers
                .iter()
                .map(|marker| TextRange::new(marker.0, marker.1))
                .collect(),
            children: Vec::new(),
        };
        // The whole text first; then a text node, an entity and a strong element inside it.
        let inlines = vec![
            block.inlines[0].clone(),
            node(InlineKind::Text, (1, 3), &[]),
            node(
                InlineKind::Entity {
                    decoded: "&".into(),
                },
                (2, 4),
                &[],
            ),
            node(InlineKind::Strong, (2, 4), &[(2, 3), (3, 4)]),
        ];
        let theme = Theme::light(DEFAULT_ACCENT);
        let base = TextStyle::new("Body", 10.0);
        let mono: Arc<str> = Arc::from("Mono");
        let reveal = Reveal::new(false, Vec::new());
        let params = DisplayParams {
            src: &text,
            range: block.range,
            inlines: &inlines,
            tokens: &theme.tokens,
            base: &base,
            mono_family: &mono,
            reveal: &reveal,
            gap: GapPolicy::Raw,
            objects: &NoObjects,
            max_width: 400.0,
            composition: None,
        };
        let d = build(&params);
        assert_eq!(d.text, "abcdef", "nothing is shown twice");
        assert_eq!(
            d.map.segments().len(),
            1,
            "and nothing is hidden or replaced"
        );
        assert!(format!("{params:?}").contains("max_width: 400.0"));
    }
}
