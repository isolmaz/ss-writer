//! Table assembly: paragraphs are collected into cells, cells into rows, rows into tables.
//! Nested tables are flattened into the parent cell; merged cells are padded with empty cells.

use super::md::{Align, join_cell, majority_alignments};
use super::props::CellDef;

/// Deepest table nesting that is tracked (Word allows 64; deeper values are clamped).
pub(super) const MAX_DEPTH: usize = 15;

/// Counts of what could not be represented, accumulated while tables are built.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct TableLoss {
    pub merged_cells: usize,
    pub multi_paragraph_cells: usize,
    pub nested_tables: usize,
}

#[derive(Default)]
struct TableState {
    rows: Vec<Vec<String>>,
    /// The alignment of every cell of `rows` (parallel to it).
    aligns: Vec<Vec<Align>>,
    cur_row: Vec<String>,
    cur_aligns: Vec<Align>,
    cur_cell: Vec<String>,
    /// The alignment of the first paragraph with text in the current cell.
    cur_align: Option<Align>,
}

impl TableState {
    fn flush_cell(&mut self) {
        let cell = std::mem::take(&mut self.cur_cell);
        self.cur_row.push(join_cell(cell));
        self.cur_aligns
            .push(self.cur_align.take().unwrap_or_default());
    }

    /// Moves the pending row (padded to `width` cells at least) into `rows`.
    fn finish_row(&mut self, width: usize) {
        let mut row = std::mem::take(&mut self.cur_row);
        if row.len() < width {
            row.resize(width, String::new());
        }
        let mut aligns = std::mem::take(&mut self.cur_aligns);
        aligns.resize(row.len(), Align::None);
        self.rows.push(row);
        self.aligns.push(aligns);
    }
}

/// The stack of open tables (index 0 is the outermost).
#[derive(Default)]
pub(super) struct Tables {
    stack: Vec<TableState>,
    /// Column alignments of the outermost table that `close_to` returned last.
    finished_align: Vec<Align>,
}

impl Tables {
    /// Makes sure `depth` tables are open (clamped to [`MAX_DEPTH`]).
    pub fn open_to(&mut self, depth: usize) {
        while self.stack.len() < depth.min(MAX_DEPTH) {
            self.stack.push(TableState::default());
        }
    }

    /// Adds one paragraph of text to the current cell of the table at `depth`.
    pub fn push_paragraph(&mut self, depth: usize, text: String) {
        if let Some(t) = self.at(depth) {
            t.cur_cell.push(text);
        }
    }

    /// Records the alignment of a paragraph with text in the current cell of the table at
    /// `depth`; the first one of a cell decides the cell's alignment.
    pub fn align_cell(&mut self, depth: usize, align: Align) {
        if let Some(t) = self.at(depth) {
            t.cur_align.get_or_insert(align);
        }
    }

    /// The column alignments of the table that the last successful `close_to` returned.
    pub fn take_alignments(&mut self) -> Vec<Align> {
        std::mem::take(&mut self.finished_align)
    }

    /// Ends the current cell of the table at `depth`.
    pub fn end_cell(&mut self, depth: usize, loss: &mut TableLoss) {
        if let Some(t) = self.at(depth) {
            if t.cur_cell.len() > 1 {
                loss.multi_paragraph_cells += 1;
            }
            t.flush_cell();
        }
    }

    /// Ends the current row of the table at `depth`; `defs` are the cell definitions of the row.
    pub fn end_row(&mut self, depth: usize, defs: Option<&[CellDef]>, loss: &mut TableLoss) {
        let Some(t) = self.at(depth) else { return };
        if !t.cur_cell.is_empty() {
            t.flush_cell();
        }
        let mut width = 0;
        if let Some(defs) = defs {
            loss.merged_cells += defs
                .iter()
                .filter(|c| c.h_merged || (c.v_merge && !c.v_restart))
                .count();
            // Word keeps one cell mark per defined cell; merged cells leave gaps whose position
            // is lost, so pad the row up to the defined width.
            width = defs.len();
        }
        t.finish_row(width);
    }

    /// Closes the tables deeper than `keep` (0 closes all). Nested tables are flattened into the
    /// cell of their parent; the outermost table, when it closes, is returned.
    pub fn close_to(&mut self, keep: usize, loss: &mut TableLoss) -> Option<Vec<Vec<String>>> {
        let mut finished = None;
        while self.stack.len() > keep {
            let mut t = self.stack.pop().unwrap_or_default();
            if !t.cur_cell.is_empty() {
                t.flush_cell();
            }
            if !t.cur_row.is_empty() {
                t.finish_row(0);
            }
            if t.rows.is_empty() {
                continue;
            }
            match self.stack.last_mut() {
                Some(parent) => {
                    loss.nested_tables += 1;
                    for r in &t.rows {
                        let line = r
                            .iter()
                            .map(|c| c.replace("<br>", " / "))
                            .collect::<Vec<_>>()
                            .join(" \u{00A6} ");
                        parent.cur_cell.push(line);
                    }
                }
                None => {
                    self.finished_align = majority_alignments(&t.rows, &t.aligns);
                    finished = Some(t.rows);
                }
            }
        }
        finished
    }

