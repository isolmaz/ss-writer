//! Tables: the cell model over the Markdown source and the table commands of spec §3.1.
//!
//! [`TableModel::from_ast`] maps a parsed pipe table to rows of cells with their source ranges.
//! A table the grid cannot represent faithfully (an escaped `|`, an inline code span crossing a
//! cell boundary, rows with more cells than the header) is [`TableModel::Raw`]: it is edited as
//! source and every grid command declines it.
//!
//! The commands are pure functions over a [`Ctx`] and return the [`Transaction`] to apply, one
//! undo step each; they act on the cell holding the primary caret. Navigation commands that stay
//! inside the existing source return a transaction without edits (a caret move only).
//!
//! Column commands, alignment changes and formatting re-render every row of the table with
//! aligned pipes (display width by `unicode-width`, so CJK and emoji line up); row commands
//! only insert or delete the one line they concern. Container prefixes (`> `, list
//! indentation) of the table lines are never touched.

use std::borrow::Cow;
use std::iter;

use unicode_width::UnicodeWidthStr;

use crate::edit::{Edit, Selection, Transaction};
use crate::markdown::ast::{self, Alignment, Block, BlockKind, Inline, InlineKind, iter_inlines};
use crate::text::{Buffer, TextRange};

use super::Ctx;
use super::typing::continuation_of;

/// Narrowest column of a formatted table: the shortest valid delimiter cell `:-:`.
const MIN_WIDTH: usize = 3;

/// Why a table is edited as raw Markdown instead of as a grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawReason {
    /// A cell holds an escaped pipe (`\|`).
    EscapedPipe,
    /// An inline code span would have to cross a cell boundary: a pipe inside code.
    PipeInCode,
    /// A row has more cells than the header; the extra cells are no part of the table.
    ExtraCells,
}

/// Position of a cell: `row` 0 is the header, the body rows follow; `col` counts from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellPos {
    pub row: usize,
    pub col: usize,
}

/// One cell of a table row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The space between the neighbouring pipes, padding included.
    pub range: TextRange,
    /// The cell text without padding.
    pub content: TextRange,
    /// Source text of the content, `\|` as written.
    pub text: String,
    /// Text a reader sees: markers stripped, escapes and entities resolved.
    pub plain: String,
    /// The content as inline HTML.
    pub html: String,
}

impl Cell {
    /// Where the caret lands when the cell is entered: behind its text, or, for an empty cell,
    /// after the first padding space.
    fn entry_caret(&self) -> usize {
        if self.content.is_empty() {
            self.range.start + usize::from(self.range.len() >= 2)
        } else {
            self.content.end
        }
    }
}

/// One line of a table (header or body row). Always holds at least one cell; a body row may
/// hold fewer cells than the table has columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The row without container prefix and trailing blanks.
    pub range: TextRange,
    pub cells: Vec<Cell>,
}

/// A table as a grid of source cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellTable {
    /// Header line to the end of the last row.
    pub range: TextRange,
    /// One entry per column.
    pub alignments: Vec<Alignment>,
    /// The header (index 0) followed by the body rows.
    pub rows: Vec<Row>,
    /// The delimiter row, between the header and the first body row.
    pub delimiter: TextRange,
}

/// A table of the document, as the grid sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableModel {
    /// Representable as a grid: the cell commands apply.
    Cells(CellTable),
    /// Not representable: shown and edited as raw Markdown. `table` still carries the parsed
    /// cells (for copying).
    Raw { table: CellTable, reason: RawReason },
}

impl TableModel {
    /// The table touching `offset`, `None` when `offset` is not inside a table.
    pub fn from_ast(ast: &ast::Document, buffer: &Buffer, offset: usize) -> Option<TableModel> {
        let (block, table) = ast.path_at(offset).into_iter().rev().find_map(|node| {
            let block = node.as_block()?;
            match &block.kind {
                BlockKind::Table(table) => Some((block, table)),
                _ => None,
            }
        })?;
        Some(build(block, table, buffer))
    }

    /// The parsed cells, also for a raw table.
    pub fn table(&self) -> &CellTable {
        match self {
            TableModel::Cells(table) | TableModel::Raw { table, .. } => table,
        }
    }

    /// Why the table is raw; `None` for a grid table.
    pub fn raw_reason(&self) -> Option<RawReason> {
        match self {
            TableModel::Cells(_) => None,
            TableModel::Raw { reason, .. } => Some(*reason),
        }
    }
}

impl CellTable {
    /// Number of columns (the header's cells).
    pub fn columns(&self) -> usize {
        self.alignments.len()
    }

    /// The cell at `pos`; `None` when its row is shorter.
    pub fn cell(&self, pos: CellPos) -> Option<&Cell> {
        self.rows.get(pos.row)?.cells.get(pos.col)
    }

    /// The cell whose line and column hold `offset`: a caret in the container prefix or on a
    /// pipe counts for the cell next to it (a pipe belongs to the cell before it). `None` on
    /// the delimiter line and outside the table.
    pub fn cell_at(&self, buffer: &Buffer, offset: usize) -> Option<CellPos> {
        let line = buffer.line_of(offset);
        let row = self
            .rows
            .iter()
            .position(|row| buffer.line_of(row.range.start) == line)?;
        let cells = &self.rows[row].cells;
        let col = cells
            .iter()
            .position(|cell| offset <= cell.range.end)
            .unwrap_or(cells.len().saturating_sub(1));
        Some(CellPos { row, col })
    }

    /// Bytes between the start of the content of the cell at `pos` and `offset` (clamped to the
    /// content).
    fn within(&self, pos: CellPos, offset: usize) -> usize {
        self.cell(pos).map_or(0, |cell| {
            offset.clamp(cell.content.start, cell.content.end) - cell.content.start
        })
    }

    /// Cell texts of `row`, padded with empty cells to the column count.
    fn grid_row(&self, row: &Row) -> Vec<String> {
        (0..self.columns())
            .map(|col| {
                row.cells
                    .get(col)
                    .map_or_else(String::new, |c| c.text.clone())
            })
            .collect()
    }

    fn grid(&self) -> Grid {
        Grid {
            alignments: self.alignments.clone(),
            rows: self.rows.iter().map(|row| self.grid_row(row)).collect(),
        }
    }

    /// The table as Markdown with aligned pipes (without container prefixes).
    pub fn to_markdown(&self) -> String {
        let rendered = render(&self.grid());
        let mut lines: Vec<&str> = Vec::with_capacity(rendered.rows.len() + 1);
        // The header always exists, so `rows` is never empty.
        lines.push(&rendered.rows[0].text);
        lines.push(&rendered.delimiter);
        lines.extend(rendered.rows[1..].iter().map(|row| row.text.as_str()));
        lines.join("\n")
    }

