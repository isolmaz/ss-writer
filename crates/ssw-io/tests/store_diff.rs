//! Line diff for the history panel: known cases, hunk shape and a minimality / round-trip
//! check against a reference LCS on pseudo-random inputs.

use ssw_io::diff::{
    DEFAULT_CONTEXT, DiffLine, Hunk, LineOp, MAX_EDIT_DISTANCE, diff_lines, diff_lines_limited,
};

fn line(op: LineOp, old: Option<usize>, new: Option<usize>, text: &str) -> DiffLine {
    DiffLine {
        op,
        old_line: old,
        new_line: new,
        text: text.to_owned(),
    }
}

fn numbered(count: usize, replace: &[(usize, &str)]) -> String {
    (1..=count)
        .map(|n| match replace.iter().find(|(at, _)| *at == n) {
            Some((_, text)) => format!("{text}\n"),
            None => format!("l{n}\n"),
        })
        .collect()
}

#[test]
fn equal_texts_have_no_hunks() {
    assert!(diff_lines("", "", 3).is_empty());
    assert!(diff_lines("a\nb\n", "a\nb\n", 3).is_empty());
    assert!(diff_lines("no newline", "no newline", 0).is_empty());
}

#[test]
fn a_deleted_line_is_one_hunk_with_context() {
    let hunks = diff_lines("A\nB\nC\n", "A\nC\n", DEFAULT_CONTEXT);
    assert_eq!(
        hunks,
        vec![Hunk {
            old_start: 0,
            old_count: 3,
            new_start: 0,
            new_count: 2,
            lines: vec![
                line(LineOp::Equal, Some(0), Some(0), "A"),
                line(LineOp::Delete, Some(1), None, "B"),
                line(LineOp::Equal, Some(2), Some(1), "C"),
            ],
        }]
    );
}

#[test]
fn an_appended_line_keeps_only_the_requested_context() {
    let hunks = diff_lines("a\nb\n", "a\nb\nc\n", 1);
    assert_eq!(
        hunks,
        vec![Hunk {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
            lines: vec![
                line(LineOp::Equal, Some(1), Some(1), "b"),
                line(LineOp::Insert, None, Some(2), "c"),
            ],
        }]
    );
}

#[test]
fn pure_insertion_without_context_is_anchored_at_the_insertion_point() {
    let hunks = diff_lines("a\nb\n", "a\nX\nb\n", 0);
    assert_eq!(
        hunks,
        vec![Hunk {
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            lines: vec![line(LineOp::Insert, None, Some(1), "X")],
        }]
    );
}

#[test]
fn a_missing_final_newline_is_a_change() {
    let hunks = diff_lines("a\nb", "a\nb\n", 3);
    assert_eq!(
        hunks,
        vec![Hunk {
            old_start: 0,
            old_count: 2,
            new_start: 0,
            new_count: 2,
            lines: vec![
                line(LineOp::Equal, Some(0), Some(0), "a"),
                line(LineOp::Delete, Some(1), None, "b"),
                line(LineOp::Insert, None, Some(1), "b"),
            ],
        }]
    );
}

#[test]
fn carriage_returns_stay_part_of_the_line_text() {
    let hunks = diff_lines("a\r\nb\r\n", "a\r\nc\r\n", 0);
    assert_eq!(
        hunks[0].lines,
        vec![
            line(LineOp::Delete, Some(1), None, "b\r"),
            line(LineOp::Insert, None, Some(1), "c\r"),
        ]
    );
}

#[test]
fn distant_changes_make_separate_hunks_and_near_changes_merge() {
    let old = numbered(20, &[]);
    let new = numbered(20, &[(3, "X3"), (18, "X18")]);
    let hunks = diff_lines(&old, &new, 3);
    assert_eq!(hunks.len(), 2);
    assert_eq!((hunks[0].old_start, hunks[0].old_count), (0, 6));
    assert_eq!((hunks[0].new_start, hunks[0].new_count), (0, 6));
    assert_eq!(
        hunks[0]
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>(),
        ["l1", "l2", "l3", "X3", "l4", "l5", "l6"]
    );
    assert_eq!((hunks[1].old_start, hunks[1].old_count), (14, 6));
    assert_eq!((hunks[1].new_start, hunks[1].new_count), (14, 6));
    assert_eq!(
        hunks[1]
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>(),
        ["l15", "l16", "l17", "l18", "X18", "l19", "l20"]
    );

    // Context 0 keeps just the changed lines of each place.
    let tight = diff_lines(&old, &new, 0);
    assert_eq!(tight.len(), 2);
    assert_eq!(
        tight[0].lines,
        vec![
            line(LineOp::Delete, Some(2), None, "l3"),
            line(LineOp::Insert, None, Some(2), "X3"),
        ]
    );
    assert_eq!(
        (tight[0].old_start, tight[0].old_count, tight[0].new_count),
        (2, 1, 1)
    );

    // Changes at lines 3 and 7 (three unchanged lines between) share one hunk at context 2.
    let near = diff_lines(&old, &numbered(20, &[(3, "X3"), (7, "X7")]), 2);
    assert_eq!(near.len(), 1);
}