    fn at(&mut self, depth: usize) -> Option<&mut TableState> {
        self.stack.get_mut(depth.checked_sub(1)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(h: bool, v: bool, restart: bool) -> CellDef {
        CellDef {
            h_merged: h,
            v_merge: v,
            v_restart: restart,
        }
    }

    #[test]
    fn builds_rows_and_cells() {
        let mut t = Tables::default();
        let mut loss = TableLoss::default();
        t.open_to(1);
        t.push_paragraph(1, "a".into());
        t.end_cell(1, &mut loss);
        t.push_paragraph(1, "b1".into());
        t.push_paragraph(1, "b2".into());
        t.end_cell(1, &mut loss);
        t.end_row(1, None, &mut loss);
        t.push_paragraph(1, "c".into());
        t.end_row(1, None, &mut loss); // a pending cell is closed by the row end
        let rows = t.close_to(0, &mut loss).unwrap();
        assert_eq!(
            rows,
            vec![
                vec!["a".to_string(), "b1<br>b2".to_string()],
                vec!["c".to_string()]
            ]
        );
        assert_eq!(
            loss,
            TableLoss {
                merged_cells: 0,
                multi_paragraph_cells: 1,
                nested_tables: 0
            }
        );
        assert!(t.close_to(0, &mut loss).is_none());
    }

    #[test]
    fn cells_carry_the_alignment_of_their_first_paragraph() {
        let mut t = Tables::default();
        let mut loss = TableLoss::default();
        t.open_to(1);
        // row 1: centered, then a cell whose first paragraph decides (right), then a pending cell
        t.push_paragraph(1, "a".into());
        t.align_cell(1, Align::Center);
        t.end_cell(1, &mut loss);
        t.push_paragraph(1, "b1".into());
        t.align_cell(1, Align::Right);
        t.push_paragraph(1, "b2".into());
        t.align_cell(1, Align::Left);
        t.end_cell(1, &mut loss);
        t.end_row(1, None, &mut loss);
        // row 2: centered again; its second cell has no alignment; a pending cell closes the table
        t.push_paragraph(1, "c".into());
        t.align_cell(1, Align::Center);
        t.end_cell(1, &mut loss);
        t.push_paragraph(1, "d".into());
        t.end_cell(1, &mut loss);
        t.end_row(1, None, &mut loss);
        t.push_paragraph(1, "e".into());
        t.align_cell(1, Align::Center);
        // alignment outside an open table is ignored
        t.align_cell(0, Align::Right);
        assert!(t.close_to(0, &mut loss).is_some());
        assert_eq!(t.take_alignments(), [Align::Center, Align::None]);
        assert!(t.take_alignments().is_empty(), "taken once");
    }

    #[test]
    fn merged_cells_pad_the_row() {
        let mut t = Tables::default();
        let mut loss = TableLoss::default();
        t.open_to(1);
        t.push_paragraph(1, "x".into());
        t.end_cell(1, &mut loss);
        let defs = [
            cell(false, false, false),
            cell(true, false, false),
            cell(false, true, false),
            cell(false, true, true),
        ];
        t.end_row(1, Some(&defs), &mut loss);
        assert_eq!(loss.merged_cells, 2);
        let rows = t.close_to(0, &mut loss).unwrap();
        assert_eq!(
            rows,
            vec![vec![
                "x".to_string(),
                String::new(),
                String::new(),
                String::new()
            ]]
        );
    }

    #[test]
    fn nested_tables_flatten_into_the_parent_cell() {
        let mut t = Tables::default();
        let mut loss = TableLoss::default();
        t.open_to(1);
        t.push_paragraph(1, "outer".into());
        t.open_to(2);
        t.push_paragraph(2, "i1".into());
        t.push_paragraph(2, "i2".into());
        t.end_cell(2, &mut loss);
        t.push_paragraph(2, "j".into());
        t.end_cell(2, &mut loss);
        t.end_row(2, None, &mut loss);
        assert!(t.close_to(1, &mut loss).is_none());
        t.end_cell(1, &mut loss);
        t.end_row(1, None, &mut loss);
        let rows = t.close_to(0, &mut loss).unwrap();
        assert_eq!(rows, vec![vec!["outer<br>i1 / i2 \u{a6} j".to_string()]]);
        assert_eq!((loss.nested_tables, loss.multi_paragraph_cells), (1, 2));
    }

    #[test]
    fn empty_tables_vanish_and_depth_is_clamped() {
        let mut t = Tables::default();
        let mut loss = TableLoss::default();
        t.open_to(2);
        assert!(t.close_to(0, &mut loss).is_none());
        assert_eq!(loss, TableLoss::default());
        t.open_to(1_000_000);
        assert_eq!(t.stack.len(), MAX_DEPTH);
        // operations outside any open table are ignored
        t.push_paragraph(0, "x".into());
        t.push_paragraph(MAX_DEPTH + 1, "x".into());
        t.end_cell(0, &mut loss);
        t.end_row(MAX_DEPTH + 1, None, &mut loss);
        assert!(t.close_to(0, &mut loss).is_none());
        // a table with only an unfinished cell and one with only an unfinished row
        t.open_to(1);
        t.push_paragraph(1, "pending".into());
        assert_eq!(
            t.close_to(0, &mut loss),
            Some(vec![vec!["pending".to_string()]])
        );
        t.open_to(1);
        t.push_paragraph(1, "a".into());
        t.end_cell(1, &mut loss);
        assert_eq!(t.close_to(0, &mut loss), Some(vec![vec!["a".to_string()]]));
    }
}
