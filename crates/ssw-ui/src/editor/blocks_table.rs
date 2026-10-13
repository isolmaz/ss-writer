//! Tables (spec §3.1): the grid renderer, the raw-Markdown mode and the table commands that need
//! the host (copy as Markdown / CSV / HTML, edit as raw Markdown).
//!
//! A GFM table whose cell model ([`TableModel`]) is representable is drawn as a grid: rounded
//! border, bold header, zebra body rows, per-column alignment, cells laid out like paragraphs
//! (inline markers hidden unless the caret touches the element, math and images through the
//! object provider). The caret lives in the cell's source text: positions in pipes, padding and
//! container prefixes belong to the neighbouring cell, and the delimiter row is not drawn (a
//! caret on it is shown at the end of the header). Tab, Shift+Tab, Enter and the re-alignment
//! when the caret leaves are the table operations of `ssw_core::ops::table`, which the editor
//! already runs; this module only has to answer where a source offset is on the grid and which
//! offset a point on the grid stands for.
//!
//! Tables the cell model cannot represent (escaped pipes, pipes inside code spans, rows longer
//! than the header) are shown as raw Markdown, and so is the table "Edit as raw Markdown"
//! ([`TableControl::edit_raw`]) was asked for, until the selection leaves it.
//!
//! The caret rectangle of a cell reaches to the top of its row on the first text line and to the
//! bottom of its row on the last one: the editor's `Up` / `Down` probe the pixel above or below
//! the caret, which has to belong to the neighbouring row and not to the cell's own padding.

use std::cell::Cell;
use std::iter;
use std::rc::Rc;

use ssw_core::command::CommandId;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::markdown::ast::{Alignment, Block, BlockKind, Table, TableCell, TableRow};
use ssw_core::ops::table::{CellPos, CopyFormat, TableModel, copy_table};
use ssw_core::text::{Buffer, TextRange};

use crate::canvas::{Canvas, Dash};
use crate::geometry::{CornerRadii, Point, Rect};
use crate::input::Cursor;
use crate::services::ClipboardContent;
use crate::text::{FontWeight, LineHeight, TextLayout, TextStyle};
use crate::widgets::SharedClipboard;

use super::block::{
    BlockClass, BlockCx, BlockLayout, BlockPaintCx, BlockRegistry, BlockRenderer, FallbackRenderer,
};
use super::config::{Composition, EditorEvent, EditorStrings, LinkTarget};
use super::display::{Display, DisplayParams, GapPolicy, build};
use super::view::EditorView;

/// The English source of the table strings (`assets/i18n/en/editor-table.ftl`).
pub const EN_FTL: &str = include_str!("../../../../assets/i18n/en/editor-table.ftl");

/// Horizontal cell padding as a multiple of the body size.
const PAD_X_EM: f32 = 0.75;
/// Vertical cell padding as a multiple of the body size.
const PAD_Y_EM: f32 = 0.25;
/// Tightest line height of cell text as a multiple of the font size: rows are denser than
/// paragraphs, but never looser than the line height setting.
const CELL_LINE_HEIGHT: f32 = 1.5;
/// Narrowest column as a multiple of the body size.
const MIN_COLUMN_EM: f32 = 3.5;
/// Corner radius of the table border.
const RADIUS: f32 = 6.0;
/// Width of the border and the cell separators.
const LINE: f32 = 1.0;
/// Opacity of the zebra fill: the selection is painted below the block and must stay visible.
const ZEBRA_OPACITY: f32 = 0.6;
/// How much wider than its column a cell's text may measure before it wraps (rounding noise of
/// the padding arithmetic must not wrap the last character of a column that fits exactly).
const WRAP_TOLERANCE: f32 = 0.01;

/// The localized strings of the table renderer (message ids in `editor-table.ftl`).
#[derive(Debug, Clone)]
pub struct TableStrings {
    localizer: Rc<Localizer>,
}

impl TableStrings {
    /// Strings resolved through `localizer`, which must hold `editor-table.ftl`.
    #[must_use]
    pub fn new(localizer: Rc<Localizer>) -> Self {
        Self { localizer }
    }

