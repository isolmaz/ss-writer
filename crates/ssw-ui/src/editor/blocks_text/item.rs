//! One item of a list (or a footnote definition): a marker column and the child blocks beside
//! it.
//!
//! While the caret is not in the item's first block, the marker source (`- `, `1. `, `- [ ] `,
//! `[^id]: `) is replaced by its rendering: a dot, ring or square, a right-aligned number, a
//! rounded checkbox or the footnote label. While the caret touches the first block, or with
//! *Auto-hide Markdown* off, the source shows dimmed instead and the content moves right by its
//! width.

use ssw_core::markdown::ast::Block;
use ssw_core::text::TextRange;

use crate::canvas::{Canvas, IconFont};
use crate::editor::block::{BlockLayout, BlockPaintCx, TaskBox};
use crate::editor::config::EditorStrings;
use crate::geometry::{CornerRadii, Point, Rect};
use crate::text::TextLayout;

use super::stack::{self, Spec, Stack, forward_backdrop, forward_links};
use super::{Ctx, MIN_WIDTH, style};

/// How the marker of an item is drawn while its source is hidden.
pub(super) enum Shape {
    Dot,
    Ring,
    Square,
    /// An ordered-list number, right-aligned in the marker column.
    Number(Box<dyn TextLayout>),
    /// A task checkbox; `true` when checked.
    Check(bool),
    /// A footnote label, right-aligned in the marker column.
    Label(Box<dyn TextLayout>),
}

/// The dimmed source of the marker zone.
struct Raw {
    layout: Box<dyn TextLayout>,
    /// Bytes of the layout text that are the `[ ]` / `[x]` of a task.
    checkbox: Option<std::ops::Range<usize>>,
}

/// The marker column of an item.
struct Marker {
    /// The source bytes of the zone: from the item start to the first child.
    zone: TextRange,
    shape: Shape,
    /// The `[ ]` / `[x]` source of a task.
    checkbox: Option<TextRange>,
    raw: Option<Raw>,
    /// Width of the column the content is moved right by.
    width: f32,
    /// The marker column of the shapes (without the room the raw source needs).
    col: f32,
    line: f32,
    body: f32,
}

impl Marker {
    /// The rectangle of the drawn checkbox.
    fn box_rect(&self) -> Rect {
        let side = (0.95 * self.body).max(8.0);
        Rect::new(
            self.col - 0.4 * self.body - side,
            (self.line - side) / 2.0,
            side,
            side,
        )
    }

    fn task_box(&self) -> Option<TaskBox> {
        let checkbox = self.checkbox?;
        let rect = match &self.raw {
            Some(raw) => {
                let bytes = raw.checkbox.clone()?;
                raw.layout
                    .range_rects(bytes)
                    .into_iter()
                    .reduce(|a, b| a.union(b))?
            }
            None => self.box_rect(),
        };
        Some(TaskBox {
            rect: rect.outset(crate::geometry::Insets::uniform(2.0)),
            offset: checkbox.start,
        })
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        if let Some(raw) = &self.raw {
            canvas.draw_text_layout(raw.layout.as_ref(), Point::ZERO, tokens.marker);
            return;
        }
        let center_x = self.col / 2.0;
        let center_y = self.line / 2.0;
        match &self.shape {
            Shape::Dot => {
                let diameter = 0.32 * self.body;
                canvas.fill_rounded_rect(
                    Rect::new(
                        center_x - diameter / 2.0,
                        center_y - diameter / 2.0,
                        diameter,
                        diameter,
                    ),
                    CornerRadii::uniform(diameter / 2.0),
                    tokens.accent,
                );
            }
            Shape::Ring => {
                let diameter = 0.4 * self.body;
                canvas.stroke_rounded_rect(
                    Rect::new(
                        center_x - diameter / 2.0,
                        center_y - diameter / 2.0,
                        diameter,
                        diameter,
                    ),
                    CornerRadii::uniform(diameter / 2.0),
                    tokens.accent,
                    1.3,
                );
            }
            Shape::Square => {
                let side = 0.3 * self.body;
                canvas.fill_rect(
                    Rect::new(center_x - side / 2.0, center_y - side / 2.0, side, side),
                    tokens.accent,
                );
            }
            Shape::Number(layout) => {
                let x = self.col - 0.4 * self.body - layout.size().width;
                canvas.draw_text_layout(layout.as_ref(), Point::new(x, 0.0), tokens.text_muted);
            }
            Shape::Label(layout) => {
                let x = self.col - 0.4 * self.body - layout.size().width;
                canvas.draw_text_layout(layout.as_ref(), Point::new(x, 0.0), tokens.link);
            }
            Shape::Check(checked) => {
                let rect = self.box_rect();
                let radii = CornerRadii::uniform(4.0);
                if *checked {
                    canvas.fill_rounded_rect(rect, radii, tokens.accent);
                    canvas.draw_icon(
                        '\u{E73E}',
                        IconFont::SegoeFluent,
                        rect.height * 0.75,
                        tokens.accent_text_on,
                        rect.center(),
                    );
                } else {
                    canvas.stroke_rounded_rect(rect, radii, tokens.text_muted, 1.5);
                }
            }
        }
    }