    /// The table as CSV (RFC 4180 quoting, records separated by `\r\n`), cells as plain text.
    pub fn to_csv(&self) -> String {
        let records: Vec<String> = self
            .rows
            .iter()
            .map(|row| {
                let fields: Vec<Cow<'_, str>> = (0..self.columns())
                    .map(|col| csv_field(row.cells.get(col).map_or("", |c| c.plain.as_str())))
                    .collect();
                fields.join(",")
            })
            .collect();
        records.join("\r\n")
    }

    /// The table as an HTML `<table>`; cells keep their inline formatting, columns their
    /// alignment.
    pub fn to_html(&self) -> String {
        let mut lines = vec![
            "<table>".to_owned(),
            "<thead>".to_owned(),
            self.html_row(&self.rows[0], "th"),
            "</thead>".to_owned(),
        ];
        if self.rows.len() > 1 {
            lines.push("<tbody>".to_owned());
            lines.extend(self.rows[1..].iter().map(|row| self.html_row(row, "td")));
            lines.push("</tbody>".to_owned());
        }
        lines.push("</table>".to_owned());
        lines.join("\n")
    }

    fn html_row(&self, row: &Row, tag: &str) -> String {
        let mut out = String::from("<tr>");
        for (col, &alignment) in self.alignments.iter().enumerate() {
            let html = row.cells.get(col).map_or("", |c| c.html.as_str());
            out.push_str(&format!(
                "<{tag}{}>{html}</{tag}>",
                align_attribute(alignment)
            ));
        }
        out.push_str("</tr>");
        out
    }
}

fn align_attribute(alignment: Alignment) -> &'static str {
    match alignment {
        Alignment::None => "",
        Alignment::Left => " style=\"text-align:left\"",
        Alignment::Center => " style=\"text-align:center\"",
        Alignment::Right => " style=\"text-align:right\"",
    }
}

fn csv_field(text: &str) -> Cow<'_, str> {
    if text.contains([',', '"', '\n', '\r']) {
        Cow::Owned(format!("\"{}\"", text.replace('"', "\"\"")))
    } else {
        Cow::Borrowed(text)
    }
}

// ------------------------------------------------------------------------- building the model

fn build(block: &Block, table: &ast::Table, buffer: &Buffer) -> TableModel {
    let rows: Vec<Row> = iter::once(&table.header)
        .chain(&table.rows)
        .map(|row| build_row(buffer, row))
        .collect();
    let model = CellTable {
        range: block.range,
        alignments: table.alignments.clone(),
        rows,
        delimiter: table.delimiter,
    };
    match raw_reason(&model, table, buffer) {
        Some(reason) => TableModel::Raw {
            table: model,
            reason,
        },
        None => TableModel::Cells(model),
    }
}

fn build_row(buffer: &Buffer, row: &ast::TableRow) -> Row {
    Row {
        range: row.range,
        cells: row
            .cells
            .iter()
            .map(|cell| {
                let mut plain = String::new();
                inline_plain(buffer, &cell.inlines, &mut plain);
                let mut html = String::new();
                inline_html(buffer, &cell.inlines, &mut html);
                Cell {
                    range: cell.range,
                    content: cell.content,
                    text: buffer.slice(cell.content).into_owned(),
                    plain,
                    html,
                }
            })
            .collect(),
    }
}

fn raw_reason(model: &CellTable, table: &ast::Table, buffer: &Buffer) -> Option<RawReason> {
    let columns = model.columns();
    if model.rows.iter().any(|row| row.cells.len() > columns) {
        return Some(RawReason::ExtraCells);
    }
    // The parser splits at unescaped pipes only, so a pipe left in a cell is an escaped one.
    if model
        .rows
        .iter()
        .flat_map(|row| &row.cells)
        .any(|cell| cell.text.contains('|'))
    {
        return Some(RawReason::EscapedPipe);
    }
    iter::once(&table.header)
        .chain(&table.rows)
        .any(|row| code_spans_cross_cells(buffer, row))
        .then_some(RawReason::PipeInCode)
}

/// True when two cells of the row hold an unmatched backtick run of the same length: the code
/// span they would open and close is cut apart by the pipe between them.
fn code_spans_cross_cells(buffer: &Buffer, row: &ast::TableRow) -> bool {
    let mut seen: Vec<(usize, usize)> = Vec::new();
    for (cell_index, cell) in row.cells.iter().enumerate() {
        for node in iter_inlines(&cell.inlines).filter(|n| matches!(n.kind, InlineKind::Text)) {
            let text = buffer.slice(node.range);
            for run in text.split(|c| c != '`').map(str::len).filter(|&n| n > 0) {
                if seen.iter().any(|&(r, c)| r == run && c != cell_index) {
                    return true;
                }
                seen.push((run, cell_index));
            }
        }
    }
    false
}

// ----------------------------------------------------------------- cell text for copy formats

/// Inline tags that wrap their children in HTML.
fn wrapper_tag(kind: &InlineKind) -> Option<&'static str> {
    match kind {
        InlineKind::Emphasis => Some("em"),
        InlineKind::Strong => Some("strong"),
        InlineKind::Underline => Some("u"),
        InlineKind::Strikethrough => Some("del"),
        InlineKind::Highlight { .. } => Some("mark"),
        _ => None,
    }
}

/// `<br>`, `<br/>`, `<br />`.
fn is_line_break_tag(tag: &str) -> bool {
    tag.to_ascii_lowercase().trim_end_matches(['>', '/', ' ']) == "<br"
}

/// The text a reader sees: markers dropped, escapes and entities resolved, `<br>` a newline.
fn inline_plain(buffer: &Buffer, inlines: &[Inline], out: &mut String) {
    for node in inlines {
        match &node.kind {
            InlineKind::Text => out.push_str(&buffer.slice(node.range)),
            InlineKind::Escape | InlineKind::WikiLink(_) => {
                out.push_str(&buffer.slice(node.content));
            }
            InlineKind::Entity { decoded } => out.push_str(decoded),
            InlineKind::Code { text } => out.push_str(text),
            InlineKind::Image(image) => out.push_str(&buffer.slice(image.alt)),
            InlineKind::RawHtml => {
                if is_line_break_tag(&buffer.slice(node.range)) {
                    out.push('\n');
                }
            }
            InlineKind::Link(_) => inline_plain(buffer, &node.children, out),
            kind if wrapper_tag(kind).is_some() => inline_plain(buffer, &node.children, out),
            _ => out.push_str(&buffer.slice(node.range)),
        }
    }
}