#[test]
fn changes_at_the_edges_do_not_read_outside_the_texts() {
    let hunks = diff_lines("a\nb\nc\n", "X\nb\nY\n", 5);
    assert_eq!(hunks.len(), 1);
    assert_eq!((hunks[0].old_start, hunks[0].old_count), (0, 3));
    assert_eq!((hunks[0].new_start, hunks[0].new_count), (0, 3));
    // Both ends are changes, the middle line is the one kept.
    assert_ne!(hunks[0].lines.first().map(|l| l.op), Some(LineOp::Equal));
    assert_ne!(hunks[0].lines.last().map(|l| l.op), Some(LineOp::Equal));
    assert_eq!(
        hunks[0]
            .lines
            .iter()
            .filter(|l| l.op == LineOp::Equal)
            .count(),
        1
    );
}

#[test]
fn rewriting_everything_is_all_deletions_then_insertions() {
    let hunks = diff_lines("a\nb\n", "x\ny\nz\n", 3);
    assert_eq!(hunks.len(), 1);
    let ops: Vec<LineOp> = hunks[0].lines.iter().map(|l| l.op).collect();
    assert_eq!(ops.iter().filter(|op| **op == LineOp::Delete).count(), 2);
    assert_eq!(ops.iter().filter(|op| **op == LineOp::Insert).count(), 3);
    assert!(!ops.contains(&LineOp::Equal));
}

#[test]
fn beyond_the_distance_limit_the_middle_is_replaced_wholesale() {
    let old = "a\nb\nc\nd\n";
    let new = "b\nc\nd\ne\n";
    let ops = |hunks: &[Hunk]| -> Vec<LineOp> {
        hunks
            .iter()
            .flat_map(|h| h.lines.iter().map(|l| l.op))
            .collect()
    };
    // The minimal script removes `a` and adds `e`.
    assert_eq!(
        ops(&diff_lines(old, new, 3)),
        [
            LineOp::Delete,
            LineOp::Equal,
            LineOp::Equal,
            LineOp::Equal,
            LineOp::Insert
        ]
    );
    // With a search bound of 1 the changed middle (everything here) is deleted and re-added.
    assert_eq!(
        ops(&diff_lines_limited(old, new, 3, 1)),
        [
            LineOp::Delete,
            LineOp::Delete,
            LineOp::Delete,
            LineOp::Delete,
            LineOp::Insert,
            LineOp::Insert,
            LineOp::Insert,
            LineOp::Insert,
        ]
    );
    assert_eq!(MAX_EDIT_DISTANCE, 2000);
}

/// Deterministic pseudo-random numbers.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound
    }

    fn text(&mut self, max_lines: u64, alphabet: u64) -> String {
        let count = self.next(max_lines + 1);
        (0..count)
            .map(|_| format!("{}\n", (b'a' + self.next(alphabet) as u8) as char))
            .collect()
    }
}

/// Length of the longest common subsequence of two line lists.
fn lcs(a: &[&str], b: &[&str]) -> usize {
    // One DP row: row[k] = LCS of the lines of `a` seen so far and the first k lines of `b`.
    let mut row = vec![0usize; b.len() + 1];
    for x in a {
        let mut diagonal = 0;
        for (j, y) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if x == y {
                diagonal + 1
            } else {
                above.max(row[j])
            };
            diagonal = above;
        }
    }
    row[b.len()]
}

/// Apply `hunks` to the lines of `old`.
fn apply(old: &str, hunks: &[Hunk]) -> String {
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut at = 0;
    for hunk in hunks {
        for kept in &old_lines[at..hunk.old_start] {
            out.push_str(kept);
        }
        for l in &hunk.lines {
            if l.op != LineOp::Delete {
                out.push_str(&l.text);
                out.push('\n');
            }
        }
        at = hunk.old_start + hunk.old_count;
    }
    for rest in &old_lines[at..] {
        out.push_str(rest);
    }
    out
}

#[test]
fn diffs_are_minimal_and_reproduce_the_new_text() {
    let mut rng = Lcg(0x5EED);
    for round in 0..400 {
        let old = rng.text(12, 3);
        let new = if round % 3 == 0 {
            rng.text(12, 3)
        } else {
            // A lightly edited copy: drop or replace some lines.
            old.lines()
                .filter_map(|l| match rng.next(5) {
                    0 => None,
                    1 => Some(format!("{l}{l}\n")),
                    _ => Some(format!("{l}\n")),
                })
                .collect()
        };
        let a: Vec<&str> = old.split_inclusive('\n').collect();
        let b: Vec<&str> = new.split_inclusive('\n').collect();
        for context in [0, 1, 3] {
            let hunks = diff_lines(&old, &new, context);
            assert_eq!(
                apply(&old, &hunks),
                new,
                "round {round}, context {context}\n{old:?}\n{new:?}"
            );
            let (deleted, inserted) =
                hunks
                    .iter()
                    .flat_map(|h| &h.lines)
                    .fold((0, 0), |(d, i), l| match l.op {
                        LineOp::Delete => (d + 1, i),
                        LineOp::Insert => (d, i + 1),
                        LineOp::Equal => (d, i),
                    });
            let common = lcs(&a, &b);
            assert_eq!(deleted, a.len() - common, "round {round}: not minimal");
            assert_eq!(inserted, b.len() - common, "round {round}: not minimal");
        }
    }
}
