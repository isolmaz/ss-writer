//! The child blocks of a container laid out one below the other.
//!
//! A [`Stack`] is made of rows: one per child block and one per blank source line between (or
//! after) children, so the caret can sit on blank lines inside a quote or a loose list. A row
//! may carry the dimmed source of the line prefix in front of its child (`> ` of a quote line)
//! while the enclosing container shows its markers. Rows know the source range they answer
//! for, which is how carets, hits and selections map between source and geometry.

use ssw_core::markdown::ast::Block;
use ssw_core::text::TextRange;

use crate::canvas::Canvas;
use crate::editor::block::{BlockCx, BlockLayout, BlockPaintCx, BlockPress, TaskBox};
use crate::editor::config::{EditorEvent, LinkTarget};
use crate::geometry::{Point, Rect};
use crate::input::{Cursor, Modifiers};
use crate::text::TextLayout;

use super::{Ctx, MIN_WIDTH, style};

/// What a row holds.
enum Body {
    Child(Box<dyn BlockLayout>),
    /// A blank source line.
    Blank,
}

/// The dimmed source of the line prefix in front of a row's child.
struct Prefix {
    range: TextRange,
    layout: Box<dyn TextLayout>,
    x: f32,
}

struct Row {
    /// The source the row answers for: the prefix and the child, or the blank line.
    range: TextRange,
    /// Where the child's own source starts (after the prefix).
    content_start: usize,
    top: f32,
    height: f32,
    /// Left edge of the child.
    x: f32,
    body: Body,
    prefix: Option<Prefix>,
}

/// The rows of a container, in container-local coordinates.
pub(super) struct Stack {
    rows: Vec<Row>,
    top: f32,
    bottom: f32,
    /// Left edge of the children.
    x: f32,
    /// Height of one empty line.
    line: f32,
    /// The source range the stack stands for when it has no rows.
    range: TextRange,
}

/// What [`build`] lays out.
pub(super) struct Spec<'a> {
    pub children: &'a [Block],
    /// The first source offset of the container's content: the prefix of the first child does
    /// not start before it.
    pub from: usize,
    /// The first line that may hold a blank row before the first child.
    pub first_gap_line: usize,
    /// The last source offset of the container: blank lines up to its line follow the last
    /// child.
    pub until: usize,
    /// Left edge of the rows (the prefix column).
    pub x: f32,
    /// Top of the first row.
    pub y: f32,
    /// Width available from `x`.
    pub width: f32,
    /// Show the dimmed prefix of every line.
    pub show_prefix: bool,
}

enum Entry<'a> {
    Child(&'a Block),
    Blank { line: usize, separator: bool },
}

/// Width of the text including its trailing spaces.
fn advance(layout: &dyn TextLayout) -> f32 {
    layout
        .size()
        .width
        .max(layout.caret_rect(layout.text().len(), false).x)
}

