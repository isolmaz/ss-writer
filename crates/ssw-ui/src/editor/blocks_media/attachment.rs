//! Attachments: a paragraph holding only a link to a local non-image file is drawn as a chip
//! with the shell icon, the file name, a TYPE pill and `size · date`. The link source shows
//! below the chip while the caret is inside; Ctrl+click opens the file like any link.

use std::rc::Rc;

use ssw_core::markdown::ast::{Block, BlockKind, InlineKind, LinkKind};
use ssw_core::text::TextRange;

use crate::canvas::{Canvas, ImageId};
use crate::editor::block::{BlockCx, BlockLayout, BlockPaintCx, SpecialRenderer};
use crate::editor::config::{EditorStrings, LinkTarget};
use crate::geometry::{CornerRadii, Point, Rect};
use crate::text::{FontWeight, StyledRange, TextLayout, TextStyle};

use super::verbatim::{Row, SourceText, display_of};
use super::{AttachmentProvider, MediaStrings};

/// Height of the chip.
const CHIP_HEIGHT: f32 = 52.0;
/// Widest chip.
const CHIP_WIDTH: f32 = 380.0;
/// Space above and below the chip.
const MARGIN: f32 = 4.0;
/// Side of the icon.
const ICON: f32 = 32.0;

/// Renders paragraphs that hold one link to a local file.
pub(super) struct AttachmentRenderer {
    strings: Rc<MediaStrings>,
    provider: Rc<dyn AttachmentProvider>,
}

impl AttachmentRenderer {
    pub fn new(strings: Rc<MediaStrings>, provider: Rc<dyn AttachmentProvider>) -> Self {
        Self { strings, provider }
    }
}

impl SpecialRenderer for AttachmentRenderer {
    fn try_measure(&self, cx: &BlockCx<'_>, block: &Block) -> Option<Box<dyn BlockLayout>> {
        if !matches!(block.kind, BlockKind::Paragraph) {
            return None;
        }
        let [inline] = block.inlines.as_slice() else {
            return None;
        };
        let InlineKind::Link(link) = &inline.kind else {
            return None;
        };
        if matches!(link.kind, LinkKind::Autolink { .. }) {
            return None;
        }
        let info = self.provider.attachment(&link.dest)?;
        let tokens = &cx.theme.tokens;
        let text_style = TextStyle::new(cx.config.text_font.clone(), cx.config.body_size() * 0.95)
            .with_weight(FontWeight::SEMIBOLD);
        let small = TextStyle::new(cx.config.text_font.clone(), cx.config.body_size() * 0.75);
        let lay =
            |text: &str, style: &TextStyle| cx.text.layout(text, style, &[], &[], f32::INFINITY);
        let width = cx.width.min(CHIP_WIDTH);
        let pill = lay(&info.type_label, &small);
        let pill_size = pill.size();
        let pill_rect = Rect::new(
            width - 12.0 - (pill_size.width + 12.0),
            10.0 + MARGIN,
            pill_size.width + 12.0,
            pill_size.height + 4.0,
        );
        let revealed = cx.reveal.touches(block.range);
        let source = revealed.then(|| {
            let rows = [Row {
                src: block.range,
                pad: 0,
            }];
            let composition = cx.composition.filter(|comp| block.range.touches(comp.at));
            let mut display = display_of(block.range, &rows, cx.buffer, composition).display;
            let base = TextStyle::new(cx.config.code_font.clone(), cx.config.body_size() * 0.95);
            for marker in &inline.markers {
                let map = &display.map;
                let range = map.source_to_display(marker.start)..map.source_to_display(marker.end);
                display.styles.push(StyledRange {
                    range,
                    style: base.clone().with_color(tokens.marker),
                });
            }
            SourceText::new(display, &base, cx.width, cx)
        });
        let chip = Rect::new(0.0, MARGIN, width, CHIP_HEIGHT);
        let source_top = chip.bottom() + MARGIN;
        let height = source
            .as_ref()
            .map_or(source_top, |text| source_top + text.height());
        Some(Box::new(AttachmentLayout {
            name: lay(&info.name, &text_style),
            detail: lay(&info.detail, &small),
            pill,
            pill_rect,
            icon: info.icon,
            chip,
            source,
            source_top,
            height,
            range: block.range,
            dest: link.dest.clone(),
            access: self.strings.attachment_named(&info.name),
        }))
    }
}

