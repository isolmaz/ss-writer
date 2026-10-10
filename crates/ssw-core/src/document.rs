//! The editable document: text, tree, selections and undo history.

use crate::edit::{Edit, EditError, Selection, Transaction};
use crate::markdown::{ast, parse_document};
use crate::ops::Ctx;
use crate::text::Buffer;
use crate::undo::{Timestamp, UndoStack};

/// Text plus its parsed tree, the selections (primary first), a version counter and the undo
/// history. Every change goes through [`Document::apply`], [`Document::apply_typing`],
/// [`Document::undo`] or [`Document::redo`], which keep the tree and the selections in sync.
#[derive(Clone, Debug)]
pub struct Document {
    buffer: Buffer,
    ast: ast::Document,
    selections: Vec<Selection>,
    version: u64,
    undo: UndoStack,
}

impl Document {
    /// A document holding `text` with the caret at the start.
    pub fn new(text: &str) -> Self {
        let buffer = Buffer::from_text(text);
        let ast = parse_document(text);
        Self {
            buffer,
            ast,
            selections: vec![Selection::caret(0)],
            version: 0,
            undo: UndoStack::new(),
        }
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// The whole text.
    pub fn text(&self) -> String {
        self.buffer.to_string()
    }

    pub fn ast(&self) -> &ast::Document {
        &self.ast
    }

    /// All selections; the primary selection is first. Never empty.
    pub fn selections(&self) -> &[Selection] {
        &self.selections
    }

    pub fn primary_selection(&self) -> Selection {
        self.selections[0]
    }

    /// Replaces the selections (clamped to the text). Does not change the version or the
    /// undo history.
    pub fn set_selections(&mut self, primary: Selection, extra: &[Selection]) {
        let len = self.buffer.len();
        self.selections.clear();
        self.selections.push(primary.clamped(len));
        self.selections.extend(extra.iter().map(|s| s.clamped(len)));
    }

    /// Increases with every text change, including undo and redo.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Context for the editing operations of [`crate::ops`], over the primary selection.
    pub fn ctx(&self) -> Ctx<'_> {
        Ctx::new(&self.buffer, &self.ast, self.primary_selection())
    }

    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    /// Applies a command transaction as one undo step. A transaction without edits only moves
    /// the selection.
    pub fn apply(&mut self, tx: Transaction) -> Result<(), EditError> {
        if tx.is_empty() {
            self.set_selections(tx.selection_after, &[]);
            return Ok(());
        }
        let inverse = self.apply_transaction(&tx)?;
        self.undo.push_command(inverse);
        Ok(())
    }

    /// Applies a keystroke at time `now`; it joins the previous typing run when it continues
    /// it (see [`crate::undo`] for the grouping rules).
    pub fn apply_typing(&mut self, tx: Transaction, now: Timestamp) -> Result<(), EditError> {
        if tx.is_empty() {
            self.set_selections(tx.selection_after, &[]);
            return Ok(());
        }
        let inverse = self.apply_transaction(&tx)?;
        self.undo.push_typing(&tx, inverse, now);
        Ok(())
    }

    /// Ends the current typing run (focus change, explicit boundary).
    pub fn break_undo_group(&mut self) {
        self.undo.break_group();
    }

    /// Undoes the newest step and restores the selection from before it. Returns `false`
    /// when there is nothing to undo.
    pub fn undo(&mut self) -> Result<bool, EditError> {
        let Some(step) = self.undo.pop_undo() else {
            return Ok(false);
        };
        let redo = self.apply_transaction(&step)?;
        self.undo.push_redo(redo);
        Ok(true)
    }

    /// Redoes the newest undone step. Returns `false` when there is nothing to redo.
    pub fn redo(&mut self) -> Result<bool, EditError> {
        let Some(step) = self.undo.pop_redo() else {
            return Ok(false);
        };
        let inverse = self.apply_transaction(&step)?;
        self.undo.push_undo(inverse);
        Ok(true)
    }

    /// Applies `tx` to the text, selections and tree; returns its inverse.
    fn apply_transaction(&mut self, tx: &Transaction) -> Result<Transaction, EditError> {
        let inverse = tx.apply(&mut self.buffer)?;
        self.version += 1;
        let len = self.buffer.len();
        let extra: Vec<Selection> = self
            .selections
            .iter()
            .skip(1)
            .map(|s| s.map_through(tx).clamped(len))
            .filter(|s| *s != tx.selection_after)
            .collect();
        self.selections.clear();
        self.selections.push(tx.selection_after.clamped(len));
        self.selections.extend(extra);
        self.refresh_ast(&tx.edits);
        Ok(inverse)
    }

