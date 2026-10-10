//! Document-level definitions: link reference definitions and footnote definitions.
//!
//! Inline parsing of a block depends on these tables (a `[foo]` is a link only when `[foo]:`
//! is defined somewhere in the document). The table therefore also records which blocks
//! looked up which labels, so an incremental parser can re-parse exactly the blocks affected
//! when a definition appears, disappears or changes.

use std::collections::{HashMap, HashSet};

use crate::text::TextRange;

/// CommonMark label normalization: trim, collapse internal whitespace runs to one space and
/// Unicode case-fold (`ẞ` and `SS` both fold to `ss`).
pub fn normalize_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut pending_space = false;
    for c in label.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        for lower in c.to_lowercase() {
            if lower == 'ß' {
                out.push_str("ss");
            } else {
                out.push(lower);
            }
        }
    }
    out
}

/// Destination and title of a `[label]: dest "title"` definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkDefinition {
    /// Normalized label (see [`normalize_label`]).
    pub label: String,
    /// Destination with backslash escapes and entities resolved.
    pub dest: String,
    pub title: Option<String>,
}

/// Which table a label lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LabelKind {
    Link,
    Footnote,
}

/// A lookup of `label` made while parsing the inlines of the block at `block`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelUse {
    /// Source range of the leaf block (paragraph, heading, table) that did the lookup.
    pub block: TextRange,
    pub kind: LabelKind,
    /// Normalized label. Recorded whether or not a definition existed.
    pub label: String,
}

/// A label whose resolution may differ between two documents.
pub type LabelKey = (LabelKind, String);

/// Link reference definitions (first definition of a label wins) and footnote definitions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Definitions {
    links: HashMap<String, LinkDefinition>,
    footnotes: HashMap<String, TextRange>,
    uses: Vec<LabelUse>,
}

impl Definitions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Definition for `label` (normalized internally).
    pub fn link(&self, label: &str) -> Option<&LinkDefinition> {
        self.links.get(&normalize_label(label))
    }

    /// Source range of the footnote definition block for `label`.
    pub fn footnote(&self, label: &str) -> Option<TextRange> {
        self.footnotes.get(&normalize_label(label)).copied()
    }

    pub fn has_footnote(&self, label: &str) -> bool {
        self.footnotes.contains_key(&normalize_label(label))
    }

    /// All link definitions, in no particular order.
    pub fn links(&self) -> impl Iterator<Item = &LinkDefinition> {
        self.links.values()
    }

    /// Normalized labels of all footnote definitions, in no particular order.
    pub fn footnote_labels(&self) -> impl Iterator<Item = &str> {
        self.footnotes.keys().map(String::as_str)
    }

    /// Records a definition; returns `false` (and keeps the earlier one) when the label is
    /// already defined.
    pub(crate) fn define_link(&mut self, def: LinkDefinition) -> bool {
        if self.links.contains_key(&def.label) {
            return false;
        }
        self.links.insert(def.label.clone(), def);
        true
    }

    /// Records a footnote definition block; returns `false` when the label is already defined.
    pub(crate) fn define_footnote(&mut self, label: &str, range: TextRange) -> bool {
        let key = normalize_label(label);
        if self.footnotes.contains_key(&key) {
            return false;
        }
        self.footnotes.insert(key, range);
        true
    }

    /// Records that the block at `block` looked up `label` (already normalized).
    pub(crate) fn record_use(&mut self, block: TextRange, kind: LabelKind, label: String) {
        let duplicate = self
            .uses
            .iter()
            .rev()
            .take_while(|u| u.block == block)
            .any(|u| u.kind == kind && u.label == label);
        if !duplicate {
            self.uses.push(LabelUse { block, kind, label });
        }
    }

    /// Every label lookup, in document order.
    pub fn uses(&self) -> &[LabelUse] {
        &self.uses
    }

    /// Source ranges of the blocks that looked up `label`.
    pub fn blocks_using<'a>(
        &'a self,
        kind: LabelKind,
        label: &str,
    ) -> impl Iterator<Item = TextRange> + 'a {
        let key = normalize_label(label);
        self.uses
            .iter()
            .filter(move |u| u.kind == kind && u.label == key)
            .map(|u| u.block)
    }

    /// Labels whose resolution differs between `self` and `other`: defined in only one of the
    /// two, or defined in both with a different destination or title. Footnote definitions
    /// are compared by presence only (their position does not change how references parse).
    pub fn changed_labels(&self, other: &Definitions) -> Vec<LabelKey> {
        let mut changed = Vec::new();
        let link_labels: HashSet<&String> = self.links.keys().chain(other.links.keys()).collect();
        for label in link_labels {
            if self.links.get(label) != other.links.get(label) {
                changed.push((LabelKind::Link, label.clone()));
            }
        }
        let note_labels: HashSet<&String> = self
            .footnotes
            .keys()
            .chain(other.footnotes.keys())
            .collect();
        for label in note_labels {
            if self.footnotes.contains_key(label) != other.footnotes.contains_key(label) {
                changed.push((LabelKind::Footnote, label.clone()));
            }
        }
        changed.sort();
        changed
    }

    /// Source ranges of the blocks (in `self`) that looked up any of `labels`, ascending and
    /// without duplicates.
    pub fn blocks_affected_by(&self, labels: &[LabelKey]) -> Vec<TextRange> {
        let wanted: HashSet<(LabelKind, &str)> = labels
            .iter()
            .map(|(kind, label)| (*kind, label.as_str()))
            .collect();
        let mut blocks: Vec<TextRange> = self
            .uses
            .iter()
            .filter(|u| wanted.contains(&(u.kind, u.label.as_str())))
            .map(|u| u.block)
            .collect();
        blocks.sort();
        blocks.dedup();
        blocks
    }
}
