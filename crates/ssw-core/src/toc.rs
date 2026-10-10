//! Table of contents (spec §8): the heading tree of a document.
//!
//! Only top-level ATX headings take part; headings nested in quotes, lists or callouts are
//! content of their container. Entries are stored flat in source order; [`TocEntry::parent`] /
//! [`TocEntry::children`] hold the tree. A heading nests under the nearest preceding heading
//! with a strictly lower level, so `# A` followed by `### B` puts B under A (level gaps are
//! tolerated) and a later `## C` is a sibling of B under A.

use std::collections::HashMap;

use unicode_segmentation::UnicodeSegmentation;

use crate::markdown::ast::{BlockKind, Document, Inline, InlineKind, WikiLink};
use crate::text::TextRange;

/// One heading of the outline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TocEntry {
    /// 1..=6.
    pub level: u8,
    /// Plain text of the heading: inline markers removed, whitespace collapsed.
    pub title: String,
    /// GitHub-style anchor, unique within the document (`-1`, `-2` … appended on collisions).
    pub slug: String,
    /// Source range of the heading block (`#` marker through the closing run, no line break).
    pub range: TextRange,
    /// Index of the entry this heading nests under.
    pub parent: Option<usize>,
    /// Indices of the entries nested directly under this heading, in source order.
    pub children: Vec<usize>,
    /// Number of ancestors (0 for roots); drives the staircase indentation.
    pub depth: usize,
}

/// The heading tree of a document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Toc {
    entries: Vec<TocEntry>,
    roots: Vec<usize>,
}

impl Toc {
    /// Builds the outline of `doc`, which was parsed from `src`.
    pub fn build(doc: &Document, src: &str) -> Toc {
        let mut toc = Toc::default();
        let mut stack: Vec<usize> = Vec::new();
        let mut occurrences: HashMap<String, usize> = HashMap::new();
        for block in &doc.blocks {
            let BlockKind::Heading { level, .. } = block.kind else {
                continue;
            };
            while stack
                .last()
                .is_some_and(|&top| toc.entries[top].level >= level)
            {
                stack.pop();
            }
            let index = toc.entries.len();
            let parent = stack.last().copied();
            match parent {
                Some(p) => toc.entries[p].children.push(index),
                None => toc.roots.push(index),
            }
            let title = plain_text(&block.inlines, src);
            let slug = unique_slug(&title, &mut occurrences);
            toc.entries.push(TocEntry {
                level,
                title,
                slug,
                range: block.range,
                parent,
                children: Vec::new(),
                depth: stack.len(),
            });
            stack.push(index);
        }
        toc
    }

    /// All headings in source order.
    pub fn entries(&self) -> &[TocEntry] {
        &self.entries
    }

    /// Indices of the top-level entries (those without a parent).
    pub fn roots(&self) -> &[usize] {
        &self.roots
    }

    /// The heading whose section contains `offset`: the last heading starting at or before it.
    /// `None` before the first heading.
    pub fn current_section(&self, offset: usize) -> Option<usize> {
        self.entries
            .partition_point(|e| e.range.start <= offset)
            .checked_sub(1)
    }

    /// Indices of the entries whose level is at most `max_level` (the level filter of the
    /// Contents tab). Parents always have a lower level than their children, so the result
    /// is a closed subtree.
    pub fn filter_levels(&self, max_level: u8) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.level <= max_level)
            .map(|(i, _)| i)
            .collect()
    }

    /// The first heading after `index` with the same or a higher level (smaller or equal level
    /// number): where the section of `index` ends. `None` when it runs to the end of the
    /// document.
    pub fn section_end(&self, index: usize) -> Option<usize> {
        let level = self.entries.get(index)?.level;
        (index + 1..self.entries.len()).find(|&j| self.entries[j].level <= level)
    }
}