/// Lays the children described by `spec` out. `measure` lays out one child (its index among
/// `spec.children` and its block) in the context it is given, whose width is the child's.
pub(super) fn build(
    ctx: &Ctx<'_>,
    spec: &Spec<'_>,
    measure: &dyn Fn(&Ctx<'_>, usize, &Block) -> Box<dyn BlockLayout>,
) -> Stack {
    let cx = ctx.cx;
    let buffer = cx.buffer;
    let mut entries: Vec<Entry<'_>> = Vec::new();
    let mut next_line = spec.first_gap_line;
    for (index, child) in spec.children.iter().enumerate() {
        let first = buffer.line_of(child.range.start);
        for line in next_line..first {
            entries.push(Entry::Blank {
                line,
                separator: index > 0 && line == next_line,
            });
        }
        entries.push(Entry::Child(child));
        next_line = buffer.line_of(child.range.end) + 1;
    }
    for line in next_line..=buffer.line_of(spec.until) {
        entries.push(Entry::Blank {
            line,
            separator: !spec.children.is_empty() && line == next_line,
        });
    }

    let body = cx.config.body_size();
    let prefix_style = style(cx, &cx.config.text_font, body);
    let mut planned = Vec::with_capacity(entries.len());
    let mut prefix_width = 0.0f32;
    for entry in entries {
        let range = match &entry {
            Entry::Child(child) => {
                let line_start = buffer.line_range(buffer.line_of(child.range.start)).start;
                TextRange::new(
                    spec.from.max(line_start).min(child.range.start),
                    child.range.start,
                )
            }
            Entry::Blank { line, .. } => buffer.line_range(*line),
        };
        let text = buffer.slice(range);
        let layout = (spec.show_prefix && !text.trim().is_empty()).then(|| {
            cx.text
                .layout(&text, &prefix_style, &[], &[], f32::INFINITY)
        });
        if let Some(layout) = &layout {
            prefix_width = prefix_width.max(advance(layout.as_ref()));
        }
        planned.push((entry, range, layout));
    }

    let gap = if prefix_width > 0.0 {
        prefix_width + 0.3 * body
    } else {
        0.0
    };
    let x = spec.x + gap;
    let width = (spec.width - gap).max(MIN_WIDTH);
    let line = cx.config.line_px();
    let spacing = cx.config.spacing_px().max(1.0);
    let child_cx = BlockCx {
        width,
        folded: false,
        ..*cx
    };
    let child_ctx = Ctx {
        cx: &child_cx,
        strings: ctx.strings,
        depth: ctx.depth,
        color: ctx.color,
        shown: spec.show_prefix,
    };

    let mut rows = Vec::with_capacity(planned.len());
    let mut y = spec.y;
    let mut child_index = 0;
    for (entry, range, layout) in planned {
        let prefix = layout.map(|layout| Prefix {
            range,
            layout,
            x: spec.x,
        });
        match entry {
            Entry::Child(child) => {
                let layout = measure(&child_ctx, child_index, child);
                child_index += 1;
                let height = layout.height();
                rows.push(Row {
                    range: TextRange::new(range.start, child.range.end.max(range.start)),
                    content_start: child.range.start,
                    top: y,
                    height,
                    x,
                    body: Body::Child(layout),
                    prefix,
                });
                y += height;
            }
            Entry::Blank { separator, .. } => {
                let touched = prefix.is_some() || cx.reveal.selection_touches(range);
                let height = if separator && !touched { spacing } else { line };
                rows.push(Row {
                    range,
                    content_start: range.end,
                    top: y,
                    height,
                    x,
                    body: Body::Blank,
                    prefix,
                });
                y += height;
            }
        }
    }
    Stack {
        rows,
        top: spec.y,
        bottom: y,
        x,
        line,
        range: TextRange::new(spec.from, spec.until.max(spec.from)),
    }
}

impl Stack {
    /// Bottom edge of the last row (the top when there are none).
    pub(super) fn bottom(&self) -> f32 {
        self.bottom
    }

    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn row_at_y(&self, y: f32) -> Option<&Row> {
        let last = self.rows.len().checked_sub(1)?;
        let index = self
            .rows
            .partition_point(|row| row.top + row.height <= y)
            .min(last);
        self.rows.get(index)
    }

    fn row_for_offset(&self, offset: usize) -> Option<&Row> {
        let index = self
            .rows
            .partition_point(|row| row.range.start <= offset)
            .saturating_sub(1);
        self.rows.get(index)
    }

    /// The child under `point` with the point in the child's coordinates.
    fn child_at(&self, point: Point) -> Option<(&Row, &dyn BlockLayout, Point)> {
        let row = self.row_at_y(point.y)?;
        if !(row.top..row.top + row.height).contains(&point.y) {
            return None;
        }
        let Body::Child(child) = &row.body else {
            return None;
        };
        Some((
            row,
            child.as_ref(),
            Point::new(point.x - row.x, point.y - row.top),
        ))
    }

    /// Lets the children draw what goes under the selection (callout tints, cards).
    pub(super) fn paint_backdrop(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        for row in &self.rows {
            if let Body::Child(child) = &row.body {
                canvas.save();
                canvas.translate(row.x, row.top);
                child.paint_backdrop(cx, canvas);
                canvas.restore();
            }
        }
    }

    pub(super) fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        for row in &self.rows {
            if let Some(prefix) = &row.prefix {
                canvas.draw_text_layout(
                    prefix.layout.as_ref(),
                    Point::new(prefix.x, row.top),
                    cx.theme.tokens.marker,
                );
            }
            if let Body::Child(child) = &row.body {
                canvas.save();
                canvas.translate(row.x, row.top);
                child.paint(cx, canvas);
                canvas.restore();
            }
        }
    }

    pub(super) fn hit_test(&self, point: Point) -> usize {
        let Some(row) = self.row_at_y(point.y) else {
            return self.range.end;
        };
        if let Some(prefix) = &row.prefix
            && point.x < row.x
        {
            let hit = prefix
                .layout
                .hit_test_point(Point::new(point.x - prefix.x, point.y - row.top));
            return (prefix.range.start + hit.insertion_offset()).min(prefix.range.end);
        }
        match &row.body {
            Body::Child(child) => child.hit_test(Point::new(point.x - row.x, point.y - row.top)),
            Body::Blank => row.range.end,
        }
    }

    pub(super) fn caret_rect(&self, offset: usize) -> Rect {
        let Some(row) = self.row_for_offset(offset) else {
            return Rect::new(self.x, self.top, 0.0, self.line);
        };
        if let Some(prefix) = &row.prefix
            && (offset < row.content_start || matches!(row.body, Body::Blank))
        {
            let at = offset
                .saturating_sub(prefix.range.start)
                .min(prefix.range.len());
            return prefix
                .layout
                .caret_rect(at, false)
                .translate(prefix.x, row.top);
        }
        match &row.body {
            Body::Child(child) => child
                .caret_rect(offset.max(row.content_start))
                .translate(row.x, row.top),
            Body::Blank => Rect::new(row.x, row.top, 0.0, row.height),
        }
    }

    pub(super) fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        let mut rects = Vec::new();
        for row in &self.rows {
            if row.range.end < range.start || row.range.start > range.end {
                continue;
            }
            if let Some(prefix) = &row.prefix
                && let Some(part) = range.intersection(prefix.range)
            {
                let local = part.start - prefix.range.start..part.end - prefix.range.start;
                rects.extend(
                    prefix
                        .layout
                        .range_rects(local)
                        .into_iter()
                        .map(|rect| rect.translate(prefix.x, row.top)),
                );
            }
            match &row.body {
                Body::Child(child) => {
                    let content = TextRange::new(row.content_start, row.range.end);
                    if let Some(part) = range.intersection(content) {
                        rects.extend(
                            child
                                .selection_rects(part)
                                .into_iter()
                                .map(|rect| rect.translate(row.x, row.top)),
                        );
                    }
                }
                Body::Blank => {
                    if (range.start..range.end).contains(&row.range.end) {
                        rects.push(Rect::new(row.x, row.top, 6.0, row.height));
                    }
                }
            }
        }
        rects
    }

    pub(super) fn line_range(&self, offset: usize) -> TextRange {
        let Some(row) = self.row_for_offset(offset) else {
            return self.range;
        };
        match &row.body {
            Body::Child(child) => child.line_range(offset.max(row.content_start)),
            Body::Blank => row.range,
        }
    }

    pub(super) fn link_at(&self, point: Point) -> Option<LinkTarget> {
        let (_, child, local) = self.child_at(point)?;
        child.link_at(local)
    }

    pub(super) fn task_box_at(&self, point: Point) -> Option<TaskBox> {
        let (row, child, local) = self.child_at(point)?;
        child.task_box_at(local).map(|found| TaskBox {
            rect: found.rect.translate(row.x, row.top),
            offset: found.offset,
        })
    }

    pub(super) fn context_menu(&self, point: Point, origin: Point) -> Option<EditorEvent> {
        let (row, child, local) = self.child_at(point)?;
        child.context_menu(local, Point::new(origin.x + row.x, origin.y + row.top))
    }

    pub(super) fn press(&self, point: Point, origin: Point, mods: Modifiers) -> Option<BlockPress> {
        let (row, child, local) = self.child_at(point)?;
        child.press(
            local,
            Point::new(origin.x + row.x, origin.y + row.top),
            mods,
        )
    }

    pub(super) fn cursor_at(&self, point: Point) -> Option<Cursor> {
        let (_, child, local) = self.child_at(point)?;
        child.cursor_at(local)
    }
}

