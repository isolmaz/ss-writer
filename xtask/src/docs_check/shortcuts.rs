//! Spec §11 table ↔ the default keymap of `ssw-core`.

use std::collections::{BTreeMap, BTreeSet};

use ssw_core::command::CommandId;
use ssw_core::keymap::{KeyChord, Keymap};

use super::spec;

/// How each first cell of the §11 table maps to commands, in the order the cell names them.
/// A spec row that is not listed here (or a listed row missing from the spec) is reported:
/// the spec table and this mapping change together.
const ROWS: &[(&str, &[CommandId])] = &[
    (
        "New file / New window",
        &[CommandId::FileNew, CommandId::FileNewWindow],
    ),
    (
        "Open file / Open folder",
        &[CommandId::FileOpen, CommandId::FileOpenFolder],
    ),
    (
        "Save / Save as",
        &[CommandId::FileSave, CommandId::FileSaveAs],
    ),
    ("Print", &[CommandId::FilePrint]),
    (
        "Close tab / Reopen closed tab / New tab",
        &[
            CommandId::TabClose,
            CommandId::TabReopenClosed,
            CommandId::TabNew,
        ],
    ),
    (
        "Next / previous tab",
        &[CommandId::TabNext, CommandId::TabPrevious],
    ),
    ("Undo / Redo", &[CommandId::EditUndo, CommandId::EditRedo]),
    ("Paste plain", &[CommandId::EditPastePlain]),
    (
        "Find / Replace / Go to line",
        &[
            CommandId::SearchFind,
            CommandId::SearchReplace,
            CommandId::SearchGoToLine,
        ],
    ),
    (
        "Find next / previous",
        &[CommandId::SearchFindNext, CommandId::SearchFindPrevious],
    ),
    (
        "Quick Open / Command palette / Search in folder",
        &[
            CommandId::SearchQuickOpen,
            CommandId::SearchCommandPalette,
            CommandId::SearchInFolder,
        ],
    ),
    (
        "Bold / Italic / Underline",
        &[
            CommandId::FormatBold,
            CommandId::FormatItalic,
            CommandId::FormatUnderline,
        ],
    ),
    (
        "Highlight / Strikethrough",
        &[CommandId::FormatHighlight, CommandId::FormatStrikethrough],
    ),
    (
        "Inline code / Code block",
        &[CommandId::FormatInlineCode, CommandId::FormatCodeBlock],
    ),
    (
        "Link / Task toggle",
        &[CommandId::FormatLink, CommandId::FormatTaskToggle],
    ),
    (
        "Heading 1–6 (again → paragraph)",
        &[
            CommandId::FormatHeading1,
            CommandId::FormatHeading2,
            CommandId::FormatHeading3,
            CommandId::FormatHeading4,
            CommandId::FormatHeading5,
            CommandId::FormatHeading6,
        ],
    ),
    (
        "Sidebar / Inspector",
        &[CommandId::ViewToggleSidebar, CommandId::ViewToggleInspector],
    ),
    (
        "Typewriter / Focus",
        &[CommandId::ViewTypewriterMode, CommandId::ViewFocusMode],
    ),
    (
        "Full screen / Zen",
        &[CommandId::ViewFullScreen, CommandId::ViewZenMode],
    ),
    ("Insert date", &[CommandId::EditInsertDate]),
    (
        "Zoom in / out / reset",
        &[
            CommandId::ViewZoomIn,
            CommandId::ViewZoomOut,
            CommandId::ViewZoomReset,
        ],
    ),
    ("Recenter caret", &[CommandId::ViewRecenterCaret]),
    ("Settings", &[CommandId::SettingsOpen]),
    ("Next pane", &[CommandId::ViewNextPane]),
];

fn parse_chord(text: &str) -> Result<KeyChord, String> {
    text.trim().parse::<KeyChord>().map_err(|e| e.to_string())
}

