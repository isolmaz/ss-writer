//! Math blocks (`$$ … $$`) and Mermaid diagrams: a vector scene supplied by the
//! [`InlineObjectProvider`](crate::editor::object::InlineObjectProvider), with the source shown
//! while the caret is inside, or instead of the scene when it cannot be drawn.

use std::rc::Rc;

use ssw_core::markdown::ast::{Block, BlockKind};
use ssw_core::text::TextRange;

use crate::canvas::Canvas;
use crate::editor::block::{
    BlockCx, BlockLayout, BlockPaintCx, BlockPress, BlockRenderer, FallbackRenderer, composition_in,
};
use crate::editor::config::{EditorEvent, EditorStrings};
use crate::editor::object::{ObjectKind, ObjectRequest, ObjectState, ObjectVisual};
use crate::geometry::{Color, Point, Rect, Size};
use crate::input::{Cursor, Modifiers};
use crate::text::{StyledRange, TextLayout, TextStyle};

use super::code::{CodeRenderer, language_name};
use super::verbatim::{Row, SourceText, display_of};
use super::{MediaEvent, MediaStrings};

/// Space around a diagram or formula.
const MARGIN: f32 = 8.0;
/// Space between a code block and the note below it.
const NOTE_GAP: f32 = 4.0;

/// The source part of a [`SceneLayout`].
type TextPart = Option<Box<dyn BlockLayout>>;
/// The scene part of a [`SceneLayout`]: what to draw and where.
type ScenePart = Option<(ObjectVisual, Rect)>;
/// The "Unsupported diagram" note and its position.
type NotePart = Option<(Box<dyn TextLayout>, Point)>;

/// `size` scaled down (aspect preserved) to fit `max_width`.
pub(super) fn fit(size: Size, max_width: f32) -> Size {
    if size.width > max_width && size.width > 0.0 {
        let scale = max_width / size.width;
        Size::new(max_width, size.height * scale)
    } else {
        size
    }
}

/// The scene or image of a ready object, drawn into `rect`.
pub(super) fn draw_visual(canvas: &mut dyn Canvas, visual: ObjectVisual, rect: Rect) {
    match visual {
        ObjectVisual::Scene(scene) => canvas.draw_scene(scene, rect),
        ObjectVisual::Image(image) => canvas.draw_image(image, rect, 1.0),
    }
}

/// Renders [`BlockKind::MathBlock`].
pub(super) struct MathRenderer {
    strings: Rc<MediaStrings>,
}

impl MathRenderer {
    pub fn new(strings: Rc<MediaStrings>) -> Self {
        Self { strings }
    }
}

impl BlockRenderer for MathRenderer {
    fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        let BlockKind::MathBlock(math) = &block.kind else {
            return FallbackRenderer.measure(cx, block);
        };
        let tokens = &cx.theme.tokens;
        let content: Vec<Row> = math.lines.iter().map(Row::of).collect();
        let mut tex = String::new();
        for (index, row) in content.iter().enumerate() {
            if index > 0 {
                tex.push('\n');
            }
            tex.push_str(&" ".repeat(usize::from(row.pad)));
            tex.push_str(&cx.buffer.slice(row.src));
        }
        let state = cx.objects.object(&ObjectRequest {
            kind: ObjectKind::DisplayMath,
            source: &tex,
            alt: "",
            width: None,
            height: None,
            font_size: cx.config.body_size(),
            max_width: cx.width,
            text_color: tokens.text,
            range: block.range,
        });
        let revealed = cx.reveal.touches(block.range);
        let access = self.strings.math_block.clone();
        let ready = match state {
            ObjectState::Ready(info) => Some(info),
            _ => None,
        };
        let text: Option<Box<dyn BlockLayout>> = if revealed || ready.is_none() {
            let rows = if revealed && block_is_one_line(cx, block) {
                vec![Row {
                    src: block.range,
                    pad: 0,
                }]
            } else if revealed {
                delimiter_rows(cx, block, &content)
            } else {
                content
            };
            let color = match state {
                ObjectState::Failed => tokens.error,
                ObjectState::Ready(_) => tokens.text,
                ObjectState::Pending | ObjectState::Blocked => tokens.text_muted,
            };
            let composition = composition_in(cx, block.range);
            let mut source = display_of(block.range, &rows, cx.buffer, composition);
            let base = TextStyle::new(cx.config.code_font.clone(), cx.config.body_size() * 0.95);
            for marker in &block.markers {
                let map = &source.display.map;
                let range = map.source_to_display(marker.start)..map.source_to_display(marker.end);
                source.display.styles.push(StyledRange {
                    range,
                    style: base.clone().with_color(tokens.marker),
                });
            }
            let text = SourceText::new(source.display, &base, cx.width, cx);
            Some(Box::new(SourceBlock {
                height: text.height(),
                text,
                color,
                access: access.clone(),
            }))
        } else {
            None
        };
        let text_height = text.as_ref().map_or(0.0, |text| text.height());
        let scene = ready.map(|info| {
            let size = fit(info.size, cx.width);
            let top = if text.is_some() {
                text_height + MARGIN
            } else {
                MARGIN
            };
            (
                info.visual,
                Rect::new((cx.width - size.width) / 2.0, top, size.width, size.height),
            )
        });
        let height = scene.map_or(text_height, |(_, rect)| rect.bottom() + MARGIN);
        // The source part names the block while it is shown.
        let access = text.is_none().then_some(access);
        Box::new(SceneLayout {
            text,
            scene,
            note: None,
            height,
            range: block.range,
            width: cx.width,
            menu: None,
            access,
        })
    }
}