    /// The source offset a click at `point` in the marker column goes to.
    fn hit(&self, point: Point) -> usize {
        match &self.raw {
            Some(raw) => {
                let hit = raw.layout.hit_test_point(point);
                (self.zone.start + hit.insertion_offset()).min(self.zone.end)
            }
            None => self.zone.end,
        }
    }

    fn caret(&self, offset: usize) -> Rect {
        match &self.raw {
            Some(raw) => {
                let at = offset.saturating_sub(self.zone.start).min(self.zone.len());
                raw.layout.caret_rect(at, false)
            }
            None => Rect::new(self.width, 0.0, 0.0, self.line),
        }
    }

    fn selection(&self, range: TextRange) -> Vec<Rect> {
        let Some(part) = range.intersection(self.zone) else {
            return Vec::new();
        };
        match &self.raw {
            Some(raw) => raw
                .layout
                .range_rects(part.start - self.zone.start..part.end - self.zone.start),
            None => vec![Rect::new(0.0, 0.0, self.width, self.line)],
        }
    }
}

/// What [`measure`] needs to know about an item.
pub(super) struct ItemSpec<'a> {
    pub block: &'a Block,
    pub shape: Shape,
    /// Width of the marker column of the shapes.
    pub col: f32,
    /// The `[ ]` / `[x]` source of a task item.
    pub checkbox: Option<TextRange>,
    /// The task is done: its text is muted.
    pub checked: bool,
    /// Accessible description.
    pub label: String,
}

/// The layout of one item.
pub(super) struct ItemLayout {
    marker: Marker,
    stack: Stack,
    range: TextRange,
    /// Where the first child starts (the end of the marker zone).
    content_start: usize,
    height: f32,
    label: String,
}

/// Lays the item described by `spec` out in `ctx.cx.width`.
pub(super) fn measure(ctx: &Ctx<'_>, spec: ItemSpec<'_>) -> ItemLayout {
    let cx = ctx.cx;
    let block = spec.block;
    let tokens = &cx.theme.tokens;
    let body = cx.config.body_size();
    let line = cx.config.line_px();
    let content_start = block
        .children
        .first()
        .map_or(block.range.end, |child| child.range.start)
        .max(block.range.start);
    let zone = TextRange::new(block.range.start, content_start);
    let lead = block.children.first().map_or(block.range, |child| {
        TextRange::new(block.range.start, child.range.end)
    });
    let raw = cx.reveal.touches(lead).then(|| {
        let text = cx.buffer.slice(zone);
        let layout = cx.text.layout(
            &text,
            &style(cx, &cx.config.text_font, body),
            &[],
            &[],
            f32::INFINITY,
        );
        Raw {
            layout,
            checkbox: spec
                .checkbox
                .map(|checkbox| checkbox.start - zone.start..checkbox.end - zone.start),
        }
    });
    let width = raw.as_ref().map_or(spec.col, |raw| {
        let advance = raw
            .layout
            .size()
            .width
            .max(raw.layout.caret_rect(raw.layout.text().len(), false).x);
        spec.col.max(advance + 0.3 * body)
    });
    let marker = Marker {
        zone,
        shape: spec.shape,
        checkbox: spec.checkbox,
        raw,
        width,
        col: spec.col,
        line,
        body,
    };
    let item_ctx = Ctx {
        depth: ctx.depth + 1,
        color: if spec.checked {
            tokens.text_muted
        } else {
            tokens.text
        },
        ..*ctx
    };
    let stack = stack::build(
        &item_ctx,
        &Spec {
            children: &block.children,
            from: content_start,
            first_gap_line: cx.buffer.line_of(block.range.start) + 1,
            until: block.range.end,
            x: marker.width,
            y: 0.0,
            width: (cx.width - marker.width).max(MIN_WIDTH),
            show_prefix: ctx.shown,
        },
        &|child_ctx, _, child| super::measure_block(child_ctx, child),
    );
    let height = stack.bottom().max(line);
    ItemLayout {
        marker,
        stack,
        range: block.range,
        content_start,
        height,
        label: spec.label,
    }
}

impl BlockLayout for ItemLayout {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        self.marker.paint(cx, canvas);
        self.stack.paint(cx, canvas);
    }

    forward_backdrop!(stack);

    fn hit_test(&self, point: Point) -> usize {
        if point.x < self.marker.width && point.y < self.marker.line {
            return self.marker.hit(point);
        }
        if self.stack.is_empty() {
            return self.range.end;
        }
        self.stack.hit_test(point)
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        if offset < self.content_start {
            self.marker.caret(offset)
        } else {
            self.stack.caret_rect(offset)
        }
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        let mut rects = self.marker.selection(range);
        rects.extend(self.stack.selection_rects(range));
        rects
    }

    fn line_range(&self, offset: usize) -> TextRange {
        if self.stack.is_empty() {
            return self.range;
        }
        let line = self.stack.line_range(offset.max(self.content_start));
        if line.start <= self.content_start {
            TextRange::new(self.range.start, line.end.max(self.range.start))
        } else {
            line
        }
    }

    fn task_box_at(&self, point: Point) -> Option<TaskBox> {
        if let Some(found) = self.marker.task_box()
            && found.rect.contains(point)
        {
            return Some(found);
        }
        self.stack.task_box_at(point)
    }

    forward_links!(stack);

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.label.clone()
    }
}
