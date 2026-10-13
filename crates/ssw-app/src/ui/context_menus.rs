//! Context menus (spec §2.6, §3.1, §4.2, §5, §6, §8): the tab menu, the sidebar item / root /
//! header menus, the editor menus (plain edit menu, spelling suggestions, table cell menu, callout
//! type) and the status bar's encoding and line-ending menus.
//!
//! A window callback never shows a menu. The loop owner turns a [`ShellAction`] into a
//! [`Popup`] with [`Controller::popup_for`], shows [`Popup::model`] at [`Popup::at`] with
//! `ssw_win::menus::PopupMenus`, and hands the chosen id to [`Controller::run_popup_choice`].
//! Sidebar events that are not menus (a committed rename, the Delete key) go through
//! [`Controller::sidebar_file_event`].

use std::path::{Path, PathBuf};
use std::rc::Rc;

use ssw_core::command::CommandId;
use ssw_core::edit::Selection;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::keymap::Keymap;
use ssw_core::markdown::ast::CalloutKind;
use ssw_core::ops::blocks;
use ssw_core::text::TextRange;
use ssw_io::encoding::{LineEnding, TextEncoding, TextFormat};
use ssw_io::fileops::{self, FileOpError, NameError};
use ssw_ui::editor::EditorEvent;
use ssw_ui::editor::blocks_media::MediaEvent;
use ssw_ui::editor::spell::SpellCheck;
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::services::{
    ClipboardContent, MessageBoxButtons, MessageBoxIcon, MessageBoxOptions, MessageBoxResult,
};
use ssw_ui::shell::sidebar::{HeaderMenu, NodeKey, SidebarEntry, SidebarEvent};
use ssw_ui::shell::statusbar::StatusEvent;
use ssw_ui::shell::tabs::{TabMenu, TabsEvent};
use ssw_ui::shell::titlebar::TitleEvent;
use ssw_win::menus::{MenuEntry, MenuItem, MenuModel, Submenu};

use CommandId as C;

use super::controller::{Browsed, Controller, WindowCell};
use super::export::DiagramFormat;
use super::menus::menu_id;
use super::shell_view::{ShellAction, ShellView};
use crate::documents::{DocEntry, DocError, Source, canonical_path};
use crate::workers::WindowId;

/// Ids of menu items that are not commands start here (above every command id).
const EXTRA_ID_BASE: u32 = 100_000;

/// What a menu item does.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Run a command in the window.
    Command(CommandId),
    /// Make tab `key` the active one, then run a tab command on it.
    TabCommand {
        /// The tab the menu was opened on.
        key: u64,
        /// The command.
        command: CommandId,
    },
    /// A sidebar command on an entry (`None`: the workspace root).
    Sidebar {
        /// The command.
        command: SidebarCommand,
        /// The entry the menu was opened on.
        target: Option<NodeKey>,
    },
    /// Replace a misspelled word.
    Spell {
        /// The word's source range.
        range: TextRange,
        /// The chosen suggestion.
        replacement: String,
    },
    /// Export a Mermaid diagram as a picture.
    MermaidExport {
        /// The diagram source.
        source: String,
        /// PNG or SVG.
        format: DiagramFormat,
    },
    /// Change the type of a callout.
    Callout {
        /// The callout block's source range.
        range: TextRange,
        /// The chosen type.
        kind: CalloutKind,
    },
    /// Read the active document's file again, decoded as this encoding.
    ReopenWithEncoding(TextEncoding),
    /// Write the active document in this encoding from now on.
    SaveWithEncoding {
        /// The encoding.
        encoding: TextEncoding,
        /// Whether a byte-order mark is written.
        bom: bool,
    },
    /// Write the active document with these line endings from now on.
    LineEnding(LineEnding),
}

/// What an item of a sidebar menu does to its entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarCommand {
    /// Open the file in a tab of the window.
    Open,
    /// Open the file in a tab (the same as Open: a file has one tab).
    OpenInNewTab,
    /// Open the file in a new window.
    OpenInNewWindow,
    /// Start renaming the entry in place.
    Rename,
    /// Copy the entry next to itself.
    Duplicate,
    /// Move the entry to the Recycle Bin.
    Delete,
    /// Show the entry in File Explorer.
    RevealInExplorer,
    /// Copy the full path.
    CopyPath,
    /// Copy the path relative to the workspace root.
    CopyRelativePath,
    /// Create a file in the folder (or next to the file).
    NewFile,
    /// Create a folder in the folder (or next to the file).
    NewFolder,
    /// Collapse every folder.
    CollapseAll,
    /// Expand every folder.
    ExpandAll,
}

