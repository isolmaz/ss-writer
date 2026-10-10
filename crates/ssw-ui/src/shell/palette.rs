//! The command palette model (spec §7): every [`CommandId`] with its localized label and its
//! shortcut text, ranked by the fuzzy matcher of `ssw_core::fuzzy`.
//!
//! The palette is shown by the Quick Open overlay ([`QuickOpen`](super::quick_open::QuickOpen))
//! in its command mode: `Ctrl+Shift+P` opens it with the `>` prefix, and typing `>` in Quick Open
//! switches to it. Labels come from Fluent (`CommandId::label_id`); shortcut text is produced by
//! the platform layer behind [`ShortcutLabels`] (`GetKeyNameTextW` for the user's keymap).

use std::ops::Range;

use ssw_core::command::CommandId;
use ssw_core::fuzzy::FuzzyMatcher;
use ssw_core::i18n::Localizer;

use super::picker::{Row, highlight_ranges};

/// Produces the text of a command's shortcut for display (`Ctrl+Shift+P`).
pub trait ShortcutLabels {
    /// The shortcut of `command` as the user's keymap and layout spell it, or `None` when the
    /// command has no shortcut.
    fn shortcut_label(&self, command: CommandId) -> Option<String>;
}

/// One command in a palette result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    pub command: CommandId,
    /// The localized label.
    pub label: String,
    /// The localized group (`Format`, `File`, …).
    pub group: String,
    /// The shortcut text, when the command has one.
    pub shortcut: Option<String>,
    /// Byte ranges of `label` matched by the query.
    pub highlights: Vec<Range<usize>>,
}

impl PaletteEntry {
    /// The list row showing this entry: group, label with highlights, shortcut at the right.
    #[must_use]
    pub fn to_row(&self) -> Row {
        Row {
            prefix: self.group.clone(),
            label: self.label.clone(),
            highlights: self.highlights.clone(),
            detail: self.shortcut.clone().unwrap_or_default(),
            ..Row::default()
        }
    }
}

#[derive(Debug, Clone)]
struct Item {
    command: CommandId,
    group: String,
    shortcut: Option<String>,
}

/// Every command with its label and shortcut, ready to be filtered.
#[derive(Debug)]
pub struct CommandPalette {
    items: Vec<Item>,
    labels: Vec<String>,
    matcher: FuzzyMatcher,
}

impl CommandPalette {
    /// Resolves the label, group and shortcut of every command.
    #[must_use]
    pub fn new(localizer: &Localizer, shortcuts: &dyn ShortcutLabels) -> Self {
        let items = CommandId::all()
            .map(|command| Item {
                command,
                group: localizer.text(command.group().label_id()),
                shortcut: shortcuts.shortcut_label(command),
            })
            .collect();
        let labels = CommandId::all()
            .map(|command| localizer.text(command.label_id()))
            .collect();
        Self {
            items,
            labels,
            matcher: FuzzyMatcher::new(),
        }
    }

    /// Re-reads every shortcut (the user changed the keymap).
    pub fn refresh_shortcuts(&mut self, shortcuts: &dyn ShortcutLabels) {
        for item in &mut self.items {
            item.shortcut = shortcuts.shortcut_label(item.command);
        }
    }

    /// Number of commands (all of `CommandId::ALL`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The commands matching `query`, best match first, at most `limit`. An empty query lists
    /// every command in menu order.
    pub fn rank(&mut self, query: &str, limit: usize) -> Vec<PaletteEntry> {
        let ranked = self.matcher.rank_texts(query, &self.labels, limit);
        ranked
            .into_iter()
            .map(|found| {
                let item = &self.items[found.index];
                let label = &self.labels[found.index];
                PaletteEntry {
                    command: item.command,
                    label: label.clone(),
                    group: item.group.clone(),
                    shortcut: item.shortcut.clone(),
                    highlights: highlight_ranges(label, &found.indices),
                }
            })
            .collect()
    }
}