/// Implements the geometry methods of [`BlockLayout`] by forwarding to the `Stack` in `$field`.
macro_rules! forward_geometry {
    ($field:ident) => {
        fn hit_test(&self, point: $crate::geometry::Point) -> usize {
            self.$field.hit_test(point)
        }

        fn caret_rect(&self, offset: usize) -> $crate::geometry::Rect {
            self.$field.caret_rect(offset)
        }

        fn selection_rects(
            &self,
            range: ::ssw_core::text::TextRange,
        ) -> Vec<$crate::geometry::Rect> {
            self.$field.selection_rects(range)
        }

        fn line_range(&self, offset: usize) -> ::ssw_core::text::TextRange {
            self.$field.line_range(offset)
        }
    };
}

/// Implements the interaction methods of [`BlockLayout`] (links, context menu, presses, cursor)
/// by forwarding to the `Stack` in `$field`.
macro_rules! forward_links {
    ($field:ident) => {
        fn link_at(
            &self,
            point: $crate::geometry::Point,
        ) -> Option<$crate::editor::config::LinkTarget> {
            self.$field.link_at(point)
        }

        fn context_menu(
            &self,
            point: $crate::geometry::Point,
            origin: $crate::geometry::Point,
        ) -> Option<$crate::editor::config::EditorEvent> {
            self.$field.context_menu(point, origin)
        }

        fn press(
            &self,
            point: $crate::geometry::Point,
            origin: $crate::geometry::Point,
            mods: $crate::input::Modifiers,
        ) -> Option<$crate::editor::block::BlockPress> {
            self.$field.press(point, origin, mods)
        }

        fn cursor_at(&self, point: $crate::geometry::Point) -> Option<$crate::input::Cursor> {
            self.$field.cursor_at(point)
        }
    };
}

/// Implements [`BlockLayout::paint_backdrop`] by forwarding to the `Stack` in `$field`.
macro_rules! forward_backdrop {
    ($field:ident) => {
        fn paint_backdrop(
            &self,
            cx: &$crate::editor::block::BlockPaintCx<'_>,
            canvas: &mut dyn $crate::canvas::Canvas,
        ) {
            self.$field.paint_backdrop(cx, canvas);
        }
    };
}

/// Implements [`BlockLayout::task_box_at`] by forwarding to the `Stack` in `$field`.
macro_rules! forward_task_box {
    ($field:ident) => {
        fn task_box_at(
            &self,
            point: $crate::geometry::Point,
        ) -> Option<$crate::editor::block::TaskBox> {
            self.$field.task_box_at(point)
        }
    };
}

pub(super) use {forward_backdrop, forward_geometry, forward_links, forward_task_box};
