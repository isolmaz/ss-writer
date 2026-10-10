//! The vertical structure of the document: layout items and the height index.
//!
//! The document is a sequence of *items*: one per top-level block and one per blank source line
//! between blocks (so the caret can sit on blank lines). Items are measured lazily; until an item
//! is measured its height is an estimate from its line count. The [`HeightIndex`] keeps all
//! heights in a Fenwick tree so the top of any item, the total height and the item at a given
//! y are `O(log n)`, and scrolling a 60 000-line document touches only the visible items.

use ssw_core::document::Document;
use ssw_core::markdown::ast::{Block, BlockKind, CodeBlock, Markers, VerbatimLine};
use ssw_core::text::{Buffer, TextRange};

use super::config::DocumentKind;

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemKind {
    /// The top-level block with this index in the AST.
    Block(usize),
    /// A blank source line; `separator` marks the first blank line after a block, which renders
    /// as the paragraph spacing instead of a full line.
    Gap { separator: bool },
}

/// One vertical slice of the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Item {
    /// Source range the item owns: the block range, or the blank line including its line break.
    pub range: TextRange,
    /// For a blank line its content without the line break; for a block its range.
    pub content: TextRange,
    pub kind: ItemKind,
    /// Source lines covered (for estimates).
    pub lines: u32,
    /// Inside a folded section: height 0, never laid out.
    pub hidden: bool,
}

impl Item {
    /// Whether two items are interchangeable for height reuse.
    fn same_shape(&self, other: &Item) -> bool {
        let class = |item: &Item| match item.kind {
            ItemKind::Block(_) => 0,
            ItemKind::Gap { separator: true } => 1,
            ItemKind::Gap { separator: false } => 2,
        };
        class(self) == class(other)
            && self.range.len() == other.range.len()
            && self.hidden == other.hidden
    }
}

fn push_gap_lines(
    buffer: &Buffer,
    items: &mut Vec<Item>,
    from: usize,
    to: usize,
    after_block: bool,
) {
    for (n, line) in (from..to).enumerate() {
        items.push(Item {
            range: buffer.line_range_with_terminator(line),
            content: buffer.line_range(line),
            kind: ItemKind::Gap {
                separator: after_block && n == 0,
            },
            lines: 1,
            hidden: false,
        });
    }
}

/// The items of `doc`; items inside a `hidden` range (sorted, disjoint) are marked hidden.
pub(crate) fn build_items(doc: &Document, hidden: &[TextRange]) -> Vec<Item> {
    let buffer = doc.buffer();
    let blocks = &doc.ast().blocks;
    let mut items = Vec::with_capacity(blocks.len() * 2 + 1);
    let mut next_line = 0usize;
    let mut after_block = false;
    for (index, block) in blocks.iter().enumerate() {
        let first = buffer.line_of(block.range.start);
        let last = buffer.line_of(block.range.end).max(first);
        push_gap_lines(buffer, &mut items, next_line, first, after_block);
        items.push(Item {
            range: block.range,
            content: block.range,
            kind: ItemKind::Block(index),
            lines: u32::try_from(last - first + 1).unwrap_or(u32::MAX),
            hidden: false,
        });
        next_line = last + 1;
        after_block = true;
    }
    push_gap_lines(
        buffer,
        &mut items,
        next_line,
        buffer.line_count(),
        after_block,
    );
    let mut h = 0;
    for item in &mut items {
        while h < hidden.len() && hidden[h].end < item.range.end {
            h += 1;
        }
        if let Some(range) = hidden.get(h)
            && range.start <= item.range.start
            && item.range.end <= range.end
        {
            item.hidden = true;
        }
    }
    items
}

/// The items of `doc` presented as `kind`: the Markdown blocks, one raw item per source line
/// (plain text), or a single item holding the whole document (Mermaid). Plain and Mermaid
/// documents have no folds.
pub(crate) fn build_document_items(
    doc: &Document,
    hidden: &[TextRange],
    kind: DocumentKind,
) -> Vec<Item> {
    let buffer = doc.buffer();
    match kind {
        DocumentKind::Markdown => build_items(doc, hidden),
        DocumentKind::PlainText => (0..buffer.line_count())
            .map(|line| Item {
                range: buffer.line_range_with_terminator(line),
                content: buffer.line_range(line),
                kind: ItemKind::Block(line),
                lines: 1,
                hidden: false,
            })
            .collect(),
        DocumentKind::Mermaid => {
            let whole = TextRange::new(0, buffer.len());
            vec![Item {
                range: whole,
                content: whole,
                kind: ItemKind::Block(0),
                lines: u32::try_from(buffer.line_count()).unwrap_or(u32::MAX),
                hidden: false,
            }]
        }
    }
}

