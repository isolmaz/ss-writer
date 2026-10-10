//! Table commands over random tables (CJK, emoji and Turkish text, random alignments and
//! padding): formatting aligns every line by display width, keeps every cell and is idempotent,
//! its result equals `to_markdown`, Tab walks the cells in reading order before it appends a row,
//! and a column added and removed again leaves the formatted table. Every command is one undo
//! step.

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use ssw_core::document::Document;
use ssw_core::edit::Selection;
use ssw_core::markdown::ast::Alignment;
use ssw_core::ops::table::{
    CellPos, TableModel, delete_column, format_table, insert_column_left, next_cell, prev_cell,
};
use unicode_width::UnicodeWidthStr;

const CELLS: &[&str] = &[
    "",
    "a",
    "bb",
    "ccc",
    "İstanbul",
    "ışık",
    "日本語",
    "👍",
    "a b",
    "x y z",
    "12345678",
];

/// A random table: `rows[0]` is the header; every row has `columns` cells.
#[derive(Clone, Debug)]
struct Shape {
    columns: usize,
    alignments: Vec<Alignment>,
    rows: Vec<Vec<&'static str>>,
    /// Spaces on each side of every cell in the source.
    pad: usize,
}

fn config(cases: u32) -> ProptestConfig {
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x5441_424C),
        failure_persistence: None,
        ..Config::default()
    }
}

fn tables() -> impl Strategy<Value = Shape> {
    (
        1usize..=4,
        prop::collection::vec(0u8..4, 4),
        prop::collection::vec(prop::collection::vec(prop::sample::select(CELLS), 4), 1..5),
        1usize..=3,
    )
        .prop_map(|(columns, alignments, rows, pad)| Shape {
            columns,
            alignments: alignments
                .into_iter()
                .take(columns)
                .map(|a| match a {
                    0 => Alignment::None,
                    1 => Alignment::Left,
                    2 => Alignment::Center,
                    _ => Alignment::Right,
                })
                .collect(),
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().take(columns).collect())
                .collect(),
            pad,
        })
}

fn row_text(cells: &[&str], pad: usize) -> String {
    let space = " ".repeat(pad);
    let mut line = String::from("|");
    for cell in cells {
        line.push_str(&space);
        line.push_str(cell);
        line.push_str(&space);
        line.push('|');
    }
    line
}

fn delimiter_text(alignments: &[Alignment], pad: usize) -> String {
    let space = " ".repeat(pad);
    let mut line = String::from("|");
    for alignment in alignments {
        line.push_str(&space);
        line.push_str(match alignment {
            Alignment::None => "---",
            Alignment::Left => ":--",
            Alignment::Center => ":-:",
            Alignment::Right => "--:",
        });
        line.push_str(&space);
        line.push('|');
    }
    line
}

fn source(shape: &Shape) -> String {
    let mut lines = vec![
        row_text(&shape.rows[0], shape.pad),
        delimiter_text(&shape.alignments, shape.pad),
    ];
    lines.extend(shape.rows[1..].iter().map(|row| row_text(row, shape.pad)));
    lines.join("\n")
}

/// A document with the caret inside the first header cell.
fn document(shape: &Shape) -> Document {
    let mut doc = Document::new(&source(shape));
    doc.set_selections(Selection::caret(1 + shape.pad), &[]);
    doc
}

fn cell_pos(doc: &Document) -> Option<CellPos> {
    let head = doc.primary_selection().head;
    match TableModel::from_ast(doc.ast(), doc.buffer(), head)? {
        TableModel::Cells(table) => table.cell_at(doc.buffer(), head),
        TableModel::Raw { .. } => None,
    }
}

proptest! {
    #![proptest_config(config(300))]

    #[test]
    fn formatting_aligns_by_display_width_and_keeps_every_cell(shape in tables()) {
        let src = source(&shape);
        let mut doc = document(&shape);
        if let Some(tx) = format_table(doc.ctx()) {
            doc.apply(tx).expect("apply");
        }
        let text = doc.text();
        let widths: Vec<usize> = text.lines().map(|line| line.width()).collect();
        prop_assert_eq!(widths.len(), shape.rows.len() + 1);
        prop_assert!(widths.iter().all(|&w| w == widths[0]), "ragged widths in {:?}", text);

        let model = TableModel::from_ast(doc.ast(), doc.buffer(), 0);
        let Some(TableModel::Cells(table)) = model else {
            return Err(TestCaseError::fail(format!("not a grid table: {text:?}")));
        };
        prop_assert_eq!(table.columns(), shape.columns);
        prop_assert_eq!(&table.alignments, &shape.alignments);
        for (row, expected) in table.rows.iter().zip(&shape.rows) {
            let texts: Vec<&str> = row.cells.iter().map(|c| c.text.as_str()).collect();
            prop_assert_eq!(&texts, expected);
        }
        prop_assert_eq!(table.to_markdown(), text.clone());

        // Aligned tables stay as they are.
        doc.set_selections(Selection::caret(2), &[]);
        prop_assert!(format_table(doc.ctx()).is_none());

        // The format step is undone in one go.
        if doc.can_undo() {
            prop_assert!(doc.undo().expect("undo"));
            prop_assert_eq!(doc.text(), src);
            prop_assert!(!doc.can_undo());
        }
    }

    #[test]
    fn tab_visits_the_cells_in_reading_order_then_adds_a_row(shape in tables()) {
        let src = source(&shape);
        let mut doc = document(&shape);
        let total = shape.rows.len() * shape.columns;
        for step in 1..total {
            let tx = next_cell(doc.ctx()).expect("the caret is in a cell");
            prop_assert!(tx.edits.is_empty());
            doc.apply(tx).expect("apply");
            prop_assert_eq!(
                cell_pos(&doc),
                Some(CellPos { row: step / shape.columns, col: step % shape.columns })
            );
        }
        let lines = doc.text().lines().count();
        let tx = next_cell(doc.ctx()).expect("the caret is in a cell");
        doc.apply(tx).expect("apply");
        prop_assert_eq!(doc.text().lines().count(), lines + 1);
        prop_assert_eq!(cell_pos(&doc), Some(CellPos { row: shape.rows.len(), col: 0 }));
        prop_assert!(doc.undo().expect("undo"));
        prop_assert_eq!(doc.text(), src);
    }

    #[test]
    fn an_added_column_removed_again_leaves_the_formatted_table(shape in tables()) {
        let mut doc = document(&shape);
        let formatted = match TableModel::from_ast(doc.ast(), doc.buffer(), 0) {
            Some(TableModel::Cells(table)) => table.to_markdown(),
            other => return Err(TestCaseError::fail(format!("not a grid table: {other:?}"))),
        };
        let tx = insert_column_left(doc.ctx()).expect("the caret is in a cell");
        doc.apply(tx).expect("apply");
        // The caret kept its cell, now the second one; step back into the new column.
        let tx = prev_cell(doc.ctx()).expect("the caret is in a cell");
        doc.apply(tx).expect("apply");
        let tx = delete_column(doc.ctx()).expect("a table with two columns");
        doc.apply(tx).expect("apply");
        prop_assert_eq!(doc.text(), formatted);
    }
}
