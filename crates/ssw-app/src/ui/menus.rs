//! The File / Edit / Format / View / Window / Help popup menus: native menu models built from
//! [`CommandId`]s, Fluent labels and the keymap's first chord of each command (spec §1, §6, §11).

use ssw_core::command::CommandId;
use ssw_core::i18n::Localizer;
use ssw_core::keymap::Keymap;
use ssw_ui::shell::titlebar::MenuKind;
use ssw_win::menus::{MenuEntry, MenuItem, MenuModel, Submenu};

use CommandId as C;

/// What the menus need to know about the window: check marks and which commands apply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuState {
    /// A document is open: editing, saving and exporting commands are enabled.
    pub has_document: bool,
    /// The active document was imported from a Word / ODT / RTF file that can be written back.
    pub save_back: bool,
    /// The sidebar is shown.
    pub sidebar: bool,
    /// The inspector is shown.
    pub inspector: bool,
    /// The status bar is shown.
    pub status_bar: bool,
    /// The B I U pill is shown.
    pub format_pill: bool,
    /// Typewriter mode is on.
    pub typewriter: bool,
    /// Focus mode is on.
    pub focus_mode: bool,
    /// The window is full screen.
    pub full_screen: bool,
    /// Zen mode is on.
    pub zen: bool,
    /// The active document is a plain-text file (spec §4.1): `.txt`, logs, source code. Only
    /// these have "Render as Markdown" and, while shown as plain text, "Word wrap".
    pub plain_file: bool,
    /// The active plain-text file is shown as Markdown ("Render as Markdown" is on).
    pub render_markdown: bool,
    /// The active document is plain text in a language "Format document" lays out (JSON, XML
    /// or HTML).
    pub formattable: bool,
    /// The active tab shows plain text and wraps its long lines at the window edge (off: the
    /// lines run on and the view scrolls sideways).
    pub word_wrap: bool,
    /// Another window is open: there is something for Window → Merge all windows to merge.
    pub other_windows: bool,
    /// The sidebar shows a workspace: new files and folders, Collapse all and Expand all apply.
    pub workspace: bool,
    /// An entry of the shown sidebar is selected: the entry commands (rename, delete, …) apply.
    pub sidebar_entry: bool,
    /// The selected sidebar entry is a file: only then do the open commands (Open, Open in new
    /// tab, Open in new window) apply.
    pub sidebar_file: bool,
}

impl MenuState {
    /// The check mark of `command`, `None` for commands that are not toggles.
    #[must_use]
    pub fn checked(&self, command: CommandId) -> Option<bool> {
        match command {
            C::ViewToggleSidebar => Some(self.sidebar),
            C::ViewToggleInspector => Some(self.inspector),
            C::ViewToggleStatusBar => Some(self.status_bar),
            C::ViewToggleFormatPill => Some(self.format_pill),
            C::ViewTypewriterMode => Some(self.typewriter),
            C::ViewFocusMode => Some(self.focus_mode),
            C::ViewFullScreen => Some(self.full_screen),
            C::ViewZenMode => Some(self.zen),
            C::ViewWordWrap => Some(self.word_wrap),
            C::ViewRenderAsMarkdown => Some(self.render_markdown),
            _ => None,
        }
    }

    /// Whether `command` can run now: the document commands need a document, merging needs a
    /// second window, saving back needs an imported word-processor file, "Render as Markdown"
    /// needs a plain-text file, "Word wrap" a plain-text file that is shown as plain text and
    /// "Format document" a JSON, XML or HTML document shown as plain text. The menus and the
    /// command palette both ask this.
    #[must_use]
    pub fn enabled(&self, command: CommandId) -> bool {
        match command {
            C::WindowMergeAll => self.other_windows,
            C::FileSaveBackToWord => self.save_back,
            C::EditFormatDocument => self.formattable,
            C::ViewRenderAsMarkdown => self.plain_file,
            C::ViewWordWrap => self.plain_file && !self.render_markdown,
            C::SidebarNewFile
            | C::SidebarNewFolder
            | C::SidebarCollapseAll
            | C::SidebarExpandAll => self.workspace,
            C::SidebarOpen | C::SidebarOpenInNewTab | C::SidebarOpenInNewWindow => {
                self.sidebar_entry && self.sidebar_file
            }
            _ if command.group() == ssw_core::command::CommandGroup::Sidebar => self.sidebar_entry,
            _ => self.has_document || !needs_document(command),
        }
    }
}

