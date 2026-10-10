//! Replacing a document's text with new text from disk while the caret stays where the reader
//! was: a line diff turns the new text into small edits, and the selections map through them.

use ssw_core::document::Document;
use ssw_core::edit::{Edit, EditError, Transaction};
use ssw_core::text::TextRange;
use ssw_io::diff::diff_lines;

/// Byte offset of the start of every line, plus the text length as the last entry.
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        starts.push(offset);
        offset += line.len();
    }
    starts.push(offset);
    starts
}

/// The edits that turn `old` into `new`, one per changed run of lines, in `old` coordinates.
#[must_use]
pub fn line_edits(old: &str, new: &str) -> Vec<Edit> {
    let old_starts = line_starts(old);
    let new_starts = line_starts(new);
    diff_lines(old, new, 0)
        .iter()
        .map(|hunk| {
            let old_range = TextRange::new(
                old_starts[hunk.old_start],
                old_starts[hunk.old_start + hunk.old_count],
            );
            let inserted =
                &new[new_starts[hunk.new_start]..new_starts[hunk.new_start + hunk.new_count]];
            Edit::replace(old_range, inserted)
        })
        .collect()
}

/// Makes the document hold `new_text` through one undoable transaction of line edits. The
/// selections follow the edits (a caret inside replaced lines moves to the edge of the new
/// text). Returns whether the text changed.
///
/// # Errors
/// [`EditError`] when the document rejects the transaction (never for the edits built here).
pub fn replace_text(document: &mut Document, new_text: &str) -> Result<bool, EditError> {
    let old_text = document.text();
    let edits = line_edits(&old_text, new_text);
    if edits.is_empty() {
        return Ok(false);
    }
    let before = document.primary_selection();
    let mut transaction = Transaction::new(edits, before, before);
    transaction.selection_after = before.map_through(&transaction).clamped(new_text.len());
    document.apply(transaction)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use ssw_core::edit::Selection;

    use super::*;

    fn apply(old: &str, new: &str) -> String {
        let mut result = old.to_owned();
        let mut edits = line_edits(old, new);
        edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        for edit in edits {
            result.replace_range(edit.range.as_range(), &edit.text);
        }
        result
    }

    #[test]
    fn line_edits_reproduce_the_new_text() {
        let cases = [
            ("", ""),
            ("", "a\n"),
            ("a\n", ""),
            ("a\nb\nc\n", "a\nB\nc\n"),
            ("a\nb\nc\n", "a\nc\n"),
            ("a\nc\n", "a\nb\nc\n"),
            ("a\nb\nc", "a\nb\nc\n"),
            ("a\nb\nc\n", "a\nb\nc"),
            (
                "one\ntwo\nthree\nfour\nfive\n",
                "zero\none\nthree\nfour\n5\nsix\n",
            ),
            ("same\n", "same\n"),
            ("héllo\nwörld\n", "héllo\nwörld!\n"),
        ];
        for (old, new) in cases {
            assert_eq!(apply(old, new), new, "{old:?} -> {new:?}");
        }
        assert!(line_edits("same\n", "same\n").is_empty());
    }

    #[test]
    fn only_changed_lines_are_edited() {
        let edits = line_edits("a\nb\nc\nd\n", "a\nB\nc\nd\nE\n");
        assert_eq!(
            edits,
            [
                Edit::replace(TextRange::new(2, 4), "B\n"),
                Edit::replace(TextRange::new(8, 8), "E\n"),
            ]
        );
    }

    #[test]
    fn the_caret_keeps_its_place_across_an_external_edit() {
        let mut document = Document::new("alpha\nbeta\ngamma\n");
        // Caret in "gamma", after the 'm'.
        let caret = "alpha\nbeta\nga".len() + 1;
        document.set_selections(Selection::caret(caret), &[]);
        let changed = replace_text(
            &mut document,
            "intro line\nalpha\nBETA!\nbeta two\ngamma\nomega\n",
        )
        .unwrap();
        assert!(changed);
        assert_eq!(
            document.text(),
            "intro line\nalpha\nBETA!\nbeta two\ngamma\nomega\n"
        );
        let expected = "intro line\nalpha\nBETA!\nbeta two\nga".len() + 1;
        assert_eq!(document.primary_selection(), Selection::caret(expected));
    }

    #[test]
    fn the_reload_is_one_undo_step() {
        let mut document = Document::new("a\nb\n");
        assert!(replace_text(&mut document, "a\nx\ny\n").unwrap());
        assert_eq!(document.text(), "a\nx\ny\n");
        assert!(document.undo().unwrap());
        assert_eq!(document.text(), "a\nb\n");
    }

    #[test]
    fn identical_text_changes_nothing() {
        let mut document = Document::new("same\n");
        let version = document.version();
        assert!(!replace_text(&mut document, "same\n").unwrap());
        assert_eq!(document.version(), version);
        assert!(!document.can_undo());
    }

    #[test]
    fn a_caret_after_removed_text_is_clamped() {
        let mut document = Document::new("keep\nremove me\n");
        document.set_selections(Selection::caret(document.text().len()), &[]);
        assert!(replace_text(&mut document, "keep\n").unwrap());
        assert_eq!(document.primary_selection(), Selection::caret(5));
    }
}