    /// Accessible description of a grid with `rows` rows (header included) and `columns` columns.
    fn access(&self, rows: usize, columns: usize) -> String {
        self.localizer.format(
            "table-access",
            &[
                ("rows", Arg::Int(rows as i64)),
                ("columns", Arg::Int(columns as i64)),
            ],
        )
    }

    /// Accessible description of a table shown as raw Markdown.
    fn access_raw(&self) -> String {
        self.localizer.text("table-access-raw")
    }
}

impl Default for TableStrings {
    /// The English strings.
    fn default() -> Self {
        Self::new(Rc::new(Localizer::new("en", &[("en", EN_FTL)])))
    }
}

/// Registers the table renderer with the English strings; see [`register_with`].
pub fn register(registry: &mut BlockRegistry) -> TableControl {
    register_with(registry, TableStrings::default())
}

/// Registers a [`TableRenderer`] for [`BlockClass::Table`] and returns the handle the host uses
/// for the table commands of the menu.
pub fn register_with(registry: &mut BlockRegistry, strings: TableStrings) -> TableControl {
    let renderer = Rc::new(TableRenderer::new(strings));
    let control = renderer.control();
    registry.register(BlockClass::Table, renderer);
    control
}

/// Renders GFM tables as a grid, or as raw Markdown when the cell model cannot represent them.
#[derive(Debug)]
pub struct TableRenderer {
    strings: TableStrings,
    /// Start of the table that was asked to be shown as raw Markdown.
    raw: Rc<Cell<Option<usize>>>,
}

impl TableRenderer {
    /// A renderer with `strings`.
    #[must_use]
    pub fn new(strings: TableStrings) -> Self {
        Self {
            strings,
            raw: Rc::new(Cell::new(None)),
        }
    }

    /// The handle for the table commands that need the host.
    #[must_use]
    pub fn control(&self) -> TableControl {
        TableControl {
            raw: Rc::clone(&self.raw),
        }
    }

    /// True while the table at `range` is shown raw by request: until the selection leaves it.
    fn raw_requested(&self, cx: &BlockCx<'_>, range: TextRange) -> bool {
        if self.raw.get() != Some(range.start) {
            return false;
        }
        if cx.reveal.selection_touches(range) {
            return true;
        }
        self.raw.set(None);
        false
    }
}

impl BlockRenderer for TableRenderer {
    fn measure(&self, cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        let BlockKind::Table(table) = &block.kind else {
            return FallbackRenderer.measure(cx, block);
        };
        let requested = self.raw_requested(cx, block.range);
        let representable = TableModel::from_ast(cx.ast, cx.buffer, block.range.start)
            .is_some_and(|model| model.raw_reason().is_none());
        if representable && !requested {
            Box::new(measure_grid(cx, table, &self.strings))
        } else {
            Box::new(RawTable {
                inner: FallbackRenderer.measure(cx, block),
                rows: raw_rows(cx.buffer, table),
                label: self.strings.access_raw(),
            })
        }
    }
}

/// The host side of the table commands: the commands of the table context menu that are not
/// editing operations of the editor itself.
#[derive(Debug, Clone)]
pub struct TableControl {
    raw: Rc<Cell<Option<usize>>>,
}

impl TableControl {
    /// Runs a table command that needs the host: `TableCopyAsMarkdown`, `TableCopyAsCsv` and
    /// `TableCopyAsHtml` put the table around the caret on `clipboard` (HTML also as an HTML
    /// fragment), `TableEditRaw` shows it as raw Markdown until the selection leaves it. Returns
    /// whether the command applied: `false` for other commands, with the caret outside a table
    /// and when the clipboard refused. The other table commands (rows, columns, alignment) go
    /// through [`EditorView::execute`].
    pub fn execute(
        &self,
        view: &mut EditorView,
        clipboard: &SharedClipboard,
        command: CommandId,
    ) -> bool {
        match command {
            CommandId::TableEditRaw => self.edit_raw(view),
            CommandId::TableCopyAsMarkdown => copy(view, clipboard, CopyFormat::Markdown),
            CommandId::TableCopyAsCsv => copy(view, clipboard, CopyFormat::Csv),
            CommandId::TableCopyAsHtml => copy(view, clipboard, CopyFormat::Html),
            _ => false,
        }
    }

