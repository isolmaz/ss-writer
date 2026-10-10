//! Images that stand alone: a paragraph holding only `![alt](path)` (or a lone `<img>` HTML
//! block) is drawn as a block no wider than the text column, aspect preserved, with a resize
//! handle that writes `|W` (or the `width` attribute) as one undo step. Images between other
//! text stay inline objects of the paragraph. Loading, blocked ("Click to load") and failed
//! images show a placeholder; the source shows above the image while the caret is inside.

use std::cell::Cell;
use std::rc::Rc;

use ssw_core::markdown::ast::{Block, Image, ImageKind, Inline, InlineKind};
use ssw_core::text::{Buffer, TextRange};

use crate::canvas::Canvas;
use crate::editor::block::{
    BlockCx, BlockDrag, BlockEdit, BlockLayout, BlockPaintCx, BlockPress, SpecialRenderer,
};
use crate::editor::config::{EditorEvent, EditorStrings};
use crate::editor::object::{ObjectKind, ObjectRequest, ObjectState, ObjectVisual};
use crate::geometry::{Color, CornerRadii, Point, Rect};
use crate::input::{Cursor, Modifiers};
use crate::text::{StyledRange, TextLayout, TextStyle};

use super::diagram::{draw_visual, fit};
use super::verbatim::{Row, SourceText, display_of};
use super::{MediaEvent, MediaStrings};

/// Side of the resize handle.
const HANDLE: f32 = 10.0;
/// Space between the source line and the image, and below the image.
const GAP: f32 = 6.0;
/// Narrowest image a resize can produce.
const MIN_WIDTH: f32 = 24.0;

/// Renders paragraphs and HTML blocks that hold one image.
pub(super) struct ImageRenderer {
    strings: Rc<MediaStrings>,
}

impl ImageRenderer {
    pub fn new(strings: Rc<MediaStrings>) -> Self {
        Self { strings }
    }
}

/// The only inline of a paragraph or `<img>` block, when it is an image.
fn lone_image(block: &Block) -> Option<(&Inline, &Image)> {
    // Registered for paragraphs and HTML blocks only (see `register_with`).
    match block.inlines.as_slice() {
        [inline] => match &inline.kind {
            InlineKind::Image(image) => Some((inline, image)),
            _ => None,
        },
        _ => None,
    }
}