impl SidebarCommand {
    /// The command the item shows (its label, shortcut and menu id).
    #[must_use]
    pub fn id(self) -> CommandId {
        match self {
            Self::Open => C::SidebarOpen,
            Self::OpenInNewTab => C::SidebarOpenInNewTab,
            Self::OpenInNewWindow => C::SidebarOpenInNewWindow,
            Self::Rename => C::SidebarRename,
            Self::Duplicate => C::SidebarDuplicate,
            Self::Delete => C::SidebarDelete,
            Self::RevealInExplorer => C::SidebarRevealInExplorer,
            Self::CopyPath => C::SidebarCopyPath,
            Self::CopyRelativePath => C::SidebarCopyRelativePath,
            Self::NewFile => C::SidebarNewFile,
            Self::NewFolder => C::SidebarNewFolder,
            Self::CollapseAll => C::SidebarCollapseAll,
            Self::ExpandAll => C::SidebarExpandAll,
        }
    }

    /// The item behind a sidebar command id (the palette, a shortcut); `None` for other
    /// commands.
    #[must_use]
    pub fn from_id(command: CommandId) -> Option<Self> {
        Self::ALL.into_iter().find(|item| item.id() == command)
    }

    /// Whether the item works on the workspace root rather than on a selected entry.
    #[must_use]
    pub fn on_root(self) -> bool {
        matches!(
            self,
            Self::NewFile | Self::NewFolder | Self::CollapseAll | Self::ExpandAll
        )
    }

    const ALL: [Self; 13] = [
        Self::Open,
        Self::OpenInNewTab,
        Self::OpenInNewWindow,
        Self::Rename,
        Self::Duplicate,
        Self::Delete,
        Self::RevealInExplorer,
        Self::CopyPath,
        Self::CopyRelativePath,
        Self::NewFile,
        Self::NewFolder,
        Self::CollapseAll,
        Self::ExpandAll,
    ];
}

/// A popup menu to show: the model, where, and what each item does.
#[derive(Debug, Clone)]
pub struct Popup {
    /// The menu for `ssw_win::menus::PopupMenus::show`.
    pub model: MenuModel,
    /// The top-left corner in window DIPs.
    pub at: Point,
    actions: Vec<(u32, Action)>,
}

impl Popup {
    /// What the item `id` does; `None` for ids this menu does not contain.
    #[must_use]
    pub fn action(&self, id: u32) -> Option<&Action> {
        self.actions
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .map(|(_, action)| action)
    }

    /// The ids of the items that do something, in menu order.
    #[must_use]
    pub fn ids(&self) -> Vec<u32> {
        self.actions.iter().map(|(id, _)| *id).collect()
    }
}

/// Collects the entries and the actions of one menu.
struct Builder<'a> {
    i18n: &'a Localizer,
    keymap: &'a Keymap,
    entries: Vec<MenuEntry>,
    actions: Vec<(u32, Action)>,
    extra: u32,
}

impl<'a> Builder<'a> {
    fn new(i18n: &'a Localizer, keymap: &'a Keymap) -> Self {
        Self {
            i18n,
            keymap,
            entries: Vec::new(),
            actions: Vec::new(),
            extra: EXTRA_ID_BASE,
        }
    }

    fn separator(&mut self) {
        if !matches!(self.entries.last(), None | Some(MenuEntry::Separator)) {
            self.entries.push(MenuEntry::Separator);
        }
    }

    /// An item for `command` (label and shortcut from Fluent and the keymap).
    /// The sidebar command `command` on the entry `target`.
    fn sidebar(&mut self, command: SidebarCommand, target: Option<NodeKey>) {
        self.command(command.id(), true, Action::Sidebar { command, target });
    }

    fn command(&mut self, command: CommandId, enabled: bool, action: Action) {
        let id = menu_id(command);
        let mut item = MenuItem::new(id, self.i18n.text(command.label_id()));
        if let Some(chord) = self.keymap.chords_for(command).next() {
            item = item.accelerator(chord);
        }
        if !enabled {
            item = item.disabled();
        }
        self.entries.push(MenuEntry::Item(item));
        if enabled {
            self.actions.push((id, action));
        }
    }

    /// An item with its own label.
    fn custom(
        &mut self,
        label: String,
        item: impl FnOnce(MenuItem) -> MenuItem,
        action: Option<Action>,
    ) {
        self.extra += 1;
        let id = self.extra;
        self.entries
            .push(MenuEntry::Item(item(MenuItem::new(id, label))));
        if let Some(action) = action {
            self.actions.push((id, action));
        }
    }

