//! Dividers, page breaks, front matter and raw HTML.

use ssw_core::markdown::ast::Block;
use ssw_core::text::TextRange;

use crate::canvas::{Canvas, Dash};
use crate::editor::block::{BlockLayout, BlockPaintCx};
use crate::editor::config::EditorStrings;
use crate::geometry::{Point, Rect};
use crate::text::TextLayout;

use super::text::TextBlock;
use super::{Ctx, style};

/// A divider or page break while its source is hidden: a hairline across the column, a page
/// break with its label centred in it.
struct RuleLayout {
    range: TextRange,
    /// The label of a page break; `None` for a divider.
    label: Option<Box<dyn TextLayout>>,
    /// Room between the label and the hairlines.
    gap: f32,
    width: f32,
    height: f32,
    access: String,
}

impl BlockLayout for RuleLayout {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        let y = self.height / 2.0;
        let Some(label) = &self.label else {
            canvas.stroke_line(
                Point::new(0.0, y),
                Point::new(self.width, y),
                tokens.hairline,
                1.0,
                Dash::Solid,
            );
            return;
        };
        let size = label.size();
        let left = (self.width - size.width) / 2.0;
        for (from, to) in [
            (0.0, left - self.gap),
            (left + size.width + self.gap, self.width),
        ] {
            if to > from {
                canvas.stroke_line(
                    Point::new(from, y),
                    Point::new(to, y),
                    tokens.hairline,
                    1.0,
                    Dash::Dashed,
                );
            }
        }
        canvas.draw_text_layout(
            label.as_ref(),
            Point::new(left, (self.height - size.height) / 2.0),
            tokens.text_muted,
        );
    }

    fn hit_test(&self, point: Point) -> usize {
        if point.x < self.width / 2.0 {
            self.range.start
        } else {
            self.range.end
        }
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        let x = if offset <= self.range.start {
            0.0
        } else {
            self.width
        };
        Rect::new(x, 0.0, 0.0, self.height)
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        if range.intersects(self.range) {
            vec![Rect::new(0.0, 0.0, self.width, self.height)]
        } else {
            Vec::new()
        }
    }

    fn line_range(&self, _offset: usize) -> TextRange {
        self.range
    }

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.access.clone()
    }
}

/// A hairline, or the dimmed source while the caret touches it.
fn rule(
    ctx: &Ctx<'_>,
    block: &Block,
    label: Option<Box<dyn TextLayout>>,
    access: &str,
) -> Box<dyn BlockLayout> {
    let cx = ctx.cx;
    let body = cx.config.body_size();
    if cx.reveal.touches(block.range) {
        let base = style(cx, &cx.config.text_font, body);
        return Box::new(TextBlock::source(
            cx,
            block.range,
            &base,
            cx.theme.tokens.marker,
            0.0,
            false,
            access.to_owned(),
        ));
    }
    Box::new(RuleLayout {
        range: block.range,
        label,
        gap: 0.6 * body,
        width: cx.width,
        height: cx.config.line_px(),
        access: access.to_owned(),
    })
}

/// `---`, `***`, `___`.
pub(super) fn divider(ctx: &Ctx<'_>, block: &Block) -> Box<dyn BlockLayout> {
    rule(ctx, block, None, &ctx.strings.divider)
}

/// `<!-- pagebreak -->`: "Page break" on a dashed hairline.
pub(super) fn page_break(ctx: &Ctx<'_>, block: &Block) -> Box<dyn BlockLayout> {
    let cx = ctx.cx;
    let label = cx.text.layout(
        &ctx.strings.page_break,
        &style(cx, &cx.config.text_font, cx.config.body_size() * 0.85),
        &[],
        &[],
        f32::INFINITY,
    );
    rule(ctx, block, Some(label), &ctx.strings.page_break)
}

/// Front matter hidden by the setting while the caret is elsewhere: no height, no painting.
struct Collapsed {
    range: TextRange,
    access: String,
}

impl BlockLayout for Collapsed {
    fn height(&self) -> f32 {
        0.0
    }

    fn paint(&self, _cx: &BlockPaintCx<'_>, _canvas: &mut dyn Canvas) {}

    fn hit_test(&self, _point: Point) -> usize {
        self.range.end
    }

    fn caret_rect(&self, _offset: usize) -> Rect {
        Rect::ZERO
    }

    fn selection_rects(&self, _range: TextRange) -> Vec<Rect> {
        Vec::new()
    }

    fn line_range(&self, _offset: usize) -> TextRange {
        self.range
    }

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.access.clone()
    }
}

/// YAML (or TOML) front matter: its source in a muted monospace card; with the setting to hide
/// it on, nothing until the caret enters it.
pub(super) fn front_matter(ctx: &Ctx<'_>, block: &Block) -> Box<dyn BlockLayout> {
    let cx = ctx.cx;
    let access = ctx.strings.front_matter.clone();
    if cx.config.hide_front_matter && !cx.reveal.selection_touches(block.range) {
        return Box::new(Collapsed {
            range: block.range,
            access,
        });
    }
    let body = cx.config.body_size();
    let base = style(cx, &cx.config.code_font, body * 0.9);
    Box::new(TextBlock::source(
        cx,
        block.range,
        &base,
        cx.theme.tokens.text_muted,
        0.75 * body,
        true,
        access,
    ))
}

/// A raw HTML block: its source, dimmed.
pub(super) fn html(ctx: &Ctx<'_>, block: &Block) -> Box<dyn BlockLayout> {
    let cx = ctx.cx;
    let base = style(cx, &cx.config.code_font, cx.config.body_size() * 0.95);
    Box::new(TextBlock::source(
        cx,
        block.range,
        &base,
        cx.theme.tokens.text_muted,
        0.0,
        false,
        ctx.strings.html.clone(),
    ))
}
