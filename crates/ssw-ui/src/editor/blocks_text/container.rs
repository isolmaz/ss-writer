//! Block quotes, callouts and footnote definitions.

use ssw_core::markdown::ast::{Block, CalloutKind};
use ssw_core::text::TextRange;

use crate::canvas::{Canvas, IconFont};
use crate::editor::block::{BlockLayout, BlockPaintCx, BlockPress, TaskBox};
use crate::editor::config::{EditorEvent, EditorStrings, LinkTarget};
use crate::geometry::{CornerRadii, Point, Rect};
use crate::input::{Cursor, Modifiers};
use crate::text::{FontWeight, StyledRange, TextLayout};
use crate::theme::CalloutKind as ThemeKind;

use super::item::{self, ItemSpec, Shape};
use super::stack::{
    self, Spec, Stack, forward_backdrop, forward_geometry, forward_links, forward_task_box,
};
use super::{Ctx, MIN_WIDTH, kind_index, measure_block, style};

/// Width of the bar at the left of a quote or callout.
const BAR: f32 = 3.0;

/// A block quote: a bar and the stack of its children.
struct QuoteLayout {
    stack: Stack,
    height: f32,
    label: String,
}

impl BlockLayout for QuoteLayout {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        canvas.fill_rounded_rect(
            Rect::new(0.0, 0.0, BAR, self.height),
            CornerRadii::uniform(BAR / 2.0),
            cx.theme.tokens.table_border,
        );
        self.stack.paint(cx, canvas);
    }

    forward_backdrop!(stack);
    forward_geometry!(stack);
    forward_task_box!(stack);
    forward_links!(stack);

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.label.clone()
    }
}

/// A block quote: the children beside a left bar, with the `>` of every line shown dimmed while
/// the caret is inside.
pub(super) fn quote(ctx: &Ctx<'_>, block: &Block) -> Box<dyn BlockLayout> {
    let cx = ctx.cx;
    let body = cx.config.body_size();
    let shown = ctx.shown || cx.reveal.touches(block.range);
    let indent = BAR + 0.7 * body;
    let quote_ctx = Ctx { shown, ..*ctx };
    let stack = stack::build(
        &quote_ctx,
        &Spec {
            children: &block.children,
            from: block.range.start,
            first_gap_line: cx.buffer.line_of(block.range.start),
            until: block.range.end,
            x: indent,
            y: 0.0,
            width: cx.width - indent,
            show_prefix: shown,
        },
        &|child_ctx, _, child| measure_block(child_ctx, child),
    );
    let height = stack.bottom().max(cx.config.line_px());
    Box::new(QuoteLayout {
        stack,
        height,
        label: ctx.strings.quote.clone(),
    })
}

/// The title line of a callout: the title text, or the whole first source line while the caret
/// is inside.
struct Header {
    layout: Box<dyn TextLayout>,
    /// The source offset of the first byte of the layout text.
    start: usize,
    /// The end of the first source line of the callout (title or tag).
    end: usize,
    /// The layout text is the default title, not source.
    synthetic: bool,
    origin: Point,
}

impl Header {
    fn display(&self, offset: usize) -> usize {
        if self.synthetic {
            0
        } else {
            offset
                .saturating_sub(self.start)
                .min(self.layout.text().len())
        }
    }

    fn hit(&self, point: Point) -> usize {
        if self.synthetic {
            return self.end;
        }
        let local = Point::new(point.x - self.origin.x, point.y - self.origin.y);
        let hit = self.layout.hit_test_point(local);
        (self.start + hit.insertion_offset()).min(self.end)
    }
}

/// A callout: a tinted card with a colored bar, the kind's icon, the title and the children.
struct CalloutLayout {
    range: TextRange,
    kind: CalloutKind,
    glyph: char,
    header: Header,
    icon: Rect,
    icon_hit: Rect,
    /// Top of the children.
    body_top: f32,
    stack: Stack,
    width: f32,
    height: f32,
    label: String,
}