/// The opening and closing delimiter lines and the content between them. The parser gives
/// every math block its opening delimiter as the first marker and the closing delimiter as the
/// second one, which an unterminated block lacks.
fn delimiter_rows(cx: &BlockCx<'_>, block: &Block, content: &[Row]) -> Vec<Row> {
    let line_row = |marker: &TextRange| Row {
        src: TextRange::new(
            marker.start,
            cx.buffer.line_range(cx.buffer.line_of(marker.start)).end,
        ),
        pad: 0,
    };
    let mut rows = Vec::with_capacity(content.len() + 2);
    rows.extend(block.markers.first().map(line_row));
    rows.extend_from_slice(content);
    rows.extend(block.markers.get(1).map(line_row));
    rows
}

/// True for `$$x$$` written on one line.
fn block_is_one_line(cx: &BlockCx<'_>, block: &Block) -> bool {
    cx.buffer.line_of(block.range.start) == cx.buffer.line_of(block.range.end)
}

/// Renders a whole `.mmd` document ([`DocumentKind::Mermaid`](crate::editor::DocumentKind)):
/// the source with the diagram always shown below it.
pub(super) struct MermaidDocumentRenderer {
    code: Rc<CodeRenderer>,
}

impl MermaidDocumentRenderer {
    pub fn new(code: Rc<CodeRenderer>) -> Self {
        Self { code }
    }
}

impl BlockRenderer for MermaidDocumentRenderer {
    fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        measure_mermaid(cx, block, &self.code, true)
    }
}

/// Draws a Mermaid fence: the diagram when the provider has one, otherwise the source as a code
/// block (with the "Unsupported diagram" note when rendering failed). A whole `.mmd`
/// `document` always shows its source above the diagram.
pub(super) fn measure_mermaid(
    cx: &BlockCx<'_>,
    block: &Block,
    code_renderer: &CodeRenderer,
    document: bool,
) -> Box<dyn BlockLayout> {
    let BlockKind::CodeBlock(code) = &block.kind else {
        return FallbackRenderer.measure(cx, block);
    };
    let tokens = &cx.theme.tokens;
    let strings = code_renderer.strings();
    let name = language_name(cx, code.info);
    let code_layout = code_renderer.build(cx, block, code, name.as_deref(), false);
    let source = code_layout.source().to_owned();
    let state = cx.objects.object(&ObjectRequest {
        kind: ObjectKind::Mermaid,
        source: &source,
        alt: "",
        width: None,
        height: None,
        font_size: cx.config.body_size(),
        max_width: cx.width,
        text_color: tokens.text,
        range: block.range,
    });
    let revealed = document || cx.reveal.touches(block.range);
    let code_height = code_layout.height();
    let (text, scene, note, height, menu): (TextPart, ScenePart, NotePart, f32, Option<String>) =
        match state {
            ObjectState::Ready(info) => {
                let size = fit(info.size, cx.width);
                let top = if revealed { code_height + MARGIN } else { 0.0 };
                let rect = Rect::new((cx.width - size.width) / 2.0, top, size.width, size.height);
                let text = revealed.then(|| Box::new(code_layout) as Box<dyn BlockLayout>);
                (
                    text,
                    Some((info.visual, rect)),
                    None,
                    rect.bottom() + if revealed { 0.0 } else { MARGIN },
                    Some(source),
                )
            }
            ObjectState::Failed => {
                let style =
                    TextStyle::new(cx.config.text_font.clone(), cx.config.body_size() * 0.85);
                let note = cx
                    .text
                    .layout(&strings.mermaid_unsupported, &style, &[], &[], cx.width);
                let top = code_height + NOTE_GAP;
                let height = top + note.size().height;
                (
                    Some(Box::new(code_layout) as Box<dyn BlockLayout>),
                    None,
                    Some((note, Point::new(0.0, top))),
                    height,
                    None,
                )
            }
            ObjectState::Pending | ObjectState::Blocked => (
                Some(Box::new(code_layout) as Box<dyn BlockLayout>),
                None,
                None,
                code_height,
                None,
            ),
        };
    Box::new(SceneLayout {
        text,
        scene,
        note,
        height,
        range: block.range,
        width: cx.width,
        menu,
        access: Some(strings.mermaid_diagram.clone()),
    })
}

