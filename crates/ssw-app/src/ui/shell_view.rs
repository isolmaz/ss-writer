//! The window content: the pane layout, title row with tabs, command row, sidebar, one editor
//! per tab, banners and the status bar, composed from the `ssw-ui` widgets (spec §1, §6, §8).
//!
//! [`ShellView`] knows nothing about Win32: it paints on a [`Canvas`], takes [`Event`]s in
//! window DIPs and reports what the user asked for as [`ShellAction`]s. The controller carries
//! the actions out; `ui::window` feeds the view from the window messages. Tests drive it with a
//! recording canvas and the fixed-advance text engine.

use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;

use ssw_core::command::CommandId;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::ops::format::InlineStyle;
use ssw_core::settings::{Settings, SortBy};
use ssw_core::stats;
use ssw_core::text::TextRange;
use ssw_ui::canvas::Canvas;
use ssw_ui::editor::blocks_table::TableControl;
use ssw_ui::editor::{DocumentKind, EditorConfig, EditorEvent, EditorStrings, EditorView};
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{Cursor, Event, FocusEvent, PointerButton, PointerEvent, PointerEventKind};
use ssw_ui::services::Clock;
use ssw_ui::shell::banners::{BannerHost, BannerStrings, HostEvent};
use ssw_ui::shell::inspector::{Inspector, InspectorEvent, InspectorStrings};
use ssw_ui::shell::layout::{Backdrop, ChromeHit, Pane, ShellLayout, ShellMode};
use ssw_ui::shell::sidebar::{
    SidebarEvent, SidebarOptions, SidebarStrings, SidebarView, SortKey, TreeSource,
};
use ssw_ui::shell::statusbar::{Eol, StatusBar, StatusEvent, StatusInfo, StatusStrings};
use ssw_ui::shell::titlebar::{FormatState, TitleBar, TitleEvent, TitleStrings};
use ssw_ui::text::TextLayoutEngine;
use ssw_ui::theme::Theme;
use ssw_ui::widget::{
    EventCx, EventResult, FocusManager, LayoutCx, PaintCx, PointerCapture, Requests, Widget,
    WidgetId,
};
use ssw_ui::widgets::{Control, SharedClipboard};

use super::clipboard::key_command;
use super::history::HistoryHost;
use super::overlays::{NoShortcuts, OverlayAction, OverlayEnv, OverlayRects, Overlays};
use crate::documents::{DocEntry, DocMode, Documents};

/// Something the user did that the window cannot carry out by itself.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellAction {
    /// The title or command row: tabs, menu buttons, toggles, the format pill.
    Title(TitleEvent),
    /// The sidebar tree.
    Sidebar(SidebarEvent),
    /// A banner button.
    Banner(HostEvent),
    /// A status bar segment.
    Status(StatusEvent),
    /// The editor of the tab with this key.
    Editor(u64, EditorEvent),
    /// An overlay (Quick Open, folder search, palette, heading menu, toolbar) asks the
    /// controller for something.
    Overlay(OverlayAction),
    /// The inspector panel: headings, tabs, backlinks.
    Inspector(InspectorEvent),
    /// The version history panel: a version was selected, restored or the panel closed.
    History(ssw_ui::shell::history::HistoryEvent),
    /// A right click in the editor that no block turned into its own menu (spell, table,
    /// callout): the plain Cut / Copy / Paste / Select all menu belongs at `at` (window DIPs).
    EditorContext {
        /// The tab whose editor was clicked.
        key: u64,
        /// The pointer position.
        at: Point,
    },
    /// Ctrl+C, Ctrl+X, Ctrl+V or Ctrl+Shift+V pressed while the editor has the focus: the
    /// controller runs the command (rich copy, smart paste; see [`super::clipboard`]) instead
    /// of the editor's plain-text one.
    Clipboard(CommandId),
}

/// Configures every editor the view creates (block renderers, spell checking, object
/// provider): the platform layer sets it once and each new tab's editor passes through it.
/// It returns the [`TableControl`] of the table renderer it registered with the editor, if it
/// registered one: the table commands that need the host (copy as Markdown / CSV / HTML, edit
/// as raw Markdown) run through it.
pub type EditorSetup = Rc<dyn Fn(&mut EditorView) -> Option<TableControl>>;

/// One tab: its document (shared with other tabs), its editor and its workspace root.
#[derive(Debug)]
pub struct TabView {
    /// Unique within the window.
    pub key: u64,
    /// The shared document.
    pub entry: Rc<DocEntry>,
    /// The editor over the document.
    pub editor: EditorView,
    /// The host side of the editor's table renderer (see [`EditorSetup`]).
    pub table: Option<TableControl>,
    /// The workspace root of the tab.
    pub workspace: Option<PathBuf>,
}

/// What the view is built from.
pub struct ViewConfig {
    /// Messages of the active locale.
    pub localizer: Rc<Localizer>,
    /// The colors.
    pub theme: Theme,
    /// Window size in DIPs.
    pub size: Size,
    /// `dpi / 96`.
    pub scale: f32,
    /// Times typing groups for undo.
    pub clock: Rc<dyn Clock>,
    /// The clipboard of copy, cut and paste.
    pub clipboard: SharedClipboard,
    /// The user's settings.
    pub settings: Settings,
}

impl fmt::Debug for ViewConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewConfig")
            .field("size", &self.size)
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}

/// The widget that receives an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Title,
    Sidebar,
    Banners,
    Editor,
    Inspector,
    Status,
}

/// What the status bar was last computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatusKey {
    tab: u64,
    version: u64,
    anchor: usize,
    head: usize,
    zoom: u32,
    format: (bool, bool, bool),
    /// Encoding, BOM and line endings: the status bar shows them and the user changes them
    /// without touching the text.
    file_format: ssw_io::encoding::TextFormat,
}