fn theme_kind(kind: CalloutKind) -> ThemeKind {
    match kind {
        CalloutKind::Note => ThemeKind::Note,
        CalloutKind::Tip => ThemeKind::Tip,
        CalloutKind::Important => ThemeKind::Important,
        CalloutKind::Warning => ThemeKind::Warning,
        CalloutKind::Caution => ThemeKind::Caution,
    }
}

/// The Segoe Fluent Icons glyph of a callout kind (names from Microsoft's published list:
/// Info, Lightbulb, Important, Warning, Error).
fn glyph(kind: CalloutKind) -> char {
    match kind {
        CalloutKind::Note => '\u{E946}',
        CalloutKind::Tip => '\u{EA80}',
        CalloutKind::Important => '\u{E8C9}',
        CalloutKind::Warning => '\u{E7BA}',
        CalloutKind::Caution => '\u{E783}',
    }
}

/// A callout of `kind`; `tag` is the `[!KIND]` source and `title` the optional title source.
pub(super) fn callout(
    ctx: &Ctx<'_>,
    block: &Block,
    kind: CalloutKind,
    tag: TextRange,
    title: Option<TextRange>,
) -> Box<dyn BlockLayout> {
    let cx = ctx.cx;
    let tokens = &cx.theme.tokens;
    let body = cx.config.body_size();
    let line = cx.config.line_px();
    let revealed = cx.reveal.touches(block.range);
    let shown = ctx.shown || revealed;
    let header_end = title.map_or(tag.end, |title| title.end);
    let pad_x = 0.75 * body;
    let pad_y = 0.5 * body;
    let content_x = BAR + pad_x;
    let icon_size = body;
    let title_x = content_x + icon_size + 0.5 * body;
    let base = style(cx, &cx.config.text_font, body).with_weight(FontWeight::SEMIBOLD);
    let (text, start, synthetic, styles) = if revealed {
        let text = cx
            .buffer
            .slice(TextRange::new(block.range.start, header_end))
            .into_owned();
        let markup = title.map_or(text.len(), |title| title.start - block.range.start);
        let styles = vec![StyledRange {
            range: 0..markup,
            style: base.clone().with_color(tokens.marker),
        }];
        (text, block.range.start, false, styles)
    } else if let Some(title) = title {
        (
            cx.buffer.slice(title).into_owned(),
            title.start,
            false,
            Vec::new(),
        )
    } else {
        (
            ctx.strings.callout_titles[kind_index(kind)].clone(),
            header_end,
            true,
            Vec::new(),
        )
    };
    let layout = cx.text.layout(
        &text,
        &base,
        &styles,
        &[],
        (cx.width - title_x - pad_x).max(MIN_WIDTH),
    );
    let header_height = layout.size().height.max(line);
    let body_top = pad_y + header_height;
    let callout_ctx = Ctx { shown, ..*ctx };
    let stack = stack::build(
        &callout_ctx,
        &Spec {
            children: &block.children,
            from: header_end,
            first_gap_line: cx.buffer.line_of(header_end) + 1,
            until: block.range.end,
            x: content_x,
            y: body_top,
            width: cx.width - content_x - pad_x,
            show_prefix: shown,
        },
        &|child_ctx, _, child| measure_block(child_ctx, child),
    );
    let height = stack.bottom() + pad_y;
    Box::new(CalloutLayout {
        range: block.range,
        kind,
        glyph: glyph(kind),
        header: Header {
            layout,
            start,
            end: header_end,
            synthetic,
            origin: Point::new(title_x, pad_y),
        },
        icon: Rect::new(
            content_x,
            pad_y + (line - icon_size) / 2.0,
            icon_size,
            icon_size,
        ),
        icon_hit: Rect::new(content_x - 4.0, pad_y, icon_size + 8.0, line),
        body_top,
        stack,
        width: cx.width,
        height,
        label: ctx.strings.callout_access[kind_index(kind)].clone(),
    })
}

impl CalloutLayout {
    fn in_header(&self, offset: usize) -> bool {
        offset <= self.header.end || self.stack.is_empty()
    }
}

