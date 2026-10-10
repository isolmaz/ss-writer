//! The command model: every user command of SS Writer as a stable identifier.
//!
//! Menus, the keymap, the command palette, the shortcut documentation and UI Automation
//! names are all derived from [`CommandId`]. The string form ([`CommandId::as_str`]) is
//! persisted (`settings.json` shortcut overrides) and MUST NOT change once released; the
//! Fluent message id ([`CommandId::label_id`]) is looked up in `assets/i18n/<locale>/main.ftl`.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

/// Error returned by [`CommandId::from_str`] for an id that names no command.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("unknown command id `{0}`")]
pub struct UnknownCommand(pub String);

macro_rules! define_groups {
    ($($variant:ident = $id:literal, $label:literal;)*) => {
        /// Menu / palette grouping of commands (the first segment of [`CommandId::as_str`]).
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum CommandGroup {
            $(
                #[doc = $id]
                $variant,
            )*
        }

        impl CommandGroup {
            /// Every group, in display order.
            pub const ALL: &'static [CommandGroup] = &[$(CommandGroup::$variant,)*];

            /// Stable lowercase id (`"format"`).
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(CommandGroup::$variant => $id,)*
                }
            }

            /// Fluent message id of the group title (`"cmd-group-format"`).
            #[must_use]
            pub const fn label_id(self) -> &'static str {
                match self {
                    $(CommandGroup::$variant => $label,)*
                }
            }
        }
    };
}

define_groups! {
    File = "file", "cmd-group-file";
    Tab = "tab", "cmd-group-tab";
    Window = "window", "cmd-group-window";
    Export = "export", "cmd-group-export";
    Edit = "edit", "cmd-group-edit";
    Format = "format", "cmd-group-format";
    Table = "table", "cmd-group-table";
    Search = "search", "cmd-group-search";
    View = "view", "cmd-group-view";
    Sidebar = "sidebar", "cmd-group-sidebar";
    Settings = "settings", "cmd-group-settings";
    Help = "help", "cmd-group-help";
}

macro_rules! define_commands {
    ($($variant:ident = $group:ident, $id:literal, $label:literal;)*) => {
        /// A user-invokable command.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum CommandId {
            $(
                #[doc = $id]
                $variant,
            )*
        }

        impl CommandId {
            /// Every command, in menu / palette order (grouped as [`CommandGroup::ALL`]).
            pub const ALL: &'static [CommandId] = &[$(CommandId::$variant,)*];

            /// Stable persisted id, `"<group>.<name>"` (`"format.bold"`).
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(CommandId::$variant => $id,)*
                }
            }

            /// Fluent message id of the command's label (`"cmd-format-bold"`).
            #[must_use]
            pub const fn label_id(self) -> &'static str {
                match self {
                    $(CommandId::$variant => $label,)*
                }
            }

            /// The menu / palette group this command belongs to.
            #[must_use]
            pub const fn group(self) -> CommandGroup {
                match self {
                    $(CommandId::$variant => CommandGroup::$group,)*
                }
            }
        }

        impl FromStr for CommandId {
            type Err = UnknownCommand;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($id => Ok(CommandId::$variant),)*
                    _ => Err(UnknownCommand(s.to_owned())),
                }
            }
        }
    };
}