/// The composed window content.
pub struct ShellView {
    localizer: Rc<Localizer>,
    theme: Theme,
    layout: ShellLayout,
    root: WidgetId,
    titlebar: TitleBar,
    statusbar: StatusBar,
    banners: BannerHost,
    sidebar: Option<SidebarView>,
    inspector: Inspector,
    /// The sidebar entry whose inline rename box opens at the next layout (it needs the text
    /// engine of the window).
    pending_rename: Option<u64>,
    /// Find, Go to line, Quick Open, folder search and the editor popups.
    overlays: Overlays,
    /// The version history panel over the editor area.
    history: HistoryHost,
    /// A right click the editor ignored; becomes [`ShellAction::EditorContext`].
    editor_context: Option<(u64, Point)>,
    tabs: Vec<TabView>,
    active: Option<usize>,
    next_key: u64,
    clock: Rc<dyn Clock>,
    clipboard: SharedClipboard,
    editor_config: EditorConfig,
    editor_strings: EditorStrings,
    editor_setup: Option<EditorSetup>,
    sidebar_options: SidebarOptions,
    focus: FocusManager,
    capture: PointerCapture,
    requests: Requests,
    focus_target: Target,
    hover: Option<Target>,
    capture_target: Option<Target>,
    refocus_editor: bool,
    status_key: Option<StatusKey>,
    words: Option<(u64, u64, usize)>,
}

impl fmt::Debug for ShellView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellView")
            .field("layout", &self.layout)
            .field("tabs", &self.tabs.len())
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

/// Makes `editor` present its document the way the file type wants (spec §4.1): live Markdown,
/// plain lines, or a whole Mermaid diagram.
fn present_as(editor: &mut EditorView, mode: DocMode) {
    let kind = match mode {
        DocMode::Markdown => DocumentKind::Markdown,
        DocMode::Plain => DocumentKind::PlainText,
        DocMode::Mermaid => DocumentKind::Mermaid,
    };
    if editor.config().document_kind != kind {
        let mut config = editor.config().clone();
        config.document_kind = kind;
        editor.set_config(config);
    }
}

/// The sidebar options the settings describe.
#[must_use]
pub fn sidebar_options(settings: &Settings) -> SidebarOptions {
    let sidebar = &settings.sidebar;
    SidebarOptions {
        sort: match sidebar.sort_by {
            SortBy::Name => SortKey::Name,
            SortBy::DateModified => SortKey::Modified,
            SortBy::DateCreated => SortKey::Created,
            SortBy::Kind => SortKey::Kind,
        },
        descending: sidebar.sort_descending,
        folders_first: sidebar.folders_first,
        show_extensions: sidebar.show_extensions,
        show_hidden: sidebar.show_hidden_items,
        hide_images: sidebar.hide_images,
        hide_non_markdown: sidebar.hide_non_markdown,
        show_modified: sidebar.show_modified_date,
    }
}

impl ShellView {
    /// A view with no tabs and no workspace.
    #[must_use]
    pub fn new(config: ViewConfig) -> Self {
        let ViewConfig {
            localizer,
            theme,
            size,
            scale,
            clock,
            clipboard,
            settings,
        } = config;
        let mut layout = ShellLayout::new(size, scale);
        layout.set_sidebar_visible(false);
        layout.set_status_bar_visible(settings.appearance.show_status_bar);
        let mut titlebar = TitleBar::new(TitleStrings::from_localizer(&localizer));
        titlebar.set_pill_visible(settings.appearance.show_format_pill);
        titlebar.set_panes_open(false, false);
        Self {
            theme,
            layout,
            root: WidgetId::allocate(),
            titlebar,
            statusbar: StatusBar::new(StatusStrings::from_localizer(&localizer)),
            banners: BannerHost::new(BannerStrings::from_localizer(&localizer)),
            sidebar: None,
            inspector: Inspector::new(InspectorStrings::from_localizer(&localizer)),
            pending_rename: None,
            overlays: Overlays::new(&localizer, &clipboard, &NoShortcuts),
            history: HistoryHost::new(Rc::clone(&localizer)),
            editor_context: None,
            tabs: Vec::new(),
            active: None,
            next_key: 1,
            clock,
            clipboard,
            editor_config: EditorConfig::from_settings(&settings),
            editor_strings: EditorStrings::new(&localizer),
            editor_setup: None,
            sidebar_options: sidebar_options(&settings),
            focus: FocusManager::new(),
            capture: PointerCapture::new(),
            requests: Requests::new(),
            focus_target: Target::Editor,
            hover: None,
            capture_target: None,
            refocus_editor: false,
            status_key: None,
            words: None,
            localizer,
        }
    }

    // ----- state ---------------------------------------------------------------------------

    /// The pane geometry.
    #[must_use]
    pub fn layout(&self) -> &ShellLayout {
        &self.layout
    }

    /// The pane geometry, for the pane toggles and the display mode.
    pub fn layout_mut(&mut self) -> &mut ShellLayout {
        &mut self.layout
    }