    /// Shows the table around the caret as raw Markdown until the selection leaves it. Returns
    /// `false` when the caret is not in a table.
    pub fn edit_raw(&self, view: &mut EditorView) -> bool {
        let start = {
            let doc = view.document().borrow();
            TableModel::from_ast(doc.ast(), doc.buffer(), doc.primary_selection().head)
                .map(|model| model.table().range.start)
        };
        let Some(start) = start else {
            return false;
        };
        self.raw.set(Some(start));
        view.invalidate_objects();
        true
    }
}

/// Puts the table around the caret on the clipboard as `format`.
fn copy(view: &EditorView, clipboard: &SharedClipboard, format: CopyFormat) -> bool {
    let text = copy_table(view.document().borrow().ctx(), format);
    let Some(text) = text else {
        return false;
    };
    let content = if format == CopyFormat::Html {
        ClipboardContent {
            html: Some(text.clone()),
            text: Some(text),
            ..ClipboardContent::default()
        }
    } else {
        ClipboardContent::from_text(text)
    };
    clipboard.borrow_mut().set(content).is_ok()
}

// ----- raw mode -------------------------------------------------------------------------------

/// The source line a table row is on and the ranges of its cells (no layout needed).
struct RawRow {
    line_start: usize,
    cells: Vec<TextRange>,
}

fn raw_rows(buffer: &Buffer, table: &Table) -> Vec<RawRow> {
    iter::once(&table.header)
        .chain(&table.rows)
        .map(|row| RawRow {
            line_start: line_start(buffer, row),
            cells: row.cells.iter().map(|cell| cell.range).collect(),
        })
        .collect()
}

/// Start of the line of `row`, its container prefix included.
fn line_start(buffer: &Buffer, row: &TableRow) -> usize {
    buffer.line_range(buffer.line_of(row.range.start)).start
}

/// A table shown as its source: the dimmed monospace text of the fallback renderer, plus the
/// table's cell menu and description.
struct RawTable {
    inner: Box<dyn BlockLayout>,
    rows: Vec<RawRow>,
    label: String,
}

impl RawTable {
    /// The cell holding `offset`: the row whose line starts last at or before it, the first cell
    /// that ends at or after it.
    fn cell_at(&self, offset: usize) -> CellPos {
        let row = self
            .rows
            .iter()
            .rposition(|row| row.line_start <= offset)
            .unwrap_or(0);
        let cells = &self.rows[row].cells;
        // A table row always holds at least one cell.
        let col = cells
            .iter()
            .position(|cell| offset <= cell.end)
            .unwrap_or(cells.len() - 1);
        CellPos { row, col }
    }
}

impl BlockLayout for RawTable {
    fn height(&self) -> f32 {
        self.inner.height()
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        self.inner.paint(cx, canvas);
    }

    fn hit_test(&self, point: Point) -> usize {
        self.inner.hit_test(point)
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        self.inner.caret_rect(offset)
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        self.inner.selection_rects(range)
    }

    fn line_range(&self, offset: usize) -> TextRange {
        self.inner.line_range(offset)
    }

    fn context_menu(&self, point: Point, origin: Point) -> Option<EditorEvent> {
        let offset = self.inner.hit_test(point);
        let anchor = self.inner.caret_rect(offset);
        Some(EditorEvent::TableMenu {
            anchor: anchor.translate(origin.x, origin.y),
            cell: self.cell_at(offset),
            raw: true,
        })
    }

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.label.clone()
    }
}

// ----- the grid -------------------------------------------------------------------------------

/// A cell with its display text and a layout at its natural (unwrapped) width.
struct Prepared {
    range: TextRange,
    content: TextRange,
    display: Display,
    layout: Box<dyn TextLayout>,
    natural: f32,
    composition_at: Option<usize>,
}

/// One laid-out cell.
struct CellBox {
    /// The space between the neighbouring pipes, padding included.
    range: TextRange,
    /// The cell text without padding: where the caret lives.
    content: TextRange,
    /// The cell, border to border, block-local.
    rect: Rect,
    /// Top-left corner of the text, block-local.
    origin: Point,
    display: Display,
    layout: Box<dyn TextLayout>,
    /// Where the IME composition is inserted.
    composition_at: Option<usize>,
}