define_commands! {
    FileNew = File, "file.new", "cmd-file-new";
    FileNewWindow = File, "file.new_window", "cmd-file-new-window";
    FileOpen = File, "file.open", "cmd-file-open";
    FileOpenFolder = File, "file.open_folder", "cmd-file-open-folder";
    FileSave = File, "file.save", "cmd-file-save";
    FileSaveAs = File, "file.save_as", "cmd-file-save-as";
    FileSaveBackToWord = File, "file.save_back_to_word", "cmd-file-save-back-to-word";
    FilePrint = File, "file.print", "cmd-file-print";
    FileShare = File, "file.share", "cmd-file-share";
    FileExit = File, "file.exit", "cmd-file-exit";
    TabNew = Tab, "tab.new", "cmd-tab-new";
    TabClose = Tab, "tab.close", "cmd-tab-close";
    TabCloseOthers = Tab, "tab.close_others", "cmd-tab-close-others";
    TabCloseToRight = Tab, "tab.close_to_right", "cmd-tab-close-to-right";
    TabReopenClosed = Tab, "tab.reopen_closed", "cmd-tab-reopen-closed";
    TabNext = Tab, "tab.next", "cmd-tab-next";
    TabPrevious = Tab, "tab.previous", "cmd-tab-previous";
    TabCopyPath = Tab, "tab.copy_path", "cmd-tab-copy-path";
    TabRevealInExplorer = Tab, "tab.reveal_in_explorer", "cmd-tab-reveal-in-explorer";
    WindowMergeAll = Window, "window.merge_all", "cmd-window-merge-all";
    ExportPdf = Export, "export.pdf", "cmd-export-pdf";
    ExportPng = Export, "export.png", "cmd-export-png";
    ExportJpg = Export, "export.jpg", "cmd-export-jpg";
    ExportHtml = Export, "export.html", "cmd-export-html";
    ExportEpub = Export, "export.epub", "cmd-export-epub";
    ExportDocx = Export, "export.docx", "cmd-export-docx";
    ExportOdt = Export, "export.odt", "cmd-export-odt";
    ExportRtf = Export, "export.rtf", "cmd-export-rtf";
    ExportTxt = Export, "export.txt", "cmd-export-txt";
    ExportMarkdownWithAttachments = Export, "export.markdown_with_attachments", "cmd-export-markdown-with-attachments";
    ExportTextBundle = Export, "export.textbundle", "cmd-export-textbundle";
    EditUndo = Edit, "edit.undo", "cmd-edit-undo";
    EditRedo = Edit, "edit.redo", "cmd-edit-redo";
    EditCut = Edit, "edit.cut", "cmd-edit-cut";
    EditCopy = Edit, "edit.copy", "cmd-edit-copy";
    EditPaste = Edit, "edit.paste", "cmd-edit-paste";
    EditPastePlain = Edit, "edit.paste_plain", "cmd-edit-paste-plain";
    EditSelectAll = Edit, "edit.select_all", "cmd-edit-select-all";
    EditCopyAsPlainText = Edit, "edit.copy_as_plain_text", "cmd-edit-copy-as-plain-text";
    EditCopyAsMarkdown = Edit, "edit.copy_as_markdown", "cmd-edit-copy-as-markdown";
    EditCopyAsRichText = Edit, "edit.copy_as_rich_text", "cmd-edit-copy-as-rich-text";
    EditCopyAsHtml = Edit, "edit.copy_as_html", "cmd-edit-copy-as-html";
    EditInsertDate = Edit, "edit.insert_date", "cmd-edit-insert-date";
    FormatBold = Format, "format.bold", "cmd-format-bold";
    FormatItalic = Format, "format.italic", "cmd-format-italic";
    FormatUnderline = Format, "format.underline", "cmd-format-underline";
    FormatStrikethrough = Format, "format.strikethrough", "cmd-format-strikethrough";
    FormatHighlight = Format, "format.highlight", "cmd-format-highlight";
    FormatHighlightRed = Format, "format.highlight_red", "cmd-format-highlight-red";
    FormatHighlightOrange = Format, "format.highlight_orange", "cmd-format-highlight-orange";
    FormatHighlightYellow = Format, "format.highlight_yellow", "cmd-format-highlight-yellow";
    FormatHighlightGreen = Format, "format.highlight_green", "cmd-format-highlight-green";
    FormatHighlightBlue = Format, "format.highlight_blue", "cmd-format-highlight-blue";
    FormatHighlightPurple = Format, "format.highlight_purple", "cmd-format-highlight-purple";
    FormatInlineCode = Format, "format.inline_code", "cmd-format-inline-code";
    FormatCodeBlock = Format, "format.code_block", "cmd-format-code-block";
    FormatLink = Format, "format.link", "cmd-format-link";
    FormatImage = Format, "format.image", "cmd-format-image";
    FormatTable = Format, "format.table", "cmd-format-table";
    FormatQuote = Format, "format.quote", "cmd-format-quote";
    FormatCallout = Format, "format.callout", "cmd-format-callout";
    FormatMath = Format, "format.math", "cmd-format-math";
    FormatMathBlock = Format, "format.math_block", "cmd-format-math-block";
    FormatDivider = Format, "format.divider", "cmd-format-divider";
    FormatFootnote = Format, "format.footnote", "cmd-format-footnote";
    FormatPageBreak = Format, "format.page_break", "cmd-format-page-break";
    FormatHeading1 = Format, "format.heading1", "cmd-format-heading1";
    FormatHeading2 = Format, "format.heading2", "cmd-format-heading2";
    FormatHeading3 = Format, "format.heading3", "cmd-format-heading3";
    FormatHeading4 = Format, "format.heading4", "cmd-format-heading4";
    FormatHeading5 = Format, "format.heading5", "cmd-format-heading5";
    FormatHeading6 = Format, "format.heading6", "cmd-format-heading6";
    FormatParagraph = Format, "format.paragraph", "cmd-format-paragraph";
    FormatBulletList = Format, "format.bullet_list", "cmd-format-bullet-list";
    FormatNumberedList = Format, "format.numbered_list", "cmd-format-numbered-list";
    FormatTaskList = Format, "format.task_list", "cmd-format-task-list";
    FormatTaskToggle = Format, "format.task_toggle", "cmd-format-task-toggle";
    TableInsertRowAbove = Table, "table.insert_row_above", "cmd-table-insert-row-above";
    TableInsertRowBelow = Table, "table.insert_row_below", "cmd-table-insert-row-below";
    TableInsertColumnLeft = Table, "table.insert_column_left", "cmd-table-insert-column-left";
    TableInsertColumnRight = Table, "table.insert_column_right", "cmd-table-insert-column-right";
    TableDeleteRow = Table, "table.delete_row", "cmd-table-delete-row";
    TableDeleteColumn = Table, "table.delete_column", "cmd-table-delete-column";
    TableAlignLeft = Table, "table.align_left", "cmd-table-align-left";
    TableAlignCenter = Table, "table.align_center", "cmd-table-align-center";
    TableAlignRight = Table, "table.align_right", "cmd-table-align-right";
    TableCopyAsMarkdown = Table, "table.copy_as_markdown", "cmd-table-copy-as-markdown";
    TableCopyAsCsv = Table, "table.copy_as_csv", "cmd-table-copy-as-csv";
    TableCopyAsHtml = Table, "table.copy_as_html", "cmd-table-copy-as-html";
    TableEditRaw = Table, "table.edit_raw", "cmd-table-edit-raw";
    SearchFind = Search, "search.find", "cmd-search-find";
    SearchReplace = Search, "search.replace", "cmd-search-replace";
    SearchGoToLine = Search, "search.go_to_line", "cmd-search-go-to-line";
    SearchFindNext = Search, "search.find_next", "cmd-search-find-next";
    SearchFindPrevious = Search, "search.find_previous", "cmd-search-find-previous";
    SearchQuickOpen = Search, "search.quick_open", "cmd-search-quick-open";
    SearchCommandPalette = Search, "search.command_palette", "cmd-search-command-palette";
    SearchInFolder = Search, "search.in_folder", "cmd-search-in-folder";
    ViewToggleSidebar = View, "view.toggle_sidebar", "cmd-view-toggle-sidebar";
    ViewToggleInspector = View, "view.toggle_inspector", "cmd-view-toggle-inspector";
    ViewToggleStatusBar = View, "view.toggle_status_bar", "cmd-view-toggle-status-bar";
    ViewToggleFormatPill = View, "view.toggle_format_pill", "cmd-view-toggle-format-pill";
    ViewTypewriterMode = View, "view.typewriter_mode", "cmd-view-typewriter-mode";
    ViewFocusMode = View, "view.focus_mode", "cmd-view-focus-mode";
    ViewFullScreen = View, "view.full_screen", "cmd-view-full-screen";
    ViewZenMode = View, "view.zen_mode", "cmd-view-zen-mode";
    ViewZoomIn = View, "view.zoom_in", "cmd-view-zoom-in";
    ViewZoomOut = View, "view.zoom_out", "cmd-view-zoom-out";
    ViewZoomReset = View, "view.zoom_reset", "cmd-view-zoom-reset";
    ViewRecenterCaret = View, "view.recenter_caret", "cmd-view-recenter-caret";
    ViewFoldAll = View, "view.fold_all", "cmd-view-fold-all";
    ViewUnfoldAll = View, "view.unfold_all", "cmd-view-unfold-all";
    ViewNextPane = View, "view.next_pane", "cmd-view-next-pane";
    SidebarOpen = Sidebar, "sidebar.open", "cmd-sidebar-open";
    SidebarOpenInNewTab = Sidebar, "sidebar.open_in_new_tab", "cmd-sidebar-open-in-new-tab";
    SidebarOpenInNewWindow = Sidebar, "sidebar.open_in_new_window", "cmd-sidebar-open-in-new-window";
    SidebarRename = Sidebar, "sidebar.rename", "cmd-sidebar-rename";
    SidebarDuplicate = Sidebar, "sidebar.duplicate", "cmd-sidebar-duplicate";
    SidebarDelete = Sidebar, "sidebar.delete", "cmd-sidebar-delete";
    SidebarRevealInExplorer = Sidebar, "sidebar.reveal_in_explorer", "cmd-sidebar-reveal-in-explorer";
    SidebarCopyPath = Sidebar, "sidebar.copy_path", "cmd-sidebar-copy-path";
    SidebarCopyRelativePath = Sidebar, "sidebar.copy_relative_path", "cmd-sidebar-copy-relative-path";
    SidebarNewFile = Sidebar, "sidebar.new_file", "cmd-sidebar-new-file";
    SidebarNewFolder = Sidebar, "sidebar.new_folder", "cmd-sidebar-new-folder";
    SidebarCollapseAll = Sidebar, "sidebar.collapse_all", "cmd-sidebar-collapse-all";
    SidebarExpandAll = Sidebar, "sidebar.expand_all", "cmd-sidebar-expand-all";
    SettingsOpen = Settings, "settings.open", "cmd-settings-open";
    SettingsOpenLogsFolder = Settings, "settings.open_logs_folder", "cmd-settings-open-logs-folder";
    SettingsReset = Settings, "settings.reset", "cmd-settings-reset";
    HelpWelcome = Help, "help.welcome", "cmd-help-welcome";
    HelpKeyboardShortcuts = Help, "help.keyboard_shortcuts", "cmd-help-keyboard-shortcuts";
    HelpReportIssue = Help, "help.report_issue", "cmd-help-report-issue";
    HelpCheckForUpdates = Help, "help.check_for_updates", "cmd-help-check-for-updates";
    HelpAbout = Help, "help.about", "cmd-help-about";
}