impl SpecialRenderer for ImageRenderer {
    fn try_measure(&self, cx: &BlockCx<'_>, block: &Block) -> Option<Box<dyn BlockLayout>> {
        let (inline, image) = lone_image(block)?;
        let tokens = &cx.theme.tokens;
        let alt = cx.buffer.slice(image.alt).into_owned();
        let state = cx.objects.object(&ObjectRequest {
            kind: ObjectKind::Image,
            source: &image.dest,
            alt: &alt,
            width: image.width,
            height: image.height,
            font_size: cx.config.body_size(),
            max_width: cx.width,
            text_color: tokens.text,
            range: block.range,
        });
        let revealed = cx.reveal.touches(block.range);
        let strings = &self.strings;

        let source = (revealed || !matches!(state, ObjectState::Ready(_))).then(|| {
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
        let top = source.as_ref().map_or(0.0, |text| text.height() + GAP);

        let preview = Rc::new(Cell::new(None));
        let (body, resize) = match state {
            ObjectState::Ready(info) => {
                let size = fit(info.size, cx.width);
                (
                    Body::Image {
                        visual: info.visual,
                        rect: Rect::new(0.0, top, size.width, size.height),
                    },
                    resize_target(cx.buffer, inline, image),
                )
            }
            other => {
                let (label, color, blocked) = match other {
                    ObjectState::Blocked => (&strings.image_blocked, tokens.link, true),
                    ObjectState::Failed => (&strings.image_failed, tokens.error, false),
                    _ => (&strings.image_loading, tokens.text_muted, false),
                };
                let style =
                    TextStyle::new(cx.config.text_font.clone(), cx.config.body_size() * 0.9);
                let label = cx.text.layout(label, &style, &[], &[], cx.width);
                let height = (cx.config.body_size() * 2.2).max(label.size().height + 12.0);
                (
                    Body::Placeholder {
                        rect: Rect::new(0.0, top, cx.width.min(320.0), height),
                        label,
                        color,
                        blocked,
                    },
                    None,
                )
            }
        };
        let body_rect = body.rect();
        Some(Box::new(ImageLayout {
            source,
            body,
            height: body_rect.bottom() + GAP,
            width: cx.width,
            range: block.range,
            dest: image.dest.clone(),
            access: strings.image_named(&alt),
            hovered: Cell::new(false),
            resize,
            preview,
        }))
    }
}

/// What is drawn below the source line.
enum Body {
    Image {
        visual: ObjectVisual,
        rect: Rect,
    },
    Placeholder {
        rect: Rect,
        label: Box<dyn TextLayout>,
        color: Color,
        /// A click loads the image.
        blocked: bool,
    },
}

impl Body {
    fn rect(&self) -> Rect {
        match self {
            Body::Image { rect, .. } | Body::Placeholder { rect, .. } => *rect,
        }
    }
}

struct ImageLayout {
    source: Option<SourceText>,
    body: Body,
    height: f32,
    width: f32,
    range: TextRange,
    dest: String,
    access: String,
    /// The pointer was over the block when it was last painted.
    hovered: Cell<bool>,
    resize: Option<ResizeTarget>,
    /// The width of a resize drag in progress.
    preview: Rc<Cell<Option<f32>>>,
}

impl ImageLayout {
    fn handle(&self) -> Option<Rect> {
        match (&self.body, &self.resize) {
            (Body::Image { rect, .. }, Some(_)) => {
                let width = self.preview.get().unwrap_or(rect.width);
                Some(Rect::new(
                    rect.x + width - HANDLE - 2.0,
                    rect.bottom() - HANDLE - 2.0,
                    HANDLE,
                    HANDLE,
                ))
            }
            _ => None,
        }
    }

    fn on_handle(&self, point: Point) -> bool {
        self.hovered.get() && self.handle().is_some_and(|handle| handle.contains(point))
    }

    fn on_blocked(&self, point: Point) -> bool {
        matches!(&self.body, Body::Placeholder { rect, blocked: true, .. } if rect.contains(point))
    }
}

impl BlockLayout for ImageLayout {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint_backdrop(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        if let Body::Placeholder { rect, .. } = &self.body {
            canvas.fill_rounded_rect(*rect, CornerRadii::uniform(6.0), cx.theme.tokens.code_bg);
        }
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        self.hovered.set(cx.hovered);
        if let Some(source) = &self.source {
            source.paint(canvas, Point::ZERO, tokens.text);
        }
        match &self.body {
            Body::Image { visual, rect } => {
                let shown = match self.preview.get() {
                    Some(width) if rect.width > 0.0 => {
                        Rect::new(rect.x, rect.y, width, rect.height * width / rect.width)
                    }
                    _ => *rect,
                };
                draw_visual(canvas, *visual, shown);
                if cx.hovered && self.resize.is_some() {
                    canvas.stroke_rounded_rect(
                        shown,
                        CornerRadii::uniform(2.0),
                        tokens.accent,
                        1.0,
                    );
                    if let Some(handle) = self.handle() {
                        canvas.fill_rounded_rect(handle, CornerRadii::uniform(2.0), tokens.accent);
                    }
                }
            }
            Body::Placeholder {
                rect, label, color, ..
            } => {
                let size = label.size();
                canvas.draw_text_layout(
                    label.as_ref(),
                    Point::new(
                        rect.x + (rect.width - size.width) / 2.0,
                        rect.y + (rect.height - size.height) / 2.0,
                    ),
                    *color,
                );
            }
        }
    }

    fn hit_test(&self, point: Point) -> usize {
        match &self.source {
            Some(source) => source.hit(point),
            None if point.x < self.width / 2.0 => self.range.start,
            None => self.range.end,
        }
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        match &self.source {
            Some(source) => source.caret(offset),
            None => {
                let rect = self.body.rect();
                let x = if offset > self.range.start {
                    rect.right()
                } else {
                    rect.x
                };
                Rect::new(x, rect.y, 0.0, rect.height)
            }
        }
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        match &self.source {
            Some(source) => source.selection(range),
            None if !range.is_empty() => vec![self.body.rect()],
            None => Vec::new(),
        }
    }

    fn line_range(&self, offset: usize) -> TextRange {
        match &self.source {
            Some(source) => source.line_range(offset),
            None => self.range,
        }
    }

    fn press(&self, point: Point, _origin: Point, _mods: Modifiers) -> Option<BlockPress> {
        if self.on_handle(point)
            && let (Body::Image { rect, .. }, Some(target)) = (&self.body, &self.resize)
        {
            return Some(BlockPress::Drag(Box::new(ResizeDrag {
                start_x: point.x,
                start_width: rect.width,
                max_width: self.width,
                target: target.clone(),
                preview: Rc::clone(&self.preview),
            })));
        }
        self.on_blocked(point).then(|| {
            BlockPress::Event(EditorEvent::Media(MediaEvent::LoadImage {
                dest: self.dest.clone(),
                range: self.range,
            }))
        })
    }

    fn cursor_at(&self, point: Point) -> Option<Cursor> {
        if self.on_handle(point) {
            Some(Cursor::SizeWE)
        } else {
            self.on_blocked(point).then_some(Cursor::Hand)
        }
    }

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.access.clone()
    }
}

/// Where a resize writes the new width.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResizeTarget {
    /// The (possibly empty) `|300x200` suffix inside the Markdown image.
    Suffix(TextRange),
    /// The whole `<img>` tag, rewritten with a `width` attribute.
    Tag { range: TextRange, tag: String },
}