impl CellBox {
    fn display_offset(&self, offset: usize) -> usize {
        let offset = offset.clamp(self.content.start, self.content.end);
        match (&self.display.composition, self.composition_at) {
            (Some(composition), Some(at)) if at == offset => composition.caret,
            _ => self.display.map.source_to_display(offset),
        }
    }

    fn local(&self, point: Point) -> Point {
        Point::new(point.x - self.origin.x, point.y - self.origin.y)
    }
}

/// One row of the grid.
struct RowBox {
    top: f32,
    height: f32,
    /// Start of the line the row is on (container prefix included).
    line_start: usize,
    /// Never empty: a table row holds at least one cell.
    cells: Vec<CellBox>,
}

/// A table drawn as a grid.
struct GridLayout {
    /// Header first; never empty.
    rows: Vec<RowBox>,
    /// Right edge of every column.
    column_right: Vec<f32>,
    width: f32,
    height: f32,
    label: String,
}

impl GridLayout {
    /// The cell at `row`, `col`: indices come from [`GridLayout::locate`] or
    /// [`GridLayout::locate_point`].
    fn cell(&self, (row, col): (usize, usize)) -> &CellBox {
        &self.rows[row].cells[col]
    }

    /// The cell holding source `offset`: the row whose line starts last at or before it, the
    /// first cell that ends at or after it.
    fn locate(&self, offset: usize) -> (usize, usize) {
        let row = self
            .rows
            .iter()
            .rposition(|row| row.line_start <= offset)
            .unwrap_or(0);
        let cells = &self.rows[row].cells;
        let col = cells
            .iter()
            .position(|cell| offset <= cell.range.end)
            .unwrap_or(cells.len() - 1);
        (row, col)
    }

    /// The cell under block-local `point`; points outside the grid and in the empty part of a
    /// short row clamp to the nearest cell.
    fn locate_point(&self, point: Point) -> (usize, usize) {
        let row = self
            .rows
            .iter()
            .position(|row| point.y < row.top + row.height)
            .unwrap_or(self.rows.len() - 1);
        let col = self
            .column_right
            .iter()
            .position(|right| point.x < *right)
            .unwrap_or(self.column_right.len() - 1);
        (row, col.min(self.rows[row].cells.len() - 1))
    }
}

impl BlockLayout for GridLayout {
    fn height(&self) -> f32 {
        self.height
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        let frame = Rect::new(0.0, 0.0, self.width, self.height);
        let radii = CornerRadii::uniform(RADIUS);
        // The tinted rows span the whole width and the first one is the third row, so only
        // the last row can touch a rounded corner (the bottom ones): it is filled with them
        // rounded instead of clipping every row to the frame, which costs a layer per table.
        let zebra = tokens.table_zebra.with_alpha(ZEBRA_OPACITY);
        let last = self.rows.len() - 1;
        for (index, row) in self.rows.iter().enumerate().skip(2).step_by(2) {
            let area = Rect::new(0.0, row.top, self.width, row.height);
            if index == last {
                canvas.fill_rounded_rect(area, CornerRadii::new(0.0, 0.0, RADIUS, RADIUS), zebra);
            } else {
                canvas.fill_rect(area, zebra);
            }
        }
        for row in self.rows.iter().skip(1) {
            canvas.stroke_line(
                Point::new(0.0, row.top),
                Point::new(self.width, row.top),
                tokens.table_border,
                LINE,
                Dash::Solid,
            );
        }
        for right in &self.column_right[..self.column_right.len() - 1] {
            canvas.stroke_line(
                Point::new(*right, 0.0),
                Point::new(*right, self.height),
                tokens.table_border,
                LINE,
                Dash::Solid,
            );
        }
        canvas.stroke_rounded_rect(frame, radii, tokens.table_border, LINE);
        for cell in self.rows.iter().flat_map(|row| &row.cells) {
            canvas.push_clip_rect(cell.rect);
            canvas.save();
            canvas.translate(cell.origin.x, cell.origin.y);
            cell.display.paint_behind(cell.layout.as_ref(), canvas);
            canvas.draw_text_layout(cell.layout.as_ref(), Point::ZERO, tokens.text);
            cell.display
                .paint_front(cell.layout.as_ref(), canvas, tokens.text);
            canvas.restore();
            canvas.pop_clip();
        }
    }

