//! Task lists: toggling checkboxes and keeping completed items at the end of their list.

use crate::edit::{Edit, Selection, Transaction};
use crate::markdown::ast::{Block, BlockKind};
use crate::text::TextRange;

use super::Ctx;
use super::lists::{ItemRef, all_items, finish};

fn is_checked(item: &Block) -> bool {
    matches!(item.kind, BlockKind::ListItem { task: Some(t), .. } if t.checked)
}

fn box_text(checked: bool) -> &'static str {
    if checked { "[x]" } else { "[ ]" }
}

/// The items of one list after reordering: output slot `s` holds the item `order[s]`; the text
/// between items stays where it is. Ordered lists keep the marker of each slot, so the numbers
/// stay in sequence.
struct Reorder {
    /// Source range from the first item's start to the last item's end.
    span: TextRange,
    /// Source range of every item in source order.
    ranges: Vec<TextRange>,
    /// Marker range of every item in source order.
    markers: Vec<TextRange>,
    ordered: bool,
    order: Vec<usize>,
}

impl Reorder {
    /// Length of item `k` when it is placed in `slot`.
    fn placed_len(&self, slot: usize, k: usize) -> usize {
        if self.ordered {
            self.ranges[k].len() - self.markers[k].len() + self.markers[slot].len()
        } else {
            self.ranges[k].len()
        }
    }

    /// The reordered text of the span, with the checkboxes of `flips` already changed.
    fn text(&self, ctx: Ctx<'_>, flips: &[(TextRange, bool)]) -> String {
        let mut out = String::new();
        for (slot, &k) in self.order.iter().enumerate() {
            let range = self.ranges[k];
            let mut chunk = ctx.slice(range).into_owned();
            for (checkbox, checked) in flips.iter().rev() {
                if range.contains_range(*checkbox) {
                    let at = checkbox.start - range.start;
                    chunk.replace_range(at..at + checkbox.len(), box_text(*checked));
                }
            }
            if self.ordered {
                let marker = self.markers[k];
                let at = marker.start - range.start;
                chunk.replace_range(at..at + marker.len(), &ctx.slice(self.markers[slot]));
            }
            out.push_str(&chunk);
            if let Some(next) = self.ranges.get(slot + 1) {
                out.push_str(&ctx.slice(TextRange::new(self.ranges[slot].end, next.start)));
            }
        }
        out
    }

    /// Where an offset of the old text ends up: inside an item it moves with the item, in the
    /// text between items it stays between the same slots.
    fn map(&self, offset: usize) -> usize {
        if !self.span.touches(offset) {
            return offset;
        }
        let slot_start = |slot: usize| {
            self.span.start
                + (0..slot)
                    .map(|s| {
                        let gap = self.ranges[s + 1].start - self.ranges[s].end;
                        self.placed_len(s, self.order[s]) + gap
                    })
                    .sum::<usize>()
        };
        if let Some((k, range)) = self
            .ranges
            .iter()
            .enumerate()
            .find(|(_, r)| r.touches(offset))
        {
            let slot = self.order.iter().position(|&o| o == k).unwrap_or(k);
            let rel = offset - range.start;
            let rel = if !self.ordered {
                rel
            } else if rel >= self.markers[k].len() {
                rel - self.markers[k].len() + self.markers[slot].len()
            } else {
                rel.min(self.markers[slot].len())
            };
            return slot_start(slot) + rel;
        }
        let gap_after = self
            .ranges
            .iter()
            .rposition(|r| r.end < offset)
            .unwrap_or_default();
        slot_start(gap_after)
            + self.placed_len(gap_after, self.order[gap_after])
            + (offset - self.ranges[gap_after].end)
    }
}

