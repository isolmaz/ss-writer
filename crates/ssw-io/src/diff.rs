//! Line diff for the history panel: Myers' O(ND) algorithm producing unified-diff style hunks.
//!
//! Lines are split after `\n`; a final line without a terminator differs from the same line with
//! one. Line numbers in the results are 0-based.

use std::collections::HashMap;

/// Context lines kept around every change by default.
pub const DEFAULT_CONTEXT: usize = 3;

/// Largest edit distance the minimal algorithm is run for. Beyond it the changed middle part of
/// the two texts is reported as one deletion followed by one insertion (still a correct diff,
/// just not minimal); this bounds time and memory for wholesale rewrites of huge files.
pub const MAX_EDIT_DISTANCE: usize = 2000;

/// What happened to a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineOp {
    Equal,
    Insert,
    Delete,
}

/// One line of a hunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub op: LineOp,
    /// Line index in the old text (`Equal`, `Delete`).
    pub old_line: Option<usize>,
    /// Line index in the new text (`Equal`, `Insert`).
    pub new_line: Option<usize>,
    /// The line without its `\n`.
    pub text: String,
}

/// A group of changes with their context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// Index of the first old line covered (or the insertion point when `old_count` is 0).
    pub old_start: usize,
    /// Number of old lines covered (`Equal` + `Delete`).
    pub old_count: usize,
    pub new_start: usize,
    /// Number of new lines covered (`Equal` + `Insert`).
    pub new_count: usize,
    pub lines: Vec<DiffLine>,
}

/// Hunks turning `old` into `new`, each with up to `context` unchanged lines around the
/// changes; hunks whose context would overlap are merged. Empty when the texts are equal.
#[must_use]
pub fn diff_lines(old: &str, new: &str, context: usize) -> Vec<Hunk> {
    diff_lines_limited(old, new, context, MAX_EDIT_DISTANCE)
}

/// [`diff_lines`] with an explicit bound on the edit distance searched (see
/// [`MAX_EDIT_DISTANCE`]).
#[must_use]
pub fn diff_lines_limited(old: &str, new: &str, context: usize, max_distance: usize) -> Vec<Hunk> {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let ops = edit_script(&a, &b, max_distance);
    hunks(&ops, &a, &b, context)
}

/// One step of the edit script with the line positions it happens at.
struct Step {
    op: LineOp,
    old: usize,
    new: usize,
}

fn edit_script(a: &[&str], b: &[&str], max_distance: usize) -> Vec<Step> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let a_mid = &a[prefix..a.len() - suffix];
    let b_mid = &b[prefix..b.len() - suffix];

    let mut ids: HashMap<&str, u32> = HashMap::new();
    let a_ids: Vec<u32> = a_mid.iter().map(|line| intern(&mut ids, line)).collect();
    let b_ids: Vec<u32> = b_mid.iter().map(|line| intern(&mut ids, line)).collect();

    let mid = myers(&a_ids, &b_ids, max_distance).unwrap_or_else(|| {
        let mut coarse = vec![LineOp::Delete; a_mid.len()];
        coarse.resize(a_mid.len() + b_mid.len(), LineOp::Insert);
        coarse
    });

    let mut steps = Vec::with_capacity(prefix + mid.len() + suffix);
    let (mut old, mut new) = (0, 0);
    let all = std::iter::repeat_n(LineOp::Equal, prefix)
        .chain(mid)
        .chain(std::iter::repeat_n(LineOp::Equal, suffix));
    for op in all {
        steps.push(Step { op, old, new });
        match op {
            LineOp::Equal => {
                old += 1;
                new += 1;
            }
            LineOp::Delete => old += 1,
            LineOp::Insert => new += 1,
        }
    }
    steps
}

/// Small integer id of `line`, equal for equal lines (so the diff compares integers).
fn intern<'a>(ids: &mut HashMap<&'a str, u32>, line: &'a str) -> u32 {
    let next = ids.len() as u32;
    *ids.entry(line).or_insert(next)
}

/// Myers' greedy shortest edit script between `a` and `b`; `None` when it needs more than
/// `max_distance` insertions plus deletions.
fn myers(a: &[u32], b: &[u32], max_distance: usize) -> Option<Vec<LineOp>> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let bound = (a.len() + b.len()).min(max_distance) as isize;
    let off = bound + 1;
    let mut v = vec![0isize; (2 * bound + 3) as usize];
    let idx = |k: isize| (off + k) as usize;
    // trace[d] is `v` for k in -(d+1)..=(d+1) as it was before round d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    for d in 0..=bound {
        trace.push(v[idx(-d - 1)..=idx(d + 1)].to_vec());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[idx(k - 1)] < v[idx(k + 1)]) {
                v[idx(k + 1)]
            } else {
                v[idx(k - 1)] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx(k)] = x;
            if x >= n && y >= m {
                return Some(backtrack(&trace, n, m, d));
            }
            k += 2;
        }
    }
    None
}

/// Rebuild the edit script of the shortest path of length `distance` ending at `(n, m)`.
fn backtrack(trace: &[Vec<isize>], n: isize, m: isize, distance: isize) -> Vec<LineOp> {
    let (mut x, mut y) = (n, m);
    let mut reversed = Vec::new();
    for d in (1..=distance).rev() {
        let before = &trace[d as usize];
        let at = |k: isize| before[(k + d + 1) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            reversed.push(LineOp::Equal);
            x -= 1;
            y -= 1;
        }
        reversed.push(if x == prev_x {
            LineOp::Insert
        } else {
            LineOp::Delete
        });
        x = prev_x;
        y = prev_y;
    }
    // Round 0 is only the leading run of equal lines.
    reversed.extend(std::iter::repeat_n(LineOp::Equal, x as usize));
    reversed.reverse();
    reversed
}

fn hunks(steps: &[Step], a: &[&str], b: &[&str], context: usize) -> Vec<Hunk> {
    let is_change = |s: &Step| s.op != LineOp::Equal;
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = steps[from..].iter().position(is_change) {
        let first = from + found;
        let mut last = first;
        while let Some(gap) = steps[last + 1..].iter().position(is_change) {
            if gap > context.saturating_mul(2) {
                break;
            }
            last += 1 + gap;
        }
        let start = first.saturating_sub(context);
        let end = last
            .saturating_add(1)
            .saturating_add(context)
            .min(steps.len());
        out.push(build_hunk(&steps[start..end], a, b));
        from = end;
    }
    out
}

fn build_hunk(steps: &[Step], a: &[&str], b: &[&str]) -> Hunk {
    let lines: Vec<DiffLine> = steps
        .iter()
        .map(|step| {
            let (old_line, new_line, source) = match step.op {
                LineOp::Equal => (Some(step.old), Some(step.new), a[step.old]),
                LineOp::Delete => (Some(step.old), None, a[step.old]),
                LineOp::Insert => (None, Some(step.new), b[step.new]),
            };
            DiffLine {
                op: step.op,
                old_line,
                new_line,
                text: source.strip_suffix('\n').unwrap_or(source).to_owned(),
            }
        })
        .collect();
    Hunk {
        old_start: steps[0].old,
        old_count: lines.iter().filter(|l| l.op != LineOp::Insert).count(),
        new_start: steps[0].new,
        new_count: lines.iter().filter(|l| l.op != LineOp::Delete).count(),
        lines,
    }
}
