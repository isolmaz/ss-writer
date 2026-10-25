//! The overlays on top of the editor and the sidebar (spec §2.9, §3.2, §7): find / replace,
//! Go to line, Quick Open and the command palette, the Search in folder panel, the link edit
//! popup, the `[[` completion list, the heading level menu and the selection toolbar.
//!
//! [`Overlays`] owns the `ssw-ui` widgets of one window. [`super::shell_view::ShellView`]
//! gives it layout, paint and input; everything that changes the editor (selection, find
//! highlights, links, completions) happens in [`Overlays::pump`], and what only the controller
//! can do (open a file, run a command, search a folder) leaves as an [`OverlayAction`].
//!
//! The B I U pill lives in the title bar's command row; it needs no overlay.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use ssw_core::command::CommandId;
use ssw_core::document::Document;
use ssw_core::edit::{Edit, Selection, Transaction};
use ssw_core::i18n::Localizer;
use ssw_core::keymap::Keymap;
use ssw_core::links::RelPath;
use ssw_core::settings::ExportSettings;
use ssw_core::text::TextRange;
use ssw_core::toc::Toc;
use ssw_ui::access::AccessTree;
use ssw_ui::canvas::Canvas;
use ssw_ui::editor::{DocumentKind, EditorEvent, EditorView};
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::input::{Event, vk};
use ssw_ui::shell::export_dialog::ExportRequest;
use ssw_ui::shell::find::{FindBar, FindEvent, SharedDocument};
use ssw_ui::shell::folder_search::{
    FolderSearchEvent, FolderSearchFile, FolderSearchHit, FolderSearchLine, FolderSearchPanel,
    FolderSearchRequest, FolderSearchSummary,
};
use ssw_ui::shell::goto::{GoToEvent, GoToLine};
use ssw_ui::shell::palette::ShortcutLabels;
use ssw_ui::shell::popups::{
    CommandMenuEvent, HeadingLevelMenu, LinkEditEvent, LinkEditPopup, SelectionToolbar,
    ToolbarEvent, WikilinkAccept, WikilinkCompletion, WikilinkEvent,
};
use ssw_ui::shell::quick_open::{QuickOpen, QuickOpenEvent, QuickOpenMode};
use ssw_ui::text::TextLayoutEngine;
use ssw_ui::theme::Theme;
use ssw_ui::widget::{
    AccessCx, EventCx, EventResult, FocusManager, LayoutCx, PaintCx, PointerCapture, Requests,
    Widget, WidgetId,
};
use ssw_ui::widgets::anim::Motion;
use ssw_ui::widgets::{Control, SharedClipboard};

use super::export::ExportHost;

/// What an overlay needs the controller to do.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayAction {
    /// Quick Open chose a file: the path relative to the workspace root, as listed.
    OpenFile(String),
    /// Run a command (the palette, the heading menu, the selection toolbar).
    RunCommand(CommandId),
    /// Run this folder search (cancel a running one first).
    FolderSearch(FolderSearchRequest),
    /// Cancel the running folder search; its results are gone.
    FolderSearchCleared,
    /// Open this file at the chosen match.
    OpenMatch(FolderSearchHit),
    /// The export dialog's Export button: export the document in this format with these
    /// options (the controller asks for the target file).
    Export(ExportRequest),
}

/// The shortcut texts of a [`Keymap`]'s first chords.
#[derive(Debug, Clone, Default)]
pub struct KeymapLabels {
    keymap: Keymap,
}

impl KeymapLabels {
    /// Labels for `keymap`.
    #[must_use]
    pub fn new(keymap: Keymap) -> Self {
        Self { keymap }
    }
}

impl ShortcutLabels for KeymapLabels {
    fn shortcut_label(&self, command: CommandId) -> Option<String> {
        self.keymap
            .chords_for(command)
            .next()
            .map(|chord| chord.to_string())
    }
}

/// The window areas the overlays are placed in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayRects {
    /// The whole client area.
    pub window: Rect,
    /// The editor area (below the banners).
    pub editor: Rect,
    /// The sidebar, when it is shown.
    pub sidebar: Option<Rect>,
}

/// The state every widget of one delivery shares (the host's focus, capture and requests).
pub struct OverlayEnv<'a> {
    /// The colors.
    pub theme: &'a Theme,
    /// `dpi / 96`.
    pub scale: f32,
    /// Text layout.
    pub text: &'a dyn TextLayoutEngine,
    /// Keyboard focus.
    pub focus: &'a mut FocusManager,
    /// Pointer capture.
    pub capture: &'a mut PointerCapture,
    /// Repaint, timer and cursor requests.
    pub requests: &'a mut Requests,
}