    /// A submenu whose items `fill` adds. A disabled submenu offers no actions.
    fn submenu(&mut self, label: String, enabled: bool, fill: impl FnOnce(&mut Self)) {
        let outer = std::mem::take(&mut self.entries);
        let known = self.actions.len();
        fill(self);
        if !enabled {
            self.actions.truncate(known);
        }
        let children = std::mem::replace(&mut self.entries, outer);
        let mut submenu = Submenu::new(label, children);
        if !enabled {
            submenu = submenu.disabled();
        }
        self.entries.push(MenuEntry::Submenu(submenu));
    }

    /// Every menu ends with a command, custom item or submenu: `separator` is only called
    /// between items, so there is no trailing separator to trim.
    fn finish(self, at: Point) -> Popup {
        Popup {
            model: MenuModel::new(self.entries),
            at,
            actions: self.actions,
        }
    }
}

/// The bottom-left corner of `anchor`: where a menu for a rectangle opens.
fn below(anchor: Rect) -> Point {
    Point::new(anchor.x, anchor.y + anchor.height)
}

/// The tab menu (spec §6): Close, Close others, Close to the right, Copy path, Reveal.
#[must_use]
pub fn tab_menu(key: u64, at: Point, menu: TabMenu, i18n: &Localizer, keymap: &Keymap) -> Popup {
    let mut builder = Builder::new(i18n, keymap);
    let tab = |command| Action::TabCommand { key, command };
    builder.command(C::TabClose, true, tab(C::TabClose));
    builder.command(
        C::TabCloseOthers,
        menu.can_close_others,
        tab(C::TabCloseOthers),
    );
    builder.command(
        C::TabCloseToRight,
        menu.can_close_to_right,
        tab(C::TabCloseToRight),
    );
    builder.separator();
    builder.command(C::TabCopyPath, menu.has_path, tab(C::TabCopyPath));
    builder.command(
        C::TabRevealInExplorer,
        menu.has_path,
        tab(C::TabRevealInExplorer),
    );
    builder.finish(at)
}

/// The selected entry of the sidebar of `view` and whether it is a folder; `None` without a
/// selection and while the sidebar is not shown (hidden, or a mode without side panes).
pub(super) fn selected_sidebar_entry(view: &ShellView) -> Option<(NodeKey, bool)> {
    let sidebar = view
        .sidebar()
        .filter(|_| view.layout().rects().sidebar.is_some())?;
    let key = sidebar.selected_key()?;
    let folder = sidebar.entry(key).is_some_and(SidebarEntry::is_folder);
    Some((key, folder))
}

/// The sidebar menu of the entry `target` (`None`: the root, or empty space): `folder` says
/// whether the entry is a folder.
#[must_use]
pub fn sidebar_menu(
    target: Option<NodeKey>,
    folder: bool,
    at: Point,
    i18n: &Localizer,
    keymap: &Keymap,
) -> Popup {
    use SidebarCommand as S;
    let mut builder = Builder::new(i18n, keymap);
    if target.is_some() {
        if !folder {
            for command in [S::Open, S::OpenInNewTab, S::OpenInNewWindow] {
                builder.sidebar(command, target);
            }
            builder.separator();
        }
        for command in [S::Rename, S::Duplicate, S::Delete] {
            builder.sidebar(command, target);
        }
        builder.separator();
    }
    builder.sidebar(S::RevealInExplorer, target);
    builder.sidebar(S::CopyPath, target);
    if target.is_some() {
        builder.sidebar(S::CopyRelativePath, target);
    }
    builder.separator();
    builder.sidebar(S::NewFile, target);
    builder.sidebar(S::NewFolder, target);
    builder.finish(at)
}

/// The sidebar header menus: `+` (New file, New folder) and `⋯` (Collapse all, Expand all).
#[must_use]
pub fn header_menu(menu: HeaderMenu, anchor: Point, i18n: &Localizer, keymap: &Keymap) -> Popup {
    let mut builder = Builder::new(i18n, keymap);
    let commands = match menu {
        HeaderMenu::New => [SidebarCommand::NewFile, SidebarCommand::NewFolder],
        HeaderMenu::Options => [SidebarCommand::CollapseAll, SidebarCommand::ExpandAll],
    };
    for command in commands {
        builder.sidebar(command, None);
    }
    builder.finish(anchor)
}

