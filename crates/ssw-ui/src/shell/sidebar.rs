//! The sidebar (spec §5): the workspace tree under a header with the root name and the ⋯ and +
//! menu buttons.
//!
//! The tree is lazy: the view asks its [`TreeSource`] for the children of a folder the first time
//! it is expanded. The app implements [`TreeSource`] on top of `ssw_io::workspace` (which already
//! sorts and filters by the [`SidebarOptions`] it receives); the view only decides how entries
//! look: extensions shown or not, files that do not open in the editor dimmed, one icon per class
//! of file, a cloud icon for placeholders, the modified date under the name.
//!
//! Everything the user can ask for is a [`SidebarEvent`]: opening, expanding, context menu
//! requests (the app shows the native menu), the ⋯ and + menus, inline rename results, delete,
//! and the drag source / drop target protocol. The app answers by changing files and calling
//! [`SidebarView::refresh_folder`].

use std::collections::HashMap;

use ssw_core::i18n::Localizer;

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, IconFont};
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::{
    Cursor, Event, FocusEvent, KeyEvent, PointerButton, PointerEvent, PointerEventKind, vk,
};
use crate::text::FontWeight;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::button::Button;
use crate::widgets::scroll::{Axis, BAR_WIDTH, ScrollBar, scroll_info};
use crate::widgets::textbox::TextBox;
use crate::widgets::tree::DRAG_THRESHOLD;
use crate::widgets::{
    Control, FONT_SIZE, SharedClipboard, draw_left_centered, focus_ring, glyph, layout_line,
    ui_style,
};

/// Height of the header with the root name and the menu buttons.
pub const HEADER_HEIGHT: f32 = 40.0;
/// Height of a row.
pub const ROW_HEIGHT: f32 = 32.0;
/// Height of a row that shows the modified date under the name.
pub const ROW_HEIGHT_WITH_DATE: f32 = 48.0;
/// Indentation per depth.
const INDENT: f32 = 16.0;
/// Width of the chevron zone.
const CHEVRON: f32 = 16.0;
/// Em size of the chevron glyph.
const CHEVRON_SIZE: f32 = 12.0;
/// Size of the file and folder icons.
const ICON: f32 = 16.0;
/// Gap between icon and label.
const GAP: f32 = 8.0;
/// Side of the header buttons.
const HEADER_BUTTON: f32 = 32.0;
/// Segoe Fluent Icons code points.
const FOLDER: char = '\u{E8B7}';
const FOLDER_OPEN: char = '\u{E838}';
const DOCUMENT: char = '\u{E8A5}';
const CODE: char = '\u{E943}';
const IMAGE: char = '\u{E91B}';
const PAGE: char = '\u{E7C3}';
const CLOUD: char = '\u{E753}';
const MORE: char = '\u{E712}';

/// The identity of a tree entry, unique within one [`TreeSource`].
pub type NodeKey = u64;

/// What kind of entry a file is; decides the icon and whether the name is dimmed. A file that
/// opens in the editor is drawn at full strength, one known to open in another app is dimmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Folder,
    /// Markdown, plain text and word-processor documents (opened in the editor).
    Prose,
    /// Source code, markup, data and configuration files, and files of a type nobody listed:
    /// the editor opens them as text (opened in the editor).
    Code,
    /// Pictures (opened by the default app).
    Image,
    /// Types known not to be text, such as archives, executables and media (opened by the
    /// default app).
    Other,
}

/// One entry of a folder listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarEntry {
    pub key: NodeKey,
    /// The file or folder name with its extension.
    pub name: String,
    pub kind: EntryKind,
    /// A OneDrive placeholder: shown with a cloud icon, never hydrated by the tree.
    pub placeholder: bool,
    /// A hidden or system item (shown only when hidden items are enabled): drawn dimmer still.
    pub hidden: bool,
    /// The modified date, formatted by the app for the active locale; shown under the name when
    /// [`SidebarOptions::show_modified`] is on.
    pub modified: String,
}

impl SidebarEntry {
    /// A folder entry.
    #[must_use]
    pub fn folder(key: NodeKey, name: impl Into<String>) -> Self {
        Self::new(key, name, EntryKind::Folder)
    }

    /// A file entry of `kind`.
    #[must_use]
    pub fn file(key: NodeKey, name: impl Into<String>, kind: EntryKind) -> Self {
        Self::new(key, name, kind)
    }

    fn new(key: NodeKey, name: impl Into<String>, kind: EntryKind) -> Self {
        Self {
            key,
            name: name.into(),
            kind,
            placeholder: false,
            hidden: false,
            modified: String::new(),
        }
    }

    #[must_use]
    pub fn is_folder(&self) -> bool {
        self.kind == EntryKind::Folder
    }

    /// Whether the name is drawn dimmed: a file that does not open in the editor.
    #[must_use]
    pub fn is_dimmed(&self) -> bool {
        matches!(self.kind, EntryKind::Image | EntryKind::Other)
    }

    /// The name shown in the tree: files lose their extension when `show_extensions` is off.
    #[must_use]
    pub fn display_name(&self, show_extensions: bool) -> &str {
        if show_extensions || self.is_folder() {
            return &self.name;
        }
        match self.name.rfind('.') {
            Some(dot) if dot > 0 => &self.name[..dot],
            _ => &self.name,
        }
    }
}

/// The sort key of a folder listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Name,
    Modified,
    Created,
    Kind,
}

/// How the tree is sorted, filtered and drawn (spec §5). The source applies sorting and the
/// filters when it lists a folder; the view applies `show_extensions` and `show_modified`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidebarOptions {
    pub sort: SortKey,
    pub descending: bool,
    pub folders_first: bool,
    pub show_extensions: bool,
    pub show_hidden: bool,
    pub hide_images: bool,
    pub hide_non_markdown: bool,
    /// Show the modified date under each name.
    pub show_modified: bool,
}

impl Default for SidebarOptions {
    fn default() -> Self {
        Self {
            sort: SortKey::Name,
            descending: false,
            folders_first: true,
            show_extensions: true,
            show_hidden: false,
            hide_images: false,
            hide_non_markdown: false,
            show_modified: false,
        }
    }
}

/// Where the tree gets its folders from. The app adapts `ssw_io::workspace` to this trait.
pub trait TreeSource {
    /// The name of the workspace root (the header text).
    fn root_name(&self) -> String;

    /// The key of the root folder, under which the top-level entries are listed.
    fn root_key(&self) -> NodeKey;

    /// The visible children of `folder`, sorted and filtered per `options`. Called the first time
    /// a folder is expanded and again on [`SidebarView::refresh_folder`]. An unreadable folder
    /// lists as empty.
    fn children(&mut self, folder: NodeKey, options: &SidebarOptions) -> Vec<SidebarEntry>;
}

/// Localized strings of the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarStrings {
    /// Accessible name of the sidebar.
    pub label: String,
    /// Accessible name of the tree.
    pub tree: String,
    /// The ⋯ button.
    pub more: String,
    /// The + button.
    pub new: String,
    /// Accessible name of the inline rename box.
    pub rename: String,
    /// Description of placeholder files.
    pub cloud: String,
    /// Shown when the workspace has no entries.
    pub empty: String,
}

impl SidebarStrings {
    /// Looks the strings up in `main.ftl` and `shell.ftl`.
    #[must_use]
    pub fn from_localizer(localizer: &Localizer) -> Self {
        Self {
            label: localizer.text("cmd-view-toggle-sidebar"),
            tree: localizer.text("sidebar-tree"),
            more: localizer.text("sidebar-more"),
            new: localizer.text("sidebar-new"),
            rename: localizer.text("sidebar-rename"),
            cloud: localizer.text("sidebar-cloud"),
            empty: localizer.text("sidebar-empty"),
        }
    }
}

/// The two header menus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderMenu {
    /// ⋯: sort, filters, collapse/expand all.
    Options,
    /// +: New file, New folder.
    New,
}

/// What the user did in the sidebar.
#[derive(Debug, Clone, PartialEq)]
pub enum SidebarEvent {
    Selected(NodeKey),
    /// Enter or double click on a file. `new_tab` when Ctrl was held.
    Open {
        key: NodeKey,
        new_tab: bool,
    },
    Expanded(NodeKey),
    Collapsed(NodeKey),
    /// Right click or the context menu key; `key` is `None` over empty space (the root).
    ContextMenu {
        key: Option<NodeKey>,
        at: Point,
    },
    /// Show the native popup of a header menu below `anchor`.
    HeaderMenu {
        menu: HeaderMenu,
        anchor: Point,
    },
    /// Enter in the rename box: rename `key` to `name` (not validated).
    RenameCommitted {
        key: NodeKey,
        name: String,
    },
    /// Escape, or an empty or unchanged name.
    RenameCancelled(NodeKey),
    /// Delete on the selected entry.
    DeleteRequested(NodeKey),
    /// A press moved past the drag threshold.
    DragStarted(NodeKey),
    /// Released over a folder (or the root) that accepts the entry: move it there, or copy it when
    /// `copy` (Ctrl held).
    Dropped {
        source: NodeKey,
        target_folder: NodeKey,
        copy: bool,
    },
    /// Released outside the tree: the app decides (insert a link into the editor, copy to
    /// Explorer…). `at` is in window coordinates.
    DragReleased {
        source: NodeKey,
        at: Point,
        copy: bool,
    },
    /// The drag ended without a target (Escape, or released on an invalid target).
    DragCancelled(NodeKey),
}

#[derive(Debug, Clone)]
struct Node {
    entry: SidebarEntry,
    id: WidgetId,
    /// `None` until a folder has been expanded for the first time.
    children: Option<Vec<Node>>,
    expanded: bool,
}

impl Node {
    fn new(entry: SidebarEntry) -> Self {
        Self {
            entry,
            id: WidgetId::allocate(),
            children: None,
            expanded: false,
        }
    }

    fn contains(&self, key: NodeKey) -> bool {
        self.entry.key == key
            || self
                .children
                .iter()
                .flatten()
                .any(|child| child.contains(key))
    }
}

fn find(nodes: &[Node], key: NodeKey) -> Option<&Node> {
    nodes.iter().find_map(|node| {
        if node.entry.key == key {
            Some(node)
        } else {
            node.children.as_deref().and_then(|c| find(c, key))
        }
    })
}

fn find_mut(nodes: &mut [Node], key: NodeKey) -> Option<&mut Node> {
    nodes.iter_mut().find_map(|node| {
        if node.entry.key == key {
            Some(node)
        } else {
            node.children.as_deref_mut().and_then(|c| find_mut(c, key))
        }
    })
}

/// The key of the folder that lists `key`, or `None` when `key` is a top-level entry.
fn parent_of(nodes: &[Node], key: NodeKey) -> Option<NodeKey> {
    nodes.iter().find_map(|node| {
        let children = node.children.as_deref()?;
        if children.iter().any(|child| child.entry.key == key) {
            Some(node.entry.key)
        } else {
            parent_of(children, key)
        }
    })
}

#[derive(Debug, Clone)]
struct Row {
    key: NodeKey,
    id: WidgetId,
    depth: usize,
    expandable: bool,
    expanded: bool,
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    source: NodeKey,
    /// The folder the entry would land in, when that is valid.
    target: Option<NodeKey>,
}

struct Rename {
    key: NodeKey,
    original: String,
    text_box: TextBox,
}

/// The workspace sidebar.
pub struct SidebarView {
    id: WidgetId,
    header_id: WidgetId,
    tree_id: WidgetId,
    strings: SidebarStrings,
    source: Box<dyn TreeSource>,
    clipboard: SharedClipboard,
    options: SidebarOptions,
    root_name: String,
    root_key: NodeKey,
    roots: Vec<Node>,
    rows: Vec<Row>,
    selected: Option<NodeKey>,
    hover: Option<usize>,
    more: Button,
    new: Button,
    rename: Option<Rename>,
    bar: ScrollBar,
    press: Option<(Point, NodeKey)>,
    drag: Option<Drag>,
    bounds: Rect,
    events: Vec<SidebarEvent>,
    motion: Motion,
}

impl std::fmt::Debug for SidebarView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidebarView")
            .field("root_name", &self.root_name)
            .field("rows", &self.rows.len())
            .field("selected", &self.selected)
            .field("bounds", &self.bounds)
            .finish_non_exhaustive()
    }
}

