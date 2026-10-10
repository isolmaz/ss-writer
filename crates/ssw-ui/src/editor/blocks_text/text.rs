//! Text laid out from a display map: paragraphs and headings inside containers, and the raw
//! source of blocks that are shown as text (HTML, front matter, markers of dividers).

use ssw_core::markdown::ast::Block;
use ssw_core::text::TextRange;

use crate::canvas::Canvas;
use crate::editor::block::{BlockCx, BlockLayout, BlockPaintCx, HEADING_SCALE};
use crate::editor::config::{EditorStrings, LinkTarget};
use crate::editor::display::{Display, DisplayParams, GapPolicy, build, raw};
use crate::geometry::{Color, CornerRadii, Point, Rect};
use crate::text::{FontWeight, TextLayout, TextStyle};

use super::{Ctx, style};

/// A block of text: a paragraph, a heading or the source of a block shown as text.
pub(super) struct TextBlock {
    layout: Box<dyn TextLayout>,
    display: Display,
    /// What the block is called to assistive technology. Blocks shown as text (HTML, front
    /// matter, divider markers) name themselves; paragraphs and headings inside a container are
    /// described by the container, so they have none.
    label: Option<String>,
    /// Where the IME composition is inserted.
    composition_at: Option<usize>,
    color: Color,
    /// Offset of the text inside the block (card padding).
    origin: Point,
    width: f32,
    height: f32,
    /// Paint a `code_bg` card behind the text.
    card: bool,
}

/// A paragraph inside a container.
pub(super) fn paragraph(ctx: &Ctx<'_>, block: &Block) -> TextBlock {
    let cx = ctx.cx;
    let base = style(cx, &cx.config.text_font, cx.config.body_size());
    let gap = GapPolicy::Marker { reveal: ctx.shown };
    TextBlock::inline(ctx, block, &base, gap)
}

/// A heading of `level` (1–6) inside a container: sized and weighted as at top level, without
/// the gutter widgets.
pub(super) fn heading(ctx: &Ctx<'_>, block: &Block, level: u8) -> TextBlock {
    let cx = ctx.cx;
    let size = cx.config.body_size() * HEADING_SCALE[usize::from(level - 1)];
    let base = TextStyle::new(cx.config.heading_font.clone(), size)
        .with_weight(FontWeight::SEMIBOLD)
        .with_line_height(cx.config.line_height_for(size, 1.3));
    let gap = GapPolicy::Marker {
        reveal: cx.reveal.touches(block.range),
    };
    TextBlock::inline(ctx, block, &base, gap)
}

impl TextBlock {
    /// The inline content of `block` laid out in `ctx.cx.width`.
    fn inline(ctx: &Ctx<'_>, block: &Block, base: &TextStyle, gap: GapPolicy) -> Self {
        let cx = ctx.cx;
        let source = cx.buffer.slice(block.range);
        let composition = cx.composition.filter(|comp| block.range.touches(comp.at));
        let display = build(&DisplayParams {
            src: &source,
            range: block.range,
            inlines: &block.inlines,
            tokens: &cx.theme.tokens,
            base,
            mono_family: &cx.config.code_font,
            reveal: cx.reveal,
            gap,
            objects: cx.objects,
            max_width: cx.width,
            composition,
        });
        let layout = display.layout(cx.text, base, cx.width);
        let height = layout.size().height;
        Self {
            layout,
            display,
            label: None,
            composition_at: composition.map(|comp| comp.at),
            color: ctx.color,
            origin: Point::ZERO,
            width: cx.width,
            height,
            card: false,
        }
    }

    /// The source text of `range` shown as it is, `pad` DIPs inside the block, optionally on a
    /// card.
    pub(super) fn source(
        cx: &BlockCx<'_>,
        range: TextRange,
        base: &TextStyle,
        color: Color,
        pad: f32,
        card: bool,
        label: String,
    ) -> Self {
        let source = cx.buffer.slice(range);
        let composition = cx.composition.filter(|comp| range.touches(comp.at));
        let display = raw(&source, range, composition);
        let layout = display.layout(cx.text, base, (cx.width - 2.0 * pad).max(1.0));
        let height = layout.size().height + 2.0 * pad;
        Self {
            layout,
            display,
            label: Some(label),
            composition_at: composition.map(|comp| comp.at),
            color,
            origin: Point::new(pad, pad),
            width: cx.width,
            height,
            card,
        }
    }

    fn display_offset(&self, offset: usize) -> usize {
        match (&self.display.composition, self.composition_at) {
            (Some(composition), Some(at)) if at == offset => composition.caret,
            _ => self.display.map.source_to_display(offset),
        }
    }

    fn local(&self, point: Point) -> Point {
        Point::new(point.x - self.origin.x, point.y - self.origin.y)
    }
}

impl BlockLayout for TextBlock {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint_backdrop(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        if self.card {
            canvas.fill_rounded_rect(
                Rect::new(0.0, 0.0, self.width, self.height),
                CornerRadii::uniform(6.0),
                cx.theme.tokens.code_bg,
            );
        }
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        canvas.save();
        canvas.translate(self.origin.x, self.origin.y);
        self.display.paint_behind(self.layout.as_ref(), canvas);
        canvas.draw_text_layout(self.layout.as_ref(), Point::ZERO, self.color);
        self.display
            .paint_front(self.layout.as_ref(), canvas, tokens.text);
        canvas.restore();
    }

    fn hit_test(&self, point: Point) -> usize {
        let hit = self.layout.hit_test_point(self.local(point));
        self.display.map.display_to_source(hit.insertion_offset())
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        self.layout
            .caret_rect(self.display_offset(offset), false)
            .translate(self.origin.x, self.origin.y)
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        let map = &self.display.map;
        let start = map.source_to_display(range.start);
        let end = map.source_to_display(range.end);
        self.layout
            .range_rects(start..end)
            .into_iter()
            .map(|rect| rect.translate(self.origin.x, self.origin.y))
            .collect()
    }

    fn line_range(&self, offset: usize) -> TextRange {
        let map = &self.display.map;
        let display = self.display_offset(offset);
        // `line_for_offset` always names an existing line; the default is the empty line.
        self.layout
            .line_metrics(self.layout.line_for_offset(display))
            .map_or(TextRange::at(offset), |line| {
                let start = map.display_to_source(line.start);
                let end = map.display_to_source(line.content_end());
                TextRange::new(start, end.max(start))
            })
    }

    fn link_at(&self, point: Point) -> Option<LinkTarget> {
        let hit = self.layout.hit_test_point(self.local(point));
        if !hit.inside {
            return None;
        }
        self.display
            .link_at(hit.offset)
            .map(|link| link.target.clone())
    }

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.label.clone().unwrap_or_default()
    }
}
