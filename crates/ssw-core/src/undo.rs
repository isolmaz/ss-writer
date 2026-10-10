//! Operation-based undo history.
//!
//! One undo step is one inverse [`Transaction`]: every command (format, replace-all, table
//! operation, paste, …) is a single step, and consecutive typing is folded into the step of the
//! run it continues. Because a typing run is always one contiguous insertion or deletion, the
//! whole run composes into a single edit, so a step never needs more than one transaction.
//!
//! A typing run is broken by:
//!
//! * a pause longer than [`GROUP_PAUSE_MS`] between two keystrokes,
//! * a caret jump (the next edit does not start where the previous one left the caret),
//! * whitespace typed after a word (the whitespace starts a new run),
//! * a change of kind (insert, backspace, delete),
//! * [`UndoStack::break_group`], undo/redo and every command step.

use crate::edit::{Edit, Selection, Transaction};

/// A pause between two keystrokes longer than this many milliseconds starts a new undo step.
pub const GROUP_PAUSE_MS: u64 = 1000;

/// A point on a monotonic clock, in milliseconds. The clock is chosen by the caller (the UI
/// converts `Instant`s to milliseconds since its own origin) so that grouping is
/// deterministic in tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    pub const fn as_millis(self) -> u64 {
        self.0
    }
}

/// How a typing transaction changes the text; only runs of the same kind are grouped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypingKind {
    /// Text inserted at the caret (possibly replacing a selection for the first keystroke).
    Insert,
    /// Backspace: removes text before the caret.
    DeleteBackward,
    /// Delete: removes text after the caret.
    DeleteForward,
}

/// Classifies `tx` as one typing keystroke, or `None` when it is a command.
pub fn typing_kind(tx: &Transaction) -> Option<TypingKind> {
    let [edit] = tx.edits.as_slice() else {
        return None;
    };
    if !edit.text.is_empty() {
        let caret = Selection::caret(edit.range.start + edit.text.len());
        return (tx.selection_after == caret).then_some(TypingKind::Insert);
    }
    if edit.range.is_empty()
        || !tx.selection_before.is_caret()
        || tx.selection_after != Selection::caret(edit.range.start)
    {
        return None;
    }
    let head = tx.selection_before.head;
    if head == edit.range.end {
        Some(TypingKind::DeleteBackward)
    } else if head == edit.range.start {
        Some(TypingKind::DeleteForward)
    } else {
        None
    }
}

/// The typing run that the top of the undo stack still accepts.
#[derive(Clone, Copy, Debug)]
struct OpenGroup {
    kind: TypingKind,
    /// Time of the last keystroke folded into the run.
    at: Timestamp,
    /// Caret after the last keystroke.
    caret: usize,
    /// Whether the last inserted character was whitespace (insert runs only).
    last_is_space: bool,
}

/// Undo and redo stacks.
#[derive(Clone, Debug, Default)]
pub struct UndoStack {
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
    open: Option<OpenGroup>,
}

impl UndoStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Number of undo steps.
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    /// Number of redo steps.
    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    /// Records a command: always its own step. `inverse` is what [`Transaction::apply`]
    /// returned for the command. Clears the redo stack.
    pub fn push_command(&mut self, inverse: Transaction) {
        self.open = None;
        self.redo.clear();
        self.undo.push(inverse);
    }

    /// Records a keystroke, folding it into the open typing run when it continues it.
    /// `forward` is the applied transaction, `inverse` what applying it returned. A
    /// transaction that is not a keystroke is recorded as a command. Clears the redo stack.
    pub fn push_typing(&mut self, forward: &Transaction, inverse: Transaction, now: Timestamp) {
        let Some(kind) = typing_kind(forward) else {
            self.push_command(inverse);
            return;
        };
        self.redo.clear();
        let edit = &forward.edits[0];
        let last_is_space = edit
            .text
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        let caret = forward.selection_after.head;
        if let Some(open) = self.open
            && self.continues(open, kind, forward, now)
            && let Some(step) = self.undo.last_mut()
        {
            merge_into(step, inverse, kind);
            self.open = Some(OpenGroup {
                kind,
                at: now,
                caret,
                last_is_space,
            });
            return;
        }
        self.undo.push(inverse);
        self.open = Some(OpenGroup {
            kind,
            at: now,
            caret,
            last_is_space,
        });
    }

    fn continues(
        &self,
        open: OpenGroup,
        kind: TypingKind,
        forward: &Transaction,
        now: Timestamp,
    ) -> bool {
        let edit = &forward.edits[0];
        if open.kind != kind
            || now.0.saturating_sub(open.at.0) > GROUP_PAUSE_MS
            || forward.selection_before.head != open.caret
        {
            return false;
        }
        match kind {
            TypingKind::Insert => {
                let starts_with_space = edit.text.chars().next().is_some_and(char::is_whitespace);
                edit.range.is_empty()
                    && edit.range.start == open.caret
                    && !(starts_with_space && !open.last_is_space)
            }
            TypingKind::DeleteBackward | TypingKind::DeleteForward => true,
        }
    }

    /// Ends the open typing run: the next keystroke starts a new step.
    pub fn break_group(&mut self) {
        self.open = None;
    }

    /// Takes the newest undo step (the inverse transaction to apply).
    pub fn pop_undo(&mut self) -> Option<Transaction> {
        self.open = None;
        self.undo.pop()
    }

    /// Takes the newest redo step.
    pub fn pop_redo(&mut self) -> Option<Transaction> {
        self.open = None;
        self.redo.pop()
    }

    /// Stores the transaction that redoes an undone step.
    pub fn push_redo(&mut self, forward: Transaction) {
        self.redo.push(forward);
    }

    /// Stores the inverse of a redone step without clearing the remaining redo steps.
    pub fn push_undo(&mut self, inverse: Transaction) {
        self.undo.push(inverse);
    }
}