impl SidebarView {
    /// A sidebar over `source`; the top-level entries are listed immediately.
    #[must_use]
    pub fn new(
        strings: SidebarStrings,
        source: Box<dyn TreeSource>,
        clipboard: SharedClipboard,
        options: SidebarOptions,
    ) -> Self {
        let mut view = Self {
            id: WidgetId::allocate(),
            header_id: WidgetId::allocate(),
            tree_id: WidgetId::allocate(),
            more: Button::icon(MORE, &strings.more),
            new: Button::icon(glyph::ADD, &strings.new),
            strings,
            root_name: source.root_name(),
            root_key: source.root_key(),
            source,
            clipboard,
            options,
            roots: Vec::new(),
            rows: Vec::new(),
            selected: None,
            hover: None,
            rename: None,
            bar: ScrollBar::new(Axis::Vertical, ""),
            press: None,
            drag: None,
            bounds: Rect::ZERO,
            events: Vec::new(),
            motion: Motion::full(),
        };
        view.refresh_all();
        view
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.motion = motion;
        self.bar = self.bar.with_motion(motion);
        self.more = self.more.with_motion(motion);
        self.new = self.new.with_motion(motion);
        self
    }

    #[must_use]
    pub fn options(&self) -> SidebarOptions {
        self.options
    }

    /// Applies new view options and lists the visible folders again.
    pub fn set_options(&mut self, options: SidebarOptions) {
        self.options = options;
        self.refresh_all();
    }

    /// The root name shown in the header.
    #[must_use]
    pub fn root_name(&self) -> &str {
        &self.root_name
    }

    #[must_use]
    pub fn selected_key(&self) -> Option<NodeKey> {
        self.selected
    }

    /// Selects `key` when it is visible, without an event; unknown or hidden keys clear it.
    pub fn select_key(&mut self, key: NodeKey) {
        self.selected = self.rows.iter().any(|row| row.key == key).then_some(key);
        self.reveal_selection();
    }

    /// The entry with `key`, when it has been listed.
    #[must_use]
    pub fn entry(&self, key: NodeKey) -> Option<&SidebarEntry> {
        find(&self.roots, key).map(|node| &node.entry)
    }

    /// Whether the folder `key` is expanded; `None` for files and unknown keys.
    #[must_use]
    pub fn is_expanded(&self, key: NodeKey) -> Option<bool> {
        find(&self.roots, key)
            .filter(|node| node.entry.is_folder())
            .map(|node| node.expanded)
    }

    /// Number of visible rows.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The key of visible row `index`.
    #[must_use]
    pub fn row_key(&self, index: usize) -> Option<NodeKey> {
        self.rows.get(index).map(|row| row.key)
    }

