//! Edits, selections and transactions.
//!
//! A [`Transaction`] is the unit of undo: every user command (typing run, format, replace-all,
//! table operation, paste) produces exactly one. All edit ranges of a transaction refer to
//! the document **before** the transaction and must be sorted and disjoint.

use thiserror::Error;

use crate::text::{Buffer, TextError, TextRange};

/// Which side a position sticks to when text is inserted exactly at it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assoc {
    /// Stay before inserted text.
    Before,
    /// Move after inserted text.
    After,
}

/// Replace `range` with `text`. An empty range inserts; an empty `text` deletes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub range: TextRange,
    pub text: String,
}

impl Edit {
    pub fn replace(range: TextRange, text: impl Into<String>) -> Self {
        Self {
            range,
            text: text.into(),
        }
    }

    pub fn insert(offset: usize, text: impl Into<String>) -> Self {
        Self::replace(TextRange::at(offset), text)
    }

    pub fn delete(range: TextRange) -> Self {
        Self::replace(range, String::new())
    }

    /// Change of document length caused by this edit.
    pub fn delta(&self) -> isize {
        self.text.len() as isize - self.range.len() as isize
    }

    /// Range the inserted text occupies after the edit is applied alone.
    pub fn new_range(&self) -> TextRange {
        TextRange::new(self.range.start, self.range.start + self.text.len())
    }

    /// Maps an offset of the old document to the new document.
    ///
    /// Offsets before the edit stay, offsets after it shift. An offset strictly inside a
    /// replaced range collapses to the start (`Assoc::Before`) or end (`Assoc::After`) of the
    /// new text; an insertion exactly at `offset` is placed by `assoc`.
    pub fn map_offset(&self, offset: usize, assoc: Assoc) -> usize {
        let TextRange { start, end } = self.range;
        let new_end = start + self.text.len();
        if offset < start {
            offset
        } else if offset > end {
            offset - end + new_end
        } else if start == end {
            match assoc {
                Assoc::Before => start,
                Assoc::After => new_end,
            }
        } else if offset == start {
            start
        } else if offset == end {
            new_end
        } else {
            match assoc {
                Assoc::Before => start,
                Assoc::After => new_end,
            }
        }
    }
}

/// A selection: `anchor` is where it started, `head` is the caret.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub const fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    /// Collapsed selection (a caret).
    pub const fn caret(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
        }
    }

    /// Selection covering `range`, caret at its end.
    pub const fn from_range(range: TextRange) -> Self {
        Self {
            anchor: range.start,
            head: range.end,
        }
    }

    pub const fn is_caret(self) -> bool {
        self.anchor == self.head
    }

    /// True when the caret is before the anchor.
    pub const fn is_reversed(self) -> bool {
        self.head < self.anchor
    }

    pub const fn start(self) -> usize {
        if self.anchor < self.head {
            self.anchor
        } else {
            self.head
        }
    }

    pub const fn end(self) -> usize {
        if self.anchor < self.head {
            self.head
        } else {
            self.anchor
        }
    }

    /// Ordered range covered by the selection.
    pub const fn range(self) -> TextRange {
        TextRange {
            start: self.start(),
            end: self.end(),
        }
    }

    /// Moves the head, keeping the anchor (extend selection).
    pub const fn with_head(self, head: usize) -> Self {
        Self {
            anchor: self.anchor,
            head,
        }
    }

    /// Collapses to the caret position.
    pub const fn collapsed(self) -> Self {
        Self::caret(self.head)
    }

    /// Selection after `tx` is applied. The caret sticks after text inserted at it; the
    /// anchor of a non-empty selection sticks to its side so the selection does not swallow
    /// text typed at its edges.
    pub fn map_through(self, tx: &Transaction) -> Self {
        if self.is_caret() {
            return Self::caret(tx.map_offset(self.head, Assoc::After));
        }
        let (start_assoc, end_assoc) = (Assoc::After, Assoc::Before);
        let (a_assoc, h_assoc) = if self.is_reversed() {
            (end_assoc, start_assoc)
        } else {
            (start_assoc, end_assoc)
        };
        Self {
            anchor: tx.map_offset(self.anchor, a_assoc),
            head: tx.map_offset(self.head, h_assoc),
        }
    }

    /// Clamps both ends to `len`.
    pub fn clamped(self, len: usize) -> Self {
        Self {
            anchor: self.anchor.min(len),
            head: self.head.min(len),
        }
    }
}

