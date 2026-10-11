//! Heading folding of one tab (spec §2.8).
//!
//! The folded headings live in a [`FoldSet`] (by heading title and occurrence, so the state
//! can be stored per file). After every change of the document the set is applied to the
//! editor again as source ranges ([`EditorView::set_folds`]); when the number of headings did
//! not change, the folds follow their headings by position, so retitling a folded heading
//! keeps it folded. A tab with nothing folded does no work on an edit.

use ssw_core::document::Document;
use ssw_core::folding::{FoldSet, fold_key, fold_ranges};
use ssw_core::text::TextRange;
use ssw_core::toc::Toc;
use ssw_ui::editor::EditorView;

/// The fold state of a tab.
#[derive(Debug, Default)]
pub struct TabFolds {
    set: FoldSet,
    /// Outline indices of the folded headings at the last application.
    folded: Vec<usize>,
    /// Number of headings at the last application that had folds; only read while something
    /// is folded.
    headings: usize,
    /// Document version the editor's fold ranges belong to.
    version: Option<u64>,
}

impl TabFolds {
    /// Folds the headings that start at `offsets` (the stored view state of the file).
    #[must_use]
    pub fn from_offsets(document: &Document, offsets: &[usize]) -> Self {
        let (toc, _) = outline(document);
        let keys = toc
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, entry)| offsets.contains(&entry.range.start))
            .filter_map(|(index, _)| fold_key(&toc, index));
        Self {
            set: FoldSet::from_keys(keys),
            ..Self::default()
        }
    }

    /// Source offsets of the folded headings, for the stored view state.
    #[must_use]
    pub fn offsets(&self, document: &Document) -> Vec<usize> {
        let (toc, _) = outline(document);
        toc.entries()
            .iter()
            .enumerate()
            .filter(|(index, _)| self.set.is_folded(&toc, *index))
            .map(|(_, entry)| entry.range.start)
            .collect()
    }

    /// Applies the folds to `editor` when its document changed since the last application.
    pub fn sync(&mut self, editor: &mut EditorView) {
        let version = editor.document().borrow().version();
        if self.version != Some(version) {
            self.apply(editor, true);
        }
    }

    /// Applies the folds to `editor`, a new view of the document (a new tab, or a tab that moved
    /// to another window): unlike [`TabFolds::sync`] it applies them even when the document did
    /// not change since they were last applied.
    pub fn attach(&mut self, editor: &mut EditorView) {
        let version = editor.document().borrow().version();
        self.apply(editor, self.version != Some(version));
    }

    /// Flips the heading block starting at `heading_start` (the gutter chevron).
    pub fn toggle(&mut self, editor: &mut EditorView, heading_start: usize) {
        let (toc, _) = outline(&editor.document().borrow());
        if let Some(index) = toc
            .entries()
            .iter()
            .position(|entry| entry.range.start == heading_start)
        {
            self.set.toggle(&toc, index);
        }
        self.apply(editor, false);
    }

    /// View → Fold all.
    pub fn fold_all(&mut self, editor: &mut EditorView) {
        {
            let (toc, text) = outline(&editor.document().borrow());
            let ranges = fold_ranges(&toc, &text);
            self.set.fold_all(&toc, &ranges);
        }
        self.apply(editor, false);
    }

    /// View → Unfold all.
    pub fn unfold_all(&mut self, editor: &mut EditorView) {
        self.set.unfold_all();
        self.apply(editor, false);
    }

    /// Unfolds the sections hiding `range` (a search match); true when one was folded.
    pub fn unfold_containing(&mut self, editor: &mut EditorView, range: TextRange) -> bool {
        let changed = {
            let (toc, text) = outline(&editor.document().borrow());
            let ranges = fold_ranges(&toc, &text);
            self.set.unfold_containing(&toc, &ranges, range.start)
                | self.set.unfold_containing(&toc, &ranges, range.end)
        };
        if changed {
            self.apply(editor, false);
        }
        changed
    }

    /// Recomputes the hidden ranges for the current text and hands them to the editor.
    /// `edited`: the text changed since the last application (folds follow their headings).
    fn apply(&mut self, editor: &mut EditorView, edited: bool) {
        let (hidden, folded_headings) = {
            let document = editor.document().borrow();
            if self.set.is_empty() && self.folded.is_empty() {
                // Nothing is folded and nothing was, so the editor has no folds to replace, and
                // the outline is not worth building for every keystroke of a large document.
                self.version = Some(document.version());
                return;
            }
            let (toc, text) = outline(&document);
            if edited && self.version.is_some() && toc.entries().len() == self.headings {
                // Same headings, possibly retitled: a fold stays with the heading at its index.
                self.set = FoldSet::from_keys(
                    self.folded
                        .iter()
                        .filter_map(|&index| fold_key(&toc, index)),
                );
            }
            let ranges = fold_ranges(&toc, &text);
            self.headings = toc.entries().len();
            self.folded = (0..self.headings)
                .filter(|&index| self.set.is_folded(&toc, index))
                .collect();
            self.version = Some(document.version());
            // The pill belongs to headings that hide something.
            let folded_headings = ranges
                .iter()
                .filter(|r| self.folded.contains(&r.heading))
                .filter_map(|r| toc.entries().get(r.heading))
                .map(|entry| entry.range.start)
                .collect();
            (self.set.hidden_ranges(&toc, &ranges), folded_headings)
        };
        editor.set_folds(hidden, folded_headings);
    }
}