impl fmt::Debug for OverlayEnv<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OverlayEnv")
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}

/// Runs `widget.event` with a fresh context. The overlays move the focus between their own
/// controls and never answer with a focus request for the host to grant.
fn route<W: Control>(widget: &mut W, event: &Event, env: &mut OverlayEnv<'_>) -> EventResult {
    let id = widget.id();
    let mut cx = EventCx::new(
        id,
        env.theme,
        env.scale,
        env.text,
        env.focus,
        env.capture,
        env.requests,
    );
    widget.event(&mut cx, event)
}

/// What the editor-side of an overlay event needs from the document.
fn empty_document() -> SharedDocument {
    Rc::new(RefCell::new(Document::new("")))
}

/// The overlays of one window.
pub struct Overlays {
    localizer: Rc<Localizer>,
    clipboard: SharedClipboard,
    find: FindBar,
    goto: GoToLine,
    quick: QuickOpen,
    folder: FolderSearchPanel,
    folder_visible: bool,
    link: LinkEditPopup,
    wikilink: WikilinkCompletion,
    heading: HeadingLevelMenu,
    toolbar: SelectionToolbar,
    /// The export dialog, over the whole window while it is open.
    export: ExportHost,
    pending_heading: Option<(Point, u8)>,
    wiki_start: Option<usize>,
    /// The editor, search generation and current match last pushed to the editor as find
    /// highlights.
    synced_highlights: Option<(WidgetId, u64, Option<usize>)>,
    refocus: bool,
    actions: Vec<OverlayAction>,
}

impl fmt::Debug for Overlays {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Overlays")
            .field("find", &self.find.is_visible())
            .field("goto", &self.goto.is_visible())
            .field("quick_open", &self.quick.is_visible())
            .field("folder_search", &self.folder_visible)
            .finish_non_exhaustive()
    }
}

/// A shortcut table with no chords (windows start with it until the controller hands over the
/// keymap).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoShortcuts;

impl ShortcutLabels for NoShortcuts {
    fn shortcut_label(&self, _command: CommandId) -> Option<String> {
        None
    }
}

impl Overlays {
    /// Hidden overlays; every string comes from `localizer`, shortcut texts from `shortcuts`.
    #[must_use]
    pub fn new(
        localizer: &Rc<Localizer>,
        clipboard: &SharedClipboard,
        shortcuts: &dyn ShortcutLabels,
    ) -> Self {
        let doc = empty_document();
        Self {
            find: FindBar::new(Rc::clone(&doc), Rc::clone(localizer), clipboard),
            goto: GoToLine::new(doc, Rc::clone(localizer), clipboard),
            quick: QuickOpen::new(Rc::clone(localizer), shortcuts, clipboard),
            folder: FolderSearchPanel::new(Rc::clone(localizer), clipboard),
            folder_visible: false,
            link: LinkEditPopup::new(localizer, clipboard),
            wikilink: WikilinkCompletion::new(Rc::clone(localizer)),
            heading: HeadingLevelMenu::new(Rc::clone(localizer), shortcuts),
            toolbar: SelectionToolbar::new(Rc::clone(localizer), shortcuts),
            export: ExportHost::new(Rc::clone(localizer), Rc::clone(clipboard)),
            pending_heading: None,
            wiki_start: None,
            synced_highlights: None,
            refocus: false,
            actions: Vec::new(),
            localizer: Rc::clone(localizer),
            clipboard: Rc::clone(clipboard),
        }
    }