    /// The names of the visible rows as drawn.
    #[must_use]
    pub fn row_names(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter_map(|row| find(&self.roots, row.key))
            .map(|node| {
                node.entry
                    .display_name(self.options.show_extensions)
                    .to_owned()
            })
            .collect()
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    #[must_use]
    pub fn scroll_offset(&self) -> f32 {
        self.bar.offset()
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<SidebarEvent> {
        std::mem::take(&mut self.events)
    }

    /// The rename box is open for this entry.
    #[must_use]
    pub fn renaming(&self) -> Option<NodeKey> {
        self.rename.as_ref().map(|rename| rename.key)
    }

    /// The folder a drop at `point` would land in: the folder under the pointer, the parent
    /// folder of a file under it, or the root over empty space. `None` outside the tree. External
    /// drags (files from Explorer) use this too.
    #[must_use]
    pub fn drop_target_at(&self, point: Point) -> Option<NodeKey> {
        if !self.list_rect().contains(point) {
            return None;
        }
        let Some(index) = self.row_at(point) else {
            return Some(self.root_key);
        };
        let row = &self.rows[index];
        if row.expandable {
            Some(row.key)
        } else {
            Some(parent_of(&self.roots, row.key).unwrap_or(self.root_key))
        }
    }

    fn row_height(&self) -> f32 {
        if self.options.show_modified {
            ROW_HEIGHT_WITH_DATE
        } else {
            ROW_HEIGHT
        }
    }

    fn header_rect(&self) -> Rect {
        Rect::new(
            self.bounds.x,
            self.bounds.y,
            self.bounds.width,
            HEADER_HEIGHT,
        )
    }

    fn list_rect(&self) -> Rect {
        let top = self.bounds.y + HEADER_HEIGHT;
        Rect::new(
            self.bounds.x,
            top,
            self.bounds.width,
            (self.bounds.bottom() - top).max(0.0),
        )
    }

    /// Bounds of visible row `index` in window coordinates.
    #[must_use]
    pub fn row_rect(&self, index: usize) -> Rect {
        let list = self.list_rect();
        Rect::new(
            list.x,
            list.y + index as f32 * self.row_height() - self.bar.offset(),
            list.width,
            self.row_height(),
        )
    }

    fn visible_range(&self) -> std::ops::Range<usize> {
        let height = self.row_height();
        let offset = self.bar.offset();
        let first = (offset / height).floor() as usize;
        let end = ((offset + self.list_rect().height) / height).ceil() as usize;
        first.min(self.rows.len())..end.min(self.rows.len())
    }

    fn row_at(&self, point: Point) -> Option<usize> {
        let list = self.list_rect();
        if !list.contains(point) {
            return None;
        }
        let y = point.y - list.y + self.bar.offset();
        let index = (y / self.row_height()) as usize;
        (index < self.rows.len()).then_some(index)
    }

    fn chevron_rect(&self, index: usize) -> Rect {
        let row = self.row_rect(index);
        Rect::new(
            row.x + 4.0 + self.rows[index].depth as f32 * INDENT,
            row.y,
            CHEVRON,
            row.height,
        )
    }

    fn icon_x(&self, index: usize) -> f32 {
        self.chevron_rect(index).right() + GAP / 2.0 + ICON / 2.0
    }

    fn label_x(&self, index: usize) -> f32 {
        self.chevron_rect(index).right() + GAP / 2.0 + ICON + GAP
    }

    fn sync_scroll(&mut self) {
        let content = self.rows.len() as f32 * self.row_height();
        let viewport = self.list_rect().height;
        self.bar.model_mut().set_extent(viewport, content);
    }

    fn rebuild_rows(&mut self) {
        fn walk(nodes: &[Node], depth: usize, out: &mut Vec<Row>) {
            for node in nodes {
                let expandable = node.entry.is_folder();
                out.push(Row {
                    key: node.entry.key,
                    id: node.id,
                    depth,
                    expandable,
                    expanded: expandable && node.expanded,
                });
                if let Some(children) = node.children.as_deref().filter(|_| node.expanded) {
                    walk(children, depth + 1, out);
                }
            }
        }
        let mut rows = Vec::new();
        walk(&self.roots, 0, &mut rows);
        self.rows = rows;
        self.hover = None;
        if self
            .selected
            .is_some_and(|key| !self.rows.iter().any(|r| r.key == key))
        {
            self.selected = None;
        }
        self.sync_scroll();
    }

    /// Lists the children of `folder`, keeping the expanded state (and the listings) of entries
    /// that still exist in `previous`. Borrows the source and the options rather than the view,
    /// so the caller can hold the node the listing is for.
    fn list(
        source: &mut dyn TreeSource,
        options: &SidebarOptions,
        folder: NodeKey,
        previous: Option<Vec<Node>>,
    ) -> Vec<Node> {
        let mut previous: HashMap<NodeKey, Node> = previous
            .unwrap_or_default()
            .into_iter()
            .map(|node| (node.entry.key, node))
            .collect();
        let entries = source.children(folder, options);
        let mut nodes = Vec::with_capacity(entries.len());
        for entry in entries {
            let old = previous.remove(&entry.key);
            let mut node = Node::new(entry);
            if let Some(old) = old {
                node.id = old.id;
                if node.entry.is_folder() && old.expanded {
                    node.expanded = true;
                    node.children = Some(Self::list(source, options, node.entry.key, old.children));
                }
            }
            nodes.push(node);
        }
        nodes
    }

    /// Lists the root and every expanded folder again (the options changed, or files changed on
    /// disk). Expansion and selection survive.
    pub fn refresh_all(&mut self) {
        self.root_name = self.source.root_name();
        let previous = std::mem::take(&mut self.roots);
        self.roots = Self::list(
            &mut *self.source,
            &self.options,
            self.root_key,
            Some(previous),
        );
        self.rebuild_rows();
    }

    /// Lists `folder` again (the live-update path); the root key refreshes the top level. Unknown
    /// or collapsed folders are ignored.
    pub fn refresh_folder(&mut self, folder: NodeKey) {
        if folder == self.root_key {
            self.refresh_all();
            return;
        }
        let Some(node) = find_mut(&mut self.roots, folder) else {
            return;
        };
        if !node.entry.is_folder() || node.children.is_none() {
            return;
        }
        let previous = node.children.take();
        node.children = Some(Self::list(
            &mut *self.source,
            &self.options,
            folder,
            previous,
        ));
        self.rebuild_rows();
    }

    /// Scrolls the least amount that shows row `index`.
    fn reveal_row(&mut self, index: usize) {
        let height = self.row_height();
        let top = index as f32 * height;
        self.bar.model_mut().ensure_visible(top, top + height);
    }

    fn row_of(&self, key: NodeKey) -> Option<usize> {
        self.rows.iter().position(|row| row.key == key)
    }

    fn reveal_selection(&mut self) {
        if let Some(index) = self.selected.and_then(|key| self.row_of(key)) {
            self.reveal_row(index);
        }
    }

    /// Expands or collapses a folder, listing it on first expansion. Returns whether it changed.
    fn set_expanded(&mut self, key: NodeKey, expanded: bool) -> bool {
        let Some(node) = find_mut(&mut self.roots, key) else {
            return false;
        };
        if !node.entry.is_folder() || node.expanded == expanded {
            return false;
        }
        node.expanded = expanded;
        if expanded && node.children.is_none() {
            node.children = Some(Self::list(&mut *self.source, &self.options, key, None));
        }
        let had_selection = self.selected.is_some();
        self.rebuild_rows();
        if !expanded && had_selection && self.selected.is_none() {
            // Collapsing hid the selection: it moves to the collapsed folder.
            self.selected = Some(key);
        }
        true
    }

    /// Expands or collapses `key` and reports it.
    fn user_expand(&mut self, key: NodeKey, expanded: bool) {
        if self.set_expanded(key, expanded) {
            self.events.push(if expanded {
                SidebarEvent::Expanded(key)
            } else {
                SidebarEvent::Collapsed(key)
            });
            if !expanded && self.selected == Some(key) {
                self.events.push(SidebarEvent::Selected(key));
            }
        }
    }

    /// Collapses every folder (the ⋯ menu).
    pub fn collapse_all(&mut self) {
        fn walk(nodes: &mut [Node]) {
            for node in nodes {
                node.expanded = false;
                if let Some(children) = node.children.as_deref_mut() {
                    walk(children);
                }
            }
        }
        walk(&mut self.roots);
        self.rebuild_rows();
    }

    /// Expands every folder, listing all of them (the ⋯ menu).
    pub fn expand_all(&mut self) {
        Self::expand_nodes(&mut *self.source, &self.options, &mut self.roots);
        self.rebuild_rows();
    }

    /// Expands every folder below `nodes`, listing the ones not listed yet.
    fn expand_nodes(source: &mut dyn TreeSource, options: &SidebarOptions, nodes: &mut [Node]) {
        for node in nodes.iter_mut().filter(|node| node.entry.is_folder()) {
            node.expanded = true;
            let key = node.entry.key;
            let children = node
                .children
                .get_or_insert_with(|| Self::list(source, options, key, None));
            Self::expand_nodes(source, options, children);
        }
    }

    fn select_row(&mut self, index: usize) {
        let key = self.rows[index].key;
        if self.selected != Some(key) {
            self.selected = Some(key);
            self.events.push(SidebarEvent::Selected(key));
        }
        self.reveal_row(index);
    }

    /// Enter or double click: files open, folders toggle.
    fn activate(&mut self, index: usize, new_tab: bool) {
        let key = self.rows[index].key;
        if self.rows[index].expandable {
            let expand = !self.rows[index].expanded;
            self.user_expand(key, expand);
        } else {
            self.events.push(SidebarEvent::Open { key, new_tab });
        }
    }

    /// Opens the inline rename box for `key` (F2, or after creating a file). The box is focused
    /// and its text is selected up to the extension. Returns whether `key` is visible.
    pub fn begin_rename(&mut self, cx: &mut EventCx, key: NodeKey) -> bool {
        let Some(node) = find(&self.roots, key) else {
            return false;
        };
        if self.row_of(key).is_none() {
            return false;
        }
        let name = node.entry.name.clone();
        let stem = if node.entry.is_folder() {
            name.len()
        } else {
            node.entry.display_name(false).len()
        };
        let mut text_box = TextBox::new(&self.strings.rename, &name, self.clipboard.clone())
            .with_motion(self.motion);
        text_box.set_selection(0, stem);
        let box_id = text_box.id();
        let mut order = cx.focus.tab_order().to_vec();
        order.push(box_id);
        cx.focus.set_tab_order(order);
        cx.focus.focus(box_id);
        self.rename = Some(Rename {
            key,
            original: name,
            text_box,
        });
        cx.invalidate(self.bounds);
        true
    }

    fn end_rename(&mut self, cx: &mut EventCx, commit: bool) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        let name = rename.text_box.text().trim().to_owned();
        if commit && !name.is_empty() && name != rename.original {
            self.events.push(SidebarEvent::RenameCommitted {
                key: rename.key,
                name,
            });
        } else {
            self.events.push(SidebarEvent::RenameCancelled(rename.key));
        }
        cx.focus.focus(self.tree_id);
        cx.invalidate(self.bounds);
    }

    fn rename_rect(&self) -> Option<Rect> {
        let rename = self.rename.as_ref()?;
        let index = self.row_of(rename.key)?;
        let row = self.row_rect(index);
        let x = self.label_x(index) - 4.0;
        Some(Rect::new(
            x,
            row.y + 2.0,
            (row.right() - BAR_WIDTH - x - 4.0).max(0.0),
            ROW_HEIGHT - 4.0,
        ))
    }

    fn valid_target(&self, source: NodeKey, folder: NodeKey) -> bool {
        let inside_source = find(&self.roots, source).is_some_and(|node| node.contains(folder));
        let current_parent = parent_of(&self.roots, source).unwrap_or(self.root_key);
        !inside_source && folder != current_parent
    }

    fn header_pointer(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        self.forward_to_buttons(cx, &Event::Pointer(*event))
    }

    fn pointer(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        let header = self.header_pointer(cx, event);
        if header.handled {
            return header;
        }
        let bar = self.bar.event(cx, &Event::Pointer(*event));
        if self.bar.take_changed() {
            cx.invalidate(self.bounds);
        }
        if bar.handled {
            return bar;
        }
        let row = self.row_at(event.position);
        match (event.kind, event.button) {
            (PointerEventKind::Down, Some(PointerButton::Left)) => {
                let Some(index) = row else {
                    return if self.list_rect().contains(event.position) {
                        EventResult::handled_with_focus()
                    } else {
                        EventResult::ignored()
                    };
                };
                let key = self.rows[index].key;
                if self.rows[index].expandable && self.chevron_rect(index).contains(event.position)
                {
                    let expand = !self.rows[index].expanded;
                    self.user_expand(key, expand);
                } else {
                    self.select_row(index);
                    if event.click_count == 2 {
                        self.activate(index, event.mods.ctrl());
                    }
                    self.press = Some((event.position, key));
                    cx.capture_pointer();
                }
                cx.invalidate(self.bounds);
                EventResult::handled_with_focus()
            }
            (PointerEventKind::Down, Some(PointerButton::Right)) => {
                if !self.list_rect().contains(event.position) {
                    return EventResult::ignored();
                }
                let key = row.map(|index| {
                    self.select_row(index);
                    self.rows[index].key
                });
                self.events.push(SidebarEvent::ContextMenu {
                    key,
                    at: event.position,
                });
                cx.invalidate(self.bounds);
                EventResult::handled_with_focus()
            }
            (PointerEventKind::Move, _) => self.pointer_move(cx, event, row),
            (PointerEventKind::Enter, _) => self.hover_to(cx, row),
            (PointerEventKind::Leave, _) if self.press.is_none() => self.hover_to(cx, None),
            (PointerEventKind::Up, Some(PointerButton::Left)) if self.press.is_some() => {
                self.press = None;
                cx.release_pointer();
                if let Some(drag) = self.drag.take() {
                    self.finish_drag(drag, event);
                }
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            _ => EventResult::ignored(),
        }
    }

    fn pointer_move(
        &mut self,
        cx: &mut EventCx,
        event: &PointerEvent,
        row: Option<usize>,
    ) -> EventResult {
        if row.is_some() || self.press.is_some() {
            cx.set_cursor(Cursor::Hand);
        }
        if let Some(drag) = self.drag {
            let target = self
                .drop_target_at(event.position)
                .filter(|folder| self.valid_target(drag.source, *folder));
            self.drag = Some(Drag { target, ..drag });
            cx.invalidate(self.bounds);
            return EventResult::handled();
        }
        if let Some((origin, source)) = self.press {
            if origin.distance(event.position) >= DRAG_THRESHOLD {
                self.drag = Some(Drag {
                    source,
                    target: None,
                });
                self.events.push(SidebarEvent::DragStarted(source));
                cx.invalidate(self.bounds);
            }
            return EventResult::handled();
        }
        self.hover_to(cx, row)
    }

    fn finish_drag(&mut self, drag: Drag, event: &PointerEvent) {
        let copy = event.mods.ctrl();
        let outside = !self.bounds.contains(event.position);
        self.events.push(match drag.target {
            Some(target_folder) => SidebarEvent::Dropped {
                source: drag.source,
                target_folder,
                copy,
            },
            None if outside => SidebarEvent::DragReleased {
                source: drag.source,
                at: event.position,
                copy,
            },
            None => SidebarEvent::DragCancelled(drag.source),
        });
    }

    fn hover_to(&mut self, cx: &mut EventCx, hover: Option<usize>) -> EventResult {
        if hover == self.hover {
            return EventResult::ignored();
        }
        self.hover = hover;
        cx.invalidate(self.bounds);
        EventResult::handled()
    }

    fn page_rows(&self) -> usize {
        ((self.list_rect().height / self.row_height()) as usize)
            .saturating_sub(1)
            .max(1)
    }

    fn key(&mut self, key: &KeyEvent, cx: &mut EventCx) -> bool {
        if key.vk == vk::ESCAPE
            && let Some(drag) = self.drag.take()
        {
            self.events.push(SidebarEvent::DragCancelled(drag.source));
            self.press = None;
            return true;
        }
        let current = self.selected.and_then(|k| self.row_of(k));
        if key.vk == vk::APPS || (key.vk == vk::F10 && key.mods == crate::input::Modifiers::SHIFT) {
            let at = current.map_or(self.list_rect().origin(), |index| {
                let rect = self.row_rect(index);
                Point::new(rect.x + rect.width / 2.0, rect.bottom())
            });
            self.events.push(SidebarEvent::ContextMenu {
                key: current.map(|index| self.rows[index].key),
                at,
            });
            return true;
        }
        if self.rows.is_empty() {
            return false;
        }
        let last = self.rows.len() - 1;
        match key.vk {
            vk::F2 if key.mods.is_empty() => {
                if let Some(index) = current {
                    let selected = self.rows[index].key;
                    self.begin_rename(cx, selected);
                }
                true
            }
            vk::DELETE if key.mods.is_empty() => {
                if let Some(index) = current {
                    self.events
                        .push(SidebarEvent::DeleteRequested(self.rows[index].key));
                }
                true
            }
            vk::RETURN => {
                if let Some(index) = current {
                    self.activate(index, key.mods.ctrl());
                }
                true
            }
            _ if !key.mods.is_empty() => false,
            vk::UP | vk::DOWN | vk::PAGE_UP | vk::PAGE_DOWN | vk::HOME | vk::END => {
                let page = self.page_rows();
                let target = match (key.vk, current) {
                    (vk::HOME, _) => 0,
                    (vk::END, _) => last,
                    (_, None) => 0,
                    (vk::UP, Some(index)) => index.saturating_sub(1),
                    (vk::DOWN, Some(index)) => (index + 1).min(last),
                    (vk::PAGE_UP, Some(index)) => index.saturating_sub(page),
                    (_, Some(index)) => (index + page).min(last),
                };
                self.select_row(target);
                true
            }
            vk::RIGHT => {
                let Some(index) = current else { return true };
                let row = self.rows[index].clone();
                if row.expandable && !row.expanded {
                    self.user_expand(row.key, true);
                } else if row.expanded && index < last {
                    self.select_row(index + 1);
                }
                true
            }
            vk::LEFT => {
                let Some(index) = current else { return true };
                let row = self.rows[index].clone();
                if row.expanded {
                    self.user_expand(row.key, false);
                } else if let Some(parent) =
                    parent_of(&self.roots, row.key).and_then(|parent| self.row_of(parent))
                {
                    self.select_row(parent);
                }
                true
            }
            _ => false,
        }
    }

    fn row_node(&self, cx: &AccessCx, index: usize) -> AccessNode {
        let row = &self.rows[index];
        let entry = find(&self.roots, row.key).map(|node| &node.entry);
        let name = entry.map_or("", |e| e.display_name(self.options.show_extensions));
        let mut node = AccessNode::new(row.id, Role::TreeItem, name, self.row_rect(index));
        node.state.selected = self.selected == Some(row.key);
        node.state.focusable = true;
        node.state.focused = cx.for_widget(self.tree_id).is_focused() && node.state.selected;
        node.state.expanded = row.expandable.then_some(row.expanded);
        node.patterns.selection_item = true;
        node.patterns.expand_collapse = row.expandable;
        node.patterns.invoke = true;
        if entry.is_some_and(|e| e.placeholder) {
            node.description = self.strings.cloud.clone();
        }
        node
    }

    /// Carries out an access action on the row whose access node has id `item`.
    pub fn perform_item(
        &mut self,
        cx: &mut EventCx,
        item: WidgetId,
        action: &AccessAction,
    ) -> bool {
        let Some(index) = self.rows.iter().position(|row| row.id == item) else {
            return false;
        };
        let key = self.rows[index].key;
        let expandable = self.rows[index].expandable;
        let done = match action {
            AccessAction::Select => {
                self.select_row(index);
                cx.focus.focus(self.tree_id);
                true
            }
            AccessAction::Invoke => {
                self.activate(index, false);
                true
            }
            AccessAction::Expand if expandable => {
                self.user_expand(key, true);
                true
            }
            AccessAction::Collapse if expandable => {
                self.user_expand(key, false);
                true
            }
            AccessAction::ScrollIntoView => {
                self.reveal_row(index);
                true
            }
            _ => false,
        };
        if done {
            cx.invalidate(self.bounds);
        }
        done
    }

    fn paint_row(&self, cx: &PaintCx, canvas: &mut dyn Canvas, index: usize) {
        let tokens = &cx.theme.tokens;
        let row = &self.rows[index];
        let node = find(&self.roots, row.key).expect("every row stands for a node of the tree");
        let entry = &node.entry;
        let rect = self.row_rect(index);
        let radius = CornerRadii::uniform(tokens.metrics.control_radius);
        let pill = rect.inset(Insets::symmetric(4.0, 2.0));
        let selected = self.selected == Some(row.key);
        if selected {
            canvas.fill_rounded_rect(pill, radius, tokens.pressed_bg);
            canvas.fill_rounded_rect(
                Rect::new(rect.x + 2.0, rect.center().y - 8.0, 3.0, 16.0),
                CornerRadii::uniform(1.5),
                tokens.accent,
            );
        } else if self.hover == Some(index) {
            canvas.fill_rounded_rect(pill, radius, tokens.hover_bg);
        }
        let dragged = self.drag.is_some_and(|drag| drag.source == row.key);
        let color = if entry.hidden {
            tokens.text_disabled
        } else if entry.is_dimmed() || dragged {
            tokens.text_muted
        } else {
            tokens.text
        };
        let chevron = self.chevron_rect(index);
        if row.expandable {
            let symbol = if row.expanded {
                glyph::CHEVRON_DOWN
            } else {
                glyph::CHEVRON_RIGHT
            };
            canvas.draw_icon(
                symbol,
                IconFont::SegoeFluent,
                CHEVRON_SIZE,
                color,
                chevron.center(),
            );
        }
        let icon = match entry.kind {
            EntryKind::Folder if row.expanded => FOLDER_OPEN,
            EntryKind::Folder => FOLDER,
            EntryKind::Prose => DOCUMENT,
            EntryKind::Code => CODE,
            EntryKind::Image => IMAGE,
            EntryKind::Other => PAGE,
        };
        canvas.draw_icon(
            icon,
            IconFont::SegoeFluent,
            ICON,
            color,
            Point::new(self.icon_x(index), rect.center().y),
        );
        let name = entry.display_name(self.options.show_extensions);
        let label = layout_line(cx.text, cx.theme, name, FONT_SIZE);
        let label_x = self.label_x(index);
        let clip = Rect::new(
            label_x,
            rect.y,
            (rect.right() - BAR_WIDTH - label_x - ICON - GAP).max(0.0),
            rect.height,
        );
        canvas.push_clip_rect(clip);
        if self.options.show_modified && !entry.modified.is_empty() {
            let top = Rect::new(rect.x, rect.y + 2.0, rect.width, rect.height / 2.0);
            let bottom = Rect::new(
                rect.x,
                rect.y + rect.height / 2.0,
                rect.width,
                rect.height / 2.0 - 2.0,
            );
            draw_left_centered(canvas, label.as_ref(), label_x, top, color);
            let date = layout_line(cx.text, cx.theme, &entry.modified, 12.0);
            draw_left_centered(canvas, date.as_ref(), label_x, bottom, tokens.text_muted);
        } else {
            draw_left_centered(canvas, label.as_ref(), label_x, rect, color);
        }
        canvas.pop_clip();
        if entry.placeholder {
            canvas.draw_icon(
                CLOUD,
                IconFont::SegoeFluent,
                CHEVRON_SIZE,
                tokens.text_muted,
                Point::new(rect.right() - BAR_WIDTH - ICON, rect.center().y),
            );
        }
        if let Some(target) = self.drag.and_then(|drag| drag.target)
            && target == row.key
        {
            canvas.stroke_rounded_rect(pill, radius, tokens.accent, 2.0);
        }
        if cx.for_widget(self.tree_id).is_focused() && selected && self.rename.is_none() {
            focus_ring::paint(canvas, pill, tokens.metrics.control_radius, cx.theme);
        }
    }
}

impl Control for SidebarView {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for SidebarView {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        let width = if available.width.is_finite() {
            available.width
        } else {
            260.0
        };
        let height = if available.height.is_finite() {
            available.height
        } else {
            HEADER_HEIGHT + self.rows.len() as f32 * self.row_height()
        };
        Size::new(width, height)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        let header = self.header_rect();
        let top = header.y + (header.height - HEADER_BUTTON) / 2.0;
        let new_rect = Rect::new(
            header.right() - 8.0 - HEADER_BUTTON,
            top,
            HEADER_BUTTON,
            HEADER_BUTTON,
        );
        let more_rect = Rect::new(
            new_rect.x - 4.0 - HEADER_BUTTON,
            top,
            HEADER_BUTTON,
            HEADER_BUTTON,
        );
        let more_id = self.more.id();
        self.more.arrange(&mut cx.for_widget(more_id), more_rect);
        let new_id = self.new.id();
        self.new.arrange(&mut cx.for_widget(new_id), new_rect);
        let list = self.list_rect();
        self.sync_scroll();
        let bar_id = self.bar.id();
        self.bar.arrange(
            &mut cx.for_widget(bar_id),
            Rect::new(list.right() - BAR_WIDTH, list.y, BAR_WIDTH, list.height),
        );
        cx.for_widget(self.tree_id).add_tab_stop();
        if let Some(rect) = self.rename_rect()
            && let Some(rename) = self.rename.as_mut()
        {
            let id = rename.text_box.id();
            rename.text_box.arrange(&mut cx.for_widget(id), rect);
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        let header = self.header_rect();
        let style = ui_style(cx.theme, FONT_SIZE).with_weight(FontWeight::SEMIBOLD);
        let name = cx
            .text
            .layout(&self.root_name, &style, &[], &[], f32::INFINITY);
        canvas.push_clip_rect(Rect::new(
            header.x,
            header.y,
            (self.more.bounds().x - header.x).max(0.0),
            header.height,
        ));
        draw_left_centered(canvas, name.as_ref(), header.x + 12.0, header, tokens.text);
        canvas.pop_clip();
        self.more.paint(&cx.for_widget(self.more.id()), canvas);
        self.new.paint(&cx.for_widget(self.new.id()), canvas);

        let list = self.list_rect();
        canvas.push_clip_rect(list);
        if self.rows.is_empty() {
            let text = layout_line(cx.text, cx.theme, &self.strings.empty, FONT_SIZE);
            let row = Rect::new(list.x, list.y, list.width, ROW_HEIGHT);
            draw_left_centered(canvas, text.as_ref(), list.x + 12.0, row, tokens.text_muted);
        }
        for index in self.visible_range() {
            self.paint_row(cx, canvas, index);
        }
        if let Some(drag) = self.drag
            && drag.target == Some(self.root_key)
        {
            canvas.stroke_rounded_rect(
                list.inset(Insets::uniform(1.0)),
                CornerRadii::uniform(tokens.metrics.control_radius),
                tokens.accent,
                2.0,
            );
        }
        canvas.pop_clip();
        if let Some(rename) = &self.rename {
            rename
                .text_box
                .paint(&cx.for_widget(rename.text_box.id()), canvas);
        }
        self.bar.paint(&cx.for_widget(self.bar.id()), canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if let Some(rename) = self.rename.as_mut() {
            let box_id = rename.text_box.id();
            let ends = match e {
                Event::Key(key) if key.is_down && key.vk == vk::ESCAPE => Some(false),
                Event::Pointer(p)
                    if p.kind == PointerEventKind::Down
                        && !rename.text_box.bounds().contains(p.position) =>
                {
                    Some(true)
                }
                _ => None,
            };
            if let Some(commit) = ends {
                self.end_rename(cx, commit);
                if matches!(e, Event::Key(_)) {
                    return EventResult::handled();
                }
            } else if !matches!(e, Event::Wheel(_) | Event::AnimationFrame(_)) {
                let result = rename.text_box.event(&mut cx.for_widget(box_id), e);
                if rename.text_box.take_submitted() {
                    self.end_rename(cx, true);
                }
                return result.or(EventResult::handled());
            }
        }
        match e {
            Event::Pointer(event) => self.pointer(cx, event),
            Event::Wheel(event) if self.list_rect().contains(event.position) => {
                if self.bar.wheel(cx, event) {
                    self.bar.take_changed();
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            Event::Key(key) if key.is_down && cx.focus.is_focused(self.tree_id) => {
                if self.key(key, cx) {
                    if self.press.is_none() && cx.has_pointer_capture() {
                        cx.release_pointer();
                    }
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    // Header buttons take Space/Enter through their own focus.
                    self.forward_to_buttons(cx, e)
                }
            }
            Event::Key(_) | Event::Focus(FocusEvent::Gained | FocusEvent::Lost) => {
                let result = self.forward_to_buttons(cx, e);
                cx.invalidate(self.bounds);
                result
            }
            Event::AnimationFrame(now) => {
                if self.bar.animate(cx, *now) {
                    cx.invalidate(self.bounds);
                }
                self.forward_to_buttons(cx, e)
            }
            _ => EventResult::ignored(),
        }
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let range = self.visible_range();
        let mut nodes: Vec<AccessNode> = Vec::with_capacity(range.len());
        // Nodes nest below the nearest realized ancestor, the tree itself otherwise.
        let mut stack: Vec<(usize, usize)> = Vec::new();
        let mut top_level = Vec::new();
        for index in range {
            let depth = self.rows[index].depth;
            while stack.last().is_some_and(|&(stacked, _)| stacked >= depth) {
                stack.pop();
            }
            let slot = nodes.len();
            nodes.push(self.row_node(cx, index));
            match stack.last() {
                Some(&(_, parent)) => {
                    let id = nodes[slot].id;
                    nodes[parent].children.push(id);
                }
                None => top_level.push(nodes[slot].id),
            }
            stack.push((depth, slot));
        }
        for node in nodes {
            out.insert(node);
        }
        let tree_cx = cx.for_widget(self.tree_id);
        let mut tree = AccessNode::new(
            self.tree_id,
            Role::Tree,
            self.strings.tree.clone(),
            self.list_rect(),
        )
        .with_children(top_level);
        tree.state.focusable = tree_cx.is_focusable();
        tree.state.focused = tree_cx.is_focused() && self.selected.is_none();
        tree.patterns.scroll = Some(scroll_info(&Default::default(), self.bar.model()));
        out.insert(tree);

        self.more.access(&cx.for_widget(self.more.id()), out);
        self.new.access(&cx.for_widget(self.new.id()), out);
        out.insert(
            AccessNode::new(
                self.header_id,
                Role::ToolBar,
                self.root_name.clone(),
                self.header_rect(),
            )
            .with_children([self.more.id(), self.new.id()]),
        );
        let mut children = vec![self.header_id, self.tree_id];
        if let Some(rename) = &self.rename {
            rename
                .text_box
                .access(&cx.for_widget(rename.text_box.id()), out);
            children.push(rename.text_box.id());
        }
        out.insert(
            AccessNode::new(self.id, Role::Pane, self.strings.label.clone(), self.bounds)
                .with_children(children),
        );
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}

impl SidebarView {
    /// The header button under `point` and its name, the text of its tooltip.
    #[must_use]
    pub fn tooltip_at(&self, point: Point) -> Option<(Rect, &str)> {
        [&self.more, &self.new]
            .into_iter()
            .find(|button| button.bounds().contains(point))
            .map(|button| (button.bounds(), button.label()))
    }

    fn forward_to_buttons(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let more_id = self.more.id();
        let new_id = self.new.id();
        let more = self.more.event(&mut cx.for_widget(more_id), e);
        let new = self.new.event(&mut cx.for_widget(new_id), e);
        for (menu, button) in [
            (HeaderMenu::Options, &mut self.more),
            (HeaderMenu::New, &mut self.new),
        ] {
            if button.take_activated() {
                let bounds = button.bounds();
                self.events.push(SidebarEvent::HeaderMenu {
                    menu,
                    anchor: Point::new(bounds.x, bounds.bottom()),
                });
            }
        }
        more.or(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::input::{Modifiers, PointerType, TextInput, WheelEvent};
    use crate::services::MemoryClipboard;
    use crate::widgets::testkit::Harness;
    use std::cell::RefCell;
    use std::rc::Rc;

    const MAIN_FTL: &str = include_str!("../../../../assets/i18n/en/main.ftl");
    const SHELL_FTL: &str = include_str!("../../../../assets/i18n/en/shell.ftl");

    type Calls = Rc<RefCell<Vec<NodeKey>>>;
    type Listings = Rc<RefCell<Vec<(NodeKey, Vec<SidebarEntry>)>>>;

    /// An in-memory workspace that sorts and filters like the real source and records which
    /// folders were listed.
    struct MemoryTree {
        listings: Listings,
        calls: Calls,
    }

    impl TreeSource for MemoryTree {
        fn root_name(&self) -> String {
            "Notes".to_owned()
        }

        fn root_key(&self) -> NodeKey {
            0
        }

        fn children(&mut self, folder: NodeKey, options: &SidebarOptions) -> Vec<SidebarEntry> {
            self.calls.borrow_mut().push(folder);
            let mut entries: Vec<SidebarEntry> = self
                .listings
                .borrow()
                .iter()
                .find(|(key, _)| *key == folder)
                .map(|(_, entries)| entries.clone())
                .unwrap_or_default();
            entries.retain(|e| {
                !(options.hide_images && e.kind == EntryKind::Image)
                    && !(options.hide_non_markdown && !e.is_folder() && !e.name.ends_with(".md"))
            });
            entries.sort_by_key(|e| {
                let group = u8::from(!(options.folders_first && e.is_folder()));
                (group, e.name.to_lowercase())
            });
            if options.descending {
                entries.reverse();
            }
            entries
        }
    }

    fn entries() -> Vec<(NodeKey, Vec<SidebarEntry>)> {
        let mut cloud = SidebarEntry::file(9, "cloud.md", EntryKind::Prose);
        cloud.placeholder = true;
        let mut data = SidebarEntry::file(10, "data.bin", EntryKind::Other);
        data.hidden = true;
        let mut readme = SidebarEntry::file(7, "readme.md", EntryKind::Prose);
        readme.modified = "1 Oct 2026".to_owned();
        vec![
            (
                0,
                vec![
                    SidebarEntry::folder(1, "docs"),
                    SidebarEntry::folder(2, "empty"),
                    readme,
                    SidebarEntry::file(8, "photo.png", EntryKind::Image),
                    cloud,
                    data,
                ],
            ),
            (
                1,
                vec![
                    SidebarEntry::file(3, "a.md", EntryKind::Prose),
                    SidebarEntry::file(4, "b.txt", EntryKind::Prose),
                    SidebarEntry::folder(5, "deep"),
                ],
            ),
            (5, vec![SidebarEntry::file(6, "c.md", EntryKind::Prose)]),
        ]
    }

    fn strings() -> SidebarStrings {
        let localizer = Localizer::new("en", &[("en", MAIN_FTL), ("en", SHELL_FTL)]);
        let diagnostics = localizer.diagnostics();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        SidebarStrings::from_localizer(&localizer)
    }

    struct Fixture {
        harness: Harness,
        view: SidebarView,
        calls: Calls,
        listings: Listings,
    }

    /// A 260 x 400 sidebar at y = 80: header 80..120, rows of 32 DIPs from y = 120.
    fn fixture(options: SidebarOptions) -> Fixture {
        let listings: Listings = Rc::new(RefCell::new(entries()));
        let calls: Calls = Rc::new(RefCell::new(Vec::new()));
        let source = MemoryTree {
            listings: listings.clone(),
            calls: calls.clone(),
        };
        let view = SidebarView::new(
            strings(),
            Box::new(source),
            Rc::new(RefCell::new(MemoryClipboard::default())),
            options,
        )
        .with_motion(Motion::reduced());
        let mut fixture = Fixture {
            harness: Harness::new(),
            view,
            calls,
            listings,
        };
        fixture.layout();
        fixture
    }

    impl Fixture {
        fn layout(&mut self) {
            self.harness
                .arrange(&mut self.view, Rect::new(0.0, 80.0, 260.0, 400.0));
        }

        fn names(&self) -> Vec<String> {
            self.view.row_names()
        }

        fn row_center(&self, index: usize) -> Point {
            self.view.row_rect(index).center()
        }

        fn click_row(&mut self, index: usize) {
            let at = self.row_center(index);
            self.harness.click(&mut self.view, at);
        }

        fn focus_tree(&mut self) {
            let id = self.view.tree_id;
            self.harness.focus.focus(id);
        }

        fn key(&mut self, key: u16) -> bool {
            self.harness.key(&mut self.view, key).handled
        }
    }

    #[test]
    fn strings_resolve_from_the_ftl_ids() {
        let strings = strings();
        assert_eq!(strings.label, "Sidebar");
        for text in [
            &strings.tree,
            &strings.more,
            &strings.new,
            &strings.rename,
            &strings.cloud,
            &strings.empty,
        ] {
            assert!(!text.is_empty() && !text.starts_with("sidebar-"), "{text}");
        }
    }

    #[test]
    fn the_root_is_listed_once_and_folders_expand_lazily() {
        let mut f = fixture(SidebarOptions::default());
        assert_eq!(f.view.root_name(), "Notes");
        assert_eq!(
            f.names(),
            [
                "docs",
                "empty",
                "cloud.md",
                "data.bin",
                "photo.png",
                "readme.md"
            ]
            .map(String::from)
        );
        assert_eq!(*f.calls.borrow(), vec![0], "only the root so far");
        assert_eq!(f.view.is_expanded(1), Some(false));
        assert_eq!(f.view.is_expanded(7), None);

        // The chevron of "docs" (depth 0: x 4..20 of the row).
        let row = f.view.row_rect(0);
        f.harness
            .click(&mut f.view, Point::new(row.x + 12.0, row.center().y));
        assert_eq!(f.view.take_events(), vec![SidebarEvent::Expanded(1)]);
        assert_eq!(*f.calls.borrow(), vec![0, 1]);
        assert_eq!(
            f.names()[..5],
            ["docs", "deep", "a.md", "b.txt", "empty"].map(String::from)
        );
        assert_eq!(f.view.is_expanded(1), Some(true));

        f.harness
            .click(&mut f.view, Point::new(row.x + 12.0, row.center().y));
        assert_eq!(f.view.take_events(), vec![SidebarEvent::Collapsed(1)]);
        f.harness
            .click(&mut f.view, Point::new(row.x + 12.0, row.center().y));
        assert_eq!(f.view.take_events(), vec![SidebarEvent::Expanded(1)]);
        assert_eq!(
            *f.calls.borrow(),
            vec![0, 1],
            "a listed folder is not listed again"
        );
        assert_eq!(f.view.row_count(), 9);
        assert_eq!(f.view.row_key(1), Some(5));
        assert_eq!(f.view.row_key(99), None);
        assert_eq!(f.view.entry(3).map(|e| e.name.as_str()), Some("a.md"));
        assert!(f.view.entry(99).is_none());
    }

    #[test]
    fn options_change_names_row_height_and_listing_but_keep_expansion() {
        let mut f = fixture(SidebarOptions::default());
        f.view.user_expand(1, true);
        f.view.take_events();
        f.view.set_options(SidebarOptions {
            show_extensions: false,
            hide_images: true,
            ..SidebarOptions::default()
        });
        assert!(!f.view.options().show_extensions);
        assert_eq!(
            f.names(),
            ["docs", "deep", "a", "b", "empty", "cloud", "data", "readme"].map(String::from)
        );
        assert_eq!(f.view.is_expanded(1), Some(true), "expansion survives");
        assert!(
            f.calls.borrow().len() > 2,
            "the visible folders were listed again"
        );

        f.view.set_options(SidebarOptions {
            hide_non_markdown: true,
            descending: true,
            folders_first: false,
            ..SidebarOptions::default()
        });
        assert_eq!(
            f.names(),
            ["readme.md", "empty", "docs", "deep", "a.md", "cloud.md"].map(String::from)
        );
        // Extensionless and dotfiles keep their names.
        assert_eq!(
            SidebarEntry::file(1, "Makefile", EntryKind::Prose).display_name(false),
            "Makefile"
        );
        assert_eq!(
            SidebarEntry::file(1, ".gitignore", EntryKind::Prose).display_name(false),
            ".gitignore"
        );
        assert_eq!(SidebarEntry::folder(1, "v1.2").display_name(false), "v1.2");

        f.view.user_expand(1, false);
        f.view.set_options(SidebarOptions {
            show_modified: true,
            ..SidebarOptions::default()
        });
        f.layout();
        assert_eq!(f.view.row_rect(1).height, 48.0);
        let canvas = f.harness.paint(&f.view);
        assert!(canvas.texts().contains(&"1 Oct 2026"));
        // Without dates the row is back to 32 DIPs.
        f.view.set_options(SidebarOptions::default());
        assert_eq!(f.view.row_rect(1).height, 32.0);
    }

    #[test]
    fn pictures_and_foreign_files_are_dimmed_and_placeholders_get_a_cloud() {
        let mut f = fixture(SidebarOptions::default());
        let canvas = f.harness.paint(&f.view);
        let tokens = f.harness.theme.tokens;
        let color_of = |name: &str| {
            canvas.ops().find_map(|op| match op {
                DrawOp::Text { text, color, .. } if text == name => Some(*color),
                _ => None,
            })
        };
        assert_eq!(color_of("docs"), Some(tokens.text));
        assert_eq!(color_of("readme.md"), Some(tokens.text));
        assert_eq!(color_of("photo.png"), Some(tokens.text_muted));
        assert_eq!(
            color_of("data.bin"),
            Some(tokens.text_disabled),
            "hidden items are dimmer"
        );
        let glyphs: Vec<char> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::Icon { glyph, .. } => Some(*glyph),
                _ => None,
            })
            .collect();
        assert_eq!(glyphs.iter().filter(|g| **g == CLOUD).count(), 1);
        assert!(glyphs.contains(&FOLDER) && glyphs.contains(&DOCUMENT) && glyphs.contains(&IMAGE));
        assert!(
            glyphs.contains(&PAGE),
            "data.bin is a type that opens elsewhere"
        );
        assert!(glyphs.contains(&MORE) && glyphs.contains(&glyph::ADD));
        assert!(canvas.texts().contains(&"Notes"));

        f.view.user_expand(1, true);
        let canvas = f.harness.paint(&f.view);
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::Icon { glyph, .. } if *glyph == FOLDER_OPEN))
        );
        // The placeholder is described to assistive technology.
        let tree = f.harness.tree(&f.view);
        let cloud = tree
            .nodes()
            .iter()
            .find(|n| n.name == "cloud.md")
            .expect("cloud node");
        assert_eq!(cloud.description, "Available online only");
    }

    #[test]
    fn each_class_has_its_icon_and_only_files_that_open_elsewhere_are_dimmed() {
        for (kind, dimmed) in [
            (EntryKind::Folder, false),
            (EntryKind::Prose, false),
            (EntryKind::Code, false),
            (EntryKind::Image, true),
            (EntryKind::Other, true),
        ] {
            assert_eq!(
                SidebarEntry::file(1, "x", kind).is_dimmed(),
                dimmed,
                "{kind:?}"
            );
        }

        let mut f = fixture(SidebarOptions::default());
        let mut hidden = SidebarEntry::file(26, "hidden.json", EntryKind::Code);
        hidden.hidden = true;
        f.listings.borrow_mut()[0].1 = vec![
            SidebarEntry::folder(1, "docs"),
            SidebarEntry::file(20, "a.md", EntryKind::Prose),
            SidebarEntry::file(21, "b.txt", EntryKind::Prose),
            SidebarEntry::file(22, "c.py", EntryKind::Code),
            SidebarEntry::file(23, "d.docx", EntryKind::Prose),
            SidebarEntry::file(24, "e.png", EntryKind::Image),
            SidebarEntry::file(25, "f.zip", EntryKind::Other),
            hidden,
        ];
        f.view.refresh_all();
        f.layout();
        let canvas = f.harness.paint(&f.view);
        let tokens = f.harness.theme.tokens;
        // The icon is tinted with the colour of the name: full strength, dimmed or dimmest.
        let expected = [
            ("docs", FOLDER, tokens.text),
            ("a.md", DOCUMENT, tokens.text),
            ("b.txt", DOCUMENT, tokens.text),
            ("c.py", CODE, tokens.text),
            ("d.docx", DOCUMENT, tokens.text),
            ("e.png", IMAGE, tokens.text_muted),
            ("f.zip", PAGE, tokens.text_muted),
            ("hidden.json", CODE, tokens.text_disabled),
        ];
        let names = f.names();
        for (index, (name, glyph, color)) in expected.into_iter().enumerate() {
            assert_eq!(names[index], name);
            let middle = f.view.row_rect(index).center().y;
            let icon = canvas.ops().find_map(|op| match op {
                DrawOp::Icon {
                    glyph,
                    color,
                    size,
                    center,
                    ..
                } if *size == ICON && center.y == middle => Some((*glyph, *color)),
                _ => None,
            });
            assert_eq!(icon, Some((glyph, color)), "icon of {name}");
            let label = canvas.ops().find_map(|op| match op {
                DrawOp::Text { text, color, .. } if text == name => Some(*color),
                _ => None,
            });
            assert_eq!(label, Some(color), "name {name}");
        }
    }

    #[test]
    fn the_keyboard_selects_expands_opens_and_requests_actions() {
        let mut f = fixture(SidebarOptions::default());
        assert!(!f.key(vk::DOWN), "not focused yet");
        f.focus_tree();
        assert!(f.key(vk::DOWN));
        assert_eq!(f.view.selected_key(), Some(1));
        f.key(vk::END);
        assert_eq!(f.view.selected_key(), Some(7));
        f.key(vk::UP);
        f.key(vk::HOME);
        f.key(vk::PAGE_DOWN);
        f.key(vk::PAGE_UP);
        assert_eq!(f.view.selected_key(), Some(1));
        // Right expands, Right again enters, Left on a leaf goes to the parent.
        f.key(vk::RIGHT);
        f.key(vk::RIGHT);
        assert_eq!(f.view.selected_key(), Some(5), "folders sort first: deep");
        f.key(vk::LEFT);
        assert_eq!(f.view.selected_key(), Some(1));
        f.key(vk::LEFT);
        assert_eq!(f.view.is_expanded(1), Some(false));
        f.key(vk::LEFT);
        assert_eq!(
            f.view.selected_key(),
            Some(1),
            "a top-level folder has no parent"
        );
        f.view.take_events();
        // Enter opens a file (Ctrl+Enter in a new tab) and toggles a folder.
        f.key(vk::END);
        f.key(vk::RETURN);
        f.harness.key_with(&mut f.view, vk::RETURN, Modifiers::CTRL);
        f.key(vk::HOME);
        f.key(vk::RETURN);
        f.key(vk::DELETE);
        f.key(vk::APPS);
        let events = f.view.take_events();
        assert!(events.contains(&SidebarEvent::Selected(7)));
        assert!(events.contains(&SidebarEvent::Open {
            key: 7,
            new_tab: false
        }));
        assert!(events.contains(&SidebarEvent::Open {
            key: 7,
            new_tab: true
        }));
        assert!(events.contains(&SidebarEvent::Expanded(1)));
        assert!(events.contains(&SidebarEvent::DeleteRequested(1)));
        let context = events.iter().find_map(|e| match e {
            SidebarEvent::ContextMenu { key, at } => Some((*key, *at)),
            _ => None,
        });
        assert_eq!(context, Some((Some(1), Point::new(130.0, 152.0))));
        assert!(!f.key(vk::TAB));
        assert!(
            !f.harness
                .key_with(&mut f.view, vk::DOWN, Modifiers::CTRL)
                .handled
        );
    }

    #[test]
    fn the_context_key_with_no_selection_and_empty_trees() {
        let mut f = fixture(SidebarOptions::default());
        f.focus_tree();
        assert!(
            f.harness
                .key_with(&mut f.view, vk::F10, Modifiers::SHIFT)
                .handled
        );
        assert_eq!(
            f.view.take_events(),
            vec![SidebarEvent::ContextMenu {
                key: None,
                at: Point::new(0.0, 120.0)
            }]
        );
        // Navigation keys do nothing before a selection exists except moving to the first row.
        assert!(f.key(vk::DOWN));
        assert!(f.key(vk::LEFT));
        f.view.select_key(99);
        assert_eq!(f.view.selected_key(), None);
        assert!(f.key(vk::RIGHT));
        assert!(f.key(vk::LEFT));
        assert!(f.key(vk::F2));
        assert!(f.key(vk::DELETE));
        assert!(f.key(vk::RETURN));

        f.listings.borrow_mut().clear();
        f.view.refresh_all();
        f.layout();
        assert_eq!(f.view.row_count(), 0);
        assert!(!f.key(vk::DOWN));
        let canvas = f.harness.paint(&f.view);
        assert!(canvas.texts().contains(&"This folder is empty"));
        // A click on the empty list asks for focus but selects nothing.
        let result = f.harness.click(&mut f.view, Point::new(100.0, 200.0));
        assert!(result.handled && result.request_focus);
        let result = f.harness.click(&mut f.view, Point::new(100.0, 20.0));
        assert!(!result.handled);
    }

    #[test]
    fn mouse_selection_double_click_context_menu_and_header_buttons() {
        let mut f = fixture(SidebarOptions::default());
        f.click_row(4);
        assert_eq!(f.view.take_events(), vec![SidebarEvent::Selected(8)]);
        f.click_row(4);
        assert!(
            f.view.take_events().is_empty(),
            "same row: no new selection event"
        );
        // Double click: a file opens, with Ctrl in a new tab.
        let at = f.row_center(5);
        for (click_count, mods) in [(1, Modifiers::NONE), (2, Modifiers::CTRL)] {
            f.harness.send(
                &mut f.view,
                Event::Pointer(PointerEvent {
                    kind: PointerEventKind::Down,
                    button: Some(PointerButton::Left),
                    position: at,
                    pointer_type: PointerType::Mouse,
                    click_count,
                    mods,
                }),
            );
            f.harness.pointer(
                &mut f.view,
                PointerEventKind::Up,
                Some(PointerButton::Left),
                at,
            );
        }
        assert_eq!(
            f.view.take_events(),
            vec![
                SidebarEvent::Selected(7),
                SidebarEvent::Open {
                    key: 7,
                    new_tab: true
                }
            ]
        );
        // Double click on a folder toggles it.
        let docs = f.row_center(0);
        f.harness.send(
            &mut f.view,
            Event::Pointer(PointerEvent {
                kind: PointerEventKind::Down,
                button: Some(PointerButton::Left),
                position: Point::new(docs.x + 40.0, docs.y),
                pointer_type: PointerType::Mouse,
                click_count: 2,
                mods: Modifiers::NONE,
            }),
        );
        assert!(f.view.take_events().contains(&SidebarEvent::Expanded(1)));
        f.harness.pointer(
            &mut f.view,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            docs,
        );
        // Right click selects and asks for the context menu; over empty space it targets the root.
        let at = f.row_center(1);
        f.harness.pointer(
            &mut f.view,
            PointerEventKind::Down,
            Some(PointerButton::Right),
            at,
        );
        let empty = Point::new(100.0, 460.0);
        f.harness.pointer(
            &mut f.view,
            PointerEventKind::Down,
            Some(PointerButton::Right),
            empty,
        );
        let outside = Point::new(100.0, 20.0);
        let ignored = f.harness.pointer(
            &mut f.view,
            PointerEventKind::Down,
            Some(PointerButton::Right),
            outside,
        );
        assert!(!ignored.handled);
        let events = f.view.take_events();
        assert!(events.contains(&SidebarEvent::ContextMenu { key: Some(5), at }));
        assert!(events.contains(&SidebarEvent::ContextMenu {
            key: None,
            at: empty
        }));

        // The header buttons request the native menus below themselves.
        let more = f.view.more.bounds();
        let new = f.view.new.bounds();
        f.harness.click(&mut f.view, more.center());
        f.harness.click(&mut f.view, new.center());
        assert_eq!(
            f.view.take_events(),
            vec![
                SidebarEvent::HeaderMenu {
                    menu: HeaderMenu::Options,
                    anchor: Point::new(more.x, more.bottom())
                },
                SidebarEvent::HeaderMenu {
                    menu: HeaderMenu::New,
                    anchor: Point::new(new.x, new.bottom())
                },
            ]
        );
        // Header buttons are keyboard reachable.
        let id = f.view.new.id();
        f.harness.focus.focus(id);
        assert!(f.key(vk::SPACE));
        assert!(matches!(
            f.view.take_events().as_slice(),
            [SidebarEvent::HeaderMenu {
                menu: HeaderMenu::New,
                ..
            }]
        ));
        f.harness.pointer(
            &mut f.view,
            PointerEventKind::Move,
            None,
            Point::new(100.0, 200.0),
        );
        f.harness.pointer(
            &mut f.view,
            PointerEventKind::Enter,
            None,
            Point::new(100.0, 232.0),
        );
        f.harness
            .pointer(&mut f.view, PointerEventKind::Leave, None, Point::ZERO);
        f.harness
            .send(&mut f.view, Event::Focus(FocusEvent::Gained));
        f.harness.frame(&mut f.view, 16);
        assert!(f.harness.paint(&f.view).is_balanced());
    }

    #[test]
    fn rows_and_header_buttons_ask_for_the_hand_and_inert_areas_for_nothing() {
        let mut f = fixture(SidebarOptions::default());
        let cursor_at = |f: &mut Fixture, at: Point| {
            f.harness.requests.take_cursor();
            f.harness
                .pointer(&mut f.view, PointerEventKind::Move, None, at);
            f.harness.requests.take_cursor()
        };
        let row = f.row_center(1);
        assert_eq!(cursor_at(&mut f, row), Some(Cursor::Hand));
        let more = f.view.more.bounds().center();
        assert_eq!(cursor_at(&mut f, more), Some(Cursor::Hand));
        assert_eq!(cursor_at(&mut f, Point::new(20.0, 100.0)), None);
        f.harness.pointer(
            &mut f.view,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            row,
        );
        assert_eq!(
            cursor_at(&mut f, Point::new(20.0, 100.0)),
            Some(Cursor::Hand)
        );
    }

    #[test]
    fn moving_over_the_empty_list_area_without_a_press_asks_for_no_cursor() {
        let mut f = fixture(SidebarOptions::default());
        // Below the last row and far from the scroll bar: inside the list, but on no row.
        let empty = Point::new(20.0, 450.0);
        assert!(f.view.list_rect().contains(empty));
        assert_eq!(f.view.row_at(empty), None);
        f.harness.requests.take_cursor();
        f.harness
            .pointer(&mut f.view, PointerEventKind::Move, None, empty);
        assert_eq!(f.harness.requests.take_cursor(), None);
    }

    #[test]
    fn the_tooltip_names_the_header_button_under_the_pointer() {
        let f = fixture(SidebarOptions::default());
        let strings = strings();
        let more = f.view.more.bounds();
        let new = f.view.new.bounds();
        assert_eq!(
            f.view.tooltip_at(more.center()),
            Some((more, strings.more.as_str()))
        );
        assert_eq!(
            f.view.tooltip_at(new.center()),
            Some((new, strings.new.as_str()))
        );
        // The root name and the rows carry no tooltip.
        assert_eq!(f.view.tooltip_at(Point::new(20.0, 100.0)), None);
        assert_eq!(f.view.tooltip_at(f.row_center(0)), None);
    }

    #[test]
    fn inline_rename_commits_cancels_and_validates() {
        let mut f = fixture(SidebarOptions::default());
        f.click_row(5);
        f.view.take_events();
        f.focus_tree();
        assert!(f.key(vk::F2));
        assert_eq!(f.view.renaming(), Some(7));
        f.layout();
        // The stem is selected: typing replaces it and keeps the extension.
        f.harness
            .send(&mut f.view, Event::Text(TextInput("notes".to_owned())));
        assert!(f.key(vk::RETURN));
        assert_eq!(
            f.view.take_events(),
            vec![SidebarEvent::RenameCommitted {
                key: 7,
                name: "notes.md".to_owned()
            }]
        );
        assert_eq!(f.view.renaming(), None);
        assert!(
            f.harness.focus.is_focused(f.view.tree_id),
            "focus returns to the tree"
        );

        // Escape cancels, an unchanged or empty name cancels too.
        f.focus_tree();
        f.key(vk::F2);
        assert!(f.key(vk::ESCAPE));
        f.key(vk::F2);
        f.key(vk::RETURN);
        f.key(vk::F2);
        f.harness
            .key_with(&mut f.view, b'A' as u16, Modifiers::CTRL);
        f.key(vk::BACK);
        f.key(vk::RETURN);
        assert_eq!(
            f.view.take_events(),
            vec![
                SidebarEvent::RenameCancelled(7),
                SidebarEvent::RenameCancelled(7),
                SidebarEvent::RenameCancelled(7),
            ]
        );

        // Clicking elsewhere commits.
        f.focus_tree();
        f.key(vk::F2);
        f.layout();
        f.harness
            .send(&mut f.view, Event::Text(TextInput("x".to_owned())));
        f.click_row(0);
        let events = f.view.take_events();
        assert!(events.contains(&SidebarEvent::RenameCommitted {
            key: 7,
            name: "x.md".to_owned()
        }));
        assert!(events.contains(&SidebarEvent::Selected(1)));

        // Folders select their whole name; hidden rows cannot be renamed.
        f.view.select_key(1);
        let mut cx = f.harness.event_cx(f.view.id());
        assert!(f.view.begin_rename(&mut cx, 1));
        assert_eq!(
            f.view.rename.as_ref().map(|r| r.text_box.selected_text()),
            Some("docs")
        );
        let mut cx = f.harness.event_cx(f.view.id());
        assert!(
            !f.view.begin_rename(&mut cx, 6),
            "c.md is inside a collapsed folder"
        );
        assert!(!f.view.begin_rename(&mut cx, 99));
        f.layout();
        let tree = f.harness.tree(&f.view);
        assert!(
            tree.nodes()
                .iter()
                .any(|n| n.name == "Rename" && n.role == Role::Edit)
        );
        assert!(f.harness.paint(&f.view).is_balanced());
    }

    #[test]
    fn dragging_moves_copies_releases_and_cancels() {
        let mut f = fixture(SidebarOptions::default());
        f.view.user_expand(1, true);
        f.view.take_events();
        // Rows: 0 docs, 1 deep, 2 a.md, 3 b.txt, 4 empty, 5 cloud.md, 6 data.bin, 7 photo.png,
        // 8 readme.md.
        let readme = f.row_center(8);
        let press = |f: &mut Fixture, at: Point| {
            f.harness.pointer(
                &mut f.view,
                PointerEventKind::Down,
                Some(PointerButton::Left),
                at,
            );
        };
        let release = |f: &mut Fixture, at: Point, mods: Modifiers| {
            f.harness.send(
                &mut f.view,
                Event::Pointer(PointerEvent {
                    kind: PointerEventKind::Up,
                    button: Some(PointerButton::Left),
                    position: at,
                    pointer_type: PointerType::Mouse,
                    click_count: 0,
                    mods,
                }),
            );
        };
        // A small movement is no drag.
        press(&mut f, readme);
        f.harness.pointer(
            &mut f.view,
            PointerEventKind::Move,
            None,
            Point::new(readme.x + 2.0, readme.y),
        );
        assert_eq!(f.view.take_events(), vec![SidebarEvent::Selected(7)]);
        // Past the threshold the drag starts; over the "empty" folder it is a valid target.
        let empty = f.row_center(4);
        f.harness
            .pointer(&mut f.view, PointerEventKind::Move, None, empty);
        assert_eq!(f.view.take_events(), vec![SidebarEvent::DragStarted(7)]);
        assert_eq!(f.view.drag.map(|d| d.source), Some(7));
        f.harness
            .pointer(&mut f.view, PointerEventKind::Move, None, empty);
        let canvas = f.harness.paint(&f.view);
        let accent = f.harness.theme.tokens.accent;
        assert!(
            canvas.ops().any(
                |op| matches!(op, DrawOp::StrokeRoundedRect { color, .. } if *color == accent)
            )
        );
        release(&mut f, empty, Modifiers::NONE);
        assert_eq!(
            f.view.take_events(),
            vec![SidebarEvent::Dropped {
                source: 7,
                target_folder: 2,
                copy: false
            }]
        );
        // With Ctrl it copies; onto a file it targets the file's folder (docs).
        press(&mut f, readme);
        f.harness.pointer(
            &mut f.view,
            PointerEventKind::Move,
            None,
            Point::new(readme.x + 30.0, readme.y),
        );
        let a_md = f.row_center(2);
        f.harness
            .pointer(&mut f.view, PointerEventKind::Move, None, a_md);
        release(&mut f, a_md, Modifiers::CTRL);
        let events = f.view.take_events();
        assert!(events.contains(&SidebarEvent::Dropped {
            source: 7,
            target_folder: 1,
            copy: true
        }));
        // Dragging a folder into its own subtree or onto the current parent is no target.
        let docs = f.row_center(0);
        press(&mut f, docs);
        let deep = f.row_center(1);
        f.harness.pointer(
            &mut f.view,
            PointerEventKind::Move,
            None,
            Point::new(docs.x + 30.0, docs.y),
        );
        f.harness
            .pointer(&mut f.view, PointerEventKind::Move, None, deep);
        release(&mut f, deep, Modifiers::NONE);
        let events = f.view.take_events();
        assert!(
            events.contains(&SidebarEvent::DragCancelled(1)),
            "{events:?}"
        );
        // Released outside the sidebar: the app decides.
        press(&mut f, readme);
        f.harness.pointer(
            &mut f.view,
            PointerEventKind::Move,
            None,
            Point::new(readme.x + 30.0, readme.y),
        );
        let editor = Point::new(600.0, 300.0);
        f.harness
            .pointer(&mut f.view, PointerEventKind::Move, None, editor);
        release(&mut f, editor, Modifiers::NONE);
        assert!(f.view.take_events().contains(&SidebarEvent::DragReleased {
            source: 7,
            at: editor,
            copy: false
        }));
        // Escape cancels a drag in progress.
        press(&mut f, readme);
        f.harness.pointer(
            &mut f.view,
            PointerEventKind::Move,
            None,
            Point::new(readme.x + 30.0, readme.y),
        );
        f.focus_tree();
        assert!(f.key(vk::ESCAPE));
        let events = f.view.take_events();
        assert!(events.contains(&SidebarEvent::DragCancelled(7)));
        assert!(f.view.drag.is_none());

        // Drop targets for external drags.
        assert_eq!(f.view.drop_target_at(f.row_center(0)), Some(1));
        assert_eq!(
            f.view.drop_target_at(f.row_center(2)),
            Some(1),
            "a file targets its folder"
        );
        assert_eq!(f.view.drop_target_at(f.row_center(8)), Some(0));
        assert_eq!(f.view.drop_target_at(Point::new(100.0, 460.0)), Some(0));
        assert_eq!(f.view.drop_target_at(Point::new(100.0, 20.0)), None);
        f.view.user_expand(5, true);
        f.layout();
        assert_eq!(
            f.view.drop_target_at(f.row_center(2)),
            Some(5),
            "c.md lives in deep"
        );
    }

    #[test]
    fn refreshing_follows_changes_on_disk_and_drops_removed_selection() {
        let mut f = fixture(SidebarOptions::default());
        f.view.user_expand(1, true);
        f.view.take_events();
        f.view.select_key(4);
        assert_eq!(f.view.selected_key(), Some(4));
        f.listings.borrow_mut()[1].1.remove(1);
        f.listings.borrow_mut()[1]
            .1
            .push(SidebarEntry::file(11, "new.md", EntryKind::Prose));
        f.view.refresh_folder(1);
        assert_eq!(
            f.names()[..5],
            ["docs", "deep", "a.md", "new.md", "empty"].map(String::from)
        );
        assert_eq!(f.view.selected_key(), None, "b.txt is gone");
        // Collapsed, unknown and file keys are ignored; the root refreshes everything.
        let before = f.calls.borrow().len();
        f.view.refresh_folder(2);
        f.view.refresh_folder(99);
        f.view.refresh_folder(7);
        assert_eq!(f.calls.borrow().len(), before);
        f.listings.borrow_mut()[0].1.pop();
        f.view.refresh_folder(0);
        assert!(!f.names().contains(&"data.bin".to_owned()));
        assert_eq!(f.view.is_expanded(1), Some(true));
    }

    #[test]
    fn expand_all_and_collapse_all() {
        let mut f = fixture(SidebarOptions::default());
        f.view.expand_all();
        assert_eq!(
            f.names(),
            [
                "docs",
                "deep",
                "c.md",
                "a.md",
                "b.txt",
                "empty",
                "cloud.md",
                "data.bin",
                "photo.png",
                "readme.md"
            ]
            .map(String::from)
        );
        f.view.collapse_all();
        assert_eq!(f.view.row_count(), 6);
        assert_eq!(f.view.is_expanded(5), Some(false));
        // Expanding again reuses the listings.
        let calls = f.calls.borrow().len();
        f.view.expand_all();
        assert_eq!(f.view.row_count(), 10);
        assert!(f.calls.borrow().len() == calls);
    }

    #[test]
    fn collapsing_a_folder_that_hides_the_selection_selects_the_folder() {
        let mut f = fixture(SidebarOptions::default());
        f.view.user_expand(1, true);
        f.view.select_key(3);
        f.view.take_events();
        f.focus_tree();
        f.view.user_expand(1, false);
        assert_eq!(f.view.selected_key(), Some(1));
        assert_eq!(
            f.view.take_events(),
            vec![SidebarEvent::Collapsed(1), SidebarEvent::Selected(1)]
        );
    }

    #[test]
    fn the_access_tree_nests_items_and_performs_actions() {
        let mut f = fixture(SidebarOptions::default());
        f.view.user_expand(1, true);
        f.view.take_events();
        f.view.select_key(3);
        f.focus_tree();
        let tree = f.harness.tree(&f.view);
        let root = tree.root().expect("root");
        assert_eq!(root.role, Role::Pane);
        let children: Vec<_> = tree.children(f.view.id()).collect();
        assert_eq!(children[0].role, Role::ToolBar);
        assert_eq!(children[0].name, "Notes");
        let buttons: Vec<&str> = tree
            .children(children[0].id)
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(buttons, vec!["More options", "New"]);
        assert_eq!(children[1].role, Role::Tree);
        assert_eq!(children[1].name, "Files");
        let top: Vec<_> = tree.children(children[1].id).collect();
        assert_eq!(top.len(), 6);
        let docs = top[0];
        assert_eq!(docs.state.expanded, Some(true));
        let nested: Vec<&str> = tree.children(docs.id).map(|n| n.name.as_str()).collect();
        assert_eq!(nested, vec!["deep", "a.md", "b.txt"]);
        let a_md = tree.children(docs.id).nth(1).expect("a.md");
        assert!(a_md.state.selected && a_md.state.focused);
        assert_eq!(tree.walk().len(), tree.len());

        let docs_id = docs.id;
        let deep_id = tree.children(docs.id).next().expect("deep").id;
        let file_id = a_md.id;
        let mut cx = f.harness.event_cx(f.view.id());
        assert!(
            f.view
                .perform_item(&mut cx, docs_id, &AccessAction::Collapse)
        );
        let mut cx = f.harness.event_cx(f.view.id());
        assert!(f.view.perform_item(&mut cx, docs_id, &AccessAction::Expand));
        let mut cx = f.harness.event_cx(f.view.id());
        assert!(!f.view.perform_item(&mut cx, file_id, &AccessAction::Expand));
        let mut cx = f.harness.event_cx(f.view.id());
        assert!(
            !f.view
                .perform_item(&mut cx, file_id, &AccessAction::Collapse)
        );
        let mut cx = f.harness.event_cx(f.view.id());
        assert!(f.view.perform_item(&mut cx, file_id, &AccessAction::Invoke));
        let mut cx = f.harness.event_cx(f.view.id());
        assert!(f.view.perform_item(&mut cx, deep_id, &AccessAction::Select));
        let mut cx = f.harness.event_cx(f.view.id());
        assert!(
            f.view
                .perform_item(&mut cx, deep_id, &AccessAction::ScrollIntoView)
        );
        let mut cx = f.harness.event_cx(f.view.id());
        assert!(!f.view.perform_item(&mut cx, deep_id, &AccessAction::Toggle));
        let mut cx = f.harness.event_cx(f.view.id());
        assert!(
            !f.view
                .perform_item(&mut cx, WidgetId::new(u64::MAX), &AccessAction::Select)
        );
        let events = f.view.take_events();
        assert!(events.contains(&SidebarEvent::Open {
            key: 3,
            new_tab: false
        }));
        assert!(events.contains(&SidebarEvent::Selected(5)));
        assert!(!f.harness.perform(&mut f.view, AccessAction::Focus));
    }

    #[test]
    fn long_listings_scroll_and_keep_the_selection_visible() {
        let mut f = fixture(SidebarOptions::default());
        {
            let mut listings = f.listings.borrow_mut();
            listings[0].1 = (0..80)
                .map(|n| SidebarEntry::file(100 + n, format!("note{n:02}.md"), EntryKind::Prose))
                .collect();
        }
        f.view.refresh_all();
        f.layout();
        assert_eq!(f.view.scroll_offset(), 0.0);
        let wheel = |delta: f32, y: f32| {
            Event::Wheel(WheelEvent {
                delta: Point::new(0.0, delta),
                precise: true,
                position: Point::new(100.0, y),
                mods: Modifiers::NONE,
            })
        };
        assert!(f.harness.send(&mut f.view, wheel(-100.0, 300.0)).handled);
        assert_eq!(f.view.scroll_offset(), 100.0);
        assert!(
            !f.harness.send(&mut f.view, wheel(-100.0, 20.0)).handled,
            "header does not scroll"
        );
        // Home/End move the selection and scroll it into view.
        f.focus_tree();
        f.key(vk::END);
        assert_eq!(f.view.selected_key(), Some(179));
        assert!(f.view.scroll_offset() > 100.0);
        f.key(vk::HOME);
        assert_eq!(f.view.scroll_offset(), 0.0);
        let canvas = f.harness.paint(&f.view);
        assert!(canvas.texts().contains(&"note00.md"));
        assert!(!canvas.texts().contains(&"note79.md"));
        // Only the rows in view are described to assistive technology.
        let tree = f.harness.tree(&f.view);
        let items = tree
            .nodes()
            .iter()
            .filter(|n| n.role == Role::TreeItem)
            .count();
        assert!(items < 20, "{items}");
    }

    #[test]
    fn measuring_and_debug() {
        let mut f = fixture(SidebarOptions::default());
        let mut cx = LayoutCx::new(
            f.view.id(),
            &f.harness.theme,
            1.0,
            &f.harness.engine,
            &mut f.harness.focus,
            &mut f.harness.requests,
        );
        assert_eq!(
            f.view.measure(&mut cx, Size::new(300.0, 500.0)),
            Size::new(300.0, 500.0)
        );
        assert_eq!(
            f.view.measure(&mut cx, Size::UNBOUNDED),
            Size::new(260.0, 40.0 + 6.0 * 32.0)
        );
        assert_eq!(f.view.bounds(), Rect::new(0.0, 80.0, 260.0, 400.0));
        let debug = format!("{:?}", f.view);
        assert!(debug.contains("SidebarView") && debug.contains("Notes"));
    }

    fn many_files(f: &mut Fixture) {
        f.listings.borrow_mut()[0].1.extend(
            (100..140).map(|n| SidebarEntry::file(n, format!("note-{n}.md"), EntryKind::Prose)),
        );
        f.view.refresh_all();
        f.layout();
    }

    fn wheel_notch(f: &Fixture, dy: f32) -> Event {
        Event::Wheel(WheelEvent {
            delta: Point::new(0.0, dy),
            precise: false,
            position: f.view.row_rect(0).center(),
            mods: Modifiers::NONE,
        })
    }

    #[test]
    fn a_custom_id_names_the_view() {
        let id = WidgetId::allocate();
        let f = fixture(SidebarOptions::default());
        let view = f.view.with_id(id);
        assert_eq!(view.id(), id);
        assert_eq!(Harness::new().node(&view).id, id);
    }

    #[test]
    fn expanding_needs_a_known_collapsed_folder() {
        let mut f = fixture(SidebarOptions::default());
        assert!(!f.view.set_expanded(9999, true));
        // A file cannot expand, and a collapsed folder cannot collapse again.
        assert!(!f.view.set_expanded(7, true));
        assert!(!f.view.set_expanded(1, false));
        f.view.user_expand(7, true);
        f.view.user_expand(1, false);
        assert!(f.view.take_events().is_empty());
        assert_eq!(f.view.is_expanded(1), Some(false));
    }

    #[test]
    fn expand_all_lists_every_nested_folder_and_collapse_all_closes_them() {
        let mut f = fixture(SidebarOptions::default());
        f.view.expand_all();
        assert_eq!(f.view.is_expanded(1), Some(true));
        assert_eq!(f.view.is_expanded(5), Some(true));
        assert_eq!(f.view.is_expanded(2), Some(true));
        assert!(f.names().contains(&"c.md".to_owned()));
        f.view.collapse_all();
        assert_eq!(f.view.is_expanded(5), Some(false));
        assert_eq!(f.view.row_count(), 6);
    }

    #[test]
    fn relisting_a_long_folder_keeps_the_expansion_selection_and_identity_of_what_remains() {
        let mut f = fixture(SidebarOptions::default());
        f.view.expand_all();
        f.view.select_key(3);
        f.layout();
        let row_ids = |f: &Fixture| -> Vec<(String, WidgetId)> {
            let tree = f.harness.tree(&f.view);
            tree.nodes()
                .iter()
                .filter(|node| {
                    ["docs", "deep", "c.md", "a.md", "b.txt", "empty", "cloud.md", "data.bin"]
                        .contains(&node.name.as_str())
                })
                .map(|node| (node.name.clone(), node.id))
                .collect()
        };
        let before = row_ids(&f);
        assert_eq!(before.len(), 8);
        many_files(&mut f);
        assert_eq!(f.view.row_count(), 50);
        assert_eq!(row_ids(&f), before, "the rows keep their accessibility ids");
        for folder in [1, 2, 5] {
            assert_eq!(f.view.is_expanded(folder), Some(true));
        }
        assert_eq!(f.view.selected_key(), Some(3));
        assert_eq!(
            f.names()[6..9],
            ["cloud.md", "data.bin", "note-100.md"].map(String::from)
        );
    }

    #[test]
    fn expand_all_lists_each_unlisted_folder_once_and_keeps_the_selection() {
        let mut f = fixture(SidebarOptions::default());
        f.view.user_expand(1, true);
        f.view.select_key(3);
        f.view.take_events();
        let listed = f.calls.borrow().len();
        f.view.expand_all();
        let mut now = f.calls.borrow()[listed..].to_vec();
        now.sort_unstable();
        assert_eq!(now, [2, 5], "the listed folder 1 is not listed again");
        assert_eq!(f.view.row_count(), 10);
        assert_eq!(f.view.selected_key(), Some(3));
        assert!(f.view.take_events().is_empty());
    }

    #[test]
    fn only_visible_entries_can_be_renamed_and_ending_without_a_rename_does_nothing() {
        let mut f = fixture(SidebarOptions::default());
        let id = f.view.id();
        let mut cx = f.harness.event_cx(id);
        assert!(!f.view.begin_rename(&mut cx, 9999));
        f.view.end_rename(&mut cx, true);
        assert_eq!(f.view.renaming(), None);
        assert!(f.view.take_events().is_empty());
        // An entry inside a folder that is collapsed again has no row to edit in.
        f.view.user_expand(1, true);
        f.view.user_expand(1, false);
        f.view.take_events();
        let mut cx = f.harness.event_cx(id);
        assert!(!f.view.begin_rename(&mut cx, 3));
        assert_eq!(f.view.renaming(), None);
    }

    #[test]
    fn animation_frames_and_wheels_pass_through_an_open_rename() {
        let mut f = fixture(SidebarOptions::default());
        let id = f.view.id();
        let mut cx = f.harness.event_cx(id);
        assert!(f.view.begin_rename(&mut cx, 7));
        let frame = Event::AnimationFrame(std::time::Duration::from_millis(16));
        f.harness.send(&mut f.view, frame);
        let notch = wheel_notch(&f, -1.0);
        f.harness.send(&mut f.view, notch);
        assert_eq!(f.view.renaming(), Some(7));
    }

    #[test]
    fn an_open_rename_box_stays_put_when_its_row_is_hidden_by_a_collapse() {
        let mut f = fixture(SidebarOptions::default());
        f.view.user_expand(1, true);
        f.view.take_events();
        f.layout();
        let id = f.view.id();
        let mut cx = f.harness.event_cx(id);
        assert!(f.view.begin_rename(&mut cx, 3));
        f.layout();
        let row = f
            .view
            .row_rect(f.view.row_of(3).expect("row of the renamed node"));
        let placed = f.view.rename.as_ref().expect("renaming").text_box.bounds();
        assert!(placed.y >= row.y && placed.bottom() <= row.bottom());
        f.view.user_expand(1, false);
        f.layout();
        assert_eq!(f.view.renaming(), Some(3));
        let kept = f.view.rename.as_ref().expect("renaming").text_box.bounds();
        assert_eq!(kept, placed, "no row to follow, so the box is not moved");
    }

    #[test]
    fn right_on_a_file_leaves_the_selection_and_the_tree_alone() {
        let mut f = fixture(SidebarOptions::default());
        f.focus_tree();
        f.view.select_key(7);
        f.view.take_events();
        let rows = f.view.row_count();
        assert!(f.key(vk::RIGHT));
        assert_eq!(f.view.selected_key(), Some(7));
        assert_eq!(f.view.row_count(), rows);
        assert!(f.view.take_events().is_empty());
    }

    #[test]
    fn the_pointer_highlights_the_hovered_row_until_it_leaves() {
        let mut f = fixture(SidebarOptions::default());
        let hover = f.harness.theme.tokens.hover_bg;
        let fills = |f: &mut Fixture| {
            f.harness
                .paint(&f.view)
                .ops()
                .filter(|op| matches!(op, DrawOp::FillRoundedRect { color, .. } if *color == hover))
                .count()
        };
        assert_eq!(fills(&mut f), 0);
        let at = f.row_center(2);
        assert!(f.harness.hover(&mut f.view, at).handled);
        assert_eq!(fills(&mut f), 1);
        // The same row again changes nothing.
        assert!(!f.harness.hover(&mut f.view, at).handled);
        f.harness
            .pointer(&mut f.view, PointerEventKind::Leave, None, at);
        assert_eq!(fills(&mut f), 0);
    }

    #[test]
    fn the_arrow_keys_walk_the_rows_from_the_current_selection() {
        let mut f = fixture(SidebarOptions::default());
        f.focus_tree();
        assert!(f.key(vk::DOWN));
        assert_eq!(f.view.selected_key(), f.view.row_key(0));
        assert!(f.key(vk::DOWN));
        assert_eq!(f.view.selected_key(), f.view.row_key(1));
        assert!(f.key(vk::UP));
        assert_eq!(f.view.selected_key(), f.view.row_key(0));
    }

    #[test]
    fn nothing_is_painted_outside_the_damaged_area() {
        let mut f = fixture(SidebarOptions::default());
        let away = Rect::new(600.0, 0.0, 50.0, 50.0);
        assert_eq!(
            f.harness.paint_damaged(&f.view, Some(away)).ops().count(),
            0
        );
        let over = f.view.bounds();
        assert!(f.harness.paint_damaged(&f.view, Some(over)).ops().count() > 0);
    }

    #[test]
    fn a_drop_onto_the_workspace_root_outlines_the_whole_list() {
        let mut f = fixture(SidebarOptions::default());
        let accent = f.harness.theme.tokens.accent;
        let outlines = |f: &mut Fixture| {
            f.harness
                .paint(&f.view)
                .ops()
                .filter(|op| matches!(op, DrawOp::StrokeRoundedRect { color, width, .. } if *color == accent && *width == 2.0))
                .count()
        };
        assert_eq!(outlines(&mut f), 0);
        f.view.drag = Some(Drag {
            source: 7,
            target: Some(0),
        });
        assert_eq!(outlines(&mut f), 1);
    }

    #[test]
    fn a_wheel_over_a_short_list_is_left_alone() {
        let mut f = fixture(SidebarOptions::default());
        let notch = wheel_notch(&f, -1.0);
        assert!(!f.harness.send(&mut f.view, notch).handled);
        assert_eq!(f.view.scroll_offset(), 0.0);
    }

    #[test]
    fn dragging_the_scroll_bar_scrolls_a_long_list() {
        let mut f = fixture(SidebarOptions::default());
        many_files(&mut f);
        let grab = f.view.bar.thumb_rect().center();
        let left = Some(PointerButton::Left);
        let down = f
            .harness
            .pointer(&mut f.view, PointerEventKind::Down, left, grab);
        assert!(down.handled);
        assert!(f.view.take_events().is_empty());
        f.harness.requests.take_invalidation();
        let to = Point::new(grab.x, grab.y + 60.0);
        f.harness
            .pointer(&mut f.view, PointerEventKind::Move, None, to);
        assert!(f.view.scroll_offset() > 0.0);
        assert!(!f.harness.requests.take_invalidation().is_clean());
        f.harness
            .pointer(&mut f.view, PointerEventKind::Up, left, to);
        assert!(!f.view.bar.is_dragging());
    }

    #[test]
    fn a_notched_wheel_eases_a_long_list_and_every_frame_repaints_it() {
        let mut f = fixture(SidebarOptions::default());
        many_files(&mut f);
        f.view.bar.model_mut().set_motion(Motion::full());
        let notch = wheel_notch(&f, -3.0);
        assert!(f.harness.send(&mut f.view, notch).handled);
        assert_eq!(f.view.scroll_offset(), 0.0);
        f.harness.frame(&mut f.view, 16);
        f.harness.requests.take_invalidation();
        f.harness.frame(&mut f.view, 100);
        assert!(f.view.scroll_offset() > 0.0);
        assert!(!f.harness.requests.take_invalidation().is_clean());
        f.harness.run_animations(&mut f.view);
        assert_eq!(f.view.scroll_offset(), f.view.bar.model().target());
    }
}