/// Toggles the task items at the caret or in the selection: all become checked, unless they
/// already all are. With `sort_completed`, a completed item (with its children) moves to the end
/// of its list and an item that is reopened moves before the first completed sibling. `None`
/// when no task item is selected.
pub fn toggle_task(ctx: Ctx<'_>, sort_completed: bool) -> Option<Transaction> {
    let items = all_items(ctx.ast);
    let (first, last) = ctx.selected_lines();
    let mut selected: Vec<ItemRef<'_>> = items
        .iter()
        .filter(|i| {
            i.checkbox().is_some() && (first..=last).contains(&ctx.line_of(i.marker().start))
        })
        .copied()
        .collect();
    if selected.is_empty() {
        let around = items.iter().rev().find(|i| {
            let lines = ctx.line_of(i.item.range.start)..=ctx.line_of(i.item.range.end);
            i.checkbox().is_some() && lines.contains(&first)
        });
        selected.extend(around.copied());
    }
    if selected.is_empty() {
        return None;
    }
    let target = !selected.iter().all(|i| is_checked(i.item));
    let flipped: Vec<ItemRef<'_>> = selected
        .into_iter()
        .filter(|i| is_checked(i.item) != target)
        .collect();
    let flips: Vec<(TextRange, bool)> = flipped
        .iter()
        .filter_map(|i| i.checkbox().map(|t| (t.checkbox, target)))
        .collect();
    let reorders = if sort_completed {
        reorders_for(&flipped, target)
    } else {
        Vec::new()
    };

    let mut edits: Vec<Edit> = reorders
        .iter()
        .map(|r| Edit::replace(r.span, r.text(ctx, &flips)))
        .collect();
    for (checkbox, checked) in &flips {
        if !reorders.iter().any(|r| r.span.contains_range(*checkbox)) {
            edits.push(Edit::replace(*checkbox, box_text(*checked)));
        }
    }
    edits.sort_by_key(|e| e.range.start);
    let map = |offset: usize| {
        reorders
            .iter()
            .find(|r| r.span.touches(offset))
            .map_or(offset, |r| r.map(offset))
    };
    let after = Selection::new(map(ctx.selection.anchor), map(ctx.selection.head));
    Some(finish(ctx, edits, after))
}