    fn hit_test(&self, point: Point) -> usize {
        let cell = self.cell(self.locate_point(point));
        let hit = cell.layout.hit_test_point(cell.local(point));
        cell.display.map.display_to_source(hit.insertion_offset())
    }

    fn caret_rect(&self, offset: usize) -> Rect {
        let at = self.locate(offset);
        let (row, cell) = (&self.rows[at.0], self.cell(at));
        let display = cell.display_offset(offset);
        let caret = cell
            .layout
            .caret_rect(display, false)
            .translate(cell.origin.x, cell.origin.y);
        let line = cell.layout.line_for_offset(display);
        let top = if line == 0 { row.top } else { caret.y };
        let bottom = if line + 1 == cell.layout.line_count() {
            row.top + row.height
        } else {
            caret.bottom()
        };
        Rect::new(caret.x, top, caret.width, bottom - top)
    }

    fn selection_rects(&self, range: TextRange) -> Vec<Rect> {
        let mut rects = Vec::new();
        for cell in self.rows.iter().flat_map(|row| &row.cells) {
            let Some(shared) = range.intersection(cell.content) else {
                continue;
            };
            let map = &cell.display.map;
            let text = map.source_to_display(shared.start)..map.source_to_display(shared.end);
            rects.extend(
                cell.layout
                    .range_rects(text)
                    .into_iter()
                    .map(|rect| rect.translate(cell.origin.x, cell.origin.y)),
            );
        }
        rects
    }

    fn line_range(&self, offset: usize) -> TextRange {
        let cell = self.cell(self.locate(offset));
        let display = cell.display_offset(offset);
        let map = &cell.display.map;
        cell.layout
            .line_metrics(cell.layout.line_for_offset(display))
            .map_or(TextRange::at(offset), |line| {
                let start = map.display_to_source(line.start);
                TextRange::new(start, map.display_to_source(line.content_end()).max(start))
            })
    }

    fn link_at(&self, point: Point) -> Option<LinkTarget> {
        let cell = self.cell(self.locate_point(point));
        let hit = cell.layout.hit_test_point(cell.local(point));
        if !hit.inside {
            return None;
        }
        cell.display
            .link_at(hit.offset)
            .map(|link| link.target.clone())
    }

    fn cursor_at(&self, point: Point) -> Option<Cursor> {
        (point.x >= self.width).then_some(Cursor::Arrow)
    }

    fn context_menu(&self, point: Point, origin: Point) -> Option<EditorEvent> {
        if !Rect::new(0.0, 0.0, self.width, self.height).contains(point) {
            return None;
        }
        let (row, col) = self.locate_point(point);
        Some(EditorEvent::TableMenu {
            anchor: self.cell((row, col)).rect.translate(origin.x, origin.y),
            cell: CellPos { row, col },
            raw: false,
        })
    }

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.label.clone()
    }
}

/// Distributes `available` over columns that want `natural` widths: columns keep their natural
/// width when everything fits, otherwise the widest ones give way first (down to `min`).
fn fit_columns(natural: &[f32], available: f32, min: f32) -> Vec<f32> {
    if natural.iter().sum::<f32>() <= available {
        return natural.to_vec();
    }
    // The narrowest column never needs more than an equal share of the space.
    let min = min.min(available / natural.len() as f32);
    let total = |cap: f32| -> f32 { natural.iter().map(|w| w.min(cap).max(min)).sum() };
    let mut low = min;
    let mut high = natural.iter().copied().fold(min, f32::max);
    for _ in 0..32 {
        let mid = (low + high) / 2.0;
        if total(mid) <= available {
            low = mid;
        } else {
            high = mid;
        }
    }
    natural.iter().map(|w| w.min(low).max(min)).collect()
}

/// Builds the display text of `cell` and measures it unwrapped.
fn prepare(cx: &BlockCx<'_>, cell: &TableCell, base: &TextStyle) -> Prepared {
    let composition = cx
        .composition
        .filter(|comp| cell.range.touches(comp.at))
        .map(|comp| Composition {
            at: comp.at.clamp(cell.content.start, cell.content.end),
            ..comp.clone()
        });
    let source = cx.buffer.slice(cell.content);
    let display = build(&DisplayParams {
        src: &source,
        range: cell.content,
        inlines: &cell.inlines,
        tokens: &cx.theme.tokens,
        base,
        mono_family: &cx.config.code_font,
        reveal: cx.reveal,
        gap: GapPolicy::Raw,
        objects: cx.objects,
        max_width: cx.width,
        composition: composition.as_ref(),
    });
    let layout = display.layout(cx.text, base, f32::INFINITY);
    Prepared {
        range: cell.range,
        content: cell.content,
        natural: layout.size().width,
        composition_at: composition.map(|comp| comp.at),
        display,
        layout,
    }
}