/// The block a non-Markdown item stands for: a plain line (a paragraph without inlines) or the
/// whole document as one unfenced code block of verbatim lines (Mermaid source).
pub(crate) fn document_block(doc: &Document, item: &Item, kind: DocumentKind) -> Block {
    let buffer = doc.buffer();
    let (block_kind, range) = match kind {
        DocumentKind::Mermaid => (
            BlockKind::CodeBlock(CodeBlock {
                fence: None,
                info: None,
                lines: (0..buffer.line_count())
                    .map(|line| VerbatimLine {
                        range: buffer.line_range(line),
                        pad: 0,
                    })
                    .collect(),
            }),
            item.range,
        ),
        _ => (BlockKind::Paragraph, item.content),
    };
    Block {
        range,
        content: range,
        markers: Markers::new(),
        kind: block_kind,
        children: Vec::new(),
        inlines: Vec::new(),
    }
}

/// The item that owns source `offset`: the last item starting at or before it.
pub(crate) fn item_at(items: &[Item], offset: usize) -> usize {
    items
        .partition_point(|item| item.range.start <= offset)
        .saturating_sub(1)
}

/// Item heights with prefix sums.
#[derive(Debug, Clone, Default)]
pub(crate) struct HeightIndex {
    /// Fenwick tree over `heights` (1-based).
    tree: Vec<f64>,
    heights: Vec<f32>,
}

impl HeightIndex {
    pub fn new(heights: Vec<f32>) -> Self {
        let n = heights.len();
        let mut tree = vec![0.0f64; n + 1];
        for i in 1..=n {
            tree[i] += f64::from(heights[i - 1]);
            let parent = i + i.isolate_lowest_one();
            if parent <= n {
                let value = tree[i];
                tree[parent] += value;
            }
        }
        Self { tree, heights }
    }

    pub fn height(&self, index: usize) -> f32 {
        self.heights[index]
    }

    /// Sets the height of `index`.
    pub fn set(&mut self, index: usize, height: f32) {
        let delta = f64::from(height) - f64::from(self.heights[index]);
        self.heights[index] = height;
        let mut i = index + 1;
        while i < self.tree.len() {
            self.tree[i] += delta;
            i += i.isolate_lowest_one();
        }
    }

    /// Sum of the heights of the first `count` items: the top of item `count`.
    pub fn prefix(&self, count: usize) -> f64 {
        let mut sum = 0.0;
        let mut i = count.min(self.heights.len());
        while i > 0 {
            sum += self.tree[i];
            i -= i.isolate_lowest_one();
        }
        sum
    }

    pub fn total(&self) -> f64 {
        self.prefix(self.heights.len())
    }

    /// The item whose vertical extent contains `y` (zero-height items are skipped); the last
    /// item for `y` beyond the end. The index must not be empty.
    pub fn find(&self, y: f64) -> usize {
        let n = self.heights.len();
        let mut position = 0usize;
        let mut remaining = y.max(0.0);
        let mut step = 1usize << (usize::BITS - 1 - n.leading_zeros());
        while step > 0 {
            let next = position + step;
            if next <= n && self.tree[next] <= remaining {
                position = next;
                remaining -= self.tree[next];
            }
            step >>= 1;
        }
        position.min(n - 1)
    }

    /// Heights for `new_items` that reuse the heights of `old_items` for the unchanged prefix
    /// and suffix and use `estimate` for what changed in between.
    pub fn remapped(
        &self,
        old_items: &[Item],
        new_items: &[Item],
        estimate: &dyn Fn(&Item) -> f32,
    ) -> HeightIndex {
        let (old_n, new_n) = (old_items.len(), new_items.len());
        let common = old_n.min(new_n);
        let mut prefix = 0;
        while prefix < common && old_items[prefix].same_shape(&new_items[prefix]) {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < common - prefix
            && old_items[old_n - 1 - suffix].same_shape(&new_items[new_n - 1 - suffix])
        {
            suffix += 1;
        }
        let heights = new_items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                if item.hidden {
                    0.0
                } else if i < prefix {
                    self.heights[i]
                } else if i >= new_n - suffix {
                    self.heights[old_n - (new_n - i)]
                } else {
                    estimate(item)
                }
            })
            .collect();
        HeightIndex::new(heights)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn prefix_sums_and_find_agree_with_a_plain_scan() {
        let heights: Vec<f32> = (0..37).map(|i| (i % 5) as f32 * 3.0).collect();
        let index = HeightIndex::new(heights.clone());
        let mut sum = 0.0;
        for (i, h) in heights.iter().enumerate() {
            assert!(close(index.prefix(i), sum), "prefix {i}");
            assert_eq!(index.height(i), *h);
            sum += f64::from(*h);
        }
        assert!(close(index.total(), sum));
        for y in [0.0, 0.5, 2.9, 3.0, 7.0, 50.0, 1e9] {
            let expected = heights
                .iter()
                .scan(0.0f64, |top, h| {
                    let range = (*top, *top + f64::from(*h));
                    *top = range.1;
                    Some(range)
                })
                .position(|(_, bottom)| bottom > y)
                .unwrap_or(heights.len() - 1);
            assert_eq!(index.find(y), expected, "y = {y}");
        }
    }