impl ResizeTarget {
    fn edit(&self, width: u32) -> BlockEdit {
        match self {
            ResizeTarget::Suffix(range) => BlockEdit {
                range: *range,
                text: format!("|{width}"),
            },
            ResizeTarget::Tag { range, tag } => BlockEdit {
                range: *range,
                text: html_with_width(tag, width),
            },
        }
    }
}

fn resize_target(buffer: &Buffer, inline: &Inline, image: &Image) -> Option<ResizeTarget> {
    if matches!(image.kind, ImageKind::Html) {
        return Some(ResizeTarget::Tag {
            range: inline.range,
            tag: buffer.slice(inline.range).into_owned(),
        });
    }
    let close = inline.markers.get(1)?;
    let text = buffer.slice(*close);
    let len = if text.starts_with('|') {
        text.find(']')?
    } else {
        0
    };
    Some(ResizeTarget::Suffix(TextRange::new(
        close.start,
        close.start + len,
    )))
}

/// The resize handle being dragged.
#[derive(Debug)]
struct ResizeDrag {
    start_x: f32,
    start_width: f32,
    max_width: f32,
    target: ResizeTarget,
    preview: Rc<Cell<Option<f32>>>,
}

impl ResizeDrag {
    fn width_at(&self, point: Point) -> f32 {
        (self.start_width + point.x - self.start_x)
            .clamp(MIN_WIDTH, self.max_width.max(MIN_WIDTH))
            .round()
    }
}

impl Drop for ResizeDrag {
    fn drop(&mut self) {
        self.preview.set(None);
    }
}

impl BlockDrag for ResizeDrag {
    fn moved(&mut self, point: Point) {
        self.preview.set(Some(self.width_at(point)));
    }

    fn finish(self: Box<Self>, point: Point) -> Option<BlockEdit> {
        let width = self.width_at(point);
        if (width - self.start_width).abs() < 0.5 {
            return None;
        }
        // The width is clamped to a positive, rounded value.
        Some(self.target.edit(width as u32))
    }

    fn cursor(&self) -> Cursor {
        Cursor::SizeWE
    }
}

/// The span of attribute `name` in `tag`: (start of the whitespace before it, start of the
/// name, end of the value).
fn attribute_span(tag: &str, name: &str) -> Option<(usize, usize, usize)> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut from = 0;
    while let Some(found) = lower[from..].find(&needle) {
        let start = from + found;
        let before = &tag[..start];
        if before.ends_with(char::is_whitespace) {
            let value = &tag[start + needle.len()..];
            let len = match value.chars().next()? {
                quote @ ('"' | '\'') => value[1..].find(quote)? + 2,
                _ => value
                    .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                    .unwrap_or(value.len()),
            };
            let space = before.trim_end().len();
            return Some((space, start, start + needle.len() + len));
        }
        from = start + needle.len();
    }
    None
}

/// `tag` with its `width` attribute set to `width` and its `height` attribute removed (the
/// aspect ratio is kept by the renderer).
fn html_with_width(tag: &str, width: u32) -> String {
    let mut tag = tag.to_owned();
    if let Some((space, _, end)) = attribute_span(&tag, "height") {
        tag.replace_range(space..end, "");
    }
    let attribute = format!("width=\"{width}\"");
    match attribute_span(&tag, "width") {
        Some((_, start, end)) => tag.replace_range(start..end, &attribute),
        None => {
            let close = if tag.ends_with("/>") {
                tag.len() - 2
            } else {
                tag.len().saturating_sub(1)
            };
            let close = tag[..close].trim_end().len();
            tag.insert_str(close, &format!(" {attribute}"));
        }
    }
    tag
}
