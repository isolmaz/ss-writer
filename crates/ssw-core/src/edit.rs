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
        for edit in self.edits.iter().rev() {
            buffer.replace(edit.range, &edit.text)?;
        }
        Ok(Transaction {
            edits: inverse,
            selection_before: self.selection_after,
            selection_after: self.selection_before,
        })
    }
}