    /// Shows the shortcuts of `keymap` in the menus and the palette.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        let labels = KeymapLabels::new(keymap);
        self.quick.refresh_shortcuts(&labels);
        if !self.toolbar.is_visible() {
            self.toolbar = SelectionToolbar::new(Rc::clone(&self.localizer), &labels);
        }
        if !self.heading.is_open() {
            self.heading = HeadingLevelMenu::new(Rc::clone(&self.localizer), &labels);
        }
    }

    /// Searches and edits the document of the active tab from now on; popups of the previous
    /// document close.
    pub fn set_document(&mut self, doc: &SharedDocument) {
        self.find.set_document(Rc::clone(doc));
        self.goto.set_document(Rc::clone(doc));
        self.link = LinkEditPopup::new(&self.localizer, &self.clipboard);
        self.wikilink.close();
        self.wiki_start = None;
        self.toolbar.hide();
    }

    // ----- opening -------------------------------------------------------------------------

    /// Ctrl+F (Ctrl+H with `replace`).
    pub fn open_find(&mut self, replace: bool) {
        self.find.open(replace);
    }

    /// F3: the next match; opens the bar when it is closed.
    pub fn find_next(&mut self) {
        if self.find.is_visible() {
            self.find.find_next();
        } else {
            self.find.open(false);
        }
    }

    /// Shift+F3: the previous match; opens the bar when it is closed.
    pub fn find_previous(&mut self) {
        if self.find.is_visible() {
            self.find.find_previous();
        } else {
            self.find.open(false);
        }
    }

    /// Ctrl+G.
    pub fn open_goto(&mut self) {
        self.goto.open();
    }

    /// Ctrl+E (files) and Ctrl+Shift+P (commands). `files` are the workspace's paths relative
    /// to its root and `recent` the indices of the recently used ones; `toc` lists the headings
    /// of the active document for the `#` mode.
    pub fn open_quick_open(
        &mut self,
        mode: QuickOpenMode,
        files: Vec<String>,
        recent: Vec<usize>,
        toc: &Toc,
    ) {
        self.quick.set_files(files, recent);
        self.quick.set_outline(toc);
        self.quick.open(mode);
    }

    /// Ctrl+Shift+F: shows the folder search panel in the sidebar; a one-line `seed` fills the
    /// query box.
    pub fn open_folder_search(&mut self, seed: Option<&str>) {
        if let Some(seed) = seed.filter(|text| !text.is_empty() && !text.contains(['\n', '\r'])) {
            self.folder.set_query(seed);
        }
        self.folder_visible = true;
        self.folder.open();
    }

    /// File → Export…: shows the export dialog over the window, preset from `defaults` (the
    /// Export settings).
    pub fn open_export(&mut self, defaults: &ExportSettings, motion: Motion) {
        self.export.open(defaults, motion);
    }

    /// Whether the export dialog is shown.
    #[must_use]
    pub fn export_visible(&self) -> bool {
        self.export.is_open()
    }

    /// The export dialog, while it is shown.
    #[must_use]
    pub fn export_dialog(&self) -> Option<&ssw_ui::shell::export_dialog::ExportDialog> {
        self.export.dialog()
    }

    /// Describes the export dialog when it is shown; returns its node id.
    pub fn export_access(&self, cx: &AccessCx, out: &mut AccessTree) -> Option<WidgetId> {
        self.export.access(cx, out)
    }

    /// Hides the folder search panel (the sidebar shows the tree again).
    pub fn close_folder_search(&mut self) {
        self.folder_visible = false;
        self.refocus = true;
    }

    /// Whether the folder search panel replaces the tree.
    #[must_use]
    pub fn folder_search_visible(&self) -> bool {
        self.folder_visible
    }

    /// Hands the matches of a finished folder search to the panel.
    pub fn set_folder_results(
        &mut self,
        files: Vec<FolderSearchFile>,
        summary: FolderSearchSummary,
    ) {
        self.folder.set_results(files, summary);
    }

    /// Shows the link popup for the link at the caret, or for a new link around the selection.
    pub fn open_link(&mut self, editor: &EditorView) {
        let anchor = editor.caret_rect().unwrap_or(Rect::ZERO);
        match editor.link_at_caret() {
            Some(info) => {
                let anchor = editor.rect_at(info.range.start).unwrap_or(anchor);
                self.link.show(anchor, &info.text, &info.url);
            }
            None => {
                let text = editor.selected_text();
                let text = if text.contains(['\n', '\r']) {
                    String::new()
                } else {
                    text
                };
                self.link.show(anchor, &text, "");
            }
        }
    }

    // ----- state ---------------------------------------------------------------------------

    /// Whether the find bar is shown.
    #[must_use]
    pub fn find_visible(&self) -> bool {
        self.find.is_visible()
    }

    /// Whether Go to line is shown.
    #[must_use]
    pub fn goto_visible(&self) -> bool {
        self.goto.is_visible()
    }

    /// Whether Quick Open (or the palette) is shown.
    #[must_use]
    pub fn quick_open_visible(&self) -> bool {
        self.quick.is_visible()
    }

    /// Whether the link popup is shown.
    #[must_use]
    pub fn link_visible(&self) -> bool {
        self.link.is_visible()
    }

    /// Whether the `[[` completion list is shown.
    #[must_use]
    pub fn wikilink_visible(&self) -> bool {
        self.wikilink.is_visible()
    }

    /// Whether the selection toolbar is shown.
    #[must_use]
    pub fn toolbar_visible(&self) -> bool {
        self.toolbar.is_visible()
    }

    /// The find bar.
    #[must_use]
    pub fn find(&self) -> &FindBar {
        &self.find
    }

    /// The find bar.
    pub fn find_mut(&mut self) -> &mut FindBar {
        &mut self.find
    }

    /// Go to line.
    pub fn goto_mut(&mut self) -> &mut GoToLine {
        &mut self.goto
    }

    /// Quick Open and the palette.
    #[must_use]
    pub fn quick_open(&self) -> &QuickOpen {
        &self.quick
    }

    /// Quick Open and the palette.
    pub fn quick_open_mut(&mut self) -> &mut QuickOpen {
        &mut self.quick
    }

    /// The folder search panel.
    #[must_use]
    pub fn folder_search(&self) -> &FolderSearchPanel {
        &self.folder
    }

    /// The folder search panel.
    pub fn folder_search_mut(&mut self) -> &mut FolderSearchPanel {
        &mut self.folder
    }

    /// The link popup.
    #[must_use]
    pub fn link(&self) -> &LinkEditPopup {
        &self.link
    }

    /// The link popup.
    pub fn link_mut(&mut self) -> &mut LinkEditPopup {
        &mut self.link
    }

    /// The `[[` completion list.
    #[must_use]
    pub fn wikilink(&self) -> &WikilinkCompletion {
        &self.wikilink
    }

    /// The `[[` completion list.
    pub fn wikilink_mut(&mut self) -> &mut WikilinkCompletion {
        &mut self.wikilink
    }

    /// The selection toolbar.
    #[must_use]
    pub fn toolbar(&self) -> &SelectionToolbar {
        &self.toolbar
    }

    /// The selection toolbar.
    pub fn toolbar_mut(&mut self) -> &mut SelectionToolbar {
        &mut self.toolbar
    }

    /// The heading level menu.
    #[must_use]
    pub fn heading_menu(&self) -> &HeadingLevelMenu {
        &self.heading
    }

    /// Feeds the workspace's Markdown files (relative paths) and the headings of the active
    /// document to the `[[` completion.
    pub fn set_completion_sources(&mut self, files: Vec<RelPath>, recent: Vec<usize>, toc: &Toc) {
        self.wikilink.set_files(files, recent);
        self.wikilink.set_outline(toc);
    }

    /// Whether any overlay is up that takes keyboard input.
    #[must_use]
    pub fn captures_keys(&self) -> bool {
        self.find.is_visible()
            || self.goto.is_visible()
            || self.quick.is_visible()
            || self.link.is_visible()
            || self.wikilink.is_visible()
            || self.toolbar.is_visible()
            || self.heading.is_open()
            || self.export.is_open()
            || self.folder_visible
    }

    /// The overlay button under `point` and its name, the text of its tooltip (the selection
    /// toolbar, the icon controls of the find bar).
    #[must_use]
    pub fn tooltip_at(&self, point: Point) -> Option<(Rect, &str)> {
        self.toolbar
            .tooltip_at(point)
            .or_else(|| self.find.tooltip_at(point))
    }

    /// Whether an open overlay takes the pointer from everything behind it, so none of the
    /// window's controls may show a tooltip.
    #[must_use]
    pub fn covers_tooltips(&self) -> bool {
        self.export.is_open()
            || self.heading.is_open()
            || self.toolbar.is_menu_open()
            || self.quick.is_visible()
    }

    // ----- layout, paint, accessibility ----------------------------------------------------

    /// Places the visible overlays.
    pub fn arrange(&mut self, cx: &mut LayoutCx, rects: &OverlayRects) {
        let id = self.find.id();
        self.find.arrange(&mut cx.for_widget(id), rects.editor);
        let id = self.goto.id();
        self.goto.arrange(&mut cx.for_widget(id), rects.editor);
        if let (true, Some(sidebar)) = (self.folder_visible, rects.sidebar) {
            let id = self.folder.id();
            self.folder.arrange(&mut cx.for_widget(id), sidebar);
        }
        let id = self.quick.id();
        self.quick.arrange(&mut cx.for_widget(id), rects.window);
        let id = self.link.id();
        self.link.arrange(&mut cx.for_widget(id), rects.window);
        let id = self.wikilink.id();
        self.wikilink.arrange(&mut cx.for_widget(id), rects.window);
        let id = self.toolbar.id();
        self.toolbar.arrange(&mut cx.for_widget(id), rects.window);
        let id = self.heading.id();
        self.heading.arrange(&mut cx.for_widget(id), rects.window);
        self.export.arrange(cx, rects.window);
    }

    /// The widgets that asked for keyboard focus (their boxes can only be focused once laid
    /// out); the host focuses them after its layout pass.
    pub fn take_focus_requests(&mut self) -> Vec<WidgetId> {
        [
            self.find.take_focus_request(),
            self.goto.take_focus_request(),
            self.quick.take_focus_request(),
            self.folder.take_focus_request(),
            self.link.take_focus_request(),
            self.export.take_focus_request(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Whether the focus should return to the editor (an overlay closed); true once.
    pub fn take_refocus(&mut self) -> bool {
        let export = self.export.take_refocus();
        std::mem::take(&mut self.refocus) || export
    }

    /// Paints the visible overlays, the topmost last.
    pub fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas, sidebar: Option<Rect>) {
        if let (true, Some(rect)) = (self.folder_visible, sidebar) {
            canvas.fill_rect(rect, cx.theme.tokens.sidebar_bg);
            self.folder.paint(&cx.for_widget(self.folder.id()), canvas);
        }
        self.find.paint(&cx.for_widget(self.find.id()), canvas);
        self.goto.paint(&cx.for_widget(self.goto.id()), canvas);
        self.quick.paint(&cx.for_widget(self.quick.id()), canvas);
        self.link.paint(&cx.for_widget(self.link.id()), canvas);
        self.wikilink
            .paint(&cx.for_widget(self.wikilink.id()), canvas);
        self.toolbar
            .paint(&cx.for_widget(self.toolbar.id()), canvas);
        self.heading
            .paint(&cx.for_widget(self.heading.id()), canvas);
        self.export.paint(cx, canvas);
    }

    /// Describes every overlay (hidden ones as off-screen nodes); returns their ids.
    pub fn access(&self, cx: &AccessCx, out: &mut AccessTree) -> Vec<WidgetId> {
        let mut ids = Vec::new();
        let mut add = |control: &dyn Control| {
            control.access(&cx.for_widget(control.id()), out);
            ids.push(control.id());
        };
        add(&self.find);
        add(&self.goto);
        if self.folder_visible {
            add(&self.folder);
        }
        add(&self.quick);
        add(&self.link);
        add(&self.wikilink);
        add(&self.toolbar);
        add(&self.heading);
        ids.extend(self.export.access(cx, out));
        ids
    }

    // ----- input ---------------------------------------------------------------------------

    /// Routes `event` to the visible overlays, the topmost first. Returns what the first one
    /// that took the event answered; an ignored result means the editor gets the event.
    pub fn event(&mut self, event: &Event, env: &mut OverlayEnv<'_>) -> EventResult {
        let mut result = EventResult::ignored();
        if self.export.is_open() {
            // Modal: nothing behind the dialog sees the event.
            result = result.or(self.export.event(event, env));
            if result.handled {
                return result;
            }
        }
        if self.heading.is_open() {
            result = result.or(route(&mut self.heading, event, env));
            if result.handled {
                return result;
            }
        }
        if self.toolbar.is_visible() {
            result = result.or(route(&mut self.toolbar, event, env));
            if result.handled {
                return result;
            }
        }
        if self.link.is_visible() {
            result = result.or(route(&mut self.link, event, env));
            if result.handled {
                return result;
            }
        }
        if self.wikilink.is_visible() {
            result = result.or(route(&mut self.wikilink, event, env));
            if result.handled {
                return result;
            }
        }
        if self.quick.is_visible() {
            result = result.or(route(&mut self.quick, event, env));
            if result.handled {
                return result;
            }
        }
        if self.goto.is_visible() {
            result = result.or(route(&mut self.goto, event, env));
            if result.handled {
                return result;
            }
        }
        if self.find.is_visible() {
            result = result.or(route(&mut self.find, event, env));
            if result.handled {
                return result;
            }
            if let Event::Key(key) = event
                && key.is_down
                && key.vk == vk::ESCAPE
                && key.mods.is_empty()
            {
                // Esc closes the bar even while the editor has the focus.
                self.find.close();
                env.requests.invalidate_all();
                return EventResult::handled();
            }
        }
        if self.folder_visible {
            let escape = matches!(event, Event::Key(key) if key.is_down
                && key.vk == vk::ESCAPE
                && key.mods.is_empty());
            if escape && env.focus.focused().is_some_and(|id| self.folder.owns(id)) {
                self.close_folder_search();
                env.requests.invalidate_all();
                return EventResult::handled();
            }
            result = result.or(route(&mut self.folder, event, env));
        }
        result
    }

    /// Opens what the editor asked for (the heading menu needs an event context).
    pub fn flush(&mut self, env: &mut OverlayEnv<'_>) {
        let Some((origin, level)) = self.pending_heading.take() else {
            return;
        };
        let id = self.heading.id();
        let mut cx = EventCx::new(
            id,
            env.theme,
            env.scale,
            env.text,
            env.focus,
            env.capture,
            env.requests,
        );
        self.heading.open_at(&mut cx, origin, Some(level));
        env.requests.invalidate_all();
    }

    // ----- editor events -------------------------------------------------------------------

    /// Looks at an event of the active editor: the ones that open overlays are consumed, the
    /// others are handed back. Typing moves the `[[` completion along.
    pub fn editor_event(&mut self, event: EditorEvent, editor: &EditorView) -> Option<EditorEvent> {
        match event {
            EditorEvent::SelectionToolbar { anchor } => {
                // Every button of the toolbar writes Markdown, which a plain-text or diagram
                // document refuses (spec §4.1): there the toolbar is not offered.
                if editor.config().document_kind == DocumentKind::Markdown {
                    self.toolbar.show(anchor);
                }
                None
            }
            EditorEvent::HeadingMenu { level, anchor, .. } => {
                self.pending_heading = Some((Point::new(anchor.x, anchor.bottom()), level));
                None
            }
            EditorEvent::Edited | EditorEvent::SelectionChanged(_) => {
                self.update_wikilink(editor);
                Some(event)
            }
            other => Some(other),
        }
    }

    /// Opens, moves or closes the `[[` completion for the caret of `editor`.
    fn update_wikilink(&mut self, editor: &EditorView) {
        let (start, query) = {
            let doc = editor.document().borrow();
            let selection = doc.primary_selection();
            if !selection.is_caret() {
                self.wikilink.close();
                self.wiki_start = None;
                return;
            }
            let caret = selection.head;
            let buffer = doc.buffer();
            let line = buffer.line_range(buffer.line_of(caret));
            let before = buffer.slice(TextRange::new(line.start.min(caret), caret));
            match wikilink_query(&before) {
                Some(found) => (caret - found.len(), found.to_owned()),
                None => {
                    self.wikilink.close();
                    self.wiki_start = None;
                    return;
                }
            }
        };
        self.wiki_start = Some(start);
        if self.wikilink.is_visible() {
            self.wikilink.set_query(&query);
        } else {
            let anchor = editor.caret_rect().unwrap_or(Rect::ZERO);
            self.wikilink.open(anchor, &query);
        }
    }

    // ----- effects -------------------------------------------------------------------------

    /// Applies what the overlays decided to the active `editor` and returns what the
    /// controller must do.
    pub fn pump(&mut self, mut editor: Option<&mut EditorView>) -> Vec<OverlayAction> {
        if let Some(editor) = editor.as_mut() {
            self.sync_highlights(editor);
        }
        for event in self.find.take_events() {
            match event {
                FindEvent::Reveal(range) => {
                    if let Some(editor) = editor.as_mut() {
                        editor.go_to(Selection::from_range(range));
                    }
                }
                FindEvent::Replaced { .. } => {}
                FindEvent::Closed => {
                    if let Some(editor) = editor.as_mut() {
                        editor.clear_search_highlights();
                    }
                    self.synced_highlights = None;
                    self.refocus = true;
                }
            }
        }
        for event in self.goto.take_events() {
            match event {
                GoToEvent::Went { offset } => {
                    if let Some(editor) = editor.as_mut() {
                        editor.go_to(Selection::caret(offset));
                    }
                    self.refocus = true;
                }
                GoToEvent::Closed => self.refocus = true,
            }
        }
        for event in self.quick.take_events() {
            match event {
                QuickOpenEvent::OpenFile { path } => {
                    self.actions.push(OverlayAction::OpenFile(path))
                }
                QuickOpenEvent::GoToHeading { range, .. } => {
                    if let Some(editor) = editor.as_mut() {
                        editor.go_to(Selection::caret(range.start));
                    }
                    self.refocus = true;
                }
                QuickOpenEvent::RunCommand(command) => {
                    self.actions.push(OverlayAction::RunCommand(command));
                }
                QuickOpenEvent::Closed => self.refocus = true,
            }
        }
        for event in self.folder.take_events() {
            self.actions.push(match event {
                FolderSearchEvent::Search(request) => OverlayAction::FolderSearch(request),
                FolderSearchEvent::Cleared => OverlayAction::FolderSearchCleared,
                FolderSearchEvent::Open(hit) => OverlayAction::OpenMatch(hit),
            });
        }
        for event in self.link.take_events() {
            if let Some(editor) = editor.as_mut() {
                apply_link_event(editor, event);
            }
            self.refocus = true;
        }
        for event in self.wikilink.take_events() {
            match event {
                WikilinkEvent::Accept(accept) => {
                    if let (Some(editor), Some(start)) = (editor.as_mut(), self.wiki_start) {
                        accept_wikilink(editor, start, &accept);
                    }
                    self.wiki_start = None;
                }
                WikilinkEvent::Dismissed => self.wiki_start = None,
            }
        }
        for event in self.heading.take_events() {
            match event {
                CommandMenuEvent::Command(command) => {
                    self.actions.push(OverlayAction::RunCommand(command));
                    self.refocus = true;
                }
                CommandMenuEvent::Dismissed => self.refocus = true,
            }
        }
        for event in self.toolbar.take_events() {
            match event {
                ToolbarEvent::Command(command) => {
                    self.actions.push(OverlayAction::RunCommand(command));
                }
                ToolbarEvent::Hidden => {}
            }
        }
        if let Some(request) = self.export.take_request() {
            self.actions.push(OverlayAction::Export(request));
        }
        std::mem::take(&mut self.actions)
    }

    /// Keeps the active editor's find highlights in step with the bar (the document may have
    /// changed since the last pump). While the bar is closed the editor shows none, including
    /// an editor that was inactive when the bar closed.
    pub fn sync_highlights(&mut self, editor: &mut EditorView) {
        if !self.find.is_visible() {
            editor.clear_search_highlights();
            self.synced_highlights = None;
            return;
        }
        self.find.refresh();
        let current = self.find.current_index();
        let synced = (editor.id(), self.find.generation(), current);
        if self.synced_highlights != Some(synced) {
            editor.set_search_highlights(self.find.matches().to_vec(), current);
            self.synced_highlights = Some(synced);
        }
    }
}

/// The text typed after the last `[[` of `before` (the line up to the caret), when the
/// wikilink is still open: no `]]` and no further `[[` after it.
fn wikilink_query(before: &str) -> Option<&str> {
    let open = before.rfind("[[")?;
    let query = &before[open + 2..];
    (!query.contains("]]") && !query.contains('[')).then_some(query)
}

/// Applies the answer of the link popup to the document.
fn apply_link_event(editor: &mut EditorView, event: LinkEditEvent) {
    match event {
        LinkEditEvent::Apply { text, url } => {
            let url = url.trim();
            if url.is_empty() {
                remove_link(editor);
            } else {
                editor.set_link(&text, url);
            }
        }
        LinkEditEvent::Remove => remove_link(editor),
        LinkEditEvent::Cancel => {}
    }
}

/// Replaces the link at the caret by its text.
fn remove_link(editor: &mut EditorView) {
    let Some(info) = editor.link_at_caret() else {
        return;
    };
    let doc = editor.document();
    let before = doc.borrow().primary_selection();
    let after = Selection::new(info.range.start, info.range.start + info.text.len());
    let edit = Edit::replace(info.range, info.text);
    let _ = doc
        .borrow_mut()
        .apply(Transaction::single(edit, before, after));
}

/// Writes the chosen completion between `[[` and `]]`, closing the brackets when they are
/// not there yet; the caret ends after them.
fn accept_wikilink(editor: &mut EditorView, query_start: usize, accept: &WikilinkAccept) {
    let doc = editor.document();
    let (before, caret, closed) = {
        let doc = doc.borrow();
        let caret = doc.primary_selection().head;
        let buffer = doc.buffer();
        let tail_end = (caret + 2).min(buffer.len());
        let closed = buffer.slice(TextRange::new(caret.min(tail_end), tail_end)) == "]]";
        (doc.primary_selection(), caret, closed)
    };
    let replace = TextRange::new(
        (query_start + accept.replace.start).min(caret),
        (query_start + accept.replace.end).min(caret),
    );
    let mut text = accept.insert.clone();
    if !closed {
        text.push_str("]]");
    }
    let end = replace.start + text.len() + if closed { 2 } else { 0 };
    let edit = Edit::replace(replace, text);
    let _ = doc
        .borrow_mut()
        .apply(Transaction::single(edit, before, Selection::caret(end)));
}

/// Most files Quick Open lists.
const FILE_LIST_LIMIT: usize = 50_000;

/// The files below `root` as paths relative to it (`/` separated), sorted. Hidden entries are
/// skipped unless `show_hidden`; `.git` never is listed. The walk stops at 50 000 files.
#[must_use]
pub fn list_workspace_files(root: &std::path::Path, show_hidden: bool) -> Vec<String> {
    list_files(root, show_hidden, FILE_LIST_LIMIT)
}

/// [`list_workspace_files`] stopping at `limit` files.
fn list_files(root: &std::path::Path, show_hidden: bool, limit: usize) -> Vec<String> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    'walk: while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" || (!show_hidden && name.starts_with('.')) {
                continue;
            }
            // The type of a Windows directory entry comes with the listing and is always there
            // (`std` returns `Ok` unconditionally), so a missing one is only ever skipped.
            let kind = entry.file_type().ok();
            let path = entry.path();
            if kind.is_some_and(|kind| kind.is_dir()) {
                stack.push(path);
            } else if kind.is_some_and(|kind| kind.is_file())
                && let Ok(relative) = path.strip_prefix(root)
            {
                files.push(relative.to_string_lossy().replace('\\', "/"));
                if files.len() >= limit {
                    break 'walk;
                }
            }
        }
    }
    files.sort();
    files
}