/// A formula or diagram: an optional text part (the source) on top and an optional scene below.
struct SceneLayout {
    text: Option<Box<dyn BlockLayout>>,
    scene: Option<(ObjectVisual, Rect)>,
    /// The "Unsupported diagram" note and its position.
    note: Option<(Box<dyn TextLayout>, Point)>,
    height: f32,
    range: TextRange,
    width: f32,
    /// The Mermaid source offered to the context menu.
    menu: Option<String>,
    /// The accessible name; a layout without one is named by its source part.
    access: Option<String>,
}

impl BlockLayout for SceneLayout {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint_backdrop(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        if let Some(text) = &self.text {
            text.paint_backdrop(cx, canvas);
        }
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        if let Some(text) = &self.text {
            text.paint(cx, canvas);
        }
        if let Some((visual, rect)) = self.scene {
            draw_visual(canvas, visual, rect);
        }
        if let Some((note, at)) = &self.note {
            canvas.draw_text_layout(note.as_ref(), *at, cx.theme.tokens.warning);
        }
    }

    fn hit_test(&self, point: Point) -> usize {
        match &self.text {
            Some(text) => text.hit_test(point),
            None if point.x < self.width / 2.0 => self.range.start,
            None => self.range.end,
        }
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        if let Some(text) = &self.text {
            return text.caret_rect(offset);
        }
        // Without a source the scene is all there is (a layout with neither has the empty
        // rectangle of its height).
        let rect = self
            .scene
            .map_or(Rect::new(0.0, 0.0, 0.0, self.height), |(_, rect)| rect);
        let x = if offset > self.range.start {
            rect.right()
        } else {
            rect.x
        };
        Rect::new(x, rect.y, 0.0, rect.height)
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        match (&self.text, self.scene) {
            (Some(text), _) => text.selection_rects(range),
            (None, Some((_, rect))) if !range.is_empty() => vec![rect],
            _ => Vec::new(),
        }
    }

    fn line_range(&self, offset: usize) -> TextRange {
        match &self.text {
            Some(text) => text.line_range(offset),
            None => self.range,
        }
    }

    fn press(&self, point: Point, origin: Point, mods: Modifiers) -> Option<BlockPress> {
        self.text.as_ref()?.press(point, origin, mods)
    }

    fn cursor_at(&self, point: Point) -> Option<Cursor> {
        self.text.as_ref()?.cursor_at(point)
    }

    fn context_menu(&self, point: Point, origin: Point) -> Option<EditorEvent> {
        let source = self.menu.as_ref()?;
        Some(EditorEvent::Media(MediaEvent::MermaidMenu {
            range: self.range,
            source: source.clone(),
            anchor: Rect::new(origin.x + point.x, origin.y + point.y, 0.0, 0.0),
        }))
    }

    fn access_label(&self, strings: &EditorStrings) -> String {
        self.access.clone().unwrap_or_else(|| {
            self.text
                .as_ref()
                .map(|text| text.access_label(strings))
                .unwrap_or_default()
        })
    }
}

/// The source of a math block: monospace text in one color.
struct SourceBlock {
    text: SourceText,
    color: Color,
    height: f32,
    access: String,
}

impl BlockLayout for SourceBlock {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint(&self, _cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        self.text.paint(canvas, Point::ZERO, self.color);
    }

    fn hit_test(&self, point: Point) -> usize {
        self.text.hit(point)
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        self.text.caret(offset)
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        self.text.selection(range)
    }

    fn line_range(&self, offset: usize) -> TextRange {
        self.text.line_range(offset)
    }

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.access.clone()
    }
}
