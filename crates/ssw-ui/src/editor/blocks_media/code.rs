//! Fenced and indented code blocks: a rounded `code_bg` box with syntax-highlighted text; on
//! hover the language label and a Copy button; the fences show (dimmed) while the caret is in
//! the block.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ssw_core::markdown::ast::{Block, BlockKind, CodeBlock};
use ssw_core::text::{Buffer, TextRange};

use crate::canvas::Canvas;
use crate::editor::block::{
    BlockCx, BlockLayout, BlockPaintCx, BlockPress, BlockRenderer, FallbackRenderer,
};
use crate::editor::config::{EditorEvent, EditorStrings};
use crate::editor::highlight::{Highlighter, Language, SyntaxColors, Token};
use crate::geometry::{CornerRadii, Point, Rect};
use crate::input::{Cursor, Modifiers};
use crate::text::{StyledRange, TextLayout, TextStyle};

use super::MediaEvent;
use super::MediaStrings;
use super::diagram;
use super::verbatim::{Row, RowSpan, SourceText, display_of};

/// Horizontal padding inside the box.
const PAD_X: f32 = 14.0;
/// Vertical padding inside the box.
const PAD_Y: f32 = 12.0;
/// Corner radius of the box.
const RADIUS: f32 = 6.0;

/// Renders [`BlockKind::CodeBlock`]; `mermaid` fences are drawn by the diagram renderer.
pub(super) struct CodeRenderer {
    strings: Rc<MediaStrings>,
    highlighter: RefCell<Highlighter>,
}

impl CodeRenderer {
    pub fn new(strings: Rc<MediaStrings>) -> Self {
        Self {
            strings,
            highlighter: RefCell::new(Highlighter::new()),
        }
    }
}

/// The row of the line that contains `offset`, from `offset` to the end of the line.
fn line_row(buffer: &Buffer, offset: usize) -> Row {
    Row {
        src: TextRange::new(offset, buffer.line_range(buffer.line_of(offset)).end),
        pad: 0,
    }
}

/// First word of the info string.
pub(super) fn language_name(cx: &BlockCx<'_>, info: Option<TextRange>) -> Option<String> {
    let info = cx.buffer.slice(info?);
    info.split_whitespace().next().map(str::to_owned)
}

impl BlockRenderer for CodeRenderer {
    fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        let BlockKind::CodeBlock(code) = &block.kind else {
            return FallbackRenderer.measure(cx, block);
        };
        let name = language_name(cx, code.info);
        if name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("mermaid"))
        {
            return diagram::measure_mermaid(cx, block, self, false);
        }
        Box::new(self.build(
            cx,
            block,
            code,
            name.as_deref(),
            cx.config.syntax_highlighting,
        ))
    }
}

impl CodeRenderer {
    /// The localized strings.
    pub(super) fn strings(&self) -> &MediaStrings {
        &self.strings
    }

    /// The layout of a code block; `name` is the first word of its info string and
    /// `highlight` turns token coloring on.
    pub(super) fn build(
        &self,
        cx: &BlockCx<'_>,
        block: &Block,
        code: &CodeBlock,
        name: Option<&str>,
        highlight: bool,
    ) -> CodeLayout {
        let tokens = &cx.theme.tokens;
        let revealed = cx.reveal.touches(block.range);
        let fence = code.fence.filter(|_| revealed);
        let mut rows = Vec::with_capacity(code.lines.len() + 2);
        let open_rows = usize::from(fence.is_some());
        if let Some(fence) = fence {
            rows.push(line_row(cx.buffer, fence.open.start));
        }
        rows.extend(code.lines.iter().map(Row::of));
        if let Some(close) = fence.and_then(|fence| fence.close) {
            rows.push(line_row(cx.buffer, close.start));
        }
        let composition = cx.composition.filter(|comp| block.range.touches(comp.at));
        let mut source = display_of(block.range, &rows, cx.buffer, composition);

        let size = cx.config.body_size() * 0.9;
        let base = TextStyle::new(cx.config.code_font.clone(), size);
        let mut code_text = String::new();
        let mut starts = Vec::with_capacity(code.lines.len());
        for row in &rows[open_rows..open_rows + code.lines.len()] {
            starts.push(code_text.len());
            code_text.push_str(&" ".repeat(usize::from(row.pad)));
            code_text.push_str(&cx.buffer.slice(row.src));
            code_text.push('\n');
        }
        let language = if highlight {
            name.and_then(Language::from_name)
        } else {
            None
        };
        if let Some(language) = language {
            let colors = SyntaxColors::from_theme(cx.theme);
            let found =
                self.highlighter
                    .borrow_mut()
                    .highlight(block.range.start, language, &code_text);
            let spans = &source.spans[open_rows..open_rows + code.lines.len()];
            for token in &found {
                for range in map_token(token, &starts, spans) {
                    source.display.styles.push(colors.styled(&range, &base, 0));
                }
            }
        }
        let marker_style = base.clone().with_color(tokens.marker);
        for index in (0..open_rows).chain(open_rows + code.lines.len()..rows.len()) {
            let span = source.spans[index];
            source.display.styles.push(StyledRange {
                range: span.disp_start..span.disp_start + span.text_len,
                style: marker_style.clone(),
            });
        }

        let text_width = (cx.width - 2.0 * PAD_X).max(1.0);
        let text = SourceText::new(source.display, &base, text_width, cx);
        let height = text.height().max(size * 1.2) + 2.0 * PAD_Y;

        let control = TextStyle::new(cx.config.text_font.clone(), cx.config.body_size() * 0.75);
        let measure_control = |label: &str| {
            let layout = cx.text.layout(label, &control, &[], &[], f32::INFINITY);
            let size = layout.size();
            (layout, size)
        };
        let (copy_text, copy_size) = measure_control(&self.strings.code_copy);
        let copy_rect = Rect::new(
            cx.width - 8.0 - (copy_size.width + 16.0),
            6.0,
            copy_size.width + 16.0,
            copy_size.height + 6.0,
        );
        let label = name.map(|name| {
            let (layout, label_size) = measure_control(name);
            let at = Point::new(
                copy_rect.x - 10.0 - label_size.width,
                copy_rect.y + (copy_rect.height - label_size.height) / 2.0,
            );
            (layout, at)
        });
        let access = match name {
            Some(name) => self.strings.code_named(name),
            None => self.strings.code_block.clone(),
        };
        CodeLayout {
            text,
            range: block.range,
            width: cx.width,
            height,
            code: code_text,
            copy: (copy_text, copy_rect),
            label,
            hovered: Cell::new(false),
            access,
        }
    }
}