/// Converts the matches of `ssw_io::folder_search` into what the panel shows. Paths are made
/// relative to `root`.
#[must_use]
pub fn panel_files(
    root: &std::path::Path,
    found: Vec<ssw_io::folder_search::FileMatches>,
) -> Vec<FolderSearchFile> {
    found
        .into_iter()
        .map(|file| FolderSearchFile {
            path: file
                .path
                .strip_prefix(root)
                .unwrap_or(&file.path)
                .to_string_lossy()
                .replace('\\', "/"),
            matches: file
                .matches
                .into_iter()
                .map(|line| FolderSearchLine {
                    line: line.line,
                    column: line.column,
                    ranges: line.ranges,
                    preview: line.preview,
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::list_files;

    #[test]
    fn the_walk_stops_at_the_limit_and_lists_everything_below_it() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        for name in ["a.md", "b.md", "c.md", "sub/d.md", "sub/e.md"] {
            std::fs::write(tmp.path().join(name), "x").unwrap();
        }
        assert_eq!(
            list_files(tmp.path(), false, 100),
            ["a.md", "b.md", "c.md", "sub/d.md", "sub/e.md"]
        );
        let limited = list_files(tmp.path(), false, 3);
        assert_eq!(limited.len(), 3);
        assert!(limited.windows(2).all(|pair| pair[0] < pair[1]), "sorted");
    }

    #[test]
    fn a_junction_is_neither_listed_nor_walked_into() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.md"), "x").unwrap();
        std::fs::write(outside.path().join("secret.md"), "x").unwrap();
        // A directory junction needs no privilege. It is a link, neither a folder nor a file
        // of the workspace, so the walk stays inside the root.
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(tmp.path().join("link"))
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J failed: {made:?}");
        assert_eq!(list_files(tmp.path(), false, 100), ["a.md"]);
    }

    #[test]
    fn hidden_entries_are_listed_on_request_git_never_and_a_missing_root_lists_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git").join("config"), "x").unwrap();
        std::fs::write(tmp.path().join(".hidden.md"), "x").unwrap();
        std::fs::write(tmp.path().join("shown.md"), "x").unwrap();
        assert_eq!(list_files(tmp.path(), false, 100), ["shown.md"]);
        assert_eq!(
            list_files(tmp.path(), true, 100),
            [".hidden.md", "shown.md"]
        );
        assert!(list_files(&tmp.path().join("missing"), false, 100).is_empty());
    }
}