impl CommandId {
    /// Iterate over every command in menu / palette order.
    pub fn all() -> impl Iterator<Item = CommandId> + Clone {
        Self::ALL.iter().copied()
    }

    /// The heading level (1–6) of `format.headingN`, if this is one.
    #[must_use]
    pub const fn heading_level(self) -> Option<u8> {
        match self {
            CommandId::FormatHeading1 => Some(1),
            CommandId::FormatHeading2 => Some(2),
            CommandId::FormatHeading3 => Some(3),
            CommandId::FormatHeading4 => Some(4),
            CommandId::FormatHeading5 => Some(5),
            CommandId::FormatHeading6 => Some(6),
            _ => None,
        }
    }

    /// The command for heading level 1–6.
    #[must_use]
    pub const fn heading(level: u8) -> Option<CommandId> {
        match level {
            1 => Some(CommandId::FormatHeading1),
            2 => Some(CommandId::FormatHeading2),
            3 => Some(CommandId::FormatHeading3),
            4 => Some(CommandId::FormatHeading4),
            5 => Some(CommandId::FormatHeading5),
            6 => Some(CommandId::FormatHeading6),
            _ => None,
        }
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const MAIN_FTL: &str = include_str!("../../../assets/i18n/en/main.ftl");

    /// Message ids defined in an `.ftl` source (lines of the form `id = …`).
    fn ftl_message_ids(ftl: &str) -> HashSet<&str> {
        ftl.lines()
            .filter(|l| !l.starts_with(['#', ' ', '\t', '.']))
            .filter_map(|l| l.split_once('=').map(|(id, _)| id.trim()))
            .filter(|id| !id.is_empty())
            .collect()
    }

    #[test]
    fn string_ids_round_trip_and_are_unique() {
        let mut seen = HashSet::new();
        for cmd in CommandId::all() {
            assert!(seen.insert(cmd.as_str()), "duplicate id {}", cmd.as_str());
            assert_eq!(cmd.as_str().parse::<CommandId>(), Ok(cmd));
            assert_eq!(cmd.to_string(), cmd.as_str());
        }
        assert_eq!(seen.len(), CommandId::ALL.len());
        assert_eq!(
            "nope.nothing".parse::<CommandId>(),
            Err(UnknownCommand("nope.nothing".into()))
        );
        assert!("".parse::<CommandId>().is_err());
    }

    #[test]
    fn ids_are_group_prefixed_lowercase_snake_case() {
        for cmd in CommandId::all() {
            let id = cmd.as_str();
            let (group, name) = id.split_once('.').expect("group.name");
            assert_eq!(group, cmd.group().as_str(), "{id}");
            assert!(!name.is_empty(), "{id}");
            assert!(
                id.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'_'
                    || b == b'.'),
                "{id}"
            );
            assert_eq!(
                cmd.label_id(),
                format!("cmd-{}", id.replace(['.', '_'], "-")),
                "{id}"
            );
        }
    }