/// The plain editor menu: Cut, Copy, Paste, Select all. Cut and Copy need a selection.
#[must_use]
pub fn editor_menu(has_selection: bool, at: Point, i18n: &Localizer, keymap: &Keymap) -> Popup {
    let mut builder = Builder::new(i18n, keymap);
    builder.command(C::EditCut, has_selection, Action::Command(C::EditCut));
    builder.command(C::EditCopy, has_selection, Action::Command(C::EditCopy));
    builder.command(C::EditPaste, true, Action::Command(C::EditPaste));
    builder.separator();
    builder.command(C::EditSelectAll, true, Action::Command(C::EditSelectAll));
    builder.finish(at)
}

/// The table cell menu (spec §3.1). A table shown as raw Markdown only offers "Edit as raw".
#[must_use]
pub fn table_menu(raw: bool, anchor: Rect, i18n: &Localizer, keymap: &Keymap) -> Popup {
    let mut builder = Builder::new(i18n, keymap);
    let groups: &[&[CommandId]] = if raw {
        &[&[C::TableEditRaw]]
    } else {
        &[
            &[
                C::TableInsertRowAbove,
                C::TableInsertRowBelow,
                C::TableInsertColumnLeft,
                C::TableInsertColumnRight,
                C::TableDeleteRow,
                C::TableDeleteColumn,
            ],
            &[C::TableAlignLeft, C::TableAlignCenter, C::TableAlignRight],
            &[
                C::TableCopyAsMarkdown,
                C::TableCopyAsCsv,
                C::TableCopyAsHtml,
            ],
            &[C::TableEditRaw],
        ]
    };
    for group in groups {
        builder.separator();
        for &command in *group {
            builder.command(command, true, Action::Command(command));
        }
    }
    builder.finish(below(anchor))
}

/// The suggestions of a misspelled word, or a disabled note when there are none.
#[must_use]
pub fn spell_menu(
    range: TextRange,
    suggestions: &[String],
    anchor: Rect,
    i18n: &Localizer,
    keymap: &Keymap,
) -> Popup {
    let mut builder = Builder::new(i18n, keymap);
    if suggestions.is_empty() {
        builder.custom(i18n.text("spell-no-suggestions"), MenuItem::disabled, None);
    }
    for suggestion in suggestions {
        builder.custom(
            suggestion.clone(),
            |item| item,
            Some(Action::Spell {
                range,
                replacement: suggestion.clone(),
            }),
        );
    }
    builder.finish(below(anchor))
}

/// The menu of a Mermaid diagram: Export as PNG, Export as SVG.
#[must_use]
pub fn mermaid_menu(source: &str, anchor: Rect, i18n: &Localizer, keymap: &Keymap) -> Popup {
    let mut builder = Builder::new(i18n, keymap);
    for (label, format) in [
        ("media-mermaid-export-png", DiagramFormat::Png),
        ("media-mermaid-export-svg", DiagramFormat::Svg),
    ] {
        builder.custom(
            i18n.text(label),
            |item| item,
            Some(Action::MermaidExport {
                source: source.to_owned(),
                format,
            }),
        );
    }
    builder.finish(below(anchor))
}

/// The five callout types, the current one marked.
#[must_use]
pub fn callout_menu(
    range: TextRange,
    current: CalloutKind,
    anchor: Rect,
    i18n: &Localizer,
    keymap: &Keymap,
) -> Popup {
    let mut builder = Builder::new(i18n, keymap);
    for kind in CalloutKind::ALL {
        let label = i18n.text(&format!("blocks-callout-{}", kind.keyword().to_lowercase()));
        let selected = kind == current;
        builder.custom(
            label,
            move |item| if selected { item.radio() } else { item },
            Some(Action::Callout { range, kind }),
        );
    }
    builder.finish(below(anchor))
}

/// What the encoding and line-ending menus of the status bar (spec §4.2, §8) need to know about
/// the active document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatMenu {
    /// Encoding, BOM and line endings the document is written with.
    pub format: TextFormat,
    /// A plain text file: only then are encoding and line endings the file's own (a document
    /// imported from Word, ODT or RTF, or a bundle, is written by its own converter).
    pub text_file: bool,
    /// The document has a file that can be read again.
    pub on_disk: bool,
}