/// The style ranges (display offsets, without the composition) of `token`, which lies inside
/// one code row; a composition inside the token splits it.
fn map_token(token: &Token, starts: &[usize], spans: &[RowSpan]) -> Vec<Token> {
    // A token lies inside the text the rows were joined into, so its row exists.
    let row = starts
        .partition_point(|&start| start <= token.start)
        .saturating_sub(1);
    let (start, span) = (starts[row], &spans[row]);
    let (from, to) = (token.start - start, token.end - start);
    let at = |relative: usize| span.disp_start + relative;
    let piece = |from: usize, to: usize| Token {
        start: at(from),
        end: at(to),
        kind: token.kind,
    };
    match span.composition {
        Some((inserted, len)) if from >= inserted => vec![piece(from + len, to + len)],
        Some((inserted, len)) if to > inserted => {
            vec![piece(from, inserted), piece(inserted + len, to + len)]
        }
        _ => vec![piece(from, to)],
    }
}

pub(super) struct CodeLayout {
    text: SourceText,
    range: TextRange,
    width: f32,
    height: f32,
    /// The code, every line followed by `\n`.
    code: String,
    copy: (Box<dyn TextLayout>, Rect),
    /// The language label and its position.
    label: Option<(Box<dyn TextLayout>, Point)>,
    /// The pointer was over the block when it was last painted.
    hovered: Cell<bool>,
    access: String,
}

impl CodeLayout {
    /// The code without the final line break.
    pub fn source(&self) -> &str {
        self.code.strip_suffix('\n').unwrap_or(&self.code)
    }

    fn on_copy(&self, point: Point) -> bool {
        self.hovered.get() && self.copy.1.contains(point)
    }

    fn origin() -> Point {
        Point::new(PAD_X, PAD_Y)
    }
}

impl BlockLayout for CodeLayout {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint_backdrop(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        canvas.fill_rounded_rect(
            Rect::new(0.0, 0.0, self.width, self.height),
            CornerRadii::uniform(RADIUS),
            cx.theme.tokens.code_bg,
        );
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        self.hovered.set(cx.hovered);
        self.text.paint(canvas, Self::origin(), tokens.text);
        if !cx.hovered {
            return;
        }
        let (copy, rect) = &self.copy;
        canvas.fill_rounded_rect(*rect, CornerRadii::uniform(4.0), tokens.hover_bg);
        let size = copy.size();
        canvas.draw_text_layout(
            copy.as_ref(),
            Point::new(
                rect.x + (rect.width - size.width) / 2.0,
                rect.y + (rect.height - size.height) / 2.0,
            ),
            tokens.hover_text,
        );
        if let Some((label, at)) = &self.label {
            canvas.draw_text_layout(label.as_ref(), *at, tokens.text_muted);
        }
    }

    fn hit_test(&self, point: Point) -> usize {
        let origin = Self::origin();
        self.text
            .hit(Point::new(point.x - origin.x, point.y - origin.y))
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        let origin = Self::origin();
        self.text.caret(offset).translate(origin.x, origin.y)
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        let origin = Self::origin();
        self.text
            .selection(range)
            .into_iter()
            .map(|rect| rect.translate(origin.x, origin.y))
            .collect()
    }

    fn line_range(&self, offset: usize) -> TextRange {
        self.text.line_range(offset)
    }

    fn press(&self, point: Point, _origin: Point, _mods: Modifiers) -> Option<BlockPress> {
        self.on_copy(point).then(|| {
            BlockPress::Event(EditorEvent::Media(MediaEvent::CopyCode {
                range: self.range,
                text: self.code.clone(),
            }))
        })
    }

    fn cursor_at(&self, point: Point) -> Option<Cursor> {
        self.on_copy(point).then_some(Cursor::Hand)
    }

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.access.clone()
    }
}