    /// The colors.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Switches the colors.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.requests.invalidate_all();
    }

    /// Resizes the view (window DIPs) and changes the DPI scale.
    pub fn resize(&mut self, size: Size, scale: f32) {
        self.layout.set_size(size);
        self.layout.set_scale(scale);
        self.requests.invalidate_all();
    }

    /// The host requests (repaint, timers, cursor) the widgets made since the last call.
    pub fn requests_mut(&mut self) -> &mut Requests {
        &mut self.requests
    }

    /// The title bar (tabs, pill state).
    #[must_use]
    pub fn titlebar(&self) -> &TitleBar {
        &self.titlebar
    }

    /// The banner stack of the active document.
    pub fn banners_mut(&mut self) -> &mut BannerHost {
        &mut self.banners
    }

    /// The sidebar, when a workspace is open.
    #[must_use]
    pub fn sidebar(&self) -> Option<&SidebarView> {
        self.sidebar.as_ref()
    }

    /// The sidebar, when a workspace is open.
    pub fn sidebar_mut(&mut self) -> Option<&mut SidebarView> {
        self.sidebar.as_mut()
    }

    /// The inspector panel.
    #[must_use]
    pub fn inspector(&self) -> &Inspector {
        &self.inspector
    }

    /// The inspector panel, for [`super::inspector`] to feed.
    pub fn inspector_mut(&mut self) -> &mut Inspector {
        &mut self.inspector
    }

    /// Asks for the inline rename box of the sidebar entry `key` (F2, "New file here"): it
    /// opens at the next layout, which has the window's text engine.
    pub fn request_rename(&mut self, key: u64) {
        self.pending_rename = Some(key);
        self.requests.invalidate_all();
    }

    /// Sets the configuration every editor passes through; applies it to the open tabs now.
    pub fn set_editor_setup(&mut self, setup: EditorSetup) {
        for tab in &mut self.tabs {
            tab.table = setup(&mut tab.editor);
        }
        self.editor_setup = Some(setup);
        self.requests.invalidate_all();
    }

    /// The id of the window's root access node.
    #[must_use]
    pub fn root_id(&self) -> WidgetId {
        self.root
    }

    /// The accessibility tree of the window: a window node over the title bar, the sidebar, the
    /// banners, the active editor, the inspector (while shown) and the status bar.
    #[must_use]
    pub fn access_tree(
        &self,
        text: &dyn TextLayoutEngine,
        title: &str,
    ) -> ssw_ui::access::AccessTree {
        use ssw_ui::access::{AccessNode, AccessTree, Role};
        use ssw_ui::widget::AccessCx;
        let mut tree = AccessTree::new(self.root);
        let theme = self.theme;
        let cx = AccessCx::new(self.root, &theme, self.layout.scale(), text, &self.focus);
        let mut children = vec![self.titlebar.id()];
        self.titlebar
            .access(&cx.for_widget(self.titlebar.id()), &mut tree);
        if let Some(sidebar) = &self.sidebar {
            sidebar.access(&cx.for_widget(sidebar.id()), &mut tree);
            children.push(sidebar.id());
        }
        if self.banners.content_height() > 0.0 {
            self.banners
                .access(&cx.for_widget(self.banners.id()), &mut tree);
            children.push(self.banners.id());
        }
        if let Some(tab) = self.active_tab() {
            tab.editor
                .access(&cx.for_widget(tab.editor.id()), &mut tree);
            children.push(tab.editor.id());
        }
        if self.layout.rects().inspector.is_some() {
            self.inspector
                .access(&cx.for_widget(self.inspector.id()), &mut tree);
            children.push(self.inspector.id());
        }
        children.extend(self.history.access(&cx, &mut tree));
        children.extend(self.overlays.export_access(&cx, &mut tree));
        self.statusbar
            .access(&cx.for_widget(self.statusbar.id()), &mut tree);
        children.push(self.statusbar.id());
        let size = self.layout.size();
        tree.insert(
            AccessNode::new(
                self.root,
                Role::Window,
                title,
                Rect::new(0.0, 0.0, size.width, size.height),
            )
            .with_children(children),
        );
        tree
    }

    /// Carries out an access action that assistive technology requested on node `id` (the
    /// title bar items and the editor); returns what the widgets asked of the shell.
    pub fn perform_access(
        &mut self,
        id: WidgetId,
        action: &ssw_ui::access::AccessAction,
        text: &dyn TextLayoutEngine,
    ) -> Vec<ShellAction> {
        let theme = self.theme;
        let scale = self.layout.scale();
        {
            let Self {
                titlebar,
                tabs,
                active,
                focus,
                capture,
                requests,
                ..
            } = self;
            let mut cx = EventCx::new(id, &theme, scale, text, focus, capture, requests);
            if !titlebar.perform_item(&mut cx, id, action)
                && let Some(tab) = active.and_then(|index| tabs.get_mut(index))
                && tab.editor.id() == id
            {
                tab.editor.perform(&mut cx.for_widget(id), action);
            }
        }
        self.collect()
    }

    /// The sidebar view options in force.
    #[must_use]
    pub fn sidebar_options(&self) -> SidebarOptions {
        self.sidebar_options
    }

    /// Replaces the sidebar view options (settings changed).
    pub fn set_sidebar_options(&mut self, options: SidebarOptions) {
        self.sidebar_options = options;
        if let Some(sidebar) = self.sidebar.as_mut() {
            sidebar.set_options(options);
        }
    }

    /// Shows the workspace of `source` in the sidebar; `None` removes the sidebar content.
    pub fn set_workspace(&mut self, source: Option<Box<dyn TreeSource>>) {
        self.sidebar = source.map(|source| {
            SidebarView::new(
                SidebarStrings::from_localizer(&self.localizer),
                source,
                Rc::clone(&self.clipboard),
                self.sidebar_options,
            )
        });
        self.requests.invalidate_all();
    }

    /// Whether the status bar and the sidebar toggle states shown on the title bar match the
    /// layout.
    fn sync_toggles(&mut self) {
        self.titlebar.set_panes_open(
            self.layout.sidebar_visible(),
            self.layout.inspector_visible(),
        );
    }

    /// Shows or hides the sidebar (Ctrl+Shift+E).
    pub fn toggle_sidebar(&mut self) {
        self.layout.toggle_sidebar();
        self.sync_toggles();
        self.requests.invalidate_all();
    }

    /// Shows or hides the inspector (Ctrl+Shift+I).
    pub fn toggle_inspector(&mut self) {
        self.layout.toggle_inspector();
        self.sync_toggles();
        self.requests.invalidate_all();
    }

    /// Shows or hides the status bar.
    pub fn toggle_status_bar(&mut self) {
        let visible = self.layout.status_bar_visible();
        self.layout.set_status_bar_visible(!visible);
        self.requests.invalidate_all();
    }

    /// Shows or hides the B I U pill.
    pub fn toggle_format_pill(&mut self) {
        let visible = self.titlebar.pill_visible();
        self.titlebar.set_pill_visible(!visible);
        self.requests.invalidate_all();
    }

    /// View → Word wrap (spec §4.1): wraps long lines at the window edge or lets them run on
    /// and scroll sideways, for the active tab. Only a tab that shows plain text has the
    /// choice: `false` (and nothing changes) for a Markdown or Mermaid tab, a plain file shown
    /// as Markdown and a window without tabs. The choice belongs to the tab, like the other
    /// ways of presenting it, and starts wrapped.
    pub fn toggle_word_wrap(&mut self) -> bool {
        let Some(editor) = self
            .active_tab_mut()
            .map(|tab| &mut tab.editor)
            .filter(|editor| editor.config().document_kind == DocumentKind::PlainText)
        else {
            return false;
        };
        let mut config = editor.config().clone();
        config.wrap = !config.wrap;
        editor.set_config(config);
        self.requests.invalidate_all();
        true
    }

    /// View → Render as Markdown (spec §4.1): shows the active plain-text file as live
    /// Markdown, or back as plain lines. The document is the same, so every tab that shows it
    /// follows; the file is saved exactly as before. `false` (and nothing changes) for a
    /// document that is not a plain-text file and without a tab.
    pub fn toggle_render_as_markdown(&mut self) -> bool {
        let Some(entry) = self.active_tab().map(|tab| Rc::clone(&tab.entry)) else {
            return false;
        };
        if !entry.set_render_as_markdown(!entry.render_as_markdown()) {
            return false;
        }
        for tab in self
            .tabs
            .iter_mut()
            .filter(|tab| Rc::ptr_eq(&tab.entry, &entry))
        {
            present_as(&mut tab.editor, entry.mode());
        }
        // The word count of the status bar follows the document's kind.
        self.words = None;
        self.status_key = None;
        self.requests.invalidate_all();
        true
    }

    /// Changes the display mode (normal, editor-only, full screen, Zen).
    pub fn set_mode(&mut self, mode: ShellMode) {
        self.layout.set_mode(mode);
        self.sync_toggles();
        self.requests.invalidate_all();
    }

    // ----- tabs ----------------------------------------------------------------------------

    /// Appends a tab showing `entry` and makes it the active one. Returns its key.
    pub fn add_tab(&mut self, entry: Rc<DocEntry>, workspace: Option<PathBuf>) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        match entry.path() {
            Some(path) => {
                self.titlebar
                    .tabs_mut()
                    .add_file(key, &path.to_string_lossy());
            }
            None => {
                let title = entry.title(&self.localizer);
                self.titlebar.tabs_mut().add_untitled(key, &title);
            }
        }
        let mut editor = EditorView::new(Rc::clone(entry.document()), Rc::clone(&self.clock));
        editor.set_config(self.editor_config.clone());
        present_as(&mut editor, entry.mode());
        editor.set_strings(self.editor_strings.clone());
        editor.set_clipboard(Rc::clone(&self.clipboard));
        let table = self
            .editor_setup
            .as_ref()
            .and_then(|setup| setup(&mut editor));
        self.tabs.push(TabView {
            key,
            entry,
            editor,
            table,
            workspace,
        });
        self.activate_tab(key);
        key
    }

    /// Opens `entry` as the result of "open a file": an active tab that is a pristine untitled
    /// document (see [`DocEntry::is_pristine_untitled`]) is replaced and its document released,
    /// every other tab stays. Returns the key of the new tab.
    pub fn open_tab(
        &mut self,
        entry: Rc<DocEntry>,
        workspace: Option<PathBuf>,
        docs: &Documents,
    ) -> u64 {
        let unused = self
            .active_tab()
            .filter(|tab| tab.entry.is_pristine_untitled())
            .map(|tab| tab.key);
        if let Some(removed) = unused.and_then(|key| self.close_tab(key)) {
            docs.release(&removed.entry);
        }
        self.add_tab(entry, workspace)
    }

    /// Makes the tab with `key` active. Returns whether it exists.
    pub fn activate_tab(&mut self, key: u64) -> bool {
        let Some(index) = self.tabs.iter().position(|tab| tab.key == key) else {
            return false;
        };
        self.active = Some(index);
        let doc = Rc::clone(self.tabs[index].entry.document());
        self.overlays.set_document(&doc);
        self.titlebar.tabs_mut().set_active_key(key);
        self.banners.clear();
        self.focus_target = Target::Editor;
        self.refocus_editor = true;
        self.status_key = None;
        self.requests.invalidate_all();
        true
    }

    /// Removes the tab with `key`; the neighbour becomes active. Returns the removed tab.
    pub fn close_tab(&mut self, key: u64) -> Option<TabView> {
        let index = self.tabs.iter().position(|tab| tab.key == key)?;
        let removed = self.tabs.remove(index);
        self.titlebar.tabs_mut().remove(key);
        self.banners.clear();
        self.status_key = None;
        if self.tabs.is_empty() {
            self.active = None;
            self.statusbar.clear();
        } else {
            let next = index.min(self.tabs.len() - 1);
            self.active = Some(next);
            let doc = Rc::clone(self.tabs[next].entry.document());
            self.overlays.set_document(&doc);
            let next_key = self.tabs[next].key;
            self.titlebar.tabs_mut().set_active_key(next_key);
            self.refocus_editor = true;
        }
        self.requests.invalidate_all();
        Some(removed)
    }

    /// Moves the tab `from` to `to` (a drag reorder already applied to the strip).
    pub fn reorder_tab(&mut self, from: usize, to: usize) {
        if from < self.tabs.len() && to < self.tabs.len() {
            let active_key = self.active_tab().map(|tab| tab.key);
            let tab = self.tabs.remove(from);
            self.tabs.insert(to, tab);
            self.active = active_key.and_then(|key| self.tabs.iter().position(|t| t.key == key));
        }
    }

    /// The tabs, left to right.
    #[must_use]
    pub fn tabs(&self) -> &[TabView] {
        &self.tabs
    }

    /// The tabs, left to right.
    pub fn tabs_mut(&mut self) -> &mut [TabView] {
        &mut self.tabs
    }

    /// The tab with `key`.
    #[must_use]
    pub fn tab(&self, key: u64) -> Option<&TabView> {
        self.tabs.iter().find(|tab| tab.key == key)
    }

    /// The active tab.
    #[must_use]
    pub fn active_tab(&self) -> Option<&TabView> {
        self.active.and_then(|index| self.tabs.get(index))
    }

    /// The active tab.
    pub fn active_tab_mut(&mut self) -> Option<&mut TabView> {
        self.active.and_then(|index| self.tabs.get_mut(index))
    }

    /// The active tab's editor.
    pub fn active_editor_mut(&mut self) -> Option<&mut EditorView> {
        self.active_tab_mut().map(|tab| &mut tab.editor)
    }

    /// Runs a table command that needs the host (copy as Markdown / CSV / HTML on the
    /// clipboard, edit as raw Markdown) on the table around the caret of the active editor.
    /// False for other commands, with no table at the caret, without an editor that has a
    /// table renderer, and when the clipboard refused.
    pub fn run_table_command(&mut self, command: CommandId) -> bool {
        let clipboard = Rc::clone(&self.clipboard);
        let Some(tab) = self.active.and_then(|index| self.tabs.get_mut(index)) else {
            return false;
        };
        tab.table
            .as_ref()
            .is_some_and(|table| table.execute(&mut tab.editor, &clipboard, command))
    }

    /// Refreshes the tab titles and dirty dots (`show_dirty`: autosave is off) and the way each
    /// editor presents its document (a Save as can change the file type).
    pub fn sync_tabs(&mut self, show_dirty: bool) {
        for tab in &mut self.tabs {
            present_as(&mut tab.editor, tab.entry.mode());
            let tabs = self.titlebar.tabs_mut();
            if let Some(path) = tab.entry.path() {
                let path = path.to_string_lossy();
                if tabs.path_of(tab.key) != Some(&*path) {
                    tabs.set_path(tab.key, &path);
                }
            }
            tabs.set_dirty(tab.key, show_dirty && tab.entry.is_dirty());
        }
    }

    /// Applies new editor settings (fonts, spacing, zoom) to every editor.
    pub fn set_editor_config(&mut self, config: EditorConfig) {
        for tab in &mut self.tabs {
            // How a document is presented (Markdown, plain text, Mermaid; wrapping) belongs
            // to the tab, not to the settings.
            let mut own = config.clone();
            own.document_kind = tab.editor.config().document_kind;
            own.wrap = tab.editor.config().wrap;
            tab.editor.set_config(own);
        }
        self.editor_config = config;
        self.requests.invalidate_all();
    }

    /// Sets the app-wide zoom on every editor.
    pub fn set_zoom_percent(&mut self, percent: u32) {
        for tab in &mut self.tabs {
            tab.editor.set_zoom_percent(percent);
        }
        self.editor_config.zoom_percent = percent;
        self.status_key = None;
        self.requests.invalidate_all();
    }

    // ----- panes and focus -----------------------------------------------------------------

    /// F6: moves the keyboard focus to the next (or previous) pane and returns it. The title
    /// bar owns the focus of the title row and of the command row, so they are one stop: from
    /// the title bar F6 goes on to the pane below the command row (stepping onto the command
    /// row would leave the focus where it is, forever), and Shift+F6 lands on the title row.
    pub fn next_pane(&mut self, forward: bool) -> Pane {
        let current = self.focused_pane();
        let pane = match self.layout.next_pane(Some(current), forward) {
            Pane::CommandRow if forward => self.layout.next_pane(Some(Pane::CommandRow), true),
            Pane::CommandRow => Pane::TitleRow,
            pane => pane,
        };
        self.focus_target = match pane {
            Pane::TitleRow | Pane::CommandRow => Target::Title,
            Pane::Sidebar => Target::Sidebar,
            Pane::Editor => Target::Editor,
            Pane::Inspector => Target::Inspector,
            Pane::StatusBar => Target::Status,
        };
        let id = match self.focus_target {
            Target::Title => Some(self.titlebar.tabs().id()),
            Target::Sidebar => self.sidebar.as_ref().map(Control::id),
            Target::Banners => Some(self.banners.id()),
            Target::Editor => self.active_tab().map(|tab| tab.editor.id()),
            Target::Inspector => Some(self.inspector.id()),
            Target::Status => Some(self.statusbar.id()),
        };
        if let Some(id) = id {
            self.focus.focus(id);
        }
        self.requests.invalidate_all();
        pane
    }

    /// The pane that has the keyboard focus.
    #[must_use]
    pub fn focused_pane(&self) -> Pane {
        match self.focus_target {
            Target::Title => Pane::TitleRow,
            Target::Sidebar => Pane::Sidebar,
            Target::Banners | Target::Editor => Pane::Editor,
            Target::Inspector => Pane::Inspector,
            Target::Status => Pane::StatusBar,
        }
    }

    /// The `WM_NCHITTEST` answer for `point`: the layout's chrome regions refined by the title
    /// bar (tabs and buttons are client).
    #[must_use]
    pub fn chrome_hit(&self, point: Point) -> ChromeHit {
        if self.layout.mode().shows_chrome() {
            self.titlebar.chrome_hit(&self.layout, point)
        } else {
            self.layout.chrome_hit(point)
        }
    }

    // ----- layout and paint ----------------------------------------------------------------

    fn arrange(&mut self, text: &dyn TextLayoutEngine) {
        let _layout = ssw_win::perf::stage(ssw_win::perf::Stage::Layout);
        let rects = self.layout.rects();
        let theme = self.theme;
        let scale = self.layout.scale();
        self.focus.begin_tab_order();
        {
            let mut cx = LayoutCx::new(
                self.root,
                &theme,
                scale,
                text,
                &mut self.focus,
                &mut self.requests,
            );
            if let (Some(title), Some(command)) = (rects.title_row, rects.command_row) {
                self.titlebar
                    .set_caption_width(rects.caption_buttons.map_or(0.0, |area| area.width));
                let bar = Rect::new(title.x, title.y, title.width, title.height + command.height);
                let id = self.titlebar.id();
                self.titlebar.arrange(&mut cx.for_widget(id), bar);
            }
            if let (Some(rect), Some(sidebar)) = (rects.sidebar, self.sidebar.as_mut()) {
                let id = sidebar.id();
                sidebar.arrange(&mut cx.for_widget(id), rect);
            }
            if let Some(rect) = rects.inspector {
                let id = self.inspector.id();
                self.inspector.arrange(&mut cx.for_widget(id), rect);
            }
            let mut editor_rect = rects.editor;
            let banner_height = self.banners.content_height().min(editor_rect.height);
            if banner_height > 0.0 {
                let id = self.banners.id();
                self.banners.arrange(
                    &mut cx.for_widget(id),
                    Rect::new(
                        editor_rect.x,
                        editor_rect.y,
                        editor_rect.width,
                        banner_height,
                    ),
                );
                editor_rect = Rect::new(
                    editor_rect.x,
                    editor_rect.y + banner_height,
                    editor_rect.width,
                    editor_rect.height - banner_height,
                );
            }
            if let Some(tab) = self.active.and_then(|index| self.tabs.get_mut(index)) {
                let id = tab.editor.id();
                let _editor = ssw_win::perf::stage(ssw_win::perf::Stage::EditorLayout);
                tab.editor.arrange(&mut cx.for_widget(id), editor_rect);
            }
            self.history.arrange(&mut cx, editor_rect);
            let size = self.layout.size();
            self.overlays.arrange(
                &mut cx,
                &OverlayRects {
                    window: Rect::new(0.0, 0.0, size.width, size.height),
                    editor: editor_rect,
                    sidebar: rects.sidebar,
                },
            );
            if let Some(rect) = rects.status_bar {
                let id = self.statusbar.id();
                self.statusbar.arrange(&mut cx.for_widget(id), rect);
            }
        }
        self.focus.commit_tab_order();
        let mut wanted = self.overlays.take_focus_requests();
        wanted.extend(self.history.take_focus_request());
        if !wanted.is_empty() {
            // A panel that just opened takes the keyboard. A pending return to the editor (a
            // tab opened or activated since the last layout) is older than that request.
            self.refocus_editor = false;
        }
        for id in wanted {
            self.focus.focus(id);
        }
        if self.history.take_refocus() {
            self.refocus_editor = true;
            self.focus_target = Target::Editor;
        }
        if self.overlays.take_refocus() && self.focus.focused().is_none() {
            self.refocus_editor = true;
            self.focus_target = Target::Editor;
        }
        if std::mem::take(&mut self.refocus_editor)
            && let Some(id) = self.active_tab().map(|tab| tab.editor.id())
        {
            self.focus.focus(id);
        }
        if let Some(key) = self.pending_rename.take()
            && let Some(sidebar) = self.sidebar.as_mut()
        {
            let id = sidebar.id();
            let mut cx = EventCx::new(
                id,
                &theme,
                scale,
                text,
                &mut self.focus,
                &mut self.capture,
                &mut self.requests,
            );
            if sidebar.begin_rename(&mut cx, key) {
                self.focus_target = Target::Sidebar;
            }
        }
    }

    /// Lays the view out and paints it. `mica` is false when the backdrop is not transparent
    /// (transparency off): the panes then get their solid theme colors.
    pub fn paint(&mut self, canvas: &mut dyn Canvas, text: &dyn TextLayoutEngine, mica: bool) {
        self.arrange(text);
        self.refresh_status();
        if let Some(tab) = self.active.and_then(|index| self.tabs.get_mut(index)) {
            self.overlays.sync_highlights(&mut tab.editor);
        }
        let tokens = self.theme.tokens;
        for surface in self.layout.surfaces() {
            match surface.backdrop {
                Backdrop::Solid => {
                    canvas.fill_rounded_rect(surface.rect, surface.radii, tokens.surface);
                }
                Backdrop::Mica if !mica => {
                    let color = match surface.pane {
                        Pane::Sidebar | Pane::Inspector => tokens.sidebar_bg,
                        _ => tokens.window_bg,
                    };
                    canvas.fill_rect(surface.rect, color);
                }
                Backdrop::Mica => {}
            }
        }
        let rects = self.layout.rects();
        let theme = self.theme;
        let cx = PaintCx::new(
            self.root,
            &theme,
            self.layout.scale(),
            text,
            &self.focus,
            &self.capture,
            None,
        );
        if rects.title_row.is_some() {
            self.titlebar
                .paint(&cx.for_widget(self.titlebar.id()), canvas);
        }
        if let (Some(_), Some(sidebar)) = (rects.sidebar, self.sidebar.as_ref()) {
            sidebar.paint(&cx.for_widget(sidebar.id()), canvas);
        }
        if rects.inspector.is_some() {
            self.inspector
                .paint(&cx.for_widget(self.inspector.id()), canvas);
        }
        if let Some(tab) = self.active_tab() {
            canvas.push_clip_rect(rects.editor);
            {
                let _editor = ssw_win::perf::stage(ssw_win::perf::Stage::EditorPaint);
                tab.editor.paint(&cx.for_widget(tab.editor.id()), canvas);
            }
            canvas.pop_clip();
        }
        self.history.paint(&cx, canvas);
        if self.banners.content_height() > 0.0 {
            self.banners
                .paint(&cx.for_widget(self.banners.id()), canvas);
        }
        if rects.status_bar.is_some() {
            self.statusbar
                .paint(&cx.for_widget(self.statusbar.id()), canvas);
        }
        self.overlays.paint(&cx, canvas, rects.sidebar);
    }

    // ----- input ---------------------------------------------------------------------------

    fn target_at(&self, point: Point) -> Option<Target> {
        let rects = self.layout.rects();
        if rects.title_row.is_some_and(|rect| rect.contains(point))
            || rects.command_row.is_some_and(|rect| rect.contains(point))
        {
            Some(Target::Title)
        } else if rects.sidebar.is_some_and(|rect| rect.contains(point)) {
            Some(Target::Sidebar)
        } else if rects.inspector.is_some_and(|rect| rect.contains(point)) {
            Some(Target::Inspector)
        } else if rects.status_bar.is_some_and(|rect| rect.contains(point)) {
            Some(Target::Status)
        } else if rects.editor.contains(point) {
            if self.banners.content_height() > 0.0 && self.banners.bounds().contains(point) {
                Some(Target::Banners)
            } else {
                Some(Target::Editor)
            }
        } else {
            None
        }
    }

    fn deliver(
        &mut self,
        target: Target,
        event: &Event,
        text: &dyn TextLayoutEngine,
    ) -> EventResult {
        let theme = self.theme;
        let scale = self.layout.scale();
        let Self {
            titlebar,
            sidebar,
            inspector,
            banners,
            statusbar,
            tabs,
            active,
            focus,
            capture,
            requests,
            ..
        } = self;
        let mut shared = Shared {
            theme: &theme,
            scale,
            text,
            focus,
            capture,
            requests,
        };
        match target {
            Target::Title => run(titlebar, event, &mut shared),
            Target::Sidebar => match sidebar.as_mut() {
                Some(sidebar) => run(sidebar, event, &mut shared),
                None => EventResult::ignored(),
            },
            Target::Banners => run(banners, event, &mut shared),
            Target::Inspector => run(inspector, event, &mut shared),
            Target::Status => run(statusbar, event, &mut shared),
            Target::Editor => match active.and_then(|index| tabs.get_mut(index)) {
                Some(tab) => run(&mut tab.editor, event, &mut shared),
                None => EventResult::ignored(),
            },
        }
    }

    /// Tells the editor when it gained or lost the keyboard focus (the other widgets keep their
    /// focus visuals from the focus manager).
    fn dispatch_focus(&mut self, text: &dyn TextLayoutEngine) {
        let editor = self.active_tab().map(|tab| tab.editor.id());
        for change in self.focus.take_changes() {
            if change.lost.is_some() && change.lost == editor {
                self.deliver(Target::Editor, &Event::Focus(FocusEvent::Lost), text);
            }
            if change.gained.is_some() && change.gained == editor {
                self.deliver(Target::Editor, &Event::Focus(FocusEvent::Gained), text);
            }
        }
    }

    fn pointer(&mut self, pointer: PointerEvent, event: &Event, text: &dyn TextLayoutEngine) {
        let position = pointer.position;
        if self.layout.dragging().is_some() {
            match pointer.kind {
                PointerEventKind::Move => {
                    if self.layout.drag_to(position.x) {
                        self.requests.invalidate_all();
                    }
                }
                PointerEventKind::Up => self.layout.end_drag(),
                _ => {}
            }
            self.requests.set_cursor(Cursor::SizeWE);
            return;
        }
        if pointer.kind == PointerEventKind::Down
            && pointer.button == Some(PointerButton::Left)
            && let Some(splitter) = self.layout.splitter_at(position)
        {
            self.layout.begin_drag(splitter);
            self.requests.set_cursor(Cursor::SizeWE);
            return;
        }
        if let Some(cursor) = self.layout.cursor_at(position) {
            self.requests.set_cursor(cursor);
        }
        let target = self.capture_target.or_else(|| self.target_at(position));
        if self.capture_target.is_none() && self.hover != target {
            if let Some(old) = self.hover {
                let leave =
                    Event::Pointer(PointerEvent::mouse(PointerEventKind::Leave, None, position));
                self.deliver(old, &leave, text);
            }
            self.hover = target;
        }
        let Some(target) = target else {
            return;
        };
        if pointer.kind == PointerEventKind::Down {
            self.focus_target = target;
        }
        let result = self.deliver(target, event, text);
        if target == Target::Editor
            && !result.handled
            && pointer.kind == PointerEventKind::Down
            && pointer.button == Some(PointerButton::Right)
            && let Some(tab) = self.active_tab()
        {
            self.editor_context = Some((tab.key, position));
        }
        self.capture_target = self.capture.owner().is_some().then_some(target);
    }

    /// Delivers `event` and returns what the user asked for.
    pub fn event(&mut self, event: &Event, text: &dyn TextLayoutEngine) -> Vec<ShellAction> {
        self.arrange(text);
        if self.deliver_overlays(event, text) {
            self.dispatch_focus(text);
            let actions = self.collect();
            self.refresh_status();
            return actions;
        }
        if self.deliver_history(event, text) {
            self.dispatch_focus(text);
            let actions = self.collect();
            self.refresh_status();
            return actions;
        }
        // Copy, cut and paste belong to the controller (rich copy, smart paste), not to the
        // editor's plain-text versions.
        let clipboard = match event {
            Event::Key(key) if self.focus_target == Target::Editor => key_command(key),
            _ => None,
        };
        match event {
            Event::Pointer(pointer) => self.pointer(*pointer, event, text),
            Event::Wheel(wheel) => {
                if let Some(target) = self.target_at(wheel.position) {
                    self.deliver(target, event, text);
                }
            }
            Event::Key(_) if clipboard.is_some() => {}
            Event::Key(_) | Event::Text(_) | Event::Ime(_) | Event::Focus(_) => {
                self.deliver(self.focus_target, event, text);
            }
            Event::Timer(_) | Event::AnimationFrame(_) => {
                for target in [
                    Target::Title,
                    Target::Sidebar,
                    Target::Banners,
                    Target::Editor,
                    Target::Inspector,
                    Target::Status,
                ] {
                    self.deliver(target, event, text);
                }
            }
        }
        self.dispatch_focus(text);
        let mut actions = self.collect();
        actions.extend(clipboard.map(ShellAction::Clipboard));
        self.flush_overlays(text);
        self.refresh_status();
        actions
    }

    /// Offers `event` to the overlays. True when one took it (timer and animation ticks are
    /// also the business of the other widgets).
    fn deliver_overlays(&mut self, event: &Event, text: &dyn TextLayoutEngine) -> bool {
        if matches!(event, Event::Focus(_)) {
            return false;
        }
        let theme = self.theme;
        let scale = self.layout.scale();
        let Self {
            overlays,
            focus,
            capture,
            requests,
            ..
        } = self;
        let mut env = OverlayEnv {
            theme: &theme,
            scale,
            text,
            focus,
            capture,
            requests,
        };
        let result = overlays.event(event, &mut env);
        result.handled && !matches!(event, Event::Timer(_) | Event::AnimationFrame(_))
    }

    /// Offers `event` to the version history panel (it ignores everything while closed). True
    /// when it took it (timer and animation ticks are also the business of the other widgets).
    fn deliver_history(&mut self, event: &Event, text: &dyn TextLayoutEngine) -> bool {
        let theme = self.theme;
        let scale = self.layout.scale();
        let Self {
            history,
            focus,
            capture,
            requests,
            ..
        } = self;
        let mut env = OverlayEnv {
            theme: &theme,
            scale,
            text,
            focus,
            capture,
            requests,
        };
        let result = history.event(event, &mut env);
        result.handled && !matches!(event, Event::Timer(_) | Event::AnimationFrame(_))
    }

    /// Opens what the editor asked the overlays for (they need the window's text engine).
    fn flush_overlays(&mut self, text: &dyn TextLayoutEngine) {
        let theme = self.theme;
        let scale = self.layout.scale();
        let Self {
            overlays,
            focus,
            capture,
            requests,
            ..
        } = self;
        let mut env = OverlayEnv {
            theme: &theme,
            scale,
            text,
            focus,
            capture,
            requests,
        };
        overlays.flush(&mut env);
    }

    /// The overlays of this window.
    #[must_use]
    pub fn overlays(&self) -> &Overlays {
        &self.overlays
    }

    /// The overlays of this window (the commands that open them).
    pub fn overlays_mut(&mut self) -> &mut Overlays {
        &mut self.overlays
    }

    /// The version history panel of this window.
    #[must_use]
    pub fn history(&self) -> &HistoryHost {
        &self.history
    }

    /// The version history panel of this window (opened and closed by the controller).
    pub fn history_mut(&mut self) -> &mut HistoryHost {
        &mut self.history
    }

    /// Ctrl+K: opens the link popup for the link at the caret of the active editor.
    pub fn open_link_popup(&mut self) {
        if let Some(tab) = self.active.and_then(|index| self.tabs.get(index)) {
            self.overlays.open_link(&tab.editor);
            self.requests.invalidate_all();
        }
    }

    /// Applies what the overlays decided to the active editor and returns what the controller
    /// must do (open a file, run a command, search a folder).
    pub fn pump_overlays(&mut self) -> Vec<OverlayAction> {
        let editor = self
            .active
            .and_then(|index| self.tabs.get_mut(index))
            .map(|tab| &mut tab.editor);
        self.overlays.pump(editor)
    }

    /// Drains the widgets' events.
    fn collect(&mut self) -> Vec<ShellAction> {
        let mut actions: Vec<ShellAction> = self
            .titlebar
            .take_events()
            .into_iter()
            .map(ShellAction::Title)
            .collect();
        if let Some(sidebar) = self.sidebar.as_mut() {
            actions.extend(sidebar.take_events().into_iter().map(ShellAction::Sidebar));
        }
        actions.extend(
            self.inspector
                .take_events()
                .into_iter()
                .map(ShellAction::Inspector),
        );
        actions.extend(
            self.banners
                .take_events()
                .into_iter()
                .map(ShellAction::Banner),
        );
        actions.extend(
            self.statusbar
                .take_events()
                .into_iter()
                .map(ShellAction::Status),
        );
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let key = tab.key;
            let active = self.active == Some(index);
            for event in tab.editor.take_events() {
                // Events that open an overlay (selection toolbar, heading menu) stay here.
                let event = if active {
                    self.overlays.editor_event(event, &tab.editor)
                } else {
                    Some(event)
                };
                actions.extend(event.map(|event| ShellAction::Editor(key, event)));
            }
        }
        actions.extend(self.pump_overlays().into_iter().map(ShellAction::Overlay));
        actions.extend(
            self.history
                .take_events()
                .into_iter()
                .map(ShellAction::History),
        );
        if let Some((key, at)) = self.editor_context.take() {
            actions.push(ShellAction::EditorContext { key, at });
        }
        actions
    }

    // ----- status --------------------------------------------------------------------------

    /// Recomputes the status bar and the format pill when the document, the caret or the zoom
    /// changed.
    pub fn refresh_status(&mut self) {
        let Some(tab) = self.active.and_then(|index| self.tabs.get(index)) else {
            return;
        };
        let bold = tab.editor.style_active(InlineStyle::Bold);
        let italic = tab.editor.style_active(InlineStyle::Italic);
        let underline = tab.editor.style_active(InlineStyle::Underline);
        let document = tab.entry.document().borrow();
        let selection = document.primary_selection();
        let format = tab.entry.format();
        let key = StatusKey {
            tab: tab.key,
            version: document.version(),
            anchor: selection.anchor,
            head: selection.head,
            zoom: tab.editor.zoom_percent(),
            format: (bold, italic, underline),
            file_format: format,
        };
        self.titlebar.set_format_state(FormatState {
            bold,
            italic,
            underline,
        });
        if self.status_key == Some(key) {
            return;
        }
        self.status_key = Some(key);
        let buffer = document.buffer();
        let line = buffer.line_of(selection.head);
        let line_start = buffer.line_range(line).start;
        let column = buffer
            .slice(TextRange::new(
                line_start.min(selection.head),
                selection.head,
            ))
            .chars()
            .count();
        let markdown = tab.entry.mode() != DocMode::Plain;
        let words = match self.words {
            Some((cached_tab, version, words))
                if cached_tab == tab.key && version == key.version =>
            {
                words
            }
            _ => {
                let source = document.text();
                let counted = if markdown {
                    stats::compute(&source, document.ast()).words
                } else {
                    stats::compute_plain(&source).words
                };
                self.words = Some((tab.key, key.version, counted));
                counted
            }
        };
        let selected_words = (!selection.range().is_empty()).then(|| {
            let source = document.text();
            if markdown {
                stats::for_range(&source, document.ast(), selection.range()).words
            } else {
                stats::for_range_plain(&source, selection.range()).words
            }
        });
        let encoding = if format.bom {
            self.localizer.format(
                "window-encoding-bom",
                &[("name", Arg::Str(format.encoding.name()))],
            )
        } else {
            format.encoding.name().to_owned()
        };
        let info = StatusInfo {
            line: line + 1,
            column: column + 1,
            words,
            selected_words,
            zoom_percent: tab.editor.zoom_percent(),
            encoding,
            eol: match format.line_ending {
                ssw_io::encoding::LineEnding::Lf => Eol::Lf,
                ssw_io::encoding::LineEnding::Crlf => Eol::CrLf,
                ssw_io::encoding::LineEnding::Cr => Eol::Cr,
            },
        };
        self.statusbar.set_info(&info, &self.localizer);
    }

    /// The status bar (segment texts).
    #[must_use]
    pub fn statusbar(&self) -> &StatusBar {
        &self.statusbar
    }
}

/// The state every widget of one event delivery shares.
struct Shared<'a> {
    theme: &'a Theme,
    scale: f32,
    text: &'a dyn TextLayoutEngine,
    focus: &'a mut FocusManager,
    capture: &'a mut PointerCapture,
    requests: &'a mut Requests,
}

/// Runs `widget.event` with a fresh context; a focus request is granted.
fn run<W: Control>(widget: &mut W, event: &Event, shared: &mut Shared<'_>) -> EventResult {
    let id = widget.id();
    let result = {
        let mut cx = EventCx::new(
            id,
            shared.theme,
            shared.scale,
            shared.text,
            shared.focus,
            shared.capture,
            shared.requests,
        );
        widget.event(&mut cx, event)
    };
    if result.request_focus {
        shared.focus.focus(id);
    }
    result
}