/// Where the text of a cell starts horizontally: after the padding, moved by the column's
/// alignment inside the free space.
fn text_x(alignment: Alignment, left: f32, width: f32, pad_x: f32, text_width: f32) -> f32 {
    let free = (width - 2.0 * pad_x - text_width).max(0.0);
    left + pad_x
        + match alignment {
            Alignment::Center => free / 2.0,
            Alignment::Right => free,
            Alignment::None | Alignment::Left => 0.0,
        }
}

/// Lays `table` out as a grid in `cx.width`.
fn measure_grid(cx: &BlockCx<'_>, table: &Table, strings: &TableStrings) -> GridLayout {
    let body = cx.config.body_size();
    let (pad_x, pad_y) = (body * PAD_X_EM, body * PAD_Y_EM);
    let regular = TextStyle::new(cx.config.text_font.clone(), body).with_line_height(
        LineHeight::Multiple(cx.config.line_height.min(CELL_LINE_HEIGHT)),
    );
    let header = regular.clone().with_weight(FontWeight::BOLD);
    let style_of = |row: usize| if row == 0 { &header } else { &regular };

    let source_rows: Vec<&TableRow> = iter::once(&table.header).chain(&table.rows).collect();
    let prepared: Vec<Vec<Prepared>> = source_rows
        .iter()
        .enumerate()
        .map(|(row, source)| {
            let base = style_of(row);
            source
                .cells
                .iter()
                .map(|cell| prepare(cx, cell, base))
                .collect()
        })
        .collect();

    let min_column = body * MIN_COLUMN_EM;
    let mut natural = vec![min_column; table.alignments.len()];
    for cells in &prepared {
        for (col, cell) in cells.iter().enumerate() {
            natural[col] = natural[col].max(cell.natural + 2.0 * pad_x);
        }
    }
    let widths = fit_columns(&natural, cx.width, min_column);
    let mut column_right = Vec::with_capacity(widths.len());
    let mut right = 0.0;
    for width in &widths {
        right += width;
        column_right.push(right);
    }

    let mut rows = Vec::with_capacity(prepared.len());
    let mut top = 0.0;
    for (row, (cells, source)) in prepared.into_iter().zip(&source_rows).enumerate() {
        let base = style_of(row);
        // Wrap the cells whose column is narrower than their text; the others keep the layout
        // they were measured with.
        let fitted: Vec<Prepared> = cells
            .into_iter()
            .enumerate()
            .map(|(col, cell)| {
                let inner = widths[col] - 2.0 * pad_x;
                if cell.natural > inner + WRAP_TOLERANCE {
                    let layout = cell.display.layout(cx.text, base, inner);
                    Prepared { layout, ..cell }
                } else {
                    cell
                }
            })
            .collect();
        let content_height = fitted
            .iter()
            .map(|cell| cell.layout.size().height)
            .fold(0.0, f32::max);
        let height = content_height + 2.0 * pad_y;
        let boxes = fitted
            .into_iter()
            .enumerate()
            .map(|(col, cell)| {
                let left = column_right[col] - widths[col];
                let text_width = cell.layout.size().width;
                CellBox {
                    range: cell.range,
                    content: cell.content,
                    rect: Rect::new(left, top, widths[col], height),
                    origin: Point::new(
                        text_x(table.alignments[col], left, widths[col], pad_x, text_width),
                        top + pad_y,
                    ),
                    display: cell.display,
                    layout: cell.layout,
                    composition_at: cell.composition_at,
                }
            })
            .collect();
        rows.push(RowBox {
            top,
            height,
            line_start: line_start(cx.buffer, source),
            cells: boxes,
        });
        top += height;
    }
    GridLayout {
        label: strings.access(rows.len(), widths.len()),
        rows,
        width: right,
        height: top,
        column_right,
    }
}