fn escape_html(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

fn push_attribute(out: &mut String, name: &str, value: &str) {
    out.push(' ');
    out.push_str(name);
    out.push_str("=\"");
    escape_html(value, out);
    out.push('"');
}

/// Inline HTML of the inline nodes; text is escaped, raw HTML passes through.
fn inline_html(buffer: &Buffer, inlines: &[Inline], out: &mut String) {
    for node in inlines {
        match &node.kind {
            InlineKind::Text => escape_html(&buffer.slice(node.range), out),
            InlineKind::Escape | InlineKind::WikiLink(_) => {
                escape_html(&buffer.slice(node.content), out);
            }
            InlineKind::Entity { decoded } => escape_html(decoded, out),
            InlineKind::Code { text } => {
                out.push_str("<code>");
                escape_html(text, out);
                out.push_str("</code>");
            }
            InlineKind::Link(link) => {
                out.push_str("<a");
                push_attribute(out, "href", &link.dest);
                if let Some(title) = &link.title {
                    push_attribute(out, "title", title);
                }
                out.push('>');
                inline_html(buffer, &node.children, out);
                out.push_str("</a>");
            }
            InlineKind::Image(image) => {
                out.push_str("<img");
                push_attribute(out, "src", &image.dest);
                push_attribute(out, "alt", &buffer.slice(image.alt));
                if let Some(title) = &image.title {
                    push_attribute(out, "title", title);
                }
                out.push('>');
            }
            InlineKind::RawHtml => out.push_str(&buffer.slice(node.range)),
            kind => match wrapper_tag(kind) {
                Some(tag) => {
                    out.push_str(&format!("<{tag}>"));
                    inline_html(buffer, &node.children, out);
                    out.push_str(&format!("</{tag}>"));
                }
                None => escape_html(&buffer.slice(node.range), out),
            },
        }
    }
}

// ----------------------------------------------------------------------------------- rendering

/// Cell texts of a table, every row exactly one entry per column.
struct Grid {
    alignments: Vec<Alignment>,
    /// The header first.
    rows: Vec<Vec<String>>,
}

/// One rendered row and where its cell contents lie inside `text`.
struct RenderedRow {
    text: String,
    cells: Vec<TextRange>,
}

struct Rendered {
    /// The header first.
    rows: Vec<RenderedRow>,
    delimiter: String,
}

fn delimiter_cell(alignment: Alignment, width: usize) -> String {
    let dashes = |n: usize| "-".repeat(n);
    // `width >= MIN_WIDTH`, so the subtractions cannot underflow.
    match alignment {
        Alignment::None => dashes(width),
        Alignment::Left => format!(":{}", dashes(width - 1)),
        Alignment::Right => format!("{}:", dashes(width - 1)),
        Alignment::Center => format!(":{}:", dashes(width - 2)),
    }
}

fn render_row(cells: &[String], widths: &[usize], alignments: &[Alignment]) -> RenderedRow {
    let mut text = String::from("|");
    let mut ranges = Vec::with_capacity(cells.len());
    for ((cell, &width), &alignment) in cells.iter().zip(widths).zip(alignments) {
        let pad = width - cell.width();
        let (left, right) = match alignment {
            Alignment::Right => (pad, 0),
            Alignment::Center => (pad / 2, pad - pad / 2),
            Alignment::None | Alignment::Left => (0, pad),
        };
        text.push(' ');
        text.push_str(&" ".repeat(left));
        let start = text.len();
        text.push_str(cell);
        ranges.push(TextRange::new(start, text.len()));
        text.push_str(&" ".repeat(right));
        text.push_str(" |");
    }
    RenderedRow {
        text,
        cells: ranges,
    }
}

/// Aligned rendering: every column as wide as its widest cell (at least [`MIN_WIDTH`]).
fn render(grid: &Grid) -> Rendered {
    let widths: Vec<usize> = (0..grid.alignments.len())
        .map(|col| {
            grid.rows
                .iter()
                .map(|row| row[col].width())
                .max()
                .unwrap_or(0)
                .max(MIN_WIDTH)
        })
        .collect();
    let mut delimiter = String::from("|");
    for (&alignment, &width) in grid.alignments.iter().zip(&widths) {
        delimiter.push(' ');
        delimiter.push_str(&delimiter_cell(alignment, width));
        delimiter.push_str(" |");
    }
    Rendered {
        rows: grid
            .rows
            .iter()
            .map(|row| render_row(row, &widths, &grid.alignments))
            .collect(),
        delimiter,
    }
}

/// A row with one space of padding on each side of every cell.
fn compact_row(cells: &[String]) -> RenderedRow {
    let mut text = String::from("|");
    let mut ranges = Vec::with_capacity(cells.len());
    for cell in cells {
        text.push(' ');
        let start = text.len();
        text.push_str(cell);
        ranges.push(TextRange::new(start, text.len()));
        text.push_str(" |");
    }
    RenderedRow {
        text,
        cells: ranges,
    }
}

/// An empty row shaped like `reference` (cell widths copied; at least one space per cell) and
/// the caret offset of every cell inside it.
fn blank_row(buffer: &Buffer, reference: &Row, columns: usize) -> (String, Vec<usize>) {
    let mut text = String::from("|");
    let mut carets = Vec::with_capacity(columns);
    for col in 0..columns {
        let width = reference
            .cells
            .get(col)
            .map_or(MIN_WIDTH, |cell| buffer.slice(cell.range).width().max(1));
        carets.push(text.len() + usize::from(width >= 2));
        text.push_str(&" ".repeat(width));
        text.push('|');
    }
    (text, carets)
}

// ----------------------------------------------------------------------------------- commands

/// The grid table touching `offset`; raw tables decline.
fn grid_table(ctx: Ctx<'_>, offset: usize) -> Option<CellTable> {
    match TableModel::from_ast(ctx.ast, ctx.buffer, offset)? {
        TableModel::Cells(table) => Some(table),
        TableModel::Raw { .. } => None,
    }
}

/// The grid table around the caret and the cell the caret is in.
fn locate(ctx: Ctx<'_>) -> Option<(CellTable, CellPos)> {
    let head = ctx.selection.head;
    let table = grid_table(ctx, head)?;
    let pos = table.cell_at(ctx.buffer, head)?;
    Some((table, pos))
}

/// Text between the start of the line holding `offset` and `offset`.
fn line_prefix(ctx: Ctx<'_>, offset: usize) -> String {
    let start = ctx.line_range(ctx.line_of(offset)).start;
    ctx.slice(TextRange::new(start, offset)).into_owned()
}

/// Replaces the header, the delimiter and every body row with their rendering of `grid`
/// (same row count as `table`). With a `caret` — a cell and a byte offset inside its text, past
/// the end meaning the end — the selection lands there; otherwise the selection is mapped
/// through the edits. `None` when nothing changes.
fn rewrite(
    ctx: Ctx<'_>,
    table: &CellTable,
    grid: &Grid,
    caret: Option<(CellPos, usize)>,
) -> Option<Transaction> {
    let rendered = render(grid);
    let mut lines: Vec<(TextRange, &str, Option<usize>)> = Vec::with_capacity(table.rows.len() + 1);
    lines.push((table.rows[0].range, rendered.rows[0].text.as_str(), Some(0)));
    lines.push((table.delimiter, rendered.delimiter.as_str(), None));
    for (index, row) in table.rows.iter().enumerate().skip(1) {
        lines.push((row.range, rendered.rows[index].text.as_str(), Some(index)));
    }
    let mut edits = Vec::new();
    let mut caret_at = None;
    let mut shift = 0isize;
    for (range, text, row) in lines {
        if let Some((pos, within)) = caret
            && row == Some(pos.row)
        {
            // `pos` addresses a cell of the rendered grid.
            let cell = rendered.rows[pos.row].cells[pos.col];
            let start = range.start.saturating_add_signed(shift);
            caret_at = Some(start + cell.start + within.min(cell.len()));
        }
        if ctx.slice(range) != text {
            edits.push(Edit::replace(range, text));
        }
        shift += text.len() as isize - range.len() as isize;
    }
    if edits.is_empty() {
        return None;
    }
    Some(match caret_at {
        Some(offset) => ctx.transaction(edits, Selection::caret(offset)),
        None => ctx.transaction_mapped(edits),
    })
}

/// Moves the caret into cell (`row`, `col`). A cell missing from a short row is created first:
/// the row is rewritten with empty cells up to the column count.
fn goto(ctx: Ctx<'_>, table: &CellTable, row: usize, col: usize) -> Transaction {
    let source = &table.rows[row];
    if let Some(cell) = source.cells.get(col) {
        return ctx.transaction(vec![], Selection::caret(cell.entry_caret()));
    }
    let padded = compact_row(&table.grid_row(source));
    // `col` is below the column count, which is the number of cells of the padded row.
    let caret = source.range.start + padded.cells[col].end;
    ctx.transaction(
        vec![Edit::replace(source.range, padded.text)],
        Selection::caret(caret),
    )
}

/// Appends an empty body row below `row` (below the delimiter for the header), shaped like
/// `row`, and puts the caret into its cell `col`.
fn add_row_after(ctx: Ctx<'_>, table: &CellTable, row: usize, col: usize) -> Transaction {
    let anchor = &table.rows[row];
    let anchor_end = if row == 0 {
        table.delimiter.end
    } else {
        anchor.range.end
    };
    let at = ctx.line_range(ctx.line_of(anchor_end)).end;
    let prefix = continuation_of(&line_prefix(ctx, anchor.range.start));
    let (blank, carets) = blank_row(ctx.buffer, anchor, table.columns());
    let caret = at + 1 + prefix.len() + carets[col];
    let text = format!("\n{prefix}{blank}");
    ctx.transaction(vec![Edit::insert(at, text)], Selection::caret(caret))
}

/// Tab: the next cell, the first cell of the next row after the last cell of a row, a new row
/// after the last cell of the table.
pub fn next_cell(ctx: Ctx<'_>) -> Option<Transaction> {
    let (table, pos) = locate(ctx)?;
    Some(if pos.col + 1 < table.columns() {
        goto(ctx, &table, pos.row, pos.col + 1)
    } else if pos.row + 1 < table.rows.len() {
        goto(ctx, &table, pos.row + 1, 0)
    } else {
        add_row_after(ctx, &table, pos.row, 0)
    })
}

/// Shift+Tab: the previous cell; in the first cell the caret moves to the end of its text.
pub fn prev_cell(ctx: Ctx<'_>) -> Option<Transaction> {
    let (table, pos) = locate(ctx)?;
    Some(if pos.col > 0 {
        goto(ctx, &table, pos.row, pos.col - 1)
    } else if pos.row > 0 {
        goto(ctx, &table, pos.row - 1, table.columns() - 1)
    } else {
        goto(ctx, &table, 0, 0)
    })
}

/// Enter: the cell below in the same column; in the last row a new row is added.
pub fn move_down(ctx: Ctx<'_>) -> Option<Transaction> {
    let (table, pos) = locate(ctx)?;
    Some(if pos.row + 1 < table.rows.len() {
        goto(ctx, &table, pos.row + 1, pos.col)
    } else {
        add_row_after(ctx, &table, pos.row, pos.col)
    })
}

/// Inserts an empty row below the caret row (below the header: the first body row). The caret
/// moves to the same column of the new row.
pub fn insert_row_below(ctx: Ctx<'_>) -> Option<Transaction> {
    let (table, pos) = locate(ctx)?;
    Some(add_row_after(ctx, &table, pos.row, pos.col))
}

/// Inserts an empty row above the caret row; `None` in the header, which nothing may precede.
pub fn insert_row_above(ctx: Ctx<'_>) -> Option<Transaction> {
    let (table, pos) = locate(ctx)?;
    if pos.row == 0 {
        return None;
    }
    let row = &table.rows[pos.row];
    let (blank, carets) = blank_row(ctx.buffer, row, table.columns());
    let prefix = line_prefix(ctx, row.range.start);
    let at = row.range.start;
    let caret = at + carets[pos.col];
    Some(ctx.transaction(
        vec![Edit::insert(at, format!("{blank}\n{prefix}"))],
        Selection::caret(caret),
    ))
}

/// Deletes the caret row (its whole line); `None` in the header. The caret moves to the same
/// column of the row that took its place, or of the row before when it was the last.
pub fn delete_row(ctx: Ctx<'_>) -> Option<Transaction> {
    let (table, pos) = locate(ctx)?;
    if pos.row == 0 {
        return None;
    }
    let line = ctx.line_of(table.rows[pos.row].range.start);
    let removed = if line + 1 == ctx.buffer.line_count() {
        // Last line of the document: it has no terminator, so take the one before it.
        TextRange::new(ctx.line_range(line - 1).end, ctx.line_range(line).end)
    } else {
        ctx.buffer.line_range_with_terminator(line)
    };
    let (target, shift) = if pos.row + 1 < table.rows.len() {
        (pos.row + 1, removed.len())
    } else {
        (pos.row - 1, 0)
    };
    let cells = &table.rows[target].cells;
    // A row always holds at least one cell.
    let cell = &cells[pos.col.min(cells.len() - 1)];
    let caret = cell.entry_caret() - shift;
    Some(ctx.transaction(vec![Edit::delete(removed)], Selection::caret(caret)))
}

fn insert_column(ctx: Ctx<'_>, right: bool) -> Option<Transaction> {
    let (table, pos) = locate(ctx)?;
    let at = pos.col + usize::from(right);
    let mut grid = table.grid();
    grid.alignments.insert(at, Alignment::None);
    for row in &mut grid.rows {
        row.insert(at, String::new());
    }
    // The caret stays in its own cell, which moves one column right when the new one is left.
    let moved = CellPos {
        row: pos.row,
        col: pos.col + usize::from(!right),
    };
    let within = table.within(pos, ctx.selection.head);
    rewrite(ctx, &table, &grid, Some((moved, within)))
}

/// Inserts an empty column left of the caret cell.
pub fn insert_column_left(ctx: Ctx<'_>) -> Option<Transaction> {
    insert_column(ctx, false)
}

/// Inserts an empty column right of the caret cell.
pub fn insert_column_right(ctx: Ctx<'_>) -> Option<Transaction> {
    insert_column(ctx, true)
}

/// Deletes the caret column; `None` for a single-column table. The caret moves to the end of the
/// same row's cell that took its place (the one before for the last column).
pub fn delete_column(ctx: Ctx<'_>) -> Option<Transaction> {
    let (table, pos) = locate(ctx)?;
    if table.columns() < 2 {
        return None;
    }
    let mut grid = table.grid();
    grid.alignments.remove(pos.col);
    for row in &mut grid.rows {
        row.remove(pos.col);
    }
    let moved = CellPos {
        row: pos.row,
        col: pos.col.min(grid.alignments.len() - 1),
    };
    rewrite(ctx, &table, &grid, Some((moved, usize::MAX)))
}

/// Sets the alignment of the caret column; `None` when it already has it.
pub fn set_alignment(ctx: Ctx<'_>, alignment: Alignment) -> Option<Transaction> {
    let (table, pos) = locate(ctx)?;
    if table.alignments[pos.col] == alignment {
        return None;
    }
    let mut grid = table.grid();
    grid.alignments[pos.col] = alignment;
    rewrite(
        ctx,
        &table,
        &grid,
        Some((pos, table.within(pos, ctx.selection.head))),
    )
}

/// Re-aligns the pipes of the table around the caret (the caret keeps its place in its cell).
/// `None` for a raw table, outside tables, and when the table is already aligned.
pub fn format_table(ctx: Ctx<'_>) -> Option<Transaction> {
    let head = ctx.selection.head;
    let table = grid_table(ctx, head)?;
    let caret = table
        .cell_at(ctx.buffer, head)
        .map(|pos| (pos, table.within(pos, head)));
    rewrite(ctx, &table, &table.grid(), caret)
}

/// The "format tables on exit" step: re-aligns the table that held the caret at
/// `previous_head` (an offset of the current text) once the selection has left it. `None` while
/// the selection is still in the table, for raw tables and tables that are already aligned. The
/// caller checks the setting.
pub fn format_after_leaving(ctx: Ctx<'_>, previous_head: usize) -> Option<Transaction> {
    let table = grid_table(ctx, previous_head)?;
    if table.range.touches(ctx.selection.head) {
        return None;
    }
    rewrite(ctx, &table, &table.grid(), None)
}

/// A text form of a table for the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFormat {
    Markdown,
    Csv,
    Html,
}