/// Works out the new item order of every list holding a flipped item. Lists nested inside the
/// span of an already reordered list are not reordered on their own.
fn reorders_for(flipped: &[ItemRef<'_>], target: bool) -> Vec<Reorder> {
    let mut lists: Vec<&Block> = Vec::new();
    for item in flipped {
        if !lists.iter().any(|l| std::ptr::eq(*l, item.list)) {
            lists.push(item.list);
        }
    }
    let mut reorders: Vec<Reorder> = Vec::new();
    for list in lists {
        let mut checked: Vec<bool> = list.children.iter().map(is_checked).collect();
        let mut order: Vec<usize> = (0..list.children.len()).collect();
        for item in flipped.iter().filter(|i| std::ptr::eq(i.list, list)) {
            checked[item.index] = target;
            let old = order
                .iter()
                .position(|&k| k == item.index)
                .unwrap_or_default();
            order.remove(old);
            let new = if target {
                order.len()
            } else {
                order.iter().position(|&k| checked[k]).unwrap_or(old)
            };
            order.insert(new, item.index);
        }
        if order.iter().enumerate().any(|(slot, &k)| slot != k) {
            let ranges: Vec<TextRange> = list.children.iter().map(|c| c.range).collect();
            let span = TextRange::new(ranges[0].start, ranges[ranges.len() - 1].end);
            reorders.push(Reorder {
                span,
                ranges,
                markers: list.children.iter().map(|c| c.markers[0]).collect(),
                ordered: matches!(list.kind, BlockKind::List { ordered: true, .. }),
                order,
            });
        }
    }
    reorders.sort_by_key(|r| r.span.start);
    let mut accepted: Vec<Reorder> = Vec::new();
    for reorder in reorders {
        let nested = accepted
            .last()
            .is_some_and(|prev| prev.span.contains_range(reorder.span));
        if !nested {
            accepted.push(reorder);
        }
    }
    accepted
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;

    fn toggle(sort: bool) -> impl FnOnce(Ctx<'_>) -> Option<Transaction> {
        move |c| toggle_task(c, sort)
    }

    #[test]
    fn toggles_the_task_at_the_caret() {
        for (before, after) in [
            ("- [ ] a‸\n- [ ] b", "- [x] a‸\n- [ ] b"),
            ("- [x] a‸", "- [ ] a‸"),
            ("- [X] a‸", "- [ ] a‸"),
            ("- [ ] a\n  more‸", "- [x] a\n  more‸"),
            ("- [ ] a\n  - [ ] b‸", "- [ ] a\n  - [x] b‸"),
            ("- ‸[ ] a", "- ‸[x] a"),
        ] {
            check(before, after, toggle(false));
        }
    }

    #[test]
    fn toggles_every_selected_task() {
        check(
            "«- [ ] a\n- [x] b\n- [ ] c»",
            "«- [x] a\n- [x] b\n- [x] c»",
            toggle(false),
        );
        check("«- [x] a\n- [x] b»", "«- [ ] a\n- [ ] b»", toggle(false));
        // Non-task items in the selection are left alone.
        check("«- [ ] a\n- b»", "«- [x] a\n- b»", toggle(false));
    }

    #[test]
    fn nothing_to_toggle_without_a_task() {
        check_none("- a‸", toggle(false));
        check_none("plain‸", toggle(true));
    }

    #[test]
    fn completed_task_moves_to_the_end() {
        check(
            "- [ ] a‸\n- [ ] b\n- [ ] c",
            "- [ ] b\n- [ ] c\n- [x] a‸",
            toggle(true),
        );
        check(
            "- [ ] a\n- [ ] b‸\n- [ ] c",
            "- [ ] a\n- [ ] c\n- [x] b‸",
            toggle(true),
        );
        check("- [ ] a\n- [ ] b‸", "- [ ] a\n- [x] b‸", toggle(true));
        check("- [ ] a‸\n- [x] b", "- [x] b\n- [x] a‸", toggle(true));
    }

    #[test]
    fn reopened_task_moves_before_the_first_completed_sibling() {
        check(
            "- [ ] a\n- [x] b\n- [x] c‸",
            "- [ ] a\n- [ ] c‸\n- [x] b",
            toggle(true),
        );
        // No other completed sibling: stays in place.
        check("- [ ] a\n- [x] b‸", "- [ ] a\n- [ ] b‸", toggle(true));
        check("- [x] a‸\n- [ ] b", "- [ ] a‸\n- [ ] b", toggle(true));
    }

    #[test]
    fn children_move_with_their_item() {
        check(
            "- [ ] a‸\n  - [ ] a1\n- [ ] b",
            "- [ ] b\n- [x] a‸\n  - [ ] a1",
            toggle(true),
        );
        check(
            "- [ ] a\n  - [ ] a1‸\n  - [ ] a2\n- [ ] b",
            "- [ ] a\n  - [ ] a2\n  - [x] a1‸\n- [ ] b",
            toggle(true),
        );
        // A lone nested task just flips.
        check("- [ ] a\n  - [ ] a1‸", "- [ ] a\n  - [x] a1‸", toggle(true));
    }

    #[test]
    fn selected_tasks_are_sorted_together() {
        check(
            "«- [ ] a\n- [x] b\n- [ ] c»",
            "- [x] b\n«- [x] a\n- [x] c»",
            toggle(true),
        );
        check("«- [x] a\n- [x] b»", "«- [ ] a\n- [ ] b»", toggle(true));
        // Both complete: a moves behind b, then b behind a, so the order is unchanged.
        check("«- [ ] a\n\n- [ ] b»", "«- [x] a\n\n- [x] b»", toggle(true));
    }

    #[test]
    fn separators_stay_in_place() {
        check("- [ ] a‸\n\n- [ ] b", "- [ ] b\n\n- [x] a‸", toggle(true));
        check(
            "> - [ ] a‸\n> - [ ] b",
            "> - [ ] b\n> - [x] a‸",
            toggle(true),
        );
    }

    #[test]
    fn ordered_task_lists_keep_their_numbers_in_sequence() {
        check("1. [ ] a‸\n2. [ ] b", "1. [ ] b\n2. [x] a‸", toggle(true));
        check("9. [ ] a‸\n10. [ ] b", "9. [ ] b\n10. [x] a‸", toggle(true));
    }

    #[test]
    fn sorting_off_only_flips() {
        check(
            "- [ ] a‸\n- [ ] b\n- [ ] c",
            "- [x] a‸\n- [ ] b\n- [ ] c",
            toggle(false),
        );
    }

    #[test]
    fn offsets_outside_the_reordered_list_do_not_move() {
        check(
            "«para\n\n- [ ] a»\n- [ ] b\n- [ ] c",
            "«para\n\n- [ ] b\n- [ ] c\n- [x] a»",
            toggle(true),
        );
    }

    #[test]
    fn caret_inside_a_marker_of_an_ordered_item_stays_inside_the_slot_marker() {
        check("1‸. [ ] a\n2. [ ] b", "1. [ ] b\n2‸. [x] a", toggle(true));
    }

    #[test]
    fn selection_in_the_gap_between_items_stays_between_the_same_slots() {
        // The selection start moves with item a, its end stays in the gap behind slot 0.
        check("«- [ ] a\n»\n- [ ] b", "- [ ] b\n»\n«- [x] a", toggle(true));
    }
}
