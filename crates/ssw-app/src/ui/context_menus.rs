//! Context menus (spec §2.6, §3.1, §5, §6): the tab menu, the sidebar item / root / header menus
//! and the editor menus (plain edit menu, spelling suggestions, table cell menu, callout type).
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
use ssw_io::fileops::{self, FileOpError, NameError};
use ssw_ui::editor::EditorEvent;
use ssw_ui::editor::blocks_media::MediaEvent;
use ssw_ui::editor::spell::SpellCheck;
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::services::{
    ClipboardContent, MessageBoxButtons, MessageBoxIcon, MessageBoxOptions, MessageBoxResult,
};
use ssw_ui::shell::sidebar::{HeaderMenu, NodeKey, SidebarEntry, SidebarEvent};
use ssw_ui::shell::tabs::{TabMenu, TabsEvent};
use ssw_ui::shell::titlebar::TitleEvent;
use ssw_win::menus::{MenuEntry, MenuItem, MenuModel};

use CommandId as C;

use super::controller::{Controller, WindowCell};
use super::export::DiagramFormat;
use super::menus::menu_id;
use super::shell_view::ShellAction;
use crate::documents::{DocError, canonical_path};
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
        command: CommandId,
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

    fn finish(mut self, at: Point) -> Popup {
        while matches!(self.entries.last(), Some(MenuEntry::Separator)) {
            self.entries.pop();
        }
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
    let mut builder = Builder::new(i18n, keymap);
    let item = |command| Action::Sidebar { command, target };
    if target.is_some() {
        if !folder {
            builder.command(C::SidebarOpen, true, item(C::SidebarOpen));
            builder.command(C::SidebarOpenInNewTab, true, item(C::SidebarOpenInNewTab));
            builder.command(
                C::SidebarOpenInNewWindow,
                true,
                item(C::SidebarOpenInNewWindow),
            );
            builder.separator();
        }
        builder.command(C::SidebarRename, true, item(C::SidebarRename));
        builder.command(C::SidebarDuplicate, true, item(C::SidebarDuplicate));
        builder.command(C::SidebarDelete, true, item(C::SidebarDelete));
        builder.separator();
    }
    builder.command(
        C::SidebarRevealInExplorer,
        true,
        item(C::SidebarRevealInExplorer),
    );
    builder.command(C::SidebarCopyPath, true, item(C::SidebarCopyPath));
    if target.is_some() {
        builder.command(
            C::SidebarCopyRelativePath,
            true,
            item(C::SidebarCopyRelativePath),
        );
    }
    builder.separator();
    builder.command(C::SidebarNewFile, true, item(C::SidebarNewFile));
    builder.command(C::SidebarNewFolder, true, item(C::SidebarNewFolder));
    builder.finish(at)
}

/// The sidebar header menus: `+` (New file, New folder) and `⋯` (Collapse all, Expand all).
#[must_use]
pub fn header_menu(menu: HeaderMenu, anchor: Point, i18n: &Localizer, keymap: &Keymap) -> Popup {
    let mut builder = Builder::new(i18n, keymap);
    let item = |command| Action::Sidebar {
        command,
        target: None,
    };
    let commands: &[CommandId] = match menu {
        HeaderMenu::New => &[C::SidebarNewFile, C::SidebarNewFolder],
        HeaderMenu::Options => &[C::SidebarCollapseAll, C::SidebarExpandAll],
    };
    for &command in commands {
        builder.command(command, true, item(command));
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

impl Controller {
    /// The popup menu `action` asks for (tab, sidebar, editor, spelling, table, callout), if it
    /// is a menu request.
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
            _ => None,
        }
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
        }
        self.refresh(&cell);
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
                self.sidebar_command(cell, C::SidebarDelete, Some(key));
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

    /// Runs the sidebar command `command` on the entry `target` (`None`: the root).
    fn sidebar_command(&self, cell: &Rc<WindowCell>, command: CommandId, target: Option<NodeKey>) {
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
            C::SidebarOpen | C::SidebarOpenInNewTab => self.open_path(cell, &path, true),
            C::SidebarOpenInNewWindow => {
                let (size, scale) = {
                    let view = cell.view.borrow();
                    (view.layout().size(), view.layout().scale())
                };
                let fresh = self.create_window(None, size, scale);
                if let Err(error) = self.open_file(&fresh, &path) {
                    self.report(&error);
                }
                self.set_active_window(fresh.id);
                self.refresh(&fresh);
            }
            C::SidebarRename => cell.view.borrow_mut().request_rename(key),
            C::SidebarDuplicate => match fileops::duplicate(fs.as_ref(), &path) {
                Ok(_) => self.refresh_sidebars(),
                Err(error) => self.report_file_op(&error),
            },
            C::SidebarDelete => self.delete_entry(cell, &path),
            C::SidebarRevealInExplorer => {
                let _ = self.shell.borrow_mut().reveal_in_explorer(&path);
            }
            C::SidebarCopyPath => self.copy_text(path.to_string_lossy().into_owned()),
            C::SidebarCopyRelativePath => {
                let root = Self::tree_path(cell, 0).unwrap_or_default();
                let relative = path.strip_prefix(&root).unwrap_or(&path);
                self.copy_text(relative.to_string_lossy().into_owned());
            }
            C::SidebarNewFile | C::SidebarNewFolder => {
                let dir = if folder {
                    path
                } else {
                    path.parent().map(Path::to_path_buf).unwrap_or_default()
                };
                let created = if command == C::SidebarNewFile {
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
            C::SidebarCollapseAll | C::SidebarExpandAll => {
                if let Some(sidebar) = cell.view.borrow_mut().sidebar_mut() {
                    if command == C::SidebarCollapseAll {
                        sidebar.collapse_all();
                    } else {
                        sidebar.expand_all();
                    }
                }
            }
            _ => {}
        }
    }

    /// Selects a freshly created entry and opens its inline rename box (a new file in a
    /// collapsed folder is not visible: it opens in a tab instead).
    fn reveal_new_entry(&self, cell: &Rc<WindowCell>, created: &Path) {
        self.refresh_sidebars();
        let key = cell.tree.borrow().as_ref().map(|paths| paths.key(created));
        let visible = key.is_some_and(|key| {
            cell.view
                .borrow()
                .sidebar()
                .is_some_and(|sidebar| sidebar.entry(key).is_some())
        });
        match key {
            Some(key) if visible => {
                let mut view = cell.view.borrow_mut();
                if let Some(sidebar) = view.sidebar_mut() {
                    sidebar.select_key(key);
                }
                view.request_rename(key);
            }
            _ if created.is_file() => self.open_path(cell, created, true),
            _ => {}
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