/// A transaction cannot be applied.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EditError {
    #[error(transparent)]
    Text(#[from] TextError),
    #[error("edit {index} overlaps or precedes the previous edit")]
    Unordered { index: usize },
}

/// One undoable step: edits plus the selections to restore on undo / apply on redo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    /// Sorted, disjoint, in coordinates of the document before the transaction.
    pub edits: Vec<Edit>,
    pub selection_before: Selection,
    pub selection_after: Selection,
}

impl Transaction {
    pub fn new(edits: Vec<Edit>, selection_before: Selection, selection_after: Selection) -> Self {
        Self {
            edits,
            selection_before,
            selection_after,
        }
    }

    pub fn single(edit: Edit, selection_before: Selection, selection_after: Selection) -> Self {
        Self::new(vec![edit], selection_before, selection_after)
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// Total change of document length.
    pub fn delta(&self) -> isize {
        self.edits.iter().map(Edit::delta).sum()
    }

    /// Maps an offset of the old document through every edit.
    pub fn map_offset(&self, offset: usize, assoc: Assoc) -> usize {
        // Edits are disjoint, so each contributes an independent displacement.
        let mut result = offset as isize;
        for edit in &self.edits {
            result += edit.map_offset(offset, assoc) as isize - offset as isize;
        }
        result.max(0) as usize
    }

    /// Maps a range of the old document through every edit.
    pub fn map_range(&self, range: TextRange) -> TextRange {
        let start = self.map_offset(range.start, Assoc::After);
        let end = self.map_offset(range.end, Assoc::Before);
        TextRange::new(start, end.max(start))
    }

    /// Checks ordering and bounds without touching a buffer.
    pub fn validate(&self, buffer: &Buffer) -> Result<(), EditError> {
        let mut prev_end = 0usize;
        for (index, edit) in self.edits.iter().enumerate() {
            if index > 0 && edit.range.start < prev_end {
                return Err(EditError::Unordered { index });
            }
            buffer.check_range(edit.range)?;
            prev_end = edit.range.end;
        }
        Ok(())
    }

    /// Applies the edits to `buffer` and returns the inverse transaction (apply it to the new
    /// buffer to undo). The buffer is untouched when the transaction is invalid.
    pub fn apply(&self, buffer: &mut Buffer) -> Result<Transaction, EditError> {
        self.validate(buffer)?;
        let mut inverse = Vec::with_capacity(self.edits.len());
        let mut shift = 0isize;
        for edit in &self.edits {
            let removed = buffer.slice(edit.range).into_owned();
            let start = edit.range.start.saturating_add_signed(shift);
            inverse.push(Edit {
                range: TextRange::new(start, start + edit.text.len()),
                text: removed,
            });
            shift += edit.delta();
        }
        // Edits are sorted and disjoint, so applying back to front keeps every remaining range
        // valid; `validate` already proved each one is in bounds on char boundaries.
        for edit in self.edits.iter().rev() {
            buffer
                .replace(edit.range, &edit.text)
                .expect("edit range validated against this buffer");
        }
        Ok(Transaction {
            edits: inverse,
            selection_before: self.selection_after,
            selection_after: self.selection_before,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: usize, end: usize) -> TextRange {
        TextRange::new(start, end)
    }

    /// Replace `1..3` by three bytes (+1) and delete `5..7` (-2) in the same transaction.
    fn two_edits() -> Transaction {
        Transaction::new(
            vec![Edit::replace(r(1, 3), "XYZ"), Edit::delete(r(5, 7))],
            Selection::caret(1),
            Selection::caret(4),
        )
    }

    #[test]
    fn edit_constructors_describe_the_replacement() {
        let replace = Edit::replace(r(2, 5), "ab");
        assert_eq!((replace.delta(), replace.new_range()), (-1, r(2, 4)));
        let insert = Edit::insert(3, "xyz");
        assert_eq!(
            (insert.range, insert.delta(), insert.new_range()),
            (r(3, 3), 3, r(3, 6))
        );
        let delete = Edit::delete(r(1, 4));
        assert_eq!(
            (delete.text.as_str(), delete.delta(), delete.new_range()),
            ("", -3, r(1, 1))
        );
    }

    #[test]
    fn edit_maps_offsets_by_side_and_association() {
        let replace = Edit::replace(r(2, 6), "ab");
        assert_eq!(replace.map_offset(1, Assoc::After), 1, "before the edit");
        assert_eq!(
            replace.map_offset(8, Assoc::Before),
            6,
            "after the edit shifts by delta"
        );
        assert_eq!(
            replace.map_offset(2, Assoc::After),
            2,
            "replaced range start sticks"
        );
        assert_eq!(
            replace.map_offset(6, Assoc::Before),
            4,
            "replaced range end moves to new end"
        );
        assert_eq!(
            replace.map_offset(4, Assoc::Before),
            2,
            "inside collapses to the start"
        );
        assert_eq!(
            replace.map_offset(4, Assoc::After),
            4,
            "inside collapses to the new end"
        );

        let insert = Edit::insert(3, "xy");
        assert_eq!(insert.map_offset(3, Assoc::Before), 3);
        assert_eq!(insert.map_offset(3, Assoc::After), 5);
        assert_eq!(insert.map_offset(2, Assoc::After), 2);
        assert_eq!(insert.map_offset(4, Assoc::Before), 6);

        let delete = Edit::delete(r(2, 6));
        assert_eq!(delete.map_offset(4, Assoc::Before), 2);
        assert_eq!(delete.map_offset(4, Assoc::After), 2);
    }

    #[test]
    fn selection_accessors_order_anchor_and_head() {
        let caret = Selection::caret(4);
        assert!(caret.is_caret() && !caret.is_reversed());
        assert_eq!(caret.range(), r(4, 4));

        let forward = Selection::from_range(r(2, 6));
        assert_eq!(forward, Selection::new(2, 6));
        assert!(!forward.is_caret() && !forward.is_reversed());
        assert_eq!(
            (forward.start(), forward.end(), forward.range()),
            (2, 6, r(2, 6))
        );

        let reversed = Selection::new(6, 2);
        assert!(reversed.is_reversed());
        assert_eq!(
            (reversed.start(), reversed.end(), reversed.range()),
            (2, 6, r(2, 6))
        );

        assert_eq!(forward.with_head(9), Selection::new(2, 9));
        assert_eq!(reversed.collapsed(), Selection::caret(2));
        assert_eq!(Selection::new(3, 12).clamped(8), Selection::new(3, 8));
        assert_eq!(Selection::new(3, 5).clamped(8), Selection::new(3, 5));
    }

    #[test]
    fn selections_map_through_a_transaction_without_swallowing_edge_text() {
        let insert = |offset| {
            Transaction::single(
                Edit::insert(offset, "xy"),
                Selection::default(),
                Selection::default(),
            )
        };
        let at_start = insert(3);
        assert_eq!(
            Selection::caret(3).map_through(&at_start),
            Selection::caret(5)
        );
        assert_eq!(
            Selection::new(3, 6).map_through(&at_start),
            Selection::new(5, 8)
        );
        assert_eq!(
            Selection::new(6, 3).map_through(&at_start),
            Selection::new(8, 5)
        );

        let at_end = insert(3);
        assert_eq!(
            Selection::new(1, 3).map_through(&at_end),
            Selection::new(1, 3)
        );
        assert_eq!(
            Selection::new(3, 1).map_through(&at_end),
            Selection::new(3, 1)
        );
    }

    #[test]
    fn transaction_sums_deltas_and_maps_offsets_through_every_edit() {
        let tx = two_edits();
        assert!(!tx.is_empty());
        assert_eq!(tx.delta(), -1);
        assert_eq!(
            [0, 4, 9].map(|offset| tx.map_offset(offset, Assoc::After)),
            [0, 5, 8]
        );
        assert_eq!(tx.map_offset(2, Assoc::Before), 1);
        assert_eq!(tx.map_offset(2, Assoc::After), 4);

        let empty = Transaction::new(Vec::new(), Selection::caret(0), Selection::caret(0));
        assert!(empty.is_empty());
        assert_eq!(empty.delta(), 0);
        assert_eq!(empty.map_offset(7, Assoc::After), 7);
    }

    #[test]
    fn transaction_maps_ranges_keeping_them_ordered() {
        let tx = two_edits();
        assert_eq!(tx.map_range(r(2, 6)), r(4, 6));
        assert_eq!(tx.map_range(r(0, 1)), r(0, 1));
        assert_eq!(
            tx.map_range(r(2, 2)),
            r(4, 4),
            "an empty range inside a replaced span lands after the new text"
        );
    }

    #[test]
    fn validate_rejects_unordered_overlapping_and_out_of_range_edits() {
        let buffer = Buffer::from_text("0123456789é");
        let tx = |edits| Transaction::new(edits, Selection::default(), Selection::default());

        assert_eq!(two_edits().validate(&buffer), Ok(()));
        assert_eq!(
            tx(vec![Edit::delete(r(1, 3)), Edit::delete(r(3, 5))]).validate(&buffer),
            Ok(()),
            "adjacent edits are disjoint"
        );
        assert_eq!(
            tx(vec![Edit::delete(r(4, 6)), Edit::delete(r(1, 3))]).validate(&buffer),
            Err(EditError::Unordered { index: 1 })
        );
        assert_eq!(
            tx(vec![Edit::delete(r(1, 5)), Edit::delete(r(3, 6))]).validate(&buffer),
            Err(EditError::Unordered { index: 1 })
        );
        assert_eq!(
            tx(vec![Edit::delete(r(8, 99))]).validate(&buffer),
            Err(EditError::Text(TextError::OutOfBounds {
                offset: 99,
                len: 12
            }))
        );
        assert_eq!(
            tx(vec![Edit::insert(11, "x")]).validate(&buffer),
            Err(EditError::Text(TextError::NotCharBoundary { offset: 11 }))
        );
    }

    #[test]
    fn apply_edits_the_buffer_and_returns_the_inverse() {
        let mut buffer = Buffer::from_text("0123456789");
        let inverse = two_edits().apply(&mut buffer).expect("valid transaction");
        assert_eq!(buffer.to_string(), "0XYZ34789");
        assert_eq!(
            inverse,
            Transaction::new(
                vec![Edit::replace(r(1, 4), "12"), Edit::insert(6, "56")],
                Selection::caret(4),
                Selection::caret(1),
            )
        );

        let redo = inverse.apply(&mut buffer).expect("inverse applies");
        assert_eq!(buffer.to_string(), "0123456789");
        assert_eq!(redo, two_edits());
    }

    #[test]
    fn apply_leaves_the_buffer_untouched_when_the_transaction_is_invalid() {
        let mut buffer = Buffer::from_text("0123456789");
        let bad = Transaction::new(
            vec![Edit::delete(r(0, 2)), Edit::delete(r(1, 3))],
            Selection::default(),
            Selection::default(),
        );
        assert_eq!(
            bad.apply(&mut buffer),
            Err(EditError::Unordered { index: 1 })
        );
        assert_eq!(buffer.to_string(), "0123456789");

        let past_end = Transaction::single(
            Edit::delete(r(5, 20)),
            Selection::default(),
            Selection::default(),
        );
        assert_eq!(
            past_end.apply(&mut buffer),
            Err(EditError::Text(TextError::OutOfBounds {
                offset: 20,
                len: 10
            }))
        );
        assert_eq!(buffer.to_string(), "0123456789");
    }
}