/// The status bar's encoding menu: "Reopen with encoding" (decode the file again) and "Save with
/// encoding" (write the document in another one), each listing [`TextEncoding::selectable`].
#[must_use]
pub fn encoding_menu(menu: FormatMenu, anchor: Rect, i18n: &Localizer, keymap: &Keymap) -> Popup {
    let mut builder = Builder::new(i18n, keymap);
    let encodings = TextEncoding::selectable();
    builder.submenu(
        i18n.text("window-menu-reopen-encoding"),
        menu.text_file && menu.on_disk,
        |builder| {
            for &encoding in &encodings {
                let current = menu.format.encoding == encoding;
                builder.custom(
                    encoding.name().to_owned(),
                    move |item| if current { item.radio() } else { item },
                    Some(Action::ReopenWithEncoding(encoding)),
                );
            }
        },
    );
    builder.submenu(
        i18n.text("window-menu-save-encoding"),
        menu.text_file,
        |builder| {
            for &encoding in &encodings {
                // UTF-8 is written with or without a byte-order mark; UTF-16 and UTF-32 always
                // with one, the legacy code pages never.
                let marks: &[bool] = match encoding {
                    TextEncoding::Utf8 => &[false, true],
                    other if other.supports_bom() => &[true],
                    _ => &[false],
                };
                for &bom in marks {
                    let label = if bom && encoding == TextEncoding::Utf8 {
                        i18n.format(
                            "window-encoding-bom",
                            &[("name", Arg::Str(encoding.name()))],
                        )
                    } else {
                        encoding.name().to_owned()
                    };
                    let current = menu.format.encoding == encoding && menu.format.bom == bom;
                    builder.custom(
                        label,
                        move |item| if current { item.radio() } else { item },
                        Some(Action::SaveWithEncoding { encoding, bom }),
                    );
                }
            }
        },
    );
    builder.finish(below(anchor))
}

/// The status bar's line-ending menu: LF, CRLF, CR, the document's current style marked.
#[must_use]
pub fn line_ending_menu(
    menu: FormatMenu,
    anchor: Rect,
    i18n: &Localizer,
    keymap: &Keymap,
) -> Popup {
    let mut builder = Builder::new(i18n, keymap);
    for (label, line_ending) in [
        ("statusbar-eol-lf", LineEnding::Lf),
        ("statusbar-eol-crlf", LineEnding::Crlf),
        ("statusbar-eol-cr", LineEnding::Cr),
    ] {
        let current = menu.format.line_ending == line_ending;
        let enabled = menu.text_file;
        builder.custom(
            i18n.text(label),
            move |item| {
                let item = if current { item.radio() } else { item };
                if enabled { item } else { item.disabled() }
            },
            enabled.then_some(Action::LineEnding(line_ending)),
        );
    }
    builder.finish(below(anchor))
}

impl Controller {
    /// The popup menu `action` asks for (tab, sidebar, editor, spelling, table, callout, status
    /// bar), if it is a menu request.
    #[must_use]
    pub fn popup_for(&self, window: WindowId, action: &ShellAction) -> Option<Popup> {
        let cell = self.window(window)?;
        let i18n = self.i18n();
        let keymap = self.keymap();
        match action {
            ShellAction::Title(TitleEvent::Tabs(TabsEvent::ContextMenu { key, at, menu })) => {
                Some(tab_menu(*key, *at, *menu, i18n, &keymap))
            }
            ShellAction::Sidebar(SidebarEvent::ContextMenu { key, at }) => {
                let folder = {
                    let view = cell.view.borrow();
                    key.and_then(|key| view.sidebar().and_then(|sidebar| sidebar.entry(key)))
                        .is_some_and(SidebarEntry::is_folder)
                };
                Some(sidebar_menu(*key, folder, *at, i18n, &keymap))
            }
            ShellAction::Sidebar(SidebarEvent::HeaderMenu { menu, anchor }) => {
                Some(header_menu(*menu, *anchor, i18n, &keymap))
            }
            ShellAction::Editor(
                _,
                EditorEvent::Media(MediaEvent::MermaidMenu { source, anchor, .. }),
            ) => Some(mermaid_menu(source, *anchor, i18n, &keymap)),
            ShellAction::EditorContext { key, at } => {
                let has_selection = cell
                    .view
                    .borrow()
                    .tab(*key)
                    .is_some_and(|tab| !tab.editor.selection().is_caret());
                Some(editor_menu(has_selection, *at, i18n, &keymap))
            }
            ShellAction::Editor(_, EditorEvent::TableMenu { anchor, raw, .. }) => {
                Some(table_menu(*raw, *anchor, i18n, &keymap))
            }
            ShellAction::Editor(
                _,
                EditorEvent::SpellMenu {
                    range,
                    suggestions,
                    anchor,
                    ..
                },
            ) => Some(spell_menu(*range, suggestions, *anchor, i18n, &keymap)),
            ShellAction::Editor(
                _,
                EditorEvent::CalloutMenu {
                    range,
                    kind,
                    anchor,
                },
            ) => Some(callout_menu(*range, *kind, *anchor, i18n, &keymap)),
            ShellAction::Status(StatusEvent::EncodingClicked { anchor }) => {
                Self::format_menu(&cell).map(|menu| encoding_menu(menu, *anchor, i18n, &keymap))
            }
            ShellAction::Status(StatusEvent::LineEndingClicked { anchor }) => {
                Self::format_menu(&cell).map(|menu| line_ending_menu(menu, *anchor, i18n, &keymap))
            }
            _ => None,
        }
    }