    #[test]
    fn set_updates_prefixes_and_zero_height_items_are_skipped() {
        let mut index = HeightIndex::new(vec![10.0, 0.0, 10.0, 10.0]);
        assert_eq!(index.find(10.0), 2, "the zero-height item is skipped");
        index.set(0, 4.0);
        assert!(close(index.prefix(2), 4.0));
        assert!(close(index.total(), 24.0));
        assert_eq!(index.find(3.9), 0);
        assert_eq!(index.find(4.0), 2);
        assert_eq!(index.find(-5.0), 0);
        let single = HeightIndex::new(vec![7.0]);
        assert_eq!(single.find(100.0), 0);
        assert_eq!(HeightIndex::default().total(), 0.0);
    }

    fn gap(start: usize, len: usize, separator: bool) -> Item {
        Item {
            range: TextRange::new(start, start + len),
            content: TextRange::new(start, start + len),
            kind: ItemKind::Gap { separator },
            lines: 1,
            hidden: false,
        }
    }

    #[test]
    fn remapping_keeps_unchanged_prefix_and_suffix() {
        let old = [gap(0, 5, false), gap(5, 6, false), gap(11, 7, false)];
        let heights = HeightIndex::new(vec![10.0, 20.0, 30.0]);
        // The middle item changed length and one item was inserted.
        let new = [
            gap(0, 5, false),
            gap(5, 9, false),
            gap(14, 1, false),
            gap(15, 7, false),
        ];
        let remapped = heights.remapped(&old, &new, &|_| 99.0);
        let got: Vec<f32> = (0..4).map(|i| remapped.height(i)).collect();
        assert_eq!(got, vec![10.0, 99.0, 99.0, 30.0]);
        let mut hidden = new;
        hidden[1].hidden = true;
        let remapped = heights.remapped(&old, &hidden, &|_| 99.0);
        assert_eq!(remapped.height(1), 0.0);
    }

    #[test]
    fn items_cover_blocks_and_blank_lines() {
        let doc = Document::new("# T\n\n\npara\nmore\n");
        let items = build_items(&doc, &[]);
        let kinds: Vec<ItemKind> = items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ItemKind::Block(0),
                ItemKind::Gap { separator: true },
                ItemKind::Gap { separator: false },
                ItemKind::Block(1),
                ItemKind::Gap { separator: true },
            ]
        );
        assert_eq!(items[0].range, TextRange::new(0, 3));
        assert_eq!(items[1].range, TextRange::new(4, 5));
        assert_eq!(items[1].content, TextRange::new(4, 4));
        assert_eq!(items[3].lines, 2);
        assert_eq!(
            items[4].range,
            TextRange::new(16, 16),
            "the empty last line"
        );
        assert_eq!(item_at(&items, 0), 0);
        assert_eq!(item_at(&items, 3), 0, "the end of a block belongs to it");
        assert_eq!(item_at(&items, 4), 1);
        assert_eq!(item_at(&items, 6), 3);
        assert_eq!(item_at(&items, 16), 4);
        assert_eq!(item_at(&items, 99), 4);
    }

    #[test]
    fn an_empty_document_has_one_blank_line() {
        let items = build_items(&Document::new(""), &[]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ItemKind::Gap { separator: false });
    }

    #[test]
    fn folded_ranges_hide_the_items_inside() {
        let doc = Document::new("# A\n\nbody\n\n# B\n");
        // Hide from the end of the first heading to the end of "body".
        let items = build_items(&doc, &[TextRange::new(3, 9)]);
        let hidden: Vec<bool> = items.iter().map(|i| i.hidden).collect();
        assert_eq!(hidden, vec![false, true, true, false, false, false]);
    }
}