/// Plain text of an inline sequence: markers, link destinations and raw HTML are dropped,
/// escapes and entities are resolved, line breaks become spaces and whitespace is collapsed.
/// Link and image text (alt) is kept; a wikilink contributes its alias or target;
/// footnote references contribute nothing.
pub fn plain_text(inlines: &[Inline], src: &str) -> String {
    let mut text = String::new();
    push_text(inlines, src, &mut text);
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn push_text(inlines: &[Inline], src: &str, out: &mut String) {
    for inline in inlines {
        match &inline.kind {
            InlineKind::Text => out.push_str(inline.range.get(src).unwrap_or("")),
            InlineKind::Escape | InlineKind::InlineMath => {
                out.push_str(inline.content.get(src).unwrap_or(""));
            }
            InlineKind::Entity { decoded } => out.push_str(decoded),
            InlineKind::Code { text } => out.push_str(text),
            InlineKind::SoftBreak | InlineKind::HardBreak => out.push(' '),
            InlineKind::Emphasis
            | InlineKind::Strong
            | InlineKind::Underline
            | InlineKind::Strikethrough
            | InlineKind::Highlight { .. }
            | InlineKind::Link(_)
            | InlineKind::Image(_) => push_text(&inline.children, src, out),
            InlineKind::WikiLink(w) => out.push_str(wikilink_text(w)),
            InlineKind::FootnoteRef { .. } | InlineKind::RawHtml => {}
        }
    }
}

/// What a reader sees for a wikilink: the alias, else the target, else the heading
/// (`[[#Heading]]`).
pub(crate) fn wikilink_text(link: &WikiLink) -> &str {
    if let Some(alias) = &link.alias {
        alias
    } else if link.target.is_empty() {
        link.heading.as_deref().unwrap_or("")
    } else {
        &link.target
    }
}

/// GitHub-style anchor of `title` (without collision handling): lower case; letters, digits,
/// `-` and `_` are kept together with the combining marks attached to them; each space becomes
/// `-`; every other character (punctuation, symbols, emoji) is dropped.
pub fn slugify(title: &str) -> String {
    let mut slug = String::with_capacity(title.len());
    for cluster in title.graphemes(true) {
        match cluster.chars().next() {
            Some(c) if c.is_alphanumeric() || c == '-' || c == '_' => slug.push_str(cluster),
            Some(' ') => slug.push('-'),
            _ => {}
        }
    }
    slug.to_lowercase()
}

/// Slug of `title` made unique through `occurrences` the way GitHub does: a repeated slug
/// gets `-1`, `-2`, … (skipping suffixed slugs that are already taken).
fn unique_slug(title: &str, occurrences: &mut HashMap<String, usize>) -> String {
    let base = slugify(title);
    let mut slug = base.clone();
    while occurrences.contains_key(&slug) {
        let count = occurrences.entry(base.clone()).or_insert(0);
        *count += 1;
        slug = format!("{base}-{count}");
    }
    occurrences.insert(slug.clone(), 0);
    slug
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parse_document;

    fn toc(src: &str) -> Toc {
        Toc::build(&parse_document(src), src)
    }

    fn titles(t: &Toc) -> Vec<&str> {
        t.entries().iter().map(|e| e.title.as_str()).collect()
    }

    fn slugs(t: &Toc) -> Vec<&str> {
        t.entries().iter().map(|e| e.slug.as_str()).collect()
    }

    #[test]
    fn empty_and_headingless_documents_have_no_entries() {
        for src in ["", "just text\n\n- list\n", "Foo\n---\n"] {
            let t = toc(src);
            assert!(t.entries().is_empty(), "{src:?}");
            assert!(t.roots().is_empty());
            assert_eq!(t.current_section(0), None);
            assert_eq!(t.section_end(0), None);
            assert!(t.filter_levels(6).is_empty());
        }
    }

    #[test]
    fn nesting_tolerates_level_gaps() {
        let t = toc("# A\n### B\n## C\n# D\n#### E\n## F\n");
        assert_eq!(titles(&t), ["A", "B", "C", "D", "E", "F"]);
        let parents: Vec<Option<usize>> = t.entries().iter().map(|e| e.parent).collect();
        assert_eq!(parents, [None, Some(0), Some(0), None, Some(3), Some(3)]);
        let depths: Vec<usize> = t.entries().iter().map(|e| e.depth).collect();
        assert_eq!(depths, [0, 1, 1, 0, 1, 1]);
        assert_eq!(t.roots(), [0, 3]);
        assert_eq!(t.entries()[0].children, [1, 2]);
        assert_eq!(t.entries()[3].children, [4, 5]);
        assert!(t.entries()[1].children.is_empty());
    }

    #[test]
    fn document_may_start_deeper_than_it_continues() {
        let t = toc("### deep\n# top\n## mid\n");
        assert_eq!(t.roots(), [0, 1]);
        assert_eq!(t.entries()[2].parent, Some(1));
        assert_eq!(t.entries()[0].level, 3);
    }

    #[test]
    fn deeper_chain_pops_to_the_nearest_lower_level() {
        let t = toc("# A\n## B\n### C\n## D\n");
        let parents: Vec<Option<usize>> = t.entries().iter().map(|e| e.parent).collect();
        assert_eq!(parents, [None, Some(0), Some(1), Some(0)]);
        assert_eq!(t.entries()[2].depth, 2);
    }

    #[test]
    fn titles_are_plain_text() {
        let src = "# Hello **bold** _em_ ~~gone~~ `code` ==hi== ~u~\n\
                   ## [a link](http://u) and ![alt text](i.png) and <http://auto.example/x>\n\
                   ### [[Target|alias]] [[Plain]] [[#Only]] [[N#H]]\n\
                   #### a &amp; b \\* c <b>d</b> $x^2$ note[^1] ##\n\
                   ##### \n\
                   ###### a    spaced   out\n\
                   \n[^1]: x\n";
        let t = toc(src);
        assert_eq!(
            titles(&t),
            [
                "Hello bold em gone code hi u",
                "a link and alt text and http://auto.example/x",
                "alias Plain Only N",
                "a & b * c d x^2 note",
                "",
                "a spaced out",
            ]
        );
    }

    #[test]
    fn plain_text_turns_breaks_into_spaces() {
        let src = "line one\nline two  \nline three\\\nend";
        let doc = parse_document(src);
        assert_eq!(
            plain_text(&doc.blocks[0].inlines, src),
            "line one line two line three end"
        );
    }

    #[test]
    fn headings_in_containers_are_not_part_of_the_outline() {
        let t = toc("> # quoted\n\n- # in list\n\n# real\n");
        assert_eq!(titles(&t), ["real"]);
    }

    #[test]
    fn heading_ranges_start_at_the_marker() {
        let src = "intro\n\n## Two\n\nbody\n";
        let t = toc(src);
        assert_eq!(t.entries()[0].range, TextRange::new(7, 13));
    }

    #[test]
    fn slugify_follows_github() {
        for (title, slug) in [
            ("Hello, World!", "hello-world"),
            ("Ünïcödé Çafé", "ünïcödé-çafé"),
            ("日本語 テスト", "日本語-テスト"),
            ("emoji 😀 title", "emoji--title"),
            ("a_b-c d", "a_b-c-d"),
            ("Don't stop", "dont-stop"),
            ("a - b", "a---b"),
            ("C# & F#", "c--f"),
            ("İstanbul", "i\u{307}stanbul"),
            ("e\u{301}x", "e\u{301}x"),
            ("\u{301}a", "a"),
            ("ΟΔΟΣ", "οδος"),
            ("हिन्दी भाषा", "हिन्दी-भाषा"),
            ("", ""),
            ("!!!", ""),
        ] {
            assert_eq!(slugify(title), slug, "title {title:?}");
        }
    }

    #[test]
    fn slug_collisions_get_numeric_suffixes() {
        let t = toc("# A\n# A\n# a\n## A 1\n# A-1\n# A\n");
        assert_eq!(slugs(&t), ["a", "a-1", "a-2", "a-1-1", "a-1-2", "a-3"]);
        let empty = toc("# 😀\n# 😀\n");
        assert_eq!(slugs(&empty), ["", "-1"]);
    }

    #[test]
    fn current_section_is_the_last_heading_at_or_before_the_offset() {
        let src = "intro\n\n# A\n\ntext\n\n## B\n\nmore\n";
        let t = toc(src);
        let a = t.entries()[0].range.start;
        let b = t.entries()[1].range.start;
        assert_eq!(t.current_section(0), None);
        assert_eq!(t.current_section(a - 1), None);
        assert_eq!(t.current_section(a), Some(0));
        assert_eq!(t.current_section(b - 1), Some(0));
        assert_eq!(t.current_section(b), Some(1));
        assert_eq!(t.current_section(src.len()), Some(1));
    }

    #[test]
    fn level_filter_keeps_closed_subtrees() {
        let t = toc("# A\n### B\n## C\n#### D\n# E\n");
        assert_eq!(t.filter_levels(1), [0, 4]);
        assert_eq!(t.filter_levels(2), [0, 2, 4]);
        assert_eq!(t.filter_levels(3), [0, 1, 2, 4]);
        assert_eq!(t.filter_levels(6), [0, 1, 2, 3, 4]);
        assert!(t.filter_levels(0).is_empty());
    }

    #[test]
    fn section_end_is_the_next_heading_of_same_or_higher_level() {
        let t = toc("# A\n## B\n### C\n## D\n# E\n## F\n");
        assert_eq!(t.section_end(0), Some(4)); // A ends at E
        assert_eq!(t.section_end(1), Some(3)); // B ends at D
        assert_eq!(t.section_end(2), Some(3)); // C ends at D (higher level)
        assert_eq!(t.section_end(3), Some(4));
        assert_eq!(t.section_end(4), None);
        assert_eq!(t.section_end(5), None);
        assert_eq!(t.section_end(99), None);
    }
}
