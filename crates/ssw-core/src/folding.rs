//! Heading folding (spec §2.8).
//!
//! A heading folds the text from the end of its line to the end of its section: the last
//! non-blank line before the next heading of the same or a higher level, or before the end of
//! the document ([`fold_ranges`]). Folded headings are remembered in a [`FoldSet`] by
//! heading text and occurrence number rather than by offset, so the state survives edits
//! anywhere in the document and can be stored per file path in the app database; after an
//! edit [`FoldSet::remap`] keeps folds attached to the heading they belong to even when the
//! heading was retitled or a heading with the same title was inserted above it.

use std::collections::BTreeSet;

use crate::edit::Edit;
use crate::text::TextRange;
use crate::toc::Toc;

/// The foldable region of one heading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldRange {
    /// Index into [`Toc::entries`].
    pub heading: usize,
    /// Hidden bytes: from the end of the heading to the end of the section's last non-blank
    /// line (trailing spaces of that line included, its line break excluded).
    pub range: TextRange,
}

/// Fold ranges of every heading that has content below it, in source order. Nested sections
/// lie inside the range of their parent.
pub fn fold_ranges(toc: &Toc, text: &str) -> Vec<FoldRange> {
    let entries = toc.entries();
    entries
        .iter()
        .enumerate()
        .filter_map(|(heading, entry)| {
            let start = entry.range.end;
            let limit = toc
                .section_end(heading)
                .map_or(text.len(), |next| entries[next].range.start);
            let region = &text[start..limit];
            let content = region.trim_end();
            if content.is_empty() {
                return None;
            }
            let end = region[content.len()..]
                .find('\n')
                .map_or(limit, |i| start + content.len() + i);
            Some(FoldRange {
                heading,
                range: TextRange::new(start, end),
            })
        })
        .collect()
}

/// Identity of a heading across edits: its title and how many headings with the same title
/// precede it (0 for the first).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FoldKey {
    pub title: String,
    pub occurrence: usize,
}

/// Key of entry `heading`; `None` when the index is out of range.
pub fn fold_key(toc: &Toc, heading: usize) -> Option<FoldKey> {
    (heading < toc.entries().len()).then(|| key_at(toc, heading))
}

/// Key of entry `heading` (which must exist).
fn key_at(toc: &Toc, heading: usize) -> FoldKey {
    let title = &toc.entries()[heading].title;
    let occurrence = toc.entries()[..heading]
        .iter()
        .filter(|e| &e.title == title)
        .count();
    FoldKey {
        title: title.clone(),
        occurrence,
    }
}

/// The entry `key` names in `toc`.
fn index_of(toc: &Toc, key: &FoldKey) -> Option<usize> {
    toc.entries()
        .iter()
        .enumerate()
        .filter(|(_, e)| e.title == key.title)
        .nth(key.occurrence)
        .map(|(i, _)| i)
}

/// The set of folded headings of one document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FoldSet {
    keys: BTreeSet<FoldKey>,
}

impl FoldSet {
    /// Restores a stored set.
    pub fn from_keys(keys: impl IntoIterator<Item = FoldKey>) -> Self {
        Self {
            keys: keys.into_iter().collect(),
        }
    }