    /// What the status bar menus need to know about the active document of `cell`.
    fn format_menu(cell: &WindowCell) -> Option<FormatMenu> {
        cell.view.borrow().active_tab().map(|tab| FormatMenu {
            format: tab.entry.format(),
            text_file: tab.entry.source() == Source::Text,
            on_disk: tab.entry.path().is_some(),
        })
    }

    /// Carries out the item `id` the user chose in `popup` (window `window`).
    pub fn run_popup_choice(&self, window: WindowId, popup: &Popup, id: u32) {
        let (Some(cell), Some(action)) = (self.window(window), popup.action(id).cloned()) else {
            return;
        };
        match action {
            Action::Command(command) => {
                self.run_command(window, command);
            }
            Action::TabCommand { key, command } => {
                cell.view.borrow_mut().activate_tab(key);
                self.run_command(window, command);
            }
            Action::Sidebar { command, target } => self.sidebar_command(&cell, command, target),
            Action::Spell { range, replacement } => {
                if let Some(editor) = cell.view.borrow_mut().active_editor_mut() {
                    // `false`: the text at the word changed meanwhile; nothing to replace.
                    let _ = SpellCheck::replace(editor, range, &replacement);
                }
            }
            Action::MermaidExport { source, format } => {
                self.export_diagram(&cell, source, format);
            }
            Action::Callout { range, kind } => {
                if let Some(editor) = cell.view.borrow_mut().active_editor_mut() {
                    editor.set_selection(Selection::caret(range.start));
                    let transaction = {
                        let document = editor.document().borrow();
                        blocks::set_callout(document.ctx(), kind)
                    };
                    if let Some(transaction) = transaction {
                        // A refused edit leaves the callout as it was.
                        let _ = editor.document().borrow_mut().apply(transaction);
                    }
                }
            }
            Action::ReopenWithEncoding(encoding) => self.reopen_with_encoding(&cell, encoding),
            Action::SaveWithEncoding { encoding, bom } => {
                if let Some(tab) = cell.view.borrow().active_tab() {
                    tab.entry.set_save_encoding(encoding, bom);
                }
            }
            Action::LineEnding(line_ending) => {
                if let Some(tab) = cell.view.borrow().active_tab() {
                    tab.entry.set_save_line_ending(line_ending);
                }
            }
        }
        self.refresh(&cell);
    }

    /// Reads the active document's file again as `encoding` (spec §4.2). Unsaved changes are
    /// replaced by the file's text, so they are confirmed first.
    fn reopen_with_encoding(&self, cell: &Rc<WindowCell>, encoding: TextEncoding) {
        let entry = cell
            .view
            .borrow()
            .active_tab()
            .map(|tab| Rc::clone(&tab.entry));
        // The menu is only offered for an active document.
        if let Some(entry) = entry
            && (!entry.is_dirty() || self.confirm_discarding(&entry))
            && let Err(error) = self.docs().reload_as(&entry, encoding)
        {
            self.report(&error);
        }
    }