    #[test]
    fn every_command_and_group_has_an_english_message() {
        let ids = ftl_message_ids(MAIN_FTL);
        for cmd in CommandId::all() {
            assert!(
                ids.contains(cmd.label_id()),
                "missing ftl message {}",
                cmd.label_id()
            );
        }
        for group in CommandGroup::ALL {
            assert!(
                ids.contains(group.label_id()),
                "missing ftl message {}",
                group.label_id()
            );
        }
    }

    #[test]
    fn every_group_is_used_and_listed_once() {
        let mut groups = HashSet::new();
        for g in CommandGroup::ALL {
            assert!(groups.insert(*g));
            assert!(CommandId::all().any(|c| c.group() == *g), "{g:?} empty");
        }
    }

    #[test]
    fn spec_commands_exist() {
        for id in [
            "file.new",
            "file.new_window",
            "file.open",
            "file.open_folder",
            "file.save",
            "file.save_as",
            "file.print",
            "file.share",
            "tab.new",
            "tab.close",
            "tab.reopen_closed",
            "tab.next",
            "tab.previous",
            "tab.close_others",
            "tab.close_to_right",
            "window.merge_all",
            "edit.undo",
            "edit.redo",
            "edit.paste_plain",
            "edit.insert_date",
            "edit.copy_as_plain_text",
            "edit.copy_as_markdown",
            "edit.copy_as_rich_text",
            "edit.copy_as_html",
            "search.find",
            "search.replace",
            "search.go_to_line",
            "search.find_next",
            "search.find_previous",
            "search.quick_open",
            "search.command_palette",
            "search.in_folder",
            "format.bold",
            "format.italic",
            "format.underline",
            "format.highlight",
            "format.strikethrough",
            "format.inline_code",
            "format.code_block",
            "format.link",
            "format.task_toggle",
            "format.heading6",
            "view.toggle_sidebar",
            "view.toggle_inspector",
            "view.typewriter_mode",
            "view.focus_mode",
            "view.full_screen",
            "view.zen_mode",
            "view.zoom_in",
            "view.zoom_out",
            "view.zoom_reset",
            "view.recenter_caret",
            "view.fold_all",
            "view.unfold_all",
            "view.next_pane",
            "settings.open",
            "export.pdf",
            "export.png",
            "export.jpg",
            "export.html",
            "export.epub",
            "export.docx",
            "export.odt",
            "export.rtf",
            "export.txt",
            "export.markdown_with_attachments",
            "export.textbundle",
            "table.insert_row_above",
            "table.delete_column",
            "table.align_center",
            "table.copy_as_csv",
            "table.edit_raw",
        ] {
            assert!(id.parse::<CommandId>().is_ok(), "{id}");
        }
    }

    #[test]
    fn heading_helpers_agree() {
        for level in 1..=6 {
            let cmd = CommandId::heading(level).expect("level in range");
            assert_eq!(cmd.heading_level(), Some(level));
            assert_eq!(cmd.as_str(), format!("format.heading{level}"));
        }
        assert_eq!(CommandId::heading(0), None);
        assert_eq!(CommandId::heading(7), None);
        assert_eq!(CommandId::FormatBold.heading_level(), None);
    }
}