struct AttachmentLayout {
    name: Box<dyn TextLayout>,
    detail: Box<dyn TextLayout>,
    pill: Box<dyn TextLayout>,
    pill_rect: Rect,
    icon: Option<ImageId>,
    chip: Rect,
    source: Option<SourceText>,
    /// Top of the source text below the chip.
    source_top: f32,
    height: f32,
    range: TextRange,
    dest: String,
    access: String,
}

impl AttachmentLayout {
    fn source_point(&self, point: Point) -> Point {
        Point::new(point.x, point.y - self.source_top)
    }
}

impl BlockLayout for AttachmentLayout {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint_backdrop(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        canvas.fill_rounded_rect(
            self.chip,
            CornerRadii::uniform(8.0),
            cx.theme.tokens.code_bg,
        );
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        canvas.stroke_rounded_rect(self.chip, CornerRadii::uniform(8.0), tokens.hairline, 1.0);
        if let Some(icon) = self.icon {
            canvas.draw_image(
                icon,
                Rect::new(
                    self.chip.x + 12.0,
                    self.chip.y + (self.chip.height - ICON) / 2.0,
                    ICON,
                    ICON,
                ),
                1.0,
            );
        }
        let text_x = self.chip.x + 12.0 + ICON + 12.0;
        let name_height = self.name.size().height;
        let block_height = name_height + self.detail.size().height;
        let top = self.chip.y + (self.chip.height - block_height) / 2.0;
        canvas.draw_text_layout(self.name.as_ref(), Point::new(text_x, top), tokens.text);
        canvas.draw_text_layout(
            self.detail.as_ref(),
            Point::new(text_x, top + name_height),
            tokens.text_muted,
        );
        canvas.fill_rounded_rect(
            self.pill_rect,
            CornerRadii::uniform(self.pill_rect.height / 2.0),
            tokens.hover_bg,
        );
        let size = self.pill.size();
        canvas.draw_text_layout(
            self.pill.as_ref(),
            Point::new(
                self.pill_rect.x + (self.pill_rect.width - size.width) / 2.0,
                self.pill_rect.y + (self.pill_rect.height - size.height) / 2.0,
            ),
            tokens.text_muted,
        );
        if let Some(source) = &self.source {
            source.paint(canvas, Point::new(0.0, self.source_top), tokens.text);
        }
    }

    fn hit_test(&self, point: Point) -> usize {
        match &self.source {
            Some(source) => source.hit(self.source_point(point)),
            None if point.x < self.chip.width / 2.0 => self.range.start,
            None => self.range.end,
        }
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        match &self.source {
            Some(source) => source.caret(offset).translate(0.0, self.source_top),
            None => {
                let x = if offset > self.range.start {
                    self.chip.right()
                } else {
                    self.chip.x
                };
                Rect::new(x, self.chip.y, 0.0, self.chip.height)
            }
        }
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        match &self.source {
            Some(source) => source
                .selection(range)
                .into_iter()
                .map(|rect| rect.translate(0.0, self.source_top))
                .collect(),
            None if !range.is_empty() => vec![self.chip],
            None => Vec::new(),
        }
    }

    fn line_range(&self, offset: usize) -> TextRange {
        match &self.source {
            Some(source) => source.line_range(offset),
            None => self.range,
        }
    }

    fn link_at(&self, point: Point) -> Option<LinkTarget> {
        self.chip
            .contains(point)
            .then(|| LinkTarget::Url(self.dest.clone()))
    }

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.access.clone()
    }
}