    /// The folded headings, e.g. for persisting.
    pub fn keys(&self) -> impl Iterator<Item = &FoldKey> {
        self.keys.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn is_folded(&self, toc: &Toc, heading: usize) -> bool {
        fold_key(toc, heading).is_some_and(|key| self.keys.contains(&key))
    }

    /// Folds `heading`; no effect for an index outside the outline.
    pub fn fold(&mut self, toc: &Toc, heading: usize) {
        if let Some(key) = fold_key(toc, heading) {
            self.keys.insert(key);
        }
    }

    /// Unfolds `heading`; no effect for an index outside the outline.
    pub fn unfold(&mut self, toc: &Toc, heading: usize) {
        if let Some(key) = fold_key(toc, heading) {
            self.keys.remove(&key);
        }
    }

    /// Flips `heading` and returns whether it is folded afterwards (`false` for an index
    /// outside the outline).
    pub fn toggle(&mut self, toc: &Toc, heading: usize) -> bool {
        if self.is_folded(toc, heading) {
            self.unfold(toc, heading);
            false
        } else {
            self.fold(toc, heading);
            self.is_folded(toc, heading)
        }
    }

    /// View → Fold all: folds every heading that has a fold range.
    pub fn fold_all(&mut self, toc: &Toc, ranges: &[FoldRange]) {
        for r in ranges {
            self.fold(toc, r.heading);
        }
    }

    /// View → Unfold all.
    pub fn unfold_all(&mut self) {
        self.keys.clear();
    }

    /// Unfolds every folded heading whose hidden range contains `offset` (a caret or search
    /// hit inside a fold). An offset at the very start of the range is the end of the heading
    /// line and stays visible; the end of the range is hidden. Returns whether anything
    /// changed.
    pub fn unfold_containing(&mut self, toc: &Toc, ranges: &[FoldRange], offset: usize) -> bool {
        let mut changed = false;
        for r in ranges {
            if r.range.start < offset && offset <= r.range.end && self.is_folded(toc, r.heading) {
                self.unfold(toc, r.heading);
                changed = true;
            }
        }
        changed
    }

    /// Ranges currently hidden, in source order, without ranges nested in another hidden
    /// range.
    pub fn hidden_ranges(&self, toc: &Toc, ranges: &[FoldRange]) -> Vec<TextRange> {
        let mut hidden: Vec<TextRange> = Vec::new();
        for r in ranges {
            let inside_previous = hidden.last().is_some_and(|last| r.range.start < last.end);
            if !inside_previous && self.is_folded(toc, r.heading) {
                hidden.push(r.range);
            }
        }
        hidden
    }

    /// Carries the folds over a document change: `old` is the outline before `edits`, `new`
    /// the outline after (`edits` refer to the old text, sorted and disjoint). A fold follows
    /// its heading by position, so retitling a heading or inserting a same-titled heading above
    /// keeps the right heading folded. A fold whose heading was deleted, overwritten or turned
    /// into something else is dropped; keys that name no heading of `old` are kept untouched.
    pub fn remap(&mut self, old: &Toc, edits: &[Edit], new: &Toc) {
        let mut remapped = BTreeSet::new();
        for key in &self.keys {
            let Some(old_index) = index_of(old, key) else {
                remapped.insert(key.clone());
                continue;
            };
            let start = old.entries()[old_index].range.start;
            if let Some(mapped) = map_heading_start(edits, start)
                && let Ok(new_index) = new
                    .entries()
                    .binary_search_by_key(&mapped, |e| e.range.start)
            {
                remapped.insert(key_at(new, new_index));
            }
        }
        self.keys = remapped;
    }
}

/// Where the heading that started at `start` starts after `edits`; `None` when an edit
/// removed or overwrote its first byte.
fn map_heading_start(edits: &[Edit], start: usize) -> Option<usize> {
    let mut delta = 0isize;
    for edit in edits {
        let range = edit.range;
        if range.end <= start {
            delta += edit.delta();
        } else if range.start < start || (range.start == start && edit.text.is_empty()) {
            return None;
        } else {
            break;
        }
    }
    Some(start.saturating_add_signed(delta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parse_document;

    fn toc(src: &str) -> Toc {
        Toc::build(&parse_document(src), src)
    }

    fn key(title: &str, occurrence: usize) -> FoldKey {
        FoldKey {
            title: title.to_owned(),
            occurrence,
        }
    }

    fn hidden<'a>(src: &'a str, ranges: &[FoldRange]) -> Vec<&'a str> {
        ranges
            .iter()
            .map(|r| &src[r.range.start..r.range.end])
            .collect()
    }

    fn apply(text: &str, edits: &[Edit]) -> String {
        let mut out = String::new();
        let mut pos = 0;
        for e in edits {
            out.push_str(&text[pos..e.range.start]);
            out.push_str(&e.text);
            pos = e.range.end;
        }
        out.push_str(&text[pos..]);
        out
    }

    #[test]
    fn fold_ranges_end_before_the_next_heading_of_same_or_higher_level() {
        let src = "# A\n\nbody\n\n## B\nb body\n\n\n# C\n\n# D\ntext\n\n\n";
        let t = toc(src);
        let ranges = fold_ranges(&t, src);
        assert_eq!(
            ranges.iter().map(|r| r.heading).collect::<Vec<_>>(),
            [0, 1, 3]
        );
        assert_eq!(
            hidden(src, &ranges),
            ["\n\nbody\n\n## B\nb body", "\nb body", "\ntext"]
        );
        // C has nothing but blank lines below it.
        assert!(ranges.iter().all(|r| r.heading != 2));
    }

    #[test]
    fn lower_level_headings_do_not_end_a_section() {
        let src = "## A\n### B\nx\n#### C\ny\n## D\nz";
        let t = toc(src);
        let ranges = fold_ranges(&t, src);
        assert_eq!(
            hidden(src, &ranges),
            ["\n### B\nx\n#### C\ny", "\nx\n#### C\ny", "\ny", "\nz"]
        );
    }

    #[test]
    fn heading_without_any_following_text_is_not_foldable() {
        for src in ["# A", "# A\n", "# A\n\n  \n", "# A\n# B\n"] {
            let t = toc(src);
            let ranges = fold_ranges(&t, src);
            assert!(
                ranges.iter().all(|r| r.heading != 0),
                "{src:?} -> {ranges:?}"
            );
        }
        assert!(fold_ranges(&toc(""), "").is_empty());
        assert!(fold_ranges(&toc("no headings\n"), "no headings\n").is_empty());
    }

    #[test]
    fn range_keeps_trailing_spaces_of_the_last_line_but_not_blank_lines() {
        let src = "# A\ntext   \n\n   \n# B\n";
        let ranges = fold_ranges(&toc(src), src);
        assert_eq!(hidden(src, &ranges), ["\ntext   "]);
        let at_end = "# A\ntext   ";
        let ranges = fold_ranges(&toc(at_end), at_end);
        assert_eq!(hidden(at_end, &ranges), ["\ntext   "]);
        let multibyte = "# Ü\nçay ☕\n\n# B";
        let ranges = fold_ranges(&toc(multibyte), multibyte);
        assert_eq!(hidden(multibyte, &ranges), ["\nçay ☕"]);
    }

    #[test]
    fn closing_run_and_indentation_do_not_matter() {
        let src = "# A #\ntext\n  # B\nmore";
        let t = toc(src);
        let ranges = fold_ranges(&t, src);
        assert_eq!(hidden(src, &ranges), ["\ntext", "\nmore"]);
    }

    #[test]
    fn keys_count_same_titled_headings() {
        let t = toc("# A\n## B\n# A\n## A\n");
        assert_eq!(fold_key(&t, 0), Some(key("A", 0)));
        assert_eq!(fold_key(&t, 1), Some(key("B", 0)));
        assert_eq!(fold_key(&t, 2), Some(key("A", 1)));
        assert_eq!(fold_key(&t, 3), Some(key("A", 2)));
        assert_eq!(fold_key(&t, 4), None);
    }

    #[test]
    fn fold_unfold_toggle() {
        let t = toc("# A\n# A\n# B\n");
        let mut set = FoldSet::default();
        assert!(set.is_empty());
        set.fold(&t, 1);
        assert!(set.is_folded(&t, 1));
        assert!(!set.is_folded(&t, 0));
        assert!(!set.is_folded(&t, 9));
        assert_eq!(set.keys().cloned().collect::<Vec<_>>(), [key("A", 1)]);
        assert!(set.toggle(&t, 2));
        assert!(!set.toggle(&t, 2));
        assert!(!set.is_folded(&t, 2));
        set.unfold(&t, 1);
        assert!(set.is_empty());
        // Out-of-range indices change nothing.
        set.fold(&t, 7);
        set.unfold(&t, 7);
        assert!(!set.toggle(&t, 7));
        assert!(set.is_empty());
    }

    #[test]
    fn fold_all_and_unfold_all() {
        let src = "# A\ntext\n# Empty\n# B\n## C\nmore\n";
        let t = toc(src);
        let ranges = fold_ranges(&t, src);
        let mut set = FoldSet::default();
        set.fold_all(&t, &ranges);
        let folded: Vec<usize> = (0..4).filter(|&i| set.is_folded(&t, i)).collect();
        assert_eq!(folded, [0, 2, 3]);
        set.unfold_all();
        assert!(set.is_empty());
        assert_eq!(
            FoldSet::from_keys([key("A", 0), key("A", 0), key("Z", 3)])
                .keys()
                .count(),
            2
        );
    }

    #[test]
    fn hidden_ranges_skip_folds_nested_in_folds() {
        let src = "# A\ntext\n## B\nmore\n# C\nlast\n";
        let t = toc(src);
        let ranges = fold_ranges(&t, src);
        let mut set = FoldSet::default();
        set.fold(&t, 1);
        assert_eq!(set.hidden_ranges(&t, &ranges), [ranges[1].range]);
        set.fold(&t, 0);
        assert_eq!(set.hidden_ranges(&t, &ranges), [ranges[0].range]);
        set.fold(&t, 2);
        assert_eq!(
            set.hidden_ranges(&t, &ranges),
            [ranges[0].range, ranges[2].range]
        );
        set.unfold_all();
        assert!(set.hidden_ranges(&t, &ranges).is_empty());
    }

    #[test]
    fn unfold_containing_reveals_offsets_inside_hidden_text() {
        let src = "# A\ntext\n## B\nmore\n# C\nlast\n";
        let t = toc(src);
        let ranges = fold_ranges(&t, src);
        let a = ranges[0].range;
        let b = ranges[1].range;
        let mut set = FoldSet::default();
        set.fold_all(&t, &ranges);
        // At the end of the heading line the caret is visible.
        assert!(!set.unfold_containing(&t, &ranges, a.start));
        // Before the section nothing is hidden.
        assert!(!set.unfold_containing(&t, &ranges, 0));
        // Past the fold nothing is hidden.
        assert!(!set.unfold_containing(&t, &ranges, a.end + 1));
        // Inside B (and therefore inside A): both unfold.
        assert!(set.unfold_containing(&t, &ranges, b.start + 2));
        assert!(!set.is_folded(&t, 0));
        assert!(!set.is_folded(&t, 1));
        assert!(set.is_folded(&t, 2));
        // The end of the range counts as hidden; unfolded headings are not touched again.
        assert!(set.unfold_containing(&t, &ranges, ranges[2].range.end));
        assert!(!set.unfold_containing(&t, &ranges, ranges[2].range.end));
        assert!(set.is_empty());
    }

    fn remapped(before: &str, folded: &[(&str, usize)], edits: &[Edit]) -> (String, Vec<FoldKey>) {
        let old = toc(before);
        let after = apply(before, edits);
        let new = toc(&after);
        let mut set = FoldSet::from_keys(folded.iter().map(|&(t, n)| key(t, n)));
        set.remap(&old, edits, &new);
        (after, set.keys().cloned().collect())
    }

    #[test]
    fn remap_edit_below_and_above_keeps_the_fold() {
        let before = "# A\ntext\n# B\nmore\n";
        let (_, keys) = remapped(before, &[("A", 0)], &[Edit::insert(before.len(), "tail\n")]);
        assert_eq!(keys, [key("A", 0)]);
        let (after, keys) = remapped(before, &[("B", 0)], &[Edit::insert(0, "intro\n\n")]);
        assert!(after.starts_with("intro"));
        assert_eq!(keys, [key("B", 0)]);
        // Edit in a body between the headings shifts the second heading.
        let (_, keys) = remapped(
            before,
            &[("B", 0)],
            &[Edit::replace(TextRange::new(4, 8), "much longer text")],
        );
        assert_eq!(keys, [key("B", 0)]);
    }

    #[test]
    fn remap_follows_the_heading_when_a_same_titled_one_is_inserted_above() {
        let before = "# A\ntext\n";
        let (after, keys) = remapped(before, &[("A", 0)], &[Edit::insert(0, "# A\nnew\n")]);
        assert_eq!(after, "# A\nnew\n# A\ntext\n");
        assert_eq!(keys, [key("A", 1)]);
        // And back: deleting the first of two same-titled headings.
        let two = "# A\none\n# A\ntwo\n";
        let (_, keys) = remapped(two, &[("A", 1)], &[Edit::delete(TextRange::new(0, 8))]);
        assert_eq!(keys, [key("A", 0)]);
    }

    #[test]
    fn remap_survives_retitling() {
        let before = "# Old\nbody\n# Other\nx\n";
        let (after, keys) = remapped(
            before,
            &[("Old", 0), ("Other", 0)],
            &[Edit::replace(TextRange::new(2, 5), "New")],
        );
        assert_eq!(after, "# New\nbody\n# Other\nx\n");
        assert_eq!(keys, [key("New", 0), key("Other", 0)]);
        // Replacing the marker (a level change) keeps the fold too.
        let (_, keys) = remapped(
            before,
            &[("Old", 0)],
            &[Edit::replace(TextRange::new(0, 1), "###")],
        );
        assert_eq!(keys, [key("Old", 0)]);
    }

    #[test]
    fn remap_drops_deleted_or_overwritten_headings() {
        let before = "# A\none\n# B\ntwo\n";
        // Delete A's line and body: B moves to offset 0 but is not A.
        let (_, keys) = remapped(before, &[("A", 0)], &[Edit::delete(TextRange::new(0, 8))]);
        assert!(keys.is_empty());
        // Overwrite the heading start with plain text.
        let (_, keys) = remapped(
            before,
            &[("A", 0)],
            &[Edit::replace(TextRange::new(0, 3), "plain")],
        );
        assert!(keys.is_empty());
        // The first byte falls strictly inside a replaced range.
        let (_, keys) = remapped(
            before,
            &[("B", 0)],
            &[Edit::replace(TextRange::new(6, 14), "x\n")],
        );
        assert!(keys.is_empty());
        // An insertion at the heading start pushes the heading down; the new text is plain.
        let (after, keys) = remapped(before, &[("B", 0)], &[Edit::insert(8, "x\n")]);
        assert_eq!(after, "# A\none\nx\n# B\ntwo\n");
        assert_eq!(keys, [key("B", 0)]);
        // Typing in front of the marker destroys the heading.
        let (_, keys) = remapped(before, &[("B", 0)], &[Edit::insert(8, "x")]);
        assert!(keys.is_empty());
    }

    #[test]
    fn remap_keeps_keys_that_name_no_heading() {
        let before = "# A\nx\n";
        let (_, keys) = remapped(before, &[("Gone", 0), ("A", 0)], &[Edit::insert(0, "\n")]);
        assert_eq!(keys, [key("A", 0), key("Gone", 0)]);
    }

    #[test]
    fn remap_with_several_edits_accumulates_the_shift() {
        let before = "# A\nx\n# B\ny\n# C\nz\n";
        let edits = [
            Edit::insert(0, "pre\n"),
            Edit::replace(TextRange::new(4, 5), "xxxx"),
            Edit::delete(TextRange::new(10, 11)),
        ];
        let (after, keys) = remapped(before, &[("C", 0)], &edits);
        assert_eq!(after, "pre\n# A\nxxxx\n# B\n\n# C\nz\n");
        assert_eq!(keys, [key("C", 0)]);
        // The same edits plus the removal of C's marker: the fold is dropped.
        let edits = [
            Edit::insert(0, "pre\n"),
            Edit::delete(TextRange::new(12, 13)),
        ];
        let (_, keys) = remapped(before, &[("C", 0)], &edits);
        assert!(keys.is_empty());
    }
}