/// The table around the caret as Markdown, CSV or HTML (raw tables included); `None` outside
/// tables.
pub fn copy_table(ctx: Ctx<'_>, format: CopyFormat) -> Option<String> {
    let model = TableModel::from_ast(ctx.ast, ctx.buffer, ctx.selection.head)?;
    let table = model.table();
    Some(match format {
        CopyFormat::Markdown => table.to_markdown(),
        CopyFormat::Csv => table.to_csv(),
        CopyFormat::Html => table.to_html(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::document::Document;
    use crate::markdown::parse_document;

    fn model(text: &str, offset: usize) -> TableModel {
        let buffer = Buffer::from_text(text);
        let ast = parse_document(text);
        TableModel::from_ast(&ast, &buffer, offset).expect("a table at the offset")
    }

    fn grid_of(text: &str) -> CellTable {
        let model = model(text, 0);
        assert_eq!(model.raw_reason(), None, "expected a grid table: {model:?}");
        model.table().clone()
    }

    #[test]
    fn model_exposes_cells_alignments_and_texts() {
        let text = "| a | b |\n|:--|--:|\n| 1 | **2** |";
        let t = grid_of(text);
        assert_eq!(t.columns(), 2);
        assert_eq!(t.alignments, [Alignment::Left, Alignment::Right]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.range, TextRange::new(0, text.len()));
        assert_eq!(t.delimiter, TextRange::new(10, 19));
        assert_eq!(t.rows[0].range, TextRange::new(0, 9));
        assert_eq!(t.rows[0].cells[0].range, TextRange::new(1, 4));
        assert_eq!(t.rows[0].cells[0].content, TextRange::new(2, 3));
        assert_eq!(t.rows[0].cells[1].text, "b");
        let strong = &t.rows[1].cells[1];
        assert_eq!(strong.text, "**2**");
        assert_eq!(strong.plain, "2");
        assert_eq!(strong.html, "<strong>2</strong>");
        assert_eq!(
            t.cell(CellPos { row: 1, col: 0 }).map(|c| c.text.as_str()),
            Some("1")
        );
        assert!(t.cell(CellPos { row: 1, col: 2 }).is_none());
        assert!(t.cell(CellPos { row: 5, col: 0 }).is_none());
        let wrapper = model(text, 0);
        assert_eq!(wrapper.table().columns(), 2);
        assert_eq!(wrapper.raw_reason(), None);
    }

    #[test]
    fn cells_are_found_by_offset() {
        let text = "| a | b |\n|:--|--:|\n| 1 | **2** |";
        let buffer = Buffer::from_text(text);
        let t = grid_of(text);
        for (offset, expected) in [
            (0, Some((0, 0))),
            (3, Some((0, 0))),
            (4, Some((0, 0))),
            (5, Some((0, 1))),
            (9, Some((0, 1))),
            (12, None),
            (20, Some((1, 0))),
            (28, Some((1, 1))),
            (33, Some((1, 1))),
        ] {
            assert_eq!(
                t.cell_at(&buffer, offset),
                expected.map(|(row, col)| CellPos { row, col }),
                "offset {offset}"
            );
        }
    }

    #[test]
    fn tables_the_grid_cannot_represent_are_raw() {
        for (text, reason) in [
            (
                "| a | b\\|c |\n|---|---|\n| 1 | 2 |",
                RawReason::EscapedPipe,
            ),
            (
                "| `a | b` | c |\n|---|---|---|\n| 1 | 2 | 3 |",
                RawReason::PipeInCode,
            ),
            ("| a |\n|---|\n| 1 | 2 |", RawReason::ExtraCells),
        ] {
            let m = model(text, 0);
            assert_eq!(m.raw_reason(), Some(reason), "{text:?}");
            assert_eq!(
                m,
                TableModel::Raw {
                    table: m.table().clone(),
                    reason
                }
            );
        }
        // Closed code spans and backtick runs of different lengths stay grid tables.
        for text in [
            "| `a` | b |\n|---|---|",
            "| ` | `` |\n|---|---|",
            "| a | b |\n|---|---|",
        ] {
            assert_eq!(model(text, 0).raw_reason(), None, "{text:?}");
        }
        let buffer = Buffer::from_text("para\n\n| a |\n|---|");
        let ast = parse_document("para\n\n| a |\n|---|");
        assert!(TableModel::from_ast(&ast, &buffer, 2).is_none());
    }

    #[test]
    fn markdown_copy_is_aligned() {
        let t = grid_of("| Name | Qty |\n|:--|--:|\n| a | 10 |");
        assert_eq!(
            t.to_markdown(),
            "| Name | Qty |\n| :--- | --: |\n| a    |  10 |"
        );
        let header_only = grid_of("| a |\n|---|");
        assert_eq!(header_only.to_markdown(), "| a   |\n| --- |");
    }

    #[test]
    fn csv_copy_quotes_fields_and_strips_markup() {
        let t = grid_of("| a,b | \"q\" |\n|---|---|\n| **x** | line1<br>line2 |\n| short |");
        assert_eq!(
            t.to_csv(),
            "\"a,b\",\"\"\"q\"\"\"\r\nx,\"line1\nline2\"\r\nshort,"
        );
    }

    #[test]
    fn html_copy_keeps_formatting_and_alignment() {
        let text = "| h1 | h2 | h3 |\n\
                    |:---|:--:|---:|\n\
                    | *e* **s** ~u~ ~~d~~ | ==m== `c<>` | [l](http://x.y \"T\") |\n\
                    | ![alt](a.png \"ti\") | [[Wiki]] $x$ | a\\*b &amp; <br>z |\n\
                    | x<b>y</b> | <https://a.b> |  |";
        let t = grid_of(text);
        assert_eq!(
            t.to_html(),
            "<table>\n<thead>\n\
             <tr><th style=\"text-align:left\">h1</th><th style=\"text-align:center\">h2</th><th style=\"text-align:right\">h3</th></tr>\n\
             </thead>\n<tbody>\n\
             <tr><td style=\"text-align:left\"><em>e</em> <strong>s</strong> <u>u</u> <del>d</del></td><td style=\"text-align:center\"><mark>m</mark> <code>c&lt;&gt;</code></td><td style=\"text-align:right\"><a href=\"http://x.y\" title=\"T\">l</a></td></tr>\n\
             <tr><td style=\"text-align:left\"><img src=\"a.png\" alt=\"alt\" title=\"ti\"></td><td style=\"text-align:center\">Wiki $x$</td><td style=\"text-align:right\">a*b &amp; <br>z</td></tr>\n\
             <tr><td style=\"text-align:left\">x<b>y</b></td><td style=\"text-align:center\"><a href=\"https://a.b\">https://a.b</a></td><td style=\"text-align:right\"></td></tr>\n\
             </tbody>\n</table>"
        );
        let plain: Vec<&str> = t.rows[1..]
            .iter()
            .flat_map(|row| row.cells.iter().map(|c| c.plain.as_str()))
            .collect();
        assert_eq!(
            plain,
            [
                "e s u d",
                "m c<>",
                "l",
                "alt",
                "Wiki $x$",
                "a*b & \nz",
                "xy",
                "https://a.b",
                ""
            ]
        );
        assert_eq!(
            grid_of("| a |\n|---|").to_html(),
            "<table>\n<thead>\n<tr><th>a</th></tr>\n</thead>\n</table>"
        );
        assert_eq!(
            grid_of("| ![alt](a.png) |\n|---|").to_html(),
            "<table>\n<thead>\n<tr><th><img src=\"a.png\" alt=\"alt\"></th></tr>\n</thead>\n</table>",
            "an image without a title has no title attribute"
        );
        assert_eq!(
            grid_of("| <br/> <BR /> |\n|---|").rows[0].cells[0].plain,
            "\n \n"
        );
    }

    #[test]
    fn copy_table_reads_the_table_at_the_caret() {
        let (text, selection) = parse_marked("| a‸ | b |\n|---|---|\n| 1 | 2 |");
        let buffer = Buffer::from_text(&text);
        let ast = parse_document(&text);
        let ctx = Ctx::new(&buffer, &ast, selection);
        assert_eq!(
            copy_table(ctx, CopyFormat::Markdown).as_deref(),
            Some("| a   | b   |\n| --- | --- |\n| 1   | 2   |")
        );
        assert_eq!(
            copy_table(ctx, CopyFormat::Csv).as_deref(),
            Some("a,b\r\n1,2")
        );
        assert!(copy_table(ctx, CopyFormat::Html).is_some_and(|h| h.starts_with("<table>")));
        // Raw tables can still be copied.
        let (raw, selection) = parse_marked("| a‸ | b\\|c |\n|---|---|");
        let buffer = Buffer::from_text(&raw);
        let ast = parse_document(&raw);
        let ctx = Ctx::new(&buffer, &ast, selection);
        assert_eq!(copy_table(ctx, CopyFormat::Csv).as_deref(), Some("a,b|c"));
        let (outside, selection) = parse_marked("text‸");
        let buffer = Buffer::from_text(&outside);
        let ast = parse_document(&outside);
        assert_eq!(
            copy_table(Ctx::new(&buffer, &ast, selection), CopyFormat::Csv),
            None
        );
    }

    #[test]
    fn tab_moves_to_the_next_cell() {
        for (before, after) in [
            (
                "| a‸ | b |\n|---|---|\n| 1 | 2 |",
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
            ),
            // End of a row: the first cell of the next row.
            (
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
                "| a | b |\n|---|---|\n| 1‸ | 2 |",
            ),
            // The caret may sit anywhere in the cell, in front of it or on a pipe.
            (
                "| ‸a | b |\n|---|---|\n| 1 | 2 |",
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
            ),
            (
                "|‸ a | b |\n|---|---|\n| 1 | 2 |",
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
            ),
            (
                "| a |‸ b |\n|---|---|\n| 1 | 2 |",
                "| a | b |\n|---|---|\n| 1‸ | 2 |",
            ),
            (
                "| a | b |\n|---|---|\n| 1‸ | 2 |",
                "| a | b |\n|---|---|\n| 1 | 2‸ |",
            ),
            // An empty cell: the caret lands behind its first padding space.
            ("| a‸ |   |\n|---|---|", "| a | ‸  |\n|---|---|"),
        ] {
            check(before, after, next_cell);
        }
    }

    #[test]
    fn tab_in_the_last_cell_adds_a_row() {
        for (before, after) in [
            (
                "| a | b |\n|---|---|\n| 1 | 2‸ |",
                "| a | b |\n|---|---|\n| 1 | 2 |\n| ‸  |   |",
            ),
            // Header only: the row goes below the delimiter.
            ("| a | b‸ |\n|---|---|", "| a | b |\n|---|---|\n| ‸  |   |"),
            // The container prefix of the table lines continues.
            (
                "> | a |\n> |---|\n> | 1‸ |",
                "> | a |\n> |---|\n> | 1 |\n> | ‸  |",
            ),
            (
                "- | a |\n  |---|\n  | 1‸ |",
                "- | a |\n  |---|\n  | 1 |\n  | ‸  |",
            ),
            // The new row follows the table row, not the end of the document.
            (
                "| a |\n|---|\n| 1‸ |\n\ntext",
                "| a |\n|---|\n| 1 |\n| ‸  |\n\ntext",
            ),
        ] {
            check(before, after, next_cell);
        }
    }

    #[test]
    fn shift_tab_moves_to_the_previous_cell() {
        for (before, after) in [
            (
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
                "| a‸ | b |\n|---|---|\n| 1 | 2 |",
            ),
            (
                "| a | b |\n|---|---|\n| 1‸ | 2 |",
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
            ),
            // The first cell has no previous one: the caret moves to the end of its text.
            (
                "| ‸a | b |\n|---|---|\n| 1 | 2 |",
                "| a‸ | b |\n|---|---|\n| 1 | 2 |",
            ),
        ] {
            check(before, after, prev_cell);
        }
    }

    #[test]
    fn enter_moves_down() {
        for (before, after) in [
            (
                "| a‸ | b |\n|---|---|\n| 1 | 2 |",
                "| a | b |\n|---|---|\n| 1‸ | 2 |",
            ),
            (
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
                "| a | b |\n|---|---|\n| 1 | 2‸ |",
            ),
            // The last row gets a new row below, in the same column.
            (
                "| a | b |\n|---|---|\n| 1 | 2‸ |",
                "| a | b |\n|---|---|\n| 1 | 2 |\n|   | ‸  |",
            ),
            ("| a‸ |\n|---|", "| a |\n|---|\n| ‸  |"),
        ] {
            check(before, after, move_down);
        }
    }

    #[test]
    fn missing_cells_of_short_rows_are_created_when_entered() {
        for (before, after) in [
            (
                "| a | b |\n|---|---|\n| 1‸ |",
                "| a | b |\n|---|---|\n| 1 | ‸ |",
            ),
            (
                "| a | b | c |\n|---|---|---|\n| 1‸ |",
                "| a | b | c |\n|---|---|---|\n| 1 | ‸ |  |",
            ),
        ] {
            check(before, after, next_cell);
        }
        check(
            "| a | b‸ |\n|---|---|\n| 1 |",
            "| a | b |\n|---|---|\n| 1 | ‸ |",
            move_down,
        );
        check(
            "| a | b |\n|---|---|\n| 1‸ |",
            "| a | b |\n|---|---|\n| 1 |\n| ‸  |   |",
            insert_row_below,
        );
    }

    #[test]
    fn insert_row_below_the_caret_row() {
        for (before, after) in [
            (
                "| a‸ | b |\n|---|---|\n| 1 | 2 |",
                "| a | b |\n|---|---|\n| ‸  |   |\n| 1 | 2 |",
            ),
            (
                "| a | b |\n|---|---|\n| 1 | 2‸ |",
                "| a | b |\n|---|---|\n| 1 | 2 |\n|   | ‸  |",
            ),
            (
                "> | a | b |\n> |---|---|\n> | 1‸ | 2 |",
                "> | a | b |\n> |---|---|\n> | 1 | 2 |\n> | ‸  |   |",
            ),
            (
                "| a | b |\n|---|---|\n| 1 | 2 |\n| 3‸ | 4 |\n\nend",
                "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n| ‸  |   |\n\nend",
            ),
            // The new row copies the cell widths of an aligned table.
            (
                "| a   | b   |\n| --- | --- |\n| 1‸   | 2   |",
                "| a   | b   |\n| --- | --- |\n| 1   | 2   |\n| ‸    |     |",
            ),
        ] {
            check(before, after, insert_row_below);
        }
    }

    #[test]
    fn insert_row_above_the_caret_row() {
        for (before, after) in [
            (
                "| a | b |\n|---|---|\n| 1‸ | 2 |",
                "| a | b |\n|---|---|\n| ‸  |   |\n| 1 | 2 |",
            ),
            (
                "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4‸ |",
                "| a | b |\n|---|---|\n| 1 | 2 |\n|   | ‸  |\n| 3 | 4 |",
            ),
            (
                "> | a |\n> |---|\n> | 1‸ |",
                "> | a |\n> |---|\n> | ‸  |\n> | 1 |",
            ),
        ] {
            check(before, after, insert_row_above);
        }
        // Nothing precedes the header.
        check_none("| a‸ | b |\n|---|---|", insert_row_above);
    }

    #[test]
    fn delete_row_removes_the_line() {
        for (before, after) in [
            (
                "| a | b |\n|---|---|\n| 1‸ | 2 |\n| 3 | 4 |",
                "| a | b |\n|---|---|\n| 3‸ | 4 |",
            ),
            (
                "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4‸ |",
                "| a | b |\n|---|---|\n| 1 | 2‸ |",
            ),
            (
                "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4‸ |\n\nend",
                "| a | b |\n|---|---|\n| 1 | 2‸ |\n\nend",
            ),
            ("| a | b |\n|---|---|\n| 1‸ | 2 |", "| a‸ | b |\n|---|---|"),
            (
                "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4‸ |\n| 5 | 6 |",
                "| a | b |\n|---|---|\n| 1 | 2 |\n| 5 | 6‸ |",
            ),
            (
                "> | a |\n> |---|\n> | 1‸ |\n> | 2 |",
                "> | a |\n> |---|\n> | 2‸ |",
            ),
            // A short row next in line: the caret goes to its last cell.
            (
                "| a | b |\n|---|---|\n| 1 | 2‸ |\n| 3 |",
                "| a | b |\n|---|---|\n| 3‸ |",
            ),
        ] {
            check(before, after, delete_row);
        }
        // The header cannot be deleted.
        check_none("| a‸ | b |\n|---|---|\n| 1 | 2 |", delete_row);
    }

    #[test]
    fn insert_columns_rerender_the_table() {
        for (before, after, left) in [
            (
                "| a‸ | b |\n|---|---|\n| 1 | 2 |",
                "| a‸   |     | b   |\n| --- | --- | --- |\n| 1   |     | 2   |",
                false,
            ),
            (
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
                "| a   |     | b‸   |\n| --- | --- | --- |\n| 1   |     | 2   |",
                true,
            ),
            (
                "| a‸ | b |\n|---|---|\n| 1 | 2 |",
                "|     | a‸   | b   |\n| --- | --- | --- |\n|     | 1   | 2   |",
                true,
            ),
            (
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
                "| a   | b‸   |     |\n| --- | --- | --- |\n| 1   | 2   |     |",
                false,
            ),
            (
                "> | a | b |\n> |---|---|\n> | 1‸ | 2 |",
                "> | a   |     | b   |\n> | --- | --- | --- |\n> | 1‸   |     | 2   |",
                false,
            ),
        ] {
            if left {
                check(before, after, insert_column_left);
            } else {
                check(before, after, insert_column_right);
            }
        }
    }

    #[test]
    fn delete_column_removes_it_from_every_row() {
        for (before, after) in [
            (
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
                "| a‸   |\n| --- |\n| 1   |",
            ),
            (
                "| a‸ | b |\n|---|---|\n| 1 | 2 |",
                "| b‸   |\n| --- |\n| 2   |",
            ),
            (
                "> | a‸ | b |\n> |---|---|\n> | 1 | 2 |",
                "> | b‸   |\n> | --- |\n> | 2   |",
            ),
            (
                "| a | b | c |\n|---|:-:|---|\n| 1 | 2‸ | 3 |",
                "| a   | c   |\n| --- | --- |\n| 1   | 3‸   |",
            ),
        ] {
            check(before, after, delete_column);
        }
        check_none("| a‸ |\n|---|\n| 1 |", delete_column);
    }

    #[test]
    fn alignment_is_set_per_column() {
        for (before, alignment, after) in [
            (
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
                Alignment::Center,
                "| a   |  b‸  |\n| --- | :-: |\n| 1   |  2  |",
            ),
            (
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
                Alignment::Right,
                "| a   |   b‸ |\n| --- | --: |\n| 1   |   2 |",
            ),
            (
                "| a | b‸ |\n|---|---|\n| 1 | 2 |",
                Alignment::Left,
                "| a   | b‸   |\n| --- | :-- |\n| 1   | 2   |",
            ),
            (
                "| a | b‸ |\n|:--|:-:|\n| 1 | 2 |",
                Alignment::None,
                "| a   | b‸   |\n| :-- | --- |\n| 1   | 2   |",
            ),
        ] {
            check(before, after, |c| set_alignment(c, alignment));
        }
        check_none("| a | b‸ |\n|---|--:|\n| 1 | 2 |", |c| {
            set_alignment(c, Alignment::Right)
        });
    }

    #[test]
    fn format_aligns_pipes_by_display_width() {
        for (before, after) in [
            (
                "|a|bb|\n|-|-|\n|cc‸c|d|",
                "| a   | bb  |\n| --- | --- |\n| cc‸c | d   |",
            ),
            // CJK characters count two columns.
            (
                "| 日本 | b |\n|---|---|\n| 1‸ | 2 |",
                "| 日本 | b   |\n| ---- | --- |\n| 1‸    | 2   |",
            ),
            // So do emoji; Turkish letters count one.
            (
                "| 👍 | ışık |\n|---|---|\n| İstanbul‸ | x |",
                "| 👍       | ışık |\n| -------- | ---- |\n| İstanbul‸ | x    |",
            ),
            // Right-aligned columns pad on the left.
            (
                "| Şehir | Nüfus |\n|---|---:|\n| İstanbul | 15‸ |",
                "| Şehir    | Nüfus |\n| -------- | ----: |\n| İstanbul |    15‸ |",
            ),
            // Tables without outer pipes gain them.
            (
                "a ‸| b\n--|--\n1 | 2",
                "| a‸   | b   |\n| --- | --- |\n| 1   | 2   |",
            ),
            // The delimiter line has no cell: the caret is mapped through the edits.
            (
                "| a | b |\n|--‸-|---|\n| 1 | 2 |",
                "| a   | b   |\n| --- | --- |‸\n| 1   | 2   |",
            ),
            // Container prefixes stay.
            (
                "> | a‸ | b |\n> |---|---|\n> | 1 | 2 |",
                "> | a‸   | b   |\n> | --- | --- |\n> | 1   | 2   |",
            ),
        ] {
            check(before, after, format_table);
        }
        // Already aligned, outside tables and raw tables: nothing to do.
        check_none("| a‸   | b   |\n| --- | --- |\n| 1   | 2   |", format_table);
        check_none("text‸", format_table);
        check_none("| a‸ | b\\|c |\n|---|---|", format_table);
    }

    #[test]
    fn leaving_a_table_formats_it() {
        let table = "| a | b |\n|---|---|\n| 1 | 2 |";
        check(
            &format!("{table}\n\nafter‸"),
            "| a   | b   |\n| --- | --- |\n| 1   | 2   |\n\nafter‸",
            |c| format_after_leaving(c, 3),
        );
        // Still inside: the table is left alone.
        check_none("| a | b‸ |\n|---|---|\n| 1 | 2 |", |c| {
            format_after_leaving(c, 3)
        });
        // The caret was not in a table, the table is aligned, or it is raw.
        check_none(&format!("{table}\n\nafter‸"), |c| {
            format_after_leaving(c, 40)
        });
        check_none(
            "| a   | b   |\n| --- | --- |\n| 1   | 2   |\n\nafter‸",
            |c| format_after_leaving(c, 3),
        );
        check_none("| a | b\\|c |\n|---|---|\n\nafter‸", |c| {
            format_after_leaving(c, 3)
        });
    }

    #[test]
    fn commands_decline_outside_grid_tables() {
        type Op = fn(Ctx<'_>) -> Option<Transaction>;
        let ops: [Op; 9] = [
            next_cell,
            prev_cell,
            move_down,
            insert_row_below,
            insert_row_above,
            delete_row,
            insert_column_left,
            insert_column_right,
            delete_column,
        ];
        for before in [
            "plain‸",
            // On the delimiter line.
            "| a | b |\n|--‸-|---|\n| 1 | 2 |",
            // A raw table.
            "| a‸ | b\\|c |\n|---|---|\n| 1 | 2 |",
            "| a‸ |\n|---|\n| 1 | 2 |",
        ] {
            for op in ops {
                check_none(before, op);
            }
            check_none(before, |c| set_alignment(c, Alignment::Center));
        }
    }

    #[test]
    fn table_commands_are_single_undo_steps() {
        let text = "| a | b |\n|---|---|\n| 1 | 2 |";
        let mut doc = Document::new(text);
        doc.set_selections(Selection::caret(text.len() - 2), &[]);
        let tx = next_cell(doc.ctx()).expect("caret in a cell");
        doc.apply(tx).expect("apply");
        assert_eq!(doc.text(), format!("{text}\n|   |   |"));
        assert_eq!(doc.primary_selection(), Selection::caret(text.len() + 3));
        assert!(doc.undo().expect("undo"));
        assert_eq!(doc.text(), text);
        assert_eq!(doc.primary_selection(), Selection::caret(text.len() - 2));
        assert!(!doc.can_undo());

        doc.set_selections(Selection::caret(3), &[]);
        let tx = insert_column_right(doc.ctx()).expect("caret in a cell");
        doc.apply(tx).expect("apply");
        assert_eq!(
            doc.text(),
            "| a   |     | b   |\n| --- | --- | --- |\n| 1   |     | 2   |"
        );
        assert_eq!(doc.primary_selection(), Selection::caret(3));
        assert!(doc.undo().expect("undo"));
        assert_eq!(doc.text(), text);
        assert!(!doc.can_undo());
        // The tree of the re-rendered table keeps its grid shape.
        doc.set_selections(Selection::caret(3), &[]);
        let tx = format_table(doc.ctx()).expect("not yet aligned");
        doc.apply(tx).expect("apply");
        assert_eq!(doc.ast().blocks.len(), 1, "the table is the only block");
        let rendered = TableModel::from_ast(doc.ast(), doc.buffer(), 3).expect("a table at 3");
        assert_eq!(rendered.raw_reason(), None, "expected a grid table");
        assert_eq!(rendered.table().columns(), 2);
    }
}