    /// Brings the tree in line with the text after `edits` (old-document coordinates).
    ///
    /// This is the only place that parses; it re-parses everything until the incremental
    /// parser replaces the call.
    fn refresh_ast(&mut self, _edits: &[Edit]) {
        self.ast = parse_document(&self.buffer.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::ast::BlockKind;
    use crate::text::TextRange;

    fn insert(at: usize, text: &str) -> Transaction {
        Transaction::single(
            Edit::insert(at, text),
            Selection::caret(at),
            Selection::caret(at + text.len()),
        )
    }

    fn ms(v: u64) -> Timestamp {
        Timestamp::from_millis(v)
    }

    #[test]
    fn new_document_parses_and_starts_at_origin() {
        let doc = Document::new("# Hi\n");
        assert_eq!(doc.text(), "# Hi\n");
        assert_eq!(doc.version(), 0);
        assert_eq!(doc.primary_selection(), Selection::caret(0));
        assert_eq!(doc.selections(), [Selection::caret(0)]);
        assert!(matches!(
            doc.ast().blocks[0].kind,
            BlockKind::Heading { level: 1, .. }
        ));
        assert_eq!(doc.buffer().len(), 5);
        assert!(!doc.can_undo());
        assert!(!doc.can_redo());
    }

    #[test]
    fn apply_updates_text_tree_selection_and_version() {
        let mut doc = Document::new("Hi\n");
        doc.apply(insert(0, "# ")).expect("apply");
        assert_eq!(doc.text(), "# Hi\n");
        assert_eq!(doc.version(), 1);
        assert_eq!(doc.primary_selection(), Selection::caret(2));
        assert!(matches!(
            doc.ast().blocks[0].kind,
            BlockKind::Heading { level: 1, .. }
        ));
        assert!(doc.can_undo());
    }

    #[test]
    fn invalid_transaction_leaves_document_untouched() {
        let mut doc = Document::new("ab");
        let bad = Transaction::single(
            Edit::insert(9, "x"),
            Selection::caret(0),
            Selection::caret(1),
        );
        assert!(doc.apply(bad).is_err());
        assert_eq!(doc.text(), "ab");
        assert_eq!(doc.version(), 0);
        assert!(!doc.can_undo());
    }

    #[test]
    fn empty_transaction_only_moves_selection() {
        let mut doc = Document::new("abc");
        doc.apply(Transaction::new(
            vec![],
            Selection::caret(0),
            Selection::new(1, 3),
        ))
        .expect("apply");
        assert_eq!(doc.primary_selection(), Selection::new(1, 3));
        assert_eq!(doc.version(), 0);
        assert!(!doc.can_undo());
        doc.apply_typing(
            Transaction::new(vec![], Selection::caret(0), Selection::caret(2)),
            ms(0),
        )
        .expect("typing");
        assert_eq!(doc.primary_selection(), Selection::caret(2));
        assert_eq!(doc.version(), 0);
        assert!(!doc.can_undo());
    }

    #[test]
    fn set_selections_clamps_and_keeps_primary_first() {
        let mut doc = Document::new("abc");
        doc.set_selections(
            Selection::new(1, 99),
            &[Selection::caret(50), Selection::caret(2)],
        );
        assert_eq!(
            doc.selections(),
            [
                Selection::new(1, 3),
                Selection::caret(3),
                Selection::caret(2)
            ]
        );
        assert_eq!(doc.version(), 0);
    }

    #[test]
    fn other_selections_follow_edits_and_duplicates_merge() {
        let mut doc = Document::new("abcdef");
        doc.set_selections(
            Selection::caret(0),
            &[Selection::caret(4), Selection::caret(1)],
        );
        doc.apply(insert(0, "XY")).expect("apply");
        // The caret at 1 was mapped to 3; the primary caret is at 2.
        assert_eq!(
            doc.selections(),
            [
                Selection::caret(2),
                Selection::caret(6),
                Selection::caret(3)
            ]
        );
        // A secondary selection that collapses onto the primary is dropped.
        let mut doc = Document::new("abc");
        doc.set_selections(Selection::caret(1), &[Selection::caret(0)]);
        let tx = Transaction::single(
            Edit::insert(0, "Z"),
            Selection::caret(1),
            Selection::caret(1),
        );
        doc.apply(tx).expect("apply");
        assert_eq!(doc.selections(), [Selection::caret(1)]);
    }

    #[test]
    fn undo_and_redo_restore_text_selection_and_tree() {
        let mut doc = Document::new("Hi\n");
        doc.set_selections(Selection::new(0, 2), &[]);
        let tx = Transaction::single(
            Edit::insert(0, "# "),
            Selection::new(0, 2),
            Selection::new(2, 4),
        );
        doc.apply(tx).expect("apply");
        assert_eq!(doc.version(), 1);

        assert!(doc.undo().expect("undo"));
        assert_eq!(doc.text(), "Hi\n");
        assert_eq!(doc.primary_selection(), Selection::new(0, 2));
        assert!(matches!(doc.ast().blocks[0].kind, BlockKind::Paragraph));
        assert_eq!(doc.version(), 2);
        assert!(doc.can_redo());
        assert!(!doc.can_undo());

        assert!(doc.redo().expect("redo"));
        assert_eq!(doc.text(), "# Hi\n");
        assert_eq!(doc.primary_selection(), Selection::new(2, 4));
        assert!(matches!(
            doc.ast().blocks[0].kind,
            BlockKind::Heading { .. }
        ));
        assert!(doc.can_undo());
        assert!(!doc.can_redo());
    }

    #[test]
    fn undo_redo_with_nothing_to_do_return_false() {
        let mut doc = Document::new("a");
        assert!(!doc.undo().expect("undo"));
        assert!(!doc.redo().expect("redo"));
        assert_eq!(doc.version(), 0);
    }

    #[test]
    fn new_edit_discards_redo() {
        let mut doc = Document::new("");
        doc.apply(insert(0, "a")).expect("apply");
        doc.undo().expect("undo");
        assert!(doc.can_redo());
        doc.apply(insert(0, "b")).expect("apply");
        assert!(!doc.can_redo());
        assert!(!doc.redo().expect("redo"));
        assert_eq!(doc.text(), "b");
    }

    #[test]
    fn typing_timeline_groups_runs() {
        let mut doc = Document::new("");
        let mut at = 0usize;
        let mut clock = 0u64;
        let mut type_text = |doc: &mut Document, text: &str, dt: u64| {
            for ch in text.chars() {
                clock += dt;
                let s = ch.to_string();
                doc.apply_typing(insert(at, &s), ms(clock)).expect("typing");
                at += s.len();
            }
        };
        type_text(&mut doc, "hello", 100); // run 1
        type_text(&mut doc, " world", 100); // run 2: whitespace after word starts it
        type_text(&mut doc, "!", 2000); // run 3: pause
        assert_eq!(doc.text(), "hello world!");

        doc.undo().expect("undo");
        assert_eq!(doc.text(), "hello world");
        assert_eq!(doc.primary_selection(), Selection::caret(11));
        doc.undo().expect("undo");
        assert_eq!(doc.text(), "hello");
        doc.undo().expect("undo");
        assert_eq!(doc.text(), "");
        assert_eq!(doc.primary_selection(), Selection::caret(0));
        assert!(!doc.can_undo());
        doc.redo().expect("redo");
        doc.redo().expect("redo");
        doc.redo().expect("redo");
        assert_eq!(doc.text(), "hello world!");
        assert_eq!(doc.primary_selection(), Selection::caret(12));
    }

    #[test]
    fn caret_jump_and_explicit_boundary_break_runs() {
        let mut doc = Document::new("abcdef");
        doc.apply_typing(insert(0, "x"), ms(0)).expect("typing");
        doc.apply_typing(insert(4, "y"), ms(10)).expect("typing");
        assert_eq!(doc.text(), "xabcydef");
        doc.undo().expect("undo");
        assert_eq!(doc.text(), "xabcdef");
        doc.undo().expect("undo");
        assert_eq!(doc.text(), "abcdef");

        let mut doc = Document::new("");
        doc.apply_typing(insert(0, "a"), ms(0)).expect("typing");
        doc.break_undo_group();
        doc.apply_typing(insert(1, "b"), ms(10)).expect("typing");
        doc.undo().expect("undo");
        assert_eq!(doc.text(), "a");
    }

    #[test]
    fn backspace_run_undoes_in_one_step() {
        let mut doc = Document::new("hello");
        doc.set_selections(Selection::caret(5), &[]);
        for (i, end) in [5usize, 4, 3].into_iter().enumerate() {
            let tx = Transaction::single(
                Edit::delete(TextRange::new(end - 1, end)),
                Selection::caret(end),
                Selection::caret(end - 1),
            );
            doc.apply_typing(tx, ms(i as u64 * 50)).expect("typing");
        }
        assert_eq!(doc.text(), "he");
        doc.undo().expect("undo");
        assert_eq!(doc.text(), "hello");
        assert_eq!(doc.primary_selection(), Selection::caret(5));
        doc.redo().expect("redo");
        assert_eq!(doc.text(), "he");
        assert_eq!(doc.primary_selection(), Selection::caret(2));
    }

    #[test]
    fn commands_break_typing_runs_and_are_single_steps() {
        let mut doc = Document::new("");
        doc.apply_typing(insert(0, "a"), ms(0)).expect("typing");
        let wrap = Transaction::new(
            vec![Edit::insert(0, "**"), Edit::insert(1, "**")],
            Selection::caret(1),
            Selection::caret(3),
        );
        doc.apply(wrap).expect("apply");
        assert_eq!(doc.text(), "**a**");
        doc.apply_typing(insert(3, "b"), ms(20)).expect("typing");
        assert_eq!(doc.text(), "**ab**");
        doc.undo().expect("undo");
        assert_eq!(doc.text(), "**a**");
        doc.undo().expect("undo");
        assert_eq!(doc.text(), "a");
        doc.undo().expect("undo");
        assert_eq!(doc.text(), "");
    }
}