/// The outline of `document` and the text it was built from (copied once).
fn outline(document: &Document) -> (Toc, String) {
    let text = document.text();
    (Toc::build(document.ast(), &text), text)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ssw_ui::services::{Clock, FakeClock};

    use super::*;
    use crate::documents::replace_text;

    const TEXT: &str = "# One\n\nbody one\n\n# Two\n\nbody two\n\n## Sub\n\nbody sub\n";

    fn editor(text: &str) -> EditorView {
        let doc = Rc::new(RefCell::new(Document::new(text)));
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        EditorView::new(doc, clock)
    }

    /// A second editor over the document of `editor`, like the one of a tab that moved.
    fn another_view(editor: &EditorView) -> EditorView {
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        EditorView::new(Rc::clone(editor.document()), clock)
    }

    fn hidden(editor: &EditorView, needle: &str) -> bool {
        let at = editor.document().borrow().text().find(needle).unwrap() + 1;
        editor.is_offset_hidden(at)
    }

    fn edit(editor: &EditorView, text: &str) {
        assert!(replace_text(&mut editor.document().borrow_mut(), text));
    }

    #[test]
    fn folds_are_restored_from_offsets_and_reported_as_offsets() {
        let mut editor = editor(TEXT);
        let two = TEXT.find("# Two").unwrap();
        let mut folds = TabFolds::from_offsets(&editor.document().borrow(), &[two, 999]);
        folds.sync(&mut editor);
        assert!(hidden(&editor, "body two") && hidden(&editor, "body sub"));
        assert!(!hidden(&editor, "body one"));
        assert_eq!(folds.offsets(&editor.document().borrow()), vec![two]);
        // A second sync without a change is free.
        folds.sync(&mut editor);
        assert!(hidden(&editor, "body two"));
    }

    #[test]
    fn chevron_fold_all_unfold_all_and_search_hits() {
        let mut editor = editor(TEXT);
        let mut folds = TabFolds::default();
        folds.sync(&mut editor);
        let one = TEXT.find("# One").unwrap();
        folds.toggle(&mut editor, one);
        assert!(hidden(&editor, "body one"));
        folds.toggle(&mut editor, one);
        assert!(!hidden(&editor, "body one"));
        // Not a heading: nothing folds.
        folds.toggle(&mut editor, one + 1);
        assert!(!hidden(&editor, "body one"));

        folds.fold_all(&mut editor);
        assert!(hidden(&editor, "body one") && hidden(&editor, "body sub"));
        let sub = TEXT.find("body sub").unwrap();
        assert!(folds.unfold_containing(&mut editor, TextRange::new(sub, sub + 4)));
        assert!(!hidden(&editor, "body sub") && hidden(&editor, "body one"));
        assert!(!folds.unfold_containing(&mut editor, TextRange::new(sub, sub + 4)));
        folds.unfold_all(&mut editor);
        assert!(!hidden(&editor, "body one"));
    }

    #[test]
    fn folds_follow_edits() {
        let mut editor = editor(TEXT);
        let mut folds = TabFolds::default();
        folds.sync(&mut editor);
        folds.toggle(&mut editor, TEXT.find("# Two").unwrap());
        assert!(hidden(&editor, "body two"));

        // Retitling the folded heading keeps it folded (same number of headings).
        edit(&editor, &TEXT.replace("# Two", "# Twice"));
        folds.sync(&mut editor);
        assert!(hidden(&editor, "body two"));

        // Inserting a heading above keeps the fold by title.
        let more = format!("# Zero\n\nzero\n\n{}", editor.document().borrow().text());
        edit(&editor, &more);
        folds.sync(&mut editor);
        assert!(hidden(&editor, "body two") && !hidden(&editor, "zero"));

        // Removing the heading drops its fold.
        edit(&editor, &more.replace("# Twice\n\n", ""));
        folds.sync(&mut editor);
        assert!(!hidden(&editor, "body two"));
        assert!(folds.offsets(&editor.document().borrow()).is_empty());
    }

    #[test]
    fn edits_without_folds_hide_nothing_and_a_later_fold_still_follows_a_retitle() {
        let mut editor = editor(TEXT);
        let mut folds = TabFolds::default();
        folds.sync(&mut editor);

        // Edits that add and remove headings while nothing is folded.
        let more = format!("# Zero\n\nzero\n\n{TEXT}");
        edit(&editor, &more);
        folds.sync(&mut editor);
        assert!(!hidden(&editor, "zero") && !hidden(&editor, "body two"));
        let fewer = more.replace("# Two\n\nbody two\n\n", "");
        edit(&editor, &fewer);
        folds.sync(&mut editor);
        assert!(!hidden(&editor, "body one") && !hidden(&editor, "body sub"));
        assert!(folds.offsets(&editor.document().borrow()).is_empty());

        // A fold made after those edits is recorded against the current headings: retitling
        // the folded heading keeps it folded.
        folds.toggle(&mut editor, fewer.find("# One").unwrap());
        assert!(hidden(&editor, "body one") && !hidden(&editor, "zero"));
        edit(&editor, &fewer.replace("# One", "# Uno"));
        folds.sync(&mut editor);
        assert!(hidden(&editor, "body one") && !hidden(&editor, "zero"));

        // Unfolding the last fold returns to the free path: later edits hide nothing.
        folds.unfold_all(&mut editor);
        assert!(!hidden(&editor, "body one"));
        edit(&editor, &fewer);
        folds.sync(&mut editor);
        assert!(!hidden(&editor, "body one"));
    }

    #[test]
    fn folds_attach_to_another_view_of_the_document() {
        let mut first = editor(TEXT);
        let mut folds = TabFolds::default();
        folds.sync(&mut first);
        folds.toggle(&mut first, TEXT.find("# Two").unwrap());

        let mut second = another_view(&first);
        assert!(!hidden(&second, "body two"));
        folds.attach(&mut second);
        assert!(hidden(&second, "body two") && hidden(&second, "body sub"));
        assert!(!hidden(&second, "body one"));

        // A retitle the folds have not seen yet is followed, as `sync` would.
        let two = TEXT.find("# Two").unwrap();
        edit(&first, &TEXT.replace("# Two", "# Twice"));
        let mut third = another_view(&first);
        folds.attach(&mut third);
        assert!(hidden(&third, "body two") && !hidden(&third, "body one"));
        assert_eq!(folds.offsets(&third.document().borrow()), vec![two]);
    }
}