/// Folds the inverse of one more keystroke into the step of the run.
///
/// Steps of a typing run hold exactly one edit; `inverse` holds one edit too (it inverts a
/// single-edit keystroke). Coordinates are those of the document after the keystroke.
fn merge_into(step: &mut Transaction, inverse: Transaction, kind: TypingKind) {
    // Both transactions hold exactly one edit: `push_typing` only folds keystrokes accepted by
    // `typing_kind`, and the first step of a run is the inverse of such a keystroke.
    let Transaction {
        mut edits,
        selection_before,
        ..
    } = inverse;
    let new = edits.swap_remove(0);
    let top = &mut step.edits[0];
    match kind {
        // The new inverse removes text right behind the run: extend the removed range.
        TypingKind::Insert => top.range.end = new.range.end,
        TypingKind::DeleteBackward => {
            *top = Edit::insert(new.range.start, new.text + &top.text);
        }
        TypingKind::DeleteForward => top.text.push_str(&new.text),
    }
    step.selection_before = selection_before;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::TextRange;

    fn t(ms: u64) -> Timestamp {
        Timestamp::from_millis(ms)
    }

    /// Forward insertion of `text` at `at` and its inverse.
    fn typed(at: usize, text: &str) -> (Transaction, Transaction) {
        let end = at + text.len();
        let forward = Transaction::single(
            Edit::insert(at, text),
            Selection::caret(at),
            Selection::caret(end),
        );
        let inverse = Transaction::single(
            Edit::delete(TextRange::new(at, end)),
            Selection::caret(end),
            Selection::caret(at),
        );
        (forward, inverse)
    }

    /// Backspace of `removed` that ended at `end`, and its inverse.
    fn backspaced(end: usize, removed: &str) -> (Transaction, Transaction) {
        let start = end - removed.len();
        let forward = Transaction::single(
            Edit::delete(TextRange::new(start, end)),
            Selection::caret(end),
            Selection::caret(start),
        );
        let inverse = Transaction::single(
            Edit::insert(start, removed),
            Selection::caret(start),
            Selection::caret(end),
        );
        (forward, inverse)
    }

    /// Delete key removing `removed` after the caret at `at`, and its inverse.
    fn deleted(at: usize, removed: &str) -> (Transaction, Transaction) {
        let forward = Transaction::single(
            Edit::delete(TextRange::new(at, at + removed.len())),
            Selection::caret(at),
            Selection::caret(at),
        );
        let inverse = Transaction::single(
            Edit::insert(at, removed),
            Selection::caret(at),
            Selection::caret(at),
        );
        (forward, inverse)
    }

    fn push(stack: &mut UndoStack, pair: (Transaction, Transaction), ms: u64) {
        stack.push_typing(&pair.0, pair.1, t(ms));
    }

    #[test]
    fn typing_kind_classifies_keystrokes() {
        assert_eq!(typing_kind(&typed(3, "a").0), Some(TypingKind::Insert));
        assert_eq!(
            typing_kind(&backspaced(3, "a").0),
            Some(TypingKind::DeleteBackward)
        );
        assert_eq!(
            typing_kind(&deleted(3, "a").0),
            Some(TypingKind::DeleteForward)
        );
        // Replacing a selection with typed text is an insert keystroke.
        let replace = Transaction::single(
            Edit::replace(TextRange::new(2, 5), "x"),
            Selection::new(2, 5),
            Selection::caret(3),
        );
        assert_eq!(typing_kind(&replace), Some(TypingKind::Insert));
    }

    #[test]
    fn typing_kind_rejects_commands() {
        // No edit, or several edits.
        let none = Transaction::new(vec![], Selection::caret(0), Selection::caret(0));
        assert_eq!(typing_kind(&none), None);
        let two = Transaction::new(
            vec![Edit::insert(0, "a"), Edit::insert(4, "b")],
            Selection::caret(0),
            Selection::caret(1),
        );
        assert_eq!(typing_kind(&two), None);
        // Insertion that does not leave the caret after the text.
        let odd_caret = Transaction::single(
            Edit::insert(0, "ab"),
            Selection::caret(0),
            Selection::caret(1),
        );
        assert_eq!(typing_kind(&odd_caret), None);
        // Empty edit.
        let empty = Transaction::single(
            Edit::insert(2, ""),
            Selection::caret(2),
            Selection::caret(2),
        );
        assert_eq!(typing_kind(&empty), None);
        // Deleting a selection.
        let sel_delete = Transaction::single(
            Edit::delete(TextRange::new(1, 4)),
            Selection::new(1, 4),
            Selection::caret(1),
        );
        assert_eq!(typing_kind(&sel_delete), None);
        // Deletion that leaves the caret elsewhere.
        let wrong_after = Transaction::single(
            Edit::delete(TextRange::new(1, 2)),
            Selection::caret(2),
            Selection::caret(2),
        );
        assert_eq!(typing_kind(&wrong_after), None);
        // Caret not at either end of the deleted range.
        let far = Transaction::single(
            Edit::delete(TextRange::new(1, 3)),
            Selection::caret(7),
            Selection::caret(1),
        );
        assert_eq!(typing_kind(&far), None);
    }

    #[test]
    fn insert_run_folds_into_one_step() {
        let mut s = UndoStack::new();
        for (i, ch) in ["h", "e", "l", "l", "o"].into_iter().enumerate() {
            push(&mut s, typed(i, ch), 100 * i as u64);
        }
        assert_eq!(s.undo_depth(), 1);
        let step = s.pop_undo().expect("one step");
        assert_eq!(
            step,
            Transaction::single(
                Edit::delete(TextRange::new(0, 5)),
                Selection::caret(5),
                Selection::caret(0)
            )
        );
    }

    #[test]
    fn pause_breaks_run_only_above_one_second() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        push(&mut s, typed(1, "b"), 1000);
        assert_eq!(s.undo_depth(), 1, "exactly 1000 ms keeps the run");
        push(&mut s, typed(2, "c"), 2001);
        assert_eq!(s.undo_depth(), 2, "1001 ms breaks the run");
    }

    #[test]
    fn caret_jump_breaks_run() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        push(&mut s, typed(10, "b"), 10);
        assert_eq!(s.undo_depth(), 2);
    }

    #[test]
    fn insert_not_at_caret_breaks_run_even_when_head_matches() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        // Selection head equals the caret but the edit lands elsewhere.
        let forward = Transaction::single(
            Edit::insert(0, "b"),
            Selection::caret(1),
            Selection::caret(1),
        );
        let inverse = Transaction::single(
            Edit::delete(TextRange::new(0, 1)),
            Selection::caret(1),
            Selection::caret(1),
        );
        s.push_typing(&forward, inverse, t(10));
        assert_eq!(s.undo_depth(), 2);
    }

    #[test]
    fn replacement_after_a_run_does_not_extend_it() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        let forward = Transaction::single(
            Edit::replace(TextRange::new(0, 1), "b"),
            Selection::new(0, 1),
            Selection::caret(1),
        );
        // selection_before.head (1) equals the caret, but the edit is not a pure insertion.
        let inverse = Transaction::single(
            Edit::replace(TextRange::new(0, 1), "a"),
            Selection::caret(1),
            Selection::new(0, 1),
        );
        s.push_typing(&forward, inverse, t(10));
        assert_eq!(s.undo_depth(), 2);
    }

    #[test]
    fn whitespace_after_word_starts_new_run() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        push(&mut s, typed(1, "b"), 10);
        push(&mut s, typed(2, " "), 20);
        assert_eq!(s.undo_depth(), 2);
        push(&mut s, typed(3, "c"), 30);
        push(&mut s, typed(4, " "), 40);
        push(&mut s, typed(5, " "), 50);
        // "c" joins the whitespace run; the space after word "c" starts a third run that the
        // second space joins.
        assert_eq!(s.undo_depth(), 3);
        let last = s.pop_undo().expect("step");
        assert_eq!(last.edits[0].range, TextRange::new(4, 6));
    }

    #[test]
    fn whitespace_run_continues_over_further_whitespace_and_words() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, " "), 0);
        push(&mut s, typed(1, " "), 10);
        push(&mut s, typed(2, "x"), 20);
        assert_eq!(s.undo_depth(), 1);
    }

    #[test]
    fn newline_counts_as_whitespace() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        push(&mut s, typed(1, "\n"), 10);
        assert_eq!(s.undo_depth(), 2);
    }

    #[test]
    fn multi_char_insertion_tracks_its_last_character() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "ab"), 0);
        // "ab" ends in a word character, so a following space breaks the run.
        push(&mut s, typed(2, " "), 10);
        assert_eq!(s.undo_depth(), 2);
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a "), 0);
        push(&mut s, typed(2, " "), 10);
        assert_eq!(s.undo_depth(), 1, "run ending in a space accepts a space");
    }

    #[test]
    fn backspace_run_composes_into_one_insertion() {
        let mut s = UndoStack::new();
        push(&mut s, backspaced(5, "o"), 0);
        push(&mut s, backspaced(4, "l"), 10);
        push(&mut s, backspaced(3, "l"), 20);
        assert_eq!(s.undo_depth(), 1);
        let step = s.pop_undo().expect("step");
        assert_eq!(
            step,
            Transaction::single(
                Edit::insert(2, "llo"),
                Selection::caret(2),
                Selection::caret(5)
            )
        );
    }

    #[test]
    fn delete_run_composes_into_one_insertion() {
        let mut s = UndoStack::new();
        push(&mut s, deleted(2, "l"), 0);
        push(&mut s, deleted(2, "l"), 10);
        push(&mut s, deleted(2, "o"), 20);
        assert_eq!(s.undo_depth(), 1);
        let step = s.pop_undo().expect("step");
        assert_eq!(
            step,
            Transaction::single(
                Edit::insert(2, "llo"),
                Selection::caret(2),
                Selection::caret(2)
            )
        );
    }

    #[test]
    fn kind_change_breaks_run() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        push(&mut s, backspaced(1, "a"), 10);
        assert_eq!(s.undo_depth(), 2);
        push(&mut s, deleted(0, "z"), 20);
        assert_eq!(s.undo_depth(), 3);
    }

    #[test]
    fn explicit_boundary_breaks_run() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        s.break_group();
        push(&mut s, typed(1, "b"), 10);
        assert_eq!(s.undo_depth(), 2);
    }

    #[test]
    fn command_between_keystrokes_breaks_run_and_is_its_own_step() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        let (_, inv) = typed(1, "**");
        s.push_command(inv);
        push(&mut s, typed(3, "b"), 20);
        assert_eq!(s.undo_depth(), 3);
    }

    #[test]
    fn non_keystroke_transaction_is_a_command() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        let two = Transaction::new(
            vec![Edit::insert(0, "x"), Edit::insert(1, "y")],
            Selection::caret(1),
            Selection::caret(1),
        );
        let inverse = Transaction::new(
            vec![
                Edit::delete(TextRange::new(0, 1)),
                Edit::delete(TextRange::new(2, 3)),
            ],
            Selection::caret(1),
            Selection::caret(1),
        );
        s.push_typing(&two, inverse, t(5));
        push(&mut s, typed(1, "b"), 6);
        assert_eq!(s.undo_depth(), 3);
    }

    #[test]
    fn pushing_clears_redo_but_push_undo_does_not() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        let step = s.pop_undo().expect("step");
        s.push_redo(step.clone());
        assert!(s.can_redo());
        assert_eq!(s.redo_depth(), 1);
        let redo = s.pop_redo().expect("redo");
        s.push_redo(redo.clone());
        s.push_undo(step.clone());
        assert!(s.can_redo(), "push_undo keeps redo");
        assert!(s.can_undo());
        push(&mut s, typed(0, "b"), 0);
        assert!(!s.can_redo(), "typing clears redo");
        s.push_redo(redo);
        s.push_command(step);
        assert!(!s.can_redo(), "command clears redo");
    }

    #[test]
    fn typing_after_undo_starts_new_run() {
        let mut s = UndoStack::new();
        push(&mut s, typed(0, "a"), 0);
        let step = s.pop_undo().expect("step");
        s.push_undo(step);
        push(&mut s, typed(1, "b"), 10);
        assert_eq!(s.undo_depth(), 2);
    }

    #[test]
    fn pops_on_empty_stacks_yield_none() {
        let mut s = UndoStack::new();
        assert!(!s.can_undo());
        assert!(s.pop_undo().is_none());
        assert!(s.pop_redo().is_none());
    }

    #[test]
    fn timestamp_round_trips_millis() {
        assert_eq!(Timestamp::from_millis(42).as_millis(), 42);
    }
}