/// `Ctrl+1 … Ctrl+6` → one chord per command.
fn expand_range(cell: &str, commands: usize) -> Result<Vec<BTreeSet<KeyChord>>, String> {
    let Some((from, to)) = cell.split_once(" … ") else {
        return Err(format!("cannot read the range `{cell}`"));
    };
    let (from, to) = (parse_chord(from)?, parse_chord(to)?);
    let same_modifiers =
        (from.ctrl, from.shift, from.alt, from.win) == (to.ctrl, to.shift, to.alt, to.win);
    if !same_modifiers || from.vk > to.vk || usize::from(to.vk - from.vk) + 1 != commands {
        return Err(format!(
            "the range `{cell}` does not cover exactly {commands} commands"
        ));
    }
    Ok((from.vk..=to.vk)
        .map(|vk| BTreeSet::from([KeyChord { vk, ..from }]))
        .collect())
}

/// The shortcut cell as one chord set per command: groups separated by ` / `, alternatives
/// within a group by `, `.
fn expand_row(cell: &str, commands: usize) -> Result<Vec<BTreeSet<KeyChord>>, String> {
    if cell.contains(" … ") {
        return expand_range(cell, commands);
    }
    let groups: Vec<&str> = cell.split(" / ").collect();
    if groups.len() != commands {
        return Err(format!(
            "names {commands} commands but lists {} shortcut groups",
            groups.len()
        ));
    }
    groups
        .into_iter()
        .map(|group| {
            group
                .split(", ")
                .map(parse_chord)
                .collect::<Result<BTreeSet<KeyChord>, String>>()
        })
        .collect()
}