    /// Asks whether the unsaved changes of `entry` may be replaced by the text of its file.
    fn confirm_discarding(&self, entry: &DocEntry) -> bool {
        let name = entry.title(self.i18n());
        let answer = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: self.i18n().text("window-reopen-discard-title"),
            message: self.i18n().format(
                "window-reopen-discard-message",
                &[("name", Arg::Str(&name))],
            ),
            buttons: MessageBoxButtons::YesNo,
            icon: Some(MessageBoxIcon::Warning),
        });
        matches!(answer, Ok(MessageBoxResult::Yes))
    }

    /// The sidebar events that are file operations rather than menus: a committed rename and
    /// the Delete key. Returns the event when it is not one of them.
    pub fn sidebar_file_event(
        &self,
        cell: &Rc<WindowCell>,
        event: SidebarEvent,
    ) -> Option<SidebarEvent> {
        match event {
            SidebarEvent::RenameCommitted { key, name } => {
                self.rename_entry(cell, key, &name);
                None
            }
            SidebarEvent::DeleteRequested(key) => {
                self.sidebar_command(cell, SidebarCommand::Delete, Some(key));
                None
            }
            SidebarEvent::Dropped {
                source,
                target_folder,
                copy,
            } => {
                self.drop_entry(cell, source, target_folder, copy);
                None
            }
            SidebarEvent::RenameCancelled(_)
            | SidebarEvent::ContextMenu { .. }
            | SidebarEvent::HeaderMenu { .. } => None,
            other => Some(other),
        }
    }

    /// The path of the sidebar entry `key` of `cell`.
    pub(super) fn tree_path(cell: &WindowCell, key: NodeKey) -> Option<PathBuf> {
        cell.tree
            .borrow()
            .as_ref()
            .and_then(|paths| paths.path(key))
    }

    /// A sidebar command from the palette or a shortcut: runs on the selected entry of the
    /// sidebar (root commands on the workspace). False without a workspace, for an entry
    /// command without a selection or while the sidebar is hidden, and for the commands that
    /// open a file when a folder is selected.
    pub(super) fn run_sidebar_command(&self, cell: &Rc<WindowCell>, item: SidebarCommand) -> bool {
        use SidebarCommand as S;
        if cell.tree.borrow().is_none() {
            return false;
        }
        let target = if item.on_root() {
            None
        } else {
            let Some((key, folder)) = selected_sidebar_entry(&cell.view.borrow()) else {
                return false;
            };
            if folder && matches!(item, S::Open | S::OpenInNewTab | S::OpenInNewWindow) {
                return false;
            }
            Some(key)
        };
        self.sidebar_command(cell, item, target);
        true
    }

    /// Runs the sidebar command `command` on the entry `target` (`None`: the root).
    fn sidebar_command(
        &self,
        cell: &Rc<WindowCell>,
        command: SidebarCommand,
        target: Option<NodeKey>,
    ) {
        use SidebarCommand as S;
        let key = target.unwrap_or(0);
        let Some(path) = Self::tree_path(cell, key) else {
            return;
        };
        let folder = cell
            .view
            .borrow()
            .sidebar()
            .and_then(|sidebar| sidebar.entry(key))
            .map_or(target.is_none(), SidebarEntry::is_folder);
        let fs = self.docs().env().fs.clone();
        match command {
            S::Open | S::OpenInNewTab => {
                self.open_path(cell, &path, true);
            }
            S::OpenInNewWindow => {
                let (size, scale) = {
                    let view = cell.view.borrow();
                    (view.layout().size(), view.layout().scale())
                };
                let fresh = self.create_window(None, size, scale);
                // Like every other way into a file of the sidebar: a binary file goes to its
                // default app, and then there is nothing to show in the new window. A window
                // that reports a failure stays, so the user sees it there.
                if self.open_path(&fresh, &path, true) == Browsed::Launched {
                    self.close_window(fresh.id);
                } else {
                    self.set_active_window(fresh.id);
                    self.refresh(&fresh);
                }
            }
            S::Rename => cell.view.borrow_mut().request_rename(key),
            S::Duplicate => match fileops::duplicate(fs.as_ref(), &path) {
                Ok(_) => self.refresh_sidebars(),
                Err(error) => self.report_file_op(&error),
            },
            S::Delete => self.delete_entry(cell, &path),
            S::RevealInExplorer => {
                let _ = self.shell.borrow_mut().reveal_in_explorer(&path);
            }
            S::CopyPath => self.copy_text(path.to_string_lossy().into_owned()),
            S::CopyRelativePath => {
                let root = Self::tree_path(cell, 0).unwrap_or_default();
                let relative = path.strip_prefix(&root).unwrap_or(&path);
                self.copy_text(relative.to_string_lossy().into_owned());
            }
            S::NewFile | S::NewFolder => {
                let dir = if folder {
                    path
                } else {
                    path.parent().map(Path::to_path_buf).unwrap_or_default()
                };
                let created = if command == S::NewFile {
                    fileops::new_untitled_file(fs.as_ref(), &dir, "md")
                } else {
                    let base = self.i18n().text("panels-new-folder-name");
                    fileops::new_untitled_folder(fs.as_ref(), &dir, &base)
                };
                match created {
                    Ok(created) => self.reveal_new_entry(cell, &created),
                    Err(error) => self.report_file_op(&error),
                }
            }
            S::CollapseAll | S::ExpandAll => {
                if let Some(sidebar) = cell.view.borrow_mut().sidebar_mut() {
                    if command == S::CollapseAll {
                        sidebar.collapse_all();
                    } else {
                        sidebar.expand_all();
                    }
                }
            }
        }
    }

    /// Selects a freshly created entry and opens its inline rename box (a new file in a
    /// collapsed folder is not visible: it opens in a tab instead).
    fn reveal_new_entry(&self, cell: &Rc<WindowCell>, created: &Path) {
        self.refresh_sidebars();
        let key = cell.tree.borrow().as_ref().map(|paths| paths.key(created));
        // The entry is selected where it is visible; a sidebar that does not show it (or no
        // sidebar at all) leaves `revealed` empty.
        let revealed = key.filter(|&key| {
            cell.view.borrow_mut().sidebar_mut().is_some_and(|sidebar| {
                let shown = sidebar.entry(key).is_some();
                shown.then(|| sidebar.select_key(key)).is_some()
            })
        });
        match revealed {
            Some(key) => cell.view.borrow_mut().request_rename(key),
            None if created.is_file() => {
                self.open_path(cell, created, true);
            }
            None => {}
        }
    }

    /// Renames the sidebar entry `key` to `name`; open documents follow their file.
    fn rename_entry(&self, cell: &Rc<WindowCell>, key: NodeKey, name: &str) {
        let Some(old) = Self::tree_path(cell, key) else {
            return;
        };
        let old_canonical = canonical_path(&old);
        let fs = self.docs().env().fs.clone();
        // What links to the entry has to be looked up while the index still knows the old name.
        let plan = self.plan_link_updates(cell, &old);
        match fileops::rename(fs.as_ref(), &old, name) {
            Ok(new) => {
                self.rebind_open_documents(&old_canonical, &canonical_path(&new));
                self.refresh_sidebars();
                self.offer_link_updates(plan, &new);
            }
            Err(error) => self.report_file_op(&error),
        }
    }

    /// Points the documents inside `old` (the file itself, or a renamed folder) at `new`.
    pub(super) fn rebind_open_documents(&self, old: &Path, new: &Path) {
        for entry in self.docs().all() {
            let Some(path) = entry.path() else {
                continue;
            };
            let Ok(rest) = path.strip_prefix(old) else {
                continue;
            };
            let target = if rest.as_os_str().is_empty() {
                new.to_path_buf()
            } else {
                new.join(rest)
            };
            if let Err(error) = self.docs().rebind_path(&entry, &target) {
                self.report(&error);
            }
        }
    }

    /// Asks, then moves `path` to the Recycle Bin.
    fn delete_entry(&self, cell: &Rc<WindowCell>, path: &Path) {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let answer = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: self.i18n().text("panels-delete-title"),
            message: self
                .i18n()
                .format("panels-delete-message", &[("name", Arg::Str(&name))]),
            buttons: MessageBoxButtons::YesNo,
            icon: Some(MessageBoxIcon::Warning),
        });
        if !matches!(answer, Ok(MessageBoxResult::Yes)) {
            return;
        }
        let fs = self.docs().env().fs.clone();
        match fileops::recycle(fs.as_ref(), &[path.to_path_buf()]) {
            Ok(()) => self.refresh_sidebars(),
            Err(error) => self.report(&DocError::Fs(error)),
        }
        self.refresh(cell);
    }

    /// Lists the folders of every window's sidebar again.
    pub(super) fn refresh_sidebars(&self) {
        for window in self.windows() {
            // The tree caches its folders: the listing has to be read from disk again.
            if let Some(paths) = window.tree.borrow().as_ref() {
                paths.request_rescan();
            }
            if let Some(sidebar) = window.view.borrow_mut().sidebar_mut() {
                sidebar.refresh_all();
            }
        }
    }

    fn copy_text(&self, text: String) {
        let _ = self
            .clipboard
            .borrow_mut()
            .set(ClipboardContent::from_text(text));
    }

    /// Tells the user why a file operation failed.
    pub(super) fn report_file_op(&self, error: &FileOpError) {
        let i18n = self.i18n();
        let message = match error {
            FileOpError::Name(name) => match name {
                NameError::Empty => i18n.text("panels-name-empty"),
                NameError::TooLong => i18n.text("panels-name-too-long"),
                NameError::InvalidChar(found) => i18n.format(
                    "panels-name-invalid-char",
                    &[("char", Arg::Str(&found.to_string()))],
                ),
                NameError::Reserved(reserved) => {
                    i18n.format("panels-name-reserved", &[("name", Arg::Str(reserved))])
                }
                NameError::TrailingDotOrSpace => i18n.text("panels-name-trailing"),
            },
            FileOpError::IntoSelf(_) => i18n.text("panels-error-into-self"),
            FileOpError::Fs(fs) => {
                return self.report(&DocError::Fs(fs.clone()));
            }
        };
        let _ = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: i18n.text("window-error-title"),
            message,
            buttons: MessageBoxButtons::Ok,
            icon: Some(MessageBoxIcon::Error),
        });
    }
}