/// Commands that act on the active document.
fn needs_document(command: CommandId) -> bool {
    use ssw_core::command::CommandGroup as G;
    matches!(command.group(), G::Edit | G::Format | G::Table | G::Export)
        || matches!(
            command,
            C::FileSave
                | C::FileSaveAs
                | C::FileVersionHistory
                | C::FileExport
                | C::FilePrint
                | C::FileShare
                | C::SearchFind
                | C::SearchReplace
                | C::SearchGoToLine
                | C::SearchFindNext
                | C::SearchFindPrevious
        )
}

/// One section of a menu: items between separators, or a submenu.
enum Section {
    Items(&'static [CommandId]),
    Submenu(&'static str, &'static [CommandId]),
}

const FILE: &[Section] = &[
    Section::Items(&[C::FileNew, C::FileNewWindow, C::FileOpen, C::FileOpenFolder]),
    Section::Items(&[C::TabReopenClosed]),
    Section::Items(&[
        C::FileSave,
        C::FileSaveAs,
        C::FileSaveBackToWord,
        C::FileVersionHistory,
    ]),
    Section::Submenu(
        "window-menu-export",
        &[
            C::ExportPdf,
            C::ExportPng,
            C::ExportJpg,
            C::ExportHtml,
            C::ExportEpub,
            C::ExportDocx,
            C::ExportOdt,
            C::ExportRtf,
            C::ExportTxt,
            C::ExportMarkdownWithAttachments,
            C::ExportTextBundle,
        ],
    ),
    Section::Items(&[C::FileExport, C::FilePrint, C::FileShare]),
    Section::Items(&[C::SettingsOpen]),
    Section::Items(&[C::FileExit]),
];

const EDIT: &[Section] = &[
    Section::Items(&[C::EditUndo, C::EditRedo]),
    Section::Items(&[
        C::EditCut,
        C::EditCopy,
        C::EditPaste,
        C::EditPastePlain,
        C::EditSelectAll,
    ]),
    Section::Submenu(
        "window-menu-copy-as",
        &[
            C::EditCopyAsPlainText,
            C::EditCopyAsMarkdown,
            C::EditCopyAsRichText,
            C::EditCopyAsHtml,
        ],
    ),
    Section::Items(&[C::EditInsertDate]),
    Section::Items(&[C::EditFormatDocument]),
    Section::Items(&[
        C::SearchFind,
        C::SearchReplace,
        C::SearchGoToLine,
        C::SearchFindNext,
        C::SearchFindPrevious,
    ]),
    Section::Items(&[
        C::SearchQuickOpen,
        C::SearchCommandPalette,
        C::SearchInFolder,
    ]),
];

const FORMAT: &[Section] = &[
    Section::Items(&[
        C::FormatBold,
        C::FormatItalic,
        C::FormatUnderline,
        C::FormatStrikethrough,
        C::FormatHighlight,
    ]),
    Section::Submenu(
        "window-menu-headings",
        &[
            C::FormatHeading1,
            C::FormatHeading2,
            C::FormatHeading3,
            C::FormatHeading4,
            C::FormatHeading5,
            C::FormatHeading6,
            C::FormatParagraph,
        ],
    ),
    Section::Submenu(
        "window-menu-lists",
        &[
            C::FormatBulletList,
            C::FormatNumberedList,
            C::FormatTaskList,
            C::FormatTaskToggle,
        ],
    ),
    Section::Items(&[
        C::FormatInlineCode,
        C::FormatCodeBlock,
        C::FormatQuote,
        C::FormatCallout,
        C::FormatMath,
        C::FormatMathBlock,
        C::FormatDivider,
        C::FormatFootnote,
        C::FormatPageBreak,
    ]),
    Section::Items(&[C::FormatLink, C::FormatImage, C::FormatTable]),
];

const VIEW: &[Section] = &[
    Section::Items(&[
        C::ViewToggleSidebar,
        C::ViewToggleInspector,
        C::ViewToggleStatusBar,
        C::ViewToggleFormatPill,
    ]),
    Section::Items(&[
        C::ViewTypewriterMode,
        C::ViewFocusMode,
        C::ViewFullScreen,
        C::ViewZenMode,
    ]),
    Section::Items(&[
        C::ViewZoomIn,
        C::ViewZoomOut,
        C::ViewZoomReset,
        C::ViewRecenterCaret,
    ]),
    Section::Items(&[C::ViewFoldAll, C::ViewUnfoldAll]),
    Section::Items(&[C::ViewWordWrap, C::ViewRenderAsMarkdown]),
    Section::Items(&[C::ViewNextPane]),
];

/// Spec §6: new window, merge all windows, and the tab commands.
const WINDOW: &[Section] = &[
    Section::Items(&[C::FileNewWindow, C::WindowMergeAll]),
    Section::Items(&[
        C::TabNew,
        C::TabClose,
        C::TabReopenClosed,
        C::TabNext,
        C::TabPrevious,
    ]),
];

const HELP: &[Section] = &[
    Section::Items(&[C::HelpWelcome, C::HelpKeyboardShortcuts]),
    Section::Items(&[C::HelpReportIssue, C::HelpCheckForUpdates]),
    Section::Items(&[C::HelpAbout]),
];

/// The id [`ssw_win::menus::PopupMenus::show`] reports for `command`.
#[must_use]
pub fn menu_id(command: CommandId) -> u32 {
    let index = CommandId::ALL
        .iter()
        .position(|&candidate| candidate == command)
        .unwrap_or(0);
    // Ids start at 1; the command list is a few hundred long.
    u32::try_from(index + 1).unwrap_or(u32::MAX)
}

/// The command a chosen menu id stands for.
#[must_use]
pub fn command_for_menu_id(id: u32) -> Option<CommandId> {
    // Ids start at 1 (0 is "nothing chosen"); a `u32` always fits a `usize` on Windows.
    let index = id.checked_sub(1)? as usize;
    CommandId::ALL.get(index).copied()
}

fn item(command: CommandId, i18n: &Localizer, keymap: &Keymap, state: &MenuState) -> MenuEntry {
    let mut entry = MenuItem::new(menu_id(command), i18n.text(command.label_id()));
    if let Some(chord) = keymap.chords_for(command).next() {
        entry = entry.accelerator(chord);
    }
    if !state.enabled(command) {
        entry = entry.disabled();
    }
    if state.checked(command) == Some(true) {
        entry = entry.checked();
    }
    MenuEntry::Item(entry)
}

/// The popup menu of `menu`.
#[must_use]
pub fn build_menu(
    menu: MenuKind,
    i18n: &Localizer,
    keymap: &Keymap,
    state: &MenuState,
) -> MenuModel {
    let sections = match menu {
        MenuKind::File => FILE,
        MenuKind::Edit => EDIT,
        MenuKind::Format => FORMAT,
        MenuKind::View => VIEW,
        MenuKind::Window => WINDOW,
        MenuKind::Help => HELP,
    };
    let mut entries = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        if index > 0 {
            entries.push(MenuEntry::Separator);
        }
        match section {
            Section::Items(commands) => {
                entries.extend(commands.iter().map(|&c| item(c, i18n, keymap, state)));
            }
            Section::Submenu(label, commands) => {
                let children = commands
                    .iter()
                    .map(|&c| item(c, i18n, keymap, state))
                    .collect();
                let mut submenu = Submenu::new(i18n.text(label), children);
                if !state.has_document {
                    submenu = submenu.disabled();
                }
                entries.push(MenuEntry::Submenu(submenu));
            }
        }
    }
    MenuModel::new(entries)
}