fn list(chords: &BTreeSet<KeyChord>) -> String {
    chords
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Compares the spec §11 table with `keymap`; every difference is one problem line.
pub fn check(spec_text: &str, keymap: &Keymap) -> Vec<String> {
    let rows = spec::shortcut_rows(spec_text);
    let mut problems = Vec::new();
    let mut documented: BTreeMap<CommandId, BTreeSet<KeyChord>> = BTreeMap::new();

    for row in &rows {
        let Some((_, commands)) = ROWS.iter().find(|(name, _)| *name == row.commands) else {
            problems.push(format!(
                "spec §11 row `{}` is not known to xtask (add it to ROWS in \
                 xtask/src/docs_check/shortcuts.rs)",
                row.commands
            ));
            continue;
        };
        match expand_row(&row.shortcuts, commands.len()) {
            Ok(per_command) => {
                for (command, chords) in commands.iter().zip(per_command) {
                    documented.insert(*command, chords);
                }
            }
            Err(message) => problems.push(format!("spec §11 row `{}`: {message}", row.commands)),
        }
    }
    for (name, _) in ROWS {
        if !rows.iter().any(|row| row.commands == *name) {
            problems.push(format!(
                "xtask maps the spec §11 row `{name}` but docs/spec.md has no such row"
            ));
        }
    }

    for (command, wanted) in &documented {
        let actual: BTreeSet<KeyChord> = keymap.chords_for(*command).collect();
        if actual != *wanted {
            problems.push(format!(
                "{command}: spec §11 says [{}] but the default keymap has [{}]",
                list(wanted),
                list(&actual)
            ));
        }
    }
    let undocumented: BTreeSet<CommandId> = keymap
        .bindings()
        .iter()
        .map(|binding| binding.command)
        .filter(|command| !documented.contains_key(command))
        .collect();
    for command in undocumented {
        problems.push(format!(
            "{command}: the default keymap binds it but spec §11 lists no shortcut"
        ));
    }
    for binding in keymap.bindings() {
        if binding.chord.is_ctrl_alt() {
            problems.push(format!(
                "{}: default shortcut {} uses Ctrl+Alt (AltGr), forbidden by spec §11",
                binding.command, binding.chord
            ));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real_spec() -> String {
        let workspace = crate::workspace::Workspace::locate().unwrap();
        crate::fsx::read_text(&workspace.path("docs/spec.md")).unwrap()
    }

    fn table(rows: &[(&str, &str)]) -> String {
        let mut text = String::from("## 11. Keyboard\n\n| Command | Default |\n|---|---|\n");
        for (commands, shortcuts) in rows {
            text.push_str(&format!("| {commands} | {shortcuts} |\n"));
        }
        text
    }

    /// The real spec table with `from` replaced by `to`.
    fn edited_spec(from: &str, to: &str) -> String {
        let spec = real_spec();
        assert!(spec.contains(from), "spec no longer contains `{from}`");
        spec.replacen(from, to, 1)
    }

    #[test]
    fn the_real_spec_table_matches_the_default_keymap() {
        assert_eq!(
            check(&real_spec(), &Keymap::default()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn every_mapped_row_resolves_to_distinct_commands() {
        let mut seen = BTreeSet::new();
        for (name, commands) in ROWS {
            for command in *commands {
                assert!(seen.insert(*command), "{name}: {command} listed twice");
            }
        }
    }

    #[test]
    fn a_changed_chord_in_the_spec_is_reported_with_both_sides() {
        let spec = edited_spec("| Print | Ctrl+P |", "| Print | Ctrl+Shift+P |");
        let problems = check(&spec, &Keymap::default());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("file.print:"), "{problems:?}");
        assert!(problems[0].contains("[Ctrl+Shift+P]"), "{problems:?}");
        assert!(problems[0].contains("[Ctrl+P]"), "{problems:?}");
    }

    #[test]
    fn a_dropped_alternative_is_reported() {
        let spec = edited_spec("Ctrl+Z / Ctrl+Y, Ctrl+Shift+Z", "Ctrl+Z / Ctrl+Y");
        let problems = check(&spec, &Keymap::default());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("edit.redo:"), "{problems:?}");
    }

    #[test]
    fn a_row_unknown_to_xtask_and_a_missing_row_are_reported() {
        let problems = check(
            &table(&[("Brand new command", "Ctrl+Q"), ("Print", "Ctrl+P")]),
            &Keymap::default(),
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`Brand new command` is not known to xtask")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`Save / Save as` but docs/spec.md has no such row")),
            "{problems:?}"
        );
    }

    #[test]
    fn keymap_bindings_without_a_spec_row_are_reported() {
        let problems = check(&table(&[("Print", "Ctrl+P")]), &Keymap::default());
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("file.new:") && p.contains("lists no shortcut")),
            "{problems:?}"
        );
    }

    #[test]
    fn malformed_cells_are_reported_not_panicked_on() {
        let problems = check(
            &table(&[
                ("New file / New window", "Ctrl+N"),
                ("Print", "Ctrl+Nonsense"),
                ("Heading 1–6 (again → paragraph)", "Ctrl+1 … Ctrl+5"),
                ("Zoom in / out / reset", "Ctrl+= … Ctrl+0"),
                ("Settings", "Ctrl+, … Shift+,"),
            ]),
            &Keymap::default(),
        );
        let has = |needle: &str| problems.iter().any(|p| p.contains(needle));
        assert!(
            has("`New file / New window`: names 2 commands but lists 1"),
            "{problems:?}"
        );
        assert!(
            has("`Print`: invalid key chord `Ctrl+Nonsense`"),
            "{problems:?}"
        );
        assert!(
            has("`Heading 1–6 (again → paragraph)`: the range"),
            "{problems:?}"
        );
        assert!(has("`Zoom in / out / reset`: the range"), "{problems:?}");
        assert!(has("`Settings`: the range"), "{problems:?}");
    }

    #[test]
    fn heading_ranges_expand_to_one_chord_per_command() {
        let expanded = expand_row("Ctrl+1 … Ctrl+6", 6).unwrap();
        assert_eq!(expanded.len(), 6);
        assert_eq!(list(&expanded[0]), "Ctrl+1");
        assert_eq!(list(&expanded[5]), "Ctrl+6");
        assert!(
            expand_row("Ctrl+1 …", 1).is_err(),
            "no separator with spaces"
        );
    }
}