impl BlockLayout for CalloutLayout {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint_backdrop(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        let colors = cx.theme.tokens.callout.get(theme_kind(self.kind));
        canvas.fill_rounded_rect(
            Rect::new(0.0, 0.0, self.width, self.height),
            CornerRadii::uniform(6.0),
            colors.background,
        );
        canvas.fill_rounded_rect(
            Rect::new(0.0, 0.0, BAR, self.height),
            CornerRadii::new(6.0, 0.0, 0.0, 6.0),
            colors.bar,
        );
        self.stack.paint_backdrop(cx, canvas);
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        let colors = cx.theme.tokens.callout.get(theme_kind(self.kind));
        canvas.draw_icon(
            self.glyph,
            IconFont::SegoeFluent,
            self.icon.height,
            colors.icon,
            self.icon.center(),
        );
        canvas.draw_text_layout(self.header.layout.as_ref(), self.header.origin, colors.icon);
        self.stack.paint(cx, canvas);
    }

    fn hit_test(&self, point: Point) -> usize {
        if point.y < self.body_top || self.stack.is_empty() {
            self.header.hit(point)
        } else {
            self.stack.hit_test(point)
        }
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        if self.in_header(offset) {
            self.header
                .layout
                .caret_rect(self.header.display(offset), false)
                .translate(self.header.origin.x, self.header.origin.y)
        } else {
            self.stack.caret_rect(offset)
        }
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        let mut rects = Vec::new();
        let header = TextRange::new(self.range.start, self.header.end.max(self.range.start));
        if let Some(part) = range.intersection(header) {
            let origin = self.header.origin;
            if self.header.synthetic {
                let size = self.header.layout.size();
                rects.push(Rect::new(origin.x, origin.y, size.width, size.height));
            } else {
                let from = self.header.display(part.start);
                let to = self.header.display(part.end);
                rects.extend(
                    self.header
                        .layout
                        .range_rects(from..to)
                        .into_iter()
                        .map(|rect| rect.translate(origin.x, origin.y)),
                );
            }
        }
        rects.extend(self.stack.selection_rects(range));
        rects
    }

    fn line_range(&self, offset: usize) -> TextRange {
        if self.in_header(offset) {
            TextRange::new(self.range.start, self.header.end.max(self.range.start))
        } else {
            self.stack.line_range(offset)
        }
    }

    fn link_at(&self, point: Point) -> Option<LinkTarget> {
        self.stack.link_at(point)
    }

    fn task_box_at(&self, point: Point) -> Option<TaskBox> {
        self.stack.task_box_at(point)
    }

    fn context_menu(&self, point: Point, origin: Point) -> Option<EditorEvent> {
        self.stack.context_menu(point, origin)
    }

    fn press(&self, point: Point, origin: Point, mods: Modifiers) -> Option<BlockPress> {
        if self.icon_hit.contains(point) {
            return Some(BlockPress::Event(EditorEvent::CalloutMenu {
                range: self.range,
                kind: self.kind,
                anchor: Rect::new(
                    origin.x + self.icon.x,
                    origin.y + self.icon.y,
                    self.icon.width,
                    self.icon.height,
                ),
            }));
        }
        self.stack.press(point, origin, mods)
    }

    fn cursor_at(&self, point: Point) -> Option<Cursor> {
        if self.icon_hit.contains(point) {
            return Some(Cursor::Hand);
        }
        self.stack.cursor_at(point)
    }

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.label.clone()
    }
}

/// A footnote definition: the label in the marker column and the children beside it.
pub(super) fn footnote(ctx: &Ctx<'_>, block: &Block, label: &str) -> Box<dyn BlockLayout> {
    let cx = ctx.cx;
    let body = cx.config.body_size();
    let layout = cx.text.layout(
        label,
        &style(cx, &cx.config.text_font, body * 0.8),
        &[],
        &[],
        f32::INFINITY,
    );
    let col = (1.5 * body).max(layout.size().width + 0.9 * body);
    Box::new(item::measure(
        ctx,
        ItemSpec {
            block,
            shape: Shape::Label(layout),
            col,
            checkbox: None,
            checked: false,
            label: ctx.strings.footnote.clone(),
        },
    ))
}