#[cfg(test)]
mod tests {
    use ssw_core::keymap::KeyChord;

    use super::*;
    use crate::i18n::localizer_for;

    fn items(model: &MenuModel) -> Vec<&MenuItem> {
        let mut found = Vec::new();
        fn walk<'a>(model: &'a MenuModel, found: &mut Vec<&'a MenuItem>) {
            for entry in &model.entries {
                match entry {
                    MenuEntry::Item(item) => found.push(item),
                    MenuEntry::Submenu(submenu) => walk(&submenu.menu, found),
                    MenuEntry::Separator => {}
                }
            }
        }
        walk(model, &mut found);
        found
    }

    #[test]
    fn ids_round_trip_for_every_command() {
        for command in CommandId::all() {
            assert_eq!(command_for_menu_id(menu_id(command)), Some(command));
        }
        assert_eq!(command_for_menu_id(0), None);
        assert_eq!(command_for_menu_id(u32::MAX), None);
    }

    #[test]
    fn every_menu_item_has_a_localized_label_and_the_keymap_shortcut() {
        let i18n = localizer_for("en");
        let keymap = Keymap::default();
        let state = MenuState {
            has_document: true,
            ..MenuState::default()
        };
        for kind in MenuKind::ALL {
            let model = build_menu(kind, &i18n, &keymap, &state);
            assert!(!model.entries.is_empty());
            for entry in &model.entries {
                if let MenuEntry::Submenu(submenu) = entry {
                    assert!(
                        !submenu.label.starts_with("window-menu-"),
                        "missing label {}",
                        submenu.label
                    );
                }
            }
            for menu_item in items(&model) {
                let command = command_for_menu_id(menu_item.id).unwrap();
                assert_eq!(menu_item.label, i18n.text(command.label_id()));
                assert!(!menu_item.label.starts_with("cmd-"), "{}", menu_item.label);
            }
        }
        let file = build_menu(MenuKind::File, &i18n, &keymap, &state);
        let save = items(&file)
            .into_iter()
            .find(|entry| entry.id == menu_id(CommandId::FileSave))
            .unwrap();
        assert_eq!(
            save.accelerator,
            Some(KeyChord::new(ssw_core::keymap::vk::S).with_ctrl())
        );
        assert_eq!(save.label, "Save");
    }

    #[test]
    fn view_toggles_are_checked_and_document_commands_follow_the_document() {
        let i18n = localizer_for("en");
        let keymap = Keymap::default();
        let state = MenuState {
            sidebar: true,
            zen: true,
            ..MenuState::default()
        };
        let view = build_menu(MenuKind::View, &i18n, &keymap, &state);
        let checked: Vec<_> = items(&view)
            .into_iter()
            .filter(|entry| entry.check != ssw_win::menus::CheckState::None)
            .map(|entry| command_for_menu_id(entry.id).unwrap())
            .collect();
        assert_eq!(
            checked,
            [CommandId::ViewToggleSidebar, CommandId::ViewZenMode]
        );
        let edit = build_menu(MenuKind::Edit, &i18n, &keymap, &state);
        let undo = items(&edit)
            .into_iter()
            .find(|entry| entry.id == menu_id(CommandId::EditUndo))
            .unwrap();
        assert!(!undo.enabled);
        let find = items(&edit)
            .into_iter()
            .find(|entry| entry.id == menu_id(CommandId::SearchQuickOpen))
            .unwrap();
        assert!(find.enabled);
        assert!(state.enabled(CommandId::FileNew));
        assert!(!state.enabled(CommandId::FileSave));
    }

    #[test]
    fn saving_back_needs_an_imported_word_processor_document() {
        let plain = MenuState {
            has_document: true,
            ..MenuState::default()
        };
        assert!(plain.enabled(CommandId::FileSave));
        assert!(!plain.enabled(CommandId::FileSaveBackToWord));
        let imported = MenuState {
            save_back: true,
            ..plain
        };
        assert!(imported.enabled(CommandId::FileSaveBackToWord));
        let i18n = localizer_for("en");
        let file = build_menu(MenuKind::File, &i18n, &Keymap::default(), &plain);
        let back = items(&file)
            .into_iter()
            .find(|entry| entry.id == menu_id(CommandId::FileSaveBackToWord))
            .unwrap();
        assert!(!back.enabled);
    }

    #[test]
    fn format_document_follows_the_edit_menu_items_for_the_document_and_needs_a_language() {
        let i18n = localizer_for("en");
        let keymap = Keymap::default();
        // The item as the Edit menu lists it: label, enabled, has a shortcut.
        let item = |state: &MenuState| {
            let edit = build_menu(MenuKind::Edit, &i18n, &keymap, state);
            items(&edit)
                .into_iter()
                .find(|entry| entry.id == menu_id(CommandId::EditFormatDocument))
                .map(|entry| {
                    (
                        entry.label.clone(),
                        entry.enabled,
                        entry.accelerator.is_some(),
                    )
                })
        };
        let none = MenuState::default();
        let document = MenuState {
            has_document: true,
            ..none
        };
        let formattable = MenuState {
            formattable: true,
            ..document
        };
        // Listed for every document, greyed unless the document is JSON, XML or HTML as text;
        // spec §11: no Ctrl+Alt default, so no shortcut either.
        for state in [none, document] {
            assert_eq!(
                item(&state),
                Some(("Format document".to_owned(), false, false))
            );
            assert!(!state.enabled(CommandId::EditFormatDocument));
        }
        assert_eq!(
            item(&formattable),
            Some(("Format document".to_owned(), true, false))
        );
        assert!(formattable.enabled(CommandId::EditFormatDocument));
        // The other Edit commands do not look at the language.
        assert!(document.enabled(CommandId::EditInsertDate));
        assert!(!none.enabled(CommandId::EditInsertDate));
    }

    #[test]
    fn sidebar_commands_need_a_workspace_and_entry_commands_a_selection() {
        let none = MenuState::default();
        assert!(!none.enabled(CommandId::SidebarNewFile));
        assert!(!none.enabled(CommandId::SidebarRename));
        let workspace = MenuState {
            workspace: true,
            ..none
        };
        assert!(workspace.enabled(CommandId::SidebarCollapseAll));
        assert!(!workspace.enabled(CommandId::SidebarDelete));
        let selected = MenuState {
            sidebar_entry: true,
            ..workspace
        };
        assert!(selected.enabled(CommandId::SidebarDelete));
        assert!(selected.enabled(CommandId::SidebarCopyPath));
        // The open commands are for a selected file: a selected folder is not opened.
        let open = [
            CommandId::SidebarOpen,
            CommandId::SidebarOpenInNewTab,
            CommandId::SidebarOpenInNewWindow,
        ];
        for command in open {
            assert!(!selected.enabled(command), "{command}");
        }
        let file = MenuState {
            sidebar_file: true,
            ..selected
        };
        for command in open {
            assert!(file.enabled(command), "{command}");
        }
        assert!(file.enabled(CommandId::SidebarRename));
        // A file flag without a selected entry enables nothing.
        let stale = MenuState {
            sidebar_file: true,
            ..workspace
        };
        for command in open {
            assert!(!stale.enabled(command), "{command}");
        }
    }

    #[test]
    fn the_help_menu_lists_the_help_commands_and_works_without_a_document() {
        let i18n = localizer_for("en");
        let keymap = Keymap::default();
        let help = build_menu(MenuKind::Help, &i18n, &keymap, &MenuState::default());
        let entries = items(&help);
        let commands: Vec<_> = entries
            .iter()
            .map(|entry| command_for_menu_id(entry.id).unwrap())
            .collect();
        assert_eq!(
            commands,
            [
                CommandId::HelpWelcome,
                CommandId::HelpKeyboardShortcuts,
                CommandId::HelpReportIssue,
                CommandId::HelpCheckForUpdates,
                CommandId::HelpAbout,
            ]
        );
        let labels: Vec<_> = entries.iter().map(|entry| entry.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Welcome document",
                "Keyboard shortcuts",
                "Report an issue",
                "Check for updates",
                "About SS Writer",
            ]
        );
        assert!(entries.iter().all(|entry| entry.enabled));
        let separators = help
            .entries
            .iter()
            .filter(|entry| matches!(entry, MenuEntry::Separator))
            .count();
        assert_eq!(separators, 2);
    }

    #[test]
    fn the_window_menu_lists_new_window_merge_all_and_the_tab_commands() {
        let i18n = localizer_for("en");
        let keymap = Keymap::default();
        let menu = |other_windows: bool| {
            let state = MenuState {
                other_windows,
                ..MenuState::default()
            };
            build_menu(MenuKind::Window, &i18n, &keymap, &state)
        };
        let alone = menu(false);
        let entries = items(&alone);
        let commands: Vec<_> = entries
            .iter()
            .map(|entry| command_for_menu_id(entry.id).unwrap())
            .collect();
        assert_eq!(
            commands,
            [
                CommandId::FileNewWindow,
                CommandId::WindowMergeAll,
                CommandId::TabNew,
                CommandId::TabClose,
                CommandId::TabReopenClosed,
                CommandId::TabNext,
                CommandId::TabPrevious,
            ]
        );
        let labels: Vec<_> = entries.iter().map(|entry| entry.label.as_str()).collect();
        assert_eq!(labels[..2], ["New window", "Merge all windows"]);
        assert_eq!(
            entries[0].accelerator,
            Some(
                KeyChord::new(ssw_core::keymap::vk::N)
                    .with_ctrl()
                    .with_shift()
            )
        );
        // Merging needs another window; everything else works without a document.
        let enabled: Vec<_> = entries.iter().map(|entry| entry.enabled).collect();
        assert_eq!(enabled, [true, false, true, true, true, true, true]);
        let together = menu(true);
        assert!(items(&together).iter().all(|entry| entry.enabled));
    }

    #[test]
    fn the_file_menu_offers_the_version_history_of_an_open_document() {
        let i18n = localizer_for("en");
        let keymap = Keymap::default();
        let history = |has_document: bool| {
            let state = MenuState {
                has_document,
                ..MenuState::default()
            };
            let file = build_menu(MenuKind::File, &i18n, &keymap, &state);
            items(&file)
                .into_iter()
                .find(|entry| entry.id == menu_id(CommandId::FileVersionHistory))
                .map(|entry| (entry.label.clone(), entry.enabled))
        };
        assert_eq!(history(true), Some(("Version history…".to_owned(), true)));
        assert_eq!(history(false), Some(("Version history…".to_owned(), false)));
    }

    /// The View menu's Word wrap and Render as Markdown items: (label, enabled, check mark).
    fn plain_items(state: &MenuState) -> Vec<(String, bool, ssw_win::menus::CheckState)> {
        let view = build_menu(
            MenuKind::View,
            &localizer_for("en"),
            &Keymap::default(),
            state,
        );
        // An item the menu does not list is missing from the result: the comparisons of the
        // tests fail on it.
        [CommandId::ViewWordWrap, CommandId::ViewRenderAsMarkdown]
            .into_iter()
            .filter_map(|command| {
                items(&view)
                    .into_iter()
                    .find(|entry| entry.id == menu_id(command))
                    .map(|entry| (entry.label.clone(), entry.enabled, entry.check))
            })
            .collect()
    }

    #[test]
    fn word_wrap_and_render_as_markdown_are_checkable_view_items_for_plain_files() {
        use ssw_win::menus::CheckState::{Check, None as Unchecked};
        // No plain-text file in front: both are listed, disabled and unmarked (also with a
        // Markdown document open).
        for has_document in [false, true] {
            let markdown = MenuState {
                has_document,
                ..MenuState::default()
            };
            assert_eq!(
                plain_items(&markdown),
                [
                    ("Word wrap".to_owned(), false, Unchecked),
                    ("Render as Markdown".to_owned(), false, Unchecked),
                ]
            );
            assert!(!markdown.enabled(CommandId::ViewWordWrap));
            assert!(!markdown.enabled(CommandId::ViewRenderAsMarkdown));
        }

        // A plain file shown as plain text: wrapping can be switched, Markdown rendering on.
        let plain = MenuState {
            has_document: true,
            plain_file: true,
            word_wrap: true,
            ..MenuState::default()
        };
        assert_eq!(
            plain_items(&plain),
            [
                ("Word wrap".to_owned(), true, Check),
                ("Render as Markdown".to_owned(), true, Unchecked),
            ]
        );
        let unwrapped = MenuState {
            word_wrap: false,
            ..plain
        };
        assert_eq!(
            plain_items(&unwrapped)[0],
            ("Word wrap".to_owned(), true, Unchecked)
        );

        // Rendered as Markdown: the switch stays enabled (and marked) so it can be turned off;
        // there is no plain text left to wrap.
        let rendered = MenuState {
            render_markdown: true,
            word_wrap: false,
            ..plain
        };
        assert_eq!(
            plain_items(&rendered),
            [
                ("Word wrap".to_owned(), false, Unchecked),
                ("Render as Markdown".to_owned(), true, Check),
            ]
        );
    }

    #[test]
    fn the_plain_mode_items_have_no_default_shortcut_and_follow_the_folding_items() {
        let i18n = localizer_for("en");
        let keymap = Keymap::default();
        for command in [CommandId::ViewWordWrap, CommandId::ViewRenderAsMarkdown] {
            assert_eq!(keymap.chords_for(command).count(), 0, "{command}");
        }
        let view = build_menu(MenuKind::View, &i18n, &keymap, &MenuState::default());
        let commands: Vec<_> = items(&view)
            .iter()
            .map(|entry| command_for_menu_id(entry.id).unwrap())
            .collect();
        let fold = commands
            .iter()
            .position(|&command| command == CommandId::ViewUnfoldAll)
            .unwrap();
        assert_eq!(
            commands[fold + 1..fold + 3],
            [CommandId::ViewWordWrap, CommandId::ViewRenderAsMarkdown]
        );
        let separators = view
            .entries
            .iter()
            .filter(|entry| matches!(entry, MenuEntry::Separator))
            .count();
        assert_eq!(
            separators, 5,
            "a section of its own between folding and panes"
        );
    }
}
